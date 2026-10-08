//! misskey.io との REST 通信(io フォーク、本家互換)。
//! N-05: ホスト名は内部パラメータで、API 基底 URL と MiAuth URL をここで生成する。
//! 認証は Misskey 流儀どおりリクエストボディの `i` フィールドにトークンを載せる。

use crate::config::{ColumnFilters, TimelineKind};
use crate::model::{Channel, Emoji, EmojisResponse, InstanceMeta, Note, Notification, User};
use serde::Serialize;
use std::time::Duration;

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
        let value: serde_json::Value = resp.json().await.map_err(ApiError::Network)?;
        if !status.is_success() {
            let err = &value["error"];
            return Err(ApiError::Server {
                code: err["code"].as_str().unwrap_or("UNKNOWN").to_owned(),
                message: err["message"].as_str().unwrap_or("不明なエラー").to_owned(),
            });
        }
        serde_json::from_value(value).map_err(|e| ApiError::Unexpected(e.to_string()))
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
    /// io での応答形(グルーピングの有無など)は要実データ検証(Phase 6)
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
/// 返信フィルタはクライアント側での最終フィルタを併用する(Phase 6 で検証)
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

#[cfg(test)]
mod tests {
    use super::*;

    // AUTH-01: MiAuth URL がホスト・セッション・権限を正しく含む(N-05 のホストパラメータ化)
    #[test]
    fn auth01_miauth_url() {
        let url = miauth_url("misskey.io", "sess-1", "NMNL-browser");
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("misskey.io"));
        assert_eq!(parsed.path(), "/miauth/sess-1");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().collect();
        assert_eq!(pairs.get("name").map(|v| v.as_ref()), Some("NMNL-browser"));
        let perms: Vec<&str> = pairs["permission"].split(',').collect();
        assert_eq!(perms, MIAUTH_PERMISSIONS);
        // 別ホストでも URL が生成できること
        assert!(miauth_url("example.com", "s", "app").starts_with("https://example.com/"));
    }

    // AUTH-02: miauth/check 応答の解釈(ok:false は Pending、ok:true は token+user を要する)
    #[test]
    fn auth02_miauth_check_parse() {
        match parse_miauth_response(serde_json::json!({"ok": false})).unwrap() {
            MiauthStatus::Pending => {}
            _ => panic!("ok:false は Pending のはず"),
        }

        let ok = serde_json::json!({
            "ok": true,
            "token": "tok",
            "user": {"id": "u1", "username": "alice", "name": "Alice", "avatarUrl": null}
        });
        match parse_miauth_response(ok).unwrap() {
            MiauthStatus::Authorized { token } => assert_eq!(token, "tok"),
            _ => panic!("ok:true は Authorized のはず"),
        }

        // ok:true でも token/user を欠く応答はエラーにする
        let broken = serde_json::json!({"ok": true, "user": null});
        assert!(parse_miauth_response(broken).is_err());
    }

    // 以下のフィクスチャは io フォーク実測応答(2026-10-07)を最小化した形。

    /// 実測キー構成のユーザー
    fn fixture_user() -> serde_json::Value {
        serde_json::json!({
            "id": "u1", "username": "alice", "name": "Alice",
            "host": null, "avatarUrl": "https://example.com/a.png",
            "avatarBlurhash": null, "isBot": false, "isCat": true,
            "emojis": {"star": "https://example.com/star.png"},
            "onlineStatus": "online"
        })
    }

    /// 実測キー構成の最小ノート
    fn fixture_note(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "createdAt": "2026-10-07T12:00:00.000Z",
            "userId": "u1", "user": fixture_user(),
            "text": "本文", "cw": null, "visibility": "public",
            "replyId": null, "renoteId": null,
            "reactions": {"❤": 2, ":meow@.": 5},
            "reactionEmojis": {"meow@.": "https://example.com/meow.png"},
            "emojis": {"meow": "https://example.com/meow.png"},
            "fileIds": [], "files": [],
            "repliesCount": 0, "renoteCount": 3, "localOnly": false
        })
    }

    // TL-01: タイムライン種別→エンドポイント名(io 実測で検証済み)と
    // ページング・フィルタ(F-03-3、F-03-4)のリクエスト生成
    #[test]
    fn tl01_timeline_endpoint_and_body() {
        assert_eq!(timeline_endpoint(TimelineKind::Home), "notes/timeline");
        assert_eq!(
            timeline_endpoint(TimelineKind::Local),
            "notes/local-timeline"
        );
        assert_eq!(
            timeline_endpoint(TimelineKind::Social),
            "notes/hybrid-timeline"
        );
        assert_eq!(
            timeline_endpoint(TimelineKind::Global),
            "notes/global-timeline"
        );

        let paging = Paging {
            limit: 30,
            until_id: Some("u1".to_owned()),
            since_id: None,
        };
        let filters = ColumnFilters {
            include_renotes: false,
            include_replies: true,
            files_only: true,
        };
        let body = timeline_body(&paging, &filters);
        assert_eq!(body["limit"], 30);
        assert_eq!(body["untilId"], "u1");
        assert!(body.get("sinceId").is_none(), "未指定の項目は送らない");
        assert_eq!(body["withRenotes"], false);
        assert_eq!(body["withReplies"], true);
        assert_eq!(body["withFiles"], true);
    }

    // TL-02: タイムライン応答(Note 配列)のデコード(実測キー構成)
    #[test]
    fn tl02_timeline_notes_decode() {
        let mut note = fixture_note("n1");
        note["renoteId"] = serde_json::json!("n0");
        note["renote"] = fixture_note("n0");
        note["renote"]["files"] = serde_json::json!([{
            "id": "f1", "createdAt": "2026-10-07T11:00:00.000Z", "name": "a.webp",
            "type": "image/webp", "size": 12345, "isSensitive": true,
            "blurhash": "LEHV6nWB", "url": "https://example.com/a.webp",
            "thumbnailUrl": "https://example.com/a.thumb.webp",
            "properties": {"width": 800, "height": 600}, "comment": null
        }]);
        let notes: Vec<Note> = serde_json::from_value(serde_json::json!([note])).unwrap();
        let n = &notes[0];
        assert_eq!(n.id, "n1");
        assert_eq!(n.visibility, crate::model::Visibility::Public);
        assert_eq!(n.reactions[":meow@."], 5);
        assert_eq!(n.reaction_emojis["meow@."], "https://example.com/meow.png");
        let inner = n.renote.as_deref().unwrap();
        assert_eq!(inner.id, "n0");
        let f = &inner.files[0];
        assert_eq!(f.file_type, "image/webp");
        assert!(f.is_sensitive);
        assert_eq!(f.properties.as_ref().unwrap().width, Some(800));
    }

    // TL-03: チャンネル関連(F-03-7)のリクエストと応答デコード(実測キー構成)
    #[test]
    fn tl03_channels() {
        let ch = serde_json::json!({
            "id": "9axtmmcxuy", "createdAt": "2023-02-07T13:07:28.305Z",
            "lastNotedAt": "2026-10-07T12:03:03.060Z", "name": "テスト部",
            "description": "説明文", "userId": "u1",
            "bannerUrl": null, "color": "#88f",
            "usersCount": 12, "notesCount": 345,
            "isArchived": false, "isSensitive": false, "isFollowing": true
        });
        let chans: Vec<Channel> = serde_json::from_value(serde_json::json!([ch])).unwrap();
        assert_eq!(chans[0].name, "テスト部");
        assert_eq!(chans[0].is_following, Some(true));

        // channel_timeline のボディは実際の生成関数を通す
        let paging = Paging {
            limit: 20,
            until_id: Some("old".to_owned()),
            since_id: None,
        };
        let body = channel_timeline_body("c1", &paging);
        assert_eq!(body["channelId"], "c1");
        assert_eq!(body["untilId"], "old");
        assert!(body.get("sinceId").is_none());
    }

    // NTF-01: 通知一覧の種別フィルタ(F-04-1)と応答デコード
    #[test]
    fn ntf01_notifications() {
        let paging = Paging::default();
        let inc = vec!["reaction".to_owned(), "mention".to_owned()];
        let body = notifications_body(&paging, &inc, &[]);
        assert_eq!(
            body["includeTypes"],
            serde_json::json!(["reaction", "mention"])
        );
        // 空のフィルタはキー自体を送らない
        let body2 = notifications_body(&paging, &[], &[]);
        assert!(body2.get("includeTypes").is_none());
        assert!(body2.get("excludeTypes").is_none());

        // 応答デコード(本家の既知形。io 実測は認証後の Phase 6 で検証)
        let notif = serde_json::json!({
            "id": "ntf1", "createdAt": "2026-10-07T12:00:00.000Z",
            "type": "reaction", "isRead": false,
            "userId": "u2", "user": fixture_user(),
            "note": fixture_note("n9"), "reaction": ":meow@.:"
        });
        let list: Vec<Notification> = serde_json::from_value(serde_json::json!([notif])).unwrap();
        let n = &list[0];
        assert_eq!(n.kind, "reaction");
        assert_eq!(n.reaction.as_deref(), Some(":meow@.:"));
        assert_eq!(n.note.as_ref().unwrap().id, "n9");
        assert!(!n.is_read);
    }

    // NOTE-01: notes/show・notes/conversation のデコード(深さ1の入れ子、F-05-4)
    #[test]
    fn note01_show_and_conversation() {
        let mut note = fixture_note("n1");
        note["replyId"] = serde_json::json!("n0");
        note["reply"] = fixture_note("n0");
        note["reply"]["text"] = serde_json::json!("返信元");
        let decoded: Note = serde_json::from_value(note).unwrap();
        let reply = decoded.reply.as_deref().unwrap();
        assert_eq!(reply.id, "n0");
        assert_eq!(reply.text.as_deref(), Some("返信元"));
        // 入れ子の中のさらに深い参照は展開しない設計(デコードできても使わない)

        let conv: Vec<Note> =
            serde_json::from_value(serde_json::json!([fixture_note("a"), fixture_note("b")]))
                .unwrap();
        assert_eq!(conv.len(), 2);
    }

    // NOTE-02: users/show の応答デコード(実測キー構成)
    #[test]
    fn note02_user_show() {
        let mut u = fixture_user();
        u["description"] = serde_json::json!("自己紹介");
        u["followersCount"] = serde_json::json!(10);
        u["host"] = serde_json::json!("remote.example");
        let user: User = serde_json::from_value(u).unwrap();
        assert_eq!(user.host.as_deref(), Some("remote.example"));
        assert_eq!(user.emojis["star"], "https://example.com/star.png");

        // リクエストの二形態
        assert_eq!(
            user_show_body(&UserQuery::ById("u9".to_owned()))["userId"],
            "u9"
        );
        let b = user_show_body(&UserQuery::ByName {
            username: "bob".to_owned(),
            host: Some("h.example".to_owned()),
        });
        assert_eq!(b["username"], "bob");
        assert_eq!(b["host"], "h.example");
    }

    // NOTE-03: emoji 個別取得の応答デコード(io 実測キー構成)
    #[test]
    fn note03_emoji_lookup() {
        let raw = serde_json::json!({
            "id": "8vzym6uouj", "aliases": [""], "name": "ai_acid_misskeyio",
            "category": "000 Misskey.io Original", "host": null,
            "url": "https://media.misskeyusercontent.jp/emoji/ai_acid_misskeyio.apng",
            "isSensitive": false, "localOnly": false
        });
        let e: Emoji = serde_json::from_value(raw).unwrap();
        assert_eq!(e.name, "ai_acid_misskeyio");
        assert_eq!(e.category.as_deref(), Some("000 Misskey.io Original"));
        assert!(!e.is_sensitive);
    }

    // REA-01: 絵文字一覧の応答デコード(F-07-3 ピッカー用、実測は `{"emojis":[...]}` ラップ)
    #[test]
    fn rea01_emojis_list() {
        let raw = serde_json::json!({
            "emojis": [
                {"aliases": ["a1"], "name": "e1", "category": "cat", "url": "https://x/e1.png"},
                {"aliases": [], "name": "e2", "category": null, "url": "https://x/e2.png"}
            ]
        });
        let resp: EmojisResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.emojis.len(), 2);
        assert_eq!(resp.emojis[0].aliases, vec!["a1"]);
    }

    // API-01: meta 応答デコードと機能フラグ参照(N-05)
    #[test]
    fn api01_meta_features() {
        let raw = serde_json::json!({
            "name": "Misskey.io", "version": "2025.4.1-io.12b",
            "features": {
                "localTimeline": true, "globalTimeline": true,
                "registration": false, "miauth": true
            },
            "maxNoteTextLength": 3000
        });
        let meta: InstanceMeta = serde_json::from_value(raw).unwrap();
        assert_eq!(meta.feature("localTimeline"), Some(true));
        assert_eq!(meta.feature("registration"), Some(false));
        assert_eq!(meta.feature("nonexistent"), None);
        assert_eq!(meta.max_note_text_length, Some(3000));
    }

    fn fixture_file() -> serde_json::Value {
        serde_json::json!({
            "id": "f1", "createdAt": "2026-10-07T11:00:00.000Z", "name": "a.webp",
            "type": "image/webp", "size": 12345, "isSensitive": true,
            "blurhash": "LEHV6nWB", "url": "https://example.com/a.webp",
            "thumbnailUrl": "https://example.com/a.thumb.webp",
            "properties": {"width": 800, "height": 600}, "comment": null
        })
    }

    // TL-04: カラムフィルタのクライアント側最終適用(F-03-4)と、
    // フィルタ前応答からのカーソル取得
    #[test]
    fn tl04_client_filters_and_cursor() {
        let mut reply = fixture_note("r1");
        reply["replyId"] = serde_json::json!("r0");
        let plain = fixture_note("p1");
        let mut rn = fixture_note("rn1");
        rn["renoteId"] = serde_json::json!("x1");
        rn["text"] = serde_json::Value::Null; // 純粋リノート
        let filters = ColumnFilters {
            include_renotes: false,
            include_replies: false,
            files_only: false,
        };
        let page = TimelinePage::new(
            vec![
                serde_json::from_value(reply).unwrap(),
                serde_json::from_value(plain).unwrap(),
                serde_json::from_value(rn).unwrap(),
            ],
            &filters,
        );
        assert_eq!(page.notes.len(), 1);
        assert_eq!(page.notes[0].id, "p1");
        // カーソルはフィルタ前の応答から取る(先頭・末尾が落ちても残る)
        assert_eq!(page.newest_id.as_deref(), Some("r1"));
        assert_eq!(page.oldest_id.as_deref(), Some("rn1"));

        // 全件がフィルタ落ちしてもカーソルは残り次ページへ進める
        let mut only = fixture_note("only");
        only["replyId"] = serde_json::json!("r0");
        let page2 = TimelinePage::new(vec![serde_json::from_value(only).unwrap()], &filters);
        assert!(page2.notes.is_empty());
        assert_eq!(page2.oldest_id.as_deref(), Some("only"));

        // files_only は入れ子リノート内のファイルも見る
        let mut inner = fixture_note("inner");
        inner["files"] = serde_json::json!([fixture_file()]);
        let mut outer = fixture_note("outer");
        outer["renoteId"] = serde_json::json!("inner");
        outer["text"] = serde_json::Value::Null;
        outer["renote"] = inner;
        let files_only = ColumnFilters {
            include_renotes: true,
            include_replies: true,
            files_only: true,
        };
        let page3 = TimelinePage::new(
            vec![
                serde_json::from_value(outer).unwrap(),
                serde_json::from_value(fixture_note("nofile")).unwrap(),
            ],
            &files_only,
        );
        assert_eq!(page3.notes.len(), 1);
        assert_eq!(page3.notes[0].id, "outer");

        // 本文 null でも添付を持つノートは引用であり、リノート非表示でも残す
        let mut quote = fixture_note("q1");
        quote["renoteId"] = serde_json::json!("target");
        quote["text"] = serde_json::Value::Null;
        quote["files"] = serde_json::json!([fixture_file()]);
        quote["fileIds"] = serde_json::json!(["f1"]);
        let page4 = TimelinePage::new(vec![serde_json::from_value(quote).unwrap()], &filters);
        assert_eq!(page4.notes.len(), 1);
        assert_eq!(page4.notes[0].id, "q1");
    }

    // TL-05: Paging 既定値は有効な limit を持つ(0 は INVALID_PARAM になる)
    #[test]
    fn tl05_paging_default_limit() {
        let p = Paging::default();
        assert!(p.limit > 0);
        assert!(p.until_id.is_none() && p.since_id.is_none());
        let body = notifications_body(&p, &[], &[]);
        assert_eq!(body["limit"], serde_json::json!(30));
    }

    // API-02: トークンの i 注入と匿名送信(post の実経路をモックで検証)
    #[tokio::test]
    async fn api02_token_injection() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/api/i")
                    .json_body_includes("{\"i\":\"tok-1\"}");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(fixture_user());
            })
            .await;
        let client = ApiClient::for_test(
            format!("{}/api", server.base_url()),
            Some("tok-1".to_owned()),
        );
        let user = client.i().await.unwrap();
        assert_eq!(user.username, "alice");
        m.assert_async().await;
    }

    // API-03: エラー応答の解釈(401 + error.code)
    #[tokio::test]
    async fn api03_error_parsing() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/api/i");
                then.status(401)
                    .header("content-type", "application/json")
                    .body("{\"error\":{\"code\":\"AUTHENTICATION_FAILED\",\"message\":\"bad\"}}");
            })
            .await;
        let client =
            ApiClient::for_test(format!("{}/api", server.base_url()), Some("bad".to_owned()));
        let err = client.i().await.unwrap_err();
        match &err {
            ApiError::Server { code, .. } => assert_eq!(code, "AUTHENTICATION_FAILED"),
            _ => panic!("Server エラーのはず: {err:?}"),
        }
        assert!(err.is_auth_failure());
    }

    // API-04: 429 は Retry-After を優先して限定的に再試行する(N-03)
    #[tokio::test]
    async fn api04_429_retry() {
        use httpmock::prelude::*;
        use httpmock::{HttpMockRequest, HttpMockResponse};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let server = MockServer::start_async().await;
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/api/emojis");
                then.respond_with(move |_req: &HttpMockRequest| {
                    let n = count2.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        // Retry-After: 0 で即再試行(テストの高速化)
                        HttpMockResponse::builder()
                            .status(429)
                            .header("retry-after", "0")
                            .body("{}")
                            .build()
                    } else {
                        HttpMockResponse::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body("{\"emojis\":[]}")
                            .build()
                    }
                });
            })
            .await;
        let anon = ApiClient::for_test(format!("{}/api", server.base_url()), None);
        let list = anon.emojis().await.unwrap();
        assert!(list.is_empty());
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert_eq!(m.calls_async().await, 3);
    }

    // API-05: 公開エンドポイントの往復(匿名では i を載せない)
    #[tokio::test]
    async fn api05_public_endpoint_no_token() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                // 完全一致ボディで i が注入されないことを検証
                when.method(POST)
                    .path("/api/emojis")
                    .json_body(serde_json::json!({}));
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(
                        serde_json::json!({"emojis":[{"name":"e1","url":"https://x/e1.png"}]}),
                    );
            })
            .await;
        let anon = ApiClient::for_test(format!("{}/api", server.base_url()), None);
        let list = anon.emojis().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "e1");
        m.assert_async().await;
    }
}
