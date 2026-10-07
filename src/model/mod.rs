//! misskey.io の API が返すデータ型。
//! フィールドは io フォーク実測の応答を根拠に、必要なものだけを抜き出す。
//! サーバーが省略しうるフィールドはすべて Option または serde(default) にする。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `POST /api/i`、ノートの投稿者などが返すユーザー情報(io 実測キーを根拠に抜粋)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
    /// リモートユーザーの所属ホスト。ローカルは null
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub avatar_blurhash: Option<String>,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub is_cat: bool,
    /// 名前装飾用のカスタム絵文字マップ(絵文字名→URL)
    #[serde(default)]
    pub emojis: HashMap<String, String>,
}

/// ノートの公開範囲(F-06-1)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    #[default]
    Public,
    Home,
    Followers,
    Specified,
    /// 将来の拡張や io 独自値を落とさないための受け皿
    #[serde(other)]
    Unknown,
}

/// ドライブファイル(io 実測キーを根拠に抜粋)。F-08 の表示対象
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    #[serde(default)]
    pub created_at: Option<String>,
    pub name: String,
    /// MIME タイプ(image/webp など)。serde では `type` が予約語なのでフィールド名を変える
    #[serde(rename = "type")]
    pub file_type: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub is_sensitive: bool,
    #[serde(default)]
    pub blurhash: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub properties: Option<FileProperties>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileProperties {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// ノート(io 実測キーを根拠に抜粋)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub id: String,
    pub created_at: String,
    /// 本文。リノートのみのノートでは null になりうる
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub cw: Option<String>,
    pub user_id: String,
    pub user: User,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub local_only: bool,
    #[serde(default)]
    pub reply_id: Option<String>,
    #[serde(default)]
    pub renote_id: Option<String>,
    /// 返信先。サーバー応答の入れ子はそのままデコードして保持する
    /// (io は最大 1 段を返す実測)。表示の深さ制限(F-05-4)は UI 層の責務
    #[serde(default)]
    pub reply: Option<Box<Note>>,
    /// リノート元。同上
    #[serde(default)]
    pub renote: Option<Box<Note>>,
    /// 絵文字名→個数(F-07-1)
    #[serde(default)]
    pub reactions: HashMap<String, u32>,
    /// リアクション絵文字名→URL(表示用)
    #[serde(default)]
    pub reaction_emojis: HashMap<String, String>,
    /// 本文中のカスタム絵文字名→URL(F-05-2)
    #[serde(default)]
    pub emojis: HashMap<String, String>,
    #[serde(default)]
    pub file_ids: Vec<String>,
    #[serde(default)]
    pub files: Vec<DriveFile>,
    /// 自分がつけたリアクション(絵文字名)。未付与は null
    #[serde(default)]
    pub my_reaction: Option<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub replies_count: u32,
    #[serde(default)]
    pub renote_count: u32,
    /// リモートノートの URI/URL(外部ブラウザ起動用、F-08-3)
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

impl Note {
    /// 自身のコンテンツ(本文・CW・添付)を持たない純粋なリノートかどうか
    /// (F-03-4 のリノートフィルタ)。renote_id を持ちながら自身の添付などを
    /// 持つものは引用であり、ここには含めない
    pub fn is_pure_renote(&self) -> bool {
        self.renote_id.is_some()
            && self.text.is_none()
            && self.cw.is_none()
            && self.files.is_empty()
            && self.file_ids.is_empty()
    }
}

/// `POST /api/i/notifications` の通知(F-04)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub id: String,
    pub created_at: String,
    /// 種別(follow、reaction、renote、reply、quote、mention など)。
    /// io フォーク独自の種別があり得るため文字列のまま保持し、フィルタは文字列比較で行う
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub note: Option<Note>,
    /// リアクション通知の絵文字名(F-07-1)
    #[serde(default)]
    pub reaction: Option<String>,
    #[serde(default)]
    pub is_read: bool,
}

/// チャンネル(F-03-7)。channels/featured・search・followed の実測キーを根拠に抜粋
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_noted_at: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub banner_url: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub notes_count: u32,
    #[serde(default)]
    pub users_count: u32,
    /// followed 一覧で返るフォロー中フラグ
    #[serde(default)]
    pub is_following: Option<bool>,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_sensitive: bool,
}

/// カスタム絵文字(F-05-2、F-07-3)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Emoji {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub is_sensitive: bool,
    #[serde(default)]
    pub local_only: bool,
}

/// `POST /api/emojis` の応答ラッパー(io 実測: `{"emojis":[...]}` で包まれる)
#[derive(Debug, Deserialize)]
pub struct EmojisResponse {
    pub emojis: Vec<Emoji>,
}

/// `POST /api/meta` のインスタンス情報。N-05 の機能差検出に使う
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstanceMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// io 実測では localTimeline・globalTimeline・miauth・registration 等の真偽値。
    /// 未知キーも保持するためマップのまま受け取る
    #[serde(default)]
    pub features: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub max_note_text_length: Option<u32>,
}

impl InstanceMeta {
    /// 機能フラグの参照。キーが無いか真偽値でなければ None
    pub fn feature(&self, name: &str) -> Option<bool> {
        self.features.get(name)?.as_bool()
    }
}
