//! アプリ状態とイベント集約層。
//! UI(egui)は通信層を直接呼ばず、tokio タスクが結果を `AppEvent` として
//! チャネル経由で返し、`update` で状態遷移させる。
//! Phase 6 では認証完了後にストリーミングを張り、カラムごとの REST 初期
//! 取得/ページング/欠落補充/購読管理をここで集約する(F-03-5/6)。

use crate::api::{self, ApiClient, MiauthStatus, Paging, TimelinePage};
use crate::composer::{Composer, PendingFile};
use crate::config::{self, AppConfig, ColumnKind, ColumnSpec};
use crate::deck::{AddableKind, ColumnDeck, ColumnView};
use crate::emoji::{self, EmojiCache};
use crate::model::{Channel, CreateNote, Emoji, Note, Notification, User};
use crate::streaming::{self, StreamChannel, StreamEvent, StreamHandle};
use crate::ui::{self, CardState, UiCtx, UiOp};
use eframe::egui;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
use tokio::task::JoinHandle;

/// 認可待ちポーリングの間隔。miauth/check は認可前 `{"ok":false}` を返すだけなので
/// ユーザーがブラウザで認可するまで周期的に問い合わせる。
const MIAUTH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// ポーリング中の連続エラーの許容数。超えたら認可待ちを打ち切る。
const MIAUTH_MAX_FAILURES: u32 = 15;
/// 認可直後の /api/i 検証の試行数と間隔(トークン伝播の遅延を吸収する)
const MIAUTH_VERIFY_ATTEMPTS: u32 = 3;
const MIAUTH_VERIFY_DELAY: Duration = Duration::from_secs(2);
/// REST 取得のページサイズ(F-03-3)
const PAGE_LIMIT: u32 = 30;
/// 欠落補充のページサイズ。切断中の欠落はこれで拾い、超過分は次回再接続で追う
const BACKFILL_LIMIT: u32 = 50;
/// 絵文字の同時取得上限(フレームあたり)
const EMOJI_BATCH: usize = 12;

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
        user: User,
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

