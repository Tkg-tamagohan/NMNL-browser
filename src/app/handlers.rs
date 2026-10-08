use super::{
    AppEvent, AuthState, ChannelListTarget, EmojiFetch, FetchKind, FetchResult, NmnlApp,
    ProfileState, ReactionPickerState,
};
use crate::config::{self, ColumnKind};
use crate::deck::ColumnView;
use crate::emoji;
use crate::model::Note;
use crate::streaming::StreamEvent;
use crate::ui::{self, UiOp};
use eframe::egui;

impl NmnlApp {
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
    pub(super) fn apply_op(&mut self, op: UiOp, ctx: &egui::Context) {
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
                    self.spawn_followed_channels(ChannelListTarget::Column(id), ctx);
                }
                // 開いた時点で初期一覧を出す(io は query:"" でチャンネル一覧が返る。
                // フォロー中が空でも検索なしで選べるようにする)
                self.spawn_channel_search(ChannelListTarget::Column(id), String::new(), ctx);
            }
            UiOp::ChannelPickerQuery(id, q) => {
                if let Some(col) = self.deck.columns.iter_mut().find(|c| c.id == id) {
                    let picker = col.channel_picker.get_or_insert_with(Default::default);
                    picker.query = q.clone();
                    picker.loading = true;
                    // 空クエリは io で全件一覧が返る。クリア時に前回の結果を
                    // 残さないよう空でも検索を再発行して表示を揃える
                    picker.results.clear();
                }
                self.spawn_channel_search(ChannelListTarget::Column(id), q, ctx);
            }
            // Phase 11: フォーム側のチャンネル選択(F-06-4)
            UiOp::ComposerChannelPickerOpen => {
                let need = {
                    let picker = &mut self.composer_channel_picker;
                    if !picker.followed_loaded {
                        picker.loading = true;
                        picker.followed_loaded = true;
                        true
                    } else {
                        false
                    }
                };
                if need {
                    self.spawn_followed_channels(ChannelListTarget::Composer, ctx);
                }
                self.spawn_channel_search(ChannelListTarget::Composer, String::new(), ctx);
            }
            UiOp::ComposerChannelPickerQuery(q) => {
                {
                    let picker = &mut self.composer_channel_picker;
                    picker.query = q.clone();
                    picker.loading = true;
                    picker.results.clear();
                }
                self.spawn_channel_search(ChannelListTarget::Composer, q, ctx);
            }
            UiOp::ComposerSetChannel(sel) => match sel {
                Some((id, name)) => self.composer.set_channel(Some(id), Some(name)),
                None => self.composer.set_channel(None, None),
            },
            // channel カラムの「このチャンネルに投稿」導線(仕様決定 V)
            UiOp::PostToChannel { channel_id, name } => {
                self.composer.set_channel(Some(channel_id), name);
            }
            UiOp::OpenUrl(u) => {
                let _ = open::that(&u);
            }
            // Phase 7: 投稿と操作
            UiOp::ReplyTo { id, label, channel } => {
                // 返信と引用は相互排他(本文中に両方参照は作れない)
                self.composer.quote_of = None;
                self.composer.reply_to = Some(crate::composer::PostTarget { id, label });
                // 対象がチャンネル所属なら投稿も同じチャンネルへ(仕様決定 W)
                self.composer.inherit_channel(channel.as_ref());
            }
            UiOp::Quote { id, label, channel } => {
                self.composer.reply_to = None;
                self.composer.quote_of = Some(crate::composer::PostTarget { id, label });
                self.composer.inherit_channel(channel.as_ref());
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
                        // ファイルは clone で渡してフォームに残す。投稿失敗時に
                        // 添付が消えて再投稿で抜け落ちるのを防ぐ
                        // (Arc<Vec<u8>> なので clone は浅い)
                        let files = self.composer.files.clone();
                        self.spawn_post(req, files, true, ctx);
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
            UiOp::Renote { note_id, channel } => {
                // リノートはフォームを通さず即時投稿(仕様決定 E)。
                // 対象がチャンネル所属なら channelId を継承する(仕様決定 W)
                let req = crate::composer::renote_request(&note_id, channel.as_ref());
                self.spawn_post(req, Vec::new(), false, ctx);
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
                // F-07-5: create に送れる形式(:name:/:name@.:/Unicode)に正規化
                if let Some(v) = crate::model::reaction_send_value(&reaction) {
                    let affect = v.clone();
                    self.spawn_reaction(note_id, v, affect, true, ctx);
                }
            }
            UiOp::ToggleReaction {
                note_id,
                reaction,
                send,
                mine,
            } => {
                // 自分のリアクションは取り消し、それ以外は同じ絵文字で付与。
                // send は UI 側で F-07-5 形式へ正規化済み(相乗りは :name@.:)
                if mine {
                    self.spawn_reaction(note_id, String::new(), reaction, false, ctx);
                } else if let Some(v) = send {
                    self.spawn_reaction(note_id, v, reaction, true, ctx);
                }
            }
            UiOp::OpenChannel { channel_id } => {
                // 仕様決定 Q: 既存の channel カラムがあれば対象を差し替える
                let (id, need_fetch) = self.deck.open_channel_column(&channel_id);
                self.sync_subscriptions(ctx);
                if need_fetch {
                    self.spawn_fetch(id, FetchKind::Initial, ctx);
                }
            }
            // Phase 8: ビューア・プロフィール・設定
            UiOp::OpenViewer {
                files,
                index,
                revealed,
            } => {
                self.viewer = Some(ui::ViewerState {
                    files,
                    index,
                    revealed,
                });
            }
            UiOp::ViewerReveal(file_id) => {
                if let Some(v) = &mut self.viewer {
                    v.revealed.insert(file_id);
                }
            }
            UiOp::CloseViewer => {
                self.viewer = None;
            }
            UiOp::ViewerStep(d) => {
                if let Some(v) = &mut self.viewer {
                    v.step(d);
                }
            }
            UiOp::OpenProfile { user_id } => {
                self.profile = Some(ProfileState {
                    user_id: user_id.clone(),
                    loading: true,
                    ..Default::default()
                });
                self.spawn_profile(&user_id, ctx);
            }
            UiOp::CloseProfile => {
                self.profile = None;
            }
            UiOp::OpenSettings => {
                self.settings_win = true;
                self.refresh_cache_sizes();
            }
            UiOp::CloseSettings => {
                self.settings_win = false;
            }
            UiOp::SetUiScale(v) => {
                // F-09-5: UI 全体の拡縮を zoom_factor へ反映し設定へ保存
                self.config.ui_scale = crate::config::normalize_ui_scale(v);
                ctx.set_zoom_factor(self.config.ui_scale);
                if let Err(e) = self.config.save() {
                    eprintln!("設定の保存に失敗しました: {e}");
                }
            }
            UiOp::ClearCache(images) => {
                if images {
                    if let Some(l) = &self.image_loader {
                        l.clear_all();
                    }
                    self.notice = Some("画像キャッシュを消去しました".to_owned());
                } else {
                    emoji::clear_emoji_list();
                    self.emoji_list.clear();
                    self.notice = Some("絵文字一覧キャッシュを消去しました".to_owned());
                }
                self.refresh_cache_sizes();
            }
        }
    }

    /// キャッシュサイズの再計測(設定画面を開く/消去したときだけ走る)
    fn refresh_cache_sizes(&mut self) {
        let img = self
            .image_loader
            .as_ref()
            .map(|l| l.cache_bytes())
            .unwrap_or(0);
        self.cache_sizes = (img, emoji::emoji_list_bytes());
    }

    pub(super) fn handle_event(&mut self, ev: AppEvent, ctx: &egui::Context) {
        match ev {
            AppEvent::VerifyResult {
                result,
                came_from_keyring,
            } => match result {
                Ok(user) => {
                    self.auth = AuthState::Authenticated {
                        user: Box::new(user),
                    };
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
                        self.auth = AuthState::Authenticated {
                            user: Box::new(user),
                        };
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
            AppEvent::FollowedResult { target, result } => {
                let Some(picker) = self.channel_picker_of(target) else {
                    return;
                };
                picker.loading = false;
                match result {
                    Ok(list) => {
                        picker.followed = list;
                        picker.error = None;
                    }
                    Err(e) => {
                        // 失敗しても loaded を true のままにすると再試行しないので戻す
                        picker.followed_loaded = false;
                        picker.error = Some(e);
                    }
                }
            }
            AppEvent::ChannelSearchResult {
                target,
                query,
                result,
            } => {
                let Some(picker) = self.channel_picker_of(target) else {
                    return;
                };
                // 古いクエリの応答が遅れて届いた場合は現在の検索結果を上書きしない
                if picker.query != query {
                    return;
                }
                picker.loading = false;
                match result {
                    Ok(list) => {
                        picker.results = list;
                        picker.error = None;
                    }
                    Err(e) => picker.error = Some(e),
                }
            }
            AppEvent::EmojiResult { name, outcome } => match outcome {
                EmojiFetch::Found(url) => self.emoji_cache.complete(&name, Some(url)),
                EmojiFetch::Missing => self.emoji_cache.complete(&name, None),
                EmojiFetch::Transient => {
                    let cooldown = self.emoji_cache.fail_transient(&name);
                    // クールダウン満了時に resolve が再要求できるよう、
                    // 期限時刻の再描画を予約する(放置だと再試行が走らない)
                    ctx.request_repaint_after(cooldown);
                }
            },
            AppEvent::PostResult {
                result,
                files,
                from_composer,
            } => {
                if !from_composer {
                    // リノート等のフォーム外投稿: フォームの下書きは
                    // そのままに、通知だけ更新する
                    self.notice = Some(match result {
                        Ok(note) => format!("投稿しました({})", note.id),
                        Err(e) => format!("投稿に失敗: {e}"),
                    });
                    return;
                }
                self.composer.posting = false;
                match result {
                    Ok(note) => {
                        // 投稿成功: フォームをリセット。タイムラインへの
                        // 反映はストリーミング/補充に委ねる
                        self.composer.clear();
                        self.notice = Some(format!("投稿しました({})", note.id));
                    }
                    Err(e) => {
                        // アップロード済み ID 入りの添付をフォームへ戻す。
                        // リトライで同じファイルを再アップロードしないため
                        self.composer.files = files;
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
            AppEvent::ProfileResult {
                user_id,
                user,
                notes,
            } => {
                // 別ユーザーを開き直した後に届いた遅れ結果は捨てる(PRF-03)
                if let Some(p) = &mut self.profile
                    && p.accepts(&user_id)
                {
                    p.loading = false;
                    match user {
                        Ok(u) => p.user = Some(*u),
                        Err(e) => p.error = Some(format!("プロフィール取得失敗: {e}")),
                    }
                    match notes {
                        Ok(ns) => p.notes = ns,
                        Err(e) => {
                            if p.error.is_none() {
                                p.error = Some(format!("ノート取得失敗: {e}"));
                            }
                        }
                    }
                }
            }
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
