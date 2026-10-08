//! ノートカードと通知カードの描画(F-05/F-07/F-08)。
//! モックで検証した見た目を実データ型に対応させたもの。
//! ネスト表示は 1 段まで(F-05-4)、CW の折りたたみは本文・メディア・
//! ネスト参照をまとめて隠す(F-05-3)、センシティブメディアはクリックで展開
//! (F-08-4)、動画・音声は外部ブラウザで開く(F-08-3)。

use super::{UiCtx, UiOp, fmt_time};
use crate::model::{DriveFile, Note, Notification};
use eframe::egui::{self, Color32, Frame, RichText, Sense, Stroke, Ui, vec2};
use std::collections::HashSet;

/// カード単位の展開状態。CW 折りたたみとセンシティブメディアを
/// ノート/ファイル ID で管理する
#[derive(Default)]
pub struct CardState {
    /// 本文を展開している CW 付きノート
    pub cw_open: HashSet<String>,
    /// サムネイルを展開したセンシティブメディア(file id)
    pub media_open: HashSet<String>,
    /// 現在描画中のカード内の「クリック可能な子 widget」の矩形。
    /// カード内部クリックの会話遷移(F-05-5)で、これらの領域への
    /// クリックは子に委ねるための除外ゾーン
    pub click_exclusions: Vec<egui::Rect>,
    /// 押下位置の保持。press_origin はリリースフレームで None に
    /// クリアされるため、押下中の各フレームで記録しておく
    pub press_pos: Option<egui::Pos2>,
}

/// 表示対象のメディア分類。F-08-3 の外部ブラウザ起動対象かどうかの判定に使う
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
    Audio,
    Other,
}

fn media_kind(file: &DriveFile) -> MediaKind {
    if file.file_type.starts_with("image/") {
        MediaKind::Image
    } else if file.file_type.starts_with("video/") {
        MediaKind::Video
    } else if file.file_type.starts_with("audio/") {
        MediaKind::Audio
    } else {
        MediaKind::Other
    }
}

/// ノート 1 件の描画。パブリックインターフェース
pub fn note_card(ui: &mut Ui, note: &Note, ctx: &mut UiCtx<'_>, col_id: u64) {
    // このカードの除外ゾーンを仕切り直す(子が描画中に矩形を積む)
    ctx.card_state.click_exclusions.clear();
    let frame = Frame::group(ui.style())
        .fill(Color32::from_rgb(0x22, 0x24, 0x2a))
        .stroke(Stroke::new(1.0f32, Color32::from_rgb(0x32, 0x34, 0x3c)))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 6));
    // 会話ビューの起点 ID をカード単位で一度だけ決める。
    // 純粋リノートは表示対象(直近のリノート元)に揃え、カード内部
    // クリックと 💬 ボタンで同じ会話を開く(入れ子リノートでの競合防止)
    let conv_id = if note.is_pure_renote() {
        note.renote.as_ref().map(|r| r.id.clone())
    } else {
        None
    }
    .unwrap_or_else(|| note.id.clone());
    let inner = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if note.is_pure_renote()
            && let Some(target) = &note.renote
        {
            // 純粋リノート: バナー行 + リノート元の内容をそのまま表示
            ui.label(
                RichText::new(format!("🔁 {} がリノート", display_name(&note.user)))
                    .color(Color32::from_rgb(0x9e, 0xd0, 0x8e))
                    .size(11.0),
            );
            body(ui, target, ctx, col_id, true, &conv_id);
        } else {
            body(ui, note, ctx, col_id, false, &conv_id);
        }
    });
    // カード本体クリックで会話ビュー(F-05-5)。widget の interact では
    // 子ボタンやラベルの hover-sense widget が遮る/遮られるため、
    // リリース位置と除外ゾーン(クリック可能な子の矩形)で判定する
    let card_rect = inner.response.rect;
    // クリック = カード内で押下を開始し、カード内でリリース。
    // primary_clicked は egui のクリック閾値(max_click_dist)考慮済みで
    // ドラッグは弾かれる。押下位置は press_origin(押下中フレーム)と
    // 同一フレームの PointerButton イベント(押下・リリースが同フレームに
    // 来るケース)の両方から記録する
    ui.ctx().input(|i| {
        if let Some(p) = i.pointer.press_origin() {
            ctx.card_state.press_pos = Some(p);
        }
        for ev in &i.events {
            if let egui::Event::PointerButton {
                button: egui::PointerButton::Primary,
                pos,
                pressed: true,
                ..
            } = ev
            {
                ctx.card_state.press_pos = Some(*pos);
            }
        }
    });
    let released = ui.ctx().input(|i| {
        i.pointer.primary_clicked()
            && i.pointer
                .latest_pos()
                .is_some_and(|p| card_rect.contains(p))
    }) && ctx
        .card_state
        .press_pos
        .is_some_and(|p| card_rect.contains(p));
    if released {
        let pos = ui.ctx().pointer_latest_pos().unwrap_or_default();
        if !ctx
            .card_state
            .click_exclusions
            .iter()
            .any(|r| r.contains(pos))
        {
            ctx.ops.push(UiOp::OpenConversation(col_id, conv_id));
        }
    }
}

