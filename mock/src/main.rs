//! NMNL-browser の UI モック(Phase 5)。
//! デッキ骨格・カラム操作(追加/削除/ドラッグ並べ替え/幅変更/一時停止)と
//! ノートカードの見た目をダミーデータで確認するための WASM/ネイティブ両対応 UI。
//! 通信・永続化は持たない(本実装は Phase 6 以降)。

mod card;
mod deck;
mod mfm;

use eframe::egui::{self, Color32, Frame, RichText, Stroke, Ui};
use std::sync::Arc;

use card::CardState;
use deck::{COL_WIDTH_MAX, COL_WIDTH_MIN, ColKind, ColumnDeck};

const DRAG_ZONE_W: f32 = 16.0;
const RESIZE_HANDLE_W: f32 = 6.0;

struct MockApp {
    deck: ColumnDeck,
    card_state: CardState,
    /// 追加メニューで選択中のカラム種別
    add_kind: ColKind,
    /// 設定ポップアウトを開いているカラム
    settings_open: Option<u64>,
    /// 幅変更ドラッグ開始時の (カラム ID, 幅)。drag_delta は累積値のため
    /// 毎フレーム現在値へ足すと二重加算になる
    resize_base: Option<(u64, f32)>,
}

impl MockApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&cc.egui_ctx);
        // eframe はシステムテーマを毎フレーム反映して set_visuals を上書きする。
        // モックはカード色を暗色固定で設計しているため、常時ダークに固定する
        cc.egui_ctx.options_mut(|o| {
            o.theme_preference = egui::ThemePreference::Dark;
        });
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        MockApp {
            deck: ColumnDeck::new(),
            card_state: CardState::default(),
            add_kind: ColKind::LocalTimeline,
            settings_open: None,
            resize_base: None,
        }
    }
}

/// 本体と同じく Noto Sans JP を Proportional/Monospace の先頭に登録する
/// (notofonts/noto-cjk の SubsetOTF/JP、SIL OFL 1.1)
fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "NotoSansJP".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSansJP-Regular.otf"
        ))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "NotoSansJP".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// ループ内の変更要求をためてあとから適用する(列走査中の借用衝突回避)
enum Op {
    Remove(u64),
    MoveTo(u64, usize),
    SetPaused(u64, bool),
    SetWidth(u64, f32),
}

impl eframe::App for MockApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        top_bar(ctx, self);
        egui::CentralPanel::default().show(ctx, |ui| {
            // 横スクロール領域の高さはここで確定して各カラムへ渡す
            // (horizontal レイアウト内の available_height は子の確定値にならないため)
            let deck_h = ui.available_height();
            egui::ScrollArea::horizontal()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut ops: Vec<Op> = Vec::new();
                        for idx in 0..self.deck.columns.len() {
                            drop_zone(ui, deck_h, idx, &mut ops);
                            column_panel(ui, self, idx, deck_h, &mut ops);
                        }
                        drop_zone(ui, deck_h, self.deck.columns.len(), &mut ops);
                        for op in ops {
                            apply(&mut self.deck, op);
                        }
                    });
                });
        });
    }
}

fn apply(deck: &mut ColumnDeck, op: Op) {
    match op {
        Op::Remove(id) => deck.remove(id),
        Op::MoveTo(id, to) => deck.move_to(id, to),
        Op::SetPaused(id, p) => deck.set_paused(id, p),
        Op::SetWidth(id, w) => deck.set_width(id, w),
    }
}

/// 追加メニューつきのトップバー(F-02-1/F-02-5)
fn top_bar(ctx: &egui::Context, app: &mut MockApp) {
    egui::TopBottomPanel::top("top").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("NMNL-browser").strong());
            ui.label(RichText::new("UI モック").color(Color32::GRAY).small());
            ui.separator();
            egui::ComboBox::from_id_salt("add_kind")
                .selected_text(app.add_kind.label())
                .show_ui(ui, |ui| {
                    for kind in ColKind::ALL {
                        ui.selectable_value(&mut app.add_kind, kind, kind.label());
                    }
                });
            if ui.button("＋ カラム追加").clicked() {
                app.deck.add(app.add_kind);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} 列", app.deck.columns.len()))
                        .color(Color32::GRAY)
                        .small(),
                );
            });
        });
    });
}

/// カラム間のドロップゾーン(F-02-1 のドラッグ並べ替えの受け側)
fn drop_zone(ui: &mut Ui, deck_h: f32, to: usize, ops: &mut Vec<Op>) {
    let h = deck_h;
    let (inner_resp, payload) = ui.dnd_drop_zone::<u64, _>(
        Frame::default().inner_margin(egui::Margin::symmetric(DRAG_ZONE_W as i8 / 2, 0)),
        |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(4.0, h), egui::Sense::hover());
            // ガイドラインとして常時うっすら表示(ドロップ位置の目印)
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(2),
                Color32::from_rgb(0x2a, 0x2d, 0x38),
            );
        },
    );
    // ドラッグ中のカラムがここで放たれたら並べ替え要求
    if let Some(dragged) = payload {
        ops.push(Op::MoveTo(*dragged, to));
    }
    // ドラッグ中にホバーしたドロップゾーンをハイライトする
    if inner_resp.response.dnd_hover_payload::<u64>().is_some() {
        let rect = inner_resp.response.rect;
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(2),
            Color32::from_rgb(0x4a, 0x55, 0x70),
        );
    }
}

