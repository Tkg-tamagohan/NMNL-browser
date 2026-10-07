use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "NMNL-browser",
        options,
        Box::new(|_cc| Ok(Box::new(NmnlApp))),
    )
}

struct NmnlApp;

impl eframe::App for NmnlApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("NMNL-browser");
            ui.label("misskey.io 専用デッキクライアント");
        });
    }
}
