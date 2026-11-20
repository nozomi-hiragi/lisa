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
//!
//! 木は深いので、全部を 1 つの座標で並べると f32 の精度が尽きる（1 段で数分の 1 になる）。
//! そこで、ある 1 つのフォルダ（基準、`Layouts::anchor`）の本体を決まった大きさに置き、その周り
//! だけを並べる: 基準から Root までの祖先と、その祖先の子（隣）、基準の祖父母から下の数段。
//! 並べ方は相似なので（大きさに比例するだけ）、子の位置から親の位置を逆に求められる。
//! 開く場所が変わって止まったら、基準を付け替える（`view.rs` の `reanchor`）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Index;

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

/// 基準のフォルダの周りの配置。
#[derive(Resource)]
pub struct Layouts {
    /// 基準のフォルダ。本体が `ANCHOR_BODY` に置かれている。
    pub anchor: NodeId,
    map: HashMap<NodeId, NodeLayout>,
}

/// 並べていないノードを引いたときの値（大きさ 0 なので、描いても見えず、クリックにも当たらない）。
const MISSING: NodeLayout = NodeLayout {
    cell: Rect {
        min: Vec2::ZERO,
        max: Vec2::ZERO,
    },
    frame: Rect {
        min: Vec2::ZERO,
        max: Vec2::ZERO,
    },
    fit: Rect {
        min: Vec2::ZERO,
        max: Vec2::ZERO,
    },
    grid: GridLayout {
        columns: 0,
        body_width: 0.0,
        ear: 0.0,
    },
};

impl Index<NodeId> for Layouts {
    type Output = NodeLayout;

    fn index(&self, id: NodeId) -> &NodeLayout {
        self.map.get(&id).unwrap_or(&MISSING)
    }
}

impl Layouts {
    /// 並べてあるか。
    pub fn contains(&self, id: NodeId) -> bool {
        self.map.contains_key(&id)
    }

    /// 探索予算の外でも、必要になったフォルダの直下だけを補う。
    pub fn ensure_children(&mut self, tree: &FileTree, folder: NodeId, ears: EarSpec) -> bool {
        let Some(node) = tree.nodes.get(folder) else {
            return false;
        };
        let frame = self[folder].frame;
        if !node.is_dir || !valid_frame(frame) || node.children.iter().all(|&id| self.contains(id))
        {
            return false;
        }
        let (grid, children) = place(tree, folder, frame, ears);
        self.map.get_mut(&folder).expect("配置済みのフォルダ").grid = grid;
        let mut inserted = false;
        for (id, entry) in children {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.map.entry(id) {
                slot.insert(entry);
                inserted = true;
            }
        }
        inserted
    }

    /// 配置済みの先祖から道のりを補い、最後のフォルダの直下まで使えるようにする。
    pub fn ensure_path(&mut self, tree: &FileTree, id: NodeId, ears: EarSpec) {
        if self.contains(id) {
            self.ensure_children(tree, id, ears);
            return;
        }
        if let Some(path) = tree.path_from_root(id)
            && let Some(start) = path.iter().rposition(|&folder| self.contains(folder))
        {
            for &folder in &path[start..] {
                self.ensure_children(tree, folder, ears);
            }
        }
    }

    /// 並べてあるノード。
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.map.keys().copied()
    }
}

pub(crate) fn valid_frame(rect: Rect) -> bool {
    let size = rect.size();
    rect.min.is_finite() && rect.max.is_finite() && size.is_finite() && size.min_element() > 0.0
}

/// 二つの基準矩形の間の相似変換。差を取ってから変換するまで f64 のまま扱う。
#[derive(Clone, Copy)]
pub(crate) struct Rebase {
    old_center: bevy::math::DVec2,
    new_center: bevy::math::DVec2,
    factor: f64,
}

