//! 指を離したあとの動き。離す直前の勢いを持ったまま動き続け、行き先（ピンチなら段、2本指なら
//! 同じ階層のタイル）へ、ずれが指数関数的に縮むように寄せる。途中でも、いちばん近いものが変われば
//! 開いている場所を切り替える（`refocus`）。

use std::time::Instant;

use bevy::prelude::*;

use crate::model::tree::{FileTree, NodeId};
use crate::ui::menu::BAR_PX;
use crate::view::{
    CurrentScene, Level, Location, Nav, SceneMaker, SyncRequest, fit, scene_path, show_breadcrumb,
};

use super::zoom_levels::{ZoomGoal, grid_levels, grid_snap};

/// 離したあとの勢いが弱まる時定数（秒）。勢いは指数関数的に弱まる。小さいほど摩擦が強い。
const MOMENTUM_SECONDS: f32 = 0.12;
/// 離したあと、いちばん近いタイルの真ん中へ寄っていく時定数（秒）。ずれが指数関数的に縮む。
const PULL_SECONDS: f32 = 0.12;
/// 勢いもずれもこれより小さくなったら（画面上のピクセル、毎秒のピクセル、倍率の対数で毎秒）、止める。
const REST_PX: f32 = 0.3;
const REST_PX_PER_SECOND: f32 = 5.0;
const REST_LN_PER_SECOND: f32 = 0.01;
/// 2本指で動かして離したとき、勢いをこれだけ先（秒）まで見込んで、行き先を決める。
const PAN_LOOKAHEAD: f32 = 0.25;
/// 2本指で離したときの速さ（画面上のピクセル毎秒）がこれ以上なら、見込んだ先が今のままでも、
/// その向きの隣へ 1 つ進む。
const FLICK_PX_PER_SECOND: f32 = 400.0;
/// 開いている場所を切り替えるとき、新しい行き先がこれだけ近くなるまで待つ（境目で行ったり来たり
/// しないように）。近さは `closeness` の単位。
const REFOCUS_MARGIN: f32 = 0.05;

/// 離したあとの動き。離したときの速さを持ったまま、いちばん近い行き先へ重力で引き寄せられる。
pub(super) struct Coast {
    /// 速さ（ワールド座標で毎秒、倍率の対数で毎秒）。
    pub(super) velocity: Vec2,
    pub(super) velocity_ln: f32,
    pub(super) last: Instant,
    /// 縮小して親が見えてきたときに、下に描いておく親の場面。
    pub(super) parent_scene: Option<Entity>,
    /// ピンチ（ホイール）で決めた行き先。その大きさへ寄せる。無ければ倍率はそのまま。
    pub(super) zoom_goal: Option<ZoomGoal>,
    /// 2本指で動かして離したときに決めた行き先。
    pub(super) pan_goal: Option<PanGoal>,
}

/// 2本指で動かして離したときの行き先。
#[derive(Clone, Copy, Debug)]
pub(super) enum PanGoal {
    /// 同じ階層のタイル（またはそのまま）。その真ん中へ寄せる。
    Node(NodeId),
    /// フォルダの中を拡大して見ているときの、タイルの切れ目に合わせた位置。
    Grid(Vec2),
}

/// 縮小して今の枠より外が見えてきたら、親の場面を下に描いておく。
pub(super) fn show_parent_if_zoomed_out(
    parent_scene: &mut Option<Entity>,
    nav: &Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
    scale: f32,
) {
    let top = *nav.top();
    let (_, rest_scale) = fit(top.frame(&maker.layouts), window);
    if nav.stack.len() < 2
        || scale <= rest_scale * 1.01
        || parent_scene.is_some()
        || !maker.tree.nodes[top.node].is_dir
    {
        return;
    }
    *parent_scene = Some(spawn_parent(nav, maker, window));
    // 今の場面を下敷きより後に作り直して、上に描かれるようにする（作った順に重なるため）。
    maker.commands.entity(current.0).despawn();
    current.0 = maker.spawn(&nav.stack, rest_scale);
    maker
        .commands
        .entity(current.0)
        .insert(Transform::from_xyz(0.0, 0.0, 0.5));
}

