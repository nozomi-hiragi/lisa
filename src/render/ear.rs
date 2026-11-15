//! フォルダの耳（本体の上に乗る、名前の入ったタブ）。
//!
//! 耳の高さは画面上のピクセル数で 2 つ決める（設定）。
//! - 開いたとき: そのフォルダを開いて本体が画面に収まったときの高さ。
//! - 一覧の中: 親のフォルダを開いたとき、一覧の中に見えている高さ。
//!
//! 開いた状態から親の一覧の大きさまで縮めるあいだは、画面上の高さを前者から後者へなめらかに
//! 変える（本体よりゆっくり縮む）。開いた状態より拡大すれば本体と一緒に大きくなり、親の一覧の
//! 大きさより縮めれば本体と一緒に小さくなる。
//!
//! 今開いているフォルダとその外側、一覧に見えている子の耳は、毎フレーム大きさを合わせる
//! （`LiveEar`）。キャッシュの画像の中の耳は、一覧の中の大きさで焼く。

use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;

use crate::render::shapes::{BaseAlpha, Painter, em_width, over};
use crate::view::MainCamera;

/// 耳の横幅の最小（本体の幅に対する割合）。
pub const MIN_WIDTH: f32 = 1.0 / 3.0;
/// 耳の高さに対する、名前の文字の大きさ。
const TEXT_RATIO: f32 = 0.55;
/// 画面上でこれより小さい名前は描かない（論理ピクセル）。
const MIN_TEXT_PX: f32 = 4.0;

/// 耳の大きさを決める値。
#[derive(Resource, Clone, Copy)]
pub struct EarSizes {
    /// 開いたフォルダの本体を画面に収めたときの幅（論理ピクセル）。
    pub fitted_width: f32,
    /// 開いたときの耳の高さ（論理ピクセル）。
    pub selected_px: f32,
    /// 一覧の中の耳の高さ（論理ピクセル）。
    pub preview_px: f32,
}

impl EarSizes {
    /// 本体の幅が `body_width`、親の本体の幅が `parent_width` のフォルダの耳の高さ
    /// （ワールド座標）。`in_list` は親を開いたときの耳の高さ（ワールド座標、タイルが小さいと
    /// 設定より小さくなる）。`scale` はカメラの倍率（1 論理ピクセルあたりのワールドの長さ）。
    pub fn height(&self, body_width: f32, parent_width: f32, in_list: f32, scale: f32) -> f32 {
        // そのフォルダを開いたときの倍率と、親を開いたときの倍率。
        let selected = body_width / self.fitted_width;
        let preview = parent_width / self.fitted_width;
        if scale <= selected {
            return self.selected_px * selected;
        }
        if scale >= preview || preview <= selected {
            return in_list;
        }
        // 画面上の高さを、倍率の対数に沿って変える。
        let preview_px = in_list / preview;
        let t = (scale / selected).ln() / (preview / selected).ln();
        (self.selected_px + (preview_px - self.selected_px) * t) * scale
    }

    /// 名前を作るときの文字の大きさ（ピクセル）。ズームで拡大縮小するので、大きめに作る。
    fn raster_px(&self) -> f32 {
        (self.selected_px.max(self.preview_px) * TEXT_RATIO * 1.5).ceil()
    }
}

/// 1 つのタイルの枠の中での置き方。枠（`cell`）の中を、本体（フォルダの中身かファイルの
/// プレビュー）と、耳（ファイルは名前）で分け合う。耳が大きくなったら、本体が縮んで譲る。
#[derive(Clone, Copy)]
pub struct Slot {
    pub cell: Rect,
    /// 開いたとき（耳が小さいとき）の本体。
    pub body: Rect,
    /// ファイルなら true（名前は本体の下）。フォルダは耳が本体の上。
    pub name_below: bool,
}

