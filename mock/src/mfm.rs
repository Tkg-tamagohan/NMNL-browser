//! MFM サブセットの描画(仕様決定 D / F-05-1)。
//! 太字・斜体・取り消し線・コード・引用・リンク・メンション・ハッシュタグ・
//! カスタム絵文字プレースホルダをスタイル付きで並べ、`$[...]` や HTML 系は
//! 記法を除去してプレーン表示に落とす。本実装のパーサではなく見た目検証用。

use eframe::egui::{Color32, FontId, TextFormat, text::LayoutJob};

/// 本文を LayoutJob に変換する(行内装飾のみ、簡易走査)
pub fn layout(text: &str, ui: &eframe::egui::Ui) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.wrap.max_rows = usize::MAX;
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
            // 引用記号を先に出し、行内装飾(太字・リンク等)は本文同様に走査する
            let mut fmt = TextFormat::simple(base.clone(), Color32::LIGHT_GRAY);
            fmt.italics = true;
            job.append("❝ ", 0.0, fmt);
            let start = job.sections.len();
            render_inline(
                content,
                &mut job,
                &base,
                Color32::LIGHT_GRAY,
                accent,
                code_bg,
                strong_fg,
            );
            // 引用行は全体を斜体にする。追加された区間すべてに italics を付ける
            for sec in &mut job.sections[start..] {
                sec.format.italics = true;
            }
        } else {
            render_inline(content, &mut job, &base, fg, accent, code_bg, strong_fg);
        }
    }
    job
}

fn push(job: &mut LayoutJob, text: &str, fmt: TextFormat) {
    job.append(text, 0.0, fmt);
}

/// 行内の MFM サブセットを走査して LayoutJob へ追加する
fn render_inline(
    s: &str,
    job: &mut LayoutJob,
    base: &FontId,
    fg: Color32,
    accent: Color32,
    code_bg: Color32,
    strong_fg: Color32,
) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut plain = String::new();
    let plain_fmt = TextFormat::simple(base.clone(), fg);

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
            flush!();
            push(job, $text, $fmt);
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
                // `$[spin テキスト]` → 関数名を落として中身だけ
                flush!();
                render_inline(
                    inner[sp..].trim(),
                    job,
                    base,
                    fg,
                    accent,
                    code_bg,
                    strong_fg,
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
            // @user@host 形式
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
        // :custom_emoji: は画像のプレースホルダ(色付きの短形)
        if bytes[i] == b':'
            && let Some(end) = s[i + 1..].find(':')
        {
            let name = &s[i + 1..i + 1 + end];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                let mut fmt = TextFormat::simple(base.clone(), Color32::from_rgb(0xff, 0xde, 0x6b));
                fmt.background = Color32::from_rgb(0x3a, 0x35, 0x20);
                styled!(&format!(":{name}:"), fmt);
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

        // UTF-8 の文字境界で 1 文字ずつ取り込む(区切り記号はすべて ASCII のため
        // マルチバイト文字をまとめて読み飛ばしても走査を壊さない)
        let ch = s[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush!();
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
    use eframe::egui;

    /// layout() の LayoutJob を取得する(テキストと区間書式を検査)
    fn layout_job(input: &str) -> egui::text::LayoutJob {
        let ctx = egui::Context::default();
        let mut job = egui::text::LayoutJob::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                job = super::layout(input, ui);
            });
        });
        job
    }

    /// layout() の出力テキストを結合して返す(装飾は捨てて文字列だけ検査)
    fn layout_text(input: &str) -> String {
        layout_job(input).text
    }

    // MFM-01: 装飾記法は除去され、本文は残る(F-05-1/6)
    #[test]
    fn mfm01_markup_stripped() {
        let out = layout_text("先に**太字**と`code`と*斜体*と~~取消~~です");
        assert_eq!(out, "先に太字とcodeと斜体と取消です");
    }

    // MFM-02: マルチバイト文字を含んでもパニックしない(UTF-8 文字境界の回帰)
    #[test]
    fn mfm02_multibyte_safe() {
        let out = layout_text("日本語の文に**太字**と`コード`と:emoji_name:を混ぜる");
        assert!(out.contains("日本語の文に太字とコードと:emoji_name:を混ぜる"));
        let out2 = layout_text("絵文字:reaction:と #タグ と@user@misskey.io");
        assert!(out2.contains("#タグ"));
    }

    // MFM-03: 引用行の内側もインライン装飾が適用され、行全体が斜体(F-05-8)
    #[test]
    fn mfm03_quote_inline() {
        let job = layout_job("> **強調**と@aliceの引用行");
        assert!(job.text.starts_with("❝ "));
        assert!(
            job.text.contains("強調と@aliceの引用行"),
            "out: {}",
            job.text
        );
        assert!(
            !job.text.contains("**"),
            "装飾記法が残っている: {}",
            job.text
        );
        // 引用記号と本文の全区間が斜体(メンション等の装飾色は維持する)
        for sec in &job.sections {
            let text = &job.text[sec.byte_range.clone()];
            assert!(sec.format.italics, "斜体になっていない区間がある: {text:?}");
        }
    }
}
