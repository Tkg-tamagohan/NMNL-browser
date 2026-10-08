use super::{UiCtx, UiOp};
use eframe::egui::{self, Color32, RichText, Ui};

/// 投稿フォーム(F-06)。メインカラム内に表示し、メインカラムが
/// 無いときだけボトムパネルに出す(F-06-5・仕様決定 X)。
/// 返信/引用は対象をセットして同じフォームから投稿する
pub(super) fn composer_panel(ui: &mut Ui, ctx: &mut UiCtx<'_>) {
    use crate::composer::{MAX_ATTACHMENTS, visibility_label};
    // 投稿中は編集を不可にする。成功時にフォーム全体をリセットするため、
    // 送信中の追記がリクエストに含まれないまま消えるのを防ぐ
    ui.add_enabled_ui(!ctx.composer.posting, |ui| {
        // カラム幅のメインカラムにも収まるよう折り返しを許可する
        ui.horizontal_wrapped(|ui| {
            // 返信/引用の対象表示と解除(F-06-3)
            let mut clear_reply = false;
            let mut clear_quote = false;
            if let Some(t) = &ctx.composer.reply_to {
                ui.label(RichText::new(format!("↩ {} への返信", t.label)).size(11.0));
                if ui.small_button("×").clicked() {
                    clear_reply = true;
                }
            }
            if let Some(t) = &ctx.composer.quote_of {
                ui.label(RichText::new(format!("❝ {} の引用", t.label)).size(11.0));
                if ui.small_button("×").clicked() {
                    clear_quote = true;
                }
            }
            if clear_reply {
                ctx.ops.push(UiOp::ClearTarget(true));
            }
            if clear_quote {
                ctx.ops.push(UiOp::ClearTarget(false));
            }
            // CW 切り替え
            let mut cw_on = ctx.composer.cw_enabled;
            if ui.checkbox(&mut cw_on, "CW").changed() {
                ctx.composer.cw_enabled = cw_on;
            }
            // 公開範囲(F-06-1)。チャンネル投稿のときはパブリック固定で
            // 選択不可にする(仕様決定 W・F-06-4)
            let channel_selected = ctx.composer.channel_id.is_some();
            ui.add_enabled_ui(!channel_selected, |ui| {
                egui::ComboBox::from_id_salt("visibility")
                    .selected_text(visibility_label(ctx.composer.visibility))
                    .show_ui(ui, |ui| {
                        for v in [
                            crate::model::Visibility::Public,
                            crate::model::Visibility::Home,
                            crate::model::Visibility::Followers,
                            crate::model::Visibility::Specified,
                        ] {
                            ui.selectable_value(
                                &mut ctx.composer.visibility,
                                v,
                                visibility_label(v),
                            );
                        }
                    });
            });
            // 投稿先チャンネル(F-06-4)。選択中は解除用の × を付ける
            {
                let ch_label = match (&ctx.composer.channel_id, &ctx.composer.channel_name) {
                    (Some(_), Some(name)) => format!("📺 {name}"),
                    (Some(id), None) => format!("📺 {id}"),
                    _ => "📺 通常投稿".to_owned(),
                };
                let resp = ui
                    .button(RichText::new(ch_label).size(11.0))
                    .on_hover_text("投稿先チャンネルを選択");
                if resp.clicked() {
                    *ctx.composer_channel_open = !*ctx.composer_channel_open;
                    if *ctx.composer_channel_open {
                        ctx.ops.push(UiOp::ComposerChannelPickerOpen);
                    }
                }
                if channel_selected && ui.small_button("×").clicked() {
                    ctx.ops.push(UiOp::ComposerSetChannel(None));
                }
            }
            // 添付(D&D、最大 MAX_ATTACHMENTS)
            ui.label(
                RichText::new("画像をドロップで添付")
                    .size(10.0)
                    .color(Color32::GRAY),
            );
            let mut remove_at = None;
            for (i, f) in ctx.composer.files.iter().enumerate() {
                ui.label(RichText::new(&f.name).size(10.0));
                if ui.small_button("×").clicked() {
                    remove_at = Some(i);
                }
            }
            if let Some(i) = remove_at {
                ctx.ops.push(UiOp::RemoveAttachment(i));
            }
            ui.label(
                RichText::new(format!("{}/{MAX_ATTACHMENTS}", ctx.composer.files.len()))
                    .size(10.0)
                    .color(Color32::GRAY),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_post = !ctx.composer.posting;
                if ui
                    .add_enabled(can_post, egui::Button::new("投稿"))
                    .clicked()
                {
                    ctx.ops.push(UiOp::PostNote);
                }
            });
        });
        if ctx.composer.cw_enabled {
            ui.add(
                egui::TextEdit::singleline(&mut ctx.composer.cw)
                    .hint_text("注釈(CW)")
                    .desired_width(f32::INFINITY),
            );
        }
        ui.add(
            egui::TextEdit::multiline(&mut ctx.composer.text)
                .hint_text("いまどうしてる?")
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        // チャンネル選択ポップアップ(F-06-4)
        if *ctx.composer_channel_open {
            composer_channel_picker(ui, ctx);
        }
        // エラーと通知
        if let Some(e) = &ctx.composer.error {
            ui.label(
                RichText::new(e)
                    .size(10.0)
                    .color(Color32::from_rgb(0xe0, 0x80, 0x80)),
            );
        }
        if let Some(n) = ctx.notice.as_ref() {
            ui.label(
                RichText::new(n)
                    .size(10.0)
                    .color(Color32::from_rgb(0x9e, 0xd0, 0x8e)),
            );
        }
        // ドロップ検知(領域全体)
        let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(bytes) = f.bytes {
                let path = f.path.clone().unwrap_or_else(|| f.name.clone().into());
                ctx.composer.push_dropped(&path, bytes.to_vec());
            } else if let Some(path) = f.path {
                if let Ok(data) = std::fs::read(&path) {
                    ctx.composer.push_dropped(&path, data);
                } else {
                    ctx.composer.error = Some(format!("読めないファイル: {}", path.display()));
                }
            }
        }
    });
}

