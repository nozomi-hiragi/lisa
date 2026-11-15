//! トラックパッドの2本指スクロールの状態（指が乗っている・離した・慣性）。
//!
//! macOS は指を離したあとも、慣性のスクロールを「始まり・動き・終わり」の順でもう一度送ってくる。
//! それを指の動きと取り違えないよう、ここだけで状態を持って見分ける。
//!
//! 状態の流れ:
//! - `Idle` --始まり--> `Touching`（指が乗っている）
//! - `Touching` --終わり--> `AfterLift`（離した直後。慣性が始まるか待つ）
//! - `AfterLift` --150ms 以内の入力--> `Momentum`（慣性）、150ms 過ぎたら `Idle`
//! - `Momentum` --終わり、または 100ms 入力なし--> `Idle`
//!
//! 慣性は、それを生んだジェスチャーのもの。使わないと決めたら（`drop_momentum`）、その慣性が
//! 終わるまで、来た入力はすべて捨てる。新しい指の始まりとみなすのは、慣性が終わったあとだけ。
//!
//! 時刻は秒（f64）で受け取るので、窓が無くてもテストできる。

use bevy::input::touch::TouchPhase;
use bevy::math::Vec2;

/// 指を離してからこれ以内に来た入力は慣性とみなす（秒）。
pub const MOMENTUM_GAP: f64 = 0.15;
/// 慣性の入力がこれだけ途切れたら、慣性は終わったとみなす（秒）。
pub const MOMENTUM_END_GAP: f64 = 0.10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Idle,
    /// 指が乗っている。
    Touching,
    /// 指を離した直後。`allowed` はこのあと来る慣性を使うか。
    AfterLift {
        lifted_at: f64,
        allowed: bool,
    },
    /// 慣性が続いている。
    Momentum {
        lifted_at: f64,
        last_event: f64,
        allowed: bool,
    },
}

/// 入力を受けた結果、呼び出し側がすること。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// 指で動かした（指の動きそのまま）。
    Touch(Vec2),
    /// 指を離した。
    Lift,
    /// 使ってよい慣性。`since_lift` は指を離してからの秒数。
    Momentum { delta: Vec2, since_lift: f32 },
    /// 何もしない（捨てた入力など）。
    None,
}

#[derive(Clone, Copy, Debug)]
pub struct ScrollMachine {
    pub state: State,
}

impl Default for ScrollMachine {
    fn default() -> Self {
        Self { state: State::Idle }
    }
}

impl ScrollMachine {
    /// 時間が過ぎたことによる移り変わり（慣性の終わりなど）。入力の前に毎回呼ぶ。
    pub fn tick(&mut self, now: f64) {
        self.state = match self.state {
            State::AfterLift { lifted_at, .. } if now - lifted_at > MOMENTUM_GAP => State::Idle,
            State::Momentum { last_event, .. } if now - last_event > MOMENTUM_END_GAP => {
                State::Idle
            }
            state => state,
        };
    }

    /// 2本指スクロールの入力を1つ受ける。
    pub fn event(&mut self, now: f64, phase: TouchPhase, delta: Vec2) -> Action {
        self.tick(now);
        let moved = delta != Vec2::ZERO;
        match (self.state, phase) {
            // 指が乗る（何もないときの始まり。始まりが抜けた動きも指とみなす）。
            (State::Idle, TouchPhase::Started | TouchPhase::Moved) => {
                self.state = State::Touching;
                touch(delta)
            }
            (State::Idle, _) => Action::None,

            (State::Touching, TouchPhase::Started | TouchPhase::Moved) => touch(delta),
            (State::Touching, TouchPhase::Ended | TouchPhase::Canceled) => {
                self.state = State::AfterLift {
                    lifted_at: now,
                    allowed: true,
                };
                Action::Lift
            }

            // 離した直後の入力は慣性の始まり。
            (State::AfterLift { lifted_at, allowed }, TouchPhase::Started | TouchPhase::Moved) => {
                self.state = State::Momentum {
                    lifted_at,
                    last_event: now,
                    allowed,
                };
                momentum(allowed && moved, delta, now - lifted_at)
            }
            (State::AfterLift { .. }, _) => Action::None,

            (
                State::Momentum {
                    lifted_at, allowed, ..
                },
                TouchPhase::Started | TouchPhase::Moved,
            ) => {
                self.state = State::Momentum {
                    lifted_at,
                    last_event: now,
                    allowed,
                };
                momentum(allowed && moved, delta, now - lifted_at)
            }
            (State::Momentum { .. }, TouchPhase::Ended | TouchPhase::Canceled) => {
                self.state = State::Idle;
                Action::None
            }
        }
    }

    /// 今の（またはこれから来る）慣性は使わない。その慣性が終わるまで入力を捨てる。
    pub fn drop_momentum(&mut self) {
        match &mut self.state {
            State::AfterLift { allowed, .. } | State::Momentum { allowed, .. } => *allowed = false,
            _ => {}
        }
    }

    /// 指が乗っているか。
    pub fn touching(&self) -> bool {
        self.state == State::Touching
    }
}

fn touch(delta: Vec2) -> Action {
    if delta == Vec2::ZERO {
        Action::None
    } else {
        Action::Touch(delta)
    }
}

