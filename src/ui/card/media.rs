use super::{UiCtx, UiOp};
use crate::model::{DriveFile, Note};
use eframe::egui::{self, Color32, RichText, Sense, Ui, vec2};

/// 表示対象のメディア分類。F-08-3 の外部ブラウザ起動対象かどうかの判定に使う
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaKind {
    Image,
    Video,
    Audio,
    Other,
}

pub(super) fn media_kind(file: &DriveFile) -> MediaKind {
    if file.file_type.starts_with("image/") {
        MediaKind::Image
    } else if file.file_type.starts_with("video/") {
        MediaKind::Video
    } else if file.file_type.starts_with("audio/") {
        MediaKind::Audio
    } else {
        MediaKind::Other
    }
}

/// 画像表示の高さ上限目安(仕様決定 M)
pub(super) const IMG_MAX_H: f32 = 360.0;
/// グリッドセル間の隙間
pub(super) const GRID_GAP: f32 = 4.0;

/// 添付ファイル(F-08)。画像は仕様決定 L の枚数グリッドでインライン表示し、
/// 動画・音声・その他は外部ブラウザで開く(F-08-3)
pub(super) fn media_row(ui: &mut Ui, note: &Note, ctx: &mut UiCtx<'_>, _col_id: u64) {
    if note.files.is_empty() {
        return;
    }
    // ビューア用にノート内の画像一覧を先に集める(F-08-1)
    let images: Vec<DriveFile> = note
        .files
        .iter()
        .filter(|f| media_kind(f) == MediaKind::Image)
        .cloned()
        .collect();
    let cells = grid_cells(images.len(), ui.available_width());
    match images.len() {
        0 => {}
        // 1 枚: 全幅。2 枚: 横 2 分割
        1 | 2 => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GRID_GAP;
                for (i, file) in images.iter().enumerate() {
                    let (w, h) = cells[i];
                    image_cell(ui, file, &images, i, w, h, ctx);
                }
            });
        }
        // 3 枚: 左大 + 右 2(上下)
        3 => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GRID_GAP;
                let (w, h) = cells[0];
                image_cell(ui, &images[0], &images, 0, w, h, ctx);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = GRID_GAP;
                    for i in 1..3 {
                        let (w, h) = cells[i];
                        image_cell(ui, &images[i], &images, i, w, h, ctx);
                    }
                });
            });
        }
        // 4 枚以上: 2 列グリッド(2 枚ごとに行を切る)
        _ => {
            let mut start = 0;
            while start < images.len() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GRID_GAP;
                    for i in start..(start + 2).min(images.len()) {
                        let (w, h) = cells[i];
                        image_cell(ui, &images[i], &images, i, w, h, ctx);
                    }
                });
                start += 2;
            }
        }
    }
    // 動画・音声・その他の添付は外部ブラウザで開く(F-08-3)
    ui.horizontal_wrapped(|ui| {
        for file in &note.files {
            if media_kind(file) == MediaKind::Image {
                continue;
            }
            let icon = if file.file_type.starts_with("video/") {
                "🎬"
            } else {
                "🎵"
            };
            let label = format!("{icon} {} をブラウザで開く", file.name);
            if ui
                .add(egui::Button::new(RichText::new(label).size(11.0)).wrap())
                .clicked()
                && let Some(u) = &file.url
            {
                ctx.ops.push(UiOp::OpenUrl(u.clone()));
            }
        }
    });
}

/// 枚数グリッドのセル定義(IMG-01・仕様決定 L)。
/// 戻り値は各セルの (幅, 高さ上限)。3 枚は先頭セルが左大(全高)になる
pub(super) fn grid_cells(n: usize, avail: f32) -> Vec<(f32, f32)> {
    let cell_w = ((avail - GRID_GAP) / 2.0).max(60.0);
    let half_h = (IMG_MAX_H - GRID_GAP) / 2.0;
    match n {
        0 => Vec::new(),
        1 => vec![(avail, IMG_MAX_H)],
        2 => vec![(cell_w, IMG_MAX_H); 2],
        3 => vec![(cell_w, IMG_MAX_H), (cell_w, half_h), (cell_w, half_h)],
        _ => vec![(cell_w, half_h); n],
    }
}

/// インラインペイン表示のソース(IMG-03・仕様決定 N)。
/// thumbnailUrl 優先、未設定は url にフォールバック
pub(super) fn inline_src(file: &DriveFile) -> Option<&str> {
    file.thumbnail_url.as_deref().or(file.url.as_deref())
}