/// 親の場面を作る。開いているフォルダの下に敷く。開いているフォルダと周りの 8 つは、動かして
/// いる間にちらつかないよう、カリングしない。
pub(super) fn spawn_parent(nav: &Nav, maker: &mut SceneMaker, window: &Window) -> Entity {
    let path = &nav.stack[..nav.stack.len() - 1];
    let (_, scale) = fit(path[path.len() - 1].frame(&maker.layouts), window);
    maker.spawn_around(path, scale, Some(nav.top().node))
}

/// 引き寄せる行き先。
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Target {
    /// 今開いているもの。
    Current,
    /// 今のフォルダの中の子。
    Child(NodeId),
    /// 親。
    Parent,
    /// 同じ階層の隣。
    Sibling(NodeId),
}

/// 行き先の候補と、それぞれを画面に収めたときのカメラの位置と倍率。
pub(super) fn targets(
    nav: &Nav,
    maker: &SceneMaker,
    window: &Window,
    pos: Vec2,
) -> Vec<(Target, Vec2, f32)> {
    let top = *nav.top();
    let fit_of = |id: NodeId| -> Option<(Vec2, f32)> {
        let rect = maker.layouts[id].fit;
        if !(rect.width() > 0.0 && rect.height() > 0.0) {
            return None;
        }
        let (p, s) = fit(rect, window);
        (s > 0.0 && s.is_finite() && p.is_finite()).then_some((p, s))
    };
    // フォルダの中を拡大して見る段も、そのフォルダの行き先として候補に入れる。
    let grid_of = |id: NodeId| {
        grid_levels(maker, window, id)
            .into_iter()
            .filter(|s| *s > 0.0 && s.is_finite())
            .map(move |s| (grid_snap(maker, window, id, pos, s), s))
    };
    let mut out = Vec::new();
    if let Some((p, s)) = fit_of(top.node) {
        out.push((Target::Current, p, s));
    }
    for (p, s) in grid_of(top.node) {
        out.push((Target::Current, p, s));
    }
    for &child in &maker.tree.nodes[top.node].children {
        if let Some((p, s)) = fit_of(child) {
            out.push((Target::Child(child), p, s));
        }
    }
    if nav.stack.len() >= 2 {
        let parent = nav.stack[nav.stack.len() - 2].node;
        if let Some((p, s)) = fit_of(parent) {
            out.push((Target::Parent, p, s));
        }
        for (p, s) in grid_of(parent) {
            out.push((Target::Parent, p, s));
        }
        for &sibling in &maker.tree.nodes[parent].children {
            if sibling != top.node
                && let Some((p, s)) = fit_of(sibling)
            {
                out.push((Target::Sibling(sibling), p, s));
            }
        }
    }
    out
}

/// 離したあとの動きを 1 歩進める。止まったら行き先を開いて true を返す。
///
/// 離したときの勢いは指数関数的に弱めながら持ち越し、行き先へは、ずれが指数関数的に縮むように
/// 寄せる。行き先は、ピンチなら `zoom_goal`、2本指なら `pan_goal`、どちらも無ければ（Root より
/// 縮めたときなど）いちばん近いもの。途中でも、いちばん近いものが変われば開いている場所を切り替える。
#[allow(clippy::too_many_arguments)]
pub(super) fn root_fit_scale(layouts: &crate::layout::Layouts, window: &Window) -> Option<f32> {
    let root = layouts[FileTree::ROOT].fit;
    if !crate::layout::valid_frame(root) {
        return None;
    }
    let scale = fit(root, window).1;
    (scale.is_finite() && scale > 0.0).then_some(scale)
}

