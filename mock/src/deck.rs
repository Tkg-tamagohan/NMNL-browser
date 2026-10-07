//! デッキのデータモデルと操作ロジック(モック用ダミーデータ込み)。
//! UI から切り離して単体テストできる形にする(F-02 の操作仕様の検証用)。

/// カラム幅の許容範囲(px)。F-02-1 の幅変更でクランプする
pub const COL_WIDTH_MIN: f32 = 180.0;
pub const COL_WIDTH_MAX: f32 = 520.0;
const COL_WIDTH_DEFAULT: f32 = 300.0;

/// カラム種別(仕様決定 C: メイン/TL/通知/メンション/チャンネル)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    Main,
    HomeTimeline,
    LocalTimeline,
    SocialTimeline,
    GlobalTimeline,
    Notifications,
    Mentions,
    Channel,
}

impl ColKind {
    /// 追加メニューなどで選べる全種別(並びは UI の選択肢順)
    pub const ALL: [ColKind; 8] = [
        ColKind::Main,
        ColKind::HomeTimeline,
        ColKind::LocalTimeline,
        ColKind::SocialTimeline,
        ColKind::GlobalTimeline,
        ColKind::Notifications,
        ColKind::Mentions,
        ColKind::Channel,
    ];

    /// カラムヘッダーと追加メニューに出す日本語名(仕様決定 J)
    pub fn label(self) -> &'static str {
        match self {
            ColKind::Main => "メイン",
            ColKind::HomeTimeline => "ホーム",
            ColKind::LocalTimeline => "ローカル",
            ColKind::SocialTimeline => "ソーシャル",
            ColKind::GlobalTimeline => "グローバル",
            ColKind::Notifications => "通知",
            ColKind::Mentions => "メンション",
            ColKind::Channel => "チャンネル",
        }
    }
}

/// ノートに付くメディアの見せ方(F-05-3: センシティブはぼかし+クリック展開)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    None,
    Normal(u32),
    Sensitive(u32),
}

/// 1 段だけ入れ子で表示するノート参照(F-05-4)
#[derive(Debug, Clone)]
pub struct NestedNote {
    pub user_name: String,
    pub user_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Renote,
    Quote,
    Reply,
}

/// モックのノート。実データ構造は src/model を正とし、ここでは
/// 見た目検証に必要な項目だけを持つ
#[derive(Debug, Clone)]
pub struct DummyNote {
    pub id: u64,
    pub user_name: String,
    pub user_id: String,
    pub host: Option<String>,
    pub time: String,
    /// CW 折りたたみの表示文(あれば本文は折りたたみ)
    pub cw: Option<String>,
    /// MFM サブセットを含む本文
    pub text: String,
    pub media: MediaKind,
    /// リノート/引用/返信の 1 段入れ子
    pub reference: Option<(RefKind, NestedNote)>,
    pub reactions: Vec<(String, u32)>,
}

pub struct Column {
    pub id: u64,
    pub kind: ColKind,
    pub width: f32,
    pub paused: bool,
    pub notes: Vec<DummyNote>,
}

pub struct ColumnDeck {
    pub columns: Vec<Column>,
    next_id: u64,
}

impl ColumnDeck {
    pub fn new() -> Self {
        let mut deck = ColumnDeck {
            columns: Vec::new(),
            next_id: 1,
        };
        // 既定構成(F-02-3 の初期状態としてメイン相当の TL 系を並べる)
        deck.add(ColKind::HomeTimeline);
        deck.add(ColKind::LocalTimeline);
        deck.add(ColKind::Notifications);
        deck
    }

    /// F-02-1: 追加。新しいカラムは末尾に既定幅で並ぶ
    pub fn add(&mut self, kind: ColKind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.columns.push(Column {
            id,
            kind,
            width: COL_WIDTH_DEFAULT,
            paused: false,
            notes: dummy_notes(id),
        });
        id
    }

    /// F-02-1: 削除
    pub fn remove(&mut self, id: u64) {
        self.columns.retain(|c| c.id != id);
    }

