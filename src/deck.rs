//! デッキのカラム状態機械(F-02/F-03/F-04)。
//! UI と非同期イベント(REST 結果・ストリーミング差分)の両方から
//! 同じメソッドで更新できるよう、egui や tokio に依存しない純粋データ層にする。
//!
//! ノート・通知 ID は AID 系(先頭に時刻が埋め込まれる)で辞書順が時系列と
//! 一致する(io/本家共通の ID 生成規則に依存した実装上の前提)ため、
//! 表示順は ID の降順ソートで維持し、過去ページも欠落補充分も
//! 単純な二分挿入で正しい位置に収まる。

use crate::api;
use crate::config::{ColumnFilters, ColumnKind, ColumnSpec, TimelineKind};
use crate::model::{Channel, Note, Notification};
use crate::streaming::StreamChannel;
use std::collections::{HashSet, VecDeque};

/// カラム幅の許容範囲(px)。F-02-1 の幅変更でクランプする
pub const COL_WIDTH_MIN: f32 = 180.0;
pub const COL_WIDTH_MAX: f32 = 520.0;
pub const COL_WIDTH_DEFAULT: f32 = 300.0;
/// カラムに保持する項目数の上限。古い側から落とす(N-01 のメモリ目標のための実装上限)
const ITEMS_CAP: usize = 500;

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

/// 既受信 ID の上限つき集合(メモリ増殖対策)。挿入順を別に持ち、
/// 上限超過で最古から捨てる。cap を超えると dedup は緩むが、
/// 長時間稼働での無制限増殖を止める方を優先する
struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

/// dedup 集合の上限。表示 500 + フィルタ落ち + 補充分を余裕で覆う
const SEEN_CAP: usize = 10_000;
/// 一時停止の保留上限。溢れた分は補充で再取得する前提で捨てる
const PENDING_CAP: usize = 500;

impl SeenIds {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    /// HashSet::insert と同じ意味(新規なら true)
    fn insert(&mut self, id: String) -> bool {
        if !self.set.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > SEEN_CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }

    fn remove(&mut self, id: &str) {
        self.set.remove(id);
    }

    fn clear(&mut self) {
        self.set.clear();
        self.order.clear();
    }
}

/// カラムに並ぶ項目。TL/メンション/チャンネルは Note、通知カラムは Notification
#[derive(Debug)]
pub enum ColumnItems {
    Notes(VecDeque<Note>),
    Notifications(VecDeque<Notification>),
}

