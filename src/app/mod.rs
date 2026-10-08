//! アプリ状態とイベント集約層。
//! UI(egui)は通信層を直接呼ばず、tokio タスクが結果を `AppEvent` として
//! チャネル経由で返し、`update` で状態遷移させる。
//! Phase 6 では認証完了後にストリーミングを張り、カラムごとの REST 初期
//! 取得/ページング/欠落補充/購読管理をここで集約する(F-03-5/6)。

mod handlers;
mod spawn;
mod tray;

use self::spawn::EMOJI_BATCH;
use self::tray::build_tray;
use crate::api::{self, ApiClient, TimelinePage};
use crate::composer::{Composer, PendingFile};
use crate::config::{self, AppConfig};
use crate::deck::{AddableKind, ColumnDeck};
use crate::emoji::{self, EmojiCache};
use crate::model::{Channel, Emoji, Note, Notification, User};
use crate::streaming::{StreamChannel, StreamEvent, StreamHandle};
use crate::ui::{self, CardState, UiCtx};
use eframe::egui;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
use tokio::task::JoinHandle;

#[derive(Debug)]
enum AuthState {
    /// 起動直後。保存済みトークンの検証中、またはトークンなしで判定済みになる前。
    Bootstrapping,
    LoginNeeded {
        reason: Option<String>,
    },
    /// MiAuth URL を提示して認可をポーリング中。abort で中断できる。
    MiauthWaiting {
        session_id: String,
    },
    /// 通信障害など、失効ではない理由で検証できなかった状態(再試行可能)。
    VerifyFailed {
        message: String,
    },
    Authenticated {
        user: Box<User>,
    },
}

/// UI のボタン操作。`&self.auth` の借用中に `&mut self` を触れないよう、
/// クリック内容をいったんこの列挙にして描画後に適用する。
#[derive(Debug, Clone, Copy)]
enum UiAction {
    StartMiauth,
    CancelMiauth,
    RetryVerify,
    ReLogin,
}

/// REST 取得の種別(初回・過去ページ・再接続の欠落補充)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchKind {
    Initial,
    Next,
    Backfill,
}

/// REST 取得結果。カラム種別で応答の型が違うので enum で包む
#[derive(Debug)]
enum FetchResult {
    /// TL/チャンネル(フィルタ適用済み+生カーソル)
    Page(TimelinePage),
    /// メンション(生の Vec。カーソルは先頭/末尾 ID から計算)
    Mentions(Vec<Note>),
    /// 通知一覧
    Notifications(Vec<Notification>),
}

/// 絵文字個別取得の結果(F-05-2)。一時失敗は否定キャッシュせず再試行する
#[derive(Debug)]
enum EmojiFetch {
    Found(String),
    /// サーバーが「存在しない」と答えた(否定キャッシュする)
    Missing,
    /// 429・通信失敗など再試行可能な失敗
    Transient,
}

/// チャンネル一覧の取得先。カラムの選択 UI(F-03-7)と
/// 投稿フォームの投稿先選択(F-06-4)で同じ取得経路を使い分ける
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelListTarget {
    Column(u64),
    Composer,
}

