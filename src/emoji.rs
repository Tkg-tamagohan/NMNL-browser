//! カスタム絵文字の解決(F-05-2)。
//! ノートが返す `emojis` マップ(name→URL)を最優先し、欠落分は
//! `/api/emoji` でオンデマンド取得する。取得結果はアプリ全体で共有し、
//! 未解決の間は `:name:` テキスト表示のまま描く(モックと同じ見た目)。

use std::collections::{HashMap, HashSet};

/// アプリ全体の絵文字 URL キャッシュ。
/// `None` は「サーバーに存在しない(404 など)」のキャッシュで、繰り返し
/// 問い合わせないための否定キャッシュ
#[derive(Default)]
pub struct EmojiCache {
    resolved: HashMap<String, Option<String>>,
    /// 取得を投げた名前一覧(重複リクエスト防止)
    in_flight: HashSet<String>,
}

impl EmojiCache {
    /// ノートの絵文字マップをまとめて取り込む(F-05-2 の優先ソース)。
    /// 既に個別取得で判明した否定を上書きしないよう、あるキーは何もしない
    pub fn absorb_note_emojis(&mut self, emojis: &HashMap<String, String>) {
        for (name, url) in emojis {
            self.resolved
                .entry(name.clone())
                .or_insert_with(|| Some(url.clone()));
        }
    }

    /// 名前を解決する。URL があれば Some(url)、未解決なら取得を予約して None。
    /// 否定キャッシュ済みも None
    pub fn resolve(&mut self, name: &str) -> Option<String> {
        match self.resolved.get(name) {
            Some(url) => url.clone(),
            None => {
                if self.in_flight.insert(name.to_owned()) {
                    // 呼び出し側(app)が drain して fetch を spawn する
                }
                None
            }
        }
    }

    /// 取得待ちの名前一覧を取り出してクリアする(app の update で回収)
    pub fn drain_pending(&mut self) -> Vec<String> {
        self.in_flight.drain().collect()
    }

    /// 取得完了を記録する(404 は Some(None))
    pub fn complete(&mut self, name: &str, url: Option<String>) {
        self.resolved.insert(name.to_owned(), url);
    }
}

/// ピッカー用絵文字一覧のディスクキャッシュ(F-07-3)。
/// メタ情報(名前/カテゴリ/URL)のみなので量は小さいが、異常に大きい
/// 応答からディスクを守るため上限を設ける
const EMOJI_LIST_CACHE_CAP: u64 = 8 * 1024 * 1024;

fn emoji_list_path() -> std::path::PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.cache_dir().join("nmnl-browser").join("emoji_list.json"))
        .unwrap_or_else(|| std::env::temp_dir().join("nmnl-browser-emoji_list.json"))
}

/// 起動時に一覧を先読みする(F-07-3 のキャッシュ)。上限超過や
/// 壊れたファイルは無視して空を返す
pub fn load_emoji_list() -> Vec<crate::model::Emoji> {
    let path = emoji_list_path();
    let Ok(meta) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    if meta.len() > EMOJI_LIST_CACHE_CAP {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 取得した一覧をディスクに書く(F-07-3)。上限超過なら書かない
pub fn save_emoji_list(list: &[crate::model::Emoji]) {
    let Ok(text) = serde_json::to_string(list) else {
        return;
    };
    if text.len() as u64 > EMOJI_LIST_CACHE_CAP {
        return;
    }
    let path = emoji_list_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, text);
}

/// 一覧キャッシュの現在サイズ(設定画面の表示用、F-09-3)
pub fn emoji_list_bytes() -> u64 {
    std::fs::metadata(emoji_list_path())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// 一覧キャッシュの消去(F-09-3)
pub fn clear_emoji_list() {
    let _ = std::fs::remove_file(emoji_list_path());
}
