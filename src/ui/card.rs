//! ノートカードと通知カードの描画(F-05/F-07/F-08)。
//! モックで検証した見た目を実データ型に対応させたもの。
//! ネスト表示は 1 段まで(F-05-4)、CW の折りたたみは本文・メディア・
//! ネスト参照をまとめて隠す(F-05-3)、センシティブメディアはクリックで展開
//! (F-08-4)、動画・音声は外部ブラウザで開く(F-08-3)。

use super::{UiCtx, UiOp, fmt_time};
use crate::model::{
    DriveFile, Note, NoteChannel, Notification, ReactionKey, parse_reaction_key,
    reaction_send_value,
};
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

/// 画像表示の高さ上限目安(仕様決定 M)
const IMG_MAX_H: f32 = 360.0;
/// グリッドセル間の隙間
const GRID_GAP: f32 = 4.0;

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

/// 添付ファイル(F-08)。画像は仕様決定 L の枚数グリッドでインライン表示し、
/// 動画・音声・その他は外部ブラウザで開く(F-08-3)
fn media_row(ui: &mut Ui, note: &Note, ctx: &mut UiCtx<'_>, _col_id: u64) {
    if note.files.is_empty() {
        return;
    }
    // ビューア用にノート内の画像一覧を先に集める(F-08-1)
    let images: Vec<DriveFile> = note
        .files
        .iter()
        .filter(|f| media_kind(f) == MediaKind::Image)
        .cloned()
        .collect();
    let cells = grid_cells(images.len(), ui.available_width());
    match images.len() {
        0 => {}
        // 1 枚: 全幅。2 枚: 横 2 分割
        1 | 2 => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GRID_GAP;
                for (i, file) in images.iter().enumerate() {
                    let (w, h) = cells[i];
                    image_cell(ui, file, &images, i, w, h, ctx);
                }
            });
        }
        // 3 枚: 左大 + 右 2(上下)
        3 => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GRID_GAP;
                let (w, h) = cells[0];
                image_cell(ui, &images[0], &images, 0, w, h, ctx);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = GRID_GAP;
                    for i in 1..3 {
                        let (w, h) = cells[i];
                        image_cell(ui, &images[i], &images, i, w, h, ctx);
                    }
                });
            });
        }
        // 4 枚以上: 2 列グリッド(2 枚ごとに行を切る)
        _ => {
            let mut start = 0;
            while start < images.len() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GRID_GAP;
                    for i in start..(start + 2).min(images.len()) {
                        let (w, h) = cells[i];
                        image_cell(ui, &images[i], &images, i, w, h, ctx);
                    }
                });
                start += 2;
            }
        }
    }
    // 動画・音声・その他の添付は外部ブラウザで開く(F-08-3)
    ui.horizontal_wrapped(|ui| {
        for file in &note.files {
            if media_kind(file) == MediaKind::Image {
                continue;
            }
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
    });
}

/// 枚数グリッドのセル定義(IMG-01・仕様決定 L)。
/// 戻り値は各セルの (幅, 高さ上限)。3 枚は先頭セルが左大(全高)になる
fn grid_cells(n: usize, avail: f32) -> Vec<(f32, f32)> {
    let cell_w = ((avail - GRID_GAP) / 2.0).max(60.0);
    let half_h = (IMG_MAX_H - GRID_GAP) / 2.0;
    match n {
        0 => Vec::new(),
        1 => vec![(avail, IMG_MAX_H)],
        2 => vec![(cell_w, IMG_MAX_H); 2],
        3 => vec![(cell_w, IMG_MAX_H), (cell_w, half_h), (cell_w, half_h)],
        _ => vec![(cell_w, half_h); n],
    }
}

/// インラインペイン表示のソース(IMG-03・仕様決定 N)。
/// thumbnailUrl 優先、未設定は url にフォールバック
fn inline_src(file: &DriveFile) -> Option<&str> {
    file.thumbnail_url.as_deref().or(file.url.as_deref())
}