/// セル内の表示寸法(IMG-02・仕様決定 M)。object-fit: contain 相当で、
/// 小さい原寸はセル幅いっぱいまで拡大し、高さは上限で留める。
/// 原寸情報がないときは None(ロード後の縮小のみに委ねる)
pub(super) fn fit_display_size(file: &DriveFile, cell_w: f32, max_h: f32) -> Option<egui::Vec2> {
    let p = file.properties.as_ref()?;
    let ow = p.width?.max(1) as f32;
    let oh = p.height?.max(1) as f32;
    let scale = (cell_w / ow).min(max_h / oh);
    Some(vec2(ow * scale, oh * scale))
}

/// 画像セルに確保する高さ(IMG-02)。閲覧注意の折りたたみや描画ソースなしは
/// 最小限、原寸既知はフィット後の高さ、原寸不明はロード済みなら実表示
/// サイズ・未ロードは控えめな仮高さ。全高を取らないのは小さい
/// サムネイルで大きな空白が残るため
pub(super) fn cell_estimate_h(
    file: &DriveFile,
    opened: bool,
    cell_w: f32,
    max_h: f32,
    loaded_size: Option<egui::Vec2>,
) -> f32 {
    if file.is_sensitive && !opened {
        return 20.0;
    }
    if inline_src(file).is_none() {
        return 20.0;
    }
    if let Some(s) = fit_display_size(file, cell_w, max_h) {
        return s.y;
    }
    loaded_size
        .map(|s| s.y.min(max_h))
        .unwrap_or_else(|| 120.0_f32.min(max_h))
}

/// 画像セル(F-08-1/-2)。センシティブはクリックで展開(F-08-4)、
/// 通常はアプリ内ビューア(F-08-1)を開く
fn image_cell(
    ui: &mut Ui,
    file: &DriveFile,
    images: &[DriveFile],
    index: usize,
    cell_w: f32,
    max_h: f32,
    ctx: &mut UiCtx<'_>,
) {
    let opened = ctx.card_state.media_open.contains(&file.id);
    // セル幅は描画内容に関係なく確保する: 縦長画像や閲覧注意ボタンが
    // 細いままだと horizontal の次のセルが寄ってグリッドが崩れるため。
    // 高さは原寸不明でも全高を取らず、ロード済みなら実表示サイズ・
    // 未ロードは控えめな仮高さに留める(小さいサムネイルで大きな空白が残るため)。
    // 閲覧注意を展開していないセルではロード呼び出し自体を走らせない
    // (閲覧注意ボタンの表示だけで画像の取得が始まってしまう)
    let loaded_size = if !file.is_sensitive || opened {
        inline_src(file).and_then(|s| {
            egui::Image::new(s)
                .max_size(vec2(cell_w, max_h))
                .load_and_calc_size(ui, vec2(cell_w, max_h))
        })
    } else {
        None
    };
    let est_h = cell_estimate_h(file, opened, cell_w, max_h, loaded_size);
    ui.allocate_ui_with_layout(
        vec2(cell_w, est_h),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            if file.is_sensitive && !opened {
                let resp = ui.add(
                    egui::Button::new(
                        RichText::new(format!("⚠ 閲覧注意: {}", file.name))
                            .size(11.0)
                            .color(Color32::from_rgb(0xf0, 0xa0, 0x80)),
                    )
                    .wrap(),
                );
                ctx.card_state.click_exclusions.push(resp.rect);
                if resp.clicked() {
                    ctx.card_state.media_open.insert(file.id.clone());
                }
                return;
            }
            let Some(src) = inline_src(file) else {
                return;
            };
            let img = match fit_display_size(file, cell_w, max_h) {
                Some(size) => egui::Image::new(src).fit_to_exact_size(size),
                // 原寸不明はロード後のテクスチャの大きさに任せる(縮小のみ)
                None => egui::Image::new(src).max_size(vec2(cell_w, max_h)),
            }
            .corner_radius(4.0);
            // クリックで拡大ビューア(F-08-1)
            let resp = ui.add(img).interact(Sense::click());
            ctx.card_state.click_exclusions.push(resp.rect);
            if resp.clicked() {
                ctx.ops.push(UiOp::OpenViewer {
                    files: images.to_vec(),
                    index,
                    revealed: ctx.card_state.media_open.clone(),
                });
            }
        },
    );
}
