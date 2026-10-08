//! デッキ UI(F-02)。モックで検証した骨格を実データへ接続したもの。
//! 描画側はアプリ状態への参照を UiCtx にまとめて受け取り、副作用は
//! UiOp として積んで app 層が処理する(モックの Op 方式と同じ)。

mod card;
mod mfm;

pub use card::CardState;
pub use mfm::Piece;

use crate::deck::{
    AddableKind, COL_WIDTH_MAX, COL_WIDTH_MIN, Column, ColumnDeck, ColumnItems, ColumnView,
    NotificationFilter, timeline_label,
};
use crate::emoji::EmojiCache;
use crate::model::User;
use eframe::egui::{self, Color32, Frame, RichText, Sense, Ui, vec2};

/// UI が発行する副作用命令。app 層が受けてデッキ更新・通信を行う
pub enum UiOp {
    AddColumn(AddableKind),
    Remove(u64),
    MoveTo(u64, usize),
    /// 相対移動(-1=左へ、+1=右へ)。≡ ドラッグと併用する別経路(F-02-4)
    MoveDelta(u64, i32),
    SetWidth(u64, f32),
    SetPaused(u64, bool),
    SetTimelineKind(u64, crate::config::TimelineKind),
    SetChannel(u64, Option<String>),
    SetFilters(u64, crate::config::ColumnFilters),
    SetNtfFilter(u64, NotificationFilter),
    /// 過去ページの要求(untilId ページング、F-03-3)
    FetchNextPage(u64),
    /// 会話ビューへの遷移(F-05-5)
    OpenConversation(u64, String),
    CloseConversation(u64),
    /// チャンネル選択 UI の操作(F-03-7)
    ChannelPickerQuery(u64, String),
    ChannelPickerOpen(u64),
    /// 外部ブラウザで開く(F-08-3 など)
    OpenUrl(String),
}

/// 描画に必要なアプリ状態の参照まとめ
pub struct UiCtx<'a> {
    pub ops: &'a mut Vec<UiOp>,
    pub emoji_cache: &'a mut EmojiCache,
    pub card_state: &'a mut CardState,
    /// 設定ポップアップを開いているカラム ID
    pub settings_open: &'a mut Option<u64>,
    /// チャンネル選択ポップアップを開いているカラム ID
    pub picker_open: &'a mut Option<u64>,
    /// 幅ドラッグの開始値(累積 delta を絶対値に変換するため)
    pub resize_base: &'a mut Option<(u64, f32)>,
    /// 追加メニューの選択中カラム種別
    pub add_kind: &'a mut AddableKind,
    /// ストリーミング接続状態の表示用ラベル
    pub stream_status: &'a str,
    /// ログイン中のユーザー(メインカラム表示用)
    pub me: Option<&'a User>,
}

/// 時刻表示。"YYYY-MM-DD HH:MM" の ISO 系文字列から "MM-DD HH:MM" を作る
pub fn fmt_time(created_at: &str) -> String {
    // io の時刻は RFC3339/ISO8601 系("2026-10-07T13:02:33.000Z" 等)
    let t = created_at.trim();
    if t.len() >= 16 && t.as_bytes()[4] == b'-' && t.as_bytes()[7] == b'-' {
        format!("{} {}", &t[5..10], &t[11..16])
    } else {
        t.chars().take(16).collect()
    }
}

/// 毎フレーム呼ぶデッキのエントリポイント
pub fn deck_ui(ctx: &egui::Context, deck: &mut ColumnDeck, ui_ctx: &mut UiCtx<'_>) {
    egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
        top_bar(ui, ui_ctx);
    });
    egui::CentralPanel::default().show(ctx, |ui| {
        // デッキ全体の横スクロール(F-02-5)
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // カラム高さは horizontal 内の available_height ではなく
                // スクロール領域の実高さを使う(内側の値は縮む)
                let col_h = ui.available_height();
                ui.horizontal(|ui| {
                    // 先頭にドロップゾーン(並べ替えの最左端)
                    drop_zone(ui, 0, ui_ctx, col_h);
                    let ids: Vec<u64> = deck.columns.iter().map(|c| c.id).collect();
                    for (i, _id) in ids.iter().enumerate() {
                        // 借用を分けるため columns を分割して列を取り出す
                        let (left, rest) = deck.columns.split_at_mut(i);
                        let _ = left;
                        let col = &mut rest[0];
                        column_panel(ui, col, ui_ctx, col_h);
                        drop_zone(ui, i + 1, ui_ctx, col_h);
                    }
                });
            });
    });
}

