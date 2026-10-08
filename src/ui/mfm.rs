//! MFM サブセットの描画(仕様決定 D / F-05-1)。
//! モックで検証したパーサを実装版に移植し、カスタム絵文字を
//! プレースホルダではなく画像でインライン描画する拡張を加えた(F-05-2)。
//!
//! 本文はスタイル付きのテキスト区間(LayoutJob)と絵文字画像の混在列
//! (Piece)に分解する。絵文字を含まないノートは従来どおり 1 個の
//! LayoutJob だけなので、描画はモックと同じパスに落ちる。

use eframe::egui::{self, Color32, FontId, TextFormat, Ui, text::LayoutJob};
#[cfg(test)]
use std::collections::HashMap;

/// 本文 1 ブロックを構成する断片。
/// 絵文字解決が無い場合は Job 1 個だけになる
pub enum Piece {
    /// スタイル付きテキスト区間
    Job(LayoutJob),
    /// カスタム絵文字(URL が解決済みのものだけ)
    Emoji(String),
}

/// 本文を描画ピース列に変換する。
/// `resolve` は `:name:` を画像 URL に解決する関数(未解決は None を返し
/// 呼び出し側の絵文字キャッシュが取得を進める)
pub fn layout_pieces(
    text: &str,
    ui: &Ui,
    resolve: &mut dyn FnMut(&str) -> Option<String>,
) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut job = LayoutJob::default();
    // max_width は Label::wrap() が利用可能幅で上書きするため設定しない。
    // 空白を含まない長いトークン(URL 等)でも列幅を超えて確保矩形が領域を
    // 拡大するのを防ぐため、任意位置での折り返しを許可する
    job.wrap.break_anywhere = true;
    let base_size = ui.text_style_height(&eframe::egui::TextStyle::Body);
    let base = FontId::proportional(base_size);
    let fg = ui.visuals().text_color();
    let accent = Color32::from_rgb(0x8b, 0xb2, 0xff);
    let code_bg = Color32::from_rgb(0x2a, 0x2c, 0x33);
    let strong_fg = ui.visuals().strong_text_color();

    // 引用行: "> " で始まる行は先に分ける
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            job.append("\n", 0.0, TextFormat::simple(base.clone(), fg));
        }
        let (is_quote, content) = match line.strip_prefix("> ") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        if is_quote {
            let mut fmt = TextFormat::simple(base.clone(), Color32::LIGHT_GRAY);
            fmt.italics = true;
            job.append("❝ ", 0.0, fmt);
            render_inline(
                content,
                &mut job,
                &mut pieces,
                &base,
                Color32::LIGHT_GRAY,
                accent,
                code_bg,
                strong_fg,
                resolve,
                true,
            );
        } else {
            render_inline(
                content,
                &mut job,
                &mut pieces,
                &base,
                fg,
                accent,
                code_bg,
                strong_fg,
                resolve,
                false,
            );
        }
    }
    // 末尾の未完 Job を畳み込む
    flush_job(&mut pieces, &mut job);
    if pieces.is_empty() {
        pieces.push(Piece::Job(LayoutJob::default()));
    }
    pieces
}

/// ノートの絵文字マップだけで解決する簡易版(キャッシュ無し、表示のみ)
#[cfg(test)]
pub fn layout_with_map(text: &str, ui: &Ui, emojis: &HashMap<String, String>) -> Vec<Piece> {
    layout_pieces(text, ui, &mut |name| emojis.get(name).cloned())
}

/// 未完の LayoutJob を Piece に畳み込む(空は積まない)
fn flush_job(pieces: &mut Vec<Piece>, job: &mut LayoutJob) {
    if job.text.is_empty() {
        return;
    }
    pieces.push(Piece::Job(std::mem::take(job)));
}

fn push(job: &mut LayoutJob, text: &str, fmt: TextFormat) {
    job.append(text, 0.0, fmt);
}

