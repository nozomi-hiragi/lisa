//! カメラに遅れて追従する、4倍周期のHUDグリッド。

use super::*;

const LAYER: usize = 3;
const TILE: f64 = 256.0;
const BASE: f64 = 128.0;
const LN4: f64 = 1.3862943611198906;

#[derive(Component)]
pub(super) struct Grid(usize);

#[derive(Resource)]
pub(super) struct CameraReset;

pub(super) fn spawn(commands: &mut Commands, image: &Handle<Image>) {
    commands.spawn((
        Camera2d,
        Camera {
            order: -2,
            ..default()
        },
        RenderLayers::layer(LAYER),
    ));
    for index in 0..3 {
        commands.spawn((
            Grid(index),
            Sprite::from_image(image.clone()),
            Transform::from_xyz(0.0, 0.0, index as f32),
            RenderLayers::layer(LAYER),
        ));
    }
}

#[derive(Default)]
pub(super) struct Motion {
    sample: Option<(NodeId, Vec2, f32, Instant)>,
    zoom: f64,
    phase: bevy::math::DVec2,
    pending_zoom: f64,
    pending_pan: bevy::math::DVec2,
}

impl Motion {
    fn advance(&mut self, elapsed: f64) {
        let factor = 1.0 - (-elapsed / 0.11).exp();
        let dz = self.pending_zoom * factor;
        let pan = self.pending_pan * factor;
        self.pending_zoom -= dz;
        self.pending_pan -= pan;
        self.zoom += dz;
        while self.zoom >= 1.0 {
            self.zoom -= 1.0;
            self.phase = (self.phase * 4.0).rem_euclid(bevy::math::DVec2::ONE);
        }
        while self.zoom < 0.0 {
            self.zoom += 1.0;
            self.phase /= 4.0;
        }
        let coarse = BASE * 4.0_f64.powf(self.zoom + 1.0);
        self.phase = (self.phase + pan / coarse).rem_euclid(bevy::math::DVec2::ONE);
    }

    fn layers(&self) -> [(f64, bevy::math::DVec2, f32); 3] {
        let u = self.zoom;
        let ease = u * u * (3.0 - 2.0 * u);
        let mid = 0.78 * (1.0 - ease) + 0.42 * ease + 0.95 * u * (1.0 - u);
        let sizes = BASE * 4.0_f64.powf(u);
        let phase_at = |factor: f64| (self.phase * factor).rem_euclid(bevy::math::DVec2::ONE);
        [
            (sizes * 4.0, self.phase, (0.42 * (1.0 - ease)) as f32),
            (sizes, phase_at(4.0), mid as f32),
            (sizes / 4.0, phase_at(16.0), (0.78 * ease) as f32),
        ]
    }
}

pub(super) fn follow(
    mut commands: Commands,
    mut state: Local<Motion>,
    camera: Single<(&Transform, &Projection), With<MainCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    layouts: Res<Layouts>,
    reset: Option<Res<CameraReset>>,
    occluded: Res<WindowOccludedState>,
    mut grids: Query<(&Grid, &mut Sprite)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if reset.is_some() {
        commands.remove_resource::<CameraReset>();
    }
    let (transform, projection) = *camera;
    let Projection::Orthographic(projection) = projection else {
        return;
    };
    let position = transform.translation.truncate();
    let scale = projection.scale;
    if !position.is_finite() || !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let now = Instant::now();
    let anchor = layouts.anchor;
    if let Some((previous_anchor, previous_position, previous_scale, previous_time)) =
        state.sample.replace((anchor, position, scale, now))
    {
        let pan = (position - previous_position).as_dvec2() / f64::from(previous_scale);
        let zoom = (f64::from(scale) / f64::from(previous_scale)).ln();
        if reset.is_none()
            && anchor == previous_anchor
            && pan.length() <= 2.0 * f64::from(window.size().length())
            && zoom.abs() <= LN4
        {
            state.pending_pan += bevy::math::DVec2::new(pan.x, -pan.y) * 0.6;
            state.pending_zoom -= zoom / LN4 * 0.75;
        }
        if state.pending_pan.length() < 0.02 && state.pending_zoom.abs() < 0.00002 {
            state.pending_pan = bevy::math::DVec2::ZERO;
            state.pending_zoom = 0.0;
        } else {
            state.advance((now - previous_time).as_secs_f64().min(0.05));
            if !occluded.0 {
                redraw.write(RequestRedraw);
            }
        }
    }
    for (grid, mut sprite) in &mut grids {
        let (size, phase, alpha) = state.layers()[grid.0];
        let size_px = window.size() + Vec2::splat(2.0 * size as f32);
        let half = size_px.as_dvec2() * (TILE / size * 0.5);
        let center = phase * TILE;
        let rect = Rect::from_corners((center - half).as_vec2(), (center + half).as_vec2());
        if sprite.rect != Some(rect) || sprite.color.alpha() != alpha {
            sprite.rect = Some(rect);
            sprite.custom_size = Some(size_px);
            sprite.color.set_alpha(alpha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::DVec2;

    #[test]
    fn grid_wrap_preserves_the_two_visible_layers_in_both_directions() {
        let mut motion = Motion {
            zoom: 1.0,
            phase: DVec2::new(0.21, 0.59),
            ..default()
        };
        let old = motion.layers();
        motion.advance(0.0);
        let new = motion.layers();
        for (a, b) in [(old[1], new[0]), (old[2], new[1])] {
            assert_eq!(a, b);
        }
        assert_eq!(old[0].2, 0.0);
        assert_eq!(new[2].2, 0.0);
        motion.pending_zoom = -0.000001;
        motion.advance(10.0);
        let reverse = motion.layers();
        assert!((reverse[1].0 - old[1].0).abs() < 0.001);
        assert!((reverse[1].1 - old[1].1).length() < 0.001);
    }

    #[test]
    fn parallax_approaches_the_target_without_overshoot() {
        let mut motion = Motion {
            pending_pan: DVec2::new(60.0, 0.0),
            pending_zoom: 0.25,
            ..default()
        };
        motion.advance(0.11);
        assert!((motion.zoom - 0.25 * (1.0 - (-1.0_f64).exp())).abs() < 0.00001);
        assert!(motion.pending_pan.x > 0.0 && motion.pending_pan.x < 60.0);
        motion.advance(10.0);
        assert!((motion.zoom - 0.25).abs() < 0.00001);
        assert!(motion.pending_pan.length() < 0.00001);
    }
}
