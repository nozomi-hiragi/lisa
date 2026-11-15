//! トラックパッドの2本指スクロールの状態（指が乗っている・離した・慣性）。
//!
//! macOS は指を離したあとも、慣性のスクロールを「始まり・動き・終わり」の順でもう一度送ってくる。
//! それを指の動きと取り違えないよう、ここだけで状態を持って見分ける。
//!
//! 状態の流れ:
//! - `Idle` --始まり--> `Touching`（指が乗っている）
//! - `Touching` --終わり--> `AfterLift`（離した直後。慣性が始まるか待つ）
//! - `AfterLift` --80ms 以内の入力--> `Momentum`（慣性）、80ms 過ぎたら `Idle`
//! - `Momentum` --終わり、または 100ms 入力なし--> `Idle`
//! - `Momentum` --動きの無い始まり（指がパッドに触れた）--> `Touching`
//!
//! 慣性は、それを生んだジェスチャーのもの。使わないと決めたら（`drop_momentum`）、その慣性が
//! 終わるまで、来た入力はすべて捨てる。新しい指の始まりとみなすのは、慣性が終わったあとだけ。
//!
//! macOS では NSEvent のスクロールを直接見て（`mac_scroll`）、指の段階（phase）と慣性の段階
//! （momentumPhase）を別々に受け取れるので、`event_raw` で確実に見分ける。指がパッドに触れた
//! だけ（MayBegin）でもすぐ分かる。ほかの環境では、winit がまとめた段階から `event` で推し量る。
//!
//! 時刻は秒（f64）で受け取るので、窓が無くてもテストできる。

use bevy::input::touch::TouchPhase;
use bevy::math::Vec2;

/// 指を離してからこれ以内に来た入力は慣性とみなす（秒）。macOS の慣性は離した直後に始まる。
pub const MOMENTUM_GAP: f64 = 0.08;
/// 慣性の入力がこれだけ途切れたら、慣性は終わったとみなす（秒）。
pub const MOMENTUM_END_GAP: f64 = 0.10;
/// 慣性がまだこれより速く動いているのに終わったら（ピクセル）、指が触れて止めたとみなす。
pub const EARLY_END_PX: f32 = 3.0;
/// 指が触れたと推し量ったあと、これだけ何も来なければ、軽く触れて離した（タップ）とみなす（秒）。
pub const INFERRED_TOUCH_TIMEOUT: f64 = 0.2;

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

/// macOS の NSEvent の段階（NSEventPhase）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RawPhase {
    None,
    MayBegin,
    Began,
    Stationary,
    Changed,
    Ended,
    Cancelled,
}

/// 入力を受けた結果、呼び出し側がすること。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// 指がパッドに触れた（動いていなくても）。戻る途中のアニメーションなどはここで止める。
    TouchBegin(Vec2),
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
    /// 最後に来た慣性の動きの大きさ（早く終わったかを見るため）。
    last_momentum: f32,
    /// 指の合図は来ていないが、慣性が急に終わったので触れたと推し量った時刻。
    inferred_touch: Option<f64>,
}

impl Default for ScrollMachine {
    fn default() -> Self {
        Self {
            state: State::Idle,
            last_momentum: 0.0,
            inferred_touch: None,
        }
    }
}

impl ScrollMachine {
    /// 時間が過ぎたことによる移り変わり（慣性の終わりなど）。入力の前に毎回呼ぶ。
    /// 触れたと推し量ったまま何も来なければ、離した（`Lift`）を返す。
    pub fn tick(&mut self, now: f64) -> Action {
        if let Some(at) = self.inferred_touch
            && self.state == State::Touching
            && now - at > INFERRED_TOUCH_TIMEOUT
        {
            self.inferred_touch = None;
            self.state = State::AfterLift {
                lifted_at: now,
                allowed: false,
            };
            return Action::Lift;
        }
        self.state = match self.state {
            State::AfterLift { lifted_at, .. } if now - lifted_at > MOMENTUM_GAP => State::Idle,
            State::Momentum { last_event, .. } if now - last_event > MOMENTUM_END_GAP => {
                State::Idle
            }
            state => state,
        };
        Action::None
    }

