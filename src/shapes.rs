//! 四角・三角・円を描く小さな道具。どれも画面上で小さすぎたら隠す（LOD）。

use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite_render::{AlphaMode2d, ColorMaterial, MeshMaterial2d};

/// 図形。画面上で `min_size`（ワールド座標）が小さすぎたら隠す。
#[derive(Component)]
pub struct LodShape {
    pub min_size: f32,
}

pub struct Painter<'a, 'w, 's> {
    pub commands: &'a mut Commands<'w, 's>,
    pub meshes: &'a mut Assets<Mesh>,
    pub materials: &'a mut Assets<ColorMaterial>,
}

impl Painter<'_, '_, '_> {
    pub fn rect(&mut self, rect: Rect, color: Color, z: f32) {
        self.commands.spawn((
            LodShape {
                min_size: rect.width().min(rect.height()),
            },
            Sprite::from_color(color, rect.size()),
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
        self.spawn_mesh(mesh, size.x.min(size.y), Vec2::ZERO, color, z);
    }

    pub fn circle(&mut self, center: Vec2, radius: f32, color: Color, z: f32) {
        let mesh = self.meshes.add(Circle::new(radius));
        self.spawn_mesh(mesh, radius * 2.0, center, color, z);
    }

    fn spawn_mesh(&mut self, mesh: Handle<Mesh>, min_size: f32, at: Vec2, color: Color, z: f32) {
        let material = self.materials.add(ColorMaterial {
            color,
            alpha_mode: AlphaMode2d::Blend,
            ..default()
        });
        self.commands.spawn((
            LodShape { min_size },
            Mesh2d(mesh),
            MeshMaterial2d(material),
            Transform::from_translation(at.extend(z)),
        ));
    }
}