impl Slot {
    /// 耳（名前）に高さ `h` を取ったときの、本体の縮み（倍率）と、見えている本体の四角。
    pub fn yielded(&self, h: f32) -> (f32, Rect) {
        let room = (self.cell.height() - h).max(self.body.height() * 0.05);
        let k = (room / self.body.height()).min(1.0);
        let size = self.body.size() * k;
        let top = if self.name_below {
            self.cell.max.y
        } else {
            (self.cell.max.y - h).min(self.body.max.y)
        };
        let center = Vec2::new(self.body.center().x, top - size.y / 2.0);
        (k, Rect::from_center_size(center, size))
    }

    /// 本体（開いたときの形で描いたもの）を、見えている所へ写す変形。
    fn body_transform(&self, h: f32) -> Transform {
        let (k, shown) = self.yielded(h);
        Transform::from_translation((shown.min - self.body.min * k).extend(0.0))
            .with_scale(Vec3::new(k, k, 1.0))
    }
}

/// 耳の高さを決めるもの（`EarSizes::height` の引数）。
#[derive(Clone, Copy)]
pub struct EarOf {
    /// 耳の高さを決めるときの本体の幅（ファイルの名前は、同じ一覧のフォルダの本体の幅）。
    pub size_width: f32,
    /// 親の本体の幅。
    pub parent_width: f32,
    /// 親を開いたときの耳の高さ（ワールド座標）。
    pub in_list: f32,
}

impl EarOf {
    fn height(&self, sizes: &EarSizes, scale: f32) -> f32 {
        sizes.height(self.size_width, self.parent_width, self.in_list, scale)
    }
}

/// 耳の大きさの決め方。ズームに合わせて変えるか（本物の場面）、決まった大きさか（画像に焼くとき）。
#[derive(Clone, Copy)]
pub enum Size<'a> {
    Live(&'a EarSizes, EarOf),
    Fixed(f32),
}

impl Size<'_> {
    fn height(&self, scale: f32) -> f32 {
        match self {
            Size::Live(sizes, of) => of.height(sizes, scale),
            Size::Fixed(h) => *h,
        }
    }
}

/// 耳に譲って縮む本体の入れ物を作る。本体はこの中に、開いたときの形で描く。
pub fn body_group(painter: &mut Painter, slot: Slot, size: Size) -> Entity {
    let transform = slot.body_transform(size.height(painter.scale));
    let mut entity = painter.commands.spawn((
        ChildOf(painter.parent),
        painter.layer.clone(),
        transform,
        Visibility::default(),
    ));
    if let Size::Live(_, of) = size {
        entity.insert(YieldBody { slot, of });
    }
    entity.id()
}

/// ズームに合わせて縮む本体の入れ物。
#[derive(Component)]
pub struct YieldBody {
    slot: Slot,
    of: EarOf,
}

/// フォルダの耳（四角と名前）を描く。
pub fn draw_ear(painter: &mut Painter, slot: Slot, size: Size, name: &str, color: Color, z: f32) {
    let parts = [Part::Back, Part::Text];
    for part in parts {
        let color = match part {
            Part::Back => color,
            _ => over(color, Color::WHITE, 0.8),
        };
        draw_part(painter, slot, size, name, color, z, part);
    }
}

/// ファイルの名前を、プレビューの下に描く。大きさは、同じ一覧のフォルダの耳の名前と同じ。
pub fn draw_name(painter: &mut Painter, slot: Slot, size: Size, name: &str, color: Color, z: f32) {
    draw_part(painter, slot, size, name, color, z, Part::Below);
}