enum AppEvent {
    VerifyResult {
        result: Result<User, api::ApiError>,
        came_from_keyring: bool,
    },
    MiauthResult {
        result: Result<(String, User), String>,
    },
    /// 購読の ack(sub_id の確定)
    Subscribed {
        channel: StreamChannel,
        sub_id: String,
    },
    /// カラムの REST 取得完了
    FetchResult {
        col_id: u64,
        kind: FetchKind,
        /// 発行時点のカラム世代。invalidate 前の飛行結果を破棄する照合に使う
        fetch_gen: u64,
        /// 補充が上限で打ち切られた場合の続き位置(untilId)。None=完走
        backfill_tail: Option<String>,
        result: Result<FetchResult, String>,
    },
    /// 会話ビュー(F-05-5): 選択ノート自身 + 会話チェーン
    ConversationResult {
        col_id: u64,
        root: Option<Box<Note>>,
        result: Result<Vec<Note>, String>,
    },
    /// フォロー中チャンネル一覧(F-03-7/F-06-4)
    FollowedResult {
        target: ChannelListTarget,
        result: Result<Vec<Channel>, String>,
    },
    /// チャンネル検索結果(F-03-7/F-06-4)
    ChannelSearchResult {
        target: ChannelListTarget,
        /// 発行時のクエリ(古いクエリの応答で上書きしない照合に使う)
        query: String,
        result: Result<Vec<Channel>, String>,
    },
    /// 絵文字のオンデマンド解決結果(F-05-2)
    EmojiResult { name: String, outcome: EmojiFetch },
    /// 投稿/返信/引用/リノートの結果(F-06)
    PostResult {
        result: Result<Box<Note>, String>,
        /// アップロード結果を反映した添付(失敗時にフォームへ戻す)
        files: Vec<PendingFile>,
        /// フォーム発の投稿か。false(リノート等)ならフォーム状態は
        /// 触らず通知だけにする(下書きの添付・本文を消さない)
        from_composer: bool,
    },
    /// リアクション付与/取消の結果(F-07)
    ReactionResult {
        note_id: String,
        reaction: String,
        add: bool,
        result: Result<(), String>,
    },
    /// ピッカー用絵文字一覧の取得結果(F-07-3)
    EmojiListResult { result: Result<Vec<Emoji>, String> },
    /// プロフィールの取得結果(F-05-6)。users/show と users/notes を一括で取る
    ProfileResult {
        /// 結果を捨てるかどうかの判定に使う要求時のユーザー ID(PRF-03)
        user_id: String,
        user: Result<Box<User>, String>,
        notes: Result<Vec<Note>, String>,
    },
    /// ストリーミング層からの転送イベント
    Stream(StreamEvent),
}

pub struct NmnlApp {
    runtime: tokio::runtime::Runtime,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    config: AppConfig,
    auth: AuthState,
    /// 現在の認証に使っているトークン(永続化とは別にメモリに保持)
    token: Option<String>,
    /// キーリングへ保存できたか。false なら次回起動で再認証(F-01-2)
    token_persisted: bool,
    /// 環境変数トークン由来か(開発用の入口)
    token_from_env: bool,
    miauth_task: Option<JoinHandle<()>>,
    // Phase 6: デッキと通信
    deck: ColumnDeck,
    /// 認証済みの API クライアント(生成は一度だけ)
    client: Option<ApiClient>,
    /// 多重化 WS のハンドル。 None = 未接続
    stream: Option<StreamHandle>,
    /// 購読中のチャンネル→sub_id(subscribe の ack で確定)
    /// ack 済みの購読(チャンネル→sub_id)
    subs: HashMap<StreamChannel, String>,
    /// 発行済みだが ack 未着の購読(ack 待ちの間に再同期しても重複発行しない)
    pending_subs: std::collections::HashSet<StreamChannel>,
    /// ストリーミング接続の表示用文字列
    stream_status: String,
    /// 認証後に初期取得を流したか(認証遷移の一度きりガード)
    bootstrapped_stream: bool,
    // UI 状態
    emoji_cache: EmojiCache,
    card_state: CardState,
    settings_open: Option<u64>,
    picker_open: Option<u64>,
    resize_base: Option<(u64, f32)>,
    add_kind: AddableKind,
    // Phase 7: 投稿と操作
    /// 投稿フォーム(F-06)
    composer: Composer,
    /// フォームの投稿先チャンネル選択 UI の状態(F-06-4)
    composer_channel_picker: crate::deck::ChannelPicker,
    /// フォーム側チャンネル選択ポップアップの開閉
    composer_channel_open: bool,
    /// ピッカー用の絵文字一覧(F-07-3)。起動時にディスクキャッシュから
    /// 先読みし、認証後に最新を取って上書きする
    emoji_list: Vec<Emoji>,
    /// リアクションピッカーの対象ノート(F-07-2)
    reaction_picker: Option<ReactionPickerState>,
    /// 操作結果の一行通知(投稿成功・失敗など)
    notice: Option<String>,
    // Phase 8: 仕上げ
    /// 画像ビューアの開閉状態(F-08-1)
    viewer: Option<ui::ViewerState>,
    /// プロフィールウィンドウ(F-05-6)
    profile: Option<ProfileState>,
    /// 設定ウィンドウの開閉(F-09-3)
    settings_win: bool,
    /// キャッシュの現在サイズ(画像, 絵文字一覧)。設定画面を開く/消すときに更新
    cache_sizes: (u64, u64),
    /// 画像キャッシュのローダー(egui の bytes loader 兼キャッシュ消去の入口)
    image_loader: Option<Arc<crate::image_loader::CachedImageLoader>>,
    /// トレイ常駐(F-09-4)。ビルドに失敗した環境では None として通常動作に落ちる
    tray: Option<tray_icon::TrayIcon>,
    /// トレイメニュー「表示」の ID
    tray_show_id: tray_icon::menu::MenuId,
    /// トレイメニュー「終了」の ID
    tray_quit_id: tray_icon::menu::MenuId,
    /// トレイに入った(非表示)状態か。隠れた間も低頻度で repaint して
    /// トレイイベントを拾い続ける
    hidden_to_tray: bool,
    /// N-01 の起動時間を最初のフレームで一度だけログに出すフラグ
    startup_logged: bool,
}

