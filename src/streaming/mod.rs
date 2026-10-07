//! misskey.io とのストリーミング通信(io フォーク、本家互換)。
//! F-03-5: 全カラムで WebSocket を 1 本に集約し、チャンネル単位で購読を多重化する。
//! F-03-6: 切断時は指数バックオフで自動再接続する。復帰時の欠落補充(REST での
//! sinceId フェッチ)は呼び出し側の責務で、ここでは `Connected{is_reconnect:true}`
//! を発行して通知する。
//!
//! プロトコルは io フォークの `packages/backend/src/server/api/stream/` を
//! ソースで確認(2026-10-07): `connect`/`disconnect`/`channel`(別名 `ch`)、
//! `subNote`(別名 `s`/`sr`)/`unsubNote`(別名 `un`) を受け付ける。
//! `connect` に `pong:true` を付けると `connected` ack が返る。
//! チャンネル名: `main`、`homeTimeline`、`localTimeline`、`hybridTimeline`
//! (ソーシャル)、`globalTimeline`、`channel`(params.channelId)。main と
//! homeTimeline・hybridTimeline は requireCredential。

use std::collections::HashMap;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, sleep};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::config::TimelineKind;
use crate::model::{Note, Notification};

/// 再接続の指数バックオフ。500ms 起点で最大 30 秒(F-03-6)
const RECONNECT_BASE: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// ハンドシェイクの停滞対策(F-03-6 の相手故障検出)
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 張り直し送信の停滞対策
const RESUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
/// connected ack の待ち上限。越えたらその接続は失敗として張り直しからやり直す
/// (Connected を発行してしまうと REST 欠落補充との間に取りこぼしが生じる)
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// この秒数以上持続した接続が切れたときだけバックオフをリセットする。
/// 即切断を繰り返す相手では失敗カウントを維持して遅延を伸ばす
const STABLE_CONNECTION: Duration = Duration::from_secs(30);

/// 購読対象のチャンネル(F-03-5 で 1 本の WS に多重化する単位)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamChannel {
    /// 自分宛イベント(通知など)。requireCredential
    Main,
    /// タイムライン系。Home/Social は requireCredential、Local/Global は匿名可
    Timeline(TimelineKind),
    /// チャンネル TL。`channel` チャンネルに channelId を渡す
    Channel(String),
}

impl StreamChannel {
    /// io フォークのチャンネル名(ソース確認済み。ソーシャルは `hybridTimeline`)
    fn name(&self) -> &'static str {
        match self {
            StreamChannel::Main => "main",
            StreamChannel::Timeline(TimelineKind::Home) => "homeTimeline",
            StreamChannel::Timeline(TimelineKind::Local) => "localTimeline",
            StreamChannel::Timeline(TimelineKind::Social) => "hybridTimeline",
            StreamChannel::Timeline(TimelineKind::Global) => "globalTimeline",
            StreamChannel::Channel(_) => "channel",
        }
    }

    fn params(&self) -> serde_json::Value {
        match self {
            StreamChannel::Channel(channel_id) => {
                serde_json::json!({ "channelId": channel_id })
            }
            _ => serde_json::json!({}),
        }
    }
}

/// アプリ側へ渡すイベント。mpsc の受信側で得る
#[derive(Debug)]
pub enum StreamEvent {
    /// WS が接続され、既存の購読を張り直した。再接続(is_reconnect)のときは
    /// 切断中の欠落分を呼び出し側が REST で補う(F-03-6)
    Connected { is_reconnect: bool },
    /// 接続が切れた。retry_in 後に自動再接続を試みる
    Disconnected { attempt: u32, retry_in: Duration },
    /// 購読の ack(`pong:true` を付けた connect の応答)
    Subscribed { sub_id: String },
    /// 購読先から届いたノート(F-03-2 の差分追加)。サイズが大きいので Box
    Note {
        sub_id: String,
        channel: StreamChannel,
        note: Box<Note>,
    },
    /// 購読先から届いた通知(main チャンネル、F-04-1)
    Notification {
        sub_id: String,
        notification: Box<Notification>,
    },
}