/// セル内の表示寸法(IMG-02・仕様決定 M)。object-fit: contain 相当で、
/// 小さい原寸はセル幅いっぱいまで拡大し、高さは上限で留める。
/// 原寸情報がないときは None(ロード後の縮小のみに委ねる)
fn fit_display_size(file: &DriveFile, cell_w: f32, max_h: f32) -> Option<egui::Vec2> {
    let p = file.properties.as_ref()?;
    let ow = p.width?.max(1) as f32;
    let oh = p.height?.max(1) as f32;
    let scale = (cell_w / ow).min(max_h / oh);
    Some(vec2(ow * scale, oh * scale))
}

/// 画像セルに確保する高さ(IMG-02)。閲覧注意の折りたたみや描画ソースなしは
/// 最小限、原寸既知はフィット後の高さ、原寸不明はロード済みなら実表示
/// サイズ・未ロードは控えめな仮高さ。全高を取らないのは小さい
/// サムネイルで大きな空白が残るため
fn cell_estimate_h(
    file: &DriveFile,
    opened: bool,
    cell_w: f32,
    max_h: f32,
    loaded_size: Option<egui::Vec2>,
) -> f32 {
    if file.is_sensitive && !opened {
        return 20.0;
    }
    if inline_src(file).is_none() {
        return 20.0;
    }
    if let Some(s) = fit_display_size(file, cell_w, max_h) {
        return s.y;
    }
    loaded_size
        .map(|s| s.y.min(max_h))
        .unwrap_or_else(|| 120.0_f32.min(max_h))
}