fn display_name(user: &crate::model::User) -> String {
    user.name.clone().unwrap_or_else(|| user.username.clone())
}

/// ノート本体(ヘッダ+CW+本文+メディア+ネスト参照+リアクション+操作行)。
/// `bannered` はリノート表示の中身側で「自分の user 行を target のものに差し替える」
/// ため既に本体がリノート元であることを示すフラグではなく、常に target を渡す設計にした
fn body(
    ui: &mut Ui,
    note: &Note,
    ctx: &mut UiCtx<'_>,
    col_id: u64,
    _bannered: bool,
    conv_id: &str,
) {
    // ヘッダ: アバター + 名前 + @id@host + 時刻
    ui.horizontal(|ui| {
        let avatar_size = vec2(28.0, 28.0);
        match &note.user.avatar_url {
            Some(url) => {
                ui.add(
                    egui::Image::new(url)
                        .fit_to_exact_size(avatar_size)
                        .corner_radius(4.0),
                );
            }
            None => {
                let (rect, _) = ui.allocate_exact_size(avatar_size, Sense::hover());
                ui.painter()
                    .rect_filled(rect, 4.0, Color32::from_rgb(0x44, 0x48, 0x55));
            }
        }
        // 時刻用の幅を先に差し引いて名前の縦積みへ渡す。
        // ui.horizontal の子は残幅を全部使うので、vertical が右端まで埋めると
        // 後続の with_layout が右端+spacing に置かれて領域を越え、行が
        // min_rect を広げてカラム全体を太くしてしまう。
        const TIME_SLOT_W: f32 = 56.0;
        let name_w = (ui.available_width() - TIME_SLOT_W).max(60.0);
        let name_rect =
            egui::Rect::from_min_size(ui.cursor().min, vec2(name_w, ui.available_height()));
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(name_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                // horizontal(非 wrap)内の label 既定は Extend で折り返さない。
                // 長い名前/ハンドルがカラム幅を膨張させるので wrapped にする
                ui.horizontal_wrapped(|ui| {
                    let name = display_name(&note.user);
                    // 名前に含まれる :emoji: はユーザー emojis マップで解決する
                    let emojis = note.user.emojis.clone();
                    let pieces = super::mfm::layout_pieces(&name, ui, &mut |n| {
                        emojis
                            .get(n)
                            .cloned()
                            .or_else(|| ctx.emoji_cache.resolve(n))
                    });
                    for p in pieces {
                        match p {
                            super::mfm::Piece::Job(job) => {
                                ui.add(egui::Label::new(job).wrap().selectable(false));
                            }
                            super::mfm::Piece::Emoji(url) => {
                                ui.add(egui::Image::new(url).fit_to_exact_size(vec2(14.0, 14.0)));
                            }
                        }
                    }
                    let host = note.user.host.as_deref().unwrap_or("");
                    let handle = if host.is_empty() {
                        format!("@{}", note.user.username)
                    } else {
                        format!("@{}@{}", note.user.username, host)
                    };
                    ui.add(
                        egui::Label::new(RichText::new(handle).size(11.0).color(Color32::GRAY))
                            .wrap(),
                    );
                });
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.label(
                RichText::new(fmt_time(&note.created_at))
                    .size(11.0)
                    .color(Color32::GRAY),
            );
        });
    });

    // 返信先のインジケータ(F-05-4 の 1 段ネストは末尾の参照フレームで表示)
    if let Some(reply) = &note.reply {
        ui.label(
            RichText::new(format!("↩ {} への返信", display_name(&reply.user)))
                .size(11.0)
                .color(Color32::from_rgb(0x8b, 0xb2, 0xff)),
        );
    }

    // CW(F-05-3): 本文・メディア・ネスト参照をまとめて折りたたむ
    let body_visible = match &note.cw {
        Some(cw) => {
            let open = ctx.card_state.cw_open.contains(&note.id);
            let label = if open {
                format!("▼ CW: {cw}")
            } else {
                format!("▶ CW: {cw}(本文・メディアを隠しています)")
            };
            // Button は既定で折り返さないので、長い CW 文が領域を超えて
            // 確保矩形を広げる(カラム自体が太くなる)のを .wrap() で防ぐ
            let cw_resp = ui.add(
                egui::Button::new(RichText::new(label).color(Color32::from_rgb(0xf0, 0xc0, 0x60)))
                    .wrap()
                    .frame(false),
            );
            ctx.card_state.click_exclusions.push(cw_resp.rect);
            if cw_resp.clicked() {
                if open {
                    ctx.card_state.cw_open.remove(&note.id);
                } else {
                    ctx.card_state.cw_open.insert(note.id.clone());
                }
            }
            open
        }
        None => true,
    };

    if body_visible {
        // 本文
        if let Some(text) = &note.text
            && !text.is_empty()
        {
            let emojis = note.emojis.clone();
            super::mfm::render(
                ui,
                super::mfm::layout_pieces(text, ui, &mut |n| {
                    emojis
                        .get(n)
                        .cloned()
                        .or_else(|| ctx.emoji_cache.resolve(n))
                }),
            );
        }

        // 添付メディア(F-08)
        media_row(ui, note, ctx, col_id);

        // ネスト参照: 返信先と引用元を 1 段だけ簡易表示(F-05-4)
        if let Some(reply) = &note.reply {
            nested_ref(ui, reply, "↩", ctx);
        }
        if let Some(quote) = &note.renote
            && !note.is_pure_renote()
        {
            nested_ref(ui, quote, "❝", ctx);
        }
    }

    // リアクション行(F-07-1)。自分のリアクションは強調
    if !note.reactions.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for (name, count) in &note.reactions {
                reaction_badge(ui, name, *count, note, ctx);
            }
        });
    }

    // 操作行(F-06/E)。投稿系は Phase 7 の範囲のため、現段階では
    // 外部ブラウザで開くリンクのみ有効にしておく
    ui.horizontal(|ui| {
        let url = note.url.clone().or_else(|| note.uri.clone());
        if ui
            .button(RichText::new("💬").size(12.0))
            .on_hover_text("会話を表示")
            .clicked()
        {
            ctx.ops
                .push(UiOp::OpenConversation(col_id, conv_id.to_owned()));
        }
        if let Some(u) = url
            && ui
                .button(RichText::new("↗").size(12.0))
                .on_hover_text("ブラウザで開く")
                .clicked()
        {
            ctx.ops.push(UiOp::OpenUrl(u));
        }
        ui.label(
            RichText::new("↩ 🔁 ❝ 😀 ⋯")
                .size(11.0)
                .color(Color32::DARK_GRAY),
        )
        .on_hover_text("投稿系の操作は Phase 7 で実装予定");
    });
}

