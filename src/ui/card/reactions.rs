use super::{UiCtx, UiOp};
use crate::model::{Note, ReactionKey, parse_reaction_key, reaction_send_value};
use eframe::egui::{self, Color32, RichText, Stroke, Ui, vec2};

/// リアクションバッジ(F-07-1/-4)。カスタム絵文字は画像+個数、
/// Unicode・未解決はキーの生テキスト+個数。クリックでトグル(決定 O)
///
/// Frame::show で組むと内容確定まで幅が決まらず、horizontal_wrapped の
/// main_wrap が幅を読めずに各バッジを数 px に潰してしまった。Button 系
/// ウィジェットは配置前に幅が決まるため折り返しが正しく効く
pub(super) fn reaction_badge(
    ui: &mut Ui,
    name: &str,
    count: u32,
    note: &Note,
    ctx: &mut UiCtx<'_>,
) {
    let key = parse_reaction_key(name);
    // 自分のリアクション: バッジキーとの一致(ローカルは `:name:`/
    // `:name@.:` の形式違いを同名扱い)のほか、リモート絵文字への
    // 相乗り(ローカル :name@.: を送信済み)も自分のものとして扱う
    let mut mine = note
        .my_reaction
        .as_deref()
        .is_some_and(|m| reaction_keys_match(m, name));
    if let ReactionKey::Remote(n, _) = key {
        mine |= note
            .my_reaction
            .as_deref()
            .is_some_and(|r| r == format!(":{n}@.:"));
    }
    let bg = if mine {
        Color32::from_rgb(0x3a, 0x44, 0x58)
    } else {
        Color32::from_rgb(0x2a, 0x2c, 0x33)
    };
    let stroke = Stroke::new(1.0f32, Color32::from_rgb(0x3a, 0x3e, 0x4a));
    let count_text = RichText::new(count.to_string())
        .size(11.0)
        .color(Color32::LIGHT_GRAY);
    // 絵文字が解決できれば画像+個数、未解決はキーの生テキスト+個数
    let button = match reaction_emoji_url(note, name, ctx) {
        Some(u) => egui::Button::image_and_text(
            egui::Image::new(u).fit_to_exact_size(vec2(14.0, 14.0)),
            count_text,
        ),
        None => egui::Button::new(RichText::new(format!("{name} {count}")).size(11.0)),
    }
    .fill(bg)
    .stroke(stroke);
    // クリック可否(決定 O): リモート絵文字は同名のローカル絵文字がある
    // ときだけ相乗り可能、無ければ表示のみ
    let actionable = match key {
        ReactionKey::Unicode | ReactionKey::Local(_) => true,
        ReactionKey::Remote(n, _) => remote_badge_toggleable(n, ctx.emoji_list),
    };
    let resp = if actionable {
        ui.add(button)
    } else {
        ui.add_enabled(false, button)
            .on_disabled_hover_text("同名のローカル絵文字がないため相乗りできません")
    };
    // 除外矩形に登録しないとカード内クリック判定が会話を開いてしまう
    ctx.card_state.click_exclusions.push(resp.rect);
    if actionable && resp.clicked() {
        // 送る値: リモートへの相乗りはローカル :name@.: に変換(決定 O)、
        // その他はキーから F-07-5 形式へ正規化。取消は my_reaction の
        // 実キーでローカルカウントを引く
        let send = match key {
            ReactionKey::Remote(n, _) => Some(format!(":{n}@.:")),
            _ => reaction_send_value(name),
        };
        if let Some(send) = send {
            let affect = if mine {
                note.my_reaction.clone().unwrap_or_else(|| send.clone())
            } else {
                send.clone()
            };
            ctx.ops.push(UiOp::ToggleReaction {
                note_id: note.id.clone(),
                reaction: affect,
                send: if mine { None } else { Some(send) },
                mine,
            });
        }
    }
}

/// ノート内マップだけでリアクション絵文字 URL を解決する(F-07-4)。
/// ローカル(`:name@.:`)は reactionEmojis(`name@.`/`name` キー)と
/// note.emojis を見て、リモート(`:name@host:`)は reactionEmojis の
/// `name@host` キーのみを見る(io 実測で reactionEmojis はリモート分のみ)。
/// EmojiCache を使わない純粋部分で、None のとき呼び出し側がキャッシュを見る
pub(super) fn reaction_emoji_url_in_note(note: &Note, key: &str) -> Option<String> {
    match parse_reaction_key(key) {
        ReactionKey::Local(name) => note
            .reaction_emojis
            .get(&format!("{name}@."))
            .or_else(|| note.reaction_emojis.get(name))
            .or_else(|| note.emojis.get(name))
            .cloned(),
        ReactionKey::Remote(name, host) => {
            note.reaction_emojis.get(&format!("{name}@{host}")).cloned()
        }
        ReactionKey::Unicode => None,
    }
}

/// リアクション絵文字の画像 URL 解決(F-07-4)。ノート内マップを先に見て、
/// ローカル絵文字は EmojiCache(/api/emoji)にも委ねる
pub(super) fn reaction_emoji_url(note: &Note, key: &str, ctx: &mut UiCtx<'_>) -> Option<String> {
    reaction_emoji_url_in_note(note, key).or_else(|| match parse_reaction_key(key) {
        ReactionKey::Local(name) => ctx.emoji_cache.resolve(name),
        _ => None,
    })
}

/// リアクションキーの同名判定(F-07-4)。ローカル絵文字は `:name:` と
/// `:name@.:` の形式違いを同一として扱う(my_reaction とバッジキーの比較用)
pub(super) fn reaction_keys_match(a: &str, b: &str) -> bool {
    match (parse_reaction_key(a), parse_reaction_key(b)) {
        (ReactionKey::Local(x), ReactionKey::Local(y)) => x == y,
        (ReactionKey::Remote(x, h1), ReactionKey::Remote(y, h2)) => x == y && h1 == h2,
        (ReactionKey::Unicode, ReactionKey::Unicode) => a == b,
        _ => false,
    }
}

/// リモート絵文字バッジの相乗り可否(REA-07・仕様決定 O)。
/// 同名のローカル絵文字(ピッカー一覧掲載)があれば `:name@.:` でトグルできる
pub(super) fn remote_badge_toggleable(name: &str, emoji_list: &[crate::model::Emoji]) -> bool {
    emoji_list.iter().any(|e| e.name == name)
}
