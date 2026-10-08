/// トレイアイコンとメニューの構築(F-09-4)。
/// 失敗した環境(システムトレイ非対応など)では None を返して通常動作に落ちる
pub(super) fn build_tray() -> (
    Option<tray_icon::TrayIcon>,
    tray_icon::menu::MenuId,
    tray_icon::menu::MenuId,
) {
    use tray_icon::menu::{Menu, MenuItem};
    // Linux では tray-icon が GTK メニューを使うため先に初期化が必要。
    // 初期化に失敗する環境(wayland 純粋環境など)ではトレイ無しで動かす
    #[cfg(target_os = "linux")]
    if gtk::init().is_err() {
        eprintln!("GTK の初期化に失敗したためトレイは無効です");
        return (
            None,
            tray_icon::menu::MenuId::new("show"),
            tray_icon::menu::MenuId::new("quit"),
        );
    }
    let menu = Menu::new();
    let show_item = MenuItem::new("表示", true, None);
    let quit_item = MenuItem::new("終了", true, None);
    let _ = menu.append(&show_item);
    let _ = menu.append(&quit_item);
    // 32x32 のプログラム生成アイコン(単色の丸)。外部アセットを増やさない
    let rgba = tray_icon_pixels();
    let icon = tray_icon::Icon::from_rgba(rgba, 32, 32).ok();
    let tray = icon.and_then(|icon| {
        tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("NMNL-browser")
            .with_icon(icon)
            .build()
            .map_err(|e| eprintln!("トレイアイコンの構築に失敗: {e}"))
            .ok()
    });
    (tray, show_item.id().clone(), quit_item.id().clone())
}

/// トレイ用の 32x32 RGBA を生成する(ミスキー系の緑で円を描く)
fn tray_icon_pixels() -> Vec<u8> {
    let mut px = vec![0u8; 32 * 32 * 4];
    let c = 16.0f32;
    for y in 0..32u32 {
        for x in 0..32u32 {
            let dx = x as f32 - c + 0.5;
            let dy = y as f32 - c + 0.5;
            if dx * dx + dy * dy <= 14.0 * 14.0 {
                let i = ((y * 32 + x) * 4) as usize;
                px[i] = 0x4a;
                px[i + 1] = 0xc5;
                px[i + 2] = 0x7a;
                px[i + 3] = 0xff;
            }
        }
    }
    px
}
