//! 木を「入れ子のタイル」に配置する。
//!
//! フォルダの中身は、フォルダとファイルをまとめて名前順に、左上から横へ並べたタイルにする。
//! スクロールはせず、全部をそのフォルダの本体の中に収める。列の数は、タイルができるだけ大きく
//! なり、余るマスが少なくなるように選ぶ。
//!
//! タイルの中は、フォルダなら上に名前の入った耳（タブ）、その下に本体（A4 の横長、√2:1）。
//! 本体の中には、そのフォルダの中身が同じ規則で小さく描かれる。ファイルなら上にプレビュー
//! （4:3、高さはフォルダの本体と同じ）、その下に名前。耳と名前の大きさはズームに合わせて変わる
//! （`render/ear.rs`）。

use bevy::math::{Rect, Vec2};
use bevy::prelude::Resource;

use crate::model::tree::{FileTree, NodeId};

/// 各ノードの領域（ワールド座標、y は上向き）。
#[derive(Clone, Copy, Default)]
pub struct NodeLayout {
    /// タイル全体（クリックの判定に使う）。
    pub cell: Rect,
    /// 開いたときの本体。フォルダなら中身を描く √2:1（耳を除く）、ファイルならプレビューを描く 4:3。
    /// 一覧の中では、耳や名前が大きくなる分、これより縮んで描かれる。
    /// Root は全体。
    pub frame: Rect,
    /// 開いたときに画面に収める四角（タイルの枠。本体と、開いたときの耳か名前）。
    pub fit: Rect,
    /// フォルダの中身の並べ方（フォルダのみ）。
    pub grid: GridLayout,
}

/// フォルダの中身の並べ方。
#[derive(Clone, Copy, Default)]
pub struct GridLayout {
    /// 列の数。
    pub columns: usize,
    /// 中の子フォルダの本体の幅。
    pub body_width: f32,
    /// 中の子の耳（ファイルは名前の欄）の高さ。このフォルダを開いたときの大きさ。
    pub ear: f32,
}

/// 全ノードの配置。
#[derive(Resource)]
pub struct Layouts(pub Vec<NodeLayout>);

/// フォルダの本体（耳を除いた四角）の縦横比（幅 / 高さ）。A4 の横長。
pub const BODY_ASPECT: f32 = std::f32::consts::SQRT_2;

/// ファイルのプレビューの縦横比（幅 / 高さ）。高さはフォルダの本体と同じ。
pub const PREVIEW_ASPECT: f32 = 4.0 / 3.0;

/// 開いたフォルダを画面に収めるときの余白（倍率）。
pub const FIT_MARGIN: f32 = 1.03;

/// 一覧の中の耳（ファイルは名前）の高さの上限（タイルの高さに対する割合）。
const MAX_EAR_IN_CELL: f32 = 0.4;
/// タイルどうしの隙間（タイルの幅に対する割合）。
const GAP: f32 = 0.08;
/// タイルの幅の上限（並べる場所の幅に対する割合）。中身が少なくても、入れ子に見えるように。
const MAX_CELL: f32 = 0.5;

/// 開いたフォルダを画面に収めたときの、本体の大きさ（論理ピクセル）。
/// `view` はメニューバーの下の領域の大きさ、`ear_px` は開いたフォルダの耳の高さ。
/// 本体の上に耳の分を取り、全体を余白つきで収める。
pub fn fitted_size(view: Vec2, ear_px: f32) -> Vec2 {
    let width = (view.x / FIT_MARGIN)
        .min((view.y / FIT_MARGIN - ear_px) * BODY_ASPECT)
        .max(1.0);
    Vec2::new(width, width / BODY_ASPECT)
}

/// 耳の大きさの設定を、開いたフォルダの本体の高さ `fitted_height` に対する割合に直したもの。
#[derive(Clone, Copy)]
pub struct EarSpec {
    /// 一覧の中の耳の高さ。
    pub preview: f32,
    /// そのフォルダを開いたときの耳の高さ（自分の本体の高さに対する割合）。
    pub selected: f32,
}

impl EarSpec {
    pub fn new(settings: &crate::settings::Settings, fitted_height: f32) -> Self {
        Self {
            preview: settings.ear_preview_px / fitted_height,
            selected: settings.ear_selected_px / fitted_height,
        }
    }
}

/// `root_rect` を Root の本体として木全体を配置する。
pub fn layout(tree: &FileTree, root_rect: Rect, ears: EarSpec) -> Vec<NodeLayout> {
    let mut out = vec![NodeLayout::default(); tree.nodes.len()];
    out[FileTree::ROOT] = NodeLayout {
        cell: root_rect,
        frame: root_rect,
        fit: with_ear_space(root_rect, ears),
        grid: GridLayout::default(),
    };
    place_children(tree, FileTree::ROOT, root_rect, ears, &mut out);
    out
}

