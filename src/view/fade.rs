//! 場面のフェード（画像から本物の描画への切り替えなど）。

use super::*;

/// 画像から本物の描画へ切り替えるときのフェードの時間（秒）。
const FADE_SECONDS: f32 = 0.1;

/// 場面のフェード。終わったら、フェードアウトなら場面を消し、フェードインなら `replaces` を消す。
#[derive(Component)]
pub(crate) struct Fade {
    pub(crate) started: Instant,
    pub(crate) fade_in: bool,
    pub(crate) replaces: Option<Entity>,
}

/// 場面の部品のうち、フェードで不透明度を変えるもの。
type ScenePart = (
    &'static BaseAlpha,
    Option<&'static mut Sprite>,
    Option<&'static mut TextColor>,
    Option<&'static MeshMaterial2d<ColorMaterial>>,
);

pub(crate) fn fade_scenes(
    mut commands: Commands,
    scenes: Query<(Entity, &Fade), With<SceneRoot>>,
    hierarchy: Query<&Children>,
    mut parts: Query<ScenePart>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    for (entity, fade) in &scenes {
        let k = (fade.started.elapsed().as_secs_f32() / FADE_SECONDS).min(1.0);
        let alpha = if fade.fade_in { k } else { 1.0 - k };
        for part in hierarchy.iter_descendants(entity) {
            let Ok((base, sprite, text, material)) = parts.get_mut(part) else {
                continue;
            };
            let a = base.0 * alpha;
            if let Some(mut sprite) = sprite {
                sprite.color.set_alpha(a);
            }
            if let Some(mut text) = text {
                text.0.set_alpha(a);
            }
            if let Some(mut material) = material.and_then(|m| materials.get_mut(&m.0)) {
                material.color.set_alpha(a);
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
