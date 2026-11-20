//! 四角・三角・円・画像・文字を、ひとつの場面（シーン）の子として描く道具。

use bevy::camera::visibility::{NoFrustumCulling, RenderLayers};
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

/// 実行ファイル単体でも HUD を描けるよう、同梱の PNG を起動時に展開する。
#[derive(Resource)]
pub struct UiThemeAssets {
    pub background: Option<Handle<Image>>,
    frame: Option<Handle<Image>>,
    ear: Option<Handle<Image>>,
}

impl UiThemeAssets {
    pub fn load(images: &mut Assets<Image>) -> Self {
        let mut load = |bytes: &[u8]| match image::load_from_memory_with_format(
            bytes,
            image::ImageFormat::Png,
        ) {
            Ok(image) => Some(images.add(Image::from_dynamic(image, true, default()))),
            Err(error) => {
                warn!("HUD の画像を読み込めませんでした: {error}");
                None
            }
        };
        let frame = load(include_bytes!("../../assets/ui/folder_frame_9slice.png"));
        let ear = load(include_bytes!("../../assets/ui/folder_ear_3slice.png"));
        let background = load(include_bytes!("../../assets/ui/bg_hud.png"));
        if let Some(mut image) = background
            .as_ref()
            .and_then(|handle| images.get_mut(handle))
        {
            use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
            image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
                address_mode_u: ImageAddressMode::Repeat,
                address_mode_v: ImageAddressMode::Repeat,
                ..ImageSamplerDescriptor::linear()
            });
        }
        Self {
            background,
            frame,
            ear,
        }
    }
}

/// 動的なスライス。描画位置と画像内の切り出し範囲を別々に持つ。
#[derive(Clone, Copy)]
pub enum SlicePart {
    Frame(usize),
    Ear(usize),
    Plate,
    EarFallback,
    EarLiftPlate,
    Thumbnail,
    Interior,
}

pub struct SlicePlacement {
    pub rect: Rect,
    pub z: f32,
    pub uv: Option<Rect>,
    pub scale: f32,
}

