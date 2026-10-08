//! 設定ファイル(TOML)の読み書き。
//! 配置は OS の設定ディレクトリ(F-09-1)、保存は一時ファイルからの置き換えで行い(N-03)、
//! トークンは `token` モジュールで OS キーリングにのみ保存する(F-01-2、N-02)。

pub mod token;

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

pub const HOST: &str = "misskey.io";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub window: WindowConfig,
    pub columns: Vec<ColumnSpec>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            // 仕様決定 C の MVP カラム構成
            columns: vec![
                ColumnSpec {
                    kind: ColumnKind::Main,
                    width: 340.0,
                    ..ColumnSpec::default()
                },
                ColumnSpec {
                    kind: ColumnKind::Timeline,
                    timeline: Some(TimelineKind::Home),
                    ..ColumnSpec::default()
                },
                ColumnSpec {
                    kind: ColumnKind::Notifications,
                    ..ColumnSpec::default()
                },
                ColumnSpec {
                    kind: ColumnKind::Mentions,
                    ..ColumnSpec::default()
                },
                ColumnSpec {
                    kind: ColumnKind::Channel,
                    ..ColumnSpec::default()
                },
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowConfig {
    pub width: f32,
    pub height: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1280.0,
            height: 720.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    Main,
    Timeline,
    Notifications,
    Mentions,
    Channel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TimelineKind {
    Home,
    Local,
    Social,
    Global,
}

/// F-03-4 のカラム単位フィルタ
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColumnFilters {
    pub include_renotes: bool,
    pub include_replies: bool,
    pub files_only: bool,
}

impl Default for ColumnFilters {
    fn default() -> Self {
        Self {
            include_renotes: true,
            include_replies: true,
            files_only: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColumnSpec {
    pub kind: ColumnKind,
    pub width: f32,
    pub timeline: Option<TimelineKind>,
    pub channel_id: Option<String>,
    pub filters: ColumnFilters,
    /// 通知カラムの除外種別(F-04-1)。カラムごとの設定として永続化(F-02-3/F-09-2)
    pub ntf_exclude: Vec<String>,
}

impl Default for ColumnSpec {
    fn default() -> Self {
        Self {
            kind: ColumnKind::Timeline,
            width: 320.0,
            timeline: None,
            channel_id: None,
            filters: ColumnFilters::default(),
            ntf_exclude: Vec::new(),
        }
    }
}

fn config_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("nmnl-browser"))
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

impl AppConfig {
    /// 保存済み設定を読み込む。ファイルがない・壊れている場合は既定値を返す。
    pub fn load() -> Self {
        match config_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let Ok(body) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str(&body) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("config.toml の解釈に失敗したため既定値を使います: {e}");
                Self::default()
            }
        }
    }

    /// 設定を保存する。同一ディレクトリの一時ファイルを rename して置き換える(N-03)。
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = config_path() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "設定ディレクトリを解決できません",
            ));
        };
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let dir = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "保存先の親がありません")
        })?;
        std::fs::create_dir_all(dir)?;
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(body.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CFG-01: 既定設定が TOML で往復する
    #[test]
    fn cfg01_default_round_trip() {
        let cfg = AppConfig::default();
        let body = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&body).unwrap();
        assert_eq!(parsed, cfg);
    }

    // CFG-02: 項目の欠けた TOML でも serde(default) で既定値が埋まる
    #[test]
    fn cfg02_partial_toml_uses_defaults() {
        let parsed: AppConfig = toml::from_str(
            r#"
columns = [{ kind = "timeline" }]
"#,
        )
        .unwrap();
        assert_eq!(parsed.window, WindowConfig::default());
        assert_eq!(parsed.columns.len(), 1);
        assert_eq!(parsed.columns[0].kind, ColumnKind::Timeline);
        assert_eq!(parsed.columns[0].width, 320.0);
        assert_eq!(parsed.columns[0].filters, ColumnFilters::default());
    }

    // CFG-03: 保存→読込で同一の構成が復元され、一時ファイルが残らない
    #[test]
    fn cfg03_atomic_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.window.width = 999.0;
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), cfg);

        cfg.columns[0].width = 111.0;
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), cfg);

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "config.toml")
            .collect();
        assert!(
            leftovers.is_empty(),
            "一時ファイルが残っています: {leftovers:?}"
        );
    }

    // CFG-04: 壊れた設定ファイルは既定値にフォールバックする
    #[test]
    fn cfg04_malformed_toml_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not toml [[[").unwrap();
        assert_eq!(AppConfig::load_from(&path), AppConfig::default());
    }

    // CFG-05: [window] の項目が部分的に欠けても既定値で埋まり、他の設定を失わない
    #[test]
    fn cfg05_partial_window_keeps_columns() {
        let parsed: AppConfig = toml::from_str(
            r#"
columns = [{ kind = "notifications" }]

[window]
width = 777.0
"#,
        )
        .unwrap();
        assert_eq!(parsed.window.width, 777.0);
        assert_eq!(parsed.window.height, WindowConfig::default().height);
        assert_eq!(parsed.columns[0].kind, ColumnKind::Notifications);
    }
}
