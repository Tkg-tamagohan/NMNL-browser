use super::column::{ITEMS_CAP, PENDING_CAP};
use super::*;
use crate::model::Notification;
use crate::streaming::StreamChannel;

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

/// mock/ の既定構成に相当する 3 カラムのデッキ(ホーム、ローカル、通知)
fn three_column_deck() -> ColumnDeck {
    ColumnDeck::from_specs(vec![
        ColumnSpec {
            kind: ColumnKind::Timeline,
            timeline: Some(TimelineKind::Home),
            ..ColumnSpec::default()
        },
        ColumnSpec {
            kind: ColumnKind::Timeline,
            timeline: Some(TimelineKind::Local),
            ..ColumnSpec::default()
        },
        ColumnSpec {
            kind: ColumnKind::Notifications,
            ..ColumnSpec::default()
        },
    ])
}

// COL-01: カラムの追加と削除(F-02-1)
#[test]
fn col01_add_remove() {
    let mut deck = three_column_deck();
    let initial = deck.columns.len();
    let id = deck.add(AddableKind::Timeline(TimelineKind::Global));
    assert_eq!(deck.columns.len(), initial + 1);
    let added = deck.columns.last().unwrap();
    assert_eq!(added.spec.kind, ColumnKind::Timeline);
    assert_eq!(added.spec.timeline, Some(TimelineKind::Global));
    deck.remove(id);
    assert_eq!(deck.columns.len(), initial);
    assert!(deck.columns.iter().all(|c| c.id != id));
}

// COL-02: ドラッグによる並べ替え(F-02-1)
#[test]
fn col02_move() {
    let mut deck = three_column_deck();
    let first = deck.columns[0].id;
    let last = deck.columns[deck.columns.len() - 1].id;
    // 先頭を末尾へ
    deck.move_to(first, deck.columns.len());
    assert_eq!(deck.columns.last().unwrap().id, first);
    // 末尾を先頭へ
    deck.move_to(last, 0);
    assert_eq!(deck.columns[0].id, last);
}

// COL-03: 幅の変更とクランプ(F-02-1)
#[test]
fn col03_width_clamp() {
    let mut deck = three_column_deck();
    let id = deck.columns[0].id;
    deck.set_width(id, 400.0);
    assert_eq!(deck.columns[0].spec.width, 400.0);
    deck.set_width(id, 10.0);
    assert_eq!(deck.columns[0].spec.width, COL_WIDTH_MIN);
    deck.set_width(id, 9999.0);
    assert_eq!(deck.columns[0].spec.width, COL_WIDTH_MAX);
}

// COL-04: カラムごとの更新一時停止(F-02-2)
#[test]
fn col04_pause() {
    let mut deck = three_column_deck();
    let id = deck.columns[0].id;
    deck.set_paused(id, true);
    assert!(deck.columns[0].paused);
    deck.set_paused(id, false);
    assert!(!deck.columns[0].paused);
}