/// アプリ→マネージャの命令
#[derive(Debug)]
pub enum StreamCmd {
    /// チャンネルを購読する。応答に購読 ID(uuid)を返す
    Subscribe {
        channel: StreamChannel,
        response: oneshot::Sender<String>,
    },
    Unsubscribe {
        sub_id: String,
    },
    Shutdown,
}

/// 管理側のハンドル。`drop` しても購読タスクは生きるので Shutdown を送ること
pub struct StreamHandle {
    cmd_tx: mpsc::UnboundedSender<StreamCmd>,
    event_rx: mpsc::UnboundedReceiver<StreamEvent>,
}

impl StreamHandle {
    /// イベント受信口。UI 層は update ループで `try_recv` する
    pub fn event_rx(&mut self) -> &mut mpsc::UnboundedReceiver<StreamEvent> {
        &mut self.event_rx
    }

    pub fn subscribe(&self, channel: StreamChannel) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(StreamCmd::Subscribe {
            channel,
            response: tx,
        });
        rx
    }

    pub fn unsubscribe(&self, sub_id: &str) {
        let _ = self.cmd_tx.send(StreamCmd::Unsubscribe {
            sub_id: sub_id.to_owned(),
        });
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(StreamCmd::Shutdown);
    }
}

pub struct StreamManager;

impl StreamManager {
    /// `wss://{host}/streaming` へ接続するマネージャを起動する
    pub fn spawn(host: &str, token: Option<&str>) -> StreamHandle {
        let mut url = format!("wss://{host}/streaming");
        // MiAuth トークンは ?i= に載せる(io/本家共通)。英数字のみの想定だが、
        // URL 構文を壊す文字が混じっても安全なようにエンコードする
        if let Some(token) = token {
            url.push_str(&format!("?i={}", urlencoding::encode(token)));
        }
        Self::spawn_with_url(url)
    }

    fn spawn_with_url(ws_url: String) -> StreamHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(ws_url, cmd_rx, event_tx));
        StreamHandle { cmd_tx, event_rx }
    }
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 切断理由の区別。Shutdown なら終了、Lost なら再接続ループへ戻る
enum DriveEnd {
    Lost,
    Shutdown,
}

async fn run(
    ws_url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<StreamCmd>,
    event_tx: mpsc::UnboundedSender<StreamEvent>,
) {
    // sub_id → チャンネル。切断中に届いた購読命令もここに反映し、
    // 再接続時にまとめて張り直す
    let mut subs: HashMap<String, StreamChannel> = HashMap::new();
    let mut attempt = 0u32;
    let mut ever_connected = false;

    loop {
        // ハンドシェイクが停滞しても命令を処理できるよう、接続中も cmd_rx を見る。
        // 期限は接続試行の開始時点で固定する(命令の処理でタイムアウトが
        // リセットされず、応答しない相手でも再試行ループへ進める)
        let outcome = {
            let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
            let mut connect = std::pin::pin!(connect_async(&ws_url));
            loop {
                tokio::select! {
                    res = connect.as_mut() => break Some(res),
                    _ = tokio::time::sleep_until(deadline) => break None,
                    cmd = cmd_rx.recv() => match cmd {
                        Some(StreamCmd::Shutdown) | None => return,
                        Some(cmd) => apply_cmd_offline(cmd, &mut subs),
                    },
                }
            }
        };

        // 接続が始まった時刻。STABLE_CONNECTION 以上持続した接続だけが
        // バックオフをリセットする(即切断を繰り返す相手では伸ばし続ける)
        let mut conn_since: Option<tokio::time::Instant> = None;
        if let Some(Ok((ws, _resp))) = outcome {
            let (mut write, mut read) = ws.split();
            // 既存購読の張り直しにもタイムアウトを設ける
            let resubscribed = matches!(
                tokio::time::timeout(RESUBSCRIBE_TIMEOUT, async {
                    for (sub_id, channel) in &subs {
                        write
                            .send(Message::Text(connect_frame(sub_id, channel).into()))
                            .await
                            .map_err(|_| ())?;
                    }
                    Ok::<(), ()>(())
                })
                .await,
                Ok(Ok(()))
            );
            if resubscribed {
                conn_since = Some(tokio::time::Instant::now());
                // Connected は購読 ack が揃ってから drive 内で発行する。
                // 張り直した購読が有効になる前に REST の欠落補充が走り、
                // その間のノートを取りこぼすのを防ぐため(F-03-6)
                let pending_acks: std::collections::HashSet<String> =
                    subs.keys().cloned().collect();
                let is_reconnect = ever_connected;
                ever_connected = true;
                match drive(
                    &mut read,
                    &mut write,
                    &mut subs,
                    &mut cmd_rx,
                    &event_tx,
                    pending_acks,
                    is_reconnect,
                )
                .await
                {
                    DriveEnd::Shutdown => return,
                    DriveEnd::Lost => {}
                }
            }
        }

        if conn_since.is_some_and(|t| t.elapsed() >= STABLE_CONNECTION) {
            attempt = 0;
        }
        let retry_in = backoff_delay(attempt);
        attempt = attempt.saturating_add(1);
        let _ = event_tx.send(StreamEvent::Disconnected { attempt, retry_in });
        // 待機中にも命令を処理する(切断中の購読変更を失わないため)。
        // 命令を受けたら即座に再接続を試みる
        tokio::select! {
            _ = sleep(retry_in) => {}
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(StreamCmd::Shutdown) | None => return,
                    Some(cmd) => apply_cmd_offline(cmd, &mut subs),
                }
            }
        }
    }
}

