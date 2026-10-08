use super::{UiCtx, UiOp, display_name, fmt_time};
use crate::model::User;
use eframe::egui::{self, Color32, RichText, vec2};

/// 画像ビューアの状態(F-08-1)
#[derive(Debug, Default)]
pub struct ViewerState {
    pub files: Vec<crate::model::DriveFile>,
    pub index: usize,
    /// カード側で開封済みのセンシティブ画像のファイル ID(VWR-02)
    pub revealed: std::collections::HashSet<String>,
}

impl ViewerState {
    /// 表示してよい画像か(センシティブは開封済みのみ、VWR-02)
    pub fn is_visible(&self, file: &crate::model::DriveFile) -> bool {
        !file.is_sensitive || self.revealed.contains(&file.id)
    }

    /// インデックスを ±1 動かす(範囲にクランプ、VWR-01)
    pub fn step(&mut self, delta: i32) {
        if self.files.is_empty() {
            self.index = 0;
            return;
        }
        let next = self.index as i64 + i64::from(delta);
        self.index = next.clamp(0, self.files.len() as i64 - 1) as usize;
    }
}

/// リアクションピッカー(F-07-2)。検索欄+絵文字グリッドの浮遊ウィンドウ
pub(super) fn reaction_picker_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
    let Some(state) = ctx.reaction_picker.as_mut() else {
        return;
    };
    let note_id = state.note_id.clone();
    let mut open = true;
    let mut pick = None;
    egui::Window::new("リアクションを選択")
        .collapsible(false)
        .resizable(true)
        .default_size([280.0, 320.0])
        .open(&mut open)
        .show(egui_ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut state.query).hint_text("検索(name/aliases)"));
            ui.separator();
            // クエリは TextEdit が state.query を更新した後に読む
            // (先にコピーすると絞り込みが 1 フレーム遅れる)
            let q = state.query.trim().to_lowercase();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for e in ctx.emoji_list.iter().filter(|e| {
                            q.is_empty()
                                || e.name.to_lowercase().contains(&q)
                                || e.aliases.iter().any(|a| a.to_lowercase().contains(&q))
                        }) {
                            let btn = egui::Button::new(
                                egui::RichText::new(format!(":{}:", e.name)).size(12.0),
                            )
                            .min_size(egui::vec2(0.0, 22.0));
                            if ui.add(btn).on_hover_text(&e.name).clicked() {
                                pick = Some(format!(":{}:", e.name));
                            }
                        }
                    });
                });
        });
    if !open {
        ctx.ops.push(UiOp::CloseReactionPicker);
    }
    if let Some(reaction) = pick {
        ctx.ops.push(UiOp::PickReaction { note_id, reaction });
    }
}

/// マウスの戻るボタン(ブラウザの「戻る」相当)がこのフレームで
/// 押されたか。egui-winit 0.32 は winit の Back→Extra1、
/// Forward→Extra2 を割り当てる(F-08-6)
pub(super) fn back_button_pressed(i: &egui::InputState) -> bool {
    i.events.iter().any(|e| {
        matches!(
            e,
            egui::Event::PointerButton {
                button: egui::PointerButton::Extra1,
                pressed: true,
                ..
            }
        )
    })
}

/// 画像ビューア(F-08-1)。サムネイルクリックで開く拡大表示。
/// ‹› ボタンと ←→ キーでめくり、×/Esc/戻るボタンで閉じる(F-08-6)
pub(super) fn viewer_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
    let Some(state) = ctx.viewer.as_mut() else {
        return;
    };
    if state.files.is_empty() {
        ctx.ops.push(UiOp::CloseViewer);
        return;
    }
    let mut open = true;
    let mut step = 0i32;
    // ←→ キーのめくりはウィンドウ表示中のみ
    if egui_ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
        step = -1;
    }
    if egui_ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
        step = 1;
    }
    // F-08-6: Esc キーとマウスの戻るボタンでも閉じる
    // (egui-winit は winit の Back を Extra1 に写す)
    if egui_ctx.input(|i| i.key_pressed(egui::Key::Escape) || back_button_pressed(i)) {
        open = false;
    }
    let total = state.files.len();
    let mut reveal: Option<String> = None;
    egui::Window::new(format!("画像 {} / {}", state.index + 1, total))
        .collapsible(false)
        .resizable(true)
        .default_size([640.0, 480.0])
        .open(&mut open)
        .show(egui_ctx, |ui| {
            let file = &state.files[state.index.min(total - 1)];
            ui.horizontal(|ui| {
                if ui.button("‹").clicked() {
                    step = -1;
                }
                if ui.button("›").clicked() {
                    step = 1;
                }
                ui.label(RichText::new(&file.name).size(12.0));
                // 動画・音声と同じく外部ブラウザへの出口は残す(F-08-3)
                if let Some(u) = &file.url
                    && ui.link("ブラウザで開く").clicked()
                {
                    ctx.ops.push(UiOp::OpenUrl(u.clone()));
                }
            });
            if !state.is_visible(file) {
                // カードと同じく未開封の閲覧注意は覆ったまま(VWR-02)
                if ui
                    .button(
                        RichText::new(format!("⚠ 閲覧注意: {}", file.name))
                            .color(Color32::from_rgb(0xf0, 0xa0, 0x80)),
                    )
                    .clicked()
                {
                    reveal = Some(file.id.clone());
                }
            } else if let Some(u) = &file.url {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(egui::Image::new(u).fit_to_fraction(vec2(1.0, 1.0)));
                    });
            } else {
                ui.label(RichText::new("(URL なし)").color(Color32::GRAY));
            }
        });
    if let Some(id) = reveal {
        ctx.ops.push(UiOp::ViewerReveal(id));
    }
    if !open {
        ctx.ops.push(UiOp::CloseViewer);
    }
    if step != 0 {
        ctx.ops.push(UiOp::ViewerStep(step));
    }
}

