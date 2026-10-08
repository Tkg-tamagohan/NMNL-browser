//! ノートカードと通知カードの描画(F-05/F-07/F-08)。
//! モックで検証した見た目を実データ型に対応させたもの。
//! ネスト表示は 1 段まで(F-05-4)、CW の折りたたみは本文・メディア・
//! ネスト参照をまとめて隠す(F-05-3)、センシティブメディアはクリックで展開
//! (F-08-4)、動画・音声は外部ブラウザで開く(F-08-3)。

mod media;
mod notif;
mod reactions;
#[cfg(test)]
mod tests;

pub use notif::notif_card;

use self::media::media_row;
use self::reactions::reaction_badge;
use super::{UiCtx, UiOp, fmt_time};
use crate::model::{Note, NoteChannel};
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

/// ノート 1 件の描画。パブリックインターフェース。
/// `hide_channel` は仕様決定 R の省略判定用に、このカードを表示している
/// カラムが対象とするチャンネル ID(channel カラムのみ Some)
pub fn note_card(
    ui: &mut Ui,
    note: &Note,
    ctx: &mut UiCtx<'_>,
    col_id: u64,
    hide_channel: Option<&str>,
) {
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
            body(ui, target, ctx, col_id, true, &conv_id, hide_channel);
        } else {
            body(ui, note, ctx, col_id, false, &conv_id, hide_channel);
        }
    });
    // カード左端のチャンネル色バー(F-05-7・仕様決定 P)。
    // 当該チャンネルの channel カラム内では名前行とともに省略(仕様決定 R)
    if let Some(ch) = display_channel(note)
        && !channel_row_hidden(hide_channel, &ch.id)
    {
        let card = inner.response.rect;
        let bar = egui::Rect::from_min_max(
            egui::pos2(card.left() + 1.0, card.top() + 1.0),
            egui::pos2(card.left() + 4.0, card.bottom() - 1.0),
        );
        ui.painter()
            .rect_filled(bar, 1.5, channel_color(ch.color.as_deref()));
    }
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

