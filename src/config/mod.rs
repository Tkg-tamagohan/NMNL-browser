//! 設定ファイル(TOML)の読み書き。
//! 配置は OS の設定ディレクトリ(F-09-1)、保存は一時ファイルからの置き換えで行い(N-03)、
//! トークンは `token` モジュールで OS キーリングにのみ保存する(F-01-2、N-02)。

pub mod token;

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

pub const HOST: &str = "misskey.io";

/// UI スケール(文字サイズ、F-09-5)の許容範囲と既定値
pub const UI_SCALE_MIN: f32 = 0.8;
pub const UI_SCALE_MAX: f32 = 1.5;
pub const UI_SCALE_DEFAULT: f32 = 1.0;

/// 保存値や入力を許容範囲に正規化する。非有限値(NaN 等)は既定値に戻す
pub fn normalize_ui_scale(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(UI_SCALE_MIN, UI_SCALE_MAX)
    } else {
        UI_SCALE_DEFAULT
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub window: WindowConfig,
    pub columns: Vec<ColumnSpec>,
    /// UI 全体の拡縮倍率(F-09-5)。仕様決定 U で決定した範囲 0.8〜1.5
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
}

fn default_ui_scale() -> f32 {
    UI_SCALE_DEFAULT
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            ui_scale: UI_SCALE_DEFAULT,
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
        match toml::from_str::<AppConfig>(&body) {
            Ok(mut cfg) => {
                // 手編集で範囲外の値が入っても描画が壊れないよう正規化する
                cfg.ui_scale = normalize_ui_scale(cfg.ui_scale);
                cfg
            }
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

    // CFG-06: UI スケール(F-09-5)が保存・復元され、欠落時は 1.0、
    // 範囲外や非有限値は読み込み時に正規化される
    #[test]
    fn cfg06_ui_scale_persisted_and_normalized() {
        // 既定は 1.0、キーが無い既存の設定ファイルも 1.0
        let parsed: AppConfig = toml::from_str(
            r#"
columns = [{ kind = "main" }]
"#,
        )
        .unwrap();
        assert_eq!(parsed.ui_scale, UI_SCALE_DEFAULT);

        // 変更値が往復する
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig {
            ui_scale: 1.3,
            ..AppConfig::default()
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).ui_scale, 1.3);

        // 範囲外はクランプ、NaN は既定値に戻す(TOML は nan を受理する)
        let cfg2 = AppConfig {
            ui_scale: 9.9,
            ..AppConfig::default()
        };
        cfg2.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).ui_scale, UI_SCALE_MAX);
        assert_eq!(
            normalize_ui_scale(f32::NAN),
            UI_SCALE_DEFAULT,
            "NaN は既定値に正規化される"
        );
        assert_eq!(
            normalize_ui_scale(0.1),
            UI_SCALE_MIN,
            "下限未満は下限に正規化される"
        );
    }
}