fn momentum(use_it: bool, delta: Vec2, since_lift: f64) -> Action {
    if use_it {
        Action::Momentum {
            delta,
            since_lift: since_lift as f32,
        }
    } else {
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 60.0;

    fn moved(y: f32) -> (TouchPhase, Vec2) {
        (TouchPhase::Moved, Vec2::new(0.0, y))
    }

    /// 指で動かして離し、そのあと慣性が `momentum` 回来る並び（時刻つき）。
    fn swipe(start: f64, finger: usize, momentum: usize) -> Vec<(f64, TouchPhase, Vec2)> {
        let mut t = start;
        let mut events = vec![(t, TouchPhase::Started, Vec2::ZERO)];
        for _ in 0..finger {
            t += DT;
            let (p, d) = moved(-20.0);
            events.push((t, p, d));
        }
        t += DT;
        events.push((t, TouchPhase::Ended, Vec2::ZERO));
        t += DT;
        events.push((t, TouchPhase::Started, Vec2::ZERO));
        for i in 0..momentum {
            t += DT;
            let (p, d) = moved(-30.0 * 0.95f32.powi(i as i32));
            events.push((t, p, d));
        }
        t += DT;
        events.push((t, TouchPhase::Ended, Vec2::ZERO));
        events
    }

    #[test]
    fn finger_then_momentum_is_classified() {
        let mut m = ScrollMachine::default();
        let actions: Vec<Action> = swipe(0.0, 5, 5)
            .into_iter()
            .map(|(t, p, d)| m.event(t, p, d))
            .collect();
        let touches = actions
            .iter()
            .filter(|a| matches!(a, Action::Touch(_)))
            .count();
        let lifts = actions.iter().filter(|a| matches!(a, Action::Lift)).count();
        let momentum = actions
            .iter()
            .filter(|a| matches!(a, Action::Momentum { .. }))
            .count();
        assert_eq!((touches, lifts, momentum), (5, 1, 5));
        assert_eq!(m.state, State::Idle);
    }

    #[test]
    fn dropped_momentum_stays_dropped_until_it_ends() {
        // 端で重さが勝って戻したあと（drop）、残りの慣性が 30 回来ても何もしない。
        let mut m = ScrollMachine::default();
        let events = swipe(0.0, 5, 40);
        let mut moved_after_drop = 0;
        for (i, (t, p, d)) in events.into_iter().enumerate() {
            if i == 15 {
                m.drop_momentum();
            }
            let action = m.event(t, p, d);
            if i > 15 && action != Action::None {
                moved_after_drop += 1;
            }
        }
        assert_eq!(moved_after_drop, 0);
        assert_eq!(m.state, State::Idle);
    }

    #[test]
    fn dropped_at_lift_ignores_whole_momentum_stream() {
        // カメラを動かしていて、離した瞬間に戻すと決めたら、その慣性はまるごと捨てる。
        let mut m = ScrollMachine::default();
        let mut any_momentum = false;
        for (t, p, d) in swipe(0.0, 5, 30) {
            let action = m.event(t, p, d);
            if action == Action::Lift {
                m.drop_momentum();
            }
            any_momentum |= matches!(action, Action::Momentum { .. });
        }
        assert!(!any_momentum);
    }

    #[test]
    fn started_during_momentum_is_not_a_new_touch() {
        // 慣性の途中に「始まり」が来ても（macOS が送ることがある）、新しい指とはみなさない。
        let mut m = ScrollMachine::default();
        let mut t = 0.0;
        m.event(t, TouchPhase::Started, Vec2::ZERO);
        t += DT;
        m.event(t, TouchPhase::Ended, Vec2::ZERO);
        t += DT;
        m.event(t, TouchPhase::Moved, Vec2::new(0.0, -10.0));
        m.drop_momentum();
        t += DT;
        assert_eq!(
            m.event(t, TouchPhase::Started, Vec2::new(0.0, -5.0)),
            Action::None
        );
        assert!(!m.touching());
    }

    #[test]
    fn real_new_touch_after_momentum_ends() {
        // 慣性が途切れて（終わりが来なくても）100ms 過ぎたあとの始まりは、新しい指。
        let mut m = ScrollMachine::default();
        let mut t = 0.0;
        m.event(t, TouchPhase::Started, Vec2::ZERO);
        t += DT;
        m.event(t, TouchPhase::Ended, Vec2::ZERO);
        t += DT;
        m.event(t, TouchPhase::Moved, Vec2::new(0.0, -10.0));
        m.drop_momentum();
        t += 0.2;
        let action = m.event(t, TouchPhase::Started, Vec2::new(0.0, 4.0));
        assert_eq!(action, Action::Touch(Vec2::new(0.0, 4.0)));
        assert!(m.touching());
    }

    #[test]
    fn holding_still_keeps_touching() {
        // 指を置いたまま止めても、離したことにはならない。
        let mut m = ScrollMachine::default();
        m.event(0.0, TouchPhase::Started, Vec2::ZERO);
        m.event(DT, TouchPhase::Moved, Vec2::new(0.0, 30.0));
        m.tick(5.0);
        assert!(m.touching());
    }

    #[test]
    fn lift_without_momentum_goes_idle() {
        let mut m = ScrollMachine::default();
        m.event(0.0, TouchPhase::Started, Vec2::ZERO);
        m.event(DT, TouchPhase::Moved, Vec2::new(0.0, 30.0));
        assert_eq!(
            m.event(2.0 * DT, TouchPhase::Ended, Vec2::ZERO),
            Action::Lift
        );
        m.tick(1.0);
        assert_eq!(m.state, State::Idle);
    }
}
