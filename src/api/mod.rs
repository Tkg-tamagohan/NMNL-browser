//! misskey.io との REST 通信(io フォーク、本家互換)。
//! N-05: ホスト名は内部パラメータで、API 基底 URL と MiAuth URL をここで生成する。
//! 認証は Misskey 流儀どおりリクエストボディの `i` フィールドにトークンを載せる。

use crate::config::{ColumnFilters, TimelineKind};
use crate::model::{
    Channel, DriveFile, Emoji, EmojisResponse, InstanceMeta, Note, Notification, User,
};
use serde::Serialize;
use std::time::Duration;

#[cfg(test)]
mod tests;

/// F-01-4 の要求権限。MVP 範囲のエンドポイントと照合した最小構成。
/// (read:user-groups はリスト/アンテナが MVP 対象外のため含めない)
pub const MIAUTH_PERMISSIONS: &[&str] = &[
    "read:account",
    "write:notes",
    "read:notifications",
    "write:reactions",
    "read:drive",
    "write:drive",
    "read:channels",
];

#[derive(Debug)]
pub enum ApiError {
    Network(reqwest::Error),
    /// サーバーが返したエラー応答(`{"error": {"code","message"}}` 形式)
    Server {
        code: String,
        message: String,
    },
    Unexpected(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Network(e) => write!(f, "通信エラー: {e}"),
            ApiError::Server { code, message } => write!(f, "サーバーエラー {code}: {message}"),
            ApiError::Unexpected(m) => write!(f, "予期しない応答: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// トークン失効・未認証かどうか(F-01-3 の再認証誘導に使う)
    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            ApiError::Server { code, .. }
                if code == "AUTHENTICATION_FAILED" || code == "CREDENTIAL_REQUIRED"
        )
    }
}

#[derive(Debug)]
pub enum MiauthStatus {
    /// まだ認可されていない(実測: `{"ok":false}` が返る)
    Pending,
    /// `user` は応答にあることを検証するが、表示名は /api/i の結果で確定するため保持しない。
    Authorized { token: String },
}

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    /// `https://{host}/api`。テストではモックサーバーの URL を差し込む
    api_base: String,
    token: Option<String>,
}

impl ApiClient {
    /// 認証不要の呼び出し用(miauth/check、meta など)
    pub fn anonymous(host: impl Into<String>) -> Self {
        Self::new(host, None)
    }

    pub fn with_token(host: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new(host, Some(token.into()))
    }

    fn new(host: impl Into<String>, token: Option<String>) -> Self {
        let host = host.into();
        Self {
            http: reqwest::Client::new(),
            api_base: format!("https://{host}/api"),
            token,
        }
    }

