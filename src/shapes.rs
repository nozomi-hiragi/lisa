//! 四角・三角・円・画像・文字を、ひとつの場面（シーン）の子として描く道具。

use bevy::camera::visibility::RenderLayers;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::{AlphaMode2d, ColorMaterial, MeshMaterial2d};

/// 画面上でこれより小さい図形は描かない（論理ピクセル）。
const MIN_SHAPE_PX: f32 = 1.0;
/// 画面上でこれより小さい文字は描かない（論理ピクセル）。
const MIN_LABEL_PX: f32 = 4.0;

/// 元の不透明度。フェードのときにこれに係数を掛ける。
#[derive(Component)]
pub struct BaseAlpha(pub f32);

pub struct Painter<'a, 'w, 's> {
    pub commands: &'a mut Commands<'w, 's>,
    pub meshes: &'a mut Assets<Mesh>,
    pub materials: &'a mut Assets<ColorMaterial>,
    /// 描いたものをぶら下げる場面の根。
    pub parent: Entity,
    /// どのカメラに映すか。
    pub layer: RenderLayers,
    /// 1 論理ピクセルあたりのワールド座標の長さ（この倍率で見たときに合わせて描く）。
    pub scale: f32,
    pub font: Handle<Font>,
}

impl Painter<'_, '_, '_> {
    fn visibility(&self, min_size: f32, min_px: f32) -> Visibility {
        if min_size / self.scale >= min_px {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        }
    }

    fn common(&self, color: Color, min_size: f32) -> impl Bundle {
        (
            ChildOf(self.parent),
            self.layer.clone(),
            BaseAlpha(color.alpha()),
            self.visibility(min_size, MIN_SHAPE_PX),
        )
    }

    pub fn rect(&mut self, rect: Rect, color: Color, z: f32) {
        let common = self.common(color, rect.width().min(rect.height()));
        self.commands.spawn((
            common,
            Sprite::from_color(color, rect.size()),
            Transform::from_translation(rect.center().extend(z)),
        ));
    }

    pub fn image(&mut self, rect: Rect, image: Handle<Image>, z: f32) {
        let common = self.common(Color::WHITE, rect.width().min(rect.height()));
        self.commands.spawn((
            common,
            Sprite {
                image,
                custom_size: Some(rect.size()),
                ..default()
            },
            Transform::from_translation(rect.center().extend(z)),
        ));
    }

    pub fn triangle(&mut self, points: [Vec2; 3], color: Color, z: f32) {
        let min = points[0].min(points[1]).min(points[2]);
        let max = points[0].max(points[1]).max(points[2]);
        let size = max - min;
        let mesh = self
            .meshes
            .add(Triangle2d::new(points[0], points[1], points[2]));
        self.mesh(mesh, size.x.min(size.y), Vec2::ZERO, color, z);
    }

    pub fn circle(&mut self, center: Vec2, radius: f32, color: Color, z: f32) {
        let mesh = self.meshes.add(Circle::new(radius));
        self.mesh(mesh, radius * 2.0, center, color, z);
    }

    fn mesh(&mut self, mesh: Handle<Mesh>, min_size: f32, at: Vec2, color: Color, z: f32) {
        let material = self.materials.add(ColorMaterial {
            color,
            alpha_mode: AlphaMode2d::Blend,
            ..default()
        });
        let common = self.common(color, min_size);
        self.commands.spawn((
            common,
            Mesh2d(mesh),
            MeshMaterial2d(material),
            Transform::from_translation(at.extend(z)),
        ));
    }

    /// 1行の文字。`height` と `width` はワールド座標での大きさの上限。
    /// 文字はワールド座標で大きさが決まり、この場面の倍率でくっきり見えるように作る。
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        text: String,
        at: Vec2,
        anchor: Anchor,
        height: f32,
        width: f32,
        color: Color,
        z: f32,
    ) {
        let world_size = height.min(width * 0.95 / em_width(&text));
        let px = (world_size / self.scale).ceil();
        if px < MIN_LABEL_PX {
            return;
        }
        self.commands.spawn((
            ChildOf(self.parent),
            self.layer.clone(),
            BaseAlpha(color.alpha()),
            Text2d::new(text),
            TextLayout::no_wrap(),
            TextFont::from_font_size(px).with_font(self.font.clone()),
            TextColor(color),
            anchor,
            // ワールドでの大きさが `world_size` になるように縮める。
            Transform::from_translation(at.extend(z)).with_scale(Vec3::splat(world_size / px)),
        ));
    }
}

/// 文字列の幅の見積もり。日本語はほぼ正方形、英数字はその半分強。
fn em_width(text: &str) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
        .sum()
}

/// `base` の上に `over` を不透明度 `t` で重ねた色（不透明）。
/// 半透明のまま画像に焼くと色が変わるので、重ねた結果を不透明な色として描く。
pub fn over(base: Color, over: Color, t: f32) -> Color {
    let a = LinearRgba::from(base);
    let b = LinearRgba::from(over);
    Color::from(LinearRgba::rgb(
        a.red + (b.red - a.red) * t,
        a.green + (b.green - a.green) * t,
        a.blue + (b.blue - a.blue) * t,
    ))
}