/// プロフィール(F-05-6)。アバター/名前クリックで開く。
/// 基本情報 + そのユーザーのノート一覧を表示する
pub(super) fn profile_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
    let Some(state) = ctx.profile.as_mut() else {
        return;
    };
    let mut open = true;
    egui::Window::new("プロフィール")
        .collapsible(false)
        .resizable(true)
        .default_size([360.0, 420.0])
        .open(&mut open)
        .show(egui_ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(err) = &state.error {
                        ui.label(
                            RichText::new(err)
                                .size(11.0)
                                .color(Color32::from_rgb(0xe0, 0x80, 0x80)),
                        );
                    }
                    if state.user.is_none() && state.error.is_none() {
                        ui.label(RichText::new("読み込み中…").color(Color32::GRAY));
                    }
                    if let Some(u) = &state.user {
                        ui.horizontal(|ui| {
                            if let Some(av) = &u.avatar_url {
                                ui.add(
                                    egui::Image::new(av)
                                        .fit_to_exact_size(vec2(48.0, 48.0))
                                        .corner_radius(4.0),
                                );
                            }
                            ui.vertical(|ui| {
                                ui.label(RichText::new(display_name(u)).strong());
                                ui.label(
                                    RichText::new(format!("@{}{}", u.username, host_suffix(u)))
                                        .size(11.0)
                                        .color(Color32::GRAY),
                                );
                                if let Some(c) = &u.created_at {
                                    ui.label(
                                        RichText::new(format!(
                                            "登録: {}",
                                            c.split('T').next().unwrap_or(c)
                                        ))
                                        .size(10.0)
                                        .color(Color32::GRAY),
                                    );
                                }
                                if let (Some(n), Some(fi), Some(fo)) =
                                    (u.notes_count, u.following_count, u.followers_count)
                                {
                                    ui.label(
                                        RichText::new(format!(
                                            "ノート {n} / フォロー {fi} / フォロワー {fo}"
                                        ))
                                        .size(10.0),
                                    );
                                }
                            });
                        });
                        if let Some(d) = &u.description {
                            ui.label(RichText::new(d).size(11.0));
                        }
                        ui.separator();
                    }
                    // ノート一覧(簡易カード。時刻+本文+添付数)
                    for n in &state.notes {
                        ui.label(
                            RichText::new(fmt_time(&n.created_at))
                                .size(10.0)
                                .color(Color32::GRAY),
                        );
                        if let Some(t) = &n.text {
                            ui.label(RichText::new(t).size(12.0));
                        }
                        if !n.files.is_empty() {
                            ui.label(
                                RichText::new(format!("📎 {} ファイル", n.files.len()))
                                    .size(10.0)
                                    .color(Color32::GRAY),
                            );
                        }
                        ui.separator();
                    }
                    if state.loading {
                        ui.label(RichText::new("…").color(Color32::GRAY));
                    }
                });
        });
    if !open {
        ctx.ops.push(UiOp::CloseProfile);
    }
}

/// @username@host 表記のホスト部分
fn host_suffix(u: &User) -> String {
    u.host.as_ref().map(|h| format!("@{h}")).unwrap_or_default()
}

/// 設定画面(F-09-3)。キャッシュ容量の表示と消去
pub(super) fn settings_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
    if !*ctx.settings_win {
        return;
    }
    let mut open = true;
    egui::Window::new("設定")
        .collapsible(false)
        .default_size([320.0, 160.0])
        .open(&mut open)
        .show(egui_ctx, |ui| {
            ui.label(RichText::new("キャッシュ").strong());
            ui.horizontal(|ui| {
                ui.label(format!("画像キャッシュ: {}", fmt_bytes(ctx.cache_sizes.0)));
                if ui.button("消去").clicked() {
                    ctx.ops.push(UiOp::ClearCache(true));
                }
            });
            ui.horizontal(|ui| {
                ui.label(format!(
                    "絵文字一覧キャッシュ: {}",
                    fmt_bytes(ctx.cache_sizes.1)
                ));
                if ui.button("消去").clicked() {
                    ctx.ops.push(UiOp::ClearCache(false));
                }
            });
            ui.separator();
            // 文字サイズ = UI 全体の拡縮(F-09-5・仕様決定 U/T)
            ui.label(RichText::new("文字サイズ").strong());
            ui.horizontal(|ui| {
                let mut scale = ctx.ui_scale;
                if ui
                    .add(
                        egui::Slider::new(
                            &mut scale,
                            crate::config::UI_SCALE_MIN..=crate::config::UI_SCALE_MAX,
                        )
                        .suffix(" 倍")
                        .fixed_decimals(2),
                    )
                    .changed()
                {
                    ctx.ops.push(UiOp::SetUiScale(scale));
                }
            });
            ui.label(
                RichText::new("UI 全体の拡縮です。再起動後も保持されます")
                    .size(10.0)
                    .color(Color32::GRAY),
            );
        });
    if !open {
        ctx.ops.push(UiOp::CloseSettings);
    }
}

/// バイト数の人間向け表記
fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KB", b as f64 / (1u64 << 10) as f64)
    } else {
        format!("{b} B")
    }
}
