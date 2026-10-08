use super::composer::composer_panel;
use super::{UiCtx, UiOp, card};
use crate::deck::{
    COL_WIDTH_MAX, COL_WIDTH_MIN, Column, ColumnItems, ColumnView, NotificationFilter,
    timeline_label,
};
use eframe::egui::{self, Color32, Frame, RichText, Sense, Ui, vec2};

/// 並べ替えドロップゾーン(F-02-2)。細い帯で、ドラッグ中にハイライトされる
pub(super) fn drop_zone(ui: &mut Ui, index: usize, ctx: &mut UiCtx<'_>, height: f32) {
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

pub(super) fn column_panel(ui: &mut Ui, col: &mut Column, ctx: &mut UiCtx<'_>, height: f32) {
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
    // F-06-4: channel カラムからの投稿導線(仕様決定 V)
    if matches!(col.spec.kind, crate::config::ColumnKind::Channel)
        && let Some(chid) = col.spec.channel_id.clone()
    {
        // 表示名はカラム内ノートの埋め込み channel から拾う。未取得のときは
        // ID をそのまま渡し、フォーム側は ID 表示にフォールバックする
        let name = match &col.items {
            ColumnItems::Notes(notes) => notes.iter().find_map(|n| {
                n.channel
                    .as_ref()
                    .filter(|c| c.id == chid)
                    .and_then(|c| c.name.clone())
            }),
            _ => None,
        };
        if ui
            .button(RichText::new("📤 このチャンネルに投稿").size(11.0))
            .clicked()
        {
            ctx.ops.push(UiOp::PostToChannel {
                channel_id: chid,
                name,
            });
        }
        ui.separator();
    }
    let id = col.id;
    let scroll = egui::ScrollArea::vertical()
        .id_salt(("col_scroll", id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(4.0);
            // 仕様決定 R: 当該チャンネルの channel カラム内では
            // カードのチャンネル名行を省略する
            let hide_channel = if matches!(col.spec.kind, crate::config::ColumnKind::Channel) {
                col.spec.channel_id.as_deref()
            } else {
                None
            };
            match &col.items {
                ColumnItems::Notes(notes) => {
                    for note in notes.iter() {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                card::note_card(ui, note, ctx, id, hide_channel);
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
    // 仕様決定 R: 当該チャンネルの channel カラム内ではチャンネル名行を省略
    let hide_channel = if matches!(col.spec.kind, crate::config::ColumnKind::Channel) {
        col.spec.channel_id.clone()
    } else {
        None
    };
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
                    card::note_card(ui, note, ctx, id, hide_channel.as_deref());
                });
            }
        });
}

/// メインカラム。ユーザー情報と投稿フォーム(F-06-5)を表示する
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
    }
    // F-06-5: 投稿フォームはメインカラムに配置する(仕様決定 X)。
    // スクロール可能にしておき、チャンネル選択一覧が長いときも
    // ユーザー情報行が押し出されないようにする
    egui::ScrollArea::vertical()
        .id_salt("main_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            composer_panel(ui, ctx);
        });
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
    if let Some(err) = &picker.error {
        let err = err.clone();
        ui.label(
            RichText::new(format!("⚠ {err}"))
                .color(Color32::LIGHT_RED)
                .size(11.0),
        );
    }
    if picker.loading {
        ui.label(RichText::new("読み込み中…").size(10.0).color(Color32::GRAY));
    }
    // 一覧がどちらも空のときは案内を出す(選び方が分からない状態を防ぐ)
    if !picker.loading
        && picker.error.is_none()
        && picker.followed.is_empty()
        && picker.results.is_empty()
    {
        ui.label(
            RichText::new("チャンネルが見つかりませんでした。名前で検索できます")
                .size(10.0)
                .color(Color32::GRAY),
        );
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