/// 1 カラム分のパネル(ヘッダー + 独立スクロールのノート列 + 幅ハンドル)
fn column_panel(ui: &mut Ui, app: &mut MockApp, idx: usize, deck_h: f32, ops: &mut Vec<Op>) {
    let col = &app.deck.columns[idx];
    let id = col.id;
    let width = col.width;
    let kind = col.kind;
    let paused = col.paused;
    let h = deck_h;

    ui.allocate_ui_with_layout(
        egui::vec2(width, h),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            // 割当領域いっぱいに背景を塗る(Frame::group は中身分しか囲まないため)
            ui.painter().rect_filled(
                ui.max_rect(),
                egui::CornerRadius::same(6),
                Color32::from_rgb(0x1e, 0x20, 0x26),
            );
            ui.painter().rect_stroke(
                ui.max_rect(),
                egui::CornerRadius::same(6),
                Stroke::new(1.0f32, Color32::from_rgb(0x2e, 0x31, 0x3a)),
                egui::StrokeKind::Inside,
            );
            Frame::default()
                .inner_margin(egui::Margin::same(0))
                .show(ui, |ui| {
                    header(ui, id, kind, paused, app, ops);
                    if paused {
                        ui.label(
                            RichText::new("⏸ 一時停止中(新着は貯めて表示しません)")
                                .small()
                                .color(Color32::YELLOW),
                        );
                        ui.separator();
                    }
                    // F-02-2: カラムごとに独立したスクロール
                    egui::ScrollArea::vertical()
                        .id_salt(("col_scroll", id))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add_space(4.0);
                            let notes = &app.deck.columns[idx].notes;
                            for note in notes {
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    ui.vertical(|ui| {
                                        card::note_card(ui, note, &mut app.card_state);
                                    });
                                    ui.add_space(4.0);
                                });
                                ui.add_space(6.0);
                            }
                            ui.add_space(8.0);
                        });
                });
        },
    );

    // 右端の幅変更ハンドル(F-02-1)
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(RESIZE_HANDLE_W, h), egui::Sense::drag());
    let hover = resp.hovered() || resp.dragged();
    ui.painter().rect_filled(
        rect,
        0.0,
        if hover {
            Color32::from_rgb(0x4a, 0x55, 0x70)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "⋮",
        egui::FontId::proportional(10.0),
        if hover {
            Color32::WHITE
        } else {
            Color32::DARK_GRAY
        },
    );
    if hover {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if resp.drag_started() {
        app.resize_base = Some((id, width));
    }
    if resp.dragged() {
        // drag_delta はドラッグ開始からの累積変位なので開始時の幅を基準にする
        let base = app
            .resize_base
            .filter(|(base_id, _)| *base_id == id)
            .map(|(_, w)| w)
            .unwrap_or(width);
        ops.push(Op::SetWidth(
            id,
            (base + resp.drag_delta().x).clamp(COL_WIDTH_MIN, COL_WIDTH_MAX),
        ));
    }
    if resp.drag_stopped() {
        app.resize_base = None;
    }
}

/// カラムヘッダー: 種類名 + 設定/一時停止/削除(F-02-4)。
/// ヘッダー自体をドラッグして並べ替えできる(ペイロードはカラム ID)
fn header(ui: &mut Ui, id: u64, kind: ColKind, paused: bool, app: &mut MockApp, ops: &mut Vec<Op>) {
    let frame = Frame::default()
        .fill(Color32::from_rgb(0x26, 0x28, 0x30))
        .inner_margin(egui::Margin::symmetric(8, 6));
    ui.dnd_drag_source(egui::Id::new(("col_drag", id)), id, |ui| {
        frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("≡").color(Color32::GRAY));
                ui.label(RichText::new(kind.label()).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("×").on_hover_text("削除").clicked() {
                        ops.push(Op::Remove(id));
                    }
                    let pause_label = if paused { "▶" } else { "⏸" };
                    if ui
                        .button(pause_label)
                        .on_hover_text(if paused { "再開" } else { "一時停止" })
                        .clicked()
                    {
                        ops.push(Op::SetPaused(id, !paused));
                    }
                    if ui.button("⚙").on_hover_text("設定").clicked() {
                        app.settings_open = if app.settings_open == Some(id) {
                            None
                        } else {
                            Some(id)
                        };
                    }
                });
            });
            if app.settings_open == Some(id) {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("幅:");
                    let cur = app
                        .deck
                        .columns
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| c.width)
                        .unwrap_or(300.0);
                    let mut w = cur;
                    if ui
                        .add(egui::Slider::new(&mut w, COL_WIDTH_MIN..=COL_WIDTH_MAX).suffix("px"))
                        .changed()
                    {
                        ops.push(Op::SetWidth(id, w));
                    }
                });
            }
        });
    });
    ui.separator();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    eframe::run_native(
        "NMNL-browser UI モック",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 700.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(MockApp::new(cc)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast;
    console_error_panic_hook::set_once();
    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("the_canvas_id"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("canvas が見つかりません");
        eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(MockApp::new(cc)))),
            )
            .await
            .expect("eframe の起動に失敗しました");
    });
}