/// プロフィールウィンドウの状態(F-05-6)
#[derive(Debug, Default)]
pub struct ProfileState {
    pub user_id: String,
    pub user: Option<User>,
    pub notes: Vec<Note>,
    /// 取得中(どちらかでも未着)か
    pub loading: bool,
    pub error: Option<String>,
}

impl ProfileState {
    /// 到着した結果が現在開いているユーザーのものか(PRF-03)
    pub fn accepts(&self, user_id: &str) -> bool {
        self.user_id == user_id
    }
}

/// リアクションピッカーの開閉状態(検索クエリを保持)
#[derive(Debug, Default)]
pub struct ReactionPickerState {
    pub note_id: String,
    pub query: String,
}

impl NmnlApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let config = AppConfig::load();
        let runtime = tokio::runtime::Runtime::new().expect("tokio ランタイムの起動に失敗");

        // 画像ローダー: デコーダは egui_extras、URI→バイトは自前のディスクキャッシュ
        // 付きローダー(F-08-2)。bytes loader は後から登録したものが先に試される
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let image_loader = Arc::new(crate::image_loader::CachedImageLoader::new(
            runtime.handle().clone(),
        ));
        cc.egui_ctx.add_bytes_loader(image_loader.clone());

        // トレイ常駐(F-09-4)。メニューは「表示」「終了」
        let (tray, tray_show_id, tray_quit_id) = build_tray();

        let deck = ColumnDeck::from_specs(config.columns.clone());

        let mut app = Self {
            runtime,
            tx,
            rx,
            config,
            auth: AuthState::Bootstrapping,
            token: None,
            token_persisted: false,
            token_from_env: false,
            miauth_task: None,
            deck,
            client: None,
            stream: None,
            subs: HashMap::new(),
            pending_subs: std::collections::HashSet::new(),
            stream_status: "未接続".to_owned(),
            bootstrapped_stream: false,
            emoji_cache: EmojiCache::default(),
            card_state: CardState::default(),
            settings_open: None,
            picker_open: None,
            resize_base: None,
            add_kind: AddableKind::Timeline(config::TimelineKind::Home),
            composer: Composer::default(),
            composer_channel_picker: crate::deck::ChannelPicker::default(),
            composer_channel_open: false,
            // F-07-3: 前回取得した一覧があれば即表示できるよう先読みする
            emoji_list: emoji::load_emoji_list(),
            reaction_picker: None,
            notice: None,
            viewer: None,
            profile: None,
            settings_win: false,
            cache_sizes: (0, 0),
            image_loader: Some(image_loader),
            tray,
            tray_show_id,
            tray_quit_id,
            hidden_to_tray: false,
            startup_logged: false,
        };

        // 文字サイズ(F-09-5): UI 全体の拡縮として zoom_factor に適用する
        cc.egui_ctx.set_zoom_factor(app.config.ui_scale);

        match config::token::resolve_token(config::HOST) {
            config::token::TokenResolution::Found { token, source } => {
                app.token = Some(token.clone());
                app.token_from_env = source == config::token::TokenSource::EnvVar;
                app.token_persisted = source == config::token::TokenSource::Keyring;
                let came_from_keyring = source == config::token::TokenSource::Keyring;
                app.spawn_verify(token, came_from_keyring, &cc.egui_ctx);
            }
            config::token::TokenResolution::Missing => {
                app.auth = AuthState::LoginNeeded { reason: None };
            }
        }
        app
    }
}

