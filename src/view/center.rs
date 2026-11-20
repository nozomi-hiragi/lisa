//! 一覧の中心にある子フォルダの実物を、サムネと切り替える。

use super::*;
use crate::render::scene::{FolderPreview, FolderShell, FolderTile};

#[derive(Component)]
pub(super) struct Contents {
    pub(super) scene: Entity,
    pub(super) folder: NodeId,
}

#[derive(Default)]
pub(super) struct Selection {
    tile: Option<Entity>,
    contents: Option<Entity>,
    outward: Option<(Entity, Entity)>,
}

fn live_alpha(parent: f32, child: f32, current: f32) -> f32 {
    if !(0.0 < child && child < parent && current > 0.0) {
        return 0.0;
    }
    let progress = ((parent / current).ln() / (parent / child).ln()).clamp(0.0, 1.0);
    let x = ((progress - 0.12) / (0.95 - 0.12)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update(
    mut selection: Local<Selection>,
    mut maker: SceneMaker,
    current: Res<CurrentScene>,
    nav: Res<Nav>,
    camera: Single<(&Transform, &Projection), With<MainCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    animating: Option<Res<Animating>>,
    gesture: Res<crate::input::gesture::Gesture>,
    roots: Query<(&SceneRoot, Option<&Fade>)>,
    tiles: Query<(Entity, &FolderTile, &ChildOf)>,
    groups: Query<&Transform, Without<MainCamera>>,
    hierarchy: Query<&Children>,
    previews: Query<(), With<FolderPreview>>,
    all: Query<Entity>,
    mut parts: Query<fade::ScenePart>,
    shells: Query<&FolderShell>,
) {
    let (transform, projection) = *camera;
    let Projection::Orthographic(ortho) = projection else {
        return;
    };
    let Ok((root, fade)) = roots.get(current.0) else {
        return;
    };
    if !maker.layouts.contains(root.folder) {
        return;
    }
    // ピンチで親を下に敷いている間は、既にある本シーンを実物として使う。
    let outward = gesture.parent_scene_entity().and_then(|parent_scene| {
        let (parent, _) = roots.get(parent_scene).ok()?;
        let (tile, _, _) = tiles
            .iter()
            .find(|(_, tile, _)| tile.scene == parent_scene && tile.folder == root.folder)?;
        let has_preview = hierarchy
            .get(tile)
            .ok()?
            .iter()
            .any(|child| previews.contains(child));
        (has_preview && maker.layouts.contains(parent.folder)).then_some((
            current.0,
            tile,
            parent.folder,
        ))
    });
    if selection.outward != outward.map(|(scene, tile, _)| (scene, tile)) {
        if let Some((scene, tile)) = selection.outward.take() {
            set_contents_alpha(
                scene,
                &hierarchy,
                &mut parts,
                1.0,
                &mut maker.materials,
                &shells,
            );
            set_preview_alpha(
                tile,
                &hierarchy,
                &previews,
                &mut parts,
                1.0,
                &mut maker.materials,
                &shells,
            );
        }
    }
    if let Some((scene, tile, parent)) = outward {
        if let Some(old) = selection.tile.take() {
            set_preview_alpha(
                old,
                &hierarchy,
                &previews,
                &mut parts,
                1.0,
                &mut maker.materials,
                &shells,
            );
        }
        if let Some(old) = selection.contents.take()
            && all.contains(old)
        {
            maker.commands.entity(old).despawn();
        }
        let parent_scale = fit(maker.layouts[parent].fit, &window).1;
        let child_scale = fit(maker.layouts[root.folder].fit, &window).1;
        let alpha = live_alpha(parent_scale, child_scale, ortho.scale);
        set_contents_alpha(
            scene,
            &hierarchy,
            &mut parts,
            alpha,
            &mut maker.materials,
            &shells,
        );
        set_preview_alpha(
            tile,
            &hierarchy,
            &previews,
            &mut parts,
            1.0 - alpha,
            &mut maker.materials,
            &shells,
        );
        selection.outward = Some((scene, tile));
        return;
    }
    let parent_scale = fit(maker.layouts[root.folder].fit, &window).1;
    let origin = transform.translation.truncate();
    let view = Rect::from_center_size(
        origin,
        Vec2::new(view_width(&window), window.height()) * ortho.scale,
    );
    let preferred = animating.as_ref().and_then(|anim| {
        if anim.then_show {
            scene_path(&nav, &maker.tree).last().map(|level| level.node)
        } else {
            root.focus
        }
    });
    let candidate = tiles
        .iter()
        .filter(|(_, tile, _)| {
            tile.scene == current.0
                && maker.layouts.contains(tile.folder)
                && maker.tree.nodes[tile.folder].loaded
        })
        .filter_map(|(entity, tile, parent)| {
            let cell = maker.layouts[tile.folder].cell;
            // タイルの直上は親の YieldBody。Transform は入力の直後に更新される。
            let cell = groups.get(parent.parent()).map_or(cell, |group| {
                Rect::from_corners(
                    group.transform_point(cell.min.extend(0.0)).truncate(),
                    group.transform_point(cell.max.extend(0.0)).truncate(),
                )
            });
            let priority = preferred == Some(tile.folder);
            let overlap = cell.intersect(view).size().min_element() > 0.0;
            (priority || overlap).then_some((
                entity,
                tile.folder,
                priority,
                cell.center().distance_squared(origin),
            ))
        })
        .min_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.3.total_cmp(&b.3))
                .then(a.1.cmp(&b.1))
        });
    let scene_alpha = fade.map_or(1.0, Fade::alpha);
    if selection.tile != candidate.map(|candidate| candidate.0) {
        if let Some(old_tile) = selection.tile.take()
            && let Ok((_, tile, _)) = tiles.get(old_tile)
        {
            let alpha = roots
                .get(tile.scene)
                .ok()
                .and_then(|(_, fade)| fade)
                .map_or(1.0, Fade::alpha);
            set_preview_alpha(
                old_tile,
                &hierarchy,
                &previews,
                &mut parts,
                alpha,
                &mut maker.materials,
                &shells,
            );
        }
        if let Some(old) = selection.contents.take()
            && all.contains(old)
        {
            maker.commands.entity(old).despawn();
        }
        if let Some((tile, folder, _, _)) = candidate {
            let child_scale = fit(maker.layouts[folder].fit, &window).1;
            if child_scale.is_finite() && child_scale > 0.0 {
                let depth = maker.tree.nodes[folder].depth as f32;
                // 中身の描画順を保ったまま、札の塗りとサムネの間の奥行きに収める。
                let z_scale = 0.001;
                let entity = maker
                    .commands
                    .spawn((
                        ChildOf(tile),
                        Contents {
                            scene: current.0,
                            folder,
                        },
                        Transform::from_xyz(0.0, 0.0, depth + 0.002 - (depth + 1.0) * z_scale)
                            .with_scale(Vec3::new(1.0, 1.0, z_scale)),
                        Visibility::Hidden,
                    ))
                    .id();
                let source = SceneSource {
                    tree: &maker.tree,
                    layouts: &maker.layouts,
                    images: &maker.images,
                    previews: &maker.previews,
                    font: &maker.font.0,
                    ears: &maker.ears,
                    theme: &maker.theme,
                    highlight: None,
                    catalog: None,
                };
                scene::draw_contents(
                    &mut maker.commands,
                    &mut maker.meshes,
                    &mut maker.materials,
                    &source,
                    folder,
                    entity,
                    RenderLayers::layer(CONTENT_LAYER),
                    child_scale,
                    true,
                );
                selection.tile = Some(tile);
                selection.contents = Some(entity);
            }
        }
    }
    if let (Some((tile, folder, _, _)), Some(contents)) = (candidate, selection.contents) {
        let child_scale = fit(maker.layouts[folder].fit, &window).1;
        let alpha = live_alpha(parent_scale, child_scale, ortho.scale);
        // 作った直後の部分フェードは、子がクエリから見える次のフレームで始める。
        if !all.contains(contents) && (alpha < 1.0 || scene_alpha < 1.0) {
            return;
        }
        maker.commands.entity(contents).insert(if alpha == 0.0 {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        });
        set_contents_alpha(
            contents,
            &hierarchy,
            &mut parts,
            alpha * scene_alpha,
            &mut maker.materials,
            &shells,
        );
        set_preview_alpha(
            tile,
            &hierarchy,
            &previews,
            &mut parts,
            (1.0 - alpha) * scene_alpha,
            &mut maker.materials,
            &shells,
        );
    }
}

