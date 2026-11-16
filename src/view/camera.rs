//! カメラ: 枠に合わせる位置と倍率、ズームのアニメーション（まっすぐ動き、速さだけ緩急）、
//! 動いている間だけ毎フレーム描画を頼むこと（止まれば1秒に1回に戻る）。

use super::*;

/// ズームにかける時間（秒）。
pub(crate) const ZOOM_SECONDS: f32 = 0.4;

/// ズームやフェードの最中なら true（毎フレーム描画している）。
#[derive(Resource, Default)]
pub struct Busy(pub bool);

/// 画面を映すカメラ（画像を焼くカメラと区別する）。
#[derive(Component)]
pub(crate) struct MainCamera;

/// カメラのズーム中だけ存在する。これがあるあいだはディスプレイの FPS で描画する。
#[derive(Resource)]
pub(crate) struct Animating {
    pub(crate) from_pos: Vec2,
    pub(crate) from_scale: f32,
    pub(crate) to_pos: Vec2,
    pub(crate) to_scale: f32,
    /// 待機中は Bevy の Time の進み方が不規則なので、実時間で測る。
    pub(crate) started: Instant,
    /// ズームが終わったら、今の位置（Nav の最後）の本物の場面に切り替える。
    pub(crate) then_show: bool,
    /// かける時間（秒）と、速さの緩急。
    pub(crate) seconds: f32,
    pub(crate) ease: Ease,
}

/// 右に開いている一覧の幅（論理ピクセル、`f32` のビット列）。その分だけ、木を映す領域が狭くなる。
static SIDE_PX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 右に開いている一覧の幅（論理ピクセル）。
pub(crate) fn side_px() -> f32 {
    f32::from_bits(SIDE_PX.load(std::sync::atomic::Ordering::Relaxed))
}

/// 右に開いている一覧の幅を変える（`ui/dups.rs` が開け閉めの動きに合わせて呼ぶ）。
pub(crate) fn set_side_px(px: f32) {
    SIDE_PX.store(px.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// 木を映す領域の幅（ウィンドウから右の一覧を除いたもの）。
pub(crate) fn view_width(window: &Window) -> f32 {
    (window.width() - side_px()).max(1.0)
}

/// `rect` がメニューバーの下の領域いっぱいに収まるカメラの位置と倍率。
pub(crate) fn fit(rect: Rect, window: &Window) -> (Vec2, f32) {
    let view_height = window.height() - BAR_PX;
    // 周りに少し余白を残す（外側のフォルダの色が見え、耳も端に貼りつかない）。
    let scale = (rect.width() / view_width(window)).max(rect.height() / view_height)
        * crate::layout::FIT_MARGIN;
    // 画面の中心より、バーの高さの半分だけ下に枠の中心が来るようにする。
    (rect.center() + Vec2::new(0.0, BAR_PX / 2.0 * scale), scale)
}

/// ウィンドウの大きさが変わったら、フィーチャー中の枠に合わせ直す。
pub(crate) fn refit_on_resize(
    mut resized: MessageReader<WindowResized>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    layouts: Res<Layouts>,
    nav: Res<Nav>,
    animating: Option<Res<Animating>>,
) {
    if resized.read().last().is_none() || animating.is_some() {
        return;
    }
    let (mut transform, mut projection) = camera.into_inner();
    let (pos, scale) = fit(nav.top().frame(&layouts), &window);
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }
}

pub(crate) fn animate_camera(
    animating: Option<Res<Animating>>,
    camera: Single<(&mut Transform, &mut Projection), With<MainCamera>>,
    mut current: ResMut<CurrentScene>,
    mut maker: SceneMaker,
    nav: Res<Nav>,
) {
    let Some(anim) = animating else {
        return;
    };
    let t = (anim.started.elapsed().as_secs_f32() / anim.seconds).min(1.0);
    let e = anim.ease.apply(t);

    // 倍率は対数で補間すると、ズームの速さが一定に見える。
    let scale = (anim.from_scale.ln() + (anim.to_scale.ln() - anim.from_scale.ln()) * e).exp();
    let pos = zoom_position(&anim, scale, e);

    let (mut transform, mut projection) = camera.into_inner();
    transform.translation = pos.extend(0.0);
    if let Projection::Orthographic(ortho) = projection.as_mut() {
        ortho.scale = scale;
    }

    if t < 1.0 {
        return;
    }
    maker.commands.remove_resource::<Animating>();
    if anim.then_show {
        // 画像の上に本物の場面を重ねてフェードインし、終わったら古い場面を消す。
        let old = current.0;
        current.0 = maker.spawn(&nav.stack, anim.to_scale);
        maker.commands.entity(current.0).insert((
            Fade {
                started: Instant::now(),
                fade_in: true,
                replaces: Some(old),
            },
            Transform::from_xyz(0.0, 0.0, 0.5),
        ));
    }
}

/// ズーム中のカメラの位置。
///
/// 始めと終わりの見え方は、ある1点（ズームの中心）を中心に拡大縮小した関係になっている。
/// その点を中心に倍率だけを変えれば、画面上のどの点も中心から（へ）まっすぐ動く。
/// 速さの緩急は倍率の補間（イージング）でつける。
fn zoom_position(anim: &Animating, scale: f32, e: f32) -> Vec2 {
    let ratio = anim.to_scale / anim.from_scale;
    // 倍率がほとんど変わらない（ただの移動）ときは、まっすぐ動かすだけ。
    if (ratio - 1.0).abs() < 1e-3 {
        return anim.from_pos.lerp(anim.to_pos, e);
    }
    let center = (anim.to_pos - anim.from_pos * ratio) / (1.0 - ratio);
    center + (anim.from_pos - center) * (scale / anim.from_scale)
}

/// 速さの緩急。
#[derive(Clone, Copy)]
pub(crate) enum Ease {
    /// ゆっくり始まり、ゆっくり止まる（ズーム）。
    InOut,
}

impl Ease {
    fn apply(self, t: f32) -> f32 {
        match self {
            Ease::InOut => ease_in_out(t),
        }
    }
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// ズームやフェードの最中は毎フレーム次の描画を頼む。垂直同期でディスプレイの FPS になる。
pub(crate) fn keep_redrawing(
    animating: Option<Res<Animating>>,
    fades: Query<(), With<Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut busy: ResMut<Busy>,
) {
    // ジェスチャーの最中も、指を離したのを（入力が止まったことで）見分けるために回し続ける。
    busy.0 = animating.is_some() || !fades.is_empty() || gesture.active();
    if busy.0 {
        redraw.write(RequestRedraw);
    }
}