impl eframe::App for NmnlApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // N-01: 起動から最初のフレーム描画までを一度だけ記録
        if !self.startup_logged {
            self.startup_logged = true;
            if let Some(t) = crate::STARTED_AT.get() {
                eprintln!("起動時間(最初のフレームまで): {:?}", t.elapsed());
            }
        }
        while let Ok(ev) = self.rx.try_recv() {
            self.handle_event(ev, ctx);
        }
        // 認証後の初回ブートストラップ
        if matches!(self.auth, AuthState::Authenticated { .. }) {
            self.bootstrap_streaming(ctx);
        }
        // ウィンドウサイズを設定に反映しておく(終了時に保存)。
        // screen_rect は zoom 適用後の egui ポイントなので、保存値は
        // zoom_factor を掛けてネイティブ論理サイズに戻す
        // (そのまま保存すると再起動のたびにサイズがずれる)
        let size = ctx.input(|i| i.screen_rect().size()) * ctx.zoom_factor();
        if size.x > 0.0 && size.y > 0.0 {
            self.config.window.width = size.x;
            self.config.window.height = size.y;
        }

        // トレイ常駐(F-09-4): 最小化を検出して非表示化し、トレイ側の
        // クリック/メニューで復帰する。トレイ無しの環境では何もしない
        if self.tray.is_some() {
            if !self.hidden_to_tray && ctx.input(|i| i.viewport().minimized) == Some(true) {
                self.hidden_to_tray = true;
                ctx.send_viewport_cmd(egui::viewport::ViewportCommand::Visible(false));
            }
            // トレイ非表示のデスクトップでもタスクバー等から復帰できるよう、
            // 最小化が外部から解除されたらトレイ待機も解除する
            if self.hidden_to_tray && ctx.input(|i| i.viewport().minimized) == Some(false) {
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(egui::viewport::ViewportCommand::Visible(true));
            }
            let mut restore = false;
            let mut quit = false;
            for ev in tray_icon::TrayIconEvent::receiver().try_iter() {
                if matches!(ev, tray_icon::TrayIconEvent::Click { .. }) {
                    restore = true;
                }
            }
            for ev in tray_icon::menu::MenuEvent::receiver().try_iter() {
                if ev.id == self.tray_show_id {
                    restore = true;
                } else if ev.id == self.tray_quit_id {
                    quit = true;
                }
            }
            if restore && self.hidden_to_tray {
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(egui::viewport::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::viewport::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::viewport::ViewportCommand::Focus);
            }
            if quit {
                ctx.send_viewport_cmd(egui::viewport::ViewportCommand::Close);
            }
            // 隠れている間はイベントが来ないので、低頻度で repaint して
            // トレイイベントを拾い続ける
            if self.hidden_to_tray {
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }

        // 認証済みならデッキ、未認証なら認証 UI を出す
        let authed = matches!(self.auth, AuthState::Authenticated { .. });
        if authed {
            let me = match &self.auth {
                AuthState::Authenticated { user } => Some(user.as_ref()),
                _ => None,
            };
            let mut ops = Vec::new();
            {
                let mut ui_ctx = UiCtx {
                    ops: &mut ops,
                    emoji_cache: &mut self.emoji_cache,
                    card_state: &mut self.card_state,
                    settings_open: &mut self.settings_open,
                    picker_open: &mut self.picker_open,
                    resize_base: &mut self.resize_base,
                    add_kind: &mut self.add_kind,
                    stream_status: &self.stream_status,
                    me,
                    composer: &mut self.composer,
                    emoji_list: &self.emoji_list,
                    reaction_picker: &mut self.reaction_picker,
                    notice: &mut self.notice,
                    viewer: &mut self.viewer,
                    profile: &mut self.profile,
                    settings_win: &mut self.settings_win,
                    cache_sizes: self.cache_sizes,
                    composer_channel_picker: &mut self.composer_channel_picker,
                    composer_channel_open: &mut self.composer_channel_open,
                    ui_scale: self.config.ui_scale,
                };
                ui::deck_ui(ctx, &mut self.deck, &mut ui_ctx);
            }
            for op in ops {
                self.apply_op(op, ctx);
            }
            // 描画中に解決要求が来た絵文字を取得する(F-05-2)
            let pending = self.emoji_cache.drain_pending();
            for name in pending.into_iter().take(EMOJI_BATCH) {
                self.spawn_emoji(name, ctx);
            }
            // カラム構成の変更を設定へ反映(F-02-3)
            if self.deck.take_dirty() {
                self.config.columns = self.deck.specs();
                if let Err(e) = self.config.save() {
                    eprintln!("設定の保存に失敗しました: {e}");
                }
            }
            return;
        }

        let mut action = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("NMNL-browser");
            match &self.auth {
                AuthState::Bootstrapping => {
                    ui.label("起動中…");
                }
                AuthState::LoginNeeded { reason } => {
                    if let Some(r) = reason {
                        ui.colored_label(egui::Color32::YELLOW, r);
                    }
                    ui.label("misskey.io のアカウントでログインしてください。");
                    if ui.button("MiAuth でログイン").clicked() {
                        action = Some(UiAction::StartMiauth);
                    }
                    ui.label("開発用: 環境変数 NMNL_TOKEN でトークンを直接指定できます。");
                }
                AuthState::MiauthWaiting { session_id } => {
                    let url = api::miauth_url(config::HOST, session_id, "NMNL-browser");
                    ui.label("ブラウザで認可を完了してください。");
                    ui.hyperlink_to("認可ページを開く", &url);
                    if ui.button("キャンセル").clicked() {
                        action = Some(UiAction::CancelMiauth);
                    }
                }
                AuthState::VerifyFailed { message } => {
                    ui.colored_label(egui::Color32::YELLOW, message);
                    if ui.button("再試行").clicked() {
                        action = Some(UiAction::RetryVerify);
                    }
                    if ui.button("ログインし直す").clicked() {
                        action = Some(UiAction::ReLogin);
                    }
                }
                AuthState::Authenticated { .. } => {}
            }
        });
        match action {
            Some(UiAction::StartMiauth) => self.start_miauth(ctx),
            Some(UiAction::CancelMiauth) => {
                if let Some(t) = self.miauth_task.take() {
                    t.abort();
                }
                self.auth = AuthState::LoginNeeded { reason: None };
            }
            Some(UiAction::RetryVerify) => {
                if let Some(t) = self.token.clone() {
                    let from_keyring = !self.token_from_env;
                    self.spawn_verify(t, from_keyring, ctx);
                    self.auth = AuthState::Bootstrapping;
                } else {
                    self.auth = AuthState::LoginNeeded { reason: None };
                }
            }
            Some(UiAction::ReLogin) => {
                self.auth = AuthState::LoginNeeded { reason: None };
            }
            None => {}
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.columns = self.deck.specs();
        if let Err(e) = self.config.save() {
            eprintln!("設定の保存に失敗しました: {e}");
        }
        if let Some(stream) = &self.stream {
            stream.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PRF-03: 開いているユーザーと違う到着結果は採用しない(F-05-6)
    #[test]
    fn prf03_profile_result_matching() {
        let mut p = ProfileState {
            user_id: "alice".to_owned(),
            ..Default::default()
        };
        assert!(p.accepts("alice"));
        assert!(!p.accepts("bob"));
        p.user_id = "bob".to_owned();
        assert!(p.accepts("bob"));
    }
}