impl SlicePlacement {
    pub fn apply(
        self,
        sprite: &mut Sprite,
        transform: &mut Transform,
        visibility: &mut Visibility,
    ) {
        if let Some(uv) = self.uv {
            sprite.rect = Some(uv);
        }
        sprite.custom_size = Some(self.rect.size().max(Vec2::ZERO));
        *transform = Transform::from_translation(self.rect.center().extend(self.z));
        *visibility = if self.rect.width().min(self.rect.height()) >= MIN_SHAPE_PX * self.scale {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

// 中央は透過なので省く。最後の 1 枚は耳の左端と本体の左レールをつなぐ。
const FRAME_UVS: [(usize, [f32; 4]); 9] = [
    (0, [0., 0., 16., 16.]),
    (1, [16., 0., 112., 4.]),
    (2, [112., 0., 128., 16.]),
    (3, [0., 16., 16., 112.]),
    (5, [112., 16., 128., 112.]),
    (6, [0., 112., 16., 128.]),
    (7, [16., 112., 112., 128.]),
    (8, [112., 112., 128., 128.]),
    (9, [0., 16., 16., 32.]),
];
const EAR_UVS: [[f32; 4]; 3] = [
    [0., 0., 16., 64.],
    [16., 0., 64., 64.],
    [64., 0., 128., 64.],
];

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
    pub theme: &'a UiThemeAssets,
    /// true なら、これから描くものを視錐台カリングしない（見えているのに消えてちらつくのを防ぐ）。
    pub no_cull: bool,
}

impl Painter<'_, '_, '_> {
    /// 部品を作る。`no_cull` なら視錐台カリングしない印を付ける。
    pub fn spawn_part(&mut self, bundle: impl Bundle) -> Entity {
        let mut entity = self.commands.spawn(bundle);
        if self.no_cull {
            entity.insert(NoFrustumCulling);
        }
        entity.id()
    }

    fn visibility(&self, min_size: f32, min_px: f32) -> Visibility {
        if min_size / self.scale >= min_px {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        }
    }

    fn common(&self, color: Color, visibility: Visibility) -> impl Bundle {
        (
            ChildOf(self.parent),
            self.layer.clone(),
            BaseAlpha(color.alpha()),
            visibility,
        )
    }

    pub fn rect(&mut self, rect: Rect, color: Color, z: f32) {
        let common = self.common(
            color,
            self.visibility(rect.size().min_element(), MIN_SHAPE_PX),
        );
        self.spawn_part((
            common,
            Sprite::from_color(color, rect.size()),
            Transform::from_translation(rect.center().extend(z)),
        ));
    }

    pub fn image(&mut self, rect: Rect, image: Handle<Image>, z: f32) {
        let common = self.common(
            Color::WHITE,
            self.visibility(rect.size().min_element(), MIN_SHAPE_PX),
        );
        self.spawn_part((
            common,
            Sprite {
                image,
                custom_size: Some(rect.size()),
                ..default()
            },
            Transform::from_translation(rect.center().extend(z)),
        ));
    }

    pub fn slice_9(
        &mut self,
        color: Color,
        place: impl Fn(SlicePart) -> SlicePlacement,
    ) -> Vec<(Entity, SlicePart)> {
        let Some(image) = self.theme.frame.clone() else {
            // 本体は別に描いてあるため、枠画像が無いときも表示できる。
            return Vec::new();
        };
        FRAME_UVS
            .iter()
            .map(|&(index, uv)| {
                let part = SlicePart::Frame(index);
                (
                    self.texture_slice(image.clone(), uv, color, place(part)),
                    part,
                )
            })
            .collect()
    }

    pub fn slice_3_horizontal(
        &mut self,
        color: Color,
        place: impl Fn(SlicePart) -> SlicePlacement,
    ) -> Vec<(Entity, SlicePart)> {
        let Some(image) = self.theme.ear.clone() else {
            let part = SlicePart::EarFallback;
            let fill = hud_plate_color(color);
            return vec![(
                self.slice_sprite(Sprite::from_color(fill, Vec2::ONE), place(part)),
                part,
            )];
        };
        EAR_UVS
            .iter()
            .enumerate()
            .map(|(index, &uv)| {
                let part = SlicePart::Ear(index);
                (
                    self.texture_slice(image.clone(), uv, color, place(part)),
                    part,
                )
            })
            .collect()
    }

    fn texture_slice(
        &mut self,
        image: Handle<Image>,
        [x0, y0, x1, y1]: [f32; 4],
        color: Color,
        placement: SlicePlacement,
    ) -> Entity {
        self.slice_sprite(
            Sprite {
                image,
                rect: Some(Rect::new(x0, y0, x1, y1)),
                color,
                ..default()
            },
            placement,
        )
    }

    pub fn slice_sprite(&mut self, mut sprite: Sprite, placement: SlicePlacement) -> Entity {
        let mut transform = Transform::default();
        let mut visibility = Visibility::default();
        placement.apply(&mut sprite, &mut transform, &mut visibility);
        let common = self.common(sprite.color, visibility);
        self.spawn_part((common, sprite, Anchor::CENTER, transform))
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
        let common = self.common(color, self.visibility(min_size, MIN_SHAPE_PX));
        self.spawn_part((
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
        self.spawn_part((
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
pub fn em_width(text: &str) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
        .sum()
}

/// 耳の画像と同じ色・不透明度を使った本体プレート。
pub fn hud_plate_color(tint: Color) -> Color {
    let plate = LinearRgba::from(crate::render::palette::BASE_100);
    let tint = LinearRgba::from(tint);
    Color::from(LinearRgba::new(
        plate.red * tint.red,
        plate.green * tint.green,
        plate.blue * tint.blue,
        tint.alpha * (160.0 / 255.0),
    ))
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