fn set_contents_alpha(
    root: Entity,
    hierarchy: &Query<&Children>,
    parts: &mut Query<fade::ScenePart>,
    alpha: f32,
    materials: &mut Assets<ColorMaterial>,
    shells: &Query<&FolderShell>,
) {
    for part in hierarchy.iter_descendants(root) {
        if let Ok(part) = parts.get_mut(part) {
            fade::set_alpha(part, alpha, materials, shells);
        }
    }
}

fn set_preview_alpha(
    tile: Entity,
    hierarchy: &Query<&Children>,
    previews: &Query<(), With<FolderPreview>>,
    parts: &mut Query<fade::ScenePart>,
    alpha: f32,
    materials: &mut Assets<ColorMaterial>,
    shells: &Query<&FolderShell>,
) {
    if let Ok(children) = hierarchy.get(tile) {
        for child in children.iter().filter(|&child| previews.contains(child)) {
            if let Ok(part) = parts.get_mut(child) {
                fade::set_alpha(part, alpha, materials, shells);
            }
            set_contents_alpha(child, hierarchy, parts, alpha, materials, shells);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_dots_fade_with_the_thumbnail_but_the_plate_does_not() {
        use bevy::ecs::system::SystemState;

        let mut world = World::new();
        let tile = world.spawn_empty().id();
        let placeholder = world.spawn((FolderPreview, ChildOf(tile))).id();
        let mut materials = Assets::<ColorMaterial>::default();
        let dots = [0, 1, 2].map(|_| {
            let material = materials.add(ColorMaterial::from(Color::WHITE.with_alpha(0.7)));
            world.spawn((
                ChildOf(placeholder),
                BaseAlpha(0.7),
                MeshMaterial2d(material.clone()),
            ));
            material
        });
        let thumbnail = world
            .spawn((
                FolderPreview,
                ChildOf(tile),
                BaseAlpha(1.0),
                Sprite::default(),
            ))
            .id();
        let plate = world
            .spawn((ChildOf(tile), BaseAlpha(1.0), Sprite::default()))
            .id();

        let mut system = SystemState::<(
            Query<&Children>,
            Query<(), With<FolderPreview>>,
            Query<fade::ScenePart>,
            Query<&FolderShell>,
        )>::new(&mut world);
        for scale in [4.0, 1.0, 0.25] {
            let alpha = 1.0 - live_alpha(4.0, 0.25, scale);
            let (hierarchy, previews, mut parts, shells) = system.get_mut(&mut world).unwrap();
            set_preview_alpha(
                tile,
                &hierarchy,
                &previews,
                &mut parts,
                alpha,
                &mut materials,
                &shells,
            );
            for dot in &dots {
                assert_eq!(materials.get(dot).unwrap().color.alpha(), 0.7 * alpha);
            }
            assert_eq!(world.get::<Sprite>(thumbnail).unwrap().color.alpha(), alpha);
            assert_eq!(world.get::<Sprite>(plate).unwrap().color.alpha(), 1.0);
        }
    }

    #[test]
    fn live_alpha_follows_logarithmic_zoom_in_both_directions() {
        let parent = 4.0_f32;
        let child = 0.25_f32;
        let sample = |p: f32| live_alpha(parent, child, parent * (child / parent).powf(p));
        assert_eq!(sample(0.0), 0.0);
        assert_eq!(sample(0.11), 0.0);
        assert!((sample((0.12 + 0.95) / 2.0) - 0.5).abs() < 0.001);
        assert!(sample(0.8) > sample(0.2));
        assert_eq!(sample(0.96), 1.0);
        assert_eq!(sample(1.0), 1.0);
        assert_eq!(live_alpha(parent, parent, child), 0.0);
    }
}