/// 添付ファイルの行(F-08)。画像はインラインサムネイル、動画・音声は外部ブラウザ
fn media_row(ui: &mut Ui, note: &Note, ctx: &mut UiCtx<'_>, _col_id: u64) {
    if note.files.is_empty() {
        return;
    }
    let avail = ui.available_width();
    let thumb_w = ((avail - 4.0) / 2.0).clamp(120.0, avail);
    ui.horizontal_wrapped(|ui| {
        for file in &note.files {
            match media_kind(file) {
                MediaKind::Image => image_thumb(ui, file, thumb_w, ctx),
                MediaKind::Video | MediaKind::Audio | MediaKind::Other => {
                    // F-08-3: 動画・音声は外部ブラウザで開く(再生しない)
                    let icon = if file.file_type.starts_with("video/") {
                        "🎬"
                    } else {
                        "🎵"
                    };
                    let label = format!("{icon} {} をブラウザで開く", file.name);
                    if ui
                        .add(egui::Button::new(RichText::new(label).size(11.0)).wrap())
                        .clicked()
                        && let Some(u) = &file.url
                    {
                        ctx.ops.push(UiOp::OpenUrl(u.clone()));
                    }
                }
            }
        }
    });
}

/// 画像サムネイル(F-08-1/-2)。センシティブはクリックで展開(F-08-4)
fn image_thumb(ui: &mut Ui, file: &DriveFile, w: f32, ctx: &mut UiCtx<'_>) {
    let opened = ctx.card_state.media_open.contains(&file.id);
    if file.is_sensitive && !opened {
        let label = format!("⚠ 閲覧注意: {}", file.name);
        let resp = ui.add(
            egui::Button::new(
                RichText::new(label)
                    .size(11.0)
                    .color(Color32::from_rgb(0xf0, 0xa0, 0x80)),
            )
            .wrap(),
        );
        ctx.card_state.click_exclusions.push(resp.rect);
        if resp.clicked() {
            ctx.card_state.media_open.insert(file.id.clone());
        }
        return;
    }
    let src = file.thumbnail_url.as_deref().or(file.url.as_deref());
    if let Some(src) = src {
        // クリックで拡大ビュー(F-08-5)は Phase 8。現段階では原寸を外部で開く
        let resp = ui
            .add(egui::Image::new(src).max_width(w).corner_radius(4.0))
            .interact(Sense::click());
        ctx.card_state.click_exclusions.push(resp.rect);
        if resp.clicked()
            && let Some(u) = &file.url
        {
            ctx.ops.push(UiOp::OpenUrl(u.clone()));
        }
    }
}

