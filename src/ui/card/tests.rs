use super::media::*;
use super::reactions::*;
use super::*;
use crate::model::DriveFile;

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