fn top_bar(ui: &mut Ui, ctx: &mut UiCtx<'_>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("NMNL-browser").strong());
        ui.separator();
        egui::ComboBox::from_id_salt("add_col")
            .selected_text(format!("＋ {}", ctx.add_kind.label()))
            .show_ui(ui, |ui| {
                for kind in AddableKind::ALL {
                    ui.selectable_value(ctx.add_kind, kind, kind.label());
                }
            });
        if ui.button("追加").clicked() {
            ctx.ops.push(UiOp::AddColumn(*ctx.add_kind));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(ctx.stream_status)
                    .size(11.0)
                    .color(Color32::GRAY),
            );
        });
    });
}

/// 並べ替えドロップゾーン(F-02-2)。細い帯で、ドラッグ中にハイライトされる
fn drop_zone(ui: &mut Ui, index: usize, ctx: &mut UiCtx<'_>, height: f32) {
    // ドラッグ中の強調は dnd_drop_zone が枠の背景で行う
    let (_resp, payload) = ui.dnd_drop_zone::<u64, _>(
        Frame::default().inner_margin(egui::Margin::symmetric(2, 0)),
        |ui| {
            ui.allocate_space(vec2(6.0, height.max(40.0)));
        },
    );
    if let Some(dragged_id) = payload {
        ctx.ops.push(UiOp::MoveTo(*dragged_id, index));
    }
}

fn column_panel(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>, height: f32) {
    let width = col.spec.width.clamp(COL_WIDTH_MIN, COL_WIDTH_MAX);
    ui.allocate_ui_with_layout(
        vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            header(ui, col, ctx);
            match &mut col.view {
                ColumnView::Conversation { .. } => conversation_body(ui, col, ctx),
                ColumnView::Timeline => timeline_body(ui, col, ctx),
            }
        },
    );
    resize_handle(ui, col, ctx, height);
}

/// ヘッダー: 並べ替えドラッグ + タイトル + 設定/一時停止/削除(F-02-4)
fn header(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>) {
    let id = col.id;
    let frame = Frame::default()
        .fill(Color32::from_rgb(0x26, 0x28, 0x30))
        .inner_margin(egui::Margin::symmetric(8, 6));
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            // ドラッグ起点は ≡ ハンドルのみに限定する。ヘッダ全体を
            // drag_source にすると全領域の interact が子ボタンより
            // 後に登録され、hit test でクリックが奪われる(egui 0.32)
            ui.dnd_drag_source(egui::Id::new(("col_drag", id)), id, |ui| {
                // グリフ単体だと掴める範囲が文字幅分しかなく小さいので、
                // 一定サイズのヒット領域を持たせる
                ui.add_sized(
                    egui::vec2(18.0, 16.0),
                    egui::Label::new(RichText::new("≡").color(Color32::GRAY)),
                );
            });
            ui.label(RichText::new(col.title()).strong());
            if col.pending_count() > 0 {
                ui.label(
                    RichText::new(format!("+{}", col.pending_count()))
                        .size(10.0)
                        .color(Color32::from_rgb(0xf0, 0xc0, 0x60)),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("×").on_hover_text("削除").clicked() {
                    ctx.ops.push(UiOp::Remove(id));
                }
                let pause_label = if col.paused { "▶" } else { "⏸" };
                if ui
                    .button(pause_label)
                    .on_hover_text(if col.paused { "再開" } else { "一時停止" })
                    .clicked()
                {
                    ctx.ops.push(UiOp::SetPaused(id, !col.paused));
                }
                if ui.button("⚙").on_hover_text("設定").clicked() {
                    *ctx.settings_open = if *ctx.settings_open == Some(id) {
                        None
                    } else {
                        Some(id)
                    };
                }
            });
        });
        if *ctx.settings_open == Some(id) {
            ui.separator();
            settings_panel(ui, col, ctx);
        }
    });
    ui.separator();
}