/// ネスト参照の簡易カード(F-05-4 の 1 段表示)。本文は先頭だけ折り返し付き
fn nested_ref(ui: &mut Ui, note: &Note, icon: &str, ctx: &mut UiCtx<'_>) {
    let frame = Frame::default()
        .fill(Color32::from_rgb(0x1c, 0x1e, 0x24))
        .stroke(Stroke::new(1.0f32, Color32::from_rgb(0x30, 0x32, 0x3a)))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 4));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(icon).size(11.0));
            ui.add(
                egui::Label::new(
                    RichText::new(display_name(&note.user))
                        .size(11.0)
                        .color(Color32::LIGHT_GRAY),
                )
                .wrap(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(format!("@{}", note.user.username))
                        .size(10.0)
                        .color(Color32::DARK_GRAY),
                )
                .wrap(),
            );
        });
        if let Some(cw) = &note.cw {
            ui.label(
                RichText::new(format!("CW: {cw}"))
                    .size(11.0)
                    .color(Color32::from_rgb(0xf0, 0xc0, 0x60)),
            );
        }
        if let Some(text) = &note.text {
            let emojis = note.emojis.clone();
            super::mfm::render(
                ui,
                super::mfm::layout_pieces(text, ui, &mut |n| {
                    emojis
                        .get(n)
                        .cloned()
                        .or_else(|| ctx.emoji_cache.resolve(n))
                }),
            );
        }
        if !note.files.is_empty() {
            ui.label(
                RichText::new(format!("📎 {}", note.files.len()))
                    .size(10.0)
                    .color(Color32::GRAY),
            );
        }
    });
}

/// リアクションバッジ。カスタム絵文字は画像+個数、Unicode はそのまま
///
/// Frame::show で組むと内容確定まで幅が決まらず、horizontal_wrapped の
/// main_wrap が幅を読めずに各バッジを数 px に潰してしまった。Button 系
/// ウィジェットは配置前に幅が決まるため折り返しが正しく効く
fn reaction_badge(ui: &mut Ui, name: &str, count: u32, note: &Note, ctx: &mut UiCtx<'_>) {
    let mine = note.my_reaction.as_deref() == Some(name);
    let bg = if mine {
        Color32::from_rgb(0x3a, 0x44, 0x58)
    } else {
        Color32::from_rgb(0x2a, 0x2c, 0x33)
    };
    let stroke = Stroke::new(1.0f32, Color32::from_rgb(0x3a, 0x3e, 0x4a));
    let count_text = RichText::new(count.to_string())
        .size(11.0)
        .color(Color32::LIGHT_GRAY);
    if let Some(emoji_name) = name.strip_prefix(':').and_then(|s| s.strip_suffix(':')) {
        // :name: 形式はカスタム絵文字。reaction_emojis → note.emojis → キャッシュの順
        let url = note
            .reaction_emojis
            .get(emoji_name)
            .or_else(|| note.emojis.get(emoji_name))
            .cloned()
            .or_else(|| ctx.emoji_cache.resolve(emoji_name));
        match url {
            Some(u) => {
                ui.add(
                    egui::Button::image_and_text(
                        egui::Image::new(u).fit_to_exact_size(vec2(14.0, 14.0)),
                        count_text,
                    )
                    .fill(bg)
                    .stroke(stroke),
                );
            }
            None => {
                ui.add(
                    egui::Button::new(RichText::new(format!(":{emoji_name}: {count}")).size(11.0))
                        .fill(bg)
                        .stroke(stroke),
                );
            }
        }
    } else {
        ui.add(
            egui::Button::new(RichText::new(format!("{name} {count}")).size(11.0))
                .fill(bg)
                .stroke(stroke),
        );
    }
}

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
            ui.label(
                RichText::new(format!("リアクション: {reaction}"))
                    .size(11.0)
                    .color(Color32::GRAY),
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn file(ty: &str) -> DriveFile {
        serde_json::from_value(serde_json::json!({
            "id": "f1", "name": "x", "type": ty
        }))
        .unwrap()
    }

    // MED-01: メディアの分類(画像=インライン/動画・音声=外部ブラウザ、F-08-1/-3)
    #[test]
    fn med01_kind() {
        assert_eq!(media_kind(&file("image/webp")), MediaKind::Image);
        assert_eq!(media_kind(&file("image/png")), MediaKind::Image);
        assert_eq!(media_kind(&file("video/mp4")), MediaKind::Video);
        assert_eq!(media_kind(&file("audio/mpeg")), MediaKind::Audio);
        assert_eq!(media_kind(&file("application/pdf")), MediaKind::Other);
    }
}