    #[cfg(test)]
    fn for_test(api_base: String, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_base,
            token,
        }
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("{}/{}", self.api_base, endpoint)
    }

    /// 全エンドポイント共通の POST。トークンがあれば Misskey 流儀どおり
    /// ボディの `i` フィールドに載せる(呼び出し側が個別に書かなくてよい)
    async fn post<B, R>(&self, endpoint: &str, body: &B) -> Result<R, ApiError>
    where
        B: Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        let mut body =
            serde_json::to_value(body).map_err(|e| ApiError::Unexpected(e.to_string()))?;
        if let (Some(token), Some(obj)) = (&self.token, body.as_object_mut()) {
            obj.insert("i".to_owned(), serde_json::Value::String(token.clone()));
        }
        // N-03: レート制限(429)は Retry-After または指数バックオフで限定的に再試行する
        let mut attempts = 0u32;
        let resp = loop {
            let resp = self
                .http
                .post(self.api_url(endpoint))
                .json(&body)
                .send()
                .await
                .map_err(ApiError::Network)?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < MAX_429_RETRIES
            {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let delay = retry_after
                    .map(Duration::from_secs)
                    .unwrap_or(RETRY_429_BASE * 2u32.pow(attempts));
                tokio::time::sleep(delay).await;
                attempts += 1;
                continue;
            }
            break resp;
        };
        let status = resp.status();
        if !status.is_success() {
            // エラー応答は `{"error":{...}}` が普通だが、Cloudflare 等が
            // HTML や空ボディを返すことがあるため status を先に見る
            let text = resp.text().await.map_err(ApiError::Network)?;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                let err = &value["error"];
                return Err(ApiError::Server {
                    code: err["code"].as_str().unwrap_or("UNKNOWN").to_owned(),
                    message: err["message"].as_str().unwrap_or("不明なエラー").to_owned(),
                });
            }
            return Err(ApiError::Server {
                code: format!("HTTP {}", status.as_u16()),
                message: format!(
                    "非 JSON 応答(先頭 120 文字): {}",
                    text.chars().take(120).collect::<String>()
                ),
            });
        }
        let value = parse_json_body(resp).await?;
        serde_json::from_value(value).map_err(|e| ApiError::Unexpected(e.to_string()))
    }

    /// `POST /api/notes/create`: 投稿・返信・リノート・引用(F-06)。
    /// リノートは renote_id のみ、引用は text + renote_id
    pub async fn create_note(&self, req: &crate::model::CreateNote) -> Result<Note, ApiError> {
        let resp: crate::model::CreatedNote = self.post("notes/create", req).await?;
        Ok(resp.created_note)
    }

    /// `POST /api/notes/reactions/create`: リアクション付与(F-07-2)。
    /// io は 204 No Content を返す
    pub async fn create_reaction(&self, note_id: &str, reaction: &str) -> Result<(), ApiError> {
        self.post(
            "notes/reactions/create",
            &serde_json::json!({ "noteId": note_id, "reaction": reaction }),
        )
        .await
    }

    /// `POST /api/notes/reactions/delete`: 自分のリアクション取り消し
    pub async fn delete_reaction(&self, note_id: &str) -> Result<(), ApiError> {
        self.post(
            "notes/reactions/delete",
            &serde_json::json!({ "noteId": note_id }),
        )
        .await
    }

    /// `POST /api/drive/files/create`: ドライブへ画像アップロード(F-06-2)。
    /// multipart/form-data で、トークンはフォームの `i` フィールド
    pub async fn upload_drive_file(
        &self,
        name: &str,
        mime: &str,
        data: Vec<u8>,
    ) -> Result<DriveFile, ApiError> {
        // N-03: 429 は JSON 系と同じく限定的に再試行する
        // (multipart のフォームは使い回せないため毎回再構築)
        let mut attempts = 0u32;
        let resp = loop {
            let part = reqwest::multipart::Part::bytes(data.clone())
                .file_name(name.to_owned())
                .mime_str(mime)
                .map_err(|e| ApiError::Unexpected(e.to_string()))?;
            let mut form = reqwest::multipart::Form::new().part("file", part);
            if let Some(token) = &self.token {
                form = form.text("i", token.clone());
            }
            let resp = self
                .http
                .post(self.api_url("drive/files/create"))
                .multipart(form)
                .send()
                .await
                .map_err(ApiError::Network)?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < MAX_429_RETRIES
            {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let delay = retry_after
                    .map(Duration::from_secs)
                    .unwrap_or(RETRY_429_BASE * 2u32.pow(attempts));
                tokio::time::sleep(delay).await;
                attempts += 1;
                continue;
            }
            break resp;
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.map_err(ApiError::Network)?;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                let err = &value["error"];
                return Err(ApiError::Server {
                    code: err["code"].as_str().unwrap_or("UNKNOWN").to_owned(),
                    message: err["message"].as_str().unwrap_or("不明なエラー").to_owned(),
                });
            }
            return Err(ApiError::Server {
                code: format!("HTTP {}", status.as_u16()),
                message: format!(
                    "非 JSON 応答(先頭 120 文字): {}",
                    text.chars().take(120).collect::<String>()
                ),
            });
        }
        let value = parse_json_body(resp).await?;
        serde_json::from_value(value).map_err(|e| ApiError::Unexpected(e.to_string()))
    }

    /// 投稿用の添付アップロード(F-06-2)。`uploaded_id` 済みのファイルは
    /// 再アップロードせずその ID を使う(投稿失敗後のリトライで
    /// ドライブに参照されないコピーが増えるのを防ぐ)。順序は files の
    /// 並びをそのまま file_ids に写す
    pub async fn upload_pending_files(
        &self,
        files: &mut [crate::composer::PendingFile],
    ) -> Result<Vec<String>, ApiError> {
        let mut ids = Vec::with_capacity(files.len());
        for f in files.iter_mut() {
            if let Some(id) = &f.uploaded_id {
                ids.push(id.clone());
                continue;
            }
            let df = self
                .upload_drive_file(&f.name, &f.mime, (*f.data).clone())
                .await?;
            f.uploaded_id = Some(df.id.clone());
            ids.push(df.id);
        }
        Ok(ids)
    }

    /// `POST /api/i`: トークンの検証と自分の情報取得(F-01)
    pub async fn i(&self) -> Result<User, ApiError> {
        if self.token.is_none() {
            return Err(ApiError::Unexpected("トークンがありません".to_owned()));
        }
        self.post("i", &serde_json::json!({})).await
    }

    /// `POST /api/miauth/{session}/check`: 認可の取り込み(F-01-1)
    pub async fn miauth_check(&self, session: &str) -> Result<MiauthStatus, ApiError> {
        let endpoint = format!("miauth/{session}/check");
        let value: serde_json::Value = self.post(&endpoint, &serde_json::json!({})).await?;
        parse_miauth_response(value)
    }

    /// タイムライン系エンドポイント(F-03-1〜4)。io 実測: ホームは `notes/timeline`
    /// (`notes/home-timeline` は 404)。local/global は匿名でも読める。
    /// 返信フィルタはサーバー任せにせずクライアント側で最終適用する(LTL では
    /// withReplies:false が無効な実測)。カーソルはフィルタ前の応答から取る
    pub async fn timeline(
        &self,
        kind: TimelineKind,
        paging: &Paging,
        filters: &ColumnFilters,
    ) -> Result<TimelinePage, ApiError> {
        let body = timeline_body(paging, filters);
        let notes: Vec<Note> = self.post(timeline_endpoint(kind), &body).await?;
        Ok(TimelinePage::new(notes, filters))
    }

    /// `POST /api/notes/show`: ノート単体(F-05)
    pub async fn show_note(&self, note_id: &str) -> Result<Note, ApiError> {
        self.post("notes/show", &serde_json::json!({ "noteId": note_id }))
            .await
    }

    /// `POST /api/notes/conversation`: 会話ビューの前後ノート(F-05-5)
    pub async fn conversation(
        &self,
        note_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Note>, ApiError> {
        self.post(
            "notes/conversation",
            &serde_json::json!({ "noteId": note_id, "limit": limit, "offset": offset }),
        )
        .await
    }

    /// `POST /api/notes/mentions`: 自分宛のノート(F-04-2)
    pub async fn mentions(&self, paging: &Paging) -> Result<Vec<Note>, ApiError> {
        let mut body = serde_json::json!({ "limit": paging.limit });
        insert_opt(&mut body, "untilId", &paging.until_id);
        insert_opt(&mut body, "sinceId", &paging.since_id);
        self.post("notes/mentions", &body).await
    }

    /// `POST /api/i/notifications`: 通知一覧(F-04-1)
    /// 通知カラムの表示で io の実データ確認済み(Phase 6)
    pub async fn notifications(
        &self,
        paging: &Paging,
        include_types: &[String],
        exclude_types: &[String],
    ) -> Result<Vec<Notification>, ApiError> {
        let body = notifications_body(paging, include_types, exclude_types);
        self.post("i/notifications", &body).await
    }

    /// `POST /api/users/show`: ユーザープロフィール(F-05-6)
    pub async fn show_user(&self, query: &UserQuery) -> Result<User, ApiError> {
        self.post("users/show", &user_show_body(query)).await
    }

    /// `POST /api/users/notes`: プロフィールに出すそのユーザーのノート一覧(F-05-6)
    pub async fn user_notes(&self, user_id: &str, paging: &Paging) -> Result<Vec<Note>, ApiError> {
        let mut body = serde_json::json!({ "userId": user_id, "limit": paging.limit });
        insert_opt(&mut body, "untilId", &paging.until_id);
        self.post("users/notes", &body).await
    }

    /// `POST /api/channels/search`: チャンネル検索(F-03-7 の選択 UI 用)
    pub async fn search_channels(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Channel>, ApiError> {
        self.post(
            "channels/search",
            &serde_json::json!({ "query": query, "limit": limit, "offset": offset }),
        )
        .await
    }

    /// `POST /api/channels/followed`: フォロー中チャンネル一覧(F-03-7)
    pub async fn followed_channels(&self, paging: &Paging) -> Result<Vec<Channel>, ApiError> {
        let mut body = serde_json::json!({ "limit": paging.limit });
        insert_opt(&mut body, "untilId", &paging.until_id);
        self.post("channels/followed", &body).await
    }

    /// `POST /api/channels/timeline`: チャンネルカラムのタイムライン(F-03-7)
    /// フィルタパラメータのサーバー側対応は未検証のため送らず、クライアント側で適用する
    pub async fn channel_timeline(
        &self,
        channel_id: &str,
        paging: &Paging,
        filters: &ColumnFilters,
    ) -> Result<TimelinePage, ApiError> {
        let notes: Vec<Note> = self
            .post(
                "channels/timeline",
                &channel_timeline_body(channel_id, paging),
            )
            .await?;
        Ok(TimelinePage::new(notes, filters))
    }

    /// `POST /api/emojis`: ピッカー用の絵文字一覧(F-07-3)。応答は `{"emojis":[...]}` で包まれる
    pub async fn emojis(&self) -> Result<Vec<Emoji>, ApiError> {
        let resp: EmojisResponse = self.post("emojis", &serde_json::json!({})).await?;
        Ok(resp.emojis)
    }

    /// `POST /api/emoji`: 絵文字の個別解決(F-05-2 のオンデマンド取得)
    pub async fn emoji(&self, name: &str) -> Result<Emoji, ApiError> {
        self.post("emoji", &serde_json::json!({ "name": name }))
            .await
    }

    /// `POST /api/meta`: インスタンス情報(N-05 の機能差検出)
    pub async fn meta(&self) -> Result<InstanceMeta, ApiError> {
        self.post("meta", &serde_json::json!({})).await
    }
}

/// REST のカーソルページング(F-03-3)
#[derive(Debug, Clone)]
pub struct Paging {
    /// 正の整数が必須(0 は Invalid param)
    pub limit: u32,
    pub until_id: Option<String>,
    /// 欠落補充(F-03-6)用の未来方向カーソル
    pub since_id: Option<String>,
}

impl Default for Paging {
    fn default() -> Self {
        Self {
            limit: 30,
            until_id: None,
            since_id: None,
        }
    }
}

/// タイムライン 1 ページ分の結果。表示用ノートはカラムフィルタ適用済みだが、
/// ページングのカーソルはフィルタ前の応答から取る(フィルタで全件落ちても
/// 次ページを辿れるようにするため)
#[derive(Debug)]
pub struct TimelinePage {
    pub notes: Vec<Note>,
    /// フィルタ前の最古ノート ID(untilId 方向の次カーソル)。生応答が空なら None
    pub oldest_id: Option<String>,
    /// フィルタ前の最新ノート ID(sinceId 方向の欠落補充用)
    pub newest_id: Option<String>,
}

impl TimelinePage {
    fn new(raw: Vec<Note>, filters: &ColumnFilters) -> Self {
        let newest_id = raw.first().map(|n| n.id.clone());
        let oldest_id = raw.last().map(|n| n.id.clone());
        Self {
            notes: apply_column_filters(raw, filters),
            oldest_id,
            newest_id,
        }
    }
}

/// F-03-4 のフィルタを受信ノートへ最終適用する。
/// io 実測で withReplies:false が LTL で無効と判明したため、返信フィルタは
/// サーバー側の効くエンドポイントでもクライアント側で再度適用する(冪等)。
/// リノート・ファイルフィルタも同じ規則で適用し、サーバー対応の有無に依らない
fn apply_column_filters(notes: Vec<Note>, filters: &ColumnFilters) -> Vec<Note> {
    notes
        .into_iter()
        .filter(|n| note_allowed(n, filters))
        .collect()
}

/// カラムフィルタ(F-03-4)を 1 件のノートに適用する。REST のページ適用と
/// ストリーミング差分の挿入判定で共用するため crate 内で公開する
pub(crate) fn note_allowed(n: &Note, filters: &ColumnFilters) -> bool {
    if !filters.include_replies && n.reply_id.is_some() {
        return false;
    }
    // 純粋なリノート(本文・CW・添付を持たない転載)のみ落とし、引用は残す。
    // text が null でも添付を持つものは引用なので残す(io 実測で存在)
    if !filters.include_renotes && n.is_pure_renote() {
        return false;
    }
    if filters.files_only
        && n.files.is_empty()
        && n.renote.as_ref().is_none_or(|r| r.files.is_empty())
    {
        return false;
    }
    true
}

/// `users/show` の指定方法
#[derive(Debug, Clone)]
pub enum UserQuery {
    ById(String),
    ByName {
        username: String,
        host: Option<String>,
    },
}

/// タイムライン種別→エンドポイント名。
/// io 実測(2026-10-07): `notes/home-timeline` は存在せずホームは `notes/timeline`
fn timeline_endpoint(kind: TimelineKind) -> &'static str {
    match kind {
        TimelineKind::Home => "notes/timeline",
        TimelineKind::Local => "notes/local-timeline",
        TimelineKind::Social => "notes/hybrid-timeline",
        TimelineKind::Global => "notes/global-timeline",
    }
}