impl Rebase {
    pub(crate) fn new(old: Rect, new: Rect) -> Option<Self> {
        if !valid_frame(old) || !valid_frame(new) {
            return None;
        }
        let center = |rect: Rect| (rect.min.as_dvec2() + rect.max.as_dvec2()) * 0.5;
        let height = |rect: Rect| f64::from(rect.max.y) - f64::from(rect.min.y);
        Some(Self {
            old_center: center(old),
            new_center: center(new),
            factor: height(new) / height(old),
        })
    }

    pub(crate) fn position(self, pos: Vec2) -> Vec2 {
        ((pos.as_dvec2() - self.old_center) * self.factor + self.new_center).as_vec2()
    }

    pub(crate) fn scale(self, scale: f32) -> f32 {
        (f64::from(scale) * self.factor) as f32
    }
    pub(crate) fn vector(self, vector: Vec2) -> Vec2 {
        (vector.as_dvec2() * self.factor).as_vec2()
    }
    pub(crate) fn log_scale(self, ln: f32) -> f32 {
        (f64::from(ln) + self.factor.ln()) as f32
    }
}

/// 基準のフォルダの本体の高さ（ワールド座標）。
pub const ANCHOR_HEIGHT: f32 = 1000.0;

/// 基準のフォルダの本体を置く四角。原点を中心にした √2:1。
pub fn anchor_body() -> Rect {
    Rect::from_center_size(
        Vec2::ZERO,
        Vec2::new(ANCHOR_HEIGHT * BODY_ASPECT, ANCHOR_HEIGHT),
    )
}

/// 基準より何段下まで並べるか。これより下は、画像の中でも見分けられないほど小さい。
pub const DEPTH_BELOW: usize = 4;

/// 並べる数の上限（祖先とその子は、これを超えても並べる）。
pub const MAX_LAID_OUT: usize = 4000;

/// フォルダの本体（耳を除いた四角）の縦横比（幅 / 高さ）。A4 の横長。
pub const BODY_ASPECT: f32 = std::f32::consts::SQRT_2;

/// ファイルのプレビューの縦横比（幅 / 高さ）。高さはフォルダの本体と同じ。
pub const PREVIEW_ASPECT: f32 = 4.0 / 3.0;

/// 開いたフォルダを画面に収めるときの余白（倍率）。
pub const FIT_MARGIN: f32 = 1.03;

/// 一覧の中の耳（ファイルは名前）の高さの上限（タイルの高さに対する割合）。
pub(crate) const MAX_EAR_IN_CELL: f32 = 0.4;
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
    pub fn new(settings: &crate::settings::Settings) -> Self {
        const REF_FITTED_HEIGHT: f32 = 720.0;
        Self {
            preview: settings.ear_preview_px / REF_FITTED_HEIGHT,
            selected: settings.ear_selected_px / REF_FITTED_HEIGHT,
        }
    }
}

