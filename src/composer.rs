//! 投稿フォームの状態とリクエスト生成(F-06)。
//! フォームはメインカラム内に配置し、メインカラムが無いときだけ
//! ボトムパネルに自動表示する(仕様決定 X・F-06-5)。
//! 返信/引用は対象参照を保持した同じフォームから投稿する(仕様決定 E・F-06-3)。

use crate::model::{CreateNote, NoteChannel, Visibility};
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
    /// アップロード済みのドライブ ID。投稿失敗後のリトライで
    /// 同じファイルを再アップロードしないよう保持する
    pub uploaded_id: Option<String>,
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
    /// チャンネルへの投稿先(F-06-4)。None は通常投稿
    pub channel_id: Option<String>,
    /// 投稿先チャンネルの表示名。None のときは channel_id を表示に使う
    pub channel_name: Option<String>,
    /// ドロップ済みでまだアップロードしていない添付
    pub files: Vec<PendingFile>,
    /// notes/create が飛行中(二重投稿防止)
    pub posting: bool,
    pub error: Option<String>,
}

impl Composer {
    /// フォーム内容から notes/create のリクエストを作る。
    /// 本文も添付もない投稿は拒否する(Misskey 側も INVALID_PARAM。
    /// 引用・返信の参照だけではノートが成立しない — 中身のない
    /// renote_id 付き投稿は表示上リノートと区別が付かない)
    /// `me_id` は visibility=specified のとき宛先として必須
    pub fn build_request(&self, me_id: Option<&str>) -> Result<CreateNote, String> {
        let text = self.text.trim();
        let has_files = !self.files.is_empty();
        if text.is_empty() && !has_files {
            return Err("本文または添付が必要です".to_owned());
        }
        // チャンネル投稿は公開範囲パブリック固定・ダイレクト不可
        // (仕様決定 W・F-06-4)。フォーム状態に関わらずリクエスト側でも強制する
        let visibility = if self.channel_id.is_some() {
            Visibility::Public
        } else {
            self.visibility
        };
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
            visibility,
            reply_id: self.reply_to.as_ref().map(|t| t.id.clone()),
            renote_id: self.quote_of.as_ref().map(|t| t.id.clone()),
            channel_id: self.channel_id.clone(),
            ..Default::default()
        };
        if visibility == Visibility::Specified {
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
        // 同一内容の重複添付は io 側で INVALID_PARAM になる実測が
        // あるため、バイト列が一致する二重追加は弾く。同名+同サイズでは
        // 別ファイルを誤爆するので内容で比較する
        if self
            .files
            .iter()
            .any(|f| f.data.as_slice() == data.as_slice())
        {
            self.error = Some(format!("同じ内容のファイルは重複添付できません: {name}"));
            return;
        }
        self.files.push(PendingFile {
            name,
            mime: mime.to_owned(),
            data: Arc::new(data),
            uploaded_id: None,
        });
    }

    /// 投稿先チャンネルを設定/解除する(F-06-4)。
    /// チャンネル投稿は公開範囲パブリック固定なので、選択時は
    /// フォームの公開範囲もパブリックに揃える(解除時は元の値へは戻さない)
    pub fn set_channel(&mut self, id: Option<String>, name: Option<String>) {
        self.channel_id = id;
        self.channel_name = name;
        if self.channel_id.is_some() {
            self.visibility = Visibility::Public;
        }
    }

    /// 対象ノートのチャンネル継承(仕様決定 W)。返信/引用の対象が
    /// チャンネル所属なら、投稿も同じチャンネルへ向ける
    pub fn inherit_channel(&mut self, channel: Option<&NoteChannel>) {
        if let Some(ch) = channel {
            self.set_channel(Some(ch.id.clone()), ch.name.clone());
        }
    }

    /// 投稿完了後に中身をリセットする
    pub fn clear(&mut self) {
        *self = Composer::default();
    }
}

