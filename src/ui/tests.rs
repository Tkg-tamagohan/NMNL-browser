use super::windows::back_button_pressed;
use super::*;

fn df(id: &str) -> crate::model::DriveFile {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": "x.png", "type": "image/png"
    }))
    .unwrap()
}

fn df_sensitive(id: &str) -> crate::model::DriveFile {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": "x.png", "type": "image/png", "isSensitive": true
    }))
    .unwrap()
}

// VWR-01: ビューアのページ送りが範囲にクランプされる(F-08-1)
#[test]
fn vwr01_step_clamps() {
    let mut v = ViewerState {
        files: vec![df("a"), df("b"), df("c")],
        index: 0,
        revealed: Default::default(),
    };
    v.step(1);
    assert_eq!(v.index, 1);
    v.step(5);
    assert_eq!(v.index, 2);
    v.step(-10);
    assert_eq!(v.index, 0);
    // 空では常に 0 に戻る
    let mut e = ViewerState::default();
    e.step(3);
    assert_eq!(e.index, 0);
}

// VWR-03: ビューアは Esc キーとマウスの戻るボタンでも閉じる(F-08-6)。
// egui-winit 0.32 は winit の Back→PointerButton::Extra1 を割り当てる
// (src/lib.rs の mouse_button 変換で確認)
#[test]
fn vwr03_esc_and_back_button_close() {
    let press = |button: egui::PointerButton| egui::RawInput {
        events: vec![egui::Event::PointerButton {
            pos: egui::pos2(10.0, 10.0),
            button,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }],
        ..Default::default()
    };
    // 戻るボタン(Extra1)の押下は検出される
    let _ = egui::Context::default().run(press(egui::PointerButton::Extra1), |ctx| {
        assert!(ctx.input(back_button_pressed));
    });
    // 進むボタン(Extra2)や通常ボタンでは閉じない
    for b in [
        egui::PointerButton::Extra2,
        egui::PointerButton::Primary,
        egui::PointerButton::Middle,
    ] {
        let _ = egui::Context::default().run(press(b), |ctx| {
            assert!(!ctx.input(back_button_pressed));
        });
    }
    // Esc キー押下も検出される
    let raw = egui::RawInput {
        events: vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }],
        ..Default::default()
    };
    let _ = egui::Context::default().run(raw, |ctx| {
        assert!(ctx.input(|i| i.key_pressed(egui::Key::Escape)));
    });
}

// VWR-02: ビューア内でも未開封の閲覧注意は覆ったまま(F-08-1)
#[test]
fn vwr02_sensitive_stays_covered() {
    let mut v = ViewerState {
        files: vec![df("a"), df_sensitive("b")],
        index: 0,
        revealed: Default::default(),
    };
    // 非センシティブはそのまま可視、センシティブは開封済みになるまで覆う
    assert!(v.is_visible(&v.files[0]));
    assert!(!v.is_visible(&v.files[1]));
    v.revealed.insert("b".to_owned());
    assert!(v.is_visible(&v.files[1]));
}