#[derive(Debug)]
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
    /// フォロー中チャンネル一覧(F-03-7)
    FollowedResult {
        col_id: u64,
        result: Result<Vec<Channel>, String>,
    },
    /// チャンネル検索結果(F-03-7)
    ChannelSearchResult {
        col_id: u64,
        /// 発行時のクエリ(古いクエリの応答で上書きしない照合に使う)
        query: String,
        result: Result<Vec<Channel>, String>,
    },
    /// 絵文字のオンデマンド解決結果(F-05-2)
    EmojiResult { name: String, url: Option<String> },
    /// 投稿/返信/引用/リノートの結果(F-06)
    PostResult { result: Result<Box<Note>, String> },
    /// リアクション付与/取消の結果(F-07)
    ReactionResult {
        note_id: String,
        reaction: String,
        add: bool,
        result: Result<(), String>,
    },
    /// ピッカー用絵文字一覧の取得結果(F-07-3)
    EmojiListResult { result: Result<Vec<Emoji>, String> },
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
    /// ピッカー用の絵文字一覧(F-07-3)。起動時にディスクキャッシュから
    /// 先読みし、認証後に最新を取って上書きする
    emoji_list: Vec<Emoji>,
    /// リアクションピッカーの対象ノート(F-07-2)
    reaction_picker: Option<ReactionPickerState>,
    /// 操作結果の一行通知(投稿成功・失敗など)
    notice: Option<String>,
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
        cc.egui_ctx
            .add_bytes_loader(Arc::new(crate::image_loader::CachedImageLoader::new(
                runtime.handle().clone(),
            )));

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
            // F-07-3: 前回取得した一覧があれば即表示できるよう先読みする
            emoji_list: emoji::load_emoji_list(),
            reaction_picker: None,
            notice: None,
        };

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

    fn spawn_verify(&self, token: String, came_from_keyring: bool, ctx: &egui::Context) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let client = ApiClient::with_token(config::HOST, token);
            let result = client.i().await;
            let _ = tx.send(AppEvent::VerifyResult {
                result,
                came_from_keyring,
            });
            ctx.request_repaint();
        });
    }

    fn start_miauth(&mut self, ctx: &egui::Context) {
        let session_id = api::new_session_id();
        let url = api::miauth_url(config::HOST, &session_id, "NMNL-browser");
        if let Err(e) = open::that(&url) {
            eprintln!("ブラウザを開けませんでした({e})。URL: {url}");
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let session = session_id.clone();
        let task = self.runtime.spawn(async move {
            let client = ApiClient::anonymous(config::HOST);
            let mut failures = 0u32;
            loop {
                tokio::time::sleep(MIAUTH_POLL_INTERVAL).await;
                match client.miauth_check(&session).await {
                    Ok(MiauthStatus::Authorized { token, .. }) => {
                        // 取り込んだトークンを /api/i で検証してから通知する
                        // (N-02: 無効なトークンを keyring に保存しない)。
                        let verify = ApiClient::with_token(config::HOST, token.clone());
                        let mut verified = None;
                        let mut last_err = None;
                        for attempt in 0..MIAUTH_VERIFY_ATTEMPTS {
                            match verify.i().await {
                                Ok(user) => {
                                    verified = Some(user);
                                    break;
                                }
                                Err(e) => {
                                    let fatal = e.is_auth_failure();
                                    last_err = Some(e);
                                    if fatal {
                                        break;
                                    }
                                    if attempt + 1 < MIAUTH_VERIFY_ATTEMPTS {
                                        tokio::time::sleep(MIAUTH_VERIFY_DELAY).await;
                                    }
                                }
                            }
                        }
                        let result = match verified {
                            Some(user) => Ok((token, user)),
                            None => Err(format!(
                                "認可されたトークンの検証に失敗しました: {}",
                                last_err
                                    .map(|e| e.to_string())
                                    .unwrap_or_else(|| "不明なエラー".to_owned())
                            )),
                        };
                        let _ = tx.send(AppEvent::MiauthResult { result });
                        ctx.request_repaint();
                        return;
                    }
                    Ok(MiauthStatus::Pending) => failures = 0,
                    Err(e) => {
                        failures += 1;
                        if failures >= MIAUTH_MAX_FAILURES {
                            let _ = tx.send(AppEvent::MiauthResult {
                                result: Err(e.to_string()),
                            });
                            ctx.request_repaint();
                            return;
                        }
                        // 一時的な障害・レート制限(429)を想定し指数バックオフで継続(N-03)
                        tokio::time::sleep(MIAUTH_POLL_INTERVAL * 2u32.pow(failures.min(4))).await;
                    }
                }
            }
        });
        self.miauth_task = Some(task);
        self.auth = AuthState::MiauthWaiting { session_id };
    }

    /// 認証完了時に呼ぶ。WS を張り、各カラムの購読と初回 REST を流す
    fn bootstrap_streaming(&mut self, ctx: &egui::Context) {
        if self.bootstrapped_stream {
            return;
        }
        self.bootstrapped_stream = true;
        let Some(token) = self.token.clone() else {
            return;
        };
        self.client = Some(ApiClient::with_token(config::HOST, token.clone()));
        // StreamManager::spawn は内部で tokio::spawn を呼ぶため、
        // UI スレッド側ではランタイムのコンテキストに入ってから実行する
        let mut stream = {
            let _guard = self.runtime.enter();
            streaming::StreamManager::spawn(config::HOST, Some(&token))
        };
        // ストリーミングイベントを AppEvent へ転送するフォワーダ。
        // egui は repaint 要求が無いと update を回さないので、
        // 到着ごとに request_repaint で UI スレッドを起こす
        let mut srx = stream.take_event_rx();
        let ftx = self.tx.clone();
        let fctx = ctx.clone();
        self.runtime.spawn(async move {
            while let Some(ev) = srx.recv().await {
                if ftx.send(AppEvent::Stream(ev)).is_err() {
                    break;
                }
                fctx.request_repaint();
            }
        });
        self.stream = Some(stream);
        self.sync_subscriptions(ctx);
        let ids: Vec<u64> = self.deck.columns.iter().map(|c| c.id).collect();
        for id in ids {
            self.spawn_fetch(id, FetchKind::Initial, ctx);
        }
        // 絵文字一覧の最新化(F-07-3)
        self.spawn_emoji_list(ctx);
    }

    /// 投稿/返信/引用/リノートの送信(F-06)。添付は先に drive へ
    /// アップロードして fileIds に乗せてから notes/create を呼ぶ
    fn spawn_post(&mut self, mut req: CreateNote, files: Vec<PendingFile>, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.composer.posting = true;
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let mut failed = None;
            for f in &files {
                match client
                    .upload_drive_file(&f.name, &f.mime, (*f.data).clone())
                    .await
                {
                    Ok(d) => req.file_ids.push(d.id),
                    Err(e) => {
                        failed = Some(format!("添付 {} のアップロード失敗: {e}", f.name));
                        break;
                    }
                }
            }
            let result = match failed {
                Some(e) => Err(e),
                None => client
                    .create_note(&req)
                    .await
                    .map(Box::new)
                    .map_err(|e| e.to_string()),
            };
            let _ = tx.send(AppEvent::PostResult { result });
            ctx2.request_repaint();
        });
    }

    /// リアクション付与/取消(F-07)
    fn spawn_reaction(
        &mut self,
        note_id: String,
        reaction: String,
        add: bool,
        ctx: &egui::Context,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let result = if add {
                client.create_reaction(&note_id, &reaction).await
            } else {
                client.delete_reaction(&note_id).await
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ReactionResult {
                note_id,
                reaction,
                add,
                result,
            });
            ctx2.request_repaint();
        });
    }

    /// ピッカー用の絵文字一覧を取る(F-07-3)
    fn spawn_emoji_list(&self, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let result = client.emojis().await.map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::EmojiListResult { result });
            ctx2.request_repaint();
        });
    }

    /// カラム構成と購読を同期する(F-02-1 の追加/削除・F-03-7 のチャンネル変更)
    fn sync_subscriptions(&mut self, ctx: &egui::Context) {
        let Some(stream) = &self.stream else {
            return;
        };
        // いま必要なチャンネル集合(複数カラムが同じチャンネルを共有する)
        let mut desired: std::collections::HashSet<StreamChannel> =
            std::collections::HashSet::new();
        for col in &self.deck.columns {
            if let Some(ch) = col.stream_channel() {
                desired.insert(ch);
            }
        }
        // 不要になった購読を解除
        let stale: Vec<StreamChannel> = self
            .subs
            .keys()
            .filter(|ch| !desired.contains(*ch))
            .cloned()
            .collect();
        for ch in stale {
            if let Some(sub_id) = self.subs.remove(&ch) {
                stream.unsubscribe(&sub_id);
            }
        }
        // 新しい分を張る(ack 待ちの重複発行を pending で防ぐ)
        for ch in desired {
            if self.subs.contains_key(&ch) || self.pending_subs.contains(&ch) {
                continue;
            }
            self.pending_subs.insert(ch.clone());
            let rx = stream.subscribe(ch.clone());
            let tx = self.tx.clone();
            let ch_clone = ch.clone();
            let ctx2 = ctx.clone();
            self.runtime.spawn(async move {
                if let Ok(sub_id) = rx.await {
                    let _ = tx.send(AppEvent::Subscribed {
                        channel: ch_clone,
                        sub_id,
                    });
                    ctx2.request_repaint();
                }
            });
        }
    }

    /// カラムの REST 取得を spawn する。取得中フラグで二重発行を抑止
    fn spawn_fetch(&mut self, col_id: u64, kind: FetchKind, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == col_id) else {
            return;
        };
        if col.fetching {
            return;
        }
        // 取得条件: メインカラムは対象外、チャンネルは選択済みのみ
        if matches!(col.spec.kind, ColumnKind::Main) {
            return;
        }
        if matches!(col.spec.kind, ColumnKind::Channel) && col.spec.channel_id.is_none() {
            return;
        }
        let paging = match kind {
            FetchKind::Initial => Paging {
                limit: PAGE_LIMIT,
                ..Default::default()
            },
            FetchKind::Next => {
                let Some(until) = col.oldest_id.clone() else {
                    return; // 初回取得がまだなら何もしない
                };
                if col.exhausted {
                    return;
                }
                Paging {
                    limit: PAGE_LIMIT,
                    until_id: Some(until),
                    since_id: None,
                }
            }
            FetchKind::Backfill => {
                // 切断時点で保存した最新 ID を起点にする。
                // 復帰直後の新着が先に届いて newest_id が進んでも、
                // 切断中の区間を取り逃さないように切り離した起点を使う
                // take せず保持する: 完走(またはエラー後の再接続)まで
                // 起点を失わない。途中打ち切り時は backfill_until から続く
                let Some(since) = col.backfill_since.clone().or_else(|| col.newest_id.clone())
                else {
                    // 未取得なら初回取得に倒す
                    return self.spawn_fetch(col_id, FetchKind::Initial, ctx);
                };
                Paging {
                    limit: BACKFILL_LIMIT,
                    until_id: col.backfill_until.clone(),
                    since_id: Some(since),
                }
            }
        };
        let spec = col.spec.clone();
        let ntf_excludes = col.ntf_filter.exclude_types();
        let fetch_gen = col.fetch_gen;
        col.fetching = true;
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let (result, backfill_tail) = if kind == FetchKind::Backfill {
                match fetch_backfill_impl(&client, &spec, paging, &ntf_excludes).await {
                    Ok((r, tail)) => (Ok(r), tail),
                    Err(e) => (Err(e), None),
                }
            } else {
                (
                    fetch_page_impl(&client, &spec, &paging, &ntf_excludes).await,
                    None,
                )
            };
            let _ = tx.send(AppEvent::FetchResult {
                col_id,
                kind,
                fetch_gen,
                backfill_tail,
                result,
            });
            ctx2.request_repaint();
        });
    }

    /// 会話ビュー用に選択ノート + 会話チェーンを取る(F-05-5)
    fn spawn_conversation(&self, col_id: u64, note_id: String, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let root = client.show_note(&note_id).await.ok().map(Box::new);
            let result = client
                .conversation(&note_id, 30, 0)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ConversationResult {
                col_id,
                root,
                result,
            });
            ctx2.request_repaint();
        });
    }

    /// フォロー中チャンネル一覧(F-03-7)
    fn spawn_followed_channels(&self, col_id: u64, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let result = client
                .followed_channels(&Paging {
                    limit: 100,
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::FollowedResult { col_id, result });
            ctx2.request_repaint();
        });
    }

    /// チャンネル検索(F-03-7)
    fn spawn_channel_search(&self, col_id: u64, query: String, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let result = client
                .search_channels(&query, 20, 0)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ChannelSearchResult {
                col_id,
                query,
                result,
            });
            ctx2.request_repaint();
        });
    }

    /// 絵文字のオンデマンド取得(F-05-2)
    fn spawn_emoji(&self, name: String, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let url = client.emoji(&name).await.ok().map(|e| e.url);
            let _ = tx.send(AppEvent::EmojiResult { name, url });
            ctx2.request_repaint();
        });
    }

    /// ストリーミングイベントをデッキへ振り分ける
    fn handle_stream_event(&mut self, ev: StreamEvent, ctx: &egui::Context) {
        match ev {
            StreamEvent::Connected { is_reconnect } => {
                self.stream_status = "接続中".to_owned();
                if is_reconnect {
                    // 切断中の欠落を REST で補充する(F-03-6)
                    let ids: Vec<u64> = self.deck.columns.iter().map(|c| c.id).collect();
                    for id in ids {
                        self.spawn_fetch(id, FetchKind::Backfill, ctx);
                    }
                }
            }
            StreamEvent::Disconnected { attempt, retry_in } => {
                self.stream_status = format!(
                    "切断(再接続 {attempt} 回目、{:.0} 秒後)",
                    retry_in.as_secs_f32()
                );
                // 欠落補充の起点を切断時点の最新 ID で固定する。
                // 未補充の起点が残っているときは上書きしない
                // (再接続中の途中受信で起点を失わないため)
                for col in &mut self.deck.columns {
                    if col.backfill_since.is_none() {
                        col.backfill_since = col.newest_id.clone();
                    }
                }
            }
            StreamEvent::Subscribed { .. } => {
                // 購読 ack は sub_id のみ。チャンネルは AppEvent::Subscribed で別経路で届く
            }
            StreamEvent::Note { channel, note, .. } => {
                // ノート本文とその発信者の絵文字をキャッシュへ(F-05-2 の第 1 ソース)
                self.absorb_note_emojis(&note);
                for col in &mut self.deck.columns {
                    if col.stream_channel() == Some(channel.clone()) {
                        col.push_note(*note.clone());
                    }
                }
            }
            StreamEvent::Notification { notification, .. } => {
                let n = *notification;
                if let Some(note) = &n.note {
                    self.absorb_note_emojis(note);
                }
                for col in &mut self.deck.columns {
                    match col.spec.kind {
                        ColumnKind::Notifications => {
                            col.push_notification(n.clone());
                        }
                        // メンションカラムは main の通知イベントからノートを拾う(F-04-2)
                        ColumnKind::Mentions if is_mention_kind(&n.kind) => {
                            if let Some(note) = &n.note {
                                col.push_note(note.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// ノートと入れ子ノートの絵文字マップをキャッシュへ吸収する
    fn absorb_note_emojis(&mut self, note: &Note) {
        self.emoji_cache.absorb_note_emojis(&note.emojis);
        self.emoji_cache.absorb_note_emojis(&note.user.emojis);
        if let Some(r) = &note.reply {
            self.emoji_cache.absorb_note_emojis(&r.emojis);
            self.emoji_cache.absorb_note_emojis(&r.user.emojis);
        }
        if let Some(r) = &note.renote {
            self.emoji_cache.absorb_note_emojis(&r.emojis);
            self.emoji_cache.absorb_note_emojis(&r.user.emojis);
        }
    }

    /// UI 命令の適用(描画後に 1 件ずつ)
    fn apply_op(&mut self, op: UiOp, ctx: &egui::Context) {
        match op {
            UiOp::AddColumn(kind) => {
                let id = self.deck.add(kind);
                self.sync_subscriptions(ctx);
                self.spawn_fetch(id, FetchKind::Initial, ctx);
            }
            UiOp::Remove(id) => {
                self.deck.remove(id);
                self.sync_subscriptions(ctx);
            }
            UiOp::MoveTo(id, to) => self.deck.move_to(id, to),
            UiOp::MoveDelta(id, d) => self.deck.move_delta(id, d),
            UiOp::SetWidth(id, w) => self.deck.set_width(id, w),
            UiOp::SetPaused(id, p) => {
                self.deck.set_paused(id, p);
                // 保留溢れで立った補充起点があれば解除直後に取りに行く
                if self
                    .deck
                    .columns
                    .iter()
                    .any(|c| c.id == id && c.backfill_since.is_some())
                {
                    self.spawn_fetch(id, FetchKind::Backfill, ctx);
                }
            }
            UiOp::SetTimelineKind(id, k) => {
                self.deck.set_timeline_kind(id, k);
                self.sync_subscriptions(ctx);
                self.spawn_fetch(id, FetchKind::Initial, ctx);
            }
            UiOp::SetChannel(id, ch) => {
                self.deck.set_channel(id, ch);
                self.sync_subscriptions(ctx);
                self.spawn_fetch(id, FetchKind::Initial, ctx);
            }
            UiOp::SetFilters(id, f) => {
                self.deck.set_filters(id, f);
                self.spawn_fetch(id, FetchKind::Initial, ctx);
            }
            UiOp::SetNtfFilter(id, f) => {
                self.deck.set_ntf_filter(id, f);
                self.spawn_fetch(id, FetchKind::Initial, ctx);
            }
            UiOp::FetchNextPage(id) => {
                // 初回判定は items ではなくカーソルで見る。
                // フィルタで初回ページが全件落ちてもカーソルは進むので
                // 次は Next(untilId 続き)で歩く必要がある
                let kind = if self
                    .deck
                    .columns
                    .iter()
                    .find(|c| c.id == id)
                    .is_some_and(|c| c.oldest_id.is_none())
                {
                    FetchKind::Initial
                } else {
                    FetchKind::Next
                };
                self.spawn_fetch(id, kind, ctx);
            }
            UiOp::OpenConversation(id, note_id) => {
                if let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == id) {
                    col.view = ColumnView::Conversation {
                        root_id: note_id.clone(),
                        notes: Vec::new(),
                        loading: true,
                        error: None,
                    };
                }
                self.spawn_conversation(id, note_id, ctx);
            }
            UiOp::CloseConversation(id) => {
                if let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == id) {
                    col.view = ColumnView::Timeline;
                }
            }
            UiOp::ChannelPickerOpen(id) => {
                let need = self
                    .deck
                    .columns
                    .iter_mut()
                    .find(|c| c.id == id)
                    .map(|col| {
                        let picker = col.channel_picker.get_or_insert_with(Default::default);
                        if !picker.followed_loaded {
                            picker.loading = true;
                            picker.followed_loaded = true;
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if need {
                    self.spawn_followed_channels(id, ctx);
                }
            }
            UiOp::ChannelPickerQuery(id, q) => {
                if let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == id) {
                    let picker = col.channel_picker.get_or_insert_with(Default::default);
                    picker.query = q.clone();
                    picker.loading = !q.is_empty();
                }
                if !q.is_empty() {
                    self.spawn_channel_search(id, q, ctx);
                }
            }
            UiOp::OpenUrl(u) => {
                let _ = open::that(&u);
            }
            // Phase 7: 投稿と操作
            UiOp::ReplyTo { id, label } => {
                // 返信と引用は相互排他(本文中に両方参照は作れない)
                self.composer.quote_of = None;
                self.composer.reply_to = Some(crate::composer::PostTarget { id, label });
            }
            UiOp::Quote { id, label } => {
                self.composer.reply_to = None;
                self.composer.quote_of = Some(crate::composer::PostTarget { id, label });
            }
            UiOp::ClearTarget(reply) => {
                if reply {
                    self.composer.reply_to = None;
                } else {
                    self.composer.quote_of = None;
                }
            }
            UiOp::PostNote => {
                let me_id = match &self.auth {
                    AuthState::Authenticated { user } => Some(user.id.clone()),
                    _ => None,
                };
                match self.composer.build_request(me_id.as_deref()) {
                    Ok(req) => {
                        self.composer.error = None;
                        self.notice = None;
                        let files = std::mem::take(&mut self.composer.files);
                        self.spawn_post(req, files, ctx);
                    }
                    Err(e) => {
                        self.composer.error = Some(e);
                    }
                }
            }
            UiOp::RemoveAttachment(i) => {
                if i < self.composer.files.len() {
                    self.composer.files.remove(i);
                }
            }
            UiOp::Renote(note_id) => {
                // リノートはフォームを通さず即時投稿(仕様決定 E)
                let req = CreateNote {
                    renote_id: Some(note_id),
                    ..Default::default()
                };
                self.spawn_post(req, Vec::new(), ctx);
            }
            UiOp::OpenReactionPicker(note_id) => {
                self.reaction_picker = Some(ReactionPickerState {
                    note_id,
                    query: String::new(),
                });
            }
            UiOp::CloseReactionPicker => {
                self.reaction_picker = None;
            }
            UiOp::PickReaction { note_id, reaction } => {
                self.reaction_picker = None;
                self.spawn_reaction(note_id, reaction, true, ctx);
            }
            UiOp::ToggleReaction {
                note_id,
                reaction,
                mine,
            } => {
                // 自分のリアクションは取り消し、それ以外は同じ絵文字で付与
                self.spawn_reaction(note_id, reaction, !mine, ctx);
            }
        }
    }

    fn handle_event(&mut self, ev: AppEvent, ctx: &egui::Context) {
        match ev {
            AppEvent::VerifyResult {
                result,
                came_from_keyring,
            } => match result {
                Ok(user) => {
                    self.auth = AuthState::Authenticated { user };
                }
                Err(e) => {
                    if e.is_auth_failure() {
                        // 失効は保存分を消して再認証へ促す(F-01-3)
                        if came_from_keyring {
                            config::token::delete_token(config::HOST);
                        }
                        self.token = None;
                        self.auth = AuthState::LoginNeeded {
                            reason: Some(format!("トークンが失効しています: {e}")),
                        };
                    } else {
                        // 通信障害などでは保存済みトークンを消さない
                        self.auth = AuthState::VerifyFailed {
                            message: e.to_string(),
                        };
                    }
                }
            },
            AppEvent::MiauthResult { result } => {
                self.miauth_task = None;
                match result {
                    Ok((token, user)) => {
                        self.token_persisted = config::token::persist_token(config::HOST, &token);
                        self.token_from_env = false;
                        self.token = Some(token);
                        self.auth = AuthState::Authenticated { user };
                    }
                    Err(msg) => {
                        self.auth = AuthState::LoginNeeded {
                            reason: Some(format!("認証に失敗しました: {msg}")),
                        };
                    }
                }
            }
            AppEvent::Subscribed { channel, sub_id } => {
                self.pending_subs.remove(&channel);
                // ack 到着時点でチャンネルが不要になっていたら即解除する
                let still_needed = self
                    .deck
                    .columns
                    .iter()
                    .any(|c| c.stream_channel().as_ref() == Some(&channel));
                if still_needed {
                    self.subs.insert(channel, sub_id);
                } else if let Some(stream) = &self.stream {
                    stream.unsubscribe(&sub_id);
                }
            }
            AppEvent::Stream(ev) => {
                self.handle_stream_event(ev, ctx);
            }
            AppEvent::FetchResult {
                col_id,
                kind,
                fetch_gen,
                backfill_tail,
                result,
            } => {
                let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == col_id) else {
                    return;
                };
                // 発行後に invalidate(TL 種別/チャンネル/フィルタ変更)されていたら
                // 前世代の結果は捨てる。新しい構成に古い内容が混入するのを防ぐ
                if col.fetch_gen != fetch_gen {
                    return;
                }
                // 生応答が空なら過去は尽きた(F-03-3)。フィルタで全件落ちても
                // カーソルは進むので raw の空判定は oldest_id/件数で見る
                let raw_empty = match &result {
                    Ok(FetchResult::Page(p)) => p.oldest_id.is_none() && p.newest_id.is_none(),
                    Ok(FetchResult::Mentions(v)) => v.is_empty(),
                    Ok(FetchResult::Notifications(v)) => v.is_empty(),
                    Err(_) => false,
                };
                let backfill = matches!(kind, FetchKind::Backfill);
                let result_ok = result.is_ok();
                match result {
                    Ok(FetchResult::Page(page)) => {
                        if backfill {
                            col.append_backfill(page.notes, page.oldest_id, page.newest_id);
                        } else {
                            col.append_page(page.notes, page.oldest_id, page.newest_id);
                        }
                    }
                    Ok(FetchResult::Mentions(notes)) => {
                        // メンションもカーソルは生応答の先頭/末尾から取る
                        let newest = notes.first().map(|n| n.id.clone());
                        let oldest = notes.last().map(|n| n.id.clone());
                        if backfill {
                            col.append_backfill(notes, oldest, newest);
                        } else {
                            col.append_page(notes, oldest, newest);
                        }
                    }
                    Ok(FetchResult::Notifications(notifs)) => {
                        let newest = notifs.first().map(|n| n.id.clone());
                        let oldest = notifs.last().map(|n| n.id.clone());
                        if backfill {
                            col.append_notif_backfill(notifs, oldest, newest);
                        } else {
                            col.append_notif_page(notifs, oldest, newest);
                        }
                    }
                    Err(msg) => {
                        col.error = Some(msg);
                    }
                }
                col.fetching = false;
                if raw_empty && !backfill {
                    col.exhausted = true;
                }
                // 補充が上限で打ち切られた場合は途中位置から続きを取る。
                // 完走したら起点を消す。エラー時は起点を残して次の再接続でやり直す
                let respawn = if backfill && result_ok {
                    match &backfill_tail {
                        Some(tail) => {
                            col.backfill_until = Some(tail.clone());
                            true
                        }
                        None => {
                            // 停止中バッファ溢れで歩き切れていない区間が
                            // 残る場合は起点を保持する(解除時に続きを取る)
                            if !col.pending_overflow {
                                col.backfill_since = None;
                                col.backfill_until = None;
                                // 別区間の欠落が控えていればそちらに進む
                                if let Some((s, u)) = col.extra_backfill.take() {
                                    col.backfill_since = s;
                                    col.backfill_until = u;
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        }
                    }
                } else {
                    false
                };
                if respawn {
                    self.spawn_fetch(col_id, FetchKind::Backfill, ctx);
                }
            }
            AppEvent::ConversationResult {
                col_id,
                root,
                result,
            } => {
                let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == col_id) else {
                    return;
                };
                let ColumnView::Conversation {
                    notes,
                    loading,
                    error,
                    ..
                } = &mut col.view
                else {
                    return;
                };
                *loading = false;
                match result {
                    Ok(mut conv) => {
                        // 選択ノート自身を先頭に挿入(重複は除く)
                        conv.retain(|n| Some(&n.id) != root.as_ref().map(|r| &r.id));
                        if let Some(r) = root {
                            conv.insert(0, *r);
                        }
                        *notes = conv;
                    }
                    Err(e) => {
                        *error = Some(e);
                    }
                }
            }
            AppEvent::FollowedResult { col_id, result } => {
                let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == col_id) else {
                    return;
                };
                let Some(picker) = col.channel_picker.as_mut() else {
                    return;
                };
                picker.loading = false;
                match result {
                    Ok(list) => picker.followed = list,
                    Err(e) => {
                        // 失敗しても loaded を true のままにすると再試行しないので戻す
                        picker.followed_loaded = false;
                        col.error = Some(e);
                    }
                }
            }
            AppEvent::ChannelSearchResult {
                col_id,
                query,
                result,
            } => {
                let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == col_id) else {
                    return;
                };
                let Some(picker) = col.channel_picker.as_mut() else {
                    return;
                };
                // 古いクエリの応答が遅れて届いた場合は現在の検索結果を上書きしない
                if picker.query != query {
                    return;
                }
                picker.loading = false;
                match result {
                    Ok(list) => picker.results = list,
                    Err(e) => col.error = Some(e),
                }
            }
            AppEvent::EmojiResult { name, url } => {
                self.emoji_cache.complete(&name, url);
            }
            AppEvent::PostResult { result } => {
                self.composer.posting = false;
                match result {
                    Ok(note) => {
                        // 投稿成功: フォームをリセット。タイムラインへの
                        // 反映はストリーミング/補充に委ねる
                        self.composer.clear();
                        self.notice = Some(format!("投稿しました({})", note.id));
                    }
                    Err(e) => {
                        self.composer.error = Some(format!("投稿に失敗: {e}"));
                        self.notice = None;
                    }
                }
            }
            AppEvent::ReactionResult {
                note_id,
                reaction,
                add,
                result,
            } => match result {
                Ok(()) => {
                    // 成功したら表示中のノートへローカル反映(F-07)
                    for col in &mut self.deck.columns {
                        col.apply_reaction(&note_id, &reaction, add);
                    }
                }
                Err(e) => {
                    self.notice = Some(format!("リアクション失敗: {e}"));
                }
            },
            AppEvent::EmojiListResult { result } => {
                if let Ok(list) = result {
                    emoji::save_emoji_list(&list);
                    self.emoji_list = list;
                }
            }
        }
    }
}

/// メンションカラムに流す通知種別(F-04-2)。io のメンションは
/// mention/reply/quote を含む(通知で届く自分宛のノート)
fn is_mention_kind(kind: &str) -> bool {
    matches!(kind, "mention" | "reply" | "quote")
}

/// カラムの REST 取得の分岐。カラム種別でエンドポイントと応答型が違う
async fn fetch_page_impl(
    client: &ApiClient,
    spec: &ColumnSpec,
    paging: &Paging,
    ntf_excludes: &[String],
) -> Result<FetchResult, String> {
    match spec.kind {
        ColumnKind::Timeline => {
            let tl_kind = spec.timeline.unwrap_or(config::TimelineKind::Home);
            client
                .timeline(tl_kind, paging, &spec.filters)
                .await
                .map(FetchResult::Page)
                .map_err(|e| e.to_string())
        }
        ColumnKind::Channel => {
            let Some(channel_id) = &spec.channel_id else {
                return Err("チャンネルが未選択です".to_owned());
            };
            client
                .channel_timeline(channel_id, paging, &spec.filters)
                .await
                .map(FetchResult::Page)
                .map_err(|e| e.to_string())
        }
        ColumnKind::Mentions => client
            .mentions(paging)
            .await
            .map(FetchResult::Mentions)
            .map_err(|e| e.to_string()),
        ColumnKind::Notifications => client
            .notifications(paging, &[], ntf_excludes)
            .await
            .map(FetchResult::Notifications)
            .map_err(|e| e.to_string()),
        ColumnKind::Main => Err("メインカラムは取得対象外です".to_owned()),
    }
}

/// 欠落補充の連続ページ上限。異常ループ防止の上限で、実用上は
/// 数分の切断で 1 ページ、長時間なら数ページで収まる
const BACKFILL_MAX_PAGES: u32 = 10;

/// 欠落補充(F-03-6): sinceId の区間が 1 ページに収まらないときは
/// untilId でさらに歩き、切断時点の境界に到達するまで取り切る
async fn fetch_backfill_impl(
    client: &ApiClient,
    spec: &ColumnSpec,
    paging: Paging,
    ntf_excludes: &[String],
) -> Result<(FetchResult, Option<String>), String> {
    let Some(since) = paging.since_id.clone() else {
        return fetch_page_impl(client, spec, &paging, ntf_excludes)
            .await
            .map(|r| (r, None));
    };
    let mut cur = Paging {
        since_id: Some(since.clone()),
        // 途中打ち切りの続き位置(backfill_until)から再開する
        until_id: paging.until_id,
        limit: paging.limit,
    };
    let mut notes: Vec<Note> = Vec::new();
    let mut notifs: Vec<Notification> = Vec::new();
    let mut merged_oldest: Option<String> = None;
    let mut merged_newest: Option<String> = None;
    // 境界(または空ページ)に到達したら true。上限で打ち切ったときだけ
    // 続き位置を返すため、最終到達を別途記録する
    let mut done = false;
    for _ in 0..BACKFILL_MAX_PAGES {
        let result = fetch_page_impl(client, spec, &cur, ntf_excludes).await?;
        // ページ末尾(最古側)の ID。次ページの untilId と境界判定に使う
        let page_oldest = match &result {
            FetchResult::Page(p) => p.oldest_id.clone(),
            FetchResult::Mentions(v) => v.last().map(|n| n.id.clone()),
            FetchResult::Notifications(v) => v.last().map(|n| n.id.clone()),
        };
        match result {
            FetchResult::Page(p) => {
                // newest は最初のページのものが区間の先端
                if merged_newest.is_none() {
                    merged_newest = p.newest_id;
                }
                merged_oldest = p.oldest_id.clone().or(merged_oldest);
                notes.extend(p.notes);
            }
            FetchResult::Mentions(v) => {
                if merged_newest.is_none() {
                    merged_newest = v.first().map(|n| n.id.clone());
                }
                if let Some(o) = v.last().map(|n| n.id.clone()) {
                    merged_oldest = Some(o);
                }
                notes.extend(v);
            }
            FetchResult::Notifications(v) => {
                if merged_newest.is_none() {
                    merged_newest = v.first().map(|n| n.id.clone());
                }
                if let Some(o) = v.last().map(|n| n.id.clone()) {
                    merged_oldest = Some(o);
                }
                notifs.extend(v);
            }
        }
        // このページの最古 ID が切断時点の境界(含む)まで達したら打ち切り。
        // 空応答(None)も打ち切り条件(区間が尽きた)
        let reached = match &page_oldest {
            None => true,
            Some(o) => o.as_str() <= since.as_str(),
        };
        if reached {
            done = true;
            break;
        }
        cur.until_id = page_oldest;
    }
    let merged = match spec.kind {
        ColumnKind::Notifications => FetchResult::Notifications(notifs),
        ColumnKind::Mentions => FetchResult::Mentions(notes),
        _ => FetchResult::Page(TimelinePage {
            notes,
            oldest_id: merged_oldest,
            newest_id: merged_newest,
        }),
    };
    // 上限ページ数で終わった場合だけ続き位置を返し、呼び側が再開する。
    // 境界に達した(または空ページで尽きた)なら None で完走とする
    Ok((merged, if done { None } else { cur.until_id }))
}

impl eframe::App for NmnlApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ev) = self.rx.try_recv() {
            self.handle_event(ev, ctx);
        }
        // 認証後の初回ブートストラップ
        if matches!(self.auth, AuthState::Authenticated { .. }) {
            self.bootstrap_streaming(ctx);
        }
        // ウィンドウサイズを設定に反映しておく(終了時に保存)
        let size = ctx.input(|i| i.screen_rect().size());
        if size.x > 0.0 && size.y > 0.0 {
            self.config.window.width = size.x;
            self.config.window.height = size.y;
        }

        // 認証済みならデッキ、未認証なら認証 UI を出す
        let authed = matches!(self.auth, AuthState::Authenticated { .. });
        if authed {
            let me = match &self.auth {
                AuthState::Authenticated { user } => Some(user),
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
