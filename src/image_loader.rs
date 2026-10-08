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
    /// uri → (エントリ, アクセス順序番号)
    map: HashMap<String, (Entry, u64)>,
    /// 順序番号 → uri(eviction は先頭から)
    lru: std::collections::BTreeMap<u64, String>,
    next: u64,
}

impl MemCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            lru: std::collections::BTreeMap::new(),
            next: 0,
        }
    }

    /// ヒットしたキーを最新順の末尾へ移す(重複せず同一キー 1 エントリ)
    fn get(&mut self, uri: &str) -> Option<&Entry> {
        let old_seq = self.map.get(uri)?.1;
        let new_seq = self.next;
        self.next += 1;
        self.lru.remove(&old_seq);
        self.lru.insert(new_seq, uri.to_owned());
        let entry = self.map.get_mut(uri).unwrap();
        entry.1 = new_seq;
        Some(&entry.0)
    }

    fn insert(&mut self, uri: String, entry: Entry) {
        let seq = self.next;
        self.next += 1;
        // 上書き時は古い順序番号を外してから登録し直す
        if let Some((_, old_seq)) = self.map.get(&uri) {
            self.lru.remove(old_seq);
        }
        self.map.insert(uri.clone(), (entry, seq));
        self.lru.insert(seq, uri);
        // 上限超過は最古アクセスから落とす(キーとマップは同じ上限で維持)
        while self.map.len() > MEM_CACHE_CAP {
            let Some((_, oldest_uri)) = self.lru.pop_first() else {
                break;
            };
            self.map.remove(&oldest_uri);
        }
    }

    fn remove(&mut self, uri: &str) {
        if let Some((_, seq)) = self.map.remove(uri) {
            self.lru.remove(&seq);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
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
            // リダイレクトは手動で追い、ホップごとにスキームと宛先を再検証。
            // DNS 解決は接続時に独自リゾルバが行うので、検証と接続の間の
            // 再解決差し替え(TOCTOU/DNS rebinding)も塞がれる(N-02)
            // 構築が失敗しても無検査のクライアントには落とさない
            // (リゾルバ無しのフォールバックは宛先検査を素通りさせる)
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(std::sync::Arc::new(PublicOnlyResolver))
                .build()
                .expect("画像クライアントの構築に失敗"),
        }
    }

    /// キャッシュした URI をクリアする(設定画面のキャッシュ消去用、F-09-3)
    pub fn clear_all(&self) {
        self.cache.lock().clear();
        let _ = std::fs::remove_dir_all(&self.cache_dir);
        let _ = std::fs::create_dir_all(&self.cache_dir);
    }

    /// ディスクキャッシュの現在サイズ(設定画面の表示用、F-09-3)
    pub fn cache_bytes(&self) -> u64 {
        std::fs::read_dir(&self.cache_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
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
            .map(|(e, _)| match e {
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
            // 失敗はメモリキャッシュに残って再試しないので、原因をログに残す
            if let Entry::Failed(msg) = &result {
                eprintln!("[image] 取得失敗: {uri_owned} -> {msg}");
            }
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
            .any(|(e, _)| matches!(e, Entry::Pending))
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

/// reqwest の DNS リゾルバ。接続時の名前解決で非グローバル IP を弾く
/// (URL 検証後に DNS 答えが変わる再バインド攻撃を接続側で封じる)
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?
                .filter(|sa| is_public_ip(&sa.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("内部宛ての URL は拒否".to_string().into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// URL の宛先を検証する。https のみ許可し、IP リテラル直打ちの
/// 非グローバル宛てを拒否する(ドメインは接続時リゾルバが担保する)
async fn validate_image_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("URL が不正: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("http スキームは許可しない".to_owned());
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) if !is_public_ip(&std::net::IpAddr::V4(ip)) => {
            Err("内部宛ての URL は拒否".to_owned())
        }
        Some(url::Host::Ipv6(ip)) if !is_public_ip(&std::net::IpAddr::V6(ip)) => {
            Err("内部宛ての URL は拒否".to_owned())
        }
        None => Err("ホスト名がありません".to_owned()),
        _ => Ok(()),
    }
}

/// URI を解決して Entry を返す非同期部。メモリに無いときだけ呼ばれる
async fn load_uri(client: reqwest::Client, cache_dir: PathBuf, uri: &str, path: &PathBuf) -> Entry {
    // ディスクヒット前に宛先検証を通す(過去に保存済みの内部宛て画像も
    // ここで弾く。ディスクキャッシュは URL ハッシュで引くため検証不能)
    if let Err(e) = validate_image_url(uri).await {
        return Entry::Failed(e);
    }
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

    /// MED-04: ディスクヒットでも宛先検証が先に走る(過去保存済みの
    /// 内部宛て画像を再生しない。検証通過後にのみディスクを読む)
    #[tokio::test]
    async fn med04_disk_hit_still_validates() {
        // 内部宛て URL の「既存キャッシュ」を偽装して置く
        let dir = std::env::temp_dir().join("nmnl-test-med04");
        let _ = std::fs::create_dir_all(&dir);
        let key = CachedImageLoader::cache_key("https://127.0.0.1/x.png");
        let path = dir.join(&key);
        std::fs::write(&path, b"PNG").unwrap();
        let entry = load_uri(
            reqwest::Client::new(),
            dir.clone(),
            "https://127.0.0.1/x.png",
            &path,
        )
        .await;
        match entry {
            Entry::Failed(_) => {}
            _ => panic!("内部宛てはディスクヒットの前に拒否されるべき"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// MED-05: メモリキャッシュのアクセス順はヒットで重複登録せず、
    /// 上限超過は最古アクセスから確実に落とす(LRU の厳密性)
    #[test]
    fn med05_mem_cache_lru_exact() {
        let mut cache = MemCache::new();
        for i in 0..MEM_CACHE_CAP {
            cache.insert(format!("u{i}"), Entry::Pending);
        }
        // 同一キーの連続ヒットは順序キューを膨張させない
        for _ in 0..100 {
            cache.get("u0");
        }
        assert_eq!(cache.lru.len(), MEM_CACHE_CAP);
        // u0 は直近アクセス済みなので、新規 1 件の挿入で落ちるのは u1
        cache.insert("new".to_owned(), Entry::Pending);
        assert!(cache.map.contains_key("u0"));
        assert!(!cache.map.contains_key("u1"));
        assert!(cache.map.contains_key("new"));
        assert_eq!(cache.map.len(), MEM_CACHE_CAP);
        assert_eq!(cache.lru.len(), MEM_CACHE_CAP);
    }
}