/// `anchor` の本体を `body` に置いて、その周りを並べる。
///
/// 並べるのは、Root から `anchor` までの祖先とその子（隣）と、`anchor` の祖父母（無ければ一番上の
/// 祖先）から下で、`anchor` より `DEPTH_BELOW` 段下までのもの（近い順に `MAX_LAID_OUT` まで）。
pub fn layout(tree: &FileTree, anchor: NodeId, body: Rect, ears: EarSpec) -> Layouts {
    // 祖先の本体を、子の位置から逆に求める（並べ方は大きさに比例するだけ）。
    let mut chain = vec![(anchor, body)];
    let unit = Rect::new(0.0, 0.0, BODY_ASPECT, 1.0);
    let mut min = body.min.as_dvec2();
    let mut height = f64::from(body.max.y) - f64::from(body.min.y);
    while let Some(parent) = tree.nodes[chain.last().unwrap().0].parent {
        let child = chain.last().unwrap().0;
        let (_, placed) = place(tree, parent, unit, ears);
        let Some(in_unit) = placed
            .iter()
            .find(|(id, _)| *id == child)
            .map(|(_, l)| l.frame)
        else {
            break;
        };
        height /= f64::from(in_unit.height());
        min -= in_unit.min.as_dvec2() * height;
        let max = min + unit.max.as_dvec2() * height;
        if !height.is_finite()
            || height > 1e30
            || !min.is_finite()
            || !max.is_finite()
            || min.abs().max(max.abs()).max_element() > 1e30
        {
            break;
        }
        let parent_body = Rect::from_corners(min.as_vec2(), max.as_vec2());
        if !valid_frame(parent_body) {
            break;
        }
        chain.push((parent, parent_body));
    }
    chain.reverse();

    // 祖先の本体は、Root から並べ直さずに、ここで求めたものを使う（Root から順に並べると、
    // 大きな座標から小さな差を取ることになって精度が尽きる）。
    let precise: HashMap<NodeId, Rect> = chain.iter().copied().collect();
    let mut map = HashMap::new();
    let (top, top_body) = chain[0];
    map.insert(
        top,
        NodeLayout {
            cell: top_body,
            frame: top_body,
            fit: with_ear_space(top_body, ears),
            grid: GridLayout::default(),
        },
    );
    // 祖先は、子を 1 段ずつ並べる（隣が見える）。
    for &(id, _) in &chain {
        place_into(tree, id, ears, &precise, &mut map);
    }
    // そこから下を、基準の子、隣（親の子）、親の隣（祖父母の子）の順に、近いものから数段並べる。
    // 中身の多いフォルダが並んでいても重くならないよう、並べる数に上限を付ける。
    let until = tree.nodes[anchor].depth + DEPTH_BELOW;
    let mut done: HashSet<NodeId> = chain.iter().map(|&(id, _)| id).collect();
    let mut queue: VecDeque<NodeId> = chain
        .iter()
        .rev()
        .take(3)
        .flat_map(|&(id, _)| tree.nodes[id].children.iter().copied())
        .collect();
    while let Some(id) = queue.pop_front() {
        let node = &tree.nodes[id];
        if !node.is_dir || node.depth >= until || !done.insert(id) {
            continue;
        }
        if map.len() >= MAX_LAID_OUT {
            break;
        }
        place_into(tree, id, ears, &precise, &mut map);
        queue.extend(tree.nodes[id].children.iter().copied());
    }
    let mut layouts = Layouts { anchor, map };
    layouts.ensure_children(tree, anchor, ears);
    if let Some(parent) = tree.nodes[anchor].parent {
        layouts.ensure_children(tree, parent, ears);
    }
    layouts
}

/// `id` の子を並べて `map` に入れる（`id` はもう並べてあること）。`precise` にある祖先は、
/// その本体の位置に合わせる。
fn place_into(
    tree: &FileTree,
    id: NodeId,
    ears: EarSpec,
    precise: &HashMap<NodeId, Rect>,
    map: &mut HashMap<NodeId, NodeLayout>,
) {
    let frame = precise.get(&id).copied().unwrap_or(map[&id].frame);
    let (grid, placed) = place(tree, id, frame, ears);
    if let Some(l) = map.get_mut(&id) {
        l.grid = grid;
    }
    for (child, mut layout) in placed {
        if let Some(body) = precise.get(&child) {
            let shift = body.min - layout.frame.min;
            layout.cell = shifted(layout.cell, shift);
            layout.fit = shifted(layout.fit, shift);
            layout.frame = *body;
        }
        let grid = map.get(&child).map_or(GridLayout::default(), |l| l.grid);
        map.insert(child, NodeLayout { grid, ..layout });
    }
}

