//! 開いたフォルダの外装だけを、子へ近づく倍率に合わせて薄くする。

use super::*;
use crate::render::scene::FolderShell;

const HIDDEN_ALPHA: f32 = 0.005;

fn opacity(start: f32, child: f32, end: f32, scale: f32) -> f32 {
    if !(0.0 < end && end < child && child < start && 0.0 < scale) {
        return 1.0;
    }
    // 階層ごとの縮小率が違っても、各階層の安定倍率で 1.0 / 0.5 / 0.0 を通る。
    let (from, to, offset) = if scale >= child {
        (start, child, 0.0)
    } else {
        (child, end, 0.5)
    };
    let progress = ((from / scale).ln() / (from / to).ln()).clamp(0.0, 1.0);
    1.0 - offset - 0.5 * progress
}

fn visibility(alpha: f32) -> Visibility {
    if alpha <= HIDDEN_ALPHA {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    }
}

fn next_folder(
    tree: &FileTree,
    layouts: &Layouts,
    folder: NodeId,
    position: Vec2,
) -> Option<NodeId> {
    tree.nodes[folder]
        .children
        .iter()
        .copied()
        .filter(|&id| tree.nodes[id].is_dir && layouts.contains(id))
        .min_by(|&a, &b| {
            let distance = |id| layouts[id].cell.center().distance_squared(position);
            distance(a).total_cmp(&distance(b))
        })
}

fn scales(path: &[NodeId], folder: NodeId, fit_scale: impl Fn(NodeId) -> f32) -> (f32, f32, f32) {
    let start = fit_scale(folder);
    let below = path
        .iter()
        .position(|&id| id == folder)
        .map_or(&[][..], |index| &path[index + 1..]);
    let child = below
        .first()
        .map(|&id| fit_scale(id))
        .filter(|&s| s > 0.0 && s < start)
        .unwrap_or(start * 0.25);
    let end = below
        .get(1)
        .map(|&id| fit_scale(id))
        .filter(|&s| s > 0.0 && s < child)
        .unwrap_or(child * (child / start));
    (start, child, end)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update(
    tree: Res<FileTree>,
    nav: Res<Nav>,
    layouts: Res<Layouts>,
    camera: Single<(&Projection, &Transform), With<MainCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut shells: Query<(&mut FolderShell, &mut Visibility, &Children)>,
    mut sprites: Query<(&BaseAlpha, Option<&mut Sprite>, Option<&mut TextColor>)>,
) {
    let (projection, transform) = *camera;
    let Projection::Orthographic(ortho) = projection else {
        return;
    };
    let mut path: Vec<_> = scene_path(&nav, &tree)
        .iter()
        .map(|level| level.node)
        .collect();
    // ピンチ中はまだ Nav が切り替わらないため、注視点の近くにある子を使う。
    let position = transform.translation.truncate();
    for _ in 0..2 {
        let Some(child) = path
            .last()
            .and_then(|&folder| next_folder(&tree, &layouts, folder, position))
        else {
            break;
        };
        path.push(child);
    }
    let fit_scale = |id| fit(layouts[id].fit, &window).1;
    for (mut shell, mut visible, children) in &mut shells {
        let (start, child, end) = scales(&path, shell.folder, fit_scale);
        let alpha = opacity(start, child, end, ortho.scale);
        shell.alpha = alpha;
        visible.set_if_neq(visibility(alpha));
        // シーンや中心フォルダのフェードはこのあと BaseAlpha に両方の係数を掛ける。
        for entity in children.iter() {
            if let Ok((base, sprite, text)) = sprites.get_mut(entity) {
                let value = base.0 * alpha;
                if let Some(mut sprite) = sprite
                    && sprite.color.alpha() != value
                {
                    sprite.color.set_alpha(value);
                }
                if let Some(mut text) = text
                    && text.0.alpha() != value
                {
                    text.0.set_alpha(value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_opacity_follows_the_three_rest_scales() {
        assert_eq!(opacity(16.0, 4.0, 0.5, 16.0), 1.0);
        assert_eq!(opacity(4.0, 0.5, 0.125, 16.0), 1.0);
        assert_eq!(opacity(16.0, 4.0, 0.5, 4.0), 0.5);
        assert_eq!(opacity(16.0, 4.0, 0.5, 0.5), 0.0);
        assert_eq!(visibility(opacity(16.0, 4.0, 0.5, 0.5)), Visibility::Hidden);
        assert_eq!(visibility(0.5), Visibility::Inherited);
    }

    #[test]
    fn switching_scenes_keeps_the_same_shell_opacity() {
        let fit = |id| [16.0, 4.0, 0.5][id];
        let before = scales(&[0, 1], 0, fit);
        let after = scales(&[0, 1, 2], 0, fit);
        assert_eq!(
            opacity(before.0, before.1, before.2, 4.0),
            opacity(after.0, after.1, after.2, 4.0)
        );
    }
}
