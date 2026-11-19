//! ピンチで止まる段（フォルダやファイルを画面に収める大きさと、フォルダの中を拡大して見る段）と、
//! フォルダの中を拡大して見ているときの、タイルの切れ目に合わせた位置。

use bevy::math::Rect;
use bevy::prelude::*;

use crate::model::tree::NodeId;
use crate::ui::menu::BAR_PX;
use crate::view::{SceneMaker, fit};

/// ピンチ（ホイール）でこれだけ拡大・縮小して離したら（倍率の比の対数）、次の段へ進む
/// （次の段がこれより近ければ、その半分で進む）。子の大きさに関係なく、ピンチに使った幅で決める。
const ZOOM_COMMIT_LN: f32 = 0.18;

/// ピンチで止まる段。フォルダやファイルを画面に収める大きさ（`grid` が false）か、フォルダの中を
/// 拡大して見る段（`grid` が true。短い辺にタイルがちょうど収まり、切れ目に合わせて止まる）。
#[derive(Clone, Copy, Debug)]
pub(super) struct ZoomGoal {
    /// 止まったときに開いているもの。
    pub(super) node: NodeId,
    pub(super) scale: f32,
    pub(super) grid: bool,
}

/// 中を拡大して見る段の数の上限。
const MAX_GRID_LEVELS: usize = 1000;

/// フォルダの中を拡大して見るときに止まれる倍率（小さい順）。ウィンドウの短い辺の向きに、タイルが
/// 見切れずにちょうど k 個（隙間込み）収まる倍率を、k = 2 から、全体を収めたときの個数より 1 少ない
/// 数まで並べる。全体や 1 つ（子を収めた大きさ）とほとんど同じ大きさのものは除く。
pub(super) fn grid_levels(maker: &SceneMaker, window: &Window, folder: NodeId) -> Vec<f32> {
    let node = &maker.tree.nodes[folder];
    let Some(&first) = node.children.first() else {
        return Vec::new();
    };
    if !node.is_dir {
        return Vec::new();
    }
    let cell = maker.layouts[first].cell;
    let columns = maker.layouts[folder].grid.columns.max(1);
    let gap = node
        .children
        .get(1)
        .filter(|_| columns > 1)
        .map_or(0.0, |c| maker.layouts[*c].cell.min.x - cell.max.x);
    let pitch = Vec2::new(cell.width() + gap, cell.height() + gap);
    let view = Vec2::new(crate::view::view_width(window), window.height() - BAR_PX);
    let (length, pitch_along) = if view.y <= view.x {
        (view.y, pitch.y)
    } else {
        (view.x, pitch.x)
    };
    // 子がまだ並べていない（大きさ 0）なら、段は無い（割ると個数が無限になって固まる）。
    if !(pitch_along > 0.0 && length > 0.0) {
        return Vec::new();
    }
    let whole = fit(maker.layouts[folder].fit, window).1;
    let one = fit(maker.layouts[first].fit, window).1;
    // 全体を収めたとき、短い辺の向きに並ぶ個数（多すぎても段は見分けられないので上限を付ける）。
    let most = ((length * whole / pitch_along).floor() as usize).min(MAX_GRID_LEVELS);
    (2..most.max(2))
        .map(|k| k as f32 * pitch_along / length)
        .filter(|s| *s < whole * 0.95 && *s > one * 1.1)
        .collect()
}