/// 行内の MFM サブセットを走査する。
/// 絵文字は resolve で画像 URL に解決できた場合のみ Piece::Emoji を出し、
/// その直前までのテキストを Job として確定する
#[allow(clippy::too_many_arguments)]
fn render_inline(
    s: &str,
    job: &mut LayoutJob,
    pieces: &mut Vec<Piece>,
    base: &FontId,
    fg: Color32,
    accent: Color32,
    code_bg: Color32,
    strong_fg: Color32,
    resolve: &mut dyn FnMut(&str) -> Option<String>,
    // 引用行の内側は全区間を斜体にする(F-05-8)。
    // :emoji: 解決で job が途中で flush されても確実に付くよう、
    // 追加後に sections を遡るのではなく push 時点で立てる
    italics_all: bool,
) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut plain = String::new();
    let mut plain_fmt = TextFormat::simple(base.clone(), fg);
    plain_fmt.italics = italics_all;

    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                push(job, &plain, plain_fmt.clone());
                plain.clear();
            }
        };
    }
    macro_rules! styled {
        ($text:expr, $fmt:expr) => {{
            let mut fmt = $fmt;
            fmt.italics |= italics_all;
            flush!();
            push(job, $text, fmt);
        }};
    }

    while i < bytes.len() {
        // 未対応の関数記法 $[fn ...] / <tag> は記法だけ除去して中身をプレーンに
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'['
            && let Some(end) = find_closing(s, i + 2, b']')
        {
            let inner = &s[i + 2..end];
            if let Some(sp) = inner.find(char::is_whitespace) {
                flush!();
                render_inline(
                    inner[sp..].trim(),
                    job,
                    pieces,
                    base,
                    fg,
                    accent,
                    code_bg,
                    strong_fg,
                    resolve,
                    italics_all,
                );
            }
            i = end + 1;
            continue;
        }
        if bytes[i] == b'<'
            && let Some(end) = s[i..].find('>')
        {
            let tag = &s[i + 1..i + end];
            if tag
                .chars()
                .all(|c| c.is_alphanumeric() || c == '/' || c == ' ')
            {
                i += end + 1;
                continue; // HTML 系タグは除去
            }
        }
        // **bold**(egui に太字ファミリはないため strong 色+僅かなサイズ増で代替)
        if s[i..].starts_with("**")
            && let Some(end) = s[i + 2..].find("**")
        {
            let fmt = TextFormat::simple(FontId::proportional(base.size * 1.05), strong_fg);
            styled!(&s[i + 2..i + 2 + end], fmt);
            i += 2 + end + 2;
            continue;
        }
        // ~~strike~~
        if s[i..].starts_with("~~")
            && let Some(end) = s[i + 2..].find("~~")
        {
            let mut fmt = TextFormat::simple(base.clone(), fg);
            fmt.strikethrough = eframe::egui::Stroke::new(1.0f32, fg);
            styled!(&s[i + 2..i + 2 + end], fmt);
            i += 2 + end + 2;
            continue;
        }
        // *italic*(** と被らないよう単独 *)
        if bytes[i] == b'*'
            && !s[i..].starts_with("**")
            && let Some(end) = s[i + 1..].find('*')
        {
            let mut fmt = TextFormat::simple(base.clone(), fg);
            fmt.italics = true;
            styled!(&s[i + 1..i + 1 + end], fmt);
            i += 1 + end + 1;
            continue;
        }
        // `code`
        if bytes[i] == b'`'
            && let Some(end) = s[i + 1..].find('`')
        {
            let mut fmt = TextFormat::simple(
                FontId::monospace(base.size * 0.92),
                Color32::from_rgb(0xff, 0xcc, 0x99),
            );
            fmt.background = code_bg;
            styled!(&s[i + 1..i + 1 + end], fmt);
            i += 1 + end + 1;
            continue;
        }
        // @mention / @user@host
        if bytes[i] == b'@'
            && (i == 0 || !is_word_char(bytes[i - 1]))
            && let Some(len) = scan_token(s, i + 1, |c| {
                c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.'
            })
        {
            let mut upto = i + 1 + len;
            if upto < bytes.len()
                && bytes[upto] == b'@'
                && let Some(hlen) = scan_token(s, upto + 1, |c| {
                    c.is_ascii_alphanumeric() || c == b'.' || c == b'-'
                })
            {
                upto += 1 + hlen;
            }
            styled!(&s[i..upto], TextFormat::simple(base.clone(), accent));
            i = upto;
            continue;
        }
        // #tag
        if bytes[i] == b'#'
            && let Some(len) = scan_token(s, i + 1, |c| {
                c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
            })
        {
            styled!(&s[i..i + 1 + len], TextFormat::simple(base.clone(), accent));
            i += 1 + len;
            continue;
        }
        // :custom_emoji: は解決できれば画像ピース、未解決は色付きプレースホルダ
        if bytes[i] == b':'
            && let Some(end) = s[i + 1..].find(':')
        {
            let name = &s[i + 1..i + 1 + end];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                if let Some(url) = resolve(name) {
                    flush!();
                    flush_job(pieces, job);
                    pieces.push(Piece::Emoji(url));
                } else {
                    let mut fmt =
                        TextFormat::simple(base.clone(), Color32::from_rgb(0xff, 0xde, 0x6b));
                    fmt.background = Color32::from_rgb(0x3a, 0x35, 0x20);
                    styled!(&format!(":{name}:"), fmt);
                }
                i += 1 + end + 1;
                continue;
            }
        }
        // URL
        if (s[i..].starts_with("http://") || s[i..].starts_with("https://"))
            && let Some(len) = scan_token(s, i, |c| !c.is_ascii_whitespace())
        {
            let mut fmt = TextFormat::simple(base.clone(), accent);
            fmt.underline = eframe::egui::Stroke::new(1.0f32, accent);
            styled!(&s[i..i + len], fmt);
            i += len;
            continue;
        }

        // UTF-8 の文字境界で 1 文字ずつ取り込む
        let ch = s[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush!();
}