/// 設定パネル: 幅、TL 種別、フィルタ、通知種別、チャンネル選択(F-02-1/F-03/F-04-1)
fn settings_panel(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>) {
    let id = col.id;
    // 順番移動(≡ ドラッグの代替経路。F-02-4)
    ui.horizontal(|ui| {
        ui.label("順番:");
        if ui.button("←").on_hover_text("左へ移動").clicked() {
            ctx.ops.push(UiOp::MoveDelta(id, -1));
        }
        if ui.button("→").on_hover_text("右へ移動").clicked() {
            ctx.ops.push(UiOp::MoveDelta(id, 1));
        }
    });
    ui.horizontal(|ui| {
        ui.label("幅:");
        let mut w = col.spec.width;
        if ui
            .add(egui::Slider::new(&mut w, COL_WIDTH_MIN..=COL_WIDTH_MAX).suffix("px"))
            .changed()
        {
            ctx.ops.push(UiOp::SetWidth(id, w));
        }
    });
    match col.spec.kind {
        crate::config::ColumnKind::Timeline => {
            ui.horizontal(|ui| {
                ui.label("タイムライン:");
                let mut k = col
                    .spec
                    .timeline
                    .unwrap_or(crate::config::TimelineKind::Home);
                egui::ComboBox::from_id_salt(("tl_kind", id))
                    .selected_text(timeline_label(k))
                    .show_ui(ui, |ui| {
                        for cand in [
                            crate::config::TimelineKind::Home,
                            crate::config::TimelineKind::Local,
                            crate::config::TimelineKind::Social,
                            crate::config::TimelineKind::Global,
                        ] {
                            ui.selectable_value(&mut k, cand, timeline_label(cand));
                        }
                    });
                if Some(k) != col.spec.timeline {
                    ctx.ops.push(UiOp::SetTimelineKind(id, k));
                }
            });
            filter_checkboxes(ui, col, ctx);
        }
        crate::config::ColumnKind::Notifications => {
            ui.label("種別フィルタ:");
            ui.horizontal_wrapped(|ui| {
                for kind in NotificationFilter::KNOWN_KINDS {
                    let mut on = col.ntf_filter.allows(kind);
                    if ui.checkbox(&mut on, kind).changed() {
                        let mut f = col.ntf_filter.clone();
                        if on {
                            f.excluded.remove(kind);
                        } else {
                            f.excluded.insert(kind.to_owned());
                        }
                        ctx.ops.push(UiOp::SetNtfFilter(id, f));
                    }
                }
            });
        }
        crate::config::ColumnKind::Channel => {
            let current = col.spec.channel_id.as_deref().unwrap_or("未選択");
            ui.horizontal(|ui| {
                ui.label("対象:");
                ui.add(
                    egui::Label::new(RichText::new(current).size(11.0).color(Color32::LIGHT_GRAY))
                        .wrap(),
                );
                if ui.button("選択…").clicked() {
                    *ctx.picker_open = if *ctx.picker_open == Some(id) {
                        None
                    } else {
                        Some(id)
                    };
                    ctx.ops.push(UiOp::ChannelPickerOpen(id));
                }
            });
        }
        crate::config::ColumnKind::Mentions => {}
        crate::config::ColumnKind::Main => {}
    }
}

/// F-03-4 のカラムフィルタ(返信/リノート/ファイル限定)
fn filter_checkboxes(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>) {
    let id = col.id;
    let mut f = col.spec.filters.clone();
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= ui.checkbox(&mut f.include_replies, "返信").changed();
        changed |= ui.checkbox(&mut f.include_renotes, "リノート").changed();
        changed |= ui.checkbox(&mut f.files_only, "ファイル付きのみ").changed();
    });
    if changed {
        ctx.ops.push(UiOp::SetFilters(id, f));
    }
}

