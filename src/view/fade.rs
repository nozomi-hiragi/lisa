//! 場面のフェード（画像から本物の描画への切り替えなど）。

use super::*;
use crate::render::scene::FolderShell;

/// 画像から本物の描画へ切り替えるときのフェードの時間（秒）。
const FADE_SECONDS: f32 = 0.1;

/// 場面のフェード。終わったら、フェードアウトなら場面を消し、フェードインなら `replaces` を消す。
#[derive(Component, Clone)]
pub(crate) struct Fade {
    pub(crate) started: Instant,
    pub(crate) fade_in: bool,
    pub(crate) replaces: Option<Entity>,
}

/// 場面の部品のうち、フェードで不透明度を変えるもの。
pub(super) type ScenePart = (
    &'static BaseAlpha,
    Option<&'static mut Sprite>,
    Option<&'static mut TextColor>,
    Option<&'static MeshMaterial2d<ColorMaterial>>,
    Option<&'static ChildOf>,
);

impl Fade {
    pub(super) fn alpha(&self) -> f32 {
        let k = (self.started.elapsed().as_secs_f32() / FADE_SECONDS).min(1.0);
        if self.fade_in { k } else { 1.0 - k }
    }
}

pub(super) fn set_alpha(
    (base, sprite, text, material, parent): (
        &BaseAlpha,
        Option<Mut<Sprite>>,
        Option<Mut<TextColor>>,
        Option<&MeshMaterial2d<ColorMaterial>>,
        Option<&ChildOf>,
    ),
    alpha: f32,
    materials: &mut Assets<ColorMaterial>,
    shells: &Query<&FolderShell>,
) {
    let shell = parent.and_then(|parent| shells.get(parent.parent()).ok());
    let a = base.0 * alpha * shell.map_or(1.0, |shell| shell.alpha);
    if let Some(mut sprite) = sprite {
        sprite.color.set_alpha(a);
    }
    if let Some(mut text) = text {
        text.0.set_alpha(a);
    }
    if let Some(handle) = material.map(|material| &material.0)
        && materials
            .get(handle)
            .is_some_and(|material| material.color.alpha() != a)
        && let Some(mut material) = materials.get_mut(handle)
    {
        material.color.set_alpha(a);
    }
}

pub(crate) fn fade_scenes(
    mut commands: Commands,
    scenes: Query<(Entity, &Fade), With<SceneRoot>>,
    hierarchy: Query<&Children>,
    mut parts: Query<ScenePart>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    shells: Query<&FolderShell>,
) {
    for (entity, fade) in &scenes {
        let k = (fade.started.elapsed().as_secs_f32() / FADE_SECONDS).min(1.0);
        let alpha = if fade.fade_in { k } else { 1.0 - k };
        for part in hierarchy.iter_descendants(entity) {
            if let Ok(part) = parts.get_mut(part) {
                set_alpha(part, alpha, &mut materials, &shells);
            }
        }
        if k < 1.0 {
            continue;
        }
        if fade.fade_in {
            commands.entity(entity).remove::<Fade>();
            if let Some(old) = fade.replaces {
                commands.entity(old).despawn();
            }
        } else {
            commands.entity(entity).despawn();
        }
    }
}