pub fn display_name(user: &crate::model::User) -> String {
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
    hide_channel: Option<&str>,
) {
    // ヘッダ: アバター + 名前 + @id@host + 時刻
    ui.horizontal(|ui| {
        let avatar_size = vec2(28.0, 28.0);
        // アバターはクリックでプロフィール(F-05-6)
        let avatar_resp = match &note.user.avatar_url {
            Some(url) => ui
                .add(
                    egui::Image::new(url)
                        .fit_to_exact_size(avatar_size)
                        .corner_radius(4.0),
                )
                .interact(Sense::click()),
            None => {
                let (rect, resp) = ui.allocate_exact_size(avatar_size, Sense::click());
                ui.painter()
                    .rect_filled(rect, 4.0, Color32::from_rgb(0x44, 0x48, 0x55));
                resp
            }
        };
        ctx.card_state.click_exclusions.push(avatar_resp.rect);
        if avatar_resp.clicked() {
            ctx.ops.push(UiOp::OpenProfile {
                user_id: note.user.id.clone(),
            });
        }
        // 時刻用の幅を先に差し引いて名前の縦積みへ渡す。
        // ui.horizontal の子は残幅を全部使うので、vertical が右端まで埋めると
        // 後続の with_layout が右端+spacing に置かれて領域を越え、行が
        // min_rect を広げてカラム全体を太くしてしまう。
        const TIME_SLOT_W: f32 = 56.0;
        let name_w = (ui.available_width() - TIME_SLOT_W).max(60.0);
        let name_rect =
            egui::Rect::from_min_size(ui.cursor().min, vec2(name_w, ui.available_height()));
        let name_scope = ui.scope_builder(
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
        // 名前の縦積みブロックもクリックでプロフィール(F-05-6)。
        // ID は unique_id でスコープする(ui.id は兄弟の子 ui で共有される
        // ため使えない): 同一ノートが複数カラムやリノート内に同時描画
        // されても衝突しない(Phase 10 実機で検出)
        let name_resp = ui.interact(
            name_scope.response.rect,
            ui.unique_id().with(("profile_click", &note.id)),
            Sense::click(),
        );
        ctx.card_state.click_exclusions.push(name_resp.rect);
        if name_resp.clicked() {
            ctx.ops.push(UiOp::OpenProfile {
                user_id: note.user.id.clone(),
            });
        }
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

    // チャンネル名行(F-05-7・仕様決定 P)。ノート下部・リアクション行の上に
    // アイコン+名前を出す。クリックで channel カラムを開く(仕様決定 Q)
    if let Some(ch) = display_channel(note)
        && !channel_row_hidden(hide_channel, &ch.id)
    {
        let name = ch.name.as_deref().unwrap_or("チャンネル");
        let resp = ui.add(
            egui::Button::new(
                RichText::new(format!("📺 {name}"))
                    .size(11.0)
                    .color(Color32::from_rgb(0x8b, 0xb2, 0xff)),
            )
            .wrap()
            .frame(false),
        );
        ctx.card_state.click_exclusions.push(resp.rect);
        if resp.clicked() {
            ctx.ops.push(UiOp::OpenChannel {
                channel_id: ch.id.clone(),
            });
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

    // 操作行(F-06/E・仕様決定 E)。行内の全ボタンは click_exclusions に
    // 矩形登録しないと、カード内クリック判定が OpenConversation を
    // 二重発火させる
    ui.horizontal(|ui| {
        let url = note.url.clone().or_else(|| note.uri.clone());
        let btn = |label: &str, tip: &str, ui: &mut Ui, ctx: &mut UiCtx<'_>| {
            let resp = ui
                .button(RichText::new(label).size(12.0))
                .on_hover_text(tip);
            ctx.card_state.click_exclusions.push(resp.rect);
            resp
        };
        if btn("💬", "会話を表示", ui, ctx).clicked() {
            ctx.ops
                .push(UiOp::OpenConversation(col_id, conv_id.to_owned()));
        }
        if let Some(u) = url
            && btn("↗", "ブラウザで開く", ui, ctx).clicked()
        {
            ctx.ops.push(UiOp::OpenUrl(u));
        }
        // ↩/❝ はフォームに対象をセット、🔁 は即時リノート、😀 はピッカー
        let label = format!(
            "@{}{}",
            note.user.username,
            note.user
                .host
                .as_deref()
                .map(|h| format!("@{h}"))
                .unwrap_or_default()
        );
        if btn("↩", "返信", ui, ctx).clicked() {
            ctx.ops.push(UiOp::ReplyTo {
                id: note.id.clone(),
                label: label.clone(),
                // 対象がチャンネル所属なら channelId を継承する(仕様決定 W)
                channel: note.channel_for_inherit(),
            });
        }
        if btn("🔁", "リノート", ui, ctx).clicked() {
            ctx.ops.push(UiOp::Renote {
                note_id: note.id.clone(),
                channel: note.channel_for_inherit(),
            });
        }
        if btn("❝", "引用", ui, ctx).clicked() {
            ctx.ops.push(UiOp::Quote {
                id: note.id.clone(),
                label,
                channel: note.channel_for_inherit(),
            });
        }
        if btn("😀", "リアクション", ui, ctx).clicked() {
            ctx.ops.push(UiOp::OpenReactionPicker(note.id.clone()));
        }
    });
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

/// カードに出すチャンネル(F-05-7・仕様決定 P)。純粋リノートは表示対象が
/// リノート元なのでそちらのチャンネルを優先し、リノート元が未取得または
/// チャンネルなしのときだけ wrapper のチャンネルにフォールバックする
fn display_channel(note: &Note) -> Option<&NoteChannel> {
    if note.is_pure_renote() {
        note.renote
            .as_ref()
            .and_then(|r| r.channel.as_ref())
            .or(note.channel.as_ref())
    } else {
        note.channel.as_ref()
    }
}

/// 仕様決定 R: 当該チャンネルの channel カラム内ではチャンネル名行を省略する
fn channel_row_hidden(col_channel: Option<&str>, channel_id: &str) -> bool {
    col_channel == Some(channel_id)
}

/// チャンネル色(F-05-7・仕様決定 P)。#rgb/#rrggbb を解釈し、
/// 解釈できない値や未設定は io 既定のアクセント色に倒す
fn channel_color(color: Option<&str>) -> Color32 {
    fn parse(c: &str) -> Option<Color32> {
        let h = c.strip_prefix('#').unwrap_or(c);
        let v = u32::from_str_radix(h, 16).ok()?;
        let (r, g, b) = match h.len() {
            3 => (((v >> 8) & 0xf) * 17, ((v >> 4) & 0xf) * 17, (v & 0xf) * 17),
            6 => ((v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff),
            _ => return None,
        };
        Some(Color32::from_rgb(r as u8, g as u8, b as u8))
    }
    color
        .and_then(parse)
        .unwrap_or(Color32::from_rgb(0x4a, 0xc5, 0x7a))
}