/// タイムライン系リクエストのボディを組み立てる(F-03-3、F-03-4)。
/// io 実測: `withFiles`、`withRenotes` は効くが `withReplies:false` は LTL で無視される挙動。
/// 返信フィルタはクライアント側での最終フィルタを併用する(Phase 6 で検証済み)
fn timeline_body(paging: &Paging, filters: &ColumnFilters) -> serde_json::Value {
    let mut body = serde_json::json!({
        "limit": paging.limit,
        "withRenotes": filters.include_renotes,
        "withReplies": filters.include_replies,
        "withFiles": filters.files_only,
    });
    insert_opt(&mut body, "untilId", &paging.until_id);
    insert_opt(&mut body, "sinceId", &paging.since_id);
    body
}

/// channels/timeline のボディを組み立てる(F-03-7)
fn channel_timeline_body(channel_id: &str, paging: &Paging) -> serde_json::Value {
    let mut body = serde_json::json!({ "channelId": channel_id, "limit": paging.limit });
    insert_opt(&mut body, "untilId", &paging.until_id);
    insert_opt(&mut body, "sinceId", &paging.since_id);
    body
}

/// i/notifications のボディを組み立てる。
/// 種別フィルタは空配列を送らない(空は「全種」を意味するため省略する)
fn notifications_body(
    paging: &Paging,
    include_types: &[String],
    exclude_types: &[String],
) -> serde_json::Value {
    let mut body = serde_json::json!({ "limit": paging.limit });
    insert_opt(&mut body, "untilId", &paging.until_id);
    insert_opt(&mut body, "sinceId", &paging.since_id);
    if !include_types.is_empty() {
        body["includeTypes"] = serde_json::json!(include_types);
    }
    if !exclude_types.is_empty() {
        body["excludeTypes"] = serde_json::json!(exclude_types);
    }
    body
}

