//! トークンの保存と解決。
//! 永続化は OS キーリングのみ(F-01-2、N-02)。利用できない環境では何も保存しない。
//! 開発用の入口として環境変数 `NMNL_TOKEN` で直接トークンを渡せる(永続化しない)。

use keyring::Entry;

/// N-05: キーリングの保存キーはホスト単位にする
const KEYRING_SERVICE: &str = "nmnl-browser";

pub const ENV_TOKEN: &str = "NMNL_TOKEN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// 開発用の環境変数指定(永続化しない)
    EnvVar,
    /// OS キーリングから復元したトークン
    Keyring,
}

#[derive(Debug)]
pub enum TokenResolution {
    Found { token: String, source: TokenSource },
    Missing,
}

/// トークンを解決する。優先順位は環境変数 → キーリング。
pub fn resolve_token(host: &str) -> TokenResolution {
    decide(std::env::var(ENV_TOKEN).ok().as_deref(), stored(host))
}

fn stored(host: &str) -> Option<String> {
    Entry::new(KEYRING_SERVICE, host)
        .ok()?
        .get_password()
        .ok()
        .filter(|t| !t.is_empty())
}

fn decide(env_token: Option<&str>, stored: Option<String>) -> TokenResolution {
    if let Some(t) = env_token.filter(|t| !t.trim().is_empty()) {
        return TokenResolution::Found {
            token: t.to_owned(),
            source: TokenSource::EnvVar,
        };
    }
    match stored {
        Some(t) => TokenResolution::Found {
            token: t,
            source: TokenSource::Keyring,
        },
        None => TokenResolution::Missing,
    }
}

/// キーリングへ保存を試みる。利用できない環境では保存せず false を返す(F-01-2)。
pub fn persist_token(host: &str, token: &str) -> bool {
    Entry::new(KEYRING_SERVICE, host)
        .and_then(|e| e.set_password(token))
        .is_ok()
}

/// 失効したトークンをキーリングから削除する(F-01-3)。
pub fn delete_token(host: &str) {
    let _ = Entry::new(KEYRING_SERVICE, host).and_then(|e| e.delete_credential());
}

#[cfg(test)]
mod tests {
    use super::*;

    // AUTH-03: 環境変数のトークンはキーリング保存分より優先される
    #[test]
    fn auth03_env_token_wins() {
        match decide(Some("dev-token"), Some("stored".to_owned())) {
            TokenResolution::Found { token, source } => {
                assert_eq!(token, "dev-token");
                assert_eq!(source, TokenSource::EnvVar);
            }
            TokenResolution::Missing => panic!("env token should win"),
        }
    }

    // AUTH-04: 環境変数が空ならキーリング、両方なければ Missing
    #[test]
    fn auth04_fallback_order() {
        match decide(Some("  "), Some("stored".to_owned())) {
            TokenResolution::Found { token, source } => {
                assert_eq!(token, "stored");
                assert_eq!(source, TokenSource::Keyring);
            }
            TokenResolution::Missing => panic!("stored token should be used"),
        }
        assert!(matches!(decide(None, None), TokenResolution::Missing));
    }
}