pub(super) fn coast_step(
    coast: &mut Coast,
    location: &mut Location,
    sync: &mut SyncRequest,
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
    transform: &mut Transform,
    ortho: &mut OrthographicProjection,
) -> bool {
    let now = Instant::now();
    // 止まっていたあとの最初のフレームなどで、時間が飛びすぎないようにする。
    let dt = (now - coast.last).as_secs_f32().min(1.0 / 30.0);
    coast.last = now;

    let pos = transform.translation.truncate();
    let root_ln = root_fit_scale(&maker.layouts, window).map(f32::ln);
    if !maker.layouts.contains(nav.top().node) && maker.tree.is_attached(nav.top().node) {
        return false;
    }
    if !(ortho.scale > 0.0 && ortho.scale.is_finite() && pos.is_finite()) {
        if let Some(parent) = coast.parent_scene.take() {
            maker.commands.entity(parent).despawn();
        }
        sync.request_reset();
        return true;
    }

    // 動いている途中でも、いちばん近い行き先が変わったら、開いている場所をそちらへ切り替える。
    refocus(
        &mut coast.parent_scene,
        location,
        nav,
        current,
        maker,
        window,
        pos,
        ortho.scale,
    );
    let ln = ortho.scale.ln();

    // 勢いは指数関数的に弱める（終わり際ほどよく効く摩擦）。
    let momentum = (-dt / MOMENTUM_SECONDS).exp();
    let pull = 1.0 - (-dt / PULL_SECONDS).exp();
    coast.velocity *= momentum;
    coast.velocity_ln *= momentum;

    // 倍率は、ピンチで行き先を決めたなら、その大きさへ寄せる（ずれが指数関数的に縮む）。
    // ピンチでなければ（2本指で動かしただけなら）、勢いで少し流してから止める。
    // どちらでも、Root より縮めていたら Root の大きさへ寄せる。
    let mut ln = ln + coast.velocity_ln * dt;
    if let Some(goal) = coast.zoom_goal
        && goal.scale > 0.0
        && goal.scale.is_finite()
    {
        ln += (goal.scale.ln() - ln) * pull;
    }
    if let Some(root_ln) = root_ln
        && ln > root_ln
    {
        ln += (root_ln - ln) * pull;
    }
    let scale = ln.exp();

    // 位置は、勢いで流しながら寄せる（ずれが指数関数的に縮む）。フォルダの中を拡大して見ているなら
    // タイルの切れ目に合わせた所へ、そうでなければいちばん近い行き先の真ん中へ。
    let grid_folder = match coast.zoom_goal {
        Some(goal) if goal.grid => Some(goal.node),
        Some(_) => None,
        None => zoomed_within(nav, maker, window, scale).then(|| nav.top().node),
    };
    let goal = match (grid_folder, coast.zoom_goal, coast.pan_goal) {
        (_, None, Some(PanGoal::Grid(goal))) => goal,
        (_, None, Some(PanGoal::Node(node))) if maker.layouts.contains(node) => {
            let (p, s) = fit(maker.layouts[node].fit, window);
            p + Vec2::new(0.0, BAR_PX / 2.0 * (scale - s))
        }
        (Some(folder), _, _) if maker.layouts.contains(folder) => {
            grid_snap(maker, window, folder, pos, scale)
        }
        (None, Some(goal), _) if maker.layouts.contains(goal.node) => {
            let (p, s) = fit(maker.layouts[goal.node].fit, window);
            p + Vec2::new(0.0, BAR_PX / 2.0 * (scale - s))
        }
        _ => match nearest(nav, maker, window, pos, scale) {
            Some(((_, p, s), _, _)) => p + Vec2::new(0.0, BAR_PX / 2.0 * (scale - s)),
            None => {
                if let Some(parent) = coast.parent_scene.take() {
                    maker.commands.entity(parent).despawn();
                }
                sync.request_reset();
                return true;
            }
        },
    };
    let mut pos = pos + coast.velocity * dt;
    pos += (goal - pos) * pull;
    if !(scale > 0.0 && scale.is_finite() && pos.is_finite()) {
        if let Some(parent) = coast.parent_scene.take() {
            maker.commands.entity(parent).despawn();
        }
        sync.request_reset();
        return true;
    }
    transform.translation = pos.extend(0.0);
    ortho.scale = scale;

    // 勢いもずれも十分小さくなったら、そこで止める（最後に跳ばない）。
    let at_rest = coast.velocity.length() / scale < REST_PX_PER_SECOND
        && coast.velocity_ln.abs() < REST_LN_PER_SECOND
        && (goal - pos).length() / scale < REST_PX
        && root_ln.is_none_or(|root| ln <= root + 1e-3)
        && coast.zoom_goal.is_none_or(|goal| {
            goal.scale <= 0.0 || !goal.scale.is_finite() || (ln - goal.scale.ln()).abs() < 0.002
        });
    if !at_rest {
        show_parent_if_zoomed_out(&mut coast.parent_scene, nav, current, maker, window, scale);
        return false;
    }
    // ピンチで決めた行き先に、開いている場所を合わせる（途中で切り替わっていれば何もしない）。
    let settled = match (coast.zoom_goal, coast.pan_goal) {
        (Some(goal), _) => Some(goal.node),
        (None, Some(PanGoal::Node(node))) => Some(node),
        _ => None,
    };
    if let Some(node) = settled {
        focus_node(
            node,
            &mut coast.parent_scene,
            location,
            sync,
            nav,
            current,
            maker,
            window,
        );
    }
    // 今のものより縮めたまま止まったら、下に敷いた親の場面は残す（周りが見えているので）。
    // ただしファイルを開いているとき（親の場面そのものを表示中）は親の下敷きは不要なので必ず消す。
    let top_is_dir = maker.tree.nodes[nav.top().node].is_dir;
    let (_, rest_scale) = fit(nav.top().frame(&maker.layouts), window);
    if (!top_is_dir || scale <= rest_scale * 1.01)
        && let Some(parent) = coast.parent_scene.take()
    {
        maker.commands.entity(parent).despawn();
    }
    true
}