/// ピンチで止まれる段（倍率の小さい順）。今のものを中心に、カーソルの下（近く）の子、今のものの中を
/// 拡大して見る段（`grid_levels`）、今のもの、親の中を拡大して見る段、親。
pub(super) fn zoom_ladder(
    top: NodeId,
    maker: &SceneMaker,
    window: &Window,
    point: Vec2,
) -> Vec<ZoomGoal> {
    let fit_goal = |node: NodeId| -> Option<ZoomGoal> {
        let rect = maker.layouts[node].fit;
        if !(rect.width() > 0.0 && rect.height() > 0.0) {
            return None;
        }
        let scale = fit(rect, window).1;
        (scale > 0.0 && scale.is_finite()).then_some(ZoomGoal {
            node,
            scale,
            grid: false,
        })
    };
    let grid_goals = |node: NodeId| {
        grid_levels(maker, window, node)
            .into_iter()
            .filter(|s| *s > 0.0 && s.is_finite())
            .map(move |scale| ZoomGoal {
                node,
                scale,
                grid: true,
            })
    };
    let mut ladder = Vec::new();
    // カーソルの下の子。タイルの隙間なら、いちばん近い子（配置済みのものだけ）。
    if let Some(child) = maker.tree.nodes[top]
        .children
        .iter()
        .copied()
        .filter(|&c| {
            let r = maker.layouts[c].fit;
            r.width() > 0.0 && r.height() > 0.0
        })
        .min_by(|a, b| {
            let d = |c: NodeId| maker.layouts[c].cell.center().distance_squared(point);
            d(*a).total_cmp(&d(*b))
        })
        && let Some(goal) = fit_goal(child)
    {
        ladder.push(goal);
    }
    ladder.extend(grid_goals(top));
    if let Some(goal) = fit_goal(top) {
        ladder.push(goal);
    }
    if let Some(parent) = maker.tree.nodes[top].parent {
        ladder.extend(grid_goals(parent));
        if let Some(goal) = fit_goal(parent) {
            ladder.push(goal);
        }
    }
    ladder.sort_by(|a, b| a.scale.total_cmp(&b.scale));
    ladder
}

/// ピンチを始めた倍率 `start_ln` から `amount`（倍率の比の対数）だけ動かして離したときの行き先。
/// 動かした先にいちばん近い段へ止まるが、その向きに十分動かしていれば、少なくとも 1 段は進む。
/// 「十分」は、`ZOOM_COMMIT_LN` と、次の段までの半分の、小さいほう（段が細かければ少しで進む）。
pub(super) fn choose_zoom(ladder: &[ZoomGoal], start_ln: f32, amount: f32) -> ZoomGoal {
    let distance = |g: &ZoomGoal, to: f32| (g.scale.ln() - to).abs();
    let nearest_to = |to: f32, among: &mut dyn Iterator<Item = &ZoomGoal>| {
        among
            .min_by(|a, b| distance(a, to).total_cmp(&distance(b, to)))
            .copied()
    };
    let here = nearest_to(start_ln, &mut ladder.iter()).expect("段は必ずある");
    let projected = start_ln + amount;
    // 始めた所より、その向きにある段。
    let inward: Vec<&ZoomGoal> = ladder
        .iter()
        .filter(|g| g.scale.ln() < start_ln - 0.01)
        .collect();
    let outward: Vec<&ZoomGoal> = ladder
        .iter()
        .filter(|g| g.scale.ln() > start_ln + 0.01)
        .collect();
    let needed = |side: &[&ZoomGoal]| {
        side.iter()
            .map(|g| distance(g, start_ln) / 2.0)
            .fold(ZOOM_COMMIT_LN, f32::min)
    };
    if amount < 0.0 && -amount >= needed(&inward) && !inward.is_empty() {
        nearest_to(projected, &mut inward.into_iter()).unwrap_or(here)
    } else if amount > 0.0 && amount >= needed(&outward) && !outward.is_empty() {
        nearest_to(projected, &mut outward.into_iter()).unwrap_or(here)
    } else {
        here
    }
}

