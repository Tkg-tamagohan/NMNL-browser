use eframe::egui;
use nmnl_browser::app::NmnlApp;
use nmnl_browser::config::AppConfig;
use std::sync::Arc;

fn main() -> eframe::Result<()> {
    let config = AppConfig::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([config.window.width, config.window.height]),
        ..Default::default()
    };
    eframe::run_native(
        "NMNL-browser",
        options,
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            Ok(Box::new(NmnlApp::new(cc)))
        }),
    )
}

// egui のデフォルトフォントは和文グリフを持たないため、
// 仕様決定 J(UI 言語は日本語のみ)を満たすため Noto Sans CJK JP の言語別サブセット版
// (notofonts/noto-cjk の SubsetOTF/JP、約 4.4MB、SIL OFL 1.1)を同梱して登録する
fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto-sans-jp".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSansJP-Regular.otf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "noto-sans-jp".to_owned());
    // 等幅はデフォルトの欧文フォントを優先し、和文のみフォールバック先にする
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("noto-sans-jp".to_owned());
    ctx.set_fonts(fonts);
}