/// 2本指で動かして離したときの行き先。勢いを `PAN_LOOKAHEAD` 秒先まで見込んだ所にいちばん近い
/// 同じ階層のタイルへ。見込んだ先が今のままでも、速く動かしていれば、その向きの隣へ 1 つ進む。
/// フォルダの中を拡大して見ているなら、見込んだ先でタイルの切れ目に合わせた位置へ。
pub(super) fn choose_pan(
    nav: &Nav,
    maker: &SceneMaker,
    window: &Window,
    pos: Vec2,
    scale: f32,
    velocity: Vec2,
) -> Option<PanGoal> {
    let top = nav.top().node;
    let offset = Vec2::new(0.0, BAR_PX / 2.0 * scale);
    let projected = pos + velocity * PAN_LOOKAHEAD;
    if zoomed_within(nav, maker, window, scale) {
        return Some(PanGoal::Grid(grid_snap(
            maker, window, top, projected, scale,
        )));
    }
    let parent = maker.tree.nodes[top].parent?;
    let siblings = &maker.tree.nodes[parent].children;
    let center = |id: NodeId| maker.layouts[id].cell.center();
    let at = projected - offset;
    let nearest = siblings
        .iter()
        .copied()
        .filter(|s| maker.layouts.contains(*s))
        .min_by(|a, b| {
            center(*a)
                .distance_squared(at)
                .total_cmp(&center(*b).distance_squared(at))
        })
        .unwrap_or(top);
    let speed = velocity.length() / scale;
    if nearest != top || speed < FLICK_PX_PER_SECOND {
        return Some(PanGoal::Node(nearest));
    }
    // 速く動かしたのに行き先が今のままなら、その向きにある隣のうち、向きに沿って近いものへ。
    let direction = velocity.normalize_or_zero();
    let from = center(top);
    let next = siblings
        .iter()
        .copied()
        .filter(|s| *s != top && maker.layouts.contains(*s))
        .filter_map(|s| {
            let d = center(s) - from;
            let along = d.dot(direction);
            (along > 0.0).then(|| {
                let across = (d - direction * along).length();
                (s, along + across * 2.0)
            })
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(top, |(s, _)| s);
    Some(PanGoal::Node(next))
}

/// 今のフォルダを、画面に収める大きさより拡大して中を見ているか。
pub(super) fn zoomed_within(nav: &Nav, maker: &SceneMaker, window: &Window, scale: f32) -> bool {
    let top = nav.top().node;
    maker.tree.nodes[top].is_dir && scale < fit(maker.layouts[top].fit, window).1 * 0.9
}

/// 開いている場所を `node` にする（Root からの道のりに置き換え、正の場所と場面を入れ替える）。
#[allow(clippy::too_many_arguments)]
pub(super) fn focus_node(
    node: NodeId,
    parent_scene: &mut Option<Entity>,
    location: &mut Location,
    sync: &mut SyncRequest,
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
) {
    let Some(nodes) = maker.tree.path_from_root(node) else {
        // 木から外れた古いノードなら無視して、正の場所からそろえ直す。
        sync.request_reset();
        return;
    };
    maker.ensure_folder(node);
    if !maker.layouts.contains(node) {
        return;
    }
    if nav.top().node == node {
        if !maker.tree.nodes[node].is_dir
            && let Some(parent) = parent_scene.take()
        {
            maker.commands.entity(parent).despawn();
        }
        return;
    }
    let path: Vec<Level> = nodes.into_iter().map(|node| Level { node }).collect();
    let old_folder = scene_path(nav, &maker.tree).last().map(|l| l.node);
    nav.stack = path;
    *location = Location::from_tree(&maker.tree, node);
    let new_scene = scene_path(nav, &maker.tree).to_vec();
    let new_folder = new_scene.last().map(|l| l.node);
    if old_folder != new_folder {
        // 下に敷いた親の場面がちょうど行き先の場面なら、それを使う。
        let parent_is_new = old_folder.is_some_and(|f| maker.tree.nodes[f].parent == new_folder);
        match parent_scene.take() {
            Some(parent) if parent_is_new => {
                maker.commands.entity(current.0).despawn();
                current.0 = parent;
            }
            other => {
                if let Some(parent) = other {
                    maker.commands.entity(parent).despawn();
                }
                let (_, scale) = fit(new_scene[new_scene.len() - 1].frame(&maker.layouts), window);
                maker.commands.entity(current.0).despawn();
                current.0 = maker.spawn(&new_scene, scale);
            }
        }
    } else if !maker.tree.nodes[node].is_dir
        && let Some(parent) = parent_scene.take()
    {
        // 同じ場面のままファイルへ移ったら、親の下敷きは不要なので消す。
        maker.commands.entity(parent).despawn();
    }
    show_breadcrumb(&mut maker.commands, &maker.tree, nav.top().node);
}

/// 行き先の近さ。倍率の比の対数と、位置のずれ（その行き先を収めたときの画面の幅で割ったもの）の和。
pub(super) fn closeness(
    pos: Vec2,
    scale: f32,
    target_pos: Vec2,
    target_scale: f32,
    view_width: f32,
) -> f32 {
    if !(scale > 0.0 && target_scale > 0.0 && view_width > 0.0) {
        return f32::INFINITY;
    }
    (scale.ln() - target_scale.ln()).abs()
        + (pos - target_pos).length() / (target_scale * view_width)
}

/// 今の位置と倍率にいちばん近い行き先と、その近さ。今開いているものの近さも返す。
pub(super) fn nearest(
    nav: &Nav,
    maker: &SceneMaker,
    window: &Window,
    pos: Vec2,
    scale: f32,
) -> Option<((Target, Vec2, f32), f32, f32)> {
    let candidates = targets(nav, maker, window, pos);
    if candidates.is_empty() {
        return None;
    }
    let close = |(_, p, s): &(Target, Vec2, f32)| {
        closeness(pos, scale, *p, *s, crate::view::view_width(window))
    };
    // 今開いているものの近さ（収める大きさと、中を拡大して見る段のうち、いちばん近いもの）。
    let current = candidates
        .iter()
        .filter(|c| c.0 == Target::Current)
        .map(close)
        .fold(f32::INFINITY, f32::min);
    let best = candidates
        .into_iter()
        .min_by(|a, b| close(a).total_cmp(&close(b)))?;
    let best_closeness = close(&best);
    Some((best, best_closeness, current))
}

/// いちばん近い行き先が今開いているものより十分近ければ、開いている場所をそちらへ切り替える
/// （カメラは動かさない）。動かしている最中も離したあとも、毎フレーム見る。
#[allow(clippy::too_many_arguments)]
pub(super) fn refocus(
    parent_scene: &mut Option<Entity>,
    location: &mut Location,
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
    pos: Vec2,
    scale: f32,
) {
    // 一度に 2 段以上ずれていることもあるので、何回か見る。
    for _ in 0..4 {
        let Some(((target, _, _), best, now)) = nearest(nav, maker, window, pos, scale) else {
            return;
        };
        // 境目で行ったり来たりしないよう、少し余裕を持たせる。
        if target == Target::Current || best > now - REFOCUS_MARGIN {
            return;
        }
        switch_focus(target, parent_scene, location, nav, current, maker, window);
    }
}

/// 開いている場所を `target` へ切り替え、正の場所と場面をその場で入れ替える（カメラは動かさない）。
pub(super) fn switch_focus(
    target: Target,
    parent_scene: &mut Option<Entity>,
    location: &mut Location,
    nav: &mut Nav,
    current: &mut CurrentScene,
    maker: &mut SceneMaker,
    window: &Window,
) {
    let was_dir = maker.tree.nodes[nav.top().node].is_dir;
    // 開いている場所の本物の場面を作り直す。ファイルなら親の場面。
    let respawn = |current: &mut CurrentScene, maker: &mut SceneMaker, nav: &Nav| {
        let path = scene_path(nav, &maker.tree).to_vec();
        maker.ensure_folder(path.last().unwrap().node);
        let (_, scale) = fit(path[path.len() - 1].frame(&maker.layouts), window);
        maker.commands.entity(current.0).despawn();
        current.0 = maker.spawn(&path, scale);
    };
    match target {
        Target::Current => return,
        Target::Child(child) => {
            maker.ensure_folder(child);
            if !maker.tree.is_attached(child) || !maker.layouts.contains(child) {
                return;
            }
            if let Some(parent) = parent_scene.take() {
                maker.commands.entity(parent).despawn();
            }
            nav.stack.push(Level { node: child });
            // ファイルは今の場面にそのまま描いてあるので切り替えない。
            if maker.tree.nodes[child].is_dir {
                respawn(current, maker, nav);
            }
        }
        Target::Parent => {
            if nav.stack.len() < 2 {
                return;
            }
            nav.stack.pop();
            // 開いていたのがファイルなら、今の場面がもう親の場面。
            if was_dir {
                match parent_scene.take() {
                    Some(parent) => {
                        maker.commands.entity(current.0).despawn();
                        current.0 = parent;
                    }
                    None => respawn(current, maker, nav),
                }
            } else if let Some(parent) = parent_scene.take() {
                maker.commands.entity(parent).despawn();
            }
        }
        Target::Sibling(sibling) => {
            maker.ensure_folder(sibling);
            if !maker.tree.is_attached(sibling) || !maker.layouts.contains(sibling) {
                return;
            }
            *nav.stack.last_mut().expect("今のもの") = Level { node: sibling };
            let is_dir = maker.tree.nodes[sibling].is_dir;
            if is_dir || was_dir {
                // 下に敷いた親の場面はそのまま使える（同じ親）。ファイルなら今の場面が親の場面になる。
                if !is_dir && let Some(parent) = parent_scene.take() {
                    maker.commands.entity(parent).despawn();
                }
                respawn(current, maker, nav);
            }
        }
    }
    *location = Location::from_tree(&maker.tree, nav.top().node);
    show_breadcrumb(&mut maker.commands, &maker.tree, nav.top().node);
}

#[cfg(test)]
mod root_limit_tests {
    use super::*;

    #[test]
    fn zoom_out_has_no_root_limit_when_the_root_is_too_far_to_lay_out() {
        let tree = FileTree::deep(160);
        let bottom = tree
            .nodes
            .iter()
            .position(|node| node.name == "底")
            .unwrap();
        let spec = crate::layout::EarSpec::new(&crate::settings::Settings::default());
        let layouts = crate::layout::layout(&tree, bottom, crate::layout::anchor_body(), spec);
        assert_eq!(root_fit_scale(&layouts, &Window::default()), None);
    }
}
