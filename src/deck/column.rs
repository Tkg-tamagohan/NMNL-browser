use super::{ChannelPicker, ColumnView, NotificationFilter, timeline_label};
use crate::api;
use crate::config::{ColumnKind, ColumnSpec, TimelineKind};
use crate::model::{Note, Notification, ReactionKey, parse_reaction_key};
use crate::streaming::StreamChannel;
use std::collections::{HashMap, HashSet, VecDeque};

/// カラムに保持する項目数の上限。古い側から落とす(N-01 のメモリ目標のための実装上限)
pub(super) const ITEMS_CAP: usize = 500;

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
pub(super) const PENDING_CAP: usize = 500;

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
    pub(super) fn new(id: u64, spec: ColumnSpec) -> Self {
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

    /// リアクション付与/取消のローカル反映(F-07)。
    /// add=true なら絵文字の個数を加算して my_reaction に記録、false なら
    /// 減算して my_reaction を外す。TL・一時停止バッファ・会話ビューの
    /// すべてを探し、純粋リノートの内側も対象にする(サーバー応答を
    /// 待たず UI へ即時反映するための近似)
    pub fn apply_reaction(&mut self, note_id: &str, reaction: &str, add: bool) {
        fn touch(n: &mut Note, note_id: &str, reaction: &str, add: bool) -> bool {
            if n.id == note_id {
                // ローカル絵文字は `:name:` と `:name@.:` の両形式で来り
                // 得るため、同名の既存エントリがあればそちらを動かして
                // バッジの分裂を防ぐ
                let key = reaction_existing_key(&n.reactions, reaction)
                    .unwrap_or_else(|| reaction.to_owned());
                if add {
                    *n.reactions.entry(key).or_insert(0) += 1;
                    n.my_reaction = Some(reaction.to_owned());
                } else {
                    if let Some(c) = n.reactions.get_mut(&key) {
                        *c = c.saturating_sub(1);
                        if *c == 0 {
                            n.reactions.remove(&key);
                        }
                    }
                    n.my_reaction = None;
                }
                return true;
            }
            if let Some(r) = n.renote.as_deref_mut()
                && touch(r, note_id, reaction, add)
            {
                return true;
            }
            if let Some(r) = n.reply.as_deref_mut() {
                return touch(r, note_id, reaction, add);
            }
            false
        }
        if let ColumnItems::Notes(notes) = &mut self.items {
            for n in notes.iter_mut() {
                touch(n, note_id, reaction, add);
            }
        }
        for n in self.pending_notes.iter_mut() {
            touch(n, note_id, reaction, add);
        }
        if let ColumnView::Conversation { notes, .. } = &mut self.view {
            for n in notes.iter_mut() {
                touch(n, note_id, reaction, add);
            }
        }
    }
}

/// リアクションマップ内の実キーを探す(F-07-4)。
/// ローカル絵文字は `:name:` と `:name@.:` の両形式で来り得るため、
/// 同名の既存エントリがあればそちらのキーを返してバッジを統合する
fn reaction_existing_key(reactions: &HashMap<String, u32>, key: &str) -> Option<String> {
    if reactions.contains_key(key) {
        return Some(key.to_owned());
    }
    if let ReactionKey::Local(name) = parse_reaction_key(key) {
        for alt in [format!(":{name}@.:"), format!(":{name}:")] {
            if reactions.contains_key(&alt) {
                return Some(alt);
            }
        }
    }
    None
}