// COL-05: 並べ替えの区切り位置補正(F-02-1、Devin Review 指摘の回帰)
#[test]
fn col05_move_boundary() {
    let mut deck = three_column_deck();
    let ids: Vec<u64> = deck.columns.iter().map(|c| c.id).collect();
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    // [A,B,C] で A を区切り 2(B|C 間)へ → [B,A,C]
    deck.move_to(a, 2);
    assert_eq!(
        deck.columns.iter().map(|x| x.id).collect::<Vec<_>>(),
        vec![b, a, c]
    );
    // [B,A,C] で C を区切り 1(B|A 間)へ → [B,C,A]
    deck.move_to(c, 1);
    assert_eq!(
        deck.columns.iter().map(|x| x.id).collect::<Vec<_>>(),
        vec![b, c, a]
    );
    // 同一区切りへの移動は変化なし(A の左区切り 2 へ A を移動)
    deck.move_to(a, 2);
    assert_eq!(
        deck.columns.iter().map(|x| x.id).collect::<Vec<_>>(),
        vec![b, c, a]
    );
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

// CH-03: チャンネル名クリックで channel カラムを開く(F-05-7・決定 Q)。
// 無ければ末尾に追加、既存があれば先頭のカラムの対象を差し替える
#[test]
fn ch03_open_channel_column() {
    // channel カラムが無いときは新規追加して取得対象にする
    let mut deck = ColumnDeck::from_specs(vec![ColumnSpec {
        kind: ColumnKind::Timeline,
        ..Default::default()
    }]);
    let (id, need) = deck.open_channel_column("ch1");
    let col = deck.columns.iter().find(|c| c.id == id).unwrap();
    assert_eq!(col.spec.kind, ColumnKind::Channel);
    assert_eq!(col.spec.channel_id.as_deref(), Some("ch1"));
    assert!(need);

    // 既存の channel カラムがあるときは対象を差し替える
    let (id2, need2) = deck.open_channel_column("ch2");
    assert_eq!(id2, id);
    assert_eq!(
        deck.columns[col_index(&deck, id2)]
            .spec
            .channel_id
            .as_deref(),
        Some("ch2")
    );
    assert!(need2);
    // 同じチャンネルを再度開くときは差し替え不要(取得も不要)
    let idx = col_index(&deck, id2);
    deck.columns[idx].push_note(note("a1"));
    let (id3, need3) = deck.open_channel_column("ch2");
    assert_eq!(id3, id2);
    assert!(!need3);
    // 会話ビューを開いたまま同じチャンネル名を押しても TL へ戻る
    deck.columns[idx].view = ColumnView::Conversation {
        root_id: "a1".to_owned(),
        notes: vec![],
        loading: false,
        error: None,
    };
    let (_, need4) = deck.open_channel_column("ch2");
    assert!(matches!(deck.columns[idx].view, ColumnView::Timeline));
    assert!(!need4);
}

// REA-04(追加面): ローカル絵文字の `:name:`/`:name@.:` 形式違いを
// 同名の既存エントリへ統合し、バッジが分裂しないようにする
#[test]
fn rea04_reaction_alias_merge() {
    let mut col = Column::new(
        1,
        ColumnSpec {
            kind: ColumnKind::Timeline,
            ..Default::default()
        },
    );
    let mut n = note("a1");
    // サーバーが `:name:` 形式で返してきた既存リアクション
    n.reactions.insert(":cat:".to_owned(), 2);
    col.push_note(n);
    // `:cat@.:` を送った結果も既存エントリに統合される
    col.apply_reaction("a1", ":cat@.:", true);
    let ColumnItems::Notes(notes) = &col.items else {
        panic!()
    };
    let n2 = &notes[0];
    assert_eq!(n2.reactions.len(), 1);
    assert_eq!(n2.reactions.get(":cat:"), Some(&3));
    assert_eq!(n2.my_reaction.as_deref(), Some(":cat@.:"));
    // 取消も同じエントリを減らす
    col.apply_reaction("a1", ":cat@.:", false);
    let ColumnItems::Notes(notes) = &col.items else {
        panic!()
    };
    let n3 = &notes[0];
    assert_eq!(n3.reactions.get(":cat:"), Some(&2));
    assert_eq!(n3.my_reaction, None);
}

// POST-07: 投稿フォームはメインカラム内に表示し、メインカラムが
// 無いときだけボトムパネル側に出す(F-06-5、仕様決定 X)
#[test]
fn post07_main_column_predicate() {
    // 既定構成にはメインカラムが含まれる
    let deck = ColumnDeck::from_specs(crate::config::AppConfig::default().columns);
    assert!(deck.has_main_column());
    // メインだけ無い構成では false
    let mut specs = crate::config::AppConfig::default().columns;
    specs.retain(|s| !matches!(s.kind, ColumnKind::Main));
    let deck = ColumnDeck::from_specs(specs);
    assert!(!deck.has_main_column());
    // 空デッキでも false(パネル側が投稿経路を引き受ける)
    let deck = ColumnDeck::from_specs(vec![]);
    assert!(!deck.has_main_column());
}

fn col_index(deck: &ColumnDeck, id: u64) -> usize {
    deck.columns.iter().position(|c| c.id == id).unwrap()
}
