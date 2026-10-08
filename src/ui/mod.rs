//! デッキ UI(F-02)。モックで検証した骨格を実データへ接続したもの。
//! 描画側はアプリ状態への参照を UiCtx にまとめて受け取り、副作用は
//! UiOp として積んで app 層が処理する(モックの Op 方式と同じ)。

mod card;
mod mfm;

pub use card::{CardState, display_name};
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
    // Phase 7: 投稿と操作(F-06/F-07)
    /// 返信対象のセット(投稿フォームへ。引用と相互排他)
    ReplyTo {
        id: String,
        label: String,
    },
    /// 引用対象のセット
    Quote {
        id: String,
        label: String,
    },
    /// 対象の解除(true=返信、false=引用)
    ClearTarget(bool),
    /// フォームから投稿
    PostNote,
    /// 添付の取り外し(インデックス)
    RemoveAttachment(usize),
    /// 即時リノート(F-06)
    Renote(String),
    /// リアクションピッカーの開閉(F-07-2)
    OpenReactionPicker(String),
    CloseReactionPicker,
    /// ピッカーで選んだリアクションを付与
    PickReaction {
        note_id: String,
        reaction: String,
    },
    /// バッジクリックのトグル(自分のは取り消し、それ以外は付与)。
    /// `reaction` はローカルカウントを動かすキー、`send` は付与時に
    /// reactions/create へ送る値(取消では None で何も送らない)
    ToggleReaction {
        note_id: String,
        reaction: String,
        send: Option<String>,
        mine: bool,
    },
    /// チャンネル名クリックでそのチャンネルの channel カラムを開く
    /// (F-05-7・仕様決定 Q)。既存の channel カラムがあれば対象を差し替える
    OpenChannel {
        channel_id: String,
    },
    // Phase 8: ビューア・プロフィール・設定
    /// 画像ビューアを開く(F-08-1)。files はそのノートの画像全件、index は開始位置
    OpenViewer {
        files: Vec<crate::model::DriveFile>,
        index: usize,
        /// カード側で既に開封済みのセンシティブ画像のファイル ID(VWR-02)
        revealed: std::collections::HashSet<String>,
    },
    CloseViewer,
    /// ビューアの画像めくり(-1=前、+1=次)
    ViewerStep(i32),
    /// ビューア内でのセンシティブ画像の開封(F-08-1, VWR-02)
    ViewerReveal(String),
    /// プロフィールを開く(F-05-6)。user_id で users/show+users/notes を取る
    OpenProfile {
        user_id: String,
    },
    CloseProfile,
    /// 設定画面を開閉(F-09-3)
    OpenSettings,
    CloseSettings,
    /// キャッシュの消去(F-09-3)。true=画像、false=絵文字一覧
    ClearCache(bool),
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
    /// 投稿フォーム(F-06)
    pub composer: &'a mut crate::composer::Composer,
    /// ピッカー用絵文字一覧(F-07-3)
    pub emoji_list: &'a [crate::model::Emoji],
    /// リアクションピッカーの開閉状態(F-07-2)
    pub reaction_picker: &'a mut Option<crate::app::ReactionPickerState>,
    /// 一行通知(投稿成功など)
    pub notice: &'a mut Option<String>,
    /// 画像ビューアの開閉状態(F-08-1)
    pub viewer: &'a mut Option<ViewerState>,
    /// プロフィールの開閉状態(F-05-6)
    pub profile: &'a mut Option<crate::app::ProfileState>,
    /// 設定画面の開閉(F-09-3)
    pub settings_win: &'a mut bool,
    /// キャッシュの現在サイズ(画像バイト, 絵文字一覧バイト)。設定画面表示用
    pub cache_sizes: (u64, u64),
}

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
    egui::TopBottomPanel::bottom("composer").show(ctx, |ui| {
        composer_panel(ui, ui_ctx);
    });
    reaction_picker_window(ctx, ui_ctx);
    viewer_window(ctx, ui_ctx);
    profile_window(ctx, ui_ctx);
    settings_window(ctx, ui_ctx);
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
            if ui.button("⚙").on_hover_text("設定").clicked() {
                ctx.ops.push(UiOp::OpenSettings);
            }
        });
    });
}

/// 投稿フォーム(F-06)。常設のボトムパネルで、返信/引用は対象を
/// セットして同じフォームから投稿する
fn composer_panel(ui: &mut Ui, ctx: &mut UiCtx<'_>) {
    use crate::composer::{MAX_ATTACHMENTS, visibility_label};
    // 投稿中は編集を不可にする。成功時にフォーム全体をリセットするため、
    // 送信中の追記がリクエストに含まれないまま消えるのを防ぐ
    ui.add_enabled_ui(!ctx.composer.posting, |ui| {
        ui.horizontal(|ui| {
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
            // 公開範囲(F-06-1)
            egui::ComboBox::from_id_salt("visibility")
                .selected_text(visibility_label(ctx.composer.visibility))
                .show_ui(ui, |ui| {
                    for v in [
                        crate::model::Visibility::Public,
                        crate::model::Visibility::Home,
                        crate::model::Visibility::Followers,
                        crate::model::Visibility::Specified,
                    ] {
                        ui.selectable_value(&mut ctx.composer.visibility, v, visibility_label(v));
                    }
                });
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

/// リアクションピッカー(F-07-2)。検索欄+絵文字グリッドの浮遊ウィンドウ
fn reaction_picker_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
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

/// 画像ビューア(F-08-1)。サムネイルクリックで開く拡大表示。
/// ‹› ボタンと ←→ キーでめくり、×/ESC で閉じる
fn viewer_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
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
fn profile_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
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
fn settings_window(egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn df(id: &str) -> crate::model::DriveFile {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "x.png", "type": "image/png"
        }))
        .unwrap()
    }

    fn df_sensitive(id: &str) -> crate::model::DriveFile {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "x.png", "type": "image/png", "isSensitive": true
        }))
        .unwrap()
    }

    // VWR-01: ビューアのページ送りが範囲にクランプされる(F-08-1)
    #[test]
    fn vwr01_step_clamps() {
        let mut v = ViewerState {
            files: vec![df("a"), df("b"), df("c")],
            index: 0,
            revealed: Default::default(),
        };
        v.step(1);
        assert_eq!(v.index, 1);
        v.step(5);
        assert_eq!(v.index, 2);
        v.step(-10);
        assert_eq!(v.index, 0);
        // 空では常に 0 に戻る
        let mut e = ViewerState::default();
        e.step(3);
        assert_eq!(e.index, 0);
    }

    // VWR-02: ビューア内でも未開封の閲覧注意は覆ったまま(F-08-1)
    #[test]
    fn vwr02_sensitive_stays_covered() {
        let mut v = ViewerState {
            files: vec![df("a"), df_sensitive("b")],
            index: 0,
            revealed: Default::default(),
        };
        // 非センシティブはそのまま可視、センシティブは開封済みになるまで覆う
        assert!(v.is_visible(&v.files[0]));
        assert!(!v.is_visible(&v.files[1]));
        v.revealed.insert("b".to_owned());
        assert!(v.is_visible(&v.files[1]));
    }
}
