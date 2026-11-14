//! 木を入れ子の四角（squarified treemap）に配置する。

use bevy::math::Rect;

use crate::tree::{FileTree, NodeId};

/// 各ノードの領域（ワールド座標、y は上向き）。
pub struct NodeLayout {
    /// ノード全体の四角。
    pub rect: Rect,
    /// 名前を書く上端の帯の高さ。
    pub label_height: f32,
    /// 四角の内側の余白。
    pub padding: f32,
}

pub fn layout(tree: &FileTree, root_rect: Rect) -> Vec<NodeLayout> {
    let mut out: Vec<NodeLayout> = (0..tree.nodes.len())
        .map(|_| NodeLayout {
            rect: Rect::default(),
            label_height: 0.0,
            padding: 0.0,
        })
        .collect();
    place(tree, FileTree::ROOT, root_rect, &mut out);
    out
}

fn place(tree: &FileTree, id: NodeId, rect: Rect, out: &mut [NodeLayout]) {
    let short = rect.width().min(rect.height());
    let padding = short * 0.015;
    let label_height = (rect.height() * 0.08).min(rect.width() * 0.08);
    out[id] = NodeLayout {
        rect,
        label_height,
        padding,
    };

    let node = &tree.nodes[id];
    if node.children.is_empty() {
        return;
    }
    let inner = Rect {
        min: rect.min + padding,
        max: rect.max - padding - bevy::math::Vec2::new(0.0, label_height),
    };
    if inner.width() <= 0.0 || inner.height() <= 0.0 {
        return;
    }

    let mut children = node.children.clone();
    children.sort_by(|a, b| tree.nodes[*b].size.cmp(&tree.nodes[*a].size));
    let sizes: Vec<f64> = children
        .iter()
        .map(|c| tree.nodes[*c].size.max(1) as f64)
        .collect();
    let gap = padding * 0.5;
    for (child, child_rect) in children.iter().zip(squarify(&sizes, inner)) {
        let shrunk = Rect {
            min: child_rect.min + gap,
            max: child_rect.max - gap,
        };
        // 細すぎて余白で潰れる場合は余白なしで置く。
        let shrunk = if shrunk.is_empty() {
            child_rect
        } else {
            shrunk
        };
        place(tree, *child, shrunk, out);
    }
}

/// 大きい順に並んだ `sizes` を、面積が比例するように `rect` の中へ敷き詰める。
fn squarify(sizes: &[f64], rect: Rect) -> Vec<Rect> {
    let total: f64 = sizes.iter().sum();
    let scale = (rect.width() as f64 * rect.height() as f64) / total;
    let areas: Vec<f64> = sizes.iter().map(|s| s * scale).collect();

    let mut result = Vec::with_capacity(areas.len());
    let mut free = rect;
    let mut i = 0;
    while i < areas.len() {
        let short = free.width().min(free.height()) as f64;
        let mut j = i + 1;
        while j < areas.len() && worst(&areas[i..=j], short) <= worst(&areas[i..j], short) {
            j += 1;
        }
        let row = &areas[i..j];
        let row_sum: f64 = row.iter().sum();

        if free.width() >= free.height() {
            // 左端に縦の列を置く。
            let width = (row_sum / free.height() as f64) as f32;
            let mut y = free.max.y;
            for a in row {
                let h = (*a / width as f64) as f32;
                result.push(Rect::new(free.min.x, y - h, free.min.x + width, y));
                y -= h;
            }
            free.min.x += width;
        } else {
            // 上端に横の行を置く。
            let height = (row_sum / free.width() as f64) as f32;
            let mut x = free.min.x;
            for a in row {
                let w = (*a / height as f64) as f32;
                result.push(Rect::new(x, free.max.y - height, x + w, free.max.y));
                x += w;
            }
            free.max.y -= height;
        }
        i = j;
    }
    result
}

/// 行の中でいちばん細長い四角の縦横比。
fn worst(row: &[f64], short: f64) -> f64 {
    let sum: f64 = row.iter().sum();
    let max = row.iter().cloned().fold(f64::MIN, f64::max);
    let min = row.iter().cloned().fold(f64::MAX, f64::min);
    let s2 = short * short;
    let sum2 = sum * sum;
    (s2 * max / sum2).max(sum2 / (s2 * min))
}