fn shifted(r: Rect, by: Vec2) -> Rect {
    Rect::from_corners(r.min + by, r.max + by)
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

/// フォルダ `id` の本体を `frame` に置いたときの、中身の並べ方と、子の配置（子の `grid` は空）。
fn place(
    tree: &FileTree,
    id: NodeId,
    frame: Rect,
    ears: EarSpec,
) -> (GridLayout, Vec<(NodeId, NodeLayout)>) {
    if !valid_frame(frame) {
        return (GridLayout::default(), Vec::new());
    }
    let unit = Rect::new(0.0, 0.0, BODY_ASPECT, 1.0);
    let (mut grid, mut children) = place_unit(tree, id, unit, ears);
    let scale = f64::from(frame.height());
    let origin = frame.min.as_dvec2();
    let transform = |rect: Rect| {
        Rect::from_corners(
            (origin + rect.min.as_dvec2() * scale).as_vec2(),
            (origin + rect.max.as_dvec2() * scale).as_vec2(),
        )
    };
    grid.body_width *= scale as f32;
    grid.ear *= scale as f32;
    for (_, child) in &mut children {
        child.cell = transform(child.cell);
        child.frame = transform(child.frame);
        child.fit = transform(child.fit);
    }
    (grid, children)
}

fn place_unit(
    tree: &FileTree,
    id: NodeId,
    frame: Rect,
    ears: EarSpec,
) -> (GridLayout, Vec<(NodeId, NodeLayout)>) {
    let children = &tree.nodes[id].children;
    let area = contents_area(frame);
    if area.is_empty() || children.is_empty() {
        return (GridLayout::default(), Vec::new());
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
    let grid = GridLayout {
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

    let placed = children
        .iter()
        .enumerate()
        .map(|(i, &child)| {
            let (col, row) = ((i % columns) as f32, (i / columns) as f32);
            let top_left = origin + Vec2::new(col * pitch.x, -row * pitch.y);
            let cell = Rect::new(
                top_left.x,
                top_left.y - cell_height,
                top_left.x + width,
                top_left.y,
            );
            // 開いたときの形: フォルダは上に耳、その下に本体。ファイルは上にプレビュー（4:3、横は
            // 真ん中）、その下に名前。どちらも枠いっぱいで、開いたときは枠を画面に収める。
            let body = if tree.nodes[child].is_dir {
                Rect::new(cell.min.x, cell.min.y, cell.max.x, cell.min.y + body_height)
            } else {
                let preview_width = body_height * PREVIEW_ASPECT;
                let x = cell.center().x - preview_width / 2.0;
                Rect::new(x, cell.max.y - body_height, x + preview_width, cell.max.y)
            };
            (
                child,
                NodeLayout {
                    cell,
                    frame: body,
                    fit: cell,
                    grid: GridLayout::default(),
                },
            )
        })
        .collect();
    (grid, placed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EARS: EarSpec = EarSpec {
        preview: 0.02,
        selected: 0.03,
    };

    /// `a` を `from` から `to` へ移す相似変換で写した四角。
    fn map(r: Rect, from: Rect, to: Rect) -> Rect {
        let k = to.height() / from.height();
        Rect::from_corners(
            to.min + (r.min - from.min) * k,
            to.min + (r.max - from.min) * k,
        )
    }

    fn close(a: Rect, b: Rect, tolerance: f32) -> bool {
        a.min.distance(b.min) <= tolerance && a.max.distance(b.max) <= tolerance
    }

    #[test]
    fn anchored_layout_is_the_same_shape() {
        // Root を基準にした配置と、深いフォルダを基準にした配置は、相似変換で重なる。
        let tree = FileTree::dummy();
        let whole = layout(&tree, FileTree::ROOT, anchor_body(), EARS);
        let scan = (0..tree.nodes.len())
            .find(|&i| tree.nodes[i].name == "スキャン")
            .unwrap();
        let near = layout(&tree, scan, anchor_body(), EARS);
        assert_eq!(near.anchor, scan);
        assert!(close(near[scan].frame, anchor_body(), 1e-2));
        for id in near.ids().filter(|id| whole.contains(*id)) {
            let expected = map(whole[id].cell, whole[scan].frame, anchor_body());
            let tolerance = near[id].cell.width().max(1.0) * 1e-3;
            assert!(
                close(near[id].cell, expected, tolerance),
                "{}: {:?} != {:?}",
                tree.nodes[id].name,
                near[id].cell,
                expected
            );
        }
    }

    #[test]
    fn deep_anchor_keeps_precision() {
        // 30 段潜っても、基準の周りの大きさは基準の大きさのまま。
        let tree = FileTree::deep(30);
        let bottom = (0..tree.nodes.len())
            .find(|&i| tree.nodes[i].name == "底")
            .unwrap();
        let near = layout(&tree, bottom, anchor_body(), EARS);
        assert!(close(near[bottom].frame, anchor_body(), 1e-2));
        let file = tree.nodes[bottom].children[0];
        assert!(near[file].frame.width() > 10.0);
        // 親と隣も並んでいる。
        let parent = tree.nodes[bottom].parent.unwrap();
        assert!(near.contains(parent));
        for &sibling in &tree.nodes[parent].children {
            assert!(near[sibling].cell.width() > 100.0);
        }
        // Root まで並んでいて、大きさは有限。
        assert!(near.contains(FileTree::ROOT));
        assert!(near[FileTree::ROOT].frame.width().is_finite());
    }

    #[test]
    fn missing_children_can_be_added_after_the_budget_is_exhausted() {
        let tree = FileTree::wide(3, 5_001);
        let mut laid = layout(&tree, FileTree::ROOT, anchor_body(), EARS);
        let child = *tree.nodes[FileTree::ROOT].children.last().unwrap();
        let old_frame = laid[child].frame;
        assert!(
            tree.nodes[child]
                .children
                .iter()
                .any(|&id| !laid.contains(id))
        );
        assert!(laid.ensure_children(&tree, child, EARS));
        assert_eq!(laid[child].frame, old_frame);
        assert!(
            tree.nodes[child]
                .children
                .iter()
                .all(|&id| valid_frame(laid[id].frame))
        );
        assert!(!laid.ensure_children(&tree, child, EARS));
    }

    #[test]
    fn very_remote_ancestors_are_truncated_without_nonfinite_rectangles() {
        let tree = FileTree::deep(160);
        let bottom = tree
            .nodes
            .iter()
            .position(|node| node.name == "底")
            .unwrap();
        let laid = layout(&tree, bottom, anchor_body(), EARS);
        assert!(!laid.contains(FileTree::ROOT));
        assert!(laid.ids().all(|id| valid_frame(laid[id].frame)));
        assert!(
            tree.nodes[bottom]
                .children
                .iter()
                .all(|&id| laid.contains(id))
        );
    }

    #[test]
    fn relayout_keeps_the_active_body_and_children_when_other_counts_change() {
        let mut tree = FileTree::deep(4);
        let active = tree
            .nodes
            .iter()
            .position(|node| node.name == "底")
            .unwrap();
        let old = layout(&tree, FileTree::ROOT, anchor_body(), EARS);
        let frame = old[active].frame;
        let parent = tree.nodes[active].parent.unwrap();
        tree.nodes[parent].children.retain(|&id| id == active);
        let new = layout(&tree, active, frame, EARS);
        assert_eq!(new[active].frame, frame);
        for &child in &tree.nodes[active].children {
            assert_eq!(new[child].cell, old[child].cell);
            assert_eq!(new[child].frame, old[child].frame);
            assert_eq!(new[child].fit, old[child].fit);
        }
    }

    #[test]
    fn many_children_stay_within_the_budget() {
        let tree = FileTree::wide(20, 400);
        let laid = layout(&tree, FileTree::ROOT, anchor_body(), EARS);
        // Root とその子は必ず並べ、そこから下は上限まで。
        assert!(laid.ids().count() <= MAX_LAID_OUT + 400);
        assert!(laid.ids().count() >= 20);
    }

    #[test]
    fn only_the_surroundings_are_laid_out() {
        let tree = FileTree::deep(30);
        let whole = layout(&tree, FileTree::ROOT, anchor_body(), EARS);
        // Root から DEPTH_BELOW 段下まで（各段 6 個 + Root）。
        assert!(whole.ids().count() <= 1 + 6 * DEPTH_BELOW + 1);
        assert!(whole.ids().all(|id| tree.nodes[id].depth <= DEPTH_BELOW));
    }

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
