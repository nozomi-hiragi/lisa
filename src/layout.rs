//! 木を「入れ子のリスト」に配置する。
//!
//! フォルダの中身は2列に分かれる。左はファイル、右はフォルダで、どちらも名前順に縦に並ぶ。
//! どの行も左に名前、右に枠がある。フォルダの枠の中にはそのフォルダの中身が同じ規則で
//! 小さく描かれ、ファイルの枠にはプレビューが入る。枠は画面と同じ縦横比なので、枠にズーム
//! すると画面いっぱいに広がる。行の高さは「その枠が画面いっぱいのときに何ピクセルか」で決める。

use bevy::math::{Rect, Vec2};
use bevy::prelude::Resource;

use crate::tree::{FileTree, NodeId};

/// 各ノードの領域（ワールド座標、y は上向き）。
#[derive(Clone, Copy, Default)]
pub struct NodeLayout {
    /// 行全体（クリックの判定に使う）。
    pub row: Rect,
    /// アイコンと名前を書く場所。
    pub label: Rect,
    /// 名前の右の枠。フォルダなら中身、ファイルならプレビューを描く。Root は全体。
    pub frame: Rect,
    /// フォルダの中身の並べ方（フォルダのみ）。
    pub list: ListLayout,
}

/// フォルダの中身の並べ方。
#[derive(Clone, Copy, Default)]
pub struct ListLayout {
    pub row_height: f32,
    /// 1列に入る行の数。
    pub fit: usize,
    /// 左がファイル、右がフォルダの列。
    pub columns: [Rect; 2],
}

/// ファイルの列とフォルダの列。
pub const FILES: usize = 0;
pub const DIRS: usize = 1;

/// 子がどちらの列に入るか。
pub fn column_of(tree: &FileTree, id: NodeId) -> usize {
    if tree.nodes[id].is_dir { DIRS } else { FILES }
}

/// その列に入る子（名前順）。
pub fn column_items(tree: &FileTree, folder: NodeId, column: usize) -> Vec<NodeId> {
    tree.nodes[folder]
        .children
        .iter()
        .copied()
        .filter(|c| column_of(tree, *c) == column)
        .collect()
}

/// 全ノードの配置。
#[derive(Resource)]
pub struct Layouts(pub Vec<NodeLayout>);

/// フォルダの枠のうち、上に出っ張る見出し（タブ）の高さの割合。
const TAB_RATIO: f32 = 0.1;
/// 列の中で、枠に使ってよい幅の割合（残りは名前の欄）。
const MAX_FRAME_RATIO: f32 = 0.45;

/// `root_rect` を Root の枠として木全体を配置する。`aspect` はフォルダの枠の縦横比（幅 / 高さ）。
/// `row_ratio` は行の高さを枠の高さに対する割合で表したもの。
pub fn layout(tree: &FileTree, root_rect: Rect, aspect: f32, row_ratio: f32) -> Vec<NodeLayout> {
    let mut out = vec![NodeLayout::default(); tree.nodes.len()];
    out[FileTree::ROOT] = NodeLayout {
        row: root_rect,
        label: Rect::default(),
        frame: root_rect,
        list: ListLayout::default(),
    };
    place_children(tree, FileTree::ROOT, root_rect, aspect, row_ratio, &mut out);
    out
}

/// フォルダの枠のうち、タブ（左上の出っ張り）。
pub fn folder_tab(folder: Rect) -> Rect {
    let h = folder.height() * TAB_RATIO;
    Rect::new(
        folder.min.x,
        folder.max.y - h,
        folder.min.x + folder.width() * 0.35,
        folder.max.y,
    )
}

/// フォルダの枠のうち、タブを除いた本体。
pub fn folder_body(folder: Rect) -> Rect {
    Rect {
        min: folder.min,
        max: Vec2::new(folder.max.x, folder.max.y - folder.height() * TAB_RATIO),
    }
}

/// フォルダの中身を並べる場所（本体の内側）。
pub fn contents_area(folder: Rect) -> Rect {
    let body = folder_body(folder);
    let pad = folder.height() * 0.04;
    Rect {
        min: body.min + pad,
        max: body.max - pad,
    }
}

fn place_children(
    tree: &FileTree,
    id: NodeId,
    frame: Rect,
    aspect: f32,
    row_ratio: f32,
    out: &mut [NodeLayout],
) {
    let area = contents_area(frame);
    if area.is_empty() {
        return;
    }
    let row_height = frame.height() * row_ratio;
    let fit = ((area.height() / row_height).floor() as usize).max(1);
    let gap = area.width() * 0.03;
    let column_width = (area.width() - gap) / 2.0;
    let columns = [
        Rect::new(
            area.min.x,
            area.min.y,
            area.min.x + column_width,
            area.max.y,
        ),
        Rect::new(
            area.max.x - column_width,
            area.min.y,
            area.max.x,
            area.max.y,
        ),
    ];
    out[id].list = ListLayout {
        row_height,
        fit,
        columns,
    };

    // 枠は行の高さに収め、列の幅の MAX_FRAME_RATIO までにする。縦横比は画面と同じ。
    let mut frame_height = row_height * 0.85;
    if frame_height * aspect > column_width * MAX_FRAME_RATIO {
        frame_height = column_width * MAX_FRAME_RATIO / aspect;
    }
    let frame_width = frame_height * aspect;
    let label_gap = row_height * 0.1;

    for (column, rect) in columns.iter().enumerate() {
        for (i, child) in column_items(tree, id, column).into_iter().enumerate() {
            let top = rect.max.y - row_height * i as f32;
            let row = Rect::new(rect.min.x, top - row_height, rect.max.x, top);
            let center_y = row.center().y;
            let child_frame = Rect::new(
                rect.max.x - frame_width,
                center_y - frame_height / 2.0,
                rect.max.x,
                center_y + frame_height / 2.0,
            );
            let label = Rect::new(
                row.min.x,
                row.min.y,
                child_frame.min.x - label_gap,
                row.max.y,
            );
            out[child] = NodeLayout {
                row,
                label,
                frame: child_frame,
                list: ListLayout::default(),
            };
            if tree.nodes[child].is_dir {
                place_children(tree, child, child_frame, aspect, row_ratio, out);
            }
        }
    }
}