/// Root の枠: 本体の上に、開いたときの耳の分を足した四角。
fn with_ear_space(body: Rect, ears: EarSpec) -> Rect {
    Rect {
        min: body.min,
        max: body.max + Vec2::new(0.0, body.height() * ears.selected),
    }
}

/// フォルダの中身を並べる場所（本体の内側）。
pub fn contents_area(body: Rect) -> Rect {
    let pad = body.height() * 0.04;
    Rect {
        min: body.min + pad,
        max: body.max - pad,
    }
}

/// `count` 個のタイル（幅に対する高さが `cell_ratio`）を `area` に並べるときの列の数と、
/// タイルの幅。タイルが最も大きくなる列の数のうち、余るマスが少ないものを選ぶ。
pub fn choose_grid(count: usize, area: Vec2, cell_ratio: f32) -> (usize, f32) {
    let mut best = (1, 0.0_f32, usize::MAX);
    for columns in 1..=count.max(1) {
        let rows = count.div_ceil(columns).max(1);
        let by_width = area.x / (columns as f32 + (columns - 1) as f32 * GAP);
        let by_height = area.y / (rows as f32 * cell_ratio + (rows - 1) as f32 * GAP);
        let width = by_width.min(by_height).min(area.x * MAX_CELL);
        let empty = columns * rows - count;
        // ほぼ同じ大きさなら、余るマスが少ないほう。
        let better = width > best.1 * 1.01 || (width > best.1 * 0.99 && empty < best.2);
        if better {
            best = (columns, width, empty);
        }
    }
    (best.0, best.1)
}

fn place_children(tree: &FileTree, id: NodeId, frame: Rect, ears: EarSpec, out: &mut [NodeLayout]) {
    let children = &tree.nodes[id].children;
    let area = contents_area(frame);
    if area.is_empty() || children.is_empty() {
        return;
    }
    // タイル（枠）は、本体と、開いたときの耳（ファイルは名前）の分。一覧の中では耳が大きくなるので、
    // 本体はその分だけ縮んで譲る（`render/ear.rs` の `yielded`）。
    let cell_ratio = (1.0 + ears.selected) / BODY_ASPECT;
    let (columns, width) = choose_grid(children.len(), area.size(), cell_ratio);
    let rows = children.len().div_ceil(columns);
    let body_height = width / BODY_ASPECT;
    let cell_height = width * cell_ratio;
    let gap = width * GAP;
    let pitch = Vec2::new(width + gap, cell_height + gap);
    let ear = (frame.height() * ears.preview).min(cell_height * MAX_EAR_IN_CELL);
    out[id].grid = GridLayout {
        columns,
        body_width: width,
        ear,
    };

    // 並べたタイル全体を、並べる場所の真ん中に置く。
    let used = Vec2::new(columns as f32 * pitch.x - gap, rows as f32 * pitch.y - gap);
    let origin = Vec2::new(
        area.center().x - used.x / 2.0,
        area.center().y + used.y / 2.0,
    );

    for (i, &child) in children.iter().enumerate() {
        let (col, row) = ((i % columns) as f32, (i / columns) as f32);
        let top_left = origin + Vec2::new(col * pitch.x, -row * pitch.y);
        let cell = Rect::new(
            top_left.x,
            top_left.y - cell_height,
            top_left.x + width,
            top_left.y,
        );
        // 開いたときの形: フォルダは上に耳、その下に本体。ファイルは上にプレビュー（4:3、横は真ん中）、
        // その下に名前。どちらも枠いっぱいで、開いたときは枠を画面に収める。
        let body = if tree.nodes[child].is_dir {
            Rect::new(cell.min.x, cell.min.y, cell.max.x, cell.min.y + body_height)
        } else {
            let preview_width = body_height * PREVIEW_ASPECT;
            let x = cell.center().x - preview_width / 2.0;
            Rect::new(x, cell.max.y - body_height, x + preview_width, cell.max.y)
        };
        out[child] = NodeLayout {
            cell,
            frame: body,
            fit: cell,
            grid: GridLayout::default(),
        };
        if tree.nodes[child].is_dir {
            place_children(tree, child, body, ears, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_fills_without_gaps_when_it_can() {
        // 6 個を横長の場所に並べるなら 3 列 × 2 行（余りなし）。
        let ratio = 1.1 / BODY_ASPECT;
        let (columns, _) = choose_grid(6, Vec2::new(3.0, 2.0 * ratio + GAP), ratio);
        assert_eq!(columns, 3);
    }

    #[test]
    fn single_item_is_capped() {
        let (columns, width) = choose_grid(1, Vec2::new(100.0, 100.0), 0.5);
        assert_eq!(columns, 1);
        assert!(width <= 100.0 * MAX_CELL + 1e-3);
    }
}