impl ColumnItems {
    pub fn len(&self) -> usize {
        match self {
            ColumnItems::Notes(v) => v.len(),
            ColumnItems::Notifications(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 1 カラムの実行時状態
pub struct Column {
    pub id: u64,
    /// 永続化対象の構成(F-02-3)。幅・TL 種別・フィルタなどはここに載せる
    pub spec: ColumnSpec,
    pub paused: bool,
    pub view: ColumnView,
    pub items: ColumnItems,
    /// 表示中・バッファ中を含む既受信 ID(dedup 用)
    seen_ids: SeenIds,
    /// 切断時点の最新 ID(再接続時の欠落補充の起点。補充が完走するまで
    /// 保持し、再切断で上書きしない)
    pub backfill_since: Option<String>,
    /// 複数ページ補充の途中位置。上限で止まったらここから続きを取る
    pub backfill_until: Option<String>,
    /// もう一方の欠落区間(since, until)。停止中に補充と新着の両方向で
    /// 溢れたとき、メインの補充とは別区間として取り切るために控える
    pub extra_backfill: Option<(Option<String>, Option<String>)>,
    /// 一時停止バッファが溢れて新着を捨てた。解除時に REST 補充する印
    pub pending_overflow: bool,
    /// invalidate ごとに増える世代番号。飛行中の REST 結果を破棄する印
    pub fetch_gen: u64,
    /// 既受信の最新 ID(欠落補充 sinceId の起点)
    pub newest_id: Option<String>,
    /// 既受信の最古 ID(過去ページ untilId の起点)
    pub oldest_id: Option<String>,
    /// これ以上過去がない(空ページが返った)
    pub exhausted: bool,
    /// REST リクエストが飛行中(重複発行の抑止)
    pub fetching: bool,
    pub error: Option<String>,
    /// 一時停止中に届いたストリーミング分(解除時に合成)
    pending_notes: VecDeque<Note>,
    pending_notifs: VecDeque<Notification>,
    /// 現在購読中のストリーミング購読 ID(app 層の切断・張り直し管理用)
    pub sub_id: Option<String>,
    /// F-04-1 の通知種別フィルタ(通知カラムのみ使用)
    pub ntf_filter: NotificationFilter,
    /// チャンネルカラムの選択 UI(F-03-7)
    pub channel_picker: Option<ChannelPicker>,
    /// 構造変更を行った(設定へ反映が必要)
    pub dirty: bool,
}

impl Column {
    fn new(id: u64, spec: ColumnSpec) -> Self {
        let items = match spec.kind {
            ColumnKind::Notifications => ColumnItems::Notifications(VecDeque::new()),
            _ => ColumnItems::Notes(VecDeque::new()),
        };
        let ntf_excluded = spec.ntf_exclude.iter().cloned().collect();
        Self {
            id,
            spec,
            paused: false,
            view: ColumnView::default(),
            items,
            seen_ids: SeenIds::new(),
            backfill_since: None,
            backfill_until: None,
            extra_backfill: None,
            pending_overflow: false,
            fetch_gen: 0,
            newest_id: None,
            oldest_id: None,
            exhausted: false,
            fetching: false,
            error: None,
            pending_notes: VecDeque::new(),
            pending_notifs: VecDeque::new(),
            sub_id: None,
            ntf_filter: NotificationFilter {
                excluded: ntf_excluded,
            },
            channel_picker: None,
            dirty: false,
        }
    }

    /// このカラムが購読するストリーミングチャンネル。
    /// 通知とメンションは main チャンネル経由の通知イベントを共有する。
    /// メインカラム(投稿欄)は購読なし
    pub fn stream_channel(&self) -> Option<StreamChannel> {
        match self.spec.kind {
            ColumnKind::Timeline => Some(StreamChannel::Timeline(
                self.spec.timeline.unwrap_or(TimelineKind::Home),
            )),
            ColumnKind::Channel => self.spec.channel_id.clone().map(StreamChannel::Channel),
            ColumnKind::Notifications | ColumnKind::Mentions => Some(StreamChannel::Main),
            ColumnKind::Main => None,
        }
    }

    /// カラムの表示名(ヘッダー用)
    pub fn title(&self) -> String {
        match self.spec.kind {
            ColumnKind::Main => "メイン".to_owned(),
            ColumnKind::Timeline => self
                .spec
                .timeline
                .map(timeline_label)
                .unwrap_or("タイムライン")
                .to_owned(),
            ColumnKind::Notifications => "通知".to_owned(),
            ColumnKind::Mentions => "メンション".to_owned(),
            ColumnKind::Channel => "チャンネル".to_owned(),
        }
    }

    /// ストリーミングから届いたノートを挿入する(フィルタ適用済み前提ではなく
    /// ここでフィルタと dedup を適用する)。一時停止中は保留に積む。
    /// 挿入した(=画面に出た)ら true
    pub fn push_note(&mut self, note: Note) -> bool {
        if self.seen_ids.contains(&note.id) {
            return false;
        }
        if !api::note_allowed(&note, &self.spec.filters) {
            self.seen_ids.insert(note.id);
            return false;
        }
        if self.paused {
            if self.pending_notes.len() >= PENDING_CAP {
                // seen に入れずに捨て、解除時の REST 補充で拾えるようにする
                self.pending_overflow = true;
                return false;
            }
            self.seen_ids.insert(note.id.clone());
            self.pending_notes.push_back(note);
            return false;
        }
        self.seen_ids.insert(note.id.clone());
        self.insert_note_sorted(note);
        true
    }

    /// ソート位置に挿入するだけ(cap 適用は呼び側が方向で選ぶ)
    fn insert_note_at(&mut self, note: Note) {
        self.bump_cursors(Some(&note.id), Some(&note.id));
        if let ColumnItems::Notes(items) = &mut self.items {
            let pos = items.partition_point(|existing| existing.id > note.id);
            items.insert(pos, note);
        }
    }

    /// 新着方向の挿入。上限超過は末尾(最古)を落とす
    fn insert_note_sorted(&mut self, note: Note) {
        self.insert_note_at(note);
        if let ColumnItems::Notes(items) = &mut self.items {
            items.truncate(ITEMS_CAP);
        }
    }

    /// 過去方向ページ追加後の上限超過は先頭(最新)側を落とす。
    /// 落とした分は seen_ids から外し、newest_id も繰り下げて
    /// 補充で再取得できる状態に戻す
    fn trim_front_overflow(&mut self) {
        // 上限を超えたときだけ先頭を落とす。落とさない場合はカーソルを触らない
        let excess = self.items.len().saturating_sub(ITEMS_CAP);
        if excess == 0 {
            return;
        }
        let (dropped, new_front): (Vec<String>, Option<String>) = match &mut self.items {
            ColumnItems::Notes(items) => {
                let dropped: Vec<String> = items.drain(..excess).map(|n| n.id).collect();
                (dropped, items.front().map(|n| n.id.clone()))
            }
            ColumnItems::Notifications(items) => {
                let dropped: Vec<String> = items.drain(..excess).map(|n| n.id).collect();
                (dropped, items.front().map(|n| n.id.clone()))
            }
        };
        for id in dropped {
            self.seen_ids.remove(&id);
        }
        if let Some(f) = new_front {
            self.newest_id = Some(f);
        }
    }

    /// REST の過去ページ(untilId 方向)を末尾へ追加する。
    /// 応答は降順で返るので各件をフィルタ+dedup してから順に挿入する
    /// (混在した新しい項目があってもソート挿入で位置が保たれる)
    pub fn append_page(
        &mut self,
        notes: Vec<Note>,
        raw_oldest: Option<String>,
        raw_newest: Option<String>,
    ) {
        for note in notes {
            if self.seen_ids.insert(note.id.clone()) {
                self.insert_note_at(note);
            }
        }
        // カーソルはフィルタ前の応答から取る(フィルタで全件落ちても辿れる)
        self.bump_cursors(raw_newest.as_deref(), raw_oldest.as_deref());
        // 過去方向で溢れた分は先頭(最新)側を落とす。末尾を落とすと
        // 取ったばかりの古いページが即捨てられて表示されない(BUG 対応)
        self.trim_front_overflow();
        self.fetching = false;
    }

    /// 再接続の欠落補充(新着方向)。一時停止中は保留に積み、
    /// 表示には載せない(F-02-2 の一時停止は更新を止める契約)
    pub fn append_backfill(
        &mut self,
        notes: Vec<Note>,
        raw_oldest: Option<String>,
        raw_newest: Option<String>,
    ) {
        for note in notes {
            if self.seen_ids.contains(&note.id) {
                continue;
            }
            if !api::note_allowed(&note, &self.spec.filters) {
                self.seen_ids.insert(note.id);
                continue;
            }
            if self.paused && self.pending_notes.len() >= PENDING_CAP {
                self.pending_overflow = true;
                continue;
            }
            self.seen_ids.insert(note.id.clone());
            if self.paused {
                self.pending_notes.push_back(note);
                continue;
            }
            self.insert_note_sorted(note);
        }
        self.bump_cursors(raw_newest.as_deref(), raw_oldest.as_deref());
        self.fetching = false;
    }

    /// 通知の過去ページを追加する
    pub fn append_notif_page(
        &mut self,
        notifs: Vec<Notification>,
        raw_oldest: Option<String>,
        raw_newest: Option<String>,
    ) {
        for n in notifs {
            if !self.ntf_filter.allows(&n.kind) {
                self.seen_ids.insert(n.id);
                continue;
            }
            if self.seen_ids.insert(n.id.clone()) {
                self.bump_cursors(None, None);
                if let ColumnItems::Notifications(items) = &mut self.items {
                    let pos = items.partition_point(|existing| existing.id > n.id);
                    items.insert(pos, n);
                }
            }
        }
        self.bump_cursors(raw_newest.as_deref(), raw_oldest.as_deref());
        self.trim_front_overflow();
        self.fetching = false;
    }

    /// 通知の欠落補充(新着方向)。一時停止中は保留に積む
    pub fn append_notif_backfill(
        &mut self,
        notifs: Vec<Notification>,
        raw_oldest: Option<String>,
        raw_newest: Option<String>,
    ) {
        for n in notifs {
            if self.seen_ids.contains(&n.id) {
                continue;
            }
            if !self.ntf_filter.allows(&n.kind) {
                self.seen_ids.insert(n.id);
                continue;
            }
            if self.paused && self.pending_notifs.len() >= PENDING_CAP {
                self.pending_overflow = true;
                continue;
            }
            self.seen_ids.insert(n.id.clone());
            if self.paused {
                self.pending_notifs.push_back(n);
                continue;
            }
            self.bump_cursors(None, None);
            if let ColumnItems::Notifications(items) = &mut self.items {
                let pos = items.partition_point(|existing| existing.id > n.id);
                items.insert(pos, n);
                items.truncate(ITEMS_CAP);
            }
        }
        self.bump_cursors(raw_newest.as_deref(), raw_oldest.as_deref());
        self.fetching = false;
    }

    /// ストリーミングから届いた通知(F-04-1 の種別フィルタ適用)
    pub fn push_notification(&mut self, n: Notification) -> bool {
        if self.seen_ids.contains(&n.id) || !self.ntf_filter.allows(&n.kind) {
            self.seen_ids.insert(n.id);
            return false;
        }
        if self.paused {
            if self.pending_notifs.len() >= PENDING_CAP {
                self.pending_overflow = true;
                return false;
            }
            self.seen_ids.insert(n.id.clone());
            self.pending_notifs.push_back(n);
            return false;
        }
        self.seen_ids.insert(n.id.clone());
        self.bump_cursors(Some(&n.id), Some(&n.id));
        if let ColumnItems::Notifications(items) = &mut self.items {
            let pos = items.partition_point(|existing| existing.id > n.id);
            items.insert(pos, n);
            if items.len() > ITEMS_CAP {
                items.truncate(ITEMS_CAP);
            }
        }
        true
    }

    /// 保留中の件数(一時停止バッジ用)
    pub fn pending_count(&self) -> usize {
        self.pending_notes.len() + self.pending_notifs.len()
    }

    /// 一時停止の切り替え(F-02-2)。解除時に保留分を合成する。
    /// 保留が溢れて捨てた分があれば解除時に補充起点を立てる
    /// (捨てた側は保留先頭より新しい区間なので、その ID から補充する)
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused && !paused {
            // 溢れで捨てた区間の境界(=残した最古・最新 ID)はドレイン前に採る。
            // 生応答のカーソルで newest_id は進むので、捨てた分を含まない
            // 実バッファ基準の値を使わないと区間を飛び越す
            let (oldest_kept, newest_kept) = if self.pending_overflow {
                let ids: Vec<&String> = self
                    .pending_notes
                    .iter()
                    .map(|n| &n.id)
                    .chain(self.pending_notifs.iter().map(|n| &n.id))
                    .collect();
                (
                    ids.iter().min().map(|s| (*s).clone()),
                    ids.iter().max().map(|s| (*s).clone()),
                )
            } else {
                (None, None)
            };
            while let Some(n) = self.pending_notes.pop_front() {
                self.insert_note_sorted(n);
            }
            while let Some(n) = self.pending_notifs.pop_front() {
                if let ColumnItems::Notifications(items) = &mut self.items {
                    let pos = items.partition_point(|existing| existing.id > n.id);
                    items.insert(pos, n);
                }
            }
            if self.pending_overflow {
                self.pending_overflow = false;
                if self.backfill_since.is_some() {
                    // 補充の途中で溢れた: 歩き切れていない区間を
                    // until→since で解除後に取り切る
                    self.backfill_until = oldest_kept;
                    // 補充と同時に届いた分が溢れた場合、残した最新側より
                    // 新しい区間も欠落しているので別区間として控える。
                    // 起点は実バッファの最新 ID(生カーソルは捨てた分を含む)
                    self.extra_backfill = Some((newest_kept, None));
                } else {
                    // ストリーミング分だけの溢れ: 捨てた分は合成結果の
                    // 最前端より新しい区間なので、その ID を起点に補充する
                    self.backfill_since = self.newest_id.clone();
                }
            }
        }
        self.paused = paused;
    }

    /// 新しい ID を受け取るたびにカーソルを広げる
    /// (AID 系は辞書順=時系列のため max/min 比較で更新できる)
    fn bump_cursors(&mut self, newest: Option<&str>, oldest: Option<&str>) {
        if let Some(id) = newest
            && self.newest_id.as_deref().is_none_or(|cur| id > cur)
        {
            self.newest_id = Some(id.to_owned());
        }
        if let Some(id) = oldest
            && self.oldest_id.as_deref().is_none_or(|cur| id < cur)
        {
            self.oldest_id = Some(id.to_owned());
        }
    }

    /// 内容を空にして再読み込みが必要な状態にする
    /// (タイムライン種別・フィルタ・チャンネル変更時)
    pub fn invalidate(&mut self) {
        match &mut self.items {
            ColumnItems::Notes(v) => v.clear(),
            ColumnItems::Notifications(v) => v.clear(),
        }
        self.seen_ids.clear();
        self.pending_notes.clear();
        self.pending_notifs.clear();
        self.newest_id = None;
        self.oldest_id = None;
        self.exhausted = false;
        self.fetching = false;
        self.error = None;
        self.view = ColumnView::default();
        self.backfill_since = None;
        self.backfill_until = None;
        self.extra_backfill = None;
        self.pending_overflow = false;
        self.fetch_gen += 1;
        self.dirty = true;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str) -> Note {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "createdAt": "2026-10-07T00:00:00.000Z",
            "userId": "u1",
            "user": { "id": "u1", "username": "alice" },
        }))
        .unwrap()
    }

