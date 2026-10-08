//! 画像 URI → バイト列のローダー(F-08-2: 遅延読み込みとディスクキャッシュ)。
//!
//! egui の `BytesLoader` を実装し、メモリ→ディスク→HTTP の順で解決する。
//! ディスクキャッシュは容量上限つき LRU(F-09-3)。`egui_extras` の
//! ImageLoader がバイト列をデコードしてテクスチャ化するので、ここでは
//! URI からバイト列を得る責務だけを持つ。
//!
//! 非同期化は BytesLoader の仕様に沿って Pending を返し、tokio タスクで
//! 裏側の読み取り/取得を進め、完了時に request_repaint する。

use eframe::egui;
use egui::epaint::mutex::Mutex;
use egui::load::{BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use sha2::Digest;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// ディスクキャッシュの容量上限(F-09-3)。256MiB
const DISK_CACHE_CAP: u64 = 256 * 1024 * 1024;
/// メモリキャッシュのエントリ上限。超えたら全消去(粗いが bounded な実装)
const MEM_CACHE_CAP: usize = 2000;

#[derive(Clone)]
enum Entry {
    /// 読み込み中
    Pending,
    /// 取得済み(bytes, mime)
    Ready(egui::load::Bytes, Option<String>),
    /// 失敗(表示エラーに変換するためメッセージを保持)
    Failed(String),
}

/// 遅延読み込みとディスクキャッシュを行う画像バイトローダー
pub struct CachedImageLoader {
    cache: Arc<Mutex<HashMap<String, Entry>>>,
    cache_dir: PathBuf,
    runtime: tokio::runtime::Handle,
    client: reqwest::Client,
}

impl CachedImageLoader {
    /// キャッシュディレクトリを用意してローダーを作る。
    /// 呼び出すのは UI スレッドのセットアップ時(eframe::App::new)を想定
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        let cache_dir = directories::BaseDirs::new()
            .map(|d| d.cache_dir().join("nmnl-browser").join("images"))
            .unwrap_or_else(|| std::env::temp_dir().join("nmnl-browser-images"));
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            cache_dir,
            runtime,
            client: reqwest::Client::new(),
        }
    }

    /// キャッシュした URI をクリアする(設定画面のキャッシュ消去用、F-09-3)
    pub fn clear_all(&self) {
        self.cache.lock().clear();
        let _ = std::fs::remove_dir_all(&self.cache_dir);
        let _ = std::fs::create_dir_all(&self.cache_dir);
    }

    /// キャッシュキーは sha256 の 16 進。衝突耐性と安定性のため固定ハッシュを使う
    fn cache_key(uri: &str) -> String {
        let digest = sha2::Sha256::digest(uri.as_bytes());
        format!("{:x}", digest)
    }

    /// URI → キャッシュファイルのパス。拡張子を末尾に付けて mime を推定可能にする
    fn cache_path(&self, uri: &str) -> PathBuf {
        let ext = ext_from_uri(uri);
        let name = if ext.is_empty() {
            Self::cache_key(uri)
        } else {
            format!("{}.{}", Self::cache_key(uri), ext)
        };
        self.cache_dir.join(name)
    }

    /// メモリキャッシュのサイズ(byte_size 実装用)
    fn mem_bytes(&self) -> usize {
        self.cache
            .lock()
            .values()
            .map(|e| match e {
                Entry::Ready(b, _) => b.len(),
                _ => 0,
            })
            .sum()
    }
}

/// キャッシュディレクトリの合計が上限を超えたら mtime の古い順に削除する
fn evict_if_needed(cache_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    // (mtime, path, size) を古い順に並べて容量を超えた分だけ消す
    let mut files: Vec<(std::time::SystemTime, PathBuf, u64)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                    e.path(),
                    meta.len(),
                )
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    if total <= DISK_CACHE_CAP {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, path, size) in files {
        if total <= DISK_CACHE_CAP {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

fn ext_from_uri(uri: &str) -> String {
    let path = uri.split('?').next().unwrap_or(uri);
    path.rsplit('.')
        .next()
        .filter(|ext| !ext.contains('/') && ext.len() <= 5)
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

fn mime_from_ext(ext: &str) -> Option<String> {
    let mime = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    Some(mime.to_owned())
}

impl BytesLoader for CachedImageLoader {
    fn id(&self) -> &str {
        egui::generate_loader_id!(CachedImageLoader)
    }

    fn load(&self, ctx: &egui::Context, uri: &str) -> BytesLoadResult {
        if !(uri.starts_with("https://") || uri.starts_with("http://")) {
            return Err(LoadError::NotSupported);
        }
        {
            let cache = self.cache.lock();
            match cache.get(uri) {
                Some(Entry::Ready(bytes, mime)) => {
                    return Ok(BytesPoll::Ready {
                        size: None,
                        bytes: bytes.clone(),
                        mime: mime.clone(),
                    });
                }
                Some(Entry::Pending) => {
                    return Ok(BytesPoll::Pending { size: None });
                }
                Some(Entry::Failed(msg)) => {
                    return Err(LoadError::Loading(msg.clone()));
                }
                None => {}
            }
        }

        // メモリキャッシュのエントリ上限。超過時は全消去(再フェッチされる)
        {
            let mut cache = self.cache.lock();
            if cache.len() >= MEM_CACHE_CAP {
                cache.clear();
            }
            cache.insert(uri.to_owned(), Entry::Pending);
        }

        let uri_owned = uri.to_owned();
        let path = self.cache_path(uri);
        let cache = self.cache.clone();
        let client = self.client.clone();
        let cache_dir = self.cache_dir.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let result = load_uri(client, cache_dir, &uri_owned, &path).await;
            cache.lock().insert(uri_owned, result);
            ctx.request_repaint();
        });
        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.cache.lock().remove(uri);
    }

    fn forget_all(&self) {
        self.clear_all();
    }

    fn byte_size(&self) -> usize {
        self.mem_bytes()
    }

    fn has_pending(&self) -> bool {
        self.cache
            .lock()
            .values()
            .any(|e| matches!(e, Entry::Pending))
    }
}

/// URI を解決して Entry を返す非同期部。メモリに無いときだけ呼ばれる
async fn load_uri(client: reqwest::Client, cache_dir: PathBuf, uri: &str, path: &PathBuf) -> Entry {
    // ディスクヒット
    if let Ok(bytes) = std::fs::read(path)
        && !bytes.is_empty()
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_owned();
        return Entry::Ready(egui::load::Bytes::from(bytes), mime_from_ext(&ext));
    }

    // HTTP 取得
    let resp = match client.get(uri).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return Entry::Failed(format!("HTTP {}", r.status())),
        Err(e) => return Entry::Failed(e.to_string()),
    };
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_owned());
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Entry::Failed(e.to_string()),
    };
    if bytes.is_empty() {
        return Entry::Failed("empty body".to_owned());
    }
    // ディスク書き込み(失敗しても表示は続行)
    if std::fs::write(path, &bytes).is_ok() {
        evict_if_needed(&cache_dir);
    }
    // reqwest の Bytes は egui の Bytes と別型なので Vec<u8> 経由で渡す
    Entry::Ready(
        egui::load::Bytes::from(bytes.to_vec()),
        mime.or_else(|| mime_from_ext(&ext_from_uri(uri))),
    )
}