/// フォルダの中を倍率 `scale` で見るとき、カメラの位置 `pos` に近い「タイルの切れ目に合わせた」位置。
/// 左右と上下それぞれ、どちらかの端を切れ目にそろえる位置のうち、画面の真ん中にいちばん近いタイルが
/// 見切れず、今の位置にいちばん近いものを選ぶ。中身の外へは出ない。
pub(super) fn grid_snap(
    maker: &SceneMaker,
    window: &Window,
    folder: NodeId,
    pos: Vec2,
    scale: f32,
) -> Vec2 {
    let children = &maker.tree.nodes[folder].children;
    let columns = maker.layouts[folder].grid.columns.max(1);
    let offset = Vec2::new(0.0, BAR_PX / 2.0 * scale);
    let Some(&first) = children.first() else {
        return pos;
    };
    let cell = maker.layouts[first].cell;
    // 隣のタイルとの隙間（2 つ目があれば、その間）。
    let gap = children
        .get(1)
        .filter(|_| columns > 1)
        .map_or(0.0, |c| maker.layouts[*c].cell.min.x - cell.max.x);
    let view = Vec2::new(crate::view::view_width(window), window.height() - BAR_PX) * scale;
    let center = pos - offset;
    // 列の左右の端と、行の上下の端（隙間の真ん中）。
    let cells: Vec<Rect> = children.iter().map(|c| maker.layouts[*c].cell).collect();
    let lefts: Vec<f32> = cells
        .iter()
        .take(columns)
        .map(|c| c.min.x - gap / 2.0)
        .collect();
    let rights: Vec<f32> = cells
        .iter()
        .take(columns)
        .map(|c| c.max.x + gap / 2.0)
        .collect();
    let tops: Vec<f32> = cells
        .iter()
        .step_by(columns)
        .map(|c| c.max.y + gap / 2.0)
        .collect();
    let bottoms: Vec<f32> = cells
        .iter()
        .step_by(columns)
        .map(|c| c.min.y - gap / 2.0)
        .collect();
    let (low, high) = (
        Vec2::new(lefts[0], bottoms.iter().copied().fold(f32::MAX, f32::min)),
        Vec2::new(rights.iter().copied().fold(f32::MIN, f32::max), tops[0]),
    );
    // 画面の真ん中にいちばん近いタイル。これが見切れないように止める。
    let nearest = cells
        .iter()
        .min_by(|a, b| {
            a.center()
                .distance_squared(center)
                .total_cmp(&b.center().distance_squared(center))
        })
        .copied()
        .unwrap_or(cell);
    let half = view / 2.0;
    // 1 つの向きについて、端（始まりか終わり）を画面の端にそろえる位置のうち、いちばん近いタイルが
    // 収まり、今の位置にいちばん近いもの。
    let snap = |starts: &[f32],
                ends: &[f32],
                current: f32,
                half: f32,
                low: f32,
                high: f32,
                keep: (f32, f32)|
     -> f32 {
        // 中身が画面より小さければ、真ん中に置く。
        if high - low <= half * 2.0 {
            return (low + high) / 2.0;
        }
        let candidates = starts
            .iter()
            .map(|e| e + half)
            .chain(ends.iter().map(|e| e - half))
            .map(|c| c.clamp(low + half, high - half));
        let fits =
            |c: &f32| keep.1 - keep.0 > half * 2.0 || (c - half <= keep.0 && keep.1 <= c + half);
        let closest = |a: &f32, b: &f32| (a - current).abs().total_cmp(&(b - current).abs());
        candidates
            .clone()
            .filter(fits)
            .min_by(closest)
            .or_else(|| candidates.min_by(closest))
            .unwrap_or(current)
    };
    let x = snap(
        &lefts,
        &rights,
        center.x,
        half.x,
        low.x,
        high.x,
        (nearest.min.x - gap / 2.0, nearest.max.x + gap / 2.0),
    );
    // 縦は、上の端を「始まり」として扱う（y は上向きなので、上の端から下へ半分ずらす）。
    let neg = |v: &[f32]| v.iter().map(|e| -e).collect::<Vec<f32>>();
    let y = -snap(
        &neg(&tops),
        &neg(&bottoms),
        -center.y,
        half.y,
        -high.y,
        -low.y,
        (-(nearest.max.y + gap / 2.0), -(nearest.min.y - gap / 2.0)),
    );
    Vec2::new(x, y) + offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(scales: &[f32]) -> Vec<ZoomGoal> {
        scales
            .iter()
            .map(|&scale| ZoomGoal {
                node: 0,
                scale,
                grid: true,
            })
            .collect()
    }

    #[test]
    fn small_pinch_stays_on_coarse_ladder() {
        // 次の段が遠い（8 → 4）なら、少しのピンチでは動かない。
        let l = ladder(&[1.0, 4.0, 8.0]);
        assert_eq!(choose_zoom(&l, 8f32.ln(), -0.05).scale, 8.0);
        assert_eq!(choose_zoom(&l, 8f32.ln(), -0.2).scale, 4.0);
    }

    #[test]
    fn small_pinch_steps_on_fine_ladder() {
        // 段が細かければ（10, 9, 8 …）、少しのピンチでも 1 段進む。
        let l = ladder(&[7.0, 8.0, 9.0, 10.0]);
        assert_eq!(choose_zoom(&l, 10f32.ln(), -0.06).scale, 9.0);
        // 大きく動かせば、動かした先にいちばん近い段へ。
        assert_eq!(choose_zoom(&l, 10f32.ln(), -0.3).scale, 7.0);
        // 広げる向きも同じ。
        assert_eq!(choose_zoom(&l, 8f32.ln(), 0.07).scale, 9.0);
    }
}
