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

use bevy::ecs::{lifecycle::HookContext, world::DeferredWorld};
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::sprite::Anchor;

use crate::render::shapes::{
    BaseAlpha, Painter, SlicePart, SlicePlacement, em_width, hud_plate_color, over,
};
use crate::view::MainCamera;

/// 耳の横幅の最小（本体の幅に対する割合）。
pub const MIN_WIDTH: f32 = 1.0 / 3.0;
/// 耳の高さに対する、名前の文字の大きさ。
const TEXT_RATIO: f32 = 0.55;
/// 画面上でこれより小さい名前は描かない（論理ピクセル）。
const MIN_TEXT_PX: f32 = 4.0;

/// 耳の大きさを決める値。
#[derive(Resource, Clone, Copy, PartialEq)]
pub struct EarSizes {
    pub viewport: Vec2,
    /// 開いたフォルダの本体を画面に収めたときの幅（論理ピクセル）。
    pub fitted_width: f32,
    /// 開いたときの耳の高さ（論理ピクセル）。
    pub selected_px: f32,
    /// 一覧の中の耳の高さ（論理ピクセル）。
    pub preview_px: f32,
}

impl EarSizes {
    fn height(&self, of: EarOf, scale: f32) -> f32 {
        let (selected, selected_height) = of.selected_fit.map_or_else(
            || {
                let selected = of.size_width / self.fitted_width;
                (selected, self.selected_px * selected)
            },
            |fit| {
                (
                    crate::layout::fit_scale(fit, self.viewport),
                    of.selected_height,
                )
            },
        );
        let preview = of.parent_width / self.fitted_width;
        let in_list = of.in_list.min(self.preview_px * preview);
        if scale <= selected {
            return selected_height;
        }
        if scale >= preview || preview <= selected {
            return in_list;
        }
        // 画面上の高さを、倍率の対数に沿って変える。
        let selected_px = selected_height / selected;
        let preview_px = in_list / preview;
        let t = (scale / selected).ln() / (preview / selected).ln();
        (selected_px + (preview_px - selected_px) * t) * scale
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
        let reserved = if self.name_below {
            self.body.min.y - self.cell.min.y
        } else {
            self.cell.max.y - self.body.max.y
        };
        if h <= reserved {
            // 中心と大きさから組み立て直すと、本体が丸めでわずかにずれる。
            return (1.0, self.body);
        }
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
    pub selected_fit: Option<Vec2>,
    pub selected_height: f32,
    /// 親の本体の幅。
    pub parent_width: f32,
    /// 親を開いたときの耳の高さの上限（ワールド座標）。
    pub in_list: f32,
}

impl EarOf {
    fn height(&self, sizes: &EarSizes, scale: f32) -> f32 {
        sizes.height(*self, scale)
    }
}

/// 耳の大きさの決め方。ズームに合わせて変えるか（本物の場面）、決まった大きさか（画像に焼くとき）。
#[derive(Clone, Copy)]
pub enum Size<'a> {
    Live(&'a EarSizes, EarOf),
    Frozen(&'a EarSizes, EarOf),
    Fixed(f32),
}

impl Size<'_> {
    pub(super) fn height(&self, scale: f32) -> f32 {
        match self {
            Size::Live(sizes, of) | Size::Frozen(sizes, of) => of.height(sizes, scale),
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
#[component(on_add = queue_ear_part)]
pub struct YieldBody {
    slot: Slot,
    of: EarOf,
}

/// フォルダ本体の枠と、名前を載せる耳を同じ境界で描く。
pub fn draw_folder(
    painter: &mut Painter,
    slot: Slot,
    size: Size,
    name: Option<&str>,
    color: Color,
    image: Option<Handle<Image>>,
    z: f32,
) {
    let em = name.map(|name| em_width(name).max(0.1));
    let geometry = HudGeometry::new(slot, em, size.height(painter.scale), painter.scale);
    let plate = Sprite::from_color(hud_plate_color(color), Vec2::ONE);
    let body = painter.slice_sprite(plate.clone(), geometry.frame(SlicePart::Plate, z));
    let mut parts = painter.slice_9(color, |part| geometry.frame(part, z));
    parts.push((body, SlicePart::Plate));
    if let Some(image) = image {
        let part = SlicePart::Thumbnail;
        let entity = painter.slice_sprite(Sprite::from_image(image), geometry.frame(part, z));
        painter
            .commands
            .entity(entity)
            .insert(super::scene::FolderPreview);
        parts.push((entity, part));
    }
    if let Some(name) = name {
        let part = SlicePart::EarLiftPlate;
        let entity = painter.slice_sprite(plate, geometry.frame(part, z));
        parts.push((entity, part));
        parts.extend(painter.slice_3_horizontal(color, |part| geometry.ear(part, z)));
        draw_part(
            painter,
            slot,
            size,
            name,
            crate::render::palette::BASE_CONTENT,
            z,
            Part::Text,
        );
    }
    if let Size::Live(_, of) = size {
        for (entity, part) in parts {
            painter.commands.entity(entity).insert(LiveSlice {
                slot,
                of,
                em,
                z,
                part,
            });
        }
    }
}

/// 状態を示す膜も、外枠と同じ位置で更新する。
pub fn draw_folder_overlay(
    painter: &mut Painter,
    slot: Slot,
    size: Size,
    name: &str,
    color: Color,
    z: f32,
) -> Entity {
    let em = Some(em_width(name).max(0.1));
    let geometry = HudGeometry::new(slot, em, size.height(painter.scale), painter.scale);
    let part = SlicePart::Interior;
    let entity = painter.slice_sprite(
        Sprite::from_color(color, Vec2::ONE),
        geometry.frame(part, z),
    );
    if let Size::Live(_, of) = size {
        painter.commands.entity(entity).insert(LiveSlice {
            slot,
            of,
            em,
            z,
            part,
        });
    }
    entity
}

#[derive(Component)]
#[component(on_add = queue_ear_part)]
pub struct LiveSlice {
    slot: Slot,
    of: EarOf,
    em: Option<f32>,
    z: f32,
    part: SlicePart,
}

struct HudGeometry {
    body: Rect,
    scale: f32,
    h: f32,
    corner: f32,
    rail: f32,
    flat: f32,
    left_cap: f32,
    slope: f32,
    lift: f32,
}

impl HudGeometry {
    fn new(slot: Slot, em: Option<f32>, h: f32, scale: f32) -> Self {
        let (_, body) = slot.yielded(h);
        let width = body.width();
        let corner = (h * 0.5)
            .max(4.0 * scale)
            .min(width * 0.45)
            .min(body.height() * 0.5);
        let available = (width - corner).max(0.0);
        let left_cap = (0.5 * h).min(available);
        let (flat, slope, lift) = em.map_or((0.0, 0.0, 0.0), |em| {
            let wanted = (em * h * TEXT_RATIO + h * 0.55).max(width * MIN_WIDTH);
            let flat = wanted.clamp(left_cap, available);
            let remaining = (available - flat).max(0.0);
            if remaining >= h {
                return (flat, h, 0.0);
            }
            let threshold = (h * 0.12).max(1.5 * scale).min(h * 0.5);
            // 閾値の手前で斜辺を連続的に短くし、角の位置が飛ばないようにする。
            let k = ((remaining - threshold) / threshold).clamp(0.0, 1.0);
            let slope = remaining * k * k * (3.0 - 2.0 * k);
            (available - slope, slope, h - slope)
        });
        Self {
            body,
            scale,
            h,
            corner,
            rail: (corner * 0.25).max(1.5 * scale).min(corner),
            flat,
            left_cap,
            slope,
            lift,
        }
    }

    fn frame(&self, part: SlicePart, z: f32) -> SlicePlacement {
        let Rect { min, max } = self.body;
        let (l, b, r, t) = (min.x, min.y, max.x, max.y);
        let c = self.corner;
        let join = l + self.flat + self.slope;
        let rail_start = if self.flat > 0.0 { join } else { l + c };
        let rect = match part {
            SlicePart::Frame(0) if self.flat == 0.0 => Rect::new(l, t - c, l + c, t),
            SlicePart::Frame(1) => Self::span(rail_start, t - self.rail, r - c, t),
            SlicePart::Frame(2) => Rect::new(r - c, t + self.lift - c, r, t + self.lift),
            SlicePart::Frame(3) => Rect::new(l, b + c, l + c, t - c),
            SlicePart::Frame(5) => Rect::new(r - c, b + c, r, t + self.lift - c),
            SlicePart::Frame(6) => Rect::new(l, b, l + c, b + c),
            SlicePart::Frame(7) => Rect::new(l + c, b, r - c, b + c),
            SlicePart::Frame(8) => Rect::new(r - c, b, r, b + c),
            SlicePart::Frame(9) if self.flat > 0.0 => Self::span(l, t - c, join.min(l + c), t),
            SlicePart::Plate | SlicePart::Thumbnail => self.body,
            SlicePart::EarLiftPlate => Rect::new(l + self.flat, t, r, t + self.lift),
            SlicePart::Interior => {
                Rect::new(l + self.rail, b + self.rail, r - self.rail, t - self.rail)
            }
            _ => Rect::new(l, t, l, t),
        };
        let z = z + match part {
            SlicePart::Thumbnail => 0.01,
            SlicePart::Frame(_) => 0.02,
            _ => 0.0,
        };
        SlicePlacement {
            rect,
            z,
            scale: self.scale,
            uv: None,
        }
    }

    fn ear(&self, part: SlicePart, z: f32) -> SlicePlacement {
        let left = self.body.min.x;
        let flat_end = left + self.flat;
        let base = self.body.max.y;
        let cap = left + self.left_cap;
        let top = base + self.h;
        let (rect, uv) = match part {
            SlicePart::Ear(0) => (Self::span(left, base, cap, top), None),
            SlicePart::Ear(1) => (Self::span(cap, base, flat_end, top), None),
            SlicePart::Ear(2) => {
                let crop = if self.h > 0.0 {
                    64.0 * self.slope / self.h
                } else {
                    0.0
                };
                (
                    Self::span(flat_end, top - self.slope, flat_end + self.slope, top),
                    Some(Rect::new(64.0, 0.0, 64.0 + crop, crop)),
                )
            }
            _ => (Self::span(left, base, flat_end, top), None),
        };
        SlicePlacement {
            rect,
            z: z + 0.02,
            scale: self.scale,
            uv,
        }
    }

    fn span(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect {
            min: Vec2::new(x0, y0),
            max: Vec2::new(x1.max(x0), y1),
        }
    }
}

/// カタログの札（アプリの色と `label`）を、フォルダ `name` の耳のすぐ右に描く。
pub fn draw_tag(
    painter: &mut Painter,
    slot: Slot,
    size: Size,
    name: &str,
    label: &str,
    color: Color,
    z: f32,
) {
    let name_em = em_width(name).max(0.1);
    draw_part(
        painter,
        slot,
        size,
        label,
        color,
        z,
        Part::TagBack { name_em },
    );
    let text = over(color, crate::render::palette::BASE_300, 0.92);
    draw_part(
        painter,
        slot,
        size,
        label,
        text,
        z,
        Part::TagText { name_em },
    );
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
        Size::Live(sizes, _) | Size::Frozen(sizes, _) => sizes.raster_px(),
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
        Part::TagBack { .. } => painter.commands.spawn((
            common,
            Sprite::from_color(color, Vec2::ONE),
            Anchor::BOTTOM_LEFT,
        )),
        Part::Text | Part::Below | Part::TagText { .. } => painter.commands.spawn((
            common,
            Text2d::new(name),
            TextLayout::no_wrap(),
            TextFont::from_font_size(px).with_font(painter.font.clone()),
            TextColor(color),
            if matches!(part, Part::Below) {
                Anchor::CENTER
            } else {
                Anchor::CENTER_LEFT
            },
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
#[component(on_add = queue_ear_part)]
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
    /// 耳に書く名前。
    Text,
    /// ファイルのプレビューの下に書く名前。
    Below,
    /// カタログの札の四角（耳のすぐ右）。`name_em` は耳の名前の幅（耳の横幅を決めるため）。
    TagBack { name_em: f32 },
    /// 札に書く文字。
    TagText { name_em: f32 },
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
        Part::TagBack { name_em } | Part::TagText { name_em } => {
            // 札は耳のすぐ右に置く。入らなければ、本体の左上（耳のすぐ下）に掛ける。
            let geometry = HudGeometry::new(slot, Some(name_em), h, scale);
            let h = geometry.h;
            let th = h * 0.7;
            let want = em * th * TEXT_RATIO + th * 0.8;
            let x_right = shown.min.x + geometry.flat + geometry.slope + h * 0.2;
            let (bottom_left, w) = if shown.max.x - x_right >= want.min(th * 3.0) {
                (
                    Vec2::new(x_right, shown.max.y),
                    want.min(shown.max.x - x_right),
                )
            } else {
                (
                    Vec2::new(shown.min.x, shown.max.y - th),
                    want.min(shown.width()),
                )
            };
            let fits = w >= th * 1.5;
            match part {
                Part::TagBack { .. } => (
                    Transform::from_translation(bottom_left.extend(z)).with_scale(Vec3::new(
                        w.max(0.0),
                        th,
                        1.0,
                    )),
                    fits && w / scale >= 1.0 && th / scale >= 1.0,
                ),
                _ => {
                    let size = (th * TEXT_RATIO).min((w - th * 0.5) * 0.95 / em);
                    let (transform, shown_text) =
                        text(bottom_left + Vec2::new(th * 0.4, th / 2.0), size);
                    (transform, fits && shown_text)
                }
            }
        }
        Part::Text => {
            let geometry = HudGeometry::new(slot, Some(em), h, scale);
            let h = geometry.h;
            let size = (h * TEXT_RATIO).min((geometry.flat - h * 0.40).max(0.0) * 0.95 / em);
            text(
                Vec2::new(shown.min.x + h * 0.30, shown.max.y + h * 0.5),
                size,
            )
        }
        Part::Below => {
            let size = (h * TEXT_RATIO).min(slot.cell.width() * 0.95 / em);
            text(Vec2::new(slot.cell.center().x, shown.min.y - h / 2.0), size)
        }
    }
}

/// カメラの倍率に合わせて、耳の大きさと、それに譲る本体の大きさを直す（描くフレームごと）。
/// Added フィルタは既存の部品も走査するため、追加されたエンティティの ID を控える。
#[derive(Resource, Default)]
pub(crate) struct PendingEarParts(Vec<Entity>);

fn queue_ear_part(mut world: DeferredWorld, context: HookContext) {
    if let Some(mut pending) = world.get_resource_mut::<PendingEarParts>() {
        pending.0.push(context.entity);
    }
}

type EarParts<'a> = (&'a LiveEar, &'a mut Transform, &'a mut Visibility);
type BodyParts<'a> = (&'a YieldBody, &'a mut Transform);
type SliceParts<'a> = (
    &'a LiveSlice,
    &'a mut Sprite,
    &'a mut Transform,
    &'a mut Visibility,
);
type EarFilter = (Without<YieldBody>, Without<LiveSlice>);
type BodyFilter = (Without<LiveEar>, Without<LiveSlice>);

#[allow(clippy::type_complexity)]
pub(crate) fn update_ears(
    camera: Single<&Projection, With<MainCamera>>,
    sizes: Res<EarSizes>,
    mut last: Local<Option<(u32, EarSizes)>>,
    mut pending: ResMut<PendingEarParts>,
    mut ears: Query<EarParts, EarFilter>,
    mut bodies: Query<BodyParts, BodyFilter>,
    mut slices: Query<SliceParts>,
) {
    let Projection::Orthographic(ortho) = *camera else {
        *last = None;
        return;
    };
    let scale = ortho.scale;
    let state = (scale.to_bits(), *sizes);
    let unchanged = last.replace(state) == Some(state);

    let update_ear =
        |(ear, mut transform, mut visibility): (&LiveEar, Mut<Transform>, Mut<Visibility>)| {
            let h = ear.of.height(&sizes, scale);
            let (next, shown) = part_transform(ear.slot, ear.em, ear.z, ear.part, ear.px, h, scale);
            transform.set_if_neq(next);
            visibility.set_if_neq(if shown {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            });
        };
    let update_slice = |(slice, sprite, transform, visibility): (
        &LiveSlice,
        Mut<Sprite>,
        Mut<Transform>,
        Mut<Visibility>,
    )| {
        let h = slice.of.height(&sizes, scale);
        let geometry = HudGeometry::new(slice.slot, slice.em, h, scale);
        let place = match slice.part {
            SlicePart::Ear(_) | SlicePart::EarFallback => geometry.ear(slice.part, slice.z),
            _ => geometry.frame(slice.part, slice.z),
        };
        place.apply(sprite, transform, visibility);
    };
    let update_body = |(body, mut transform): (&YieldBody, Mut<Transform>)| {
        let h = body.of.height(&sizes, scale);
        transform.set_if_neq(body.slot.body_transform(h));
    };

    if unchanged {
        if !pending.0.is_empty() {
            for entity in pending.0.drain(..) {
                if let Ok(part) = ears.get_mut(entity) {
                    update_ear(part);
                } else if let Ok(part) = slices.get_mut(entity) {
                    update_slice(part);
                } else if let Ok(part) = bodies.get_mut(entity) {
                    update_body(part);
                }
            }
        }
    } else {
        ears.iter_mut().for_each(update_ear);
        slices.iter_mut().for_each(update_slice);
        bodies.iter_mut().for_each(update_body);
        if !pending.0.is_empty() {
            pending.0.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_scale_only_updates_new_ear_parts() {
        let mut app = App::new();
        let sizes = EarSizes {
            viewport: Vec2::new(800.0, 600.0),
            fitted_width: 800.0,
            selected_px: 28.0,
            preview_px: 16.0,
        };
        let mut projection = OrthographicProjection::default_2d();
        projection.scale = 1.0;
        let camera = app
            .world_mut()
            .spawn((MainCamera, Projection::Orthographic(projection)))
            .id();
        app.insert_resource(sizes)
            .init_resource::<PendingEarParts>()
            .add_systems(Update, update_ears);
        let slot = Slot {
            cell: Rect::new(0.0, 0.0, 800.0, 600.0),
            body: Rect::new(0.0, 0.0, 800.0, 600.0),
            name_below: false,
        };
        let of = EarOf {
            size_width: 800.0,
            selected_fit: None,
            selected_height: 600.0,
            parent_width: 1600.0,
            in_list: 12.0,
        };
        let uninitialized = Transform::from_xyz(50.0, 50.0, 10.0);
        let first = app
            .world_mut()
            .spawn((YieldBody { slot, of }, uninitialized))
            .id();
        app.update();
        let expected = slot.body_transform(of.height(&sizes, 1.0));
        assert_eq!(*app.world().get::<Transform>(first).unwrap(), expected);

        app.world_mut().entity_mut(first).insert(uninitialized);
        let added = app
            .world_mut()
            .spawn((YieldBody { slot, of }, uninitialized))
            .id();
        app.update();
        assert_eq!(*app.world().get::<Transform>(first).unwrap(), uninitialized);
        assert_eq!(*app.world().get::<Transform>(added).unwrap(), expected);

        let mut projection = app.world_mut().get_mut::<Projection>(camera).unwrap();
        let Projection::Orthographic(projection) = &mut *projection else {
            unreachable!()
        };
        projection.scale = 2.0;
        app.update();
        let expected = slot.body_transform(of.height(&sizes, 2.0));
        for entity in [first, added] {
            assert_eq!(*app.world().get::<Transform>(entity).unwrap(), expected);
        }
    }

    #[test]
    fn hud_slope_crops_and_the_right_corner_rises_without_a_jump() {
        let body = Rect::new(0.0, 0.0, 180.0, 120.0);
        let slot = Slot {
            cell: body,
            body,
            name_below: false,
        };
        let h = 24.0;
        let seed = HudGeometry::new(slot, None, h, 1.0);
        let available = seed.body.width() - seed.corner;
        let layout = |remaining: f32| {
            let em = (available - remaining - h * 0.55) / (h * TEXT_RATIO);
            HudGeometry::new(slot, Some(em), h, 1.0)
        };
        let full = layout(h + 4.0);
        assert_eq!(full.slope, h);
        assert_eq!(full.lift, 0.0);
        let cropped = layout(h / 2.0);
        let slope = cropped.ear(SlicePart::Ear(2), 0.0);
        let right = cropped.frame(SlicePart::Frame(2), 0.0).rect;
        assert!((slope.rect.width() - h / 2.0).abs() < 0.001);
        assert!((slope.rect.height() - slope.rect.width()).abs() < 0.001);
        let uv = slope.uv.unwrap();
        assert!((uv.width() - 32.0).abs() < 0.001 && (uv.height() - 32.0).abs() < 0.001);
        assert_eq!(uv.min, Vec2::new(64.0, 0.0));
        assert!((right.min.x - slope.rect.max.x).abs() < 0.001);
        assert_eq!(right.max.y, slope.rect.min.y);
        assert_eq!(
            cropped.frame(SlicePart::Frame(5), 0.0).rect.max.y,
            right.min.y
        );
        assert_eq!(
            cropped.frame(SlicePart::EarLiftPlate, 0.0).rect.max.y,
            right.max.y
        );
        assert_eq!(cropped.frame(SlicePart::Thumbnail, 0.0).rect, full.body);

        let threshold = h * 0.12;
        let near = layout(threshold + 0.01);
        let collapsed = layout(threshold - 0.01);
        let center = collapsed.ear(SlicePart::Ear(1), 0.0).rect;
        let corner = collapsed.frame(SlicePart::Frame(2), 0.0).rect;
        assert_eq!(collapsed.slope, 0.0);
        assert!((near.lift - collapsed.lift).abs() < 0.001);
        assert!((center.max.x - corner.min.x).abs() < 0.001);
        assert_eq!(center.max.y, corner.max.y);
    }

    #[test]
    fn thumbnail_covers_the_yielded_body_and_the_flat_ear_spans_a_third() {
        let body = Rect::new(0.0, 0.0, 300.0, 120.0);
        let slot = Slot {
            cell: body,
            body,
            name_below: false,
        };
        let hud = HudGeometry::new(slot, Some(1.0), 40.0, 1.0);
        let (_, shown) = slot.yielded(40.0);
        assert_ne!(shown, body);
        assert_eq!(hud.frame(SlicePart::Thumbnail, 2.0).rect, shown);
        assert_eq!(hud.frame(SlicePart::Plate, 2.0).rect, shown);
        assert_eq!(hud.frame(SlicePart::Thumbnail, 2.0).z, 2.01);
        assert!(hud.frame(SlicePart::Frame(8), 2.0).z > 2.0);
        assert!((hud.flat - shown.width() * MIN_WIDTH).abs() < 0.001);
        let corner = Vec2::splat(hud.corner);
        for index in [0, 2, 6, 8] {
            let hud = HudGeometry::new(slot, None, 40.0, 1.0);
            assert!(
                (hud.frame(SlicePart::Frame(index), 0.0).rect.size() - corner).length() < 0.001
            );
        }
    }

    #[test]
    fn hud_corners_and_ear_join_keep_their_angles_at_extreme_aspect_ratios() {
        for (width, height) in [(600.0, 120.0), (120.0, 600.0), (18.0, 60.0)] {
            let body = Rect::new(0.0, 0.0, width, height);
            let slot = Slot {
                cell: body,
                body,
                name_below: false,
            };
            for scale in [0.1, 1.0, 5.0] {
                let hud = HudGeometry::new(slot, Some(100.0), 18.0, scale);
                let slope = hud.ear(SlicePart::Ear(2), 0.0).rect;
                let top = hud.frame(SlicePart::Frame(1), 0.0).rect;
                let upper_right = hud.frame(SlicePart::Frame(2), 0.0).rect;
                let lower_right = hud.frame(SlicePart::Frame(8), 0.0).rect;
                assert!((slope.width() - slope.height()).abs() < 0.001);
                assert!((lower_right.width() - lower_right.height()).abs() < 0.001);
                assert!((slope.max.x - top.min.x).abs() < 0.001);
                assert!((top.max.x - upper_right.min.x).abs() < 0.001);
                assert!((slope.min.y - upper_right.max.y).abs() < 0.001);
                assert!((top.height() - hud.rail).abs() < 0.001);
                assert!(hud.body.contains(lower_right.min));
            }
        }
    }
}