/// Piece 列を描画する。絵文字を含まない場合は 1 個の Label に畳み、
/// 混在する場合は horizontal_wrapped の行内フローでテキストと画像を並べる
pub fn render(ui: &mut Ui, pieces: Vec<Piece>) {
    let emoji_h = ui.text_style_height(&eframe::egui::TextStyle::Body) + 4.0;
    if pieces.len() == 1 {
        match pieces.into_iter().next().unwrap() {
            Piece::Job(job) => {
                ui.add(egui::Label::new(job).wrap());
            }
            Piece::Emoji(url) => {
                ui.add(egui::Image::new(url).fit_to_exact_size(egui::vec2(emoji_h, emoji_h)));
            }
        }
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for piece in pieces {
            match piece {
                Piece::Job(job) => {
                    ui.add(egui::Label::new(job).wrap());
                }
                Piece::Emoji(url) => {
                    ui.add(egui::Image::new(url).fit_to_exact_size(egui::vec2(emoji_h, emoji_h)));
                }
            }
        }
    });
}

fn find_closing(s: &str, from: usize, close: u8) -> Option<usize> {
    s.as_bytes()[from..]
        .iter()
        .position(|&b| b == close)
        .map(|p| from + p)
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn scan_token(s: &str, from: usize, pred: impl Fn(u8) -> bool) -> Option<usize> {
    let bytes = s.as_bytes();
    if from >= bytes.len() {
        return None;
    }
    let mut len = 0;
    while from + len < bytes.len() && pred(bytes[from + len]) {
        len += 1;
    }
    (len > 0).then_some(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    fn pieces_of(input: &str, emojis: &[(&str, &str)]) -> Vec<String> {
        let ctx = egui::Context::default();
        let map: HashMap<String, String> = emojis
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut texts = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let pieces = layout_with_map(input, ui, &map);
                for p in pieces {
                    match p {
                        Piece::Job(j) => texts.push(j.text),
                        Piece::Emoji(u) => texts.push(format!("[img:{u}]")),
                    }
                }
            });
        });
        texts
    }

    // MFM-04: 絵文字マップで解決できた :name: は画像ピースになる(F-05-2)
    #[test]
    fn mfm04_emoji_piece() {
        let texts = pieces_of(
            "あいう:reaction:えお",
            &[("reaction", "https://files.io/e.webp")],
        );
        assert_eq!(
            texts,
            vec![
                "あいう".to_owned(),
                "[img:https://files.io/e.webp]".to_owned(),
                "えお".to_owned()
            ]
        );
    }

    // MFM-05: 未解決の :name: はプレースホルダ表示に留まる(F-05-2 のオンデマンド前提)
    #[test]
    fn mfm05_emoji_placeholder() {
        let texts = pieces_of("まえ:unknown_emoji:あと", &[]);
        assert_eq!(texts, vec!["まえ:unknown_emoji:あと".to_owned()]);
    }

    // MFM-06: 引用行の内側もインライン装飾が適用され、行全体が斜体(F-05-8)
    #[test]
    fn mfm06_quote_inline() {
        let ctx = egui::Context::default();
        let mut job_sections = Vec::new();
        let mut text = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut resolve = |_: &str| None;
                for p in layout_pieces("> **強調**と@aliceの引用行", ui, &mut resolve) {
                    if let Piece::Job(j) = p {
                        text.push_str(&j.text);
                        for sec in &j.sections {
                            job_sections.push((sec.byte_range.clone(), sec.format.italics));
                        }
                    }
                }
            });
        });
        assert!(text.starts_with("❝ "));
        assert!(text.contains("強調と@aliceの引用行"));
        assert!(!text.contains("**"));
        for (range, italics) in job_sections {
            let t = &text[range];
            assert!(italics, "斜体になっていない区間: {t:?}");
        }
    }

    // MFM-07: 引用行の中で :emoji: が解決されてもパニックしない。
    // かつて job.sections を事後スライスしていたが、emoji 解決時の
    // flush_job で job が空に差し替わり範囲外 index になっていた(F-05-8)
    #[test]
    fn mfm07_quote_with_emoji_no_panic() {
        let ctx = egui::Context::default();
        let mut jobs = 0;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let map: HashMap<String, String> =
                    [("e1".to_owned(), "https://files.io/e.webp".to_owned())]
                        .into_iter()
                        .collect();
                let pieces = layout_with_map("> 引用:e1:終わり", ui, &map);
                for p in &pieces {
                    if let Piece::Job(j) = p {
                        jobs += 1;
                        for sec in &j.sections {
                            assert!(
                                sec.format.italics,
                                "引用行内で斜体になっていない区間: {:?}",
                                j.text
                            );
                        }
                    }
                }
            });
        });
        assert!(jobs >= 2, "絵文字の前後で Job が分かれていない: {jobs}");
    }
}