/// users/show のボディを組み立てる(userId 指定と username+host 指定の二形態)
fn user_show_body(query: &UserQuery) -> serde_json::Value {
    match query {
        UserQuery::ById(id) => serde_json::json!({ "userId": id }),
        UserQuery::ByName { username, host } => {
            let mut b = serde_json::json!({ "username": username });
            insert_opt(&mut b, "host", host);
            b
        }
    }
}

/// 成功応答のボディを JSON として読む。204 や空ボディは Null として返す
/// (reactions/create 等のボディ無し応答対策)
async fn parse_json_body(resp: reqwest::Response) -> Result<serde_json::Value, ApiError> {
    let text = resp.text().await.map_err(ApiError::Network)?;
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| ApiError::Unexpected(e.to_string()))
}

/// `Option<String>` が Some のときだけ JSON オブジェクトにキーを挿入する。
/// Misskey は省略すべき任意パラメータに null を拒否することがあるため、送らない設計にする
fn insert_opt(body: &mut serde_json::Value, key: &str, value: &Option<String>) {
    if let Some(v) = value {
        body[key] = serde_json::Value::String(v.clone());
    }
}

/// miauth/check の応答を解釈する。
/// io 実測: 認可前は `{"ok":false}`、認可後は `{"ok":true,"token":...,"user":{...}}`。
fn parse_miauth_response(value: serde_json::Value) -> Result<MiauthStatus, ApiError> {
    #[derive(serde::Deserialize)]
    struct Check {
        ok: bool,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        user: Option<User>,
    }
    let resp: Check =
        serde_json::from_value(value).map_err(|e| ApiError::Unexpected(e.to_string()))?;
    Ok(match (resp.ok, resp.token, resp.user) {
        (true, Some(token), Some(_)) => MiauthStatus::Authorized { token },
        (true, _, _) => {
            return Err(ApiError::Unexpected(
                "miauth/check が ok:true だが token/user を欠いています".to_owned(),
            ));
        }
        (false, _, _) => MiauthStatus::Pending,
    })
}

/// 429 応答への最大再試行回数と基準遅延(N-03)
const MAX_429_RETRIES: u32 = 3;
const RETRY_429_BASE: Duration = Duration::from_millis(500);

/// MiAuth の認可 URL を生成する(F-01-1)
pub fn miauth_url(host: &str, session: &str, app_name: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("https://{host}/miauth/{session}"))
        .expect("host/session が不正な URL になりました");
    url.query_pairs_mut()
        .append_pair("name", app_name)
        .append_pair("permission", &MIAUTH_PERMISSIONS.join(","));
    url.into()
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
