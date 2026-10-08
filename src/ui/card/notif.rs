use super::reactions::reaction_emoji_url;
use super::{UiCtx, display_name, fmt_time, nested_ref};
use crate::model::Notification;
use eframe::egui::{self, Color32, Frame, RichText, Stroke, Ui, vec2};

/// 通知カード(F-04)。種別アイコン + ユーザー + 関連ノート
pub fn notif_card(ui: &mut Ui, n: &Notification, ctx: &mut UiCtx<'_>, _col_id: u64) {
    let frame = Frame::group(ui.style())
        .fill(Color32::from_rgb(0x22, 0x24, 0x2a))
        .stroke(Stroke::new(1.0f32, Color32::from_rgb(0x32, 0x34, 0x3c)))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 6));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let (icon, label) = notif_label(&n.kind);
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon).size(13.0));
            let who = n
                .user
                .as_ref()
                .map(display_name)
                .unwrap_or_else(|| "システム".to_owned());
            // 非 wrap の horizontal 内では明示的に .wrap() が要る
            ui.add(egui::Label::new(RichText::new(format!("{label}: {who}")).size(11.0)).wrap());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.label(
                    RichText::new(fmt_time(&n.created_at))
                        .size(11.0)
                        .color(Color32::GRAY),
                );
            });
        });
        if let Some(reaction) = &n.reaction {
            // F-07-4: 通知のリアクション欄も同じ解決で絵文字画像を出す。
            // リモート絵文字は対象ノートの reactionEmojis がソース
            let url = n
                .note
                .as_ref()
                .and_then(|note| reaction_emoji_url(note, reaction, ctx));
            match url {
                Some(u) => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("リアクション:")
                                .size(11.0)
                                .color(Color32::GRAY),
                        );
                        ui.add(egui::Image::new(u).fit_to_exact_size(vec2(14.0, 14.0)));
                    });
                }
                None => {
                    ui.label(
                        RichText::new(format!("リアクション: {reaction}"))
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                }
            }
        }
        if let Some(note) = &n.note {
            nested_ref(ui, note, "💬", ctx);
        }
    });
}

fn notif_label(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "follow" => ("➕", "フォローされました"),
        "mention" => ("💬", "メンション"),
        "reply" => ("↩", "返信"),
        "quote" => ("❝", "引用されました"),
        "renote" => ("🔁", "リノートされました"),
        "reaction" => ("⭐", "リアクション"),
        "pollEnded" => ("📊", "アンケート終了"),
        "receiveAchievement" => ("🏅", "実績を解除"),
        _ => ("🔔", kind_icon_fallback(kind)),
    }
}

/// 未知種別は kind 名をそのまま出す(io 独自種別があり得るため)
fn kind_icon_fallback(_kind: &str) -> &'static str {
    "通知"
}
