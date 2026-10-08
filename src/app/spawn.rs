use super::{AppEvent, AuthState, ChannelListTarget, EmojiFetch, FetchKind, FetchResult, NmnlApp};
use crate::api::{self, ApiClient, MiauthStatus, Paging, TimelinePage};
use crate::composer::PendingFile;
use crate::config::{self, ColumnKind, ColumnSpec};
use crate::model::{CreateNote, Note, Notification};
use crate::streaming::{self, StreamChannel};
use eframe::egui;
use std::time::Duration;

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
pub(super) const EMOJI_BATCH: usize = 12;

impl NmnlApp {
    pub(super) fn spawn_verify(&self, token: String, came_from_keyring: bool, ctx: &egui::Context) {
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

    pub(super) fn start_miauth(&mut self, ctx: &egui::Context) {
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
    pub(super) fn bootstrap_streaming(&mut self, ctx: &egui::Context) {
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
    /// アップロードして fileIds に乗せてから notes/create を呼ぶ。
    /// `from_composer` が true のときだけフォームを送信中にし、
    /// 結果で添付を復元する(リノート等のフォーム外投稿は触らない)
    pub(super) fn spawn_post(
        &mut self,
        mut req: CreateNote,
        files: Vec<PendingFile>,
        from_composer: bool,
        ctx: &egui::Context,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if from_composer {
            self.composer.posting = true;
        }
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let mut files = files;
            let result = match client.upload_pending_files(&mut files).await {
                Err(e) => Err(format!("添付のアップロード失敗: {e}")),
                Ok(ids) => {
                    req.file_ids = ids;
                    client
                        .create_note(&req)
                        .await
                        .map(Box::new)
                        .map_err(|e| e.to_string())
                }
            };
            let _ = tx.send(AppEvent::PostResult {
                result,
                files,
                from_composer,
            });
            ctx2.request_repaint();
        });
    }

    /// リアクション付与/取消(F-07)。
    /// `send` は付与時に reactions/create へ送る値(F-07-5 の正規化済み形式)、
    /// `affect` は結果イベントでローカルカウントを動かすキー。
    /// 相乗り(リモート絵文字→ローカル :name@.:)では両者が異なる
    pub(super) fn spawn_reaction(
        &mut self,
        note_id: String,
        send: String,
        affect: String,
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
                client.create_reaction(&note_id, &send).await
            } else {
                client.delete_reaction(&note_id).await
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ReactionResult {
                note_id,
                reaction: affect,
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
    pub(super) fn sync_subscriptions(&mut self, ctx: &egui::Context) {
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
    pub(super) fn spawn_fetch(&mut self, col_id: u64, kind: FetchKind, ctx: &egui::Context) {
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
    pub(super) fn spawn_conversation(&self, col_id: u64, note_id: String, ctx: &egui::Context) {
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

    /// フォロー中チャンネル一覧(F-03-7/F-06-4)
    pub(super) fn spawn_followed_channels(&self, target: ChannelListTarget, ctx: &egui::Context) {
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
            let _ = tx.send(AppEvent::FollowedResult { target, result });
            ctx2.request_repaint();
        });
    }

    /// チャンネル検索(F-03-7/F-06-4)
    pub(super) fn spawn_channel_search(
        &self,
        target: ChannelListTarget,
        query: String,
        ctx: &egui::Context,
    ) {
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
                target,
                query,
                result,
            });
            ctx2.request_repaint();
        });
    }

    /// ChannelListTarget に対応する ChannelPicker 状態への参照
    pub(super) fn channel_picker_of(
        &mut self,
        target: ChannelListTarget,
    ) -> Option<&mut crate::deck::ChannelPicker> {
        match target {
            ChannelListTarget::Column(id) => self
                .deck
                .columns
                .iter_mut()
                .find(|c| c.id == id)
                .and_then(|c| c.channel_picker.as_mut()),
            ChannelListTarget::Composer => Some(&mut self.composer_channel_picker),
        }
    }

    /// 絵文字のオンデマンド取得(F-05-2)
    pub(super) fn spawn_emoji(&self, name: String, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        self.runtime.spawn(async move {
            let outcome = match client.emoji(&name).await {
                Ok(e) => EmojiFetch::Found(e.url),
                // 不在を明示するコードだけ否定キャッシュする。io 実測では
                // 存在しない名も INTERNAL_ERROR で返るが、サーバー障害と
                // 区別できないので INTERNAL_ERROR は再試行対象とする
                Err(api::ApiError::Server { code, .. }) if code == "NO_SUCH_EMOJI" => {
                    EmojiFetch::Missing
                }
                Err(_) => EmojiFetch::Transient,
            };
            let _ = tx.send(AppEvent::EmojiResult { name, outcome });
            ctx2.request_repaint();
        });
    }

    /// プロフィール取得(F-05-6)。users/show と users/notes をまとめて取る
    pub(super) fn spawn_profile(&self, user_id: &str, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        let user_id = user_id.to_owned();
        self.runtime.spawn(async move {
            let user = client
                .show_user(&api::UserQuery::ById(user_id.clone()))
                .await
                .map(Box::new)
                .map_err(|e| e.to_string());
            let notes = client
                .user_notes(
                    &user_id,
                    &Paging {
                        limit: 20,
                        until_id: None,
                        since_id: None,
                    },
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ProfileResult {
                user_id,
                user,
                notes,
            });
            ctx2.request_repaint();
        });
    }
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
