use serde::{Deserialize, Serialize};

/// `POST /api/i` などが返すユーザー情報(実装時に必要な最小フィールドのみ)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "avatarUrl", default)]
    pub avatar_url: Option<String>,
}
