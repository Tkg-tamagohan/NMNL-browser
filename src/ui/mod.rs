//! デッキ UI(F-02)。モックで検証した骨格を実データへ接続したもの。
//! 描画側はアプリ状態への参照を UiCtx にまとめて受け取り、副作用は
//! UiOp として積んで app 層が処理する(モックの Op 方式と同じ)。

mod card;
mod columns;
mod composer;
mod mfm;
#[cfg(test)]
mod tests;
mod windows;

pub use card::{CardState, display_name};
pub use mfm::Piece;
pub use windows::ViewerState;

use self::columns::{column_panel, drop_zone};
use self::composer::composer_panel;
use self::windows::{profile_window, reaction_picker_window, settings_window, viewer_window};
use crate::deck::{AddableKind, ColumnDeck, NotificationFilter};
use crate::emoji::EmojiCache;
use crate::model::User;
use eframe::egui::{self, Color32, RichText, Ui};

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
    /// 返信対象のセット(投稿フォームへ。引用と相互排他)。
    /// 対象がチャンネル所属なら channelId を継承する(仕様決定 W)
    ReplyTo {
        id: String,
        label: String,
        channel: Option<crate::model::NoteChannel>,
    },
    /// 引用対象のセット
    Quote {
        id: String,
        label: String,
        channel: Option<crate::model::NoteChannel>,
    },
    /// 対象の解除(true=返信、false=引用)
    ClearTarget(bool),
    /// フォームから投稿
    PostNote,
    /// 添付の取り外し(インデックス)
    RemoveAttachment(usize),
    /// 即時リノート(F-06)。対象がチャンネル所属なら channelId を継承する
    Renote {
        note_id: String,
        channel: Option<crate::model::NoteChannel>,
    },
    /// 投稿フォームのチャンネル選択 UI の開閉(F-06-4)
    ComposerChannelPickerOpen,
    /// フォーム側チャンネル検索のクエリ変更(F-06-4)
    ComposerChannelPickerQuery(String),
    /// フォームの投稿先チャンネルを設定/解除する(F-06-4)。
    /// Some((id, name)) でチャンネル投稿、None で通常投稿へ戻す
    ComposerSetChannel(Option<(String, String)>),
    /// channel カラムの「このチャンネルに投稿」導線(F-06-4・仕様決定 V)
    PostToChannel {
        channel_id: String,
        name: Option<String>,
    },
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
    /// UI スケール(文字サイズ)の変更(F-09-5)
    SetUiScale(f32),
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
    /// 投稿フォームのチャンネル選択 UI 状態(F-06-4)
    pub composer_channel_picker: &'a mut crate::deck::ChannelPicker,
    /// フォーム側チャンネル選択ポップアップの開閉
    pub composer_channel_open: &'a mut bool,
    /// 現在の UI スケール(F-09-5、設定画面のスライダー表示用)
    pub ui_scale: f32,
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
    // 投稿フォームはメインカラム内に表示する(F-06-5、仕様決定 X)。
    // メインカラムが未配置のときだけ、投稿経路を残すために
    // ボトムパネルのフォームを自動表示する
    if !deck.has_main_column() {
        egui::TopBottomPanel::bottom("composer").show(ctx, |ui| {
            composer_panel(ui, ui_ctx);
        });
    }
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
