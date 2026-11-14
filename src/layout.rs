//! 木を「入れ子のリスト」に配置する。
//!
//! フォルダの中身は縦に1行ずつ並ぶ。どの行も左に名前、右に枠がある。フォルダの枠の中には
//! そのフォルダの中身が同じ規則で小さく描かれ、ファイルの枠にはプレビューが入る。
//! 枠はウィンドウと同じ縦横比なので、枠にズームするとウィンドウいっぱいに広がる。

use bevy::math::{Rect, Vec2};

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
}

/// フォルダの枠のうち、上に出っ張る見出し（タブ）の高さの割合。
const TAB_RATIO: f32 = 0.1;
/// 名前の欄に最低限残す幅の割合。
const MIN_LABEL_RATIO: f32 = 0.3;

/// `root_rect` を Root の枠として木全体を配置する。`aspect` はフォルダの枠の縦横比（幅 / 高さ）。
pub fn layout(tree: &FileTree, root_rect: Rect, aspect: f32) -> Vec<NodeLayout> {
    let mut out = vec![NodeLayout::default(); tree.nodes.len()];
    out[FileTree::ROOT] = NodeLayout {
        row: root_rect,
        label: Rect::default(),
        frame: root_rect,
    };
    place_children(
        tree,
        FileTree::ROOT,
        contents_area(root_rect),
        aspect,
        &mut out,
    );
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
fn contents_area(folder: Rect) -> Rect {
    let body = folder_body(folder);
    let pad = folder.height() * 0.04;
    Rect {
        min: body.min + pad,
        max: body.max - pad,
    }
}

fn place_children(tree: &FileTree, id: NodeId, area: Rect, aspect: f32, out: &mut [NodeLayout]) {
    let children = &tree.nodes[id].children;
    if children.is_empty() || area.is_empty() {
        return;
    }

    // 行の高さは均等割り。ただし枠が広がりすぎて名前の欄が狭くならないよう抑える。
    let mut row_height = area.height() / children.len() as f32;
    let max_frame_width = area.width() * (1.0 - MIN_LABEL_RATIO);
    if row_height * 0.9 * aspect > max_frame_width {
        row_height = max_frame_width / aspect / 0.9;
    }
    let frame_height = row_height * 0.9;
    let frame_width = frame_height * aspect;
    let gap = row_height * 0.05;

    for (i, &child) in children.iter().enumerate() {
        let top = area.max.y - row_height * i as f32;
        let row = Rect::new(area.min.x, top - row_height, area.max.x, top);
        let center_y = row.center().y;
        let frame = Rect::new(
            area.max.x - frame_width,
            center_y - frame_height / 2.0,
            area.max.x,
            center_y + frame_height / 2.0,
        );
        let label = Rect::new(row.min.x, row.min.y, frame.min.x - gap, row.max.y);
        out[child] = NodeLayout { row, label, frame };
        if tree.nodes[child].is_dir {
            place_children(tree, child, contents_area(frame), aspect, out);
        }
    }
}