/// タイムライン本体(スクロール + 項目列 + ページング)
fn timeline_body(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>) {
    if let Some(err) = &col.error {
        ui.label(
            RichText::new(format!("⚠ {err}"))
                .color(Color32::LIGHT_RED)
                .size(11.0),
        );
        if ui.button("再読み込み").clicked() {
            ctx.ops.push(UiOp::FetchNextPage(col.id));
        }
    }
    if matches!(col.spec.kind, crate::config::ColumnKind::Main) {
        main_body(ui, ctx);
        return;
    }
    if matches!(col.spec.kind, crate::config::ColumnKind::Channel) && col.spec.channel_id.is_none()
    {
        ui.label(RichText::new("チャンネルが未選択です。⚙ から選択してください").size(11.0));
        if *ctx.picker_open == Some(col.id) {
            channel_picker(ui, col, ctx);
        }
        return;
    }
    let id = col.id;
    let scroll = egui::ScrollArea::vertical()
        .id_salt(("col_scroll", id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(4.0);
            match &col.items {
                ColumnItems::Notes(notes) => {
                    for note in notes.iter() {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                card::note_card(ui, note, ctx, id);
                            });
                        });
                    }
                }
                ColumnItems::Notifications(notifs) => {
                    for n in notifs.iter() {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                card::notif_card(ui, n, ctx, id);
                            });
                        });
                    }
                }
            }
            if col.items.is_empty() && !col.fetching {
                ui.label(
                    RichText::new(if col.oldest_id.is_none() {
                        "読み込み中…"
                    } else {
                        "項目がありません"
                    })
                    .size(11.0)
                    .color(Color32::DARK_GRAY),
                );
            }
            if col.fetching {
                ui.label(RichText::new("読み込み中…").size(11.0).color(Color32::GRAY));
            }
            ui.add_space(8.0);
            // ページング判定用に内容末尾の y を返す
            ui.min_rect().bottom()
        });
    // スクロール下端に達していたら追加読み込みを要求する(F-03-3)
    let view = scroll.inner_rect;
    let content_bottom = scroll.inner;
    // フィルタで初回ページが全件落ちても items は空のままなので、
    // 発行条件は items ではなく過去カーソル(oldest_id)の有無で見る
    if content_bottom <= view.bottom() + 40.0
        && !col.exhausted
        && !col.fetching
        && col.oldest_id.is_some()
        && matches!(
            col.spec.kind,
            crate::config::ColumnKind::Channel
                | crate::config::ColumnKind::Timeline
                | crate::config::ColumnKind::Mentions
                | crate::config::ColumnKind::Notifications
        )
    {
        ctx.ops.push(UiOp::FetchNextPage(id));
    }
    if *ctx.picker_open == Some(col.id)
        && matches!(col.spec.kind, crate::config::ColumnKind::Channel)
    {
        channel_picker(ui, col, ctx);
    }
}

/// 会話ビュー(F-05-5)。選択ノートを中心に notes/conversation を表示する
fn conversation_body(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>) {
    let id = col.id;
    if ui.button("← 戻る").clicked() {
        ctx.ops.push(UiOp::CloseConversation(id));
        return;
    }
    let ColumnView::Conversation {
        root_id,
        notes,
        loading,
        error,
    } = &mut col.view
    else {
        return;
    };
    if *loading {
        ui.label(RichText::new("読み込み中…").size(11.0).color(Color32::GRAY));
    }
    if let Some(e) = error {
        ui.label(
            RichText::new(format!("⚠ {e}"))
                .size(11.0)
                .color(Color32::LIGHT_RED),
        );
    }
    let rid = root_id.clone();
    let snapshot: Vec<crate::model::Note> = notes.clone();
    egui::ScrollArea::vertical()
        .id_salt(("conv_scroll", id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for note in &snapshot {
                // 選択ノートをハイライト
                let is_root = note.id == rid;
                let bg = if is_root {
                    Color32::from_rgb(0x2a, 0x30, 0x40)
                } else {
                    Color32::from_rgb(0x22, 0x24, 0x2a)
                };
                Frame::default().fill(bg).corner_radius(4.0).show(ui, |ui| {
                    card::note_card(ui, note, ctx, id);
                });
            }
        });
}

