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
