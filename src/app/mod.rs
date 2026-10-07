//! アプリ状態とイベント集約層。
//! UI(egui)は通信層を直接呼ばず、tokio タスクが結果を `AppEvent` として
//! チャネル経由で返し、`update` で状態遷移させる。

use crate::api::{self, ApiClient, MiauthStatus};
use crate::config::{self, AppConfig};
use crate::model::User;
use eframe::egui;
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

#[derive(Debug)]
enum AppEvent {
    VerifyResult {
        result: Result<User, api::ApiError>,
        came_from_keyring: bool,
    },
    MiauthResult {
        result: Result<(String, User), String>,
    },
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
}

impl NmnlApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let config = AppConfig::load();
        let runtime = tokio::runtime::Runtime::new().expect("tokio ランタイムの起動に失敗");

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

    fn handle_event(&mut self, ev: AppEvent) {
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
        }
    }
}

impl eframe::App for NmnlApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ev) = self.rx.try_recv() {
            self.handle_event(ev);
        }
        // ウィンドウサイズを設定に反映しておく(終了時に保存)
        let size = ctx.input(|i| i.screen_rect().size());
        if size.x > 0.0 && size.y > 0.0 {
            self.config.window.width = size.x;
            self.config.window.height = size.y;
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
                AuthState::Authenticated { user } => {
                    let display = user.name.as_deref().unwrap_or(&user.username);
                    ui.label(format!("{display}(@{})としてログイン中", user.username));
                    if self.token_from_env {
                        ui.label("開発用トークン(環境変数)を使用中です。");
                    } else if !self.token_persisted {
                        ui.label(
                            "この環境ではキーリングを利用できないため、次回起動時に再認証が必要です。",
                        );
                    }
                }
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
        if let Err(e) = self.config.save() {
            eprintln!("設定の保存に失敗しました: {e}");
        }
    }
}
