//! 一覧の中心にある子フォルダの実物を、サムネと切り替える。

use super::*;
use crate::render::scene::{FolderDisplay, FolderPreview, FolderShell, FolderTile};
use crate::render::shapes::Painter;
use crate::render::status::FolderBodyState;

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

const REQUEST_AT: f32 = 0.12;

#[derive(Clone, Copy)]
struct Focus {
    tile: Entity,
    folder: NodeId,
    progress: f32,
    requested: bool,
}

#[derive(Resource, Default)]
pub(crate) struct CenterApproach(Option<Focus>);

impl CenterApproach {
    pub(crate) fn requested(&self) -> Option<NodeId> {
        self.0
            .filter(|focus| focus.requested)
            .map(|focus| focus.folder)
    }
}

fn zoom_progress(parent: f32, child: f32, current: f32) -> f32 {
    if !(0.0 < child && child < parent && current > 0.0) {
        return 0.0;
    }
    ((parent / current).ln() / (parent / child).ln()).clamp(0.0, 1.0)
}

fn requests_preview(progress: f32, click_target: Option<bool>) -> bool {
    click_target.unwrap_or(progress > REQUEST_AT)
}

fn live_alpha(progress: f32) -> f32 {
    let x = ((progress - REQUEST_AT) / (0.95 - REQUEST_AT)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// アニメーション中の通常遷移ではサムネだけが実物と重なれる。
fn approach_alpha(body: &FolderBodyState, progress: f32, loaded: bool) -> f32 {
    if !loaded || matches!(body, FolderBodyState::Failed | FolderBodyState::Empty) {
        0.0
    } else if matches!(body, FolderBodyState::Thumbnail(_)) {
        live_alpha(progress)
    } else {
        f32::from(progress >= 0.90)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update(
    mut selection: Local<Selection>,
    mut maker: SceneMaker,
    current: Res<CurrentScene>,
    approach: Res<CenterApproach>,
    camera: Single<&Projection, With<MainCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    gesture: Res<crate::input::gesture::Gesture>,
    roots: Query<(&SceneRoot, Option<&Fade>)>,
    tiles: Query<(Entity, &FolderTile, &ChildOf)>,
    hierarchy: Query<&Children>,
    previews: Query<(), With<FolderPreview>>,
    all: Query<Entity>,
    mut parts: Query<fade::ScenePart>,
    shells: Query<&FolderShell>,
    displays: Query<&FolderDisplay>,
) {
    let Projection::Orthographic(ortho) = *camera else {
        return;
    };
    let Ok((root, fade)) = roots.get(current.0) else {
        return;
    };
    if !maker.layouts.contains(root.folder) {
        return;
    }
    // ピンチで親を下に敷いている間は、既にある本シーンを実物として使う。
    let appearance = |tile| {
        displays
            .get(tile)
            .map_or(tile, |display| display.appearance)
    };
    let outward = gesture.parent_scene_entity().and_then(|parent_scene| {
        let (parent, _) = roots.get(parent_scene).ok()?;
        let (tile, _, _) = tiles
            .iter()
            .find(|(_, tile, _)| tile.scene == parent_scene && tile.folder == root.folder)?;
        let display = displays.get(tile).ok()?;
        maker.layouts.contains(parent.folder).then_some((
            current.0,
            tile,
            parent.folder,
            &display.body,
        ))
    });
    if selection.outward != outward.map(|(scene, tile, _, _)| (scene, tile)) {
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
                appearance(tile),
                &hierarchy,
                &previews,
                &mut parts,
                1.0,
                &mut maker.materials,
                &shells,
            );
        }
    }
    if let Some((scene, tile, parent, body)) = outward {
        if let Some(old) = selection.tile.take() {
            set_preview_alpha(
                appearance(old),
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
        let progress = zoom_progress(parent_scale, child_scale, ortho.scale);
        let alpha = if matches!(body, FolderBodyState::Thumbnail(_)) {
            live_alpha(progress)
        } else {
            // 逆方向でも、待ち表示と実物を同時に出さない。
            f32::from(progress >= 0.90)
        };
        set_contents_alpha(
            scene,
            &hierarchy,
            &mut parts,
            alpha,
            &mut maker.materials,
            &shells,
        );
        set_preview_alpha(
            appearance(tile),
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
    let candidate = approach.0;
    let scene_alpha = fade.map_or(1.0, Fade::alpha);
    if selection.tile != candidate.map(|focus| focus.tile) {
        if let Some(old_tile) = selection.tile.take()
            && let Ok((_, tile, _)) = tiles.get(old_tile)
        {
            let alpha = roots
                .get(tile.scene)
                .ok()
                .and_then(|(_, fade)| fade)
                .map_or(1.0, Fade::alpha);
            set_preview_alpha(
                appearance(old_tile),
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
    }
    if let Some(Focus {
        tile,
        folder,
        progress,
        ..
    }) = candidate
    {
        let Ok(display) = displays.get(tile) else {
            return;
        };
        let alpha = approach_alpha(&display.body, progress, maker.tree.nodes[folder].loaded);
        if selection.contents.is_none() && alpha > 0.0 {
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
                selection.contents = Some(entity);
            }
        }
        selection.tile = Some(tile);
        let Some(contents) = selection.contents else {
            set_preview_alpha(
                appearance(tile),
                &hierarchy,
                &previews,
                &mut parts,
                scene_alpha,
                &mut maker.materials,
                &shells,
            );
            return;
        };
        // 作った直後の部分フェードは、子がクエリから見える次のフレームで始める。
        let alpha = if !all.contains(contents) && (alpha < 1.0 || scene_alpha < 1.0) {
            0.0
        } else {
            alpha
        };
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
            appearance(tile),
            &hierarchy,
            &previews,
            &mut parts,
            (1.0 - alpha) * scene_alpha,
            &mut maker.materials,
            &shells,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn choose(
    mut approach: ResMut<CenterApproach>,
    current: Res<CurrentScene>,
    nav: Res<Nav>,
    tree: Res<FileTree>,
    layouts: Res<Layouts>,
    camera: Single<(&Transform, &Projection), With<MainCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    animating: Option<Res<Animating>>,
    gesture: Res<crate::input::gesture::Gesture>,
    roots: Query<&SceneRoot>,
    tiles: Query<(Entity, &FolderTile, &ChildOf)>,
    groups: Query<&Transform, Without<MainCamera>>,
) {
    approach.0 = None;
    if gesture.parent_scene_entity().is_some() {
        return;
    }
    let (transform, projection) = *camera;
    let Projection::Orthographic(ortho) = projection else {
        return;
    };
    let Ok(root) = roots.get(current.0) else {
        return;
    };
    if !layouts.contains(root.folder) {
        return;
    }
    let parent_scale = fit(layouts[root.folder].fit, &window).1;
    let origin = transform.translation.truncate();
    let view = Rect::from_center_size(
        origin,
        Vec2::new(view_width(&window), window.height()) * ortho.scale,
    );
    let preferred = animating.as_ref().and_then(|anim| {
        if anim.then_show {
            scene_path(&nav, &tree).last().map(|level| level.node)
        } else {
            root.focus
        }
    });
    let candidate = tiles
        .iter()
        .filter(|(_, tile, _)| tile.scene == current.0 && layouts.contains(tile.folder))
        .filter_map(|(entity, tile, parent)| {
            let cell = layouts[tile.folder].cell;
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
    if let Some((tile, folder, priority, _)) = candidate {
        let child_scale = fit(layouts[folder].fit, &window).1;
        let progress = zoom_progress(parent_scale, child_scale, ortho.scale);
        let requested = requests_preview(
            progress,
            animating.as_ref().map(|anim| anim.then_show && priority),
        );
        approach.0 = Some(Focus {
            tile,
            folder,
            progress,
            requested,
        });
    }
}

pub(super) fn refresh_displays(
    mut maker: SceneMaker,
    approach: Res<CenterApproach>,
    capture: Res<cache::ThumbnailCapture>,
    mut displays: Query<(Entity, &mut FolderDisplay)>,
) {
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
    for (tile, mut display) in &mut displays {
        let id = display.folder;
        if !maker.layouts.contains(id) {
            continue;
        }
        let pending = approach.requested() == Some(id) || capture.is_pending(&maker.tree, id);
        let body = scene::folder_body(&maker.tree, &maker.images, id, pending);
        let load_state = maker.tree.nodes[id].state;
        if display.body == body && display.load_state == load_state {
            continue;
        }
        maker.commands.entity(display.appearance).despawn();
        display.appearance = maker
            .commands
            .spawn((ChildOf(tile), Transform::default(), Visibility::default()))
            .id();
        display.body = body;
        display.load_state = load_state;
        let mut painter = Painter {
            commands: &mut maker.commands,
            meshes: &mut maker.meshes,
            materials: &mut maker.materials,
            parent: display.appearance,
            layer: RenderLayers::layer(CONTENT_LAYER),
            scale: display.scale,
            font: maker.font.0.clone(),
            theme: &maker.theme,
            no_cull: display.no_cull,
        };
        scene::draw_folder(&mut painter, &source, id, &display.body, true);
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
    fn approach_replaces_dots_with_spinner_before_showing_live_contents() {
        use crate::model::tree::LoadState;
        let choose = |progress, click, image| {
            let pending = requests_preview(progress, click);
            FolderBodyState::new(LoadState::Loaded, image, false, pending)
        };
        let idle = choose(0.12, None, None);
        assert_eq!(idle, FolderBodyState::UnloadedDots);
        assert_eq!(approach_alpha(&idle, 0.12, true), 0.0);
        assert!(!requests_preview(0.5, Some(false)));
        let approaching = choose(0.40, None, None);
        assert_eq!(approaching, FolderBodyState::LoadingSpinner);
        assert_eq!(choose(0.0, Some(true), None), approaching);
        assert_eq!(approach_alpha(&approaching, 0.89, true), 0.0);
        let captured = choose(0.40, None, Some(Handle::default()));
        assert!(matches!(captured, FolderBodyState::Thumbnail(_)));
        assert!(approach_alpha(&captured, 0.40, true) > 0.0);
        // 読み戻しが間に合わないときは、中間のアルファを一切使わない。
        assert_eq!(approach_alpha(&approaching, 0.90, true), 1.0);
        assert_eq!(approach_alpha(&approaching, 1.0, false), 0.0);
    }

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
            let alpha = 1.0 - live_alpha(zoom_progress(4.0, 0.25, scale));
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
        let sample = |p: f32| {
            live_alpha(zoom_progress(
                parent,
                child,
                parent * (child / parent).powf(p),
            ))
        };
        assert_eq!(sample(0.0), 0.0);
        assert_eq!(sample(0.11), 0.0);
        assert!((sample((0.12 + 0.95) / 2.0) - 0.5).abs() < 0.001);
        assert!(sample(0.8) > sample(0.2));
        assert_eq!(sample(0.96), 1.0);
        assert_eq!(sample(1.0), 1.0);
        assert_eq!(live_alpha(zoom_progress(parent, parent, child)), 0.0);
    }
}
