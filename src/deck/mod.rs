//! デッキのカラム状態機械(F-02/F-03/F-04)。
//! UI と非同期イベント(REST 結果・ストリーミング差分)の両方から
//! 同じメソッドで更新できるよう、egui や tokio に依存しない純粋データ層にする。
//!
//! ノート・通知 ID は AID 系(先頭に時刻が埋め込まれる)で辞書順が時系列と
//! 一致する(io/本家共通の ID 生成規則に依存した実装上の前提)ため、
//! 表示順は ID の降順ソートで維持し、過去ページも欠落補充分も
//! 単純な二分挿入で正しい位置に収まる。

mod column;
#[cfg(test)]
mod tests;

pub use column::{Column, ColumnItems};

use crate::config::{ColumnFilters, ColumnKind, ColumnSpec, TimelineKind};
use crate::model::{Channel, Note};
use std::collections::HashSet;

/// カラム幅の許容範囲(px)。F-02-1 の幅変更でクランプする
pub const COL_WIDTH_MIN: f32 = 180.0;
pub const COL_WIDTH_MAX: f32 = 520.0;
pub const COL_WIDTH_DEFAULT: f32 = 300.0;

/// カラムの表示ビュー
#[derive(Debug, Default)]
pub enum ColumnView {
    /// 通常のタイムライン/通知表示
    #[default]
    Timeline,
    /// ノート選択で遷移する会話ビュー(F-05-5)
    Conversation {
        /// 選択したノート ID(ハイライト表示用)
        root_id: String,
        /// notes/conversation の取得結果(選択ノート自身は含まない応答)
        notes: Vec<Note>,
        loading: bool,
        error: Option<String>,
    },
}

/// チャンネルカラムの選択 UI 状態(F-03-7)。
/// 選択方法は仕様で「実装時に確定」とあり、フォロー中一覧+検索の組み合わせを採用した
#[derive(Debug, Default)]
pub struct ChannelPicker {
    /// フォロー中チャンネル一覧(channels/followed)
    pub followed: Vec<Channel>,
    /// 検索結果(channels/search)
    pub results: Vec<Channel>,
    pub query: String,
    pub loading: bool,
    /// 読み込み済みフラグ(フォロー中一覧は初回オープン時に一度だけ取得)
    pub followed_loaded: bool,
    /// ピッカー内で出す取得エラー(カラム本体のエラーとは別にする)
    pub error: Option<String>,
}

/// 通知カラムの種別フィルタ(F-04-1)。オフの種別を集合で保持し、
/// REST の excludeTypes とストリーミング側のクライアントフィルタの両方に使う
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationFilter {
    /// 表示しない種別(デフォルトは空=全表示)
    pub excluded: HashSet<String>,
}

impl NotificationFilter {
    /// 通知一覧で選択肢に出す主要種別(io 実測の頻出種別+未収録はその他として通す)
    pub const KNOWN_KINDS: [&'static str; 8] = [
        "follow",
        "mention",
        "reply",
        "quote",
        "renote",
        "reaction",
        "pollEnded",
        "receiveAchievement",
    ];

    pub fn allows(&self, kind: &str) -> bool {
        !self.excluded.contains(kind)
    }

    /// excludeTypes パラメータ用の配列
    pub fn exclude_types(&self) -> Vec<String> {
        self.excluded.iter().cloned().collect()
    }
}

pub fn timeline_label(kind: TimelineKind) -> &'static str {
    match kind {
        TimelineKind::Home => "ホーム",
        TimelineKind::Local => "ローカル",
        TimelineKind::Social => "ソーシャル",
        TimelineKind::Global => "グローバル",
    }
}

/// 追加メニューの選択肢(F-02-1)。TL 系は TimelineKind を内包する
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddableKind {
    Main,
    Timeline(TimelineKind),
    Notifications,
    Mentions,
    Channel,
}

impl AddableKind {
    pub const ALL: [AddableKind; 8] = [
        AddableKind::Main,
        AddableKind::Timeline(TimelineKind::Home),
        AddableKind::Timeline(TimelineKind::Local),
        AddableKind::Timeline(TimelineKind::Social),
        AddableKind::Timeline(TimelineKind::Global),
        AddableKind::Notifications,
        AddableKind::Mentions,
        AddableKind::Channel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AddableKind::Main => "メイン",
            AddableKind::Timeline(k) => timeline_label(k),
            AddableKind::Notifications => "通知",
            AddableKind::Mentions => "メンション",
            AddableKind::Channel => "チャンネル",
        }
    }

    pub fn to_spec(self) -> ColumnSpec {
        match self {
            AddableKind::Main => ColumnSpec {
                kind: ColumnKind::Main,
                ..ColumnSpec::default()
            },
            AddableKind::Timeline(k) => ColumnSpec {
                kind: ColumnKind::Timeline,
                timeline: Some(k),
                ..ColumnSpec::default()
            },
            AddableKind::Notifications => ColumnSpec {
                kind: ColumnKind::Notifications,
                ..ColumnSpec::default()
            },
            AddableKind::Mentions => ColumnSpec {
                kind: ColumnKind::Mentions,
                ..ColumnSpec::default()
            },
            AddableKind::Channel => ColumnSpec {
                kind: ColumnKind::Channel,
                ..ColumnSpec::default()
            },
        }
    }
}

/// デッキ全体(F-02-1 の追加/削除/並べ替え/幅変更 + F-02-3 の構成反映)
pub struct ColumnDeck {
    pub columns: Vec<Column>,
    next_id: u64,
    /// カラム削除など、残ったカラムの dirty では表せない構成変更
    dirty: bool,
}

