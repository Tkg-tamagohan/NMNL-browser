//! ノートカードの見た目(F-05)。モックでは実画像を使わず、
//! アバターは頭文字の色ブロック、絵文字・メディアはプレースホルダで表現する。

use eframe::egui::{self, Color32, Frame, RichText, Stroke, Ui};

use crate::deck::{DummyNote, MediaKind, RefKind};
use crate::mfm;

/// センシティブ画像の開示状態はノート ID ごとに持つ
#[derive(Default)]
pub struct CardState {
    pub cw_open: std::collections::HashSet<u64>,
    pub media_open: std::collections::HashSet<u64>,
}

/// 1 件分のノートカードを描く
pub fn note_card(ui: &mut Ui, note: &DummyNote, state: &mut CardState) {
    let frame = Frame::group(ui.style())
        .fill(Color32::from_rgb(0x22, 0x24, 0x2a))
        .stroke(Stroke::new(1.0f32, Color32::from_rgb(0x33, 0x36, 0x40)))
        .inner_margin(egui::Margin::same(8))
        .corner_radius(egui::CornerRadius::same(6));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        header(ui, note);
        ui.add_space(4.0);

        // CW: 折りたたみ(F-05-3)
        let body_visible = match &note.cw {
            Some(cw) => {
                let open = state.cw_open.contains(&note.id);
                let label = if open {
                    format!("⚠ CW: {cw} ▼")
                } else {
                    format!("⚠ CW: {cw} ▶(クリックで展開)")
                };
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).color(Color32::YELLOW)).frame(false),
                    )
                    .clicked()
                {
                    if open {
                        state.cw_open.remove(&note.id);
                    } else {
                        state.cw_open.insert(note.id);
                    }
                }
                open
            }
            None => true,
        };

        // CW 折りたたみ時は本文だけでなくメディアと入れ子参照も隠す
        // (CW はノート内容全体の閲覧注意。F-05-3)
        if body_visible {
            if !note.text.is_empty() {
                let job = mfm::layout(&note.text, ui);
                ui.add(egui::Label::new(job).wrap());
            }
            media_block(ui, note, state);
            nested_reference(ui, note);
        }
        reactions(ui, note);
        action_row(ui);
    });
}

fn header(ui: &mut Ui, note: &DummyNote) {
    ui.horizontal(|ui| {
        // アバター代替: 頭文字入りの色ブロック
        let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            avatar_color(&note.user_id),
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial(&note.user_name),
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&note.user_name).strong());
                let mut id = note.user_id.clone();
                if let Some(host) = &note.host {
                    id.push('@');
                    id.push_str(host);
                }
                ui.label(RichText::new(id).color(Color32::GRAY).small());
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.label(RichText::new(&note.time).color(Color32::GRAY).small());
        });
    });
}

fn media_block(ui: &mut Ui, note: &DummyNote, state: &mut CardState) {
    match note.media {
        MediaKind::None => {}
        MediaKind::Normal(n) => {
            ui.add_space(4.0);
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 64.0), egui::Sense::hover());
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(4),
                Color32::from_rgb(0x2c, 0x3a, 0x4d),
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("🖼 画像 ×{n}"),
                egui::FontId::proportional(13.0),
                Color32::LIGHT_GRAY,
            );
        }
        MediaKind::Sensitive(n) => {
            ui.add_space(4.0);
            let open = state.media_open.contains(&note.id);
            let label = if open {
                format!("🖼 画像 ×{n}(開示済み)")
            } else {
                "🖼 センシティブ画像(クリックで表示)".to_owned()
            };
            let (rect, resp) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::click());
            let fill = if open {
                Color32::from_rgb(0x2c, 0x3a, 0x4d)
            } else {
                Color32::from_rgb(0x39, 0x2d, 0x41)
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(4), fill);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(13.0),
                Color32::LIGHT_GRAY,
            );
            if resp.clicked() {
                state.media_open.insert(note.id);
            }
        }
    }
}

/// リノート/引用/返信の 1 段入れ子(F-05-4)
fn nested_reference(ui: &mut Ui, note: &DummyNote) {
    let Some((kind, target)) = &note.reference else {
        return;
    };
    ui.add_space(4.0);
    let icon = match kind {
        RefKind::Renote => "🔁",
        RefKind::Quote => "❝",
        RefKind::Reply => "↩",
    };
    let inner = Frame::group(ui.style())
        .fill(Color32::from_rgb(0x1c, 0x1e, 0x24))
        .stroke(Stroke::new(1.0f32, Color32::from_rgb(0x2c, 0x2f, 0x38)))
        .inner_margin(egui::Margin::same(6))
        .corner_radius(egui::CornerRadius::same(4));
    inner.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{icon} {} {}", target.user_name, target.user_id))
                    .small()
                    .color(Color32::LIGHT_GREEN),
            );
        });
        let job = mfm::layout(&target.text, ui);
        ui.add(egui::Label::new(job).wrap());
    });
}

fn reactions(ui: &mut Ui, note: &DummyNote) {
    if note.reactions.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (name, count) in &note.reactions {
            let label = format!("{name} {count}");
            let resp = ui.add(
                egui::Button::new(RichText::new(label).small())
                    .fill(Color32::from_rgb(0x2a, 0x2d, 0x36))
                    .stroke(Stroke::new(1.0f32, Color32::from_rgb(0x3a, 0x3e, 0x4a))),
            );
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    });
}

/// ノート操作(仕様決定 E)。モックでは配置と見た目だけ
fn action_row(ui: &mut Ui) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        for icon in ["↩", "🔁", "❝", "⭐", "⋯"] {
            ui.add(egui::Button::new(RichText::new(icon)).frame(false));
        }
    });
}

fn initial(name: &str) -> String {
    name.chars().next().unwrap_or('?').to_uppercase().collect()
}

fn avatar_color(user_id: &str) -> Color32 {
    // ID から決定的な色を作る(モック用の見分け)
    let hash: u32 = user_id.bytes().map(|b| b as u32).sum();
    let hue = (hash % 360) as f32 / 360.0;
    egui::epaint::Hsva::new(hue, 0.45, 0.55, 1.0).into()
}