fn draw_part(
    painter: &mut Painter,
    slot: Slot,
    size: Size,
    name: &str,
    color: Color,
    z: f32,
    part: Part,
) {
    let em = em_width(name).max(0.1);
    let px = match size {
        Size::Live(sizes, _) => sizes.raster_px(),
        Size::Fixed(h) => (h / painter.scale * TEXT_RATIO).ceil().max(1.0),
    };
    let h = size.height(painter.scale);
    let (transform, shown) = part_transform(slot, em, z, part, px, h, painter.scale);
    let visibility = if shown {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    let common = (
        ChildOf(painter.parent),
        painter.layer.clone(),
        transform,
        visibility,
        BaseAlpha(color.alpha()),
    );
    let mut entity = match part {
        Part::Back => painter.commands.spawn((
            common,
            Sprite::from_color(color, Vec2::ONE),
            Anchor::BOTTOM_LEFT,
        )),
        Part::Text | Part::Below => painter.commands.spawn((
            common,
            Text2d::new(name),
            TextLayout::no_wrap(),
            TextFont::from_font_size(px).with_font(painter.font.clone()),
            TextColor(color),
            Anchor::CENTER_LEFT,
        )),
    };
    if painter.no_cull {
        entity.insert(bevy::camera::visibility::NoFrustumCulling);
    }
    if let Size::Live(_, of) = size {
        entity.insert(LiveEar {
            slot,
            of,
            em,
            z,
            part,
            px,
        });
    }
}

/// ズームに合わせて大きさを変える部品（フォルダの耳と、ファイルの名前）。
#[derive(Component)]
pub struct LiveEar {
    slot: Slot,
    of: EarOf,
    /// 名前の幅（文字の大きさに対する割合）。
    em: f32,
    z: f32,
    part: Part,
    /// 文字を作った大きさ（ピクセル）。
    px: f32,
}

#[derive(Clone, Copy)]
enum Part {
    /// 耳の四角。
    Back,
    /// 耳に書く名前。
    Text,
    /// ファイルのプレビューの下に書く名前。
    Below,
}

/// 耳の高さが `h` のときの、部品の位置と大きさ、見せるかどうか。
fn part_transform(
    slot: Slot,
    em: f32,
    z: f32,
    part: Part,
    px: f32,
    h: f32,
    scale: f32,
) -> (Transform, bool) {
    let (_, shown) = slot.yielded(h);
    let text = |at: Vec2, size: f32| {
        (
            Transform::from_translation(at.extend(z + 0.05)).with_scale(Vec3::splat(size / px)),
            size / scale >= MIN_TEXT_PX,
        )
    };
    match part {
        Part::Back | Part::Text => {
            // 耳は見えている本体の左上に乗り、横幅は名前に合わせる（本体の幅の 1/3 から本体の幅まで）。
            let w = (em * h * TEXT_RATIO + h * 0.9)
                .max(shown.width() * MIN_WIDTH)
                .min(shown.width());
            let top_left = Vec2::new(shown.min.x, shown.max.y);
            match part {
                Part::Back => (
                    Transform::from_translation(top_left.extend(z))
                        .with_scale(Vec3::new(w, h, 1.0)),
                    w / scale >= 1.0 && h / scale >= 1.0,
                ),
                _ => {
                    let size = (h * TEXT_RATIO).min((w - h * 0.6) * 0.95 / em);
                    text(top_left + Vec2::new(h * 0.45, h / 2.0), size)
                }
            }
        }
        Part::Below => {
            let size = (h * TEXT_RATIO).min(shown.width() * 0.95 / em);
            text(Vec2::new(shown.min.x, shown.min.y - h / 2.0), size)
        }
    }
}

/// カメラの倍率に合わせて、耳の大きさと、それに譲る本体の大きさを直す（描くフレームごと）。
pub fn update_ears(
    camera: Single<&Projection, With<MainCamera>>,
    sizes: Res<EarSizes>,
    mut ears: Query<(&LiveEar, &mut Transform, &mut Visibility), Without<YieldBody>>,
    mut bodies: Query<(&YieldBody, &mut Transform), Without<LiveEar>>,
) {
    let Projection::Orthographic(ortho) = *camera else {
        return;
    };
    for (ear, mut transform, mut visibility) in &mut ears {
        let h = ear.of.height(&sizes, ortho.scale);
        let (next, shown) =
            part_transform(ear.slot, ear.em, ear.z, ear.part, ear.px, h, ortho.scale);
        *transform = next;
        let next_visibility = if shown {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        visibility.set_if_neq(next_visibility);
    }
    for (body, mut transform) in &mut bodies {
        let h = body.of.height(&sizes, ortho.scale);
        *transform = body.slot.body_transform(h);
    }
}