    /// F-02-1: ドラッグによる並べ替え。`id` のカラムを区切り位置 `to` へ移す。
    /// `to` は除去前の配列の区切り番号なので、移動元が `to` より左なら
    /// 除去ぶんだけ挿入位置が 1 つ前にずれる
    pub fn move_to(&mut self, id: u64, to: usize) {
        if let Some(from) = self.columns.iter().position(|c| c.id == id) {
            let col = self.columns.remove(from);
            let to = if from < to { to.saturating_sub(1) } else { to };
            let to = to.min(self.columns.len());
            self.columns.insert(to, col);
        }
    }

    /// F-02-1: 幅の変更(許容範囲でクランプ)
    pub fn set_width(&mut self, id: u64, width: f32) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id) {
            col.width = width.clamp(COL_WIDTH_MIN, COL_WIDTH_MAX);
        }
    }

    /// F-02-2: カラムごとの更新一時停止
    pub fn set_paused(&mut self, id: u64, paused: bool) {
        if let Some(col) = self.columns.iter_mut().find(|c| c.id == id) {
            col.paused = paused;
        }
    }
}

/// 表示確認用のダミーノート列。MFM 各要素、CW、センシティブ、
/// 入れ子 1 段、絵文字を散りばめて見た目を網羅する
fn dummy_notes(seed: u64) -> Vec<DummyNote> {
    let base = [
        ("Misano :misskey:", "@misano", Some("misskey.io"), "13:02"),
        ("あおい", "@aoi_dev", Some("misskey.io"), "13:05"),
        (
            "リモートの人",
            "@remote_user",
            Some("example.social"),
            "13:11",
        ),
        ("猫のロボ", "@neko_bot", None, "13:20"),
    ];
    let mk = |i: usize, text: &str| DummyNote {
        id: seed * 100 + i as u64,
        user_name: base[i % 4].0.to_owned(),
        user_id: base[i % 4].1.to_owned(),
        host: base[i % 4].2.map(str::to_owned),
        time: base[i % 4].3.to_owned(),
        cw: None,
        text: text.to_owned(),
        media: MediaKind::None,
        reference: None,
        reactions: vec![],
    };

    let mut notes = vec![
        mk(
            0,
            "**NMNL-browser** のモックです。`egui` でデッキ UI を試作しています #開発メモ",
        ),
        mk(
            1,
            "MFM の基本装飾: *斜体* と ~~取り消し線~~ と `inline_code` です。リンク https://misskey.io も貼ります",
        ),
        mk(
            2,
            "@tkgtamagohan このノートからは引用と返信の見た目を確認できます:misskey:",
        ),
        mk(
            3,
            "未対応の $[spin 関数記法] や <i>HTML</i> は記法だけ除去されてプレーン表示になります",
        ),
    ];
    // CW 折りたたみ
    let mut cw_note = mk(
        0,
        "ネタバレ注意の本文。クリックで展開された内容がここに出ます。**太字** も効きます",
    );
    cw_note.cw = Some("今日の考察ネタバレ".to_owned());
    notes.push(cw_note);
    // センシティブ画像 + 通常画像
    let mut media = mk(1, "画像つきノート(センシティブはぼかし表示)");
    media.media = MediaKind::Sensitive(2);
    notes.push(media);
    let mut media2 = mk(2, "こちらは通常の画像つきノートです");
    media2.media = MediaKind::Normal(3);
    notes.push(media2);
    // リノート(純粋リノート: 本文なしで元ノートを入れ子)
    let mut renote = mk(3, "");
    renote.reference = Some((
        RefKind::Renote,
        NestedNote {
            user_name: "Misano :misskey:".to_owned(),
            user_id: "@misano".to_owned(),
            text: "**リノートされる元ノート** です。入れ子は 1 段まで(F-05-4)".to_owned(),
        },
    ));
    notes.push(renote);
    // 引用
    let mut quote = mk(0, "この意見には同意。 *引用* 元も一緒に見せます");
    quote.reference = Some((
        RefKind::Quote,
        NestedNote {
            user_name: "あおい".to_owned(),
            user_id: "@aoi_dev".to_owned(),
            text: "引用される側のノート本文。長めのテキストが入ってもカラム幅内で折り返します"
                .to_owned(),
        },
    ));
    notes.push(quote);
    // 返信
    let mut reply = mk(
        1,
        "返信です。会話ビューへの遷移はノート選択で行います(F-05-5)",
    );
    reply.reference = Some((
        RefKind::Reply,
        NestedNote {
            user_name: "リモートの人".to_owned(),
            user_id: "@remote_user".to_owned(),
            text: "返信先のノート(入れ子 1 段)".to_owned(),
        },
    ));
    notes.push(reply);
    // リアクションつき
    let mut reacted = mk(2, "リアクションの見た目: 名前と件数をバッジにします");
    reacted.reactions = vec![
        ("👍".to_owned(), 12),
        ("❤".to_owned(), 3),
        (":misskey:".to_owned(), 5),
    ];
    notes.push(reacted);
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    // COL-01: カラムの追加と削除(F-02-1)
    #[test]
    fn col01_add_remove() {
        let mut deck = ColumnDeck::new();
        let initial = deck.columns.len();
        let id = deck.add(ColKind::GlobalTimeline);
        assert_eq!(deck.columns.len(), initial + 1);
        assert_eq!(deck.columns.last().unwrap().kind, ColKind::GlobalTimeline);
        deck.remove(id);
        assert_eq!(deck.columns.len(), initial);
        assert!(deck.columns.iter().all(|c| c.id != id));
    }

    // COL-02: ドラッグによる並べ替え(F-02-1)
    #[test]
    fn col02_move() {
        let mut deck = ColumnDeck::new();
        let first = deck.columns[0].id;
        let last = deck.columns[deck.columns.len() - 1].id;
        // 先頭を末尾へ
        deck.move_to(first, deck.columns.len());
        assert_eq!(deck.columns.last().unwrap().id, first);
        // 末尾を先頭へ
        deck.move_to(last, 0);
        assert_eq!(deck.columns[0].id, last);
    }

    // COL-03: 幅の変更とクランプ(F-02-1)
    #[test]
    fn col03_width_clamp() {
        let mut deck = ColumnDeck::new();
        let id = deck.columns[0].id;
        deck.set_width(id, 400.0);
        assert_eq!(deck.columns[0].width, 400.0);
        deck.set_width(id, 10.0);
        assert_eq!(deck.columns[0].width, COL_WIDTH_MIN);
        deck.set_width(id, 9999.0);
        assert_eq!(deck.columns[0].width, COL_WIDTH_MAX);
    }

    // COL-04: カラムごとの更新一時停止(F-02-2)
    #[test]
    fn col04_pause() {
        let mut deck = ColumnDeck::new();
        let id = deck.columns[0].id;
        deck.set_paused(id, true);
        assert!(deck.columns[0].paused);
        deck.set_paused(id, false);
        assert!(!deck.columns[0].paused);
    }

    // COL-05: 並べ替えの区切り位置補正(F-02-1、Devin Review 指摘の回帰)
    #[test]
    fn col05_move_boundary() {
        let mut deck = ColumnDeck::new();
        let ids: Vec<u64> = deck.columns.iter().map(|c| c.id).collect();
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        // [A,B,C] で A を区切り 2(B|C 間)へ → [B,A,C]
        deck.move_to(a, 2);
        assert_eq!(
            deck.columns.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![b, a, c]
        );
        // [B,A,C] で C を区切り 1(B|A 間)へ → [B,C,A]
        deck.move_to(c, 1);
        assert_eq!(
            deck.columns.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![b, c, a]
        );
        // 同一区切りへの移動は変化なし(A の左区切り 2 へ A を移動)
        deck.move_to(a, 2);
        assert_eq!(
            deck.columns.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![b, c, a]
        );
    }
}
