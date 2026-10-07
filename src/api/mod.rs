//! misskey.io との REST 通信(io フォーク、本家互換)。
//! N-05: ホスト名は内部パラメータで、API 基底 URL と MiAuth URL をここで生成する。
//! 認証は Misskey 流儀どおりリクエストボディの `i` フィールドにトークンを載せる。

use crate::model::User;
use serde::Serialize;

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
    Authorized {
        token: String,
        user: User,
    },
}

pub struct ApiClient {
    http: reqwest::Client,
    host: String,
    token: Option<String>,
}

impl ApiClient {
    /// 認証不要の呼び出し用(miauth/check、meta など)
    pub fn anonymous(host: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            host: host.into(),
            token: None,
        }
    }

    pub fn with_token(host: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            host: host.into(),
            token: Some(token.into()),
        }
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("https://{}/api/{}", self.host, endpoint)
    }

    async fn post<B, R>(&self, endpoint: &str, body: &B) -> Result<R, ApiError>
    where
        B: Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        let resp = self
            .http
            .post(self.api_url(endpoint))
            .json(body)
            .send()
            .await
            .map_err(ApiError::Network)?;
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
        let token = self
            .token
            .clone()
            .ok_or_else(|| ApiError::Unexpected("トークンがありません".to_owned()))?;
        self.post("i", &serde_json::json!({ "i": token })).await
    }

    /// `POST /api/miauth/{session}/check`: 認可の取り込み(F-01-1)
    pub async fn miauth_check(&self, session: &str) -> Result<MiauthStatus, ApiError> {
        #[derive(serde::Deserialize)]
        struct Check {
            ok: bool,
            #[serde(default)]
            token: Option<String>,
            #[serde(default)]
            user: Option<User>,
        }
        let endpoint = format!("miauth/{session}/check");
        let resp: Check = self.post(&endpoint, &serde_json::json!({})).await?;
        Ok(match (resp.ok, resp.token, resp.user) {
            (true, Some(token), Some(user)) => MiauthStatus::Authorized { token, user },
            (true, _, _) => {
                return Err(ApiError::Unexpected(
                    "miauth/check が ok:true だが token/user を欠いています".to_owned(),
                ));
            }
            (false, _, _) => MiauthStatus::Pending,
        })
    }
}

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
        let pending: serde_json::Value = serde_json::json!({"ok": false});
        assert!(!pending["ok"].as_bool().unwrap());

        let ok = serde_json::json!({
            "ok": true,
            "token": "tok",
            "user": {"id": "u1", "username": "alice", "name": "Alice", "avatarUrl": null}
        });
        let user: User = serde_json::from_value(ok["user"].clone()).unwrap();
        assert_eq!(user.username, "alice");
    }
}
