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
/// メモリキャッシュのエントリ上限(LRU 順に eviction)
const MEM_CACHE_CAP: usize = 2000;
/// 画像 URL のリダイレクト追従上限(リダイレクトループ対策)
const MAX_REDIRECTS: u32 = 5;

/// メモリキャッシュ。HashMap + 挿入順キューの簡易 LRU(F-09-3)。
/// ヒット時にキーを末尾へ積み直し、上限超過で最古参照から捨てる
struct MemCache {
    map: HashMap<String, Entry>,
    order: std::collections::VecDeque<String>,
}

impl MemCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    fn get(&mut self, uri: &str) -> Option<&Entry> {
        if self.map.contains_key(uri) {
            self.order.push_back(uri.to_owned());
        }
        self.map.get(uri)
    }

    fn insert(&mut self, uri: String, entry: Entry) {
        self.map.insert(uri.clone(), entry);
        self.order.push_back(uri);
        while self.map.len() > MEM_CACHE_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            // 二重登録された古いキーも既に map に無いなら no-op で読み飛ばす
            self.map.remove(&oldest);
        }
    }

    fn remove(&mut self, uri: &str) {
        self.map.remove(uri);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

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
    cache: Arc<Mutex<MemCache>>,
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
            cache: Arc::new(Mutex::new(MemCache::new())),
            cache_dir,
            runtime,
            // リダイレクトは手動で追う。ホップごとにスキームと宛先 IP を
            // 再検証して SSRF の迂回を防ぐ(N-02)
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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
            .map
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
        // 通信は https のみ(N-02)。http の画像は拒否する
        if !uri.starts_with("https://") {
            return Err(LoadError::NotSupported);
        }
        {
            let mut cache = self.cache.lock();
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

        // メモリキャッシュへ Pending 登録(上限超過は LRU の最古から落とす)
        {
            let mut cache = self.cache.lock();
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
            .map
            .values()
            .any(|e| matches!(e, Entry::Pending))
    }
}

/// IP がグローバル到達可能か。プライベート・ループバック・リンクローカル等
/// の内部宛てを弾いて SSRF を緩和する(N-02 の延長として防御)
fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // RFC1918/loopback/link-local に加え、CGNAT(100.64/10)、
            // benchmarking(198.18/15)、予約済み(240/4)、documentation
            // (192.0.2・198.51.100・203.0.113)も内部宛てとして弾く
            let manual = o[0] == 0
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 192 && o[1] == 0 && o[2] == 2)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
                || o[0] >= 240;
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || manual)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

/// URL の宛先を検証する。https のみ許可し、ホストが非グローバル IP に
/// 解決される場合は拒否する(IP リテラル直打ちも同じ検査に通す)
async fn validate_image_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("URL が不正: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("http スキームは許可しない".to_owned());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "ホスト名がありません".to_owned())?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !is_public_ip(&ip) {
            return Err("内部宛ての URL は拒否".to_owned());
        }
        return Ok(());
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    // DNS 解決して宛先 IP を検査する。接続時の再解決との差(TOCTOU)は
    // 残存リスクとして受容する(クライアント側 fetcher の実務的な緩和)
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => {
            let mut saw_public = false;
            for sa in addrs {
                if !is_public_ip(&sa.ip()) {
                    return Err("内部宛ての URL は拒否".to_owned());
                }
                saw_public = true;
            }
            if saw_public {
                Ok(())
            } else {
                Err("名前解決できません".to_owned())
            }
        }
        Err(e) => Err(format!("名前解決に失敗: {e}")),
    }
}

/// URI を解決して Entry を返す非同期部。メモリに無いときだけ呼ばれる
async fn load_uri(client: reqwest::Client, cache_dir: PathBuf, uri: &str, path: &PathBuf) -> Entry {
    // ディスクヒット(mtime を更新して LRU 順を維持する)
    if let Ok(bytes) = std::fs::read(path)
        && !bytes.is_empty()
    {
        if let Ok(f) = std::fs::File::options().write(true).open(path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_owned();
        return Entry::Ready(egui::load::Bytes::from(bytes), mime_from_ext(&ext));
    }

    // HTTPS 取得。リダイレクトはホップごとに宛先を再検証して手動で追う
    let mut url = uri.to_owned();
    let mut hops = 0u32;
    let resp = loop {
        if let Err(e) = validate_image_url(&url).await {
            return Entry::Failed(e);
        }
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return Entry::Failed(e.to_string()),
        };
        if !resp.status().is_redirection() {
            break resp;
        }
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Entry::Failed("リダイレクト回数超過".to_owned());
        }
        let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Entry::Failed(format!("HTTP {}", resp.status()));
        };
        // 相対 Location の解決のため現在の URL を基底に join する
        let base = match reqwest::Url::parse(&url) {
            Ok(u) => u,
            Err(e) => return Entry::Failed(e.to_string()),
        };
        url = match base.join(loc) {
            Ok(u) => u.to_string(),
            Err(e) => return Entry::Failed(e.to_string()),
        };
    };
    if !resp.status().is_success() {
        return Entry::Failed(format!("HTTP {}", resp.status()));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// MED-02: http スキームと内部宛て IP を拒否する(N-02 の延長)
    #[test]
    fn med02_public_ip_check() {
        assert!(!is_public_ip(&"10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"192.168.1.1".parse().unwrap()));
        assert!(!is_public_ip(&"127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"169.254.1.1".parse().unwrap()));
        assert!(!is_public_ip(&"100.64.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"::1".parse().unwrap()));
        assert!(!is_public_ip(&"fc00::1".parse().unwrap()));
        assert!(!is_public_ip(&"fe80::1".parse().unwrap()));
        assert!(is_public_ip(&"8.8.8.8".parse().unwrap()));
        assert!(is_public_ip(&"54.230.0.1".parse().unwrap()));
        assert!(is_public_ip(&"2606:4700::1111".parse().unwrap()));
    }

    /// MED-03: http スキームと IP リテラルの内部宛ては即座に拒否する
    #[tokio::test]
    async fn med03_validate_rejects_insecure_and_internal() {
        assert!(
            validate_image_url("http://example.com/a.png")
                .await
                .is_err()
        );
        assert!(validate_image_url("https://127.0.0.1/a.png").await.is_err());
        assert!(validate_image_url("https://10.0.0.5/a.png").await.is_err());
        assert!(validate_image_url("https://[::1]/a.png").await.is_err());
        assert!(validate_image_url("not-a-url").await.is_err());
    }
}