/// 接続中のループ。購読命令の処理と受信フレームのイベント化を行う。
/// `Connected` は張り直した購読の ack が揃う(または待ち中の購読が解除で
/// 空になる)まで保留にし、ACK_TIMEOUT で揃わなければ接続失敗として
/// 張り直しからやり直す。REST 欠落補充が購読有効化より先に走ってノートを
/// 取りこぼすのを防ぐため(F-03-6)
async fn drive(
    read: &mut futures_util::stream::SplitStream<WsStream>,
    write: &mut futures_util::stream::SplitSink<WsStream, Message>,
    subs: &mut HashMap<String, StreamChannel>,
    cmd_rx: &mut mpsc::UnboundedReceiver<StreamCmd>,
    event_tx: &mpsc::UnboundedSender<StreamEvent>,
    mut pending_acks: std::collections::HashSet<String>,
    is_reconnect: bool,
) -> DriveEnd {
    let mut connected_sent = pending_acks.is_empty();
    if connected_sent {
        let _ = event_tx.send(StreamEvent::Connected { is_reconnect });
    }
    let ack_deadline = tokio::time::Instant::now() + ACK_TIMEOUT;
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(StreamCmd::Subscribe { channel, response }) => {
                        // 送信成否に関わらず購読を保持する。送信失敗時は接続を
                        // 切り替えるが、保持した分は再接続で張り直される
                        let sub_id = uuid::Uuid::new_v4().to_string();
                        subs.insert(sub_id.clone(), channel.clone());
                        if write
                            .send(Message::Text(connect_frame(&sub_id, &channel).into()))
                            .await
                            .is_err()
                        {
                            let _ = response.send(sub_id);
                            return DriveEnd::Lost;
                        }
                        // Connected 未発行の間に送った購読も ack 待ちに加える。
                        // さもないとその購読の有効化前に Connected が出る
                        if !connected_sent {
                            pending_acks.insert(sub_id.clone());
                        }
                        let _ = response.send(sub_id);
                    }
                    Some(StreamCmd::Unsubscribe { sub_id }) => {
                        subs.remove(&sub_id);
                        pending_acks.remove(&sub_id);
                        if write
                            .send(Message::Text(disconnect_frame(&sub_id).into()))
                            .await
                            .is_err()
                        {
                            return DriveEnd::Lost;
                        }
                        // 解除で待ち中の購読が空になったら待機完了とみなす。
                        // このままだと ack 期限で健全な接続を切ってしまう
                        if pending_acks.is_empty() && !connected_sent {
                            let _ = event_tx
                                .send(StreamEvent::Connected { is_reconnect });
                            connected_sent = true;
                        }
                    }
                    Some(StreamCmd::Shutdown) | None => return DriveEnd::Shutdown,
                }
            }
            _ = tokio::time::sleep_until(ack_deadline), if !connected_sent => {
                // ack が期限までに揃わなければ購読が有効化される保証がない。
                // Connected は「欠落補充してよい」契機なので、ここで発行すると
                // 補充完了〜購読有効化の間のノートを取りこぼす。接続失敗として
                // 張り直しからやり直す(F-03-6)
                return DriveEnd::Lost;
            }
            frame = read.next() => {
                match frame {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => {
                        return DriveEnd::Lost
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Some(ev) = parse_stream_message(&text, subs) {
                            let last_ack = matches!(&ev, StreamEvent::Subscribed { sub_id }
                                if pending_acks.remove(sub_id) && pending_acks.is_empty());
                            let _ = event_tx.send(ev);
                            // 最後の ack まで揃った時点で Connected を発行する
                            // (REST 欠落補充は購読有効化の後である必要がある)
                            if last_ack && !connected_sent {
                                let _ = event_tx
                                    .send(StreamEvent::Connected { is_reconnect });
                                connected_sent = true;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        // tungstenite は split 後は Pong を自動返送しないため明示的に返す
                        if write.send(Message::Pong(payload)).await.is_err() {
                            return DriveEnd::Lost;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 切断中に受けた命令を購読状態へだけ反映する(送信は再接続時にまとめて行う)
fn apply_cmd_offline(cmd: StreamCmd, subs: &mut HashMap<String, StreamChannel>) {
    match cmd {
        StreamCmd::Subscribe { channel, response } => {
            let sub_id = uuid::Uuid::new_v4().to_string();
            subs.insert(sub_id.clone(), channel);
            let _ = response.send(sub_id);
        }
        StreamCmd::Unsubscribe { sub_id } => {
            subs.remove(&sub_id);
        }
        StreamCmd::Shutdown => {}
    }
}

/// `{"type":"connect",...}`。`pong:true` で `connected` ack を要求する
fn connect_frame(sub_id: &str, channel: &StreamChannel) -> String {
    serde_json::json!({
        "type": "connect",
        "body": {
            "channel": channel.name(),
            "id": sub_id,
            "params": channel.params(),
            "pong": true,
        }
    })
    .to_string()
}

fn disconnect_frame(sub_id: &str) -> String {
    serde_json::json!({
        "type": "disconnect",
        "body": { "id": sub_id }
    })
    .to_string()
}

/// サーバーからの 1 フレームをイベントへ変換する。
/// トップレベル `type` が `channel` なら購読先のイベント、`connected` なら
/// 購読 ack。それ以外(noteUpdated・broadcast 系)は現状の UI で未使用のため捨てる
fn parse_stream_message(text: &str, subs: &HashMap<String, StreamChannel>) -> Option<StreamEvent> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let msg_type = v.get("type")?.as_str()?;
    let body = v.get("body")?;
    match msg_type {
        "connected" => body.get("id")?.as_str().map(|id| StreamEvent::Subscribed {
            sub_id: id.to_owned(),
        }),
        "channel" => {
            let sub_id = body.get("id")?.as_str()?;
            let kind = body.get("type")?.as_str()?;
            let payload = body.get("body")?.clone();
            match kind {
                "note" => {
                    let note: Note = serde_json::from_value(payload).ok()?;
                    let channel = subs.get(sub_id)?.clone();
                    Some(StreamEvent::Note {
                        sub_id: sub_id.to_owned(),
                        channel,
                        note: Box::new(note),
                    })
                }
                "notification" => {
                    // 購読済み ID のイベントだけを通す(解除済み購読の滞在メッセージを捨てる)
                    subs.get(sub_id)?;
                    serde_json::from_value::<Notification>(payload)
                        .ok()
                        .map(|notification| StreamEvent::Notification {
                            sub_id: sub_id.to_owned(),
                            notification: Box::new(notification),
                        })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// 指数バックオフ: 500ms, 1s, 2s, ... 最大 30s(連続失敗カウントを上限まで使う)
fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(8);
    (RECONNECT_BASE * 2u32.pow(shift)).min(RECONNECT_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::accept_async;

    fn fixture_note(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "createdAt": "2026-10-07T11:00:00.000Z",
            "userId": "u1", "visibility": "public",
            "user": {"id": "u1", "username": "alice", "name": "Alice"},
            "text": "hello"
        })
    }

    // STR-01: connect/disconnect フレーム生成とチャンネル名対応(io フォーク準拠)
    #[test]
    fn str01_frame_builders() {
        let f = connect_frame("s1", &StreamChannel::Timeline(TimelineKind::Social));
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["type"], "connect");
        // io フォークではソーシャル TL のチャンネル名は hybridTimeline
        assert_eq!(v["body"]["channel"], "hybridTimeline");
        assert_eq!(v["body"]["id"], "s1");
        assert_eq!(v["body"]["pong"], true);
        let f = connect_frame("s2", &StreamChannel::Channel("ch9".to_owned()));
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["body"]["channel"], "channel");
        assert_eq!(v["body"]["params"]["channelId"], "ch9");
        let f = disconnect_frame("s1");
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["type"], "disconnect");
        assert_eq!(v["body"]["id"], "s1");
    }

    // STR-02: 受信フレームのパース(connected/channel(note|notification)/未知型)
    #[test]
    fn str02_message_parse() {
        let mut subs = HashMap::new();
        subs.insert(
            "sub1".to_owned(),
            StreamChannel::Timeline(TimelineKind::Local),
        );
        subs.insert("subMain".to_owned(), StreamChannel::Main);

        let ev = parse_stream_message("{\"type\":\"connected\",\"body\":{\"id\":\"sub1\"}}", &subs);
        assert!(matches!(ev, Some(StreamEvent::Subscribed { .. })));

        let note_frame = serde_json::json!({
            "type": "channel",
            "body": {"id": "sub1", "type": "note", "body": fixture_note("n9")}
        })
        .to_string();
        match parse_stream_message(&note_frame, &subs) {
            Some(StreamEvent::Note {
                sub_id,
                channel,
                note,
            }) => {
                assert_eq!(sub_id, "sub1");
                assert_eq!(channel, StreamChannel::Timeline(TimelineKind::Local));
                assert_eq!(note.id, "n9");
            }
            _ => panic!("note イベントのはず"),
        }

        let ntf_frame = serde_json::json!({
            "type": "channel",
            "body": {
                "id": "subMain", "type": "notification",
                "body": {"id":"ntf1","createdAt":"2026-10-07T11:00:00.000Z","type":"follow"}
            }
        })
        .to_string();
        assert!(matches!(
            parse_stream_message(&ntf_frame, &subs),
            Some(StreamEvent::Notification { .. })
        ));

        // 未知の型・未購読 ID・壊れた JSON は捨てる
        assert!(parse_stream_message("{\"type\":\"noteUpdated\",\"body\":{}}", &subs).is_none());
        assert!(parse_stream_message(
            "{\"type\":\"channel\",\"body\":{\"id\":\"unknown\",\"type\":\"note\",\"body\":{}}}",
            &subs
        )
        .is_none());
        assert!(parse_stream_message("not json", &subs).is_none());
    }

    // STR-03: バックオフ計算(指数・上限)
    #[test]
    fn str03_backoff() {
        assert_eq!(backoff_delay(0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(10), RECONNECT_MAX);
        assert_eq!(backoff_delay(100), RECONNECT_MAX);
    }

    /// connect フレームを 1 回読み取り、購読 ID を返す(サーバー側)
    async fn read_connect(
        read: &mut futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<TcpStream>>,
    ) -> String {
        while let Some(Ok(Message::Text(t))) = read.next().await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
                && v["type"] == "connect"
            {
                return v["body"]["id"].as_str().unwrap().to_owned();
            }
        }
        panic!("connect フレームを受け取れなかった")
    }

    async fn send_note(
        write: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<TcpStream>,
            Message,
        >,
        sub_id: &str,
        note_id: &str,
    ) {
        write
            .send(Message::Text(
                serde_json::json!({
                    "type": "connected", "body": {"id": sub_id}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        write
            .send(Message::Text(
                serde_json::json!({
                    "type": "channel",
                    "body": {"id": sub_id, "type": "note", "body": fixture_note(note_id)}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    }

    /// タイムアウト付きで Note イベントが来るまでイベントを消費する
    async fn wait_note(handle: &mut StreamHandle, timeout: Duration) -> Option<Note> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let ev = tokio::time::timeout_at(deadline, handle.event_rx().recv())
                .await
                .ok()??;
            if let StreamEvent::Note { note, .. } = ev {
                return Some(*note);
            }
        }
    }

    // STR-04: 購読→connected ack→note イベント受信(実 WS 経路)
    #[tokio::test]
    async fn str04_subscribe_receives_note() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = accept_async(stream).await else {
                return;
            };
            let (mut write, mut read) = ws.split();
            let sub_id = read_connect(&mut read).await;
            send_note(&mut write, &sub_id, "n1").await;
            // 接続を保持してクライアントの切断を待つ
            while read.next().await.is_some() {}
        });
        let mut handle = StreamManager::spawn_with_url(format!("ws://{addr}/streaming"));
        let sub_id = handle
            .subscribe(StreamChannel::Timeline(TimelineKind::Local))
            .await
            .unwrap();
        assert!(!sub_id.is_empty());
        let note = wait_note(&mut handle, Duration::from_secs(5)).await;
        assert_eq!(note.unwrap().id, "n1");
        handle.shutdown();
    }

    // STR-05: 切断→指数バックオフ再接続→購読の張り直し(F-03-6)
    #[tokio::test]
    async fn str05_reconnect_resubscribes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let conns2 = conns.clone();
        tokio::spawn(async move {
            // 接続を逐次処理する(クライアントが前の接続を失ってから再接続する前提)
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(ws) = accept_async(stream).await else {
                    continue;
                };
                let n = conns2.fetch_add(1, Ordering::SeqCst) + 1;
                let (mut write, mut read) = ws.split();
                let sub_id = read_connect(&mut read).await;
                if n == 1 {
                    // 最初の接続: 購読を受けたら黙って切断(サーバー側クラッシュ相当)
                    drop(write);
                    drop(read);
                    continue;
                }
                // 再接続: 張り直された購読に note を返す
                send_note(&mut write, &sub_id, "n2").await;
                while read.next().await.is_some() {}
            }
        });
        let mut handle = StreamManager::spawn_with_url(format!("ws://{addr}/streaming"));
        handle
            .subscribe(StreamChannel::Timeline(TimelineKind::Local))
            .await
            .unwrap();

        // 期待順: Connected(false) → Disconnected → Connected(is_reconnect) → Note
        let mut saw_disconnect = false;
        let mut saw_reconnect = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, handle.event_rx().recv())
                .await
                .expect("タイムアウト")
                .expect("イベントチャンネルが閉じた");
            match ev {
                StreamEvent::Disconnected { .. } => saw_disconnect = true,
                StreamEvent::Connected { is_reconnect: true } => saw_reconnect = true,
                StreamEvent::Note { note, .. } => {
                    assert_eq!(note.id, "n2");
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_disconnect && saw_reconnect);
        assert!(conns.load(Ordering::SeqCst) >= 2);
        handle.shutdown();
    }

    // STR-06: 再接続時の Connected は張り直し購読の ack 揃い待ち(F-03-6)
    #[tokio::test]
    async fn str06_reconnect_connected_waits_for_acks() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let conns2 = conns.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(ws) = accept_async(stream).await else {
                    continue;
                };
                let n = conns2.fetch_add(1, Ordering::SeqCst) + 1;
                let (mut write, mut read) = ws.split();
                if n == 1 {
                    // 最初の接続: 2 件の購読を受け取ったら無応答で切断
                    let _a = read_connect(&mut read).await;
                    let _b = read_connect(&mut read).await;
                    drop(write);
                    drop(read);
                    continue;
                }
                // 再接続: 1 件目の ack → ack 未着の購読先の note → 2 件目の ack
                let a = read_connect(&mut read).await;
                let b = read_connect(&mut read).await;
                write
                    .send(Message::Text(
                        serde_json::json!({"type":"connected","body":{"id":a}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                // ack 未着の購読先から届くノートは待機中でも流す(取りこぼしではない)
                write
                    .send(Message::Text(
                        serde_json::json!({
                            "type":"channel",
                            "body":{"id":b,"type":"note","body":fixture_note("n-early")}
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
                write
                    .send(Message::Text(
                        serde_json::json!({"type":"connected","body":{"id":b}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                while read.next().await.is_some() {}
            }
        });
        let mut handle = StreamManager::spawn_with_url(format!("ws://{addr}/streaming"));
        handle
            .subscribe(StreamChannel::Timeline(TimelineKind::Local))
            .await
            .unwrap();
        handle
            .subscribe(StreamChannel::Timeline(TimelineKind::Global))
            .await
            .unwrap();

        let mut subscribed = std::collections::HashSet::new();
        let mut saw_note = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, handle.event_rx().recv())
                .await
                .expect("タイムアウト")
                .expect("イベントチャンネルが閉じた");
            match ev {
                StreamEvent::Subscribed { sub_id } => {
                    subscribed.insert(sub_id);
                }
                StreamEvent::Note { note, .. } => {
                    assert_eq!(note.id, "n-early");
                    saw_note = true;
                }
                StreamEvent::Connected { is_reconnect: true } => {
                    // Connected は両購読の ack が揃った後でなければならない
                    assert_eq!(subscribed.len(), 2);
                    assert!(saw_note);
                    break;
                }
                _ => {}
            }
        }
        handle.shutdown();
    }

    // STR-07: ack が ACK_TIMEOUT 内に揃わない接続は失敗として再接続する(F-03-6)
    #[tokio::test]
    async fn str07_ack_timeout_reconnects() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let conns2 = conns.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(ws) = accept_async(stream).await else {
                    continue;
                };
                let n = conns2.fetch_add(1, Ordering::SeqCst) + 1;
                let (write, mut read) = ws.split();
                // 購読は受け取るが ack は一切返さない相手
                let _ = read_connect(&mut read).await;
                if n == 1 {
                    // 最初の接続は切断して張り直し(ack 待ち)を起こす
                    drop(write);
                    drop(read);
                    continue;
                }
                // 2 接続目以降: 接続は保ったまま無応答 → クライアントは
                // ACK_TIMEOUT でこの接続を失敗とみなして張り直すはず
                while read.next().await.is_some() {}
            }
        });
        let mut handle = StreamManager::spawn_with_url(format!("ws://{addr}/streaming"));
        handle
            .subscribe(StreamChannel::Timeline(TimelineKind::Local))
            .await
            .unwrap();

        // 張り直した購読の ack が返らない相手では、クライアントは ACK_TIMEOUT
        // (5s)で接続を失敗とみなして張り直しを繰り返す。その間 Connected
        // (is_reconnect) は一度も発行されないはず(発行すると REST 欠落補充
        // との取りこぼしになる)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut disconnects = 0;
        while disconnects < 2 {
            let ev = tokio::time::timeout_at(deadline, handle.event_rx().recv())
                .await
                .expect("タイムアウト")
                .expect("イベントチャンネルが閉じた");
            match ev {
                StreamEvent::Disconnected { .. } => disconnects += 1,
                StreamEvent::Connected { is_reconnect: true } => {
                    panic!("ack 未着なのに Connected(is_reconnect) が発行された")
                }
                _ => {}
            }
        }
        assert!(conns.load(Ordering::SeqCst) >= 2);
        handle.shutdown();
    }
}