impl ColumnDeck {
    /// 保存済み構成から復元する。ID は実行時に振り直す(順序で同一性を保つ)
    pub fn from_specs(specs: Vec<ColumnSpec>) -> Self {
        let mut deck = Self {
            columns: Vec::new(),
            next_id: 1,
            dirty: false,
        };
        for spec in specs {
            deck.push(spec);
        }
        if deck.columns.is_empty() {
            deck.push(AddableKind::Timeline(TimelineKind::Home).to_spec());
        }
        deck
    }

    fn push(&mut self, spec: ColumnSpec) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.columns.push(Column::new(id, spec));
        id
    }

    pub fn add(&mut self, kind: AddableKind) -> u64 {
        let mut spec = kind.to_spec();
        spec.width = COL_WIDTH_DEFAULT;
        let id = self.push(spec);
        self.columns.last_mut().unwrap().dirty = true;
        id
    }

    pub fn remove(&mut self, id: u64) {
        let len = self.columns.len();
        self.columns.retain(|c| c.id != id);
        // 削除は残存カラムの dirty では表せないのでデッキ側で持つ
        if self.columns.len() != len {
            self.dirty = true;
        }
    }

    /// ドラッグ並べ替え。`to` は除去前の区切り番号(COL-05 の補正と同じ)
    pub fn move_to(&mut self, id: u64, to: usize) {
        if let Some(from) = self.columns.iter().position(|c| c.id == id) {
            let col = self.columns.remove(from);
            let to = if from < to { to.saturating_sub(1) } else { to };
            let to = to.min(self.columns.len());
            self.columns.insert(to, col);
            self.columns[to].dirty = true;
        }
    }

    /// 相対移動(F-02-4 の ≡ ドラッグ代替経路)。-1 で左、+1 で右。
    /// 端での移動は無操作
    pub fn move_delta(&mut self, id: u64, delta: i32) {
        let Some(from) = self.columns.iter().position(|c| c.id == id) else {
            return;
        };
        let to = from as i64 + delta as i64;
        if to < 0 || to >= self.columns.len() as i64 {
            return;
        }
        self.columns.swap(from, to as usize);
        self.columns[to as usize].dirty = true;
    }

    pub fn set_width(&mut self, id: u64, width: f32) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id) {
            col.spec.width = width.clamp(COL_WIDTH_MIN, COL_WIDTH_MAX);
            col.dirty = true;
        }
    }

    pub fn set_paused(&mut self, id: u64, paused: bool) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id) {
            col.set_paused(paused);
        }
    }

    /// タイムライン種別の切替(F-03-1)。内容をクリアして購読・取得を張り直す
    pub fn set_timeline_kind(&mut self, id: u64, kind: TimelineKind) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id)
            && col.spec.timeline != Some(kind)
        {
            col.spec.timeline = Some(kind);
            col.invalidate();
        }
    }

    /// チャンネルカラムの対象チャンネル設定(F-03-7)
    pub fn set_channel(&mut self, id: u64, channel_id: Option<String>) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id)
            && col.spec.channel_id != channel_id
        {
            col.spec.channel_id = channel_id;
            col.invalidate();
        }
    }

    /// チャンネル名クリックで channel カラムを開く(F-05-7・仕様決定 Q)。
    /// 既存の channel カラムがあれば先頭のものの対象を差し替え、
    /// 無ければ末尾に追加する。戻り値は (カラム ID, 内容取得が必要か)
    pub fn open_channel_column(&mut self, channel_id: &str) -> (u64, bool) {
        let id = self
            .columns
            .iter()
            .find(|c| c.spec.kind == ColumnKind::Channel)
            .map(|c| c.id)
            .unwrap_or_else(|| self.add(AddableKind::Channel));
        let need_fetch = self.columns.iter().find(|c| c.id == id).is_some_and(|c| {
            c.spec.channel_id.as_deref() != Some(channel_id) || c.items.is_empty()
        });
        self.set_channel(id, Some(channel_id.to_owned()));
        // 遷移先はタイムライン: 同じチャンネルでも会話ビューのまま残さない
        // (チャンネル変更時は invalidate が同じことをしている)
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id) {
            col.view = ColumnView::default();
        }
        (id, need_fetch)
    }

    /// 通知フィルタ変更(F-04-1)。サーバー側 excludeTypes にも使うので
    /// 内容クリアして REST から取り直す(フィルタ緩和で過去分を拾うため)
    pub fn set_ntf_filter(&mut self, id: u64, filter: NotificationFilter) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id)
            && col.ntf_filter != filter
        {
            col.spec.ntf_exclude = filter.excluded.iter().cloned().collect();
            col.ntf_filter = filter;
            col.invalidate();
        }
    }

    /// カラム単位フィルタ変更(F-03-4)。同様に取り直す
    pub fn set_filters(&mut self, id: u64, filters: ColumnFilters) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id)
            && col.spec.filters != filters
        {
            col.spec.filters = filters;
            col.invalidate();
        }
    }

    /// 永続化用の構成一覧(F-02-3)
    pub fn specs(&self) -> Vec<ColumnSpec> {
        self.columns.iter().map(|c| c.spec.clone()).collect()
    }

    pub fn take_dirty(&mut self) -> bool {
        let dirty = self.dirty || self.columns.iter().any(|c| c.dirty);
        self.dirty = false;
        for c in &mut self.columns {
            c.dirty = false;
        }
        dirty
    }

    /// メインカラムが配置されているか(F-06-5)。投稿フォームは
    /// メインカラム内に表示し、無いときだけボトムパネルを自動表示する
    pub fn has_main_column(&self) -> bool {
        self.columns
            .iter()
            .any(|c| matches!(c.spec.kind, ColumnKind::Main))
    }
}