/// 即時リノートのリクエスト(F-06)。対象がチャンネル所属なら
/// channelId を継承して同じチャンネルへ送る(仕様決定 W。継承時の io 受理と反映は未検証)
pub fn renote_request(note_id: &str, channel: Option<&NoteChannel>) -> CreateNote {
    CreateNote {
        renote_id: Some(note_id.to_owned()),
        channel_id: channel.map(|c| c.id.clone()),
        ..Default::default()
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
    // 引用+本文(引用のみは拒否)/specified は visibleUserIds 必須
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

        // 引用は本文か添付を伴う場合だけ送れる。引用だけの投稿は
        // 中身のない renote_id 投稿=純粋リノートと区別が付かないため拒否
        let mut q = Composer {
            quote_of: Some(PostTarget {
                id: "n2".to_owned(),
                label: "y".to_owned(),
            }),
            ..Composer::default()
        };
        assert!(q.build_request(Some("me")).is_err());
        q.text = "コメント".to_owned();
        let r = q.build_request(Some("me")).unwrap();
        assert_eq!(r.renote_id.as_deref(), Some("n2"));
        assert_eq!(r.text.as_deref(), Some("コメント"));

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
            c.push_dropped(Path::new(&format!("/tmp/p{i}.png")), vec![i as u8]);
        }
        assert_eq!(c.files.len(), MAX_ATTACHMENTS);
        assert!(c.error.is_some());

        // 非対応拡張子は追加されない
        let mut c2 = Composer::default();
        c2.push_dropped(Path::new("/tmp/a.exe"), vec![0u8]);
        assert!(c2.files.is_empty());
        assert!(c2.error.is_some());
        // webp/gif/jpeg は受け付ける
        for (i, n) in ["b.webp", "c.gif", "d.jpg", "e.jpeg"].iter().enumerate() {
            c2.push_dropped(Path::new(&format!("/tmp/{n}")), vec![i as u8]);
        }
        assert_eq!(c2.files.len(), 4);

        // 同一内容の二重添付は弾く(io が INVALID_PARAM を返す実測)
        let mut c3 = Composer::default();
        c3.push_dropped(Path::new("/tmp/same.png"), vec![7u8; 4]);
        c3.push_dropped(Path::new("/tmp/same.png"), vec![7u8; 4]);
        assert_eq!(c3.files.len(), 1);
        assert!(c3.error.is_some());
        // 同名+同サイズでも中身が違えば別ファイルとして受け付ける
        c3.push_dropped(Path::new("/tmp/same.png"), vec![8u8; 4]);
        assert_eq!(c3.files.len(), 2);
    }

    // POST-05: チャンネル投稿(F-06-4)。channelId が乗り、
    // 公開範囲はフォーム状態に関わらずパブリック固定になる
    #[test]
    fn post05_channel_post_forces_public() {
        let mut c = Composer {
            text: "チャンネルへ".to_owned(),
            ..Composer::default()
        };
        c.set_channel(Some("ch1".to_owned()), Some("テストチャンネル".to_owned()));
        // set_channel でフォームの公開範囲もパブリックに揃う
        assert_eq!(c.visibility, Visibility::Public);
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.channel_id.as_deref(), Some("ch1"));
        assert_eq!(r.visibility, Visibility::Public);
        assert!(r.visible_user_ids.is_empty());

        // フォーム側がダイレクトのまま残ってもリクエストはパブリックに矯正され、
        // 宛先リストは付かない(ダイレクト不可、仕様決定 W)
        c.visibility = Visibility::Specified;
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.visibility, Visibility::Public);
        assert!(r.visible_user_ids.is_empty());

        // 解除するとフォーム選択がそのまま使われる
        c.set_channel(None, None);
        c.visibility = Visibility::Home;
        let r = c.build_request(Some("me")).unwrap();
        assert!(r.channel_id.is_none());
        assert_eq!(r.visibility, Visibility::Home);
    }

    // POST-06: チャンネル所属ノートへの返信/引用/リノートは channelId を
    // 継承して同じチャンネルへ投稿する(仕様決定 W)
    #[test]
    fn post06_channel_inheritance() {
        let ch = crate::model::NoteChannel {
            id: "ch9".to_owned(),
            name: Some("対象チャンネル".to_owned()),
            color: None,
            is_sensitive: false,
        };
        // 返信対象がチャンネル所属 → フォームの投稿先がそのチャンネルになる
        let mut c = Composer {
            reply_to: Some(PostTarget {
                id: "n1".to_owned(),
                label: "x".to_owned(),
            }),
            ..Composer::default()
        };
        c.inherit_channel(Some(&ch));
        assert_eq!(c.channel_id.as_deref(), Some("ch9"));
        assert_eq!(c.channel_name.as_deref(), Some("対象チャンネル"));
        assert_eq!(c.visibility, Visibility::Public);
        c.text = "返信".to_owned();
        let r = c.build_request(Some("me")).unwrap();
        assert_eq!(r.channel_id.as_deref(), Some("ch9"));
        assert_eq!(r.reply_id.as_deref(), Some("n1"));
        assert_eq!(r.visibility, Visibility::Public);

        // 対象にチャンネルが無ければ既存の投稿先を変えない
        let mut c2 = Composer::default();
        c2.set_channel(Some("ch_user".to_owned()), None);
        c2.inherit_channel(None);
        assert_eq!(c2.channel_id.as_deref(), Some("ch_user"));

        // リノートは即時投稿なので channelId はリクエストへ直接継承する
        let r = renote_request("n2", Some(&ch));
        assert_eq!(r.renote_id.as_deref(), Some("n2"));
        assert_eq!(r.channel_id.as_deref(), Some("ch9"));
        let r = renote_request("n3", None);
        assert!(r.channel_id.is_none());

        // 埋め込み channel を持たず channel_id だけのノートでも
        // 継承先を解決できる(channel_for_inherit のフォールバック)
        let n: crate::model::Note = serde_json::from_value(serde_json::json!({
            "id": "n9",
            "createdAt": "2026-10-08T00:00:00.000Z",
            "userId": "u1",
            "user": { "id": "u1", "username": "alice" },
            "channelId": "ch_idonly"
        }))
        .unwrap();
        assert!(n.channel.is_none());
        let ch = n.channel_for_inherit().expect("channel_id から合成される");
        assert_eq!(ch.id, "ch_idonly");
        assert_eq!(ch.name, None);
        let mut c3 = Composer::default();
        c3.inherit_channel(Some(&ch));
        assert_eq!(c3.channel_id.as_deref(), Some("ch_idonly"));
    }
}