/// メインカラム(F-06 の投稿欄は Phase 7)。現段階はユーザー情報と状態表示
fn main_body(ui: &mut Ui, ctx: &mut UiCtx<'_>) {
    if let Some(me) = ctx.me {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if let Some(url) = &me.avatar_url {
                ui.add(
                    egui::Image::new(url)
                        .fit_to_exact_size(vec2(40.0, 40.0))
                        .corner_radius(6.0),
                );
            }
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(me.name.clone().unwrap_or_else(|| me.username.clone())).strong(),
                );
                ui.label(
                    RichText::new(format!("@{}", me.username))
                        .size(11.0)
                        .color(Color32::GRAY),
                );
            });
        });
        ui.separator();
        ui.label(
            RichText::new("投稿欄は Phase 7 で実装予定です")
                .size(11.0)
                .color(Color32::DARK_GRAY),
        );
    }
}

/// チャンネル選択(F-03-7)。フォロー中一覧 + 検索
fn channel_picker(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>) {
    let id = col.id;
    let picker = col.channel_picker.get_or_insert_with(Default::default);
    ui.separator();
    ui.label(RichText::new("チャンネル選択").strong().size(11.0));
    let mut q = picker.query.clone();
    if ui
        .add(
            egui::TextEdit::singleline(&mut q)
                .hint_text("検索…")
                .desired_width(f32::INFINITY),
        )
        .changed()
    {
        ctx.ops.push(UiOp::ChannelPickerQuery(id, q.clone()));
        picker.query = q;
    }
    if !picker.followed.is_empty() {
        ui.label(RichText::new("フォロー中").size(10.0).color(Color32::GRAY));
        let followed = picker.followed.clone();
        for ch in &followed {
            // SelectableLabel は折り返さないので、長いチャンネル名が
            // カラム確保幅を膨張させないよう Button::wrap() を使う
            if ui
                .add(
                    egui::Button::new(format!("📺 {}", ch.name))
                        .wrap()
                        .frame(false),
                )
                .clicked()
            {
                ctx.ops.push(UiOp::SetChannel(id, Some(ch.id.clone())));
                *ctx.picker_open = None;
            }
        }
    }
    if !picker.results.is_empty() {
        ui.label(RichText::new("検索結果").size(10.0).color(Color32::GRAY));
        let results = picker.results.clone();
        for ch in &results {
            if ui
                .add(
                    egui::Button::new(format!("📺 {}", ch.name))
                        .wrap()
                        .frame(false),
                )
                .clicked()
            {
                ctx.ops.push(UiOp::SetChannel(id, Some(ch.id.clone())));
                *ctx.picker_open = None;
            }
        }
    }
    if picker.loading {
        ui.label(RichText::new("読み込み中…").size(10.0).color(Color32::GRAY));
    }
}

/// 右端の細いハンドルで幅をドラッグ変更(F-02-1)
fn resize_handle(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>, _height: f32) {
    let id = col.id;
    let width = col.spec.width;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(6.0, ui.available_height().max(40.0)), Sense::drag());
    ui.painter().rect_filled(
        rect.shrink2(vec2(1.0, 0.0)),
        1.0,
        if resp.hovered() || resp.dragged() {
            Color32::WHITE
        } else {
            Color32::DARK_GRAY
        },
    );
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if resp.drag_started() {
        *ctx.resize_base = Some((id, width));
    }
    if resp.dragged() {
        // drag_delta はドラッグ開始からの累積変位なので開始時の幅を基準にする
        let base = ctx
            .resize_base
            .filter(|(base_id, _)| *base_id == id)
            .map(|(_, w)| w)
            .unwrap_or(width);
        ctx.ops.push(UiOp::SetWidth(
            id,
            (base + resp.drag_delta().x).clamp(COL_WIDTH_MIN, COL_WIDTH_MAX),
        ));
    }
    if resp.drag_stopped() {
        *ctx.resize_base = None;
    }
}
