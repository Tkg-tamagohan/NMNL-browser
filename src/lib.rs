//! NMNL-browser のライブラリクレート。
//! レイヤ構成は実装計画の「プロジェクト構成」どおり:
//! UI(egui)→ app(状態・イベント集約)→ api/streaming → misskey.io。
//! api・model は UI より前のフェーズで実装されるため、公開 API として lib に置く。

pub mod api;
pub mod app;
pub mod composer;
pub mod config;
pub mod deck;
pub mod emoji;
pub mod image_loader;
pub mod model;
pub mod streaming;
pub mod ui;

/// 起動時間計測の起点(N-01)。main がプロセス開始直後に set し、
/// 最初のフレームで経過をログに出す
pub static STARTED_AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