    /// 2本指スクロールの入力を1つ受ける。
    pub fn event(&mut self, now: f64, phase: TouchPhase, delta: Vec2) -> Action {
        let _ = self.tick(now);
        let moved = delta != Vec2::ZERO;
        match (self.state, phase) {
            // 指が乗る（何もないときの始まり。始まりが抜けた動きも指とみなす）。
            (State::Idle, TouchPhase::Started) => {
                self.state = State::Touching;
                Action::TouchBegin(delta)
            }
            (State::Idle, TouchPhase::Moved) => {
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

            // 慣性の途中に、動きの無い始まりが来たら、指がパッドに触れた（新しい指）。
            // 慣性そのものの入力には、必ず動きがある。
            (State::Momentum { .. }, TouchPhase::Started) if !moved => {
                self.state = State::Touching;
                Action::TouchBegin(Vec2::ZERO)
            }
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

    /// macOS の NSEvent から、指の段階と慣性の段階を別々に受け取る（推し量らなくてよい）。
    pub fn event_raw(
        &mut self,
        now: f64,
        phase: RawPhase,
        momentum_phase: RawPhase,
        delta: Vec2,
    ) -> Action {
        let _ = self.tick(now);
        let moved = delta != Vec2::ZERO;
        if momentum_phase != RawPhase::None {
            // 慣性。新しい指には決してならない。
            return match momentum_phase {
                RawPhase::Ended | RawPhase::Cancelled => {
                    let fast = self.last_momentum > EARLY_END_PX;
                    self.last_momentum = 0.0;
                    if matches!(self.state, State::Momentum { .. }) && fast {
                        // まだ速く流れていたのに終わった。指が触れて止めた（つかんだ）とみなす。
                        // 指の合図が来なければ、少しして離したことにする（tick）。
                        self.state = State::Touching;
                        self.inferred_touch = Some(now);
                        return Action::TouchBegin(Vec2::ZERO);
                    }
                    if matches!(self.state, State::Momentum { .. } | State::AfterLift { .. }) {
                        self.state = State::Idle;
                    }
                    Action::None
                }
                _ => {
                    let (lifted_at, allowed) = match self.state {
                        State::AfterLift { lifted_at, allowed }
                        | State::Momentum {
                            lifted_at, allowed, ..
                        } => (lifted_at, allowed),
                        // 指が乗っている間や、何もないときに来た慣性は使わない。
                        State::Touching | State::Idle => (now, false),
                    };
                    if self.state == State::Touching {
                        return Action::None;
                    }
                    self.last_momentum = delta.length();
                    self.state = State::Momentum {
                        lifted_at,
                        last_event: now,
                        allowed,
                    };
                    momentum(allowed && moved, delta, now - lifted_at)
                }
            };
        }
        if phase != RawPhase::None {
            // 本物の指の合図が来たので、推し量りはやめる。
            self.inferred_touch = None;
        }
        match phase {
            // 指がパッドに触れた。慣性やアニメーションの途中でも、すぐ指の状態にする。
            RawPhase::MayBegin | RawPhase::Began => {
                self.state = State::Touching;
                Action::TouchBegin(delta)
            }
            RawPhase::Changed | RawPhase::Stationary => {
                if self.state == State::Touching {
                    touch(delta)
                } else {
                    // 始まりを取りこぼしていても、指が動いているなら指。
                    self.state = State::Touching;
                    Action::TouchBegin(delta)
                }
            }
            RawPhase::Ended | RawPhase::Cancelled => {
                if self.state == State::Touching {
                    self.state = State::AfterLift {
                        lifted_at: now,
                        allowed: true,
                    };
                    Action::Lift
                } else {
                    Action::None
                }
            }
            // 段階の無いスクロール（マウスのホイールなど）は、ここでは扱わない。
            RawPhase::None => {
                let _ = moved;
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
    fn touching_the_pad_during_momentum_is_a_new_touch() {
        // 慣性（捨てている途中）に指がパッドに触れたら（動きの無い始まり）、すぐ新しい指として扱う。
        let mut m = ScrollMachine::default();
        let mut t = 0.0;
        m.event(t, TouchPhase::Started, Vec2::ZERO);
        t += DT;
        m.event(t, TouchPhase::Ended, Vec2::ZERO);
        t += DT;
        m.event(t, TouchPhase::Moved, Vec2::new(0.0, -10.0));
        m.drop_momentum();
        t += DT;
        m.event(t, TouchPhase::Started, Vec2::ZERO);
        assert!(m.touching());
        t += DT;
        assert_eq!(
            m.event(t, TouchPhase::Moved, Vec2::new(0.0, 6.0)),
            Action::Touch(Vec2::new(0.0, 6.0))
        );
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
        assert_eq!(action, Action::TouchBegin(Vec2::new(0.0, 4.0)));
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

    fn raw(m: &mut ScrollMachine, t: f64, p: RawPhase, mp: RawPhase, y: f32) -> Action {
        m.event_raw(t, p, mp, Vec2::new(0.0, y))
    }

    #[test]
    fn raw_touch_during_spring_is_immediate() {
        // 離して戻っている途中（慣性が来ている）にパッドへ触れたら（MayBegin）、すぐ指の状態。
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        raw(&mut m, 0.0, Began, None, 0.0);
        raw(&mut m, DT, Changed, None, -30.0);
        assert_eq!(raw(&mut m, 2.0 * DT, Ended, None, 0.0), Action::Lift);
        m.drop_momentum(); // 戻り始めたので、この慣性は使わない。
        raw(&mut m, 3.0 * DT, None, Began, -20.0);
        raw(&mut m, 4.0 * DT, None, Changed, -18.0);
        assert_eq!(
            raw(&mut m, 5.0 * DT, MayBegin, None, 0.0),
            Action::TouchBegin(Vec2::ZERO)
        );
        assert!(m.touching());
        assert_eq!(
            raw(&mut m, 6.0 * DT, Changed, None, 5.0),
            Action::Touch(Vec2::new(0.0, 5.0))
        );
    }

    #[test]
    fn raw_touch_during_momentum_discards_it() {
        // 使っている慣性の途中で触れたら、その慣性は終わり。残りの慣性の入力は指に混ざらない。
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        raw(&mut m, 0.0, Began, None, 0.0);
        raw(&mut m, DT, Changed, None, -30.0);
        raw(&mut m, 2.0 * DT, Ended, None, 0.0);
        assert!(matches!(
            raw(&mut m, 3.0 * DT, None, Began, -20.0),
            Action::Momentum { .. }
        ));
        raw(&mut m, 4.0 * DT, MayBegin, None, 0.0);
        assert!(m.touching());
        // 触れたあとに遅れて届いた慣性の入力は使わない。
        assert_eq!(raw(&mut m, 5.0 * DT, None, Changed, -10.0), Action::None);
        assert!(m.touching());
    }

    #[test]
    fn raw_momentum_began_is_never_a_touch() {
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        // 何もないところに慣性の始まりが来ても、指にはならない。
        let action = raw(&mut m, 0.0, None, Began, -20.0);
        assert!(!matches!(action, Action::TouchBegin(_) | Action::Touch(_)));
        assert!(!m.touching());
    }

    #[test]
    fn raw_grab_during_list_momentum() {
        // 慣性でリストが流れている途中に2本指を置く（MayBegin）。慣性の残りは使わない。
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        raw(&mut m, 0.0, Began, None, 0.0);
        raw(&mut m, DT, Changed, None, -30.0);
        raw(&mut m, 2.0 * DT, Ended, None, 0.0);
        for i in 0..5 {
            let action = raw(&mut m, (3 + i) as f64 * DT, None, Changed, -25.0);
            assert!(matches!(action, Action::Momentum { .. }));
        }
        assert_eq!(
            raw(&mut m, 9.0 * DT, MayBegin, None, 0.0),
            Action::TouchBegin(Vec2::ZERO)
        );
        for i in 0..10 {
            assert_eq!(
                raw(&mut m, (10 + i) as f64 * DT, None, Changed, -20.0),
                Action::None
            );
        }
        assert!(m.touching());
    }

    #[test]
    fn raw_tap_during_spring_grabs_then_releases() {
        // 戻る途中に軽く触れて離す（MayBegin → Cancelled、動きなし）。つかんで、離す。
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        raw(&mut m, 0.0, Began, None, 0.0);
        raw(&mut m, DT, Changed, None, 40.0);
        raw(&mut m, 2.0 * DT, Ended, None, 0.0);
        m.drop_momentum();
        assert_eq!(
            raw(&mut m, 10.0 * DT, MayBegin, None, 0.0),
            Action::TouchBegin(Vec2::ZERO)
        );
        assert_eq!(raw(&mut m, 14.0 * DT, Cancelled, None, 0.0), Action::Lift);
    }

    #[test]
    fn raw_early_momentum_end_is_a_grab() {
        // MayBegin が来なくても、速く流れている慣性が急に終わったら、触れてつかんだとみなす。
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        raw(&mut m, 0.0, Began, None, 0.0);
        raw(&mut m, DT, Changed, None, -30.0);
        raw(&mut m, 2.0 * DT, Ended, None, 0.0);
        raw(&mut m, 3.0 * DT, None, Began, -28.0);
        raw(&mut m, 4.0 * DT, None, Changed, -26.0);
        assert_eq!(
            raw(&mut m, 5.0 * DT, None, Ended, 0.0),
            Action::TouchBegin(Vec2::ZERO)
        );
        assert!(m.touching());
        // 指の合図が続かなければ、少しして離したことになる。
        assert_eq!(m.tick(5.0 * DT + 0.25), Action::Lift);
        assert!(!m.touching());
    }

    #[test]
    fn raw_natural_momentum_end_is_not_a_grab() {
        // 慣性が弱まりきってから終わったのは、ふつうの終わり。
        use RawPhase::*;
        let mut m = ScrollMachine::default();
        raw(&mut m, 0.0, Began, None, 0.0);
        raw(&mut m, DT, Changed, None, -30.0);
        raw(&mut m, 2.0 * DT, Ended, None, 0.0);
        raw(&mut m, 3.0 * DT, None, Began, -2.0);
        raw(&mut m, 4.0 * DT, None, Changed, -0.5);
        assert_eq!(raw(&mut m, 5.0 * DT, None, Ended, 0.0), Action::None);
        assert!(!m.touching());
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
