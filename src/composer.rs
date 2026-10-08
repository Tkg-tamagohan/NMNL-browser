//! 投稿フォームの状態とリクエスト生成(F-06)。
//! フォーム自体は常設のボトムパネルで、返信/引用は対象参照を保持した
//! 同じフォームから投稿する(仕様決定 E・F-06-3)。

use crate::model::{CreateNote, Visibility};
use std::sync::Arc;

/// 添付の上限。misskey.io は 1 投稿 12 ファイル(ユーザー確認済みの io 仕様)
pub const MAX_ATTACHMENTS: usize = 12;

/// 添付として受け付ける拡張子→MIME。アプリがデコード表示できる
/// 形式と揃える(png/jpeg/webp/gif)
pub fn mime_for_path(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        Some("gif") => Some("image/gif"),
        _ => None,
    }
}

/// ドロップされて未アップロードの添付
#[derive(Debug, Clone)]
pub struct PendingFile {
    pub name: String,
    pub mime: String,
    pub data: Arc<Vec<u8>>,
}

/// 返信/引用の対象表示用の最小情報
#[derive(Debug, Clone)]
pub struct PostTarget {
    pub id: String,
    /// フォームに出す要約("@user のノート" など)
    pub label: String,
}

/// 投稿フォームの状態(F-06-1)
#[derive(Debug, Default)]
pub struct Composer {
    pub text: String,
    pub cw: String,
    pub cw_enabled: bool,
    pub visibility: Visibility,
    /// 返信先(F-06-3)。引用と相互排他にする
    pub reply_to: Option<PostTarget>,
    /// 引用元(F-06-3)
    pub quote_of: Option<PostTarget>,
    /// チャンネルへの投稿(既存ノートへの返信等では使わない)
    pub channel_id: Option<String>,
    /// ドロップ済みでまだアップロードしていない添付
    pub files: Vec<PendingFile>,
    /// notes/create が飛行中(二重投稿防止)
    pub posting: bool,
    pub error: Option<String>,
}

impl Composer {
    /// フォーム内容から notes/create のリクエストを作る。
    /// 本文も添付も引用もない投稿は拒否する(Misskey 側も INVALID_PARAM)
    /// `me_id` は visibility=specified のとき宛先として必須
    pub fn build_request(&self, me_id: Option<&str>) -> Result<CreateNote, String> {
        let text = self.text.trim();
        let has_files = !self.files.is_empty();
        if text.is_empty() && !has_files && self.quote_of.is_none() {
            return Err("本文・添付・引用のいずれかが必要です".to_owned());
        }
        let mut req = CreateNote {
            text: if text.is_empty() {
                None
            } else {
                Some(text.to_owned())
            },
            cw: if self.cw_enabled && !self.cw.trim().is_empty() {
                Some(self.cw.trim().to_owned())
            } else {
                None
            },
            visibility: self.visibility,
            reply_id: self.reply_to.as_ref().map(|t| t.id.clone()),
            renote_id: self.quote_of.as_ref().map(|t| t.id.clone()),
            channel_id: self.channel_id.clone(),
            ..Default::default()
        };
        if self.visibility == Visibility::Specified {
            // ダイレクトは宛先必須。対象が取れなければ投稿を止める
            let Some(me) = me_id else {
                return Err("ダイレクト投稿には宛先ユーザー ID が必要です".to_owned());
            };
            req.visible_user_ids = vec![me.to_owned()];
        }
        Ok(req)
    }

    /// ドロップされたファイルを追加する。非対応形式と上限超過は捨てて
    /// 理由を error に書く(呼び出し側が表示)
    pub fn push_dropped(&mut self, path: &std::path::Path, data: Vec<u8>) {
        if self.files.len() >= MAX_ATTACHMENTS {
            self.error = Some(format!("添付は {MAX_ATTACHMENTS} 件までです"));
            return;
        }
        let Some(mime) = mime_for_path(path) else {
            self.error = Some(format!(
                "非対応の形式です: {}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(不明なファイル)")
            ));
            return;
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_owned();
        self.files.push(PendingFile {
            name,
            mime: mime.to_owned(),
            data: Arc::new(data),
        });
    }

    /// 投稿完了後に中身をリセットする
    pub fn clear(&mut self) {
        *self = Composer::default();
    }
}

/// visibility の日本語ラベル(F-06-1 の 4 種)
pub fn visibility_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Public => "パブリック",
        Visibility::Home => "ホーム",
        Visibility::Followers => "フォロワー",
        Visibility::Specified => "ダイレクト",
        Visibility::Unknown => "不明",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // POST-03: フォーム状態→CreateNote の変換。本文のみ/返信+本文/
    // 引用+添付/specified は visibleUserIds 必須
    #[test]
    fn post03_composer_to_request() {
        let mut c = Composer::default();
        assert!(c.build_request(Some("me")).is_err()); // 空

        c.text = "こんにちは".to_owned();
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.text.as_deref(), Some("こんにちは"));
        assert_eq!(r.visibility, Visibility::Public);
        assert!(r.visible_user_ids.is_empty());

        c.reply_to = Some(PostTarget {
            id: "n1".to_owned(),
            label: "x".to_owned(),
        });
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.reply_id.as_deref(), Some("n1"));

        // specified は me_id を宛先にする
        c.visibility = Visibility::Specified;
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.visible_user_ids, vec!["me".to_owned()]);
        // me_id が無いと投稿を止める
        assert!(c.build_request(None).is_err());

        // 引用のみでも投稿可能(本文は空でもよい)
        let q = Composer {
            quote_of: Some(PostTarget {
                id: "n2".to_owned(),
                label: "y".to_owned(),
            }),
            ..Composer::default()
        };
        let r = q.build_request(Some("me")).unwrap();
        assert_eq!(r.renote_id.as_deref(), Some("n2"));
        assert!(r.text.is_none());

        // CW は有効かつ非空のときだけ送る
        c.cw_enabled = true;
        c.cw = "  ".to_owned();
        let r = c.build_request(Some("me")).unwrap();
        assert!(r.cw.is_none());
        c.cw = "注意".to_owned();
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.cw.as_deref(), Some("注意"));
    }

    // POST-04: 添付の受け付け。対応拡張子のみ��� 12 件上限
    #[test]
    fn post04_attachment_rules() {
        let mut c = Composer::default();
        for i in 0..MAX_ATTACHMENTS + 2 {
            c.push_dropped(Path::new(&format!("/tmp/p{i}.png")), vec![0u8]);
        }
        assert_eq!(c.files.len(), MAX_ATTACHMENTS);
        assert!(c.error.is_some());

        // 非対応拡張子は追加されない
        let mut c2 = Composer::default();
        c2.push_dropped(Path::new("/tmp/a.exe"), vec![0u8]);
        assert!(c2.files.is_empty());
        assert!(c2.error.is_some());
        // webp/gif/jpeg は受け付ける
        for n in ["b.webp", "c.gif", "d.jpg", "e.jpeg"] {
            c2.push_dropped(Path::new(&format!("/tmp/{n}")), vec![0u8]);
        }
        assert_eq!(c2.files.len(), 4);
    }
}
