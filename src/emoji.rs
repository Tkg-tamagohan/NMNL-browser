//! カスタム絵文字の解決(F-05-2)。
//! ノートが返す `emojis` マップ(name→URL)を最優先し、欠落分は
//! `/api/emoji` でオンデマンド取得する。取得結果はアプリ全体で共有し、
//! 未解決の間は `:name:` テキスト表示のまま描く(モックと同じ見た目)。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// 一時失敗(429・ネットワーク等)を恒久否定にしないための再試行間隔。
/// なしだと描画ごとに再要求が走りリクエスト嵐になる
pub const RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// 一時失敗の再試行上限。io は存在しない絵文字名にも INTERNAL_ERROR を返すので、
/// そのまま無限再試行すると「存在しない名前」を延々と問い合わせ続ける。
/// 回数上限を超えたら不在扱い(否定キャッシュ)に倒す
const MAX_TRANSIENT_ATTEMPTS: u8 = 5;

/// アプリ全体の絵文字 URL キャッシュ。
/// `None` は「サーバーに存在しない(404 など)」のキャッシュで、繰り返し
/// 問い合わせないための否定キャッシュ
#[derive(Default)]
pub struct EmojiCache {
    resolved: HashMap<String, Option<String>>,
    /// 取得を投げた名前一覧(重複リクエスト防止)
    in_flight: HashSet<String>,
    /// 一時失敗で再試行を待つ名前と再試行可能時刻
    retry_after: HashMap<String, Instant>,
    /// 一時失敗の累計回数(クールダウン経過でリセットしない)
    fails: HashMap<String, u8>,
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
    /// 否定キャッシュ済みも None。一時失敗後はクールダウン経過まで再要求しない
    pub fn resolve(&mut self, name: &str) -> Option<String> {
        if let Some(url) = self.resolved.get(name) {
            return url.clone();
        }
        if let Some(&at) = self.retry_after.get(name) {
            if Instant::now() < at {
                return None;
            }
            // 期限経過で再要求可能にする。失敗回数は fails に残す
            self.retry_after.remove(name);
        }
        self.in_flight.insert(name.to_owned());
        None
    }

    /// 一時的な失敗(429、通信障害など)を記録し、クールダウン後に再試行する。
    /// 上限回数を超えたら不在扱いにして打ち切る
    pub fn fail_transient(&mut self, name: &str) {
        let attempts = self.fails.get(name).copied().unwrap_or(0) + 1;
        if attempts >= MAX_TRANSIENT_ATTEMPTS {
            self.fails.remove(name);
            self.retry_after.remove(name);
            self.resolved.insert(name.to_owned(), None);
        } else {
            self.fails.insert(name.to_owned(), attempts);
            self.retry_after
                .insert(name.to_owned(), Instant::now() + RETRY_COOLDOWN);
        }
    }

    /// 取得待ちの名前一覧を取り出してクリアする(app の update で回収)
    pub fn drain_pending(&mut self) -> Vec<String> {
        self.in_flight.drain().collect()
    }

    /// 取得完了を記録する(404 は Some(None))
    pub fn complete(&mut self, name: &str, url: Option<String>) {
        self.resolved.insert(name.to_owned(), url);
        self.fails.remove(name);
        self.retry_after.remove(name);
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

#[cfg(test)]
mod tests {
    use super::*;

    // EMO-01: 一時失敗は否定キャッシュせず、クールダウン経過後に再要求される。
    // クールダウン中は再要求しない(毎フレームの取得嵐を防ぐ)
    #[test]
    fn emo01_transient_retries_after_cooldown() {
        let mut c = EmojiCache::default();
        assert!(c.resolve("a").is_none());
        assert_eq!(c.drain_pending(), vec!["a".to_owned()]);
        c.fail_transient("a");
        // クールダウン中: 再要求されない
        assert!(c.resolve("a").is_none());
        assert!(c.drain_pending().is_empty());
        // クールダウン経過: 再要求される(否定キャッシュにならない)
        c.retry_after
            .insert("a".to_owned(), Instant::now() - Duration::from_secs(1));
        assert!(c.resolve("a").is_none());
        assert_eq!(c.drain_pending(), vec!["a".to_owned()]);
    }

    // EMO-02: サーバーの「存在しない」応答は否定キャッシュされ再要求しない
    #[test]
    fn emo02_missing_stays_negative_cached() {
        let mut c = EmojiCache::default();
        c.complete("a", None);
        assert!(c.resolve("a").is_none());
        assert!(c.drain_pending().is_empty());
    }

    // EMO-03: 一時失敗が上限回数に達したら不在扱いに倒す(io は存在しない名にも
    // INTERNAL_ERROR を返すので、無限再試行しない)。回数はクールダウン経過の
    // 再要求をまたいで累計される(resolve→失敗のサイクルで上限に達すること)
    #[test]
    fn emo03_transient_gives_up_after_cap() {
        let mut c = EmojiCache::default();
        for _ in 0..MAX_TRANSIENT_ATTEMPTS {
            // resolve で要求 → 取得失敗のライフサイクルを回す
            assert!(c.resolve("a").is_none());
            let _ = c.drain_pending();
            c.fail_transient("a");
            // クールダウンを経過させる(上限到達でエントリが消えた後は触らない)
            if c.retry_after.contains_key("a") {
                c.retry_after
                    .insert("a".to_owned(), Instant::now() - Duration::from_secs(60));
            }
        }
        // 否定キャッシュに倒れて再要求されない
        assert!(c.resolve("a").is_none());
        assert!(c.drain_pending().is_empty());
        assert!(c.retry_after.is_empty());
    }
}