/// 画像セル(F-08-1/-2)。センシティブはクリックで展開(F-08-4)、
/// 通常はアプリ内ビューア(F-08-1)を開く
fn image_cell(
    ui: &mut Ui,
    file: &DriveFile,
    images: &[DriveFile],
    index: usize,
    cell_w: f32,
    max_h: f32,
    ctx: &mut UiCtx<'_>,
) {
    let opened = ctx.card_state.media_open.contains(&file.id);
    // セル幅は描画内容に関係なく確保する: 縦長画像や閲覧注意ボタンが
    // 細いままだと horizontal の次のセルが寄ってグリッドが崩れるため。
    // 高さは原寸不明でも全高を取らず、ロード済みなら実表示サイズ・
    // 未ロードは控えめな仮高さに留める(小さいサムネイルで大きな空白が残るため)。
    // 閲覧注意を展開していないセルではロード呼び出し自体を走らせない
    // (閲覧注意ボタンの表示だけで画像の取得が始まってしまう)
    let loaded_size = if !file.is_sensitive || opened {
        inline_src(file).and_then(|s| {
            egui::Image::new(s)
                .max_size(vec2(cell_w, max_h))
                .load_and_calc_size(ui, vec2(cell_w, max_h))
        })
    } else {
        None
    };
    let est_h = cell_estimate_h(file, opened, cell_w, max_h, loaded_size);
    ui.allocate_ui_with_layout(
        vec2(cell_w, est_h),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            if file.is_sensitive && !opened {
                let resp = ui.add(
                    egui::Button::new(
                        RichText::new(format!("⚠ 閲覧注意: {}", file.name))
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
            let Some(src) = inline_src(file) else {
                return;
            };
            let img = match fit_display_size(file, cell_w, max_h) {
                Some(size) => egui::Image::new(src).fit_to_exact_size(size),
                // 原寸不明はロード後のテクスチャの大きさに任せる(縮小のみ)
                None => egui::Image::new(src).max_size(vec2(cell_w, max_h)),
            }
            .corner_radius(4.0);
            // クリックで拡大ビューア(F-08-1)
            let resp = ui.add(img).interact(Sense::click());
            ctx.card_state.click_exclusions.push(resp.rect);
            if resp.clicked() {
                ctx.ops.push(UiOp::OpenViewer {
                    files: images.to_vec(),
                    index,
                    revealed: ctx.card_state.media_open.clone(),
                });
            }
        },
    );
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

/// リアクションバッジ(F-07-1/-4)。カスタム絵文字は画像+個数、
/// Unicode・未解決はキーの生テキスト+個数。クリックでトグル(決定 O)
///
/// Frame::show で組むと内容確定まで幅が決まらず、horizontal_wrapped の
/// main_wrap が幅を読めずに各バッジを数 px に潰してしまった。Button 系
/// ウィジェットは配置前に幅が決まるため折り返しが正しく効く
fn reaction_badge(ui: &mut Ui, name: &str, count: u32, note: &Note, ctx: &mut UiCtx<'_>) {
    let key = parse_reaction_key(name);
    // 自分のリアクション: バッジキーとの一致(ローカルは `:name:`/
    // `:name@.:` の形式違いを同名扱い)のほか、リモート絵文字への
    // 相乗り(ローカル :name@.: を送信済み)も自分のものとして扱う
    let mut mine = note
        .my_reaction
        .as_deref()
        .is_some_and(|m| reaction_keys_match(m, name));
    if let ReactionKey::Remote(n, _) = key {
        mine |= note
            .my_reaction
            .as_deref()
            .is_some_and(|r| r == format!(":{n}@.:"));
    }
    let bg = if mine {
        Color32::from_rgb(0x3a, 0x44, 0x58)
    } else {
        Color32::from_rgb(0x2a, 0x2c, 0x33)
    };
    let stroke = Stroke::new(1.0f32, Color32::from_rgb(0x3a, 0x3e, 0x4a));
    let count_text = RichText::new(count.to_string())
        .size(11.0)
        .color(Color32::LIGHT_GRAY);
    // 絵文字が解決できれば画像+個数、未解決はキーの生テキスト+個数
    let button = match reaction_emoji_url(note, name, ctx) {
        Some(u) => egui::Button::image_and_text(
            egui::Image::new(u).fit_to_exact_size(vec2(14.0, 14.0)),
            count_text,
        ),
        None => egui::Button::new(RichText::new(format!("{name} {count}")).size(11.0)),
    }
    .fill(bg)
    .stroke(stroke);
    // クリック可否(決定 O): リモート絵文字は同名のローカル絵文字がある
    // ときだけ相乗り可能、無ければ表示のみ
    let actionable = match key {
        ReactionKey::Unicode | ReactionKey::Local(_) => true,
        ReactionKey::Remote(n, _) => remote_badge_toggleable(n, ctx.emoji_list),
    };
    let resp = if actionable {
        ui.add(button)
    } else {
        ui.add_enabled(false, button)
            .on_disabled_hover_text("同名のローカル絵文字がないため相乗りできません")
    };
    // 除外矩形に登録しないとカード内クリック判定が会話を開いてしまう
    ctx.card_state.click_exclusions.push(resp.rect);
    if actionable && resp.clicked() {
        // 送る値: リモートへの相乗りはローカル :name@.: に変換(決定 O)、
        // その他はキーから F-07-5 形式へ正規化。取消は my_reaction の
        // 実キーでローカルカウントを引く
        let send = match key {
            ReactionKey::Remote(n, _) => Some(format!(":{n}@.:")),
            _ => reaction_send_value(name),
        };
        if let Some(send) = send {
            let affect = if mine {
                note.my_reaction.clone().unwrap_or_else(|| send.clone())
            } else {
                send.clone()
            };
            ctx.ops.push(UiOp::ToggleReaction {
                note_id: note.id.clone(),
                reaction: affect,
                send: if mine { None } else { Some(send) },
                mine,
            });
        }
    }
}

/// ノート内マップだけでリアクション絵文字 URL を解決する(F-07-4)。
/// ローカル(`:name@.:`)は reactionEmojis(`name@.`/`name` キー)と
/// note.emojis を見て、リモート(`:name@host:`)は reactionEmojis の
/// `name@host` キーのみを見る(io 実測で reactionEmojis はリモート分のみ)。
/// EmojiCache を使わない純粋部分で、None のとき呼び出し側がキャッシュを見る
fn reaction_emoji_url_in_note(note: &Note, key: &str) -> Option<String> {
    match parse_reaction_key(key) {
        ReactionKey::Local(name) => note
            .reaction_emojis
            .get(&format!("{name}@."))
            .or_else(|| note.reaction_emojis.get(name))
            .or_else(|| note.emojis.get(name))
            .cloned(),
        ReactionKey::Remote(name, host) => {
            note.reaction_emojis.get(&format!("{name}@{host}")).cloned()
        }
        ReactionKey::Unicode => None,
    }
}

/// リアクション絵文字の画像 URL 解決(F-07-4)。ノート内マップを先に見て、
/// ローカル絵文字は EmojiCache(/api/emoji)にも委ねる
fn reaction_emoji_url(note: &Note, key: &str, ctx: &mut UiCtx<'_>) -> Option<String> {
    reaction_emoji_url_in_note(note, key).or_else(|| match parse_reaction_key(key) {
        ReactionKey::Local(name) => ctx.emoji_cache.resolve(name),
        _ => None,
    })
}

/// リアクションキーの同名判定(F-07-4)。ローカル絵文字は `:name:` と
/// `:name@.:` の形式違いを同一として扱う(my_reaction とバッジキーの比較用)
fn reaction_keys_match(a: &str, b: &str) -> bool {
    match (parse_reaction_key(a), parse_reaction_key(b)) {
        (ReactionKey::Local(x), ReactionKey::Local(y)) => x == y,
        (ReactionKey::Remote(x, h1), ReactionKey::Remote(y, h2)) => x == y && h1 == h2,
        (ReactionKey::Unicode, ReactionKey::Unicode) => a == b,
        _ => false,
    }
}

/// リモート絵文字バッジの相乗り可否(REA-07・仕様決定 O)。
/// 同名のローカル絵文字(ピッカー一覧掲載)があれば `:name@.:` でトグルできる
fn remote_badge_toggleable(name: &str, emoji_list: &[crate::model::Emoji]) -> bool {
    emoji_list.iter().any(|e| e.name == name)
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

    fn note() -> Note {
        serde_json::from_value(serde_json::json!({
            "id": "n1", "createdAt": "t", "userId": "u1",
            "user": {"id": "u1", "username": "me"}
        }))
        .unwrap()
    }

    fn emoji(name: &str) -> crate::model::Emoji {
        crate::model::Emoji {
            name: name.to_owned(),
            url: "https://x/e.png".to_owned(),
            aliases: vec![],
            category: None,
            is_sensitive: false,
            local_only: false,
        }
    }

    // CH-02(一部): チャンネル色の解釈と当該カラムでの省略ルール
    // (F-05-7・仕様決定 P/R)。バーと名前行の描画自体は実機検証で見る
    #[test]
    fn ch02_channel_display_rules() {
        assert_eq!(
            channel_color(Some("#88f")),
            Color32::from_rgb(0x88, 0x88, 0xff)
        );
        assert_eq!(
            channel_color(Some("#a0b1c2")),
            Color32::from_rgb(0xa0, 0xb1, 0xc2)
        );
        // 解釈不能・未設定は既定色
        assert_eq!(channel_color(Some("xyz")), channel_color(None));
        // 当該チャンネルの channel カラム内のみ省略(決定 R)
        assert!(channel_row_hidden(Some("ch1"), "ch1"));
        assert!(!channel_row_hidden(Some("ch1"), "ch2"));
        assert!(!channel_row_hidden(None, "ch1"));
        // 純粋リノートはリノート元のチャンネルを使う(wrapper と異なる
        // チャンネルでも表示対象であるリノート元を優先)
        let mut n = note();
        n.channel = Some(crate::model::NoteChannel {
            id: "chA".to_owned(),
            name: Some("A".to_owned()),
            color: None,
            is_sensitive: false,
        });
        let mut inner = note();
        inner.channel = Some(crate::model::NoteChannel {
            id: "ch9".to_owned(),
            name: Some("開発".to_owned()),
            color: Some("#88f".to_owned()),
            is_sensitive: false,
        });
        n.renote_id = Some("t1".to_owned());
        n.renote = Some(Box::new(inner));
        assert_eq!(display_channel(&n).map(|c| c.id.as_str()), Some("ch9"));
        // リノート元に channel が無いときだけ wrapper にフォールバック
        let mut n2 = note();
        n2.channel = Some(crate::model::NoteChannel {
            id: "chA".to_owned(),
            name: Some("A".to_owned()),
            color: None,
            is_sensitive: false,
        });
        n2.renote_id = Some("t2".to_owned());
        n2.renote = Some(Box::new(note()));
        assert_eq!(display_channel(&n2).map(|c| c.id.as_str()), Some("chA"));
        // リノート元が未取得(renote_id だけで renote なし)でも
        // wrapper のチャンネルは消えない
        let mut n3 = note();
        n3.channel = Some(crate::model::NoteChannel {
            id: "chA".to_owned(),
            name: Some("A".to_owned()),
            color: None,
            is_sensitive: false,
        });
        n3.renote_id = Some("t3".to_owned());
        assert_eq!(display_channel(&n3).map(|c| c.id.as_str()), Some("chA"));
    }

    // IMG-01: 枚数グリッド(仕様決定 L)。1=全幅+上限、2=横 2 分割、
    // 3=左大+右 2、4 枚以上=2 列
    #[test]
    fn img01_grid_cells() {
        let w = 300.0;
        let cw = (w - GRID_GAP) / 2.0;
        let half = (IMG_MAX_H - GRID_GAP) / 2.0;
        assert_eq!(grid_cells(0, w), Vec::<(f32, f32)>::new());
        assert_eq!(grid_cells(1, w), vec![(w, IMG_MAX_H)]);
        assert_eq!(grid_cells(2, w), vec![(cw, IMG_MAX_H), (cw, IMG_MAX_H)]);
        assert_eq!(
            grid_cells(3, w),
            vec![(cw, IMG_MAX_H), (cw, half), (cw, half)]
        );
        let cells = grid_cells(5, w);
        assert_eq!(cells.len(), 5);
        assert!(cells.iter().all(|&(cw2, h)| cw2 == cw && h == half));
    }

    // IMG-04: セルに確保する高さ(IMG-02)。原寸不明で全高を取ると
    // 小さいサムネイルで大きな空白が残るため、ロード済みは実表示
    // サイズ・未ロードは控えめな仮高さに留める
    #[test]
    fn img04_cell_estimate_h() {
        // 閲覧注意の折りたたみは最小限
        let mut f = file("image/webp");
        f.is_sensitive = true;
        f.url = Some("https://x/f.webp".to_owned());
        assert_eq!(cell_estimate_h(&f, false, 150.0, 360.0, None), 20.0);
        // 閲覧注意を展開済みなら通常どおり見積もる
        f.properties = Some(crate::model::FileProperties {
            width: Some(300),
            height: Some(300),
        });
        assert_eq!(cell_estimate_h(&f, true, 150.0, 360.0, None), 150.0);
        // URL もサムネイルも無ければ高さを取らない
        let f2 = file("image/webp");
        assert_eq!(cell_estimate_h(&f2, false, 150.0, 360.0, None), 20.0);
        // 原寸不明でもロード済みなら実表示サイズで留める
        let mut f3 = file("image/webp");
        f3.thumbnail_url = Some("https://x/t.webp".to_owned());
        assert_eq!(
            cell_estimate_h(&f3, false, 150.0, 360.0, Some(vec2(150.0, 40.0))),
            40.0
        );
        // 未ロードの原寸不明は控えめな仮高さ(全高ではない)
        assert_eq!(cell_estimate_h(&f3, false, 150.0, 360.0, None), 120.0);
        // 原寸既知はフィット後の高さ
        let mut f4 = file("image/webp");
        f4.url = Some("https://x/f.webp".to_owned());
        f4.properties = Some(crate::model::FileProperties {
            width: Some(100),
            height: Some(800),
        });
        // 150x1200 → 高さ上限 360 に収まるので幅 45 x 360
        assert_eq!(cell_estimate_h(&f4, false, 150.0, 360.0, None), 360.0);
    }

    // IMG-02: セルいっぱいまでの拡大と高さ上限(仕様決定 M、
    // object-fit: contain 相当)
    #[test]
    fn img02_fit_display_size() {
        let f = |w: u32, h: u32| {
            serde_json::from_value::<DriveFile>(serde_json::json!({
                "id": "f1", "name": "x", "type": "image/png",
                "properties": {"width": w, "height": h}
            }))
            .unwrap()
        };
        // 小さい原寸はセル幅まで拡大(100x50 → セル幅 200 で 2 倍)
        assert_eq!(
            fit_display_size(&f(100, 50), 200.0, 360.0),
            Some(vec2(200.0, 100.0))
        );
        // 大きい原寸はセルに収まるよう縮小
        assert_eq!(
            fit_display_size(&f(2000, 1000), 200.0, 360.0),
            Some(vec2(200.0, 100.0))
        );
        // 縦長は高さ上限で留まる(幅はセル幅未満)
        assert_eq!(
            fit_display_size(&f(100, 1000), 200.0, 360.0),
            Some(vec2(36.0, 360.0))
        );
        // 原寸情報が無いときは None(縮小のみ)
        assert_eq!(fit_display_size(&file("image/png"), 200.0, 360.0), None);
    }

    // IMG-03: インライン表示は thumbnailUrl 優先・url フォールバック(決定 N)
    #[test]
    fn img03_inline_src() {
        let f = |thumb: Option<&str>, url: Option<&str>| {
            serde_json::from_value::<DriveFile>(serde_json::json!({
                "id": "f1", "name": "x", "type": "image/png",
                "thumbnailUrl": thumb, "url": url
            }))
            .unwrap()
        };
        assert_eq!(
            inline_src(&f(Some("https://t/th.webp"), Some("https://o/orig.png"))),
            Some("https://t/th.webp")
        );
        assert_eq!(
            inline_src(&f(None, Some("https://o/orig.png"))),
            Some("https://o/orig.png")
        );
        assert_eq!(inline_src(&f(None, None)), None);
    }

    // REA-04: `:name@.:` はローカル絵文字として解決(F-07-4)。
    // reactionEmojis(`name@.`/`name` キー)と note.emojis を見る
    #[test]
    fn rea04_local_resolution() {
        let mut n = note();
        n.emojis
            .insert("cat".to_owned(), "https://x/cat.png".to_owned());
        assert_eq!(
            reaction_emoji_url_in_note(&n, ":cat@.:"),
            Some("https://x/cat.png".to_owned())
        );
        // reactionEmojis の `name@.` キーも使う
        let mut n = note();
        n.reaction_emojis
            .insert("cat@.".to_owned(), "https://x/cat2.png".to_owned());
        assert_eq!(
            reaction_emoji_url_in_note(&n, ":cat@.:"),
            Some("https://x/cat2.png".to_owned())
        );
        // `:name:` 形式もローカルとして同じ解決に乗る
        assert_eq!(
            reaction_emoji_url_in_note(&n, ":cat:"),
            Some("https://x/cat2.png".to_owned())
        );
    }

    // REA-05: `:name@host:` は reactionEmojis の `name@host` キーから
    // 解決(F-07-4)。note.emojis の同名ローカル絵文字は使わない
    #[test]
    fn rea05_remote_resolution() {
        let mut n = note();
        n.reaction_emojis.insert(
            "blob@remote.tld".to_owned(),
            "https://r/blob.png".to_owned(),
        );
        n.emojis
            .insert("blob".to_owned(), "https://x/local.png".to_owned());
        assert_eq!(
            reaction_emoji_url_in_note(&n, ":blob@remote.tld:"),
            Some("https://r/blob.png".to_owned())
        );
    }

    // REA-06: 未解決は None(= バッジはテキストフォールバック)。
    // Unicode キーも画像解決なし
    #[test]
    fn rea06_unresolved() {
        let n = note();
        assert_eq!(reaction_emoji_url_in_note(&n, ":unknown@.:"), None);
        assert_eq!(reaction_emoji_url_in_note(&n, ":unknown@remote.tld:"), None);
        assert_eq!(reaction_emoji_url_in_note(&n, "❤"), None);
    }

    // REA-07: リモート絵文字バッジの相乗り可否(決定 O)。
    // 同名のローカル絵文字がピッカー一覧にあればトグル可能
    #[test]
    fn rea07_remote_badge_toggleable() {
        let list = vec![emoji("blob")];
        assert!(remote_badge_toggleable("blob", &list));
        assert!(!remote_badge_toggleable("nyan", &list));
    }

    // REA-04(追加面): ローカル絵文字キーの `:name:`/`:name@.:` 形式違いを
    // 同名扱いする(my_reaction とバッジキーの比較、バッジ分裂の防止)
    #[test]
    fn rea04_reaction_keys_match() {
        assert!(reaction_keys_match(":cat:", ":cat@.:"));
        assert!(reaction_keys_match(":cat@.:", ":cat:"));
        assert!(!reaction_keys_match(":cat:", ":dog:"));
        assert!(reaction_keys_match(":cat@x.tld:", ":cat@x.tld:"));
        assert!(!reaction_keys_match(":cat:", ":cat@x.tld:"));
        assert!(reaction_keys_match("❤", "❤"));
        assert!(!reaction_keys_match("❤", ":heart:"));
    }
}