    fn notif(id: &str, kind: &str) -> Notification {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "createdAt": "2026-10-07T00:00:00.000Z",
            "type": kind,
        }))
        .unwrap()
    }

    fn tl_column() -> Column {
        Column::new(
            1,
            ColumnSpec {
                kind: ColumnKind::Timeline,
                timeline: Some(TimelineKind::Home),
                ..ColumnSpec::default()
            },
        )
    }

    fn note_ids(col: &Column) -> Vec<String> {
        match &col.items {
            ColumnItems::Notes(v) => v.iter().map(|n| n.id.clone()).collect(),
            _ => vec![],
        }
    }

    // COL-06: ストリーム差分は ID dedup され ID 降順(新しい順)で保持される(F-03-2)
    #[test]
    fn col06_push_dedup_sorted() {
        let mut col = tl_column();
        assert!(col.push_note(note("a2")));
        assert!(col.push_note(note("a1")));
        assert!(col.push_note(note("a3")));
        assert!(!col.push_note(note("a2"))); // 重複は挿入されない
        assert_eq!(note_ids(&col), vec!["a3", "a2", "a1"]);
    }

    // COL-07: 一時停止中は保留に積み、解除時に順序を保って合成する(F-02-2)
    #[test]
    fn col07_pause_buffer() {
        let mut col = tl_column();
        col.push_note(note("a1"));
        col.set_paused(true);
        assert!(!col.push_note(note("a3")));
        assert!(!col.push_note(note("a2")));
        assert_eq!(col.pending_count(), 2);
        assert_eq!(note_ids(&col), vec!["a1"]);
        col.set_paused(false);
        assert_eq!(col.pending_count(), 0);
        assert_eq!(note_ids(&col), vec!["a3", "a2", "a1"]);
    }

    // COL-08: 過去ページは末尾へ追加され、生応答の最古 ID が untilId カーソルになる(F-03-3)
    #[test]
    fn col08_append_page_cursor() {
        let mut col = tl_column();
        col.append_page(
            vec![note("a5"), note("a4")],
            Some("a4".to_owned()),
            Some("a5".to_owned()),
        );
        col.append_page(
            vec![note("a2"), note("a1")],
            Some("a1".to_owned()),
            Some("a2".to_owned()),
        );
        assert_eq!(note_ids(&col), vec!["a5", "a4", "a2", "a1"]);
        assert_eq!(col.oldest_id.as_deref(), Some("a1"));
        assert_eq!(col.newest_id.as_deref(), Some("a5"));
    }

    // COL-09: 再接続後の欠落補充(sinceId 分)は既存と重複排除して合成される(F-03-6)
    #[test]
    fn col09_backfill_merge() {
        let mut col = tl_column();
        col.append_page(
            vec![note("a3"), note("a1")],
            Some("a1".to_owned()),
            Some("a3".to_owned()),
        );
        // 欠落分(a4)と既知(a3)が混ざって届く
        col.append_page(
            vec![note("a5"), note("a4"), note("a3")],
            Some("a3".to_owned()),
            Some("a5".to_owned()),
        );
        assert_eq!(note_ids(&col), vec!["a5", "a4", "a3", "a1"]);
        assert_eq!(col.newest_id.as_deref(), Some("a5"));
    }

    // COL-10: カラム種別→購読チャンネルの対応(F-03-5)
    #[test]
    fn col10_stream_channel_map() {
        let mut tl = tl_column();
        assert_eq!(
            tl.stream_channel(),
            Some(StreamChannel::Timeline(TimelineKind::Home))
        );
        tl.spec.timeline = Some(TimelineKind::Social);
        assert_eq!(
            tl.stream_channel(),
            Some(StreamChannel::Timeline(TimelineKind::Social))
        );

        let ntf = Column::new(
            2,
            ColumnSpec {
                kind: ColumnKind::Notifications,
                ..Default::default()
            },
        );
        assert_eq!(ntf.stream_channel(), Some(StreamChannel::Main));
        let men = Column::new(
            3,
            ColumnSpec {
                kind: ColumnKind::Mentions,
                ..Default::default()
            },
        );
        assert_eq!(men.stream_channel(), Some(StreamChannel::Main));

        let ch = Column::new(
            4,
            ColumnSpec {
                kind: ColumnKind::Channel,
                channel_id: Some("c1".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            ch.stream_channel(),
            Some(StreamChannel::Channel("c1".to_owned()))
        );
        let ch_unset = Column::new(
            5,
            ColumnSpec {
                kind: ColumnKind::Channel,
                ..Default::default()
            },
        );
        assert_eq!(ch_unset.stream_channel(), None);

        let main = Column::new(
            6,
            ColumnSpec {
                kind: ColumnKind::Main,
                ..Default::default()
            },
        );
        assert_eq!(main.stream_channel(), None);
    }

    // COL-11: フィルタ・TL 種別・チャンネル変更で内容がクリアされ再取得対象になる(F-03-1/4)
    #[test]
    fn col11_invalidate_on_change() {
        let mut deck = ColumnDeck::from_specs(vec![]);
        let id = deck.columns[0].id;
        deck.columns[0].push_note(note("a1"));
        deck.set_timeline_kind(id, TimelineKind::Local);
        assert_eq!(deck.columns[0].spec.timeline, Some(TimelineKind::Local));
        assert_eq!(note_ids(&deck.columns[0]), Vec::<String>::new());
        assert!(deck.columns[0].oldest_id.is_none());

        let filters = ColumnFilters {
            files_only: true,
            ..Default::default()
        };
        deck.set_filters(id, filters.clone());
        assert_eq!(deck.columns[0].spec.filters, filters);
    }

    // COL-12: フィルタはストリーミング挿入にも適用される(F-03-4)
    #[test]
    fn col12_stream_filter_applied() {
        let mut col = tl_column();
        col.spec.filters.include_renotes = false;
        // 純粋リノート
        let rn = serde_json::from_value::<Note>(serde_json::json!({
            "id": "b1",
            "createdAt": "2026-10-07T00:00:00.000Z",
            "userId": "u1",
            "user": { "id": "u1", "username": "alice" },
            "renoteId": "x1",
        }))
        .unwrap();
        assert!(!col.push_note(rn));
        assert!(col.push_note(note("b2")));
        assert_eq!(note_ids(&col), vec!["b2"]);
    }

    // NTF-02: 通知の種別フィルタをカラム側にも適用する(F-04-1)
    #[test]
    fn ntf02_kind_filter() {
        let mut col = Column::new(
            1,
            ColumnSpec {
                kind: ColumnKind::Notifications,
                ..Default::default()
            },
        );
        col.ntf_filter.excluded.insert("reaction".to_owned());
        assert!(!col.push_notification(notif("n1", "reaction")));
        assert!(col.push_notification(notif("n2", "follow")));
        match &col.items {
            ColumnItems::Notifications(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].kind, "follow");
            }
            _ => panic!(),
        }
    }

    // NTF-03: ページ適用でも種別フィルタが効き、カーソルは生応答から進む(F-04-1/-3)
    #[test]
    fn ntf03_page_filter_cursor() {
        let mut col = Column::new(
            1,
            ColumnSpec {
                kind: ColumnKind::Notifications,
                ..Default::default()
            },
        );
        col.ntf_filter.excluded.insert("reaction".to_owned());
        col.append_notif_page(
            vec![
                notif("n3", "reaction"),
                notif("n2", "follow"),
                notif("n1", "mention"),
            ],
            Some("n1".to_owned()),
            Some("n3".to_owned()),
        );
        match &col.items {
            ColumnItems::Notifications(v) => {
                let kinds: Vec<&str> = v.iter().map(|n| n.kind.as_str()).collect();
                assert_eq!(kinds, vec!["follow", "mention"]);
            }
            _ => panic!(),
        }
        assert_eq!(col.oldest_id.as_deref(), Some("n1"));
        assert_eq!(col.newest_id.as_deref(), Some("n3"));
    }

    /// COL-13: invalidate で取得世代が進む(飛行中の旧世代結果は捨てる)
    #[test]
    fn col13_invalidate_bumps_fetch_gen() {
        let mut col = tl_column();
        let gen0 = col.fetch_gen;
        col.invalidate();
        assert_eq!(col.fetch_gen, gen0 + 1);
        assert!(col.backfill_since.is_none());
    }

    /// COL-14: 一時停止中の欠落補充は保留に積み、解除で合成する
    #[test]
    fn col14_paused_backfill_goes_pending() {
        let mut col = tl_column();
        col.append_page(
            vec![note("n1")],
            Some("n1".to_owned()),
            Some("n1".to_owned()),
        );
        col.set_paused(true);
        col.append_backfill(
            vec![note("n3"), note("n2")],
            Some("n2".to_owned()),
            Some("n3".to_owned()),
        );
        // 一時停止中は表示を増やさない
        assert_eq!(note_ids(&col), vec!["n1"]);
        assert_eq!(col.pending_count(), 2);
        // カーソルは進む(以降の補充の起点)
        assert_eq!(col.newest_id.as_deref(), Some("n3"));
        col.set_paused(false);
        assert_eq!(note_ids(&col), vec!["n3", "n2", "n1"]);
        assert_eq!(col.pending_count(), 0);
    }

    /// COL-15: 過去方向の追加で上限超過したら最新側を落とす
    /// (末尾を落とすと取ったばかりの古いページが見えない)
    #[test]
    fn col15_page_overflow_evicts_front() {
        let mut col = tl_column();
        // 新着方向で cap まで埋める
        for i in 0..ITEMS_CAP {
            col.push_note(note(&format!("n{:06}", i + 1000)));
        }
        let top_before = note_ids(&col)[0].clone();
        // 過去方向に cap を超えるページを追加
        let older: Vec<Note> = (0..20).map(|i| note(&format!("n{:06}", 100 - i))).collect();
        col.append_page(
            older,
            Some("n000081".to_owned()),
            Some("n000100".to_owned()),
        );
        let ids = note_ids(&col);
        assert_eq!(ids.len(), ITEMS_CAP);
        // 末尾(最古側)は新しく取ったページが残る
        assert_eq!(ids.last().unwrap().as_str(), "n000081");
        // 先頭の新着は落ち、newest_id が繰り下がる
        assert_ne!(top_before, ids[0]);
        assert_eq!(col.newest_id.as_deref(), Some(ids[0].as_str()));
    }

    /// COL-16: カラム削除(残存カラムがあってもなくても)で dirty が立つ
    #[test]
    fn col16_remove_marks_deck_dirty() {
        let mut deck = ColumnDeck::from_specs(vec![
            ColumnSpec {
                kind: ColumnKind::Timeline,
                ..Default::default()
            },
            ColumnSpec {
                kind: ColumnKind::Mentions,
                ..Default::default()
            },
        ]);
        deck.take_dirty(); // 初期状態の dirty を消費
        assert!(!deck.take_dirty());
        deck.remove(deck.columns[0].id);
        assert!(deck.take_dirty());
        assert!(!deck.take_dirty());
        // 最後の 1 列の削除でも dirty は立つ
        deck.remove(deck.columns[0].id);
        assert!(deck.take_dirty());
    }

    /// NTF-04: 通知種別フィルタはカラム構成として保存・復元される(F-02-3/F-09-2)
    #[test]
    fn ntf04_filter_persists_in_spec() {
        let mut deck = ColumnDeck::from_specs(vec![ColumnSpec {
            kind: ColumnKind::Notifications,
            ..Default::default()
        }]);
        let id = deck.columns[0].id;
        let mut filter = NotificationFilter::default();
        filter.excluded.insert("reaction".to_owned());
        deck.set_ntf_filter(id, filter);
        // 保存される構成に除外種別が含まれる
        assert_eq!(deck.specs()[0].ntf_exclude, vec!["reaction".to_owned()]);
        // 復元したカラムはフィルタが効いた状態で再構成される
        let deck2 = ColumnDeck::from_specs(deck.specs());
        assert!(deck2.columns[0].ntf_filter.excluded.contains("reaction"));
    }

    /// COL-17: 一時停止バッファが溢れたら解除時に補充起点を立てる。
    /// 捨てた分は保留先頭より新しい区間なので、その新 ID を sinceId にする
    #[test]
    fn col17_pending_overflow_marks_backfill() {
        let mut col = tl_column();
        col.set_paused(true);
        for i in 0..PENDING_CAP + 1 {
            col.push_note(note(&format!("n{i:04}")));
        }
        assert!(col.pending_overflow);
        col.set_paused(false);
        assert!(!col.pending_overflow);
        // 保留先頭(=直近の新着)以降の欠落を拾うためその ID が起点になる
        assert_eq!(col.backfill_since, col.newest_id);
        assert_eq!(col.backfill_since.as_deref(), Some("n0499"));
    }

    /// COL-18: 補充の途中で保留が溢れた場合、歩き切れていない区間を
    /// until→since で解除後に取り切る(捨てた分は seen に残さない)
    #[test]
    fn col18_paused_backfill_overflow_resumes_gap() {
        let mut col = tl_column();
        col.backfill_since = Some("n0000".to_owned());
        col.set_paused(true);
        // 補充が n0600..n0001 を降順で流す(600 件 > 保留上限 500)
        let notes: Vec<Note> = (1..=600).rev().map(|i| note(&format!("n{i:04}"))).collect();
        col.append_backfill(notes, Some("n0001".to_owned()), Some("n0600".to_owned()));
        assert!(col.pending_overflow);
        col.set_paused(false);
        assert_eq!(col.backfill_since.as_deref(), Some("n0000"));
        // 残した最古 ID が続き位置になる(n0100 以下が落ちた区間)
        assert_eq!(col.backfill_until.as_deref(), Some("n0101"));
        // 残した最新側より新しい欠落の控えも立つ(この例では新着なし)
        assert_eq!(col.extra_backfill, Some((Some("n0600".to_owned()), None)));
        // 捨てた分は seen に入っていないので補充で拾い直せる
        assert!(col.push_note(note("n0050")));
    }

    /// COL-19: 停止中に補充と新着が同時に溢れたら、古い側は until→since、
    /// 新しい側は別区間の sinceId で両方取り切る状態を作る
    #[test]
    fn col19_both_directions_refilled() {
        let mut col = tl_column();
        col.backfill_since = Some("n0000".to_owned());
        col.set_paused(true);
        // 補充がバッファを埋める(n0600..n0101)
        let notes: Vec<Note> = (101..=600)
            .rev()
            .map(|i| note(&format!("n{i:04}")))
            .collect();
        col.append_backfill(notes, Some("n0101".to_owned()), Some("n0600".to_owned()));
        // そのあと届いた新着は溢れで捨てられる
        assert!(!col.push_note(note("n0700")));
        col.set_paused(false);
        // 古い側: 残した最古から切断境界まで
        assert_eq!(col.backfill_until.as_deref(), Some("n0101"));
        assert_eq!(col.backfill_since.as_deref(), Some("n0000"));
        // 新しい側: 残した最新から先(落ちた n0700 を拾う区間)
        assert_eq!(col.extra_backfill, Some((Some("n0600".to_owned()), None)));
        // 落ちた新着は seen に残っていないので拾い直せる
        assert!(col.push_note(note("n0700")));
    }

    /// COL-20: ストリーミングが先にバッファを埋めてから補充が来た場合、
    /// 捨てた補充分を挟んで新しい側の区間も拾う(生カーソルではなく
    /// 実バッファの最新 ID を起点にする)
    #[test]
    fn col20_streaming_fills_buffer_before_backfill() {
        let mut col = tl_column();
        col.backfill_since = Some("n0000".to_owned());
        col.set_paused(true);
        // ストリーミング分でバッファを埋める(n0001..n0500)
        for i in 1..=PENDING_CAP {
            col.push_note(note(&format!("n{i:04}")));
        }
        // 遅れて届いた補充(n1100..n0001)は全部溢れで捨てられるが、
        // 生カーソルで newest_id は n1100 まで進む
        let notes: Vec<Note> = (1..=1100)
            .rev()
            .map(|i| note(&format!("n{i:04}")))
            .collect();
        col.append_backfill(notes, Some("n0001".to_owned()), Some("n1100".to_owned()));
        assert_eq!(col.newest_id.as_deref(), Some("n1100"));
        col.set_paused(false);
        // 古い側: 残した最古から切断境界まで
        assert_eq!(col.backfill_until.as_deref(), Some("n0001"));
        assert_eq!(col.backfill_since.as_deref(), Some("n0000"));
        // 新しい側は「残した最新 n0500」起点 — 生カーソル n1100 を
        // 起点にすると捨てた n0501..n1100 を永遠に飛び越す
        assert_eq!(col.extra_backfill, Some((Some("n0500".to_owned()), None)));
        // 捨てた補充分は seen に残っていないので拾い直せる
        assert!(col.push_note(note("n0700")));
    }

    /// COL-21: 設定パネルの ←→ 相対移動(move_delta)が列を交換し、端では無操作
    #[test]
    fn col21_move_delta_swaps_and_clamps() {
        let mut deck = ColumnDeck::from_specs(vec![
            ColumnSpec {
                kind: ColumnKind::Timeline,
                ..Default::default()
            },
            ColumnSpec {
                kind: ColumnKind::Mentions,
                ..Default::default()
            },
            ColumnSpec {
                kind: ColumnKind::Notifications,
                ..Default::default()
            },
        ]);
        deck.take_dirty();
        let ids: Vec<u64> = deck.columns.iter().map(|c| c.id).collect();
        let kind_at = |d: &ColumnDeck, i: usize| d.columns[i].spec.kind;

        // 中央を右へ → 1 つ右と交換
        deck.move_delta(ids[1], 1);
        assert_eq!(kind_at(&deck, 2), ColumnKind::Mentions);
        assert_eq!(kind_at(&deck, 1), ColumnKind::Notifications);
        assert!(deck.take_dirty());

        // 右端を右へ → 無操作
        deck.move_delta(ids[1], 1);
        assert_eq!(kind_at(&deck, 2), ColumnKind::Mentions);

        // 左端を左へ → 無操作
        deck.move_delta(ids[0], -1);
        assert_eq!(kind_at(&deck, 0), ColumnKind::Timeline);

        // 左端を右へ
        deck.move_delta(ids[0], 1);
        assert_eq!(kind_at(&deck, 1), ColumnKind::Timeline);
        assert_eq!(kind_at(&deck, 0), ColumnKind::Notifications);
    }
}