/// 投稿フォームの投稿先チャンネル選択(F-06-4・仕様決定 V)。
/// 構成はカラムの channel_picker と同じ(フォロー中一覧+検索)で、
/// 状態と取得はアプリ側の composer 用スロットに置く
fn composer_channel_picker(ui: &mut Ui, ctx: &mut UiCtx<'_>) {
    let picker = &mut *ctx.composer_channel_picker;
    ui.separator();
    ui.label(RichText::new("投稿先チャンネル").strong().size(11.0));
    if ui
        .button(RichText::new("通常投稿(チャンネル指定なし)").size(11.0))
        .clicked()
    {
        ctx.ops.push(UiOp::ComposerSetChannel(None));
        *ctx.composer_channel_open = false;
    }
    let mut q = picker.query.clone();
    if ui
        .add(
            egui::TextEdit::singleline(&mut q)
                .hint_text("検索…")
                .desired_width(f32::INFINITY),
        )
        .changed()
    {
        ctx.ops.push(UiOp::ComposerChannelPickerQuery(q.clone()));
        picker.query = q;
    }
    // 一覧が長いとフォームが伸びるので高さを抑える
    egui::ScrollArea::vertical()
        .id_salt("composer_channel_list")
        .max_height(200.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !picker.followed.is_empty() {
                ui.label(RichText::new("フォロー中").size(10.0).color(Color32::GRAY));
                let followed = picker.followed.clone();
                for ch in &followed {
                    if ui
                        .add(
                            egui::Button::new(format!("📺 {}", ch.name))
                                .wrap()
                                .frame(false),
                        )
                        .clicked()
                    {
                        ctx.ops.push(UiOp::ComposerSetChannel(Some((
                            ch.id.clone(),
                            ch.name.clone(),
                        ))));
                        *ctx.composer_channel_open = false;
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
                        ctx.ops.push(UiOp::ComposerSetChannel(Some((
                            ch.id.clone(),
                            ch.name.clone(),
                        ))));
                        *ctx.composer_channel_open = false;
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
        });
}
