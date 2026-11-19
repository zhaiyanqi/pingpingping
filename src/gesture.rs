//! Platform-independent gesture recognition in desktop coordinates.
//! 使用桌面坐标的跨平台手势识别。

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Position {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Wheel {
    Ping,
    Emote,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GestureEvent {
    Open(Wheel, Position, Option<usize>),
    Select(Option<usize>),
    Finish(Option<(Wheel, usize, Position)>),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum TriggerKey {
    Alt,
    Ctrl,
    Shift,
    Super,
    Mouse4,
    Mouse5,
    Keyboard { code: u16, label: String },
}

impl TriggerKey {
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Alt if cfg!(target_os = "macos") => "Option",
            Self::Alt => "Alt",
            Self::Ctrl => "Ctrl",
            Self::Shift => "Shift",
            Self::Super if cfg!(target_os = "macos") => "Command",
            Self::Super => "Win",
            Self::Mouse4 => "鼠标侧键 4（后退）",
            Self::Mouse5 => "鼠标侧键 5（前进）",
            Self::Keyboard { label, .. } => label,
        }
    }

    pub(crate) fn same_key(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Keyboard { code, .. }, Self::Keyboard { code: other, .. }) => code == other,
            _ => self == other,
        }
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Triggers {
    pub(crate) ping_key: TriggerKey,
    pub(crate) emote_key: TriggerKey,
    pub(crate) ping_enabled: bool,
    pub(crate) emote_enabled: bool,
    pub(crate) click_ping: bool,
}

impl Default for Triggers {
    fn default() -> Self {
        Self {
            ping_key: TriggerKey::Alt,
            emote_key: TriggerKey::Ctrl,
            ping_enabled: true,
            emote_enabled: true,
            click_ping: false,
        }
    }
}

impl Triggers {
    #[cfg(target_os = "macos")]
    pub(crate) fn captures_mouse(&self) -> bool {
        self.ping_enabled || self.emote_enabled
    }

    fn match_wheel(&self, matches: impl Fn(&TriggerKey) -> bool) -> Option<Wheel> {
        match (
            self.ping_enabled && matches(&self.ping_key),
            self.emote_enabled && matches(&self.emote_key),
        ) {
            (true, false) => Some(Wheel::Ping),
            (false, true) => Some(Wheel::Emote),
            _ => None,
        }
    }
}

pub(crate) const ALT: u8 = 1;
pub(crate) const CTRL: u8 = 2;
pub(crate) const SHIFT: u8 = 4;
pub(crate) const SUPER: u8 = 8;

#[derive(Clone, Copy)]
pub(crate) enum InputEvent {
    Key(u16, bool),
    Modifier,
    Side(u8, bool),
    Left(bool),
    OtherButton(u8, bool),
    Move,
    Interrupt,
}

impl InputEvent {
    fn button(self) -> Option<(u8, bool)> {
        match self {
            Self::Left(down) => Some((0, down)),
            Self::Side(bit, down) => Some((bit + 2, down)),
            Self::OtherButton(number, down) => Some((number, down)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseDelivery {
    Pass,
    HoldDown,
    WheelMove,
    Swallow,
    ReplayClick,
}

#[derive(Default)]
pub(crate) struct WheelInput {
    gesture: Gesture,
    keys: Vec<u16>,
    sides: u8,
    other_buttons: u64,
    held_button: Option<u8>,
    swallowed_buttons: u64,
    left: bool,
    suppressed: bool,
}

impl WheelInput {
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn reset_after_gap(&mut self) {
        // If the withheld press is still held, its eventual release must also be consumed.
        // 如果被暂扣的鼠标键仍按着，恢复监听后还需拦截它的松开事件。
        *self = Self {
            swallowed_buttons: self.swallowed_buttons | self.held_button.map_or(0, |id| 1u64 << id),
            ..Self::default()
        };
    }

    pub(crate) fn active(&self) -> bool {
        !self.keys.is_empty() || self.sides != 0 || self.left || self.other_buttons != 0
    }

    pub(crate) fn event(
        &mut self,
        config: &Triggers,
        modifiers: u8,
        event: InputEvent,
        at: Position,
        paused: bool,
        scale: impl FnOnce() -> f64,
    ) -> (Vec<GestureEvent>, MouseDelivery) {
        let button = event.button();
        let was_open = self.gesture.visible;
        if let Some((id, true)) = button {
            // A new down also recovers from a release lost while the hook was suspended.
            // 如果监听暂停时丢失了松开事件，新按下事件会重新开始这一对按键。
            self.swallowed_buttons &= !(1u64 << id);
        }
        match event {
            InputEvent::Key(code, true) if !self.keys.contains(&code) => self.keys.push(code),
            InputEvent::Key(code, false) => self.keys.retain(|key| *key != code),
            InputEvent::Side(bit, true) => {
                self.sides |= bit;
            }
            InputEvent::Left(true) => self.left = true,
            InputEvent::OtherButton(id, true) => self.other_buttons |= 1u64 << id,
            _ => {}
        }
        // Resolve ownership before clearing the released button. A side button owns its whole drag.
        // 先判定手势归属，再清除松开的鼠标键。侧键独立完成整个拖动。
        let count = self.keys.len() + (self.sides.count_ones() + modifiers.count_ones()) as usize;
        let wheel = if count == 1 {
            config.match_wheel(|key| match key {
                TriggerKey::Alt => modifiers == ALT,
                TriggerKey::Ctrl => modifiers == CTRL,
                TriggerKey::Shift => modifiers == SHIFT,
                TriggerKey::Super => modifiers == SUPER,
                TriggerKey::Mouse4 => self.sides == 1,
                TriggerKey::Mouse5 => self.sides == 2,
                TriggerKey::Keyboard { code, .. } => self.keys.first() == Some(code),
            })
        } else {
            None
        };
        let action = match event {
            InputEvent::Side(bit, down) if self.sides == bit => {
                if down {
                    Pointer::Down
                } else {
                    Pointer::Up
                }
            }
            InputEvent::Left(down) if self.sides == 0 => {
                if down {
                    Pointer::Down
                } else {
                    Pointer::Up
                }
            }
            InputEvent::Move => Pointer::Move,
            _ => Pointer::Key,
        };
        self.suppressed |= paused
            || count > 1
            || (self.sides != 0 && self.left)
            || matches!(
                event,
                InputEvent::Interrupt | InputEvent::OtherButton(_, true)
            );
        let scale = if action == Pointer::Down && wheel.is_some() && !self.suppressed {
            scale()
        } else {
            1.0
        };
        // Hold the initiating press so a drag never leaves the foreground app with a stuck button.
        // 暂扣起始按下事件，避免拖动时前台应用只收到按下、收不到松开。
        let mut delivery = match button {
            Some((id, true)) if self.held_button == Some(id) => MouseDelivery::Swallow,
            Some((id, true)) if was_open => {
                self.swallowed_buttons |= 1u64 << id;
                MouseDelivery::Swallow
            }
            Some((id, true)) if action == Pointer::Down && wheel.is_some() && !self.suppressed => {
                self.held_button = Some(id);
                MouseDelivery::HoldDown
            }
            Some((id, false)) if self.swallowed_buttons & (1u64 << id) != 0 => {
                self.swallowed_buttons &= !(1u64 << id);
                MouseDelivery::Swallow
            }
            Some((id, false)) if self.held_button == Some(id) => {
                self.held_button = None;
                if !self.suppressed
                    && !self.gesture.visible
                    && self.gesture.held.is_some_and(|(_, origin)| {
                        (at.x - origin.x).hypot(at.y - origin.y)
                            <= OPEN_DISTANCE * self.gesture.scale
                    })
                {
                    MouseDelivery::ReplayClick
                } else {
                    MouseDelivery::Swallow
                }
            }
            _ => MouseDelivery::Pass,
        };
        let updates =
            self.gesture
                .input(wheel, at, action, self.suppressed, scale, config.click_ping);
        if was_open || self.gesture.visible {
            delivery = match event {
                InputEvent::Move => MouseDelivery::WheelMove,
                InputEvent::Interrupt => MouseDelivery::Swallow,
                _ => delivery,
            };
        }
        match event {
            InputEvent::Side(bit, false) => self.sides &= !bit,
            InputEvent::Left(false) => self.left = false,
            InputEvent::OtherButton(id, false) => self.other_buttons &= !(1u64 << id),
            _ => {}
        }
        if modifiers == 0 && !self.active() {
            self.suppressed = false;
        }
        (updates, delivery)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pointer {
    Down,
    Up,
    Move,
    Key,
}

#[derive(Default)]
pub(crate) struct Gesture {
    held: Option<(Wheel, Position)>,
    visible: bool,
    selected: Option<usize>,
    scale: f64,
}

pub(crate) const WHEEL_SIZE: u32 = 600;
pub(crate) const INNER_RADIUS: f64 = 86.0;
const OPEN_DISTANCE: f64 = 8.0;

// Sector zero is up. Screen coordinates increase downward, and sectors advance clockwise.
// 上方为第 0 个扇区。屏幕坐标向下递增，扇区按顺时针排列。
fn sector(origin: Position, position: Position, scale: f64) -> Option<usize> {
    let dx = position.x - origin.x;
    let dy = position.y - origin.y;
    if dx.hypot(dy) < INNER_RADIUS * scale {
        return None;
    }
    let eighth = std::f64::consts::FRAC_PI_4;
    Some(((dx.atan2(-dy) / eighth + 0.5).floor() as i32).rem_euclid(8) as usize)
}

impl Gesture {
    pub(crate) fn input(
        &mut self,
        wheel: Option<Wheel>,
        cursor: Position,
        action: Pointer,
        cancel: bool,
        scale: f64,
        click_ping: bool,
    ) -> Vec<GestureEvent> {
        let Some((owner, origin)) = self.held else {
            if !cancel && action == Pointer::Down {
                self.held = wheel.map(|wheel| (wheel, cursor));
                self.scale = scale;
            }
            return Vec::new();
        };
        let distance = (cursor.x - origin.x).hypot(cursor.y - origin.y);
        if cancel || wheel != Some(owner) || action == Pointer::Up {
            // Only button-up confirms. Releasing the shortcut first cancels the pending drag.
            // 仅松开鼠标时确认；先松开快捷键会取消当前拖动。
            let finish = !cancel && wheel == Some(owner) && action == Pointer::Up;
            let choice = if finish && self.visible {
                sector(origin, cursor, self.scale).map(|index| (owner, index, origin))
            } else if finish
                && click_ping
                && owner == Wheel::Ping
                && distance <= OPEN_DISTANCE * self.scale
            {
                Some((owner, 8, cursor))
            } else {
                None
            };
            let notify = self.visible || choice.is_some();
            *self = Self::default();
            return if notify {
                vec![GestureEvent::Finish(choice)]
            } else {
                Vec::new()
            };
        }
        if action != Pointer::Move {
            return Vec::new();
        }
        let selected = sector(origin, cursor, self.scale);
        if !self.visible && distance > OPEN_DISTANCE * self.scale {
            self.visible = true;
            self.selected = selected;
            vec![GestureEvent::Open(owner, origin, selected)]
        } else if self.visible && selected != self.selected {
            self.selected = selected;
            vec![GestureEvent::Select(selected)]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_drag_consumes_open_move_selection_and_release() {
        let mut state = WheelInput::default();
        let mut event = |kind, x| {
            state.event(
                &Triggers::default(),
                ALT,
                kind,
                Position { x, y: 0. },
                false,
                || 1.,
            )
        };
        assert_eq!(event(InputEvent::Left(true), 0.).1, MouseDelivery::HoldDown);
        assert_eq!(
            event(InputEvent::Left(false), 1.).1,
            MouseDelivery::ReplayClick
        );
        assert_eq!(event(InputEvent::Left(true), 0.).1, MouseDelivery::HoldDown);
        assert_eq!(event(InputEvent::Move, 9.).1, MouseDelivery::WheelMove);
        let (updates, delivery) = event(InputEvent::Move, 100.);
        assert_eq!(
            (updates, delivery),
            (
                vec![GestureEvent::Select(Some(2))],
                MouseDelivery::WheelMove
            )
        );
        let (updates, delivery) = event(InputEvent::Left(false), -100.);
        assert!(matches!(
            updates.as_slice(),
            [GestureEvent::Finish(Some((Wheel::Ping, 6, _)))]
        ));
        assert_eq!(delivery, MouseDelivery::Swallow);
        assert_eq!(event(InputEvent::Move, 0.).1, MouseDelivery::Pass);
    }

    #[test]
    fn cancelling_mouse_events_and_their_releases_do_not_leak() {
        let config = Triggers {
            emote_key: TriggerKey::Mouse5,
            ..Triggers::default()
        };
        for interrupt in [
            InputEvent::OtherButton(1, true),
            InputEvent::OtherButton(2, true),
            InputEvent::Left(true),
            InputEvent::Side(1, true),
            InputEvent::Interrupt,
        ] {
            let mut state = WheelInput::default();
            let mut event =
                |kind, x| state.event(&config, 0, kind, Position { x, y: 0. }, false, || 1.);
            assert_eq!(
                event(InputEvent::Side(2, true), 0.).1,
                MouseDelivery::HoldDown
            );
            assert_eq!(event(InputEvent::Move, 90.).1, MouseDelivery::WheelMove);
            assert_eq!(
                event(interrupt, 90.),
                (vec![GestureEvent::Finish(None)], MouseDelivery::Swallow)
            );
            assert_eq!(
                event(InputEvent::Side(2, false), 90.).1,
                MouseDelivery::Swallow
            );
            let release = match interrupt {
                InputEvent::OtherButton(id, _) => InputEvent::OtherButton(id, false),
                InputEvent::Left(_) => InputEvent::Left(false),
                InputEvent::Side(bit, _) => InputEvent::Side(bit, false),
                _ => continue,
            };
            assert_eq!(event(release, 90.).1, MouseDelivery::Swallow);
            assert_eq!(event(interrupt, 90.).1, MouseDelivery::Pass);
        }
    }

    #[test]
    fn a_gap_in_input_cancels_the_held_click_and_accepts_the_next_press() {
        let config = Triggers {
            emote_key: TriggerKey::Mouse4,
            ..Triggers::default()
        };
        let at = Position { x: 0., y: 0. };
        let mut input = WheelInput::default();
        let side = |state: &mut WheelInput, down| {
            state
                .event(&config, 0, InputEvent::Side(1, down), at, false, || 1.)
                .1
        };
        assert_eq!(side(&mut input, true), MouseDelivery::HoldDown);
        input.reset_after_gap();
        assert_eq!(side(&mut input, false), MouseDelivery::Swallow);
        assert_eq!(side(&mut input, true), MouseDelivery::HoldDown);
        input.reset_after_gap();
        assert_eq!(side(&mut input, true), MouseDelivery::HoldDown);
        assert_eq!(side(&mut input, false), MouseDelivery::ReplayClick);
    }

    #[test]
    fn side_click_replays_once_but_opening_the_wheel_consumes_the_click_even_after_cancel() {
        let config = Triggers {
            emote_key: TriggerKey::Mouse5,
            ..Triggers::default()
        };
        let mut input = WheelInput::default();
        let mut event =
            |kind, x| input.event(&config, 0, kind, Position { x, y: 0. }, false, || 1.5);
        for open in [false, true] {
            assert_eq!(
                event(InputEvent::Side(2, true), 0.).1,
                MouseDelivery::HoldDown
            );
            assert_eq!(event(InputEvent::Move, 12.), (vec![], MouseDelivery::Pass));
            if open {
                let (updates, delivery) = event(InputEvent::Move, 13.);
                assert!(matches!(
                    updates.as_slice(),
                    [GestureEvent::Open(Wheel::Emote, _, None)]
                ));
                assert_eq!(delivery, MouseDelivery::WheelMove);
                assert_eq!(
                    event(InputEvent::Side(2, false), 0.),
                    (vec![GestureEvent::Finish(None)], MouseDelivery::Swallow)
                );
            } else {
                assert_eq!(
                    event(InputEvent::Side(2, false), 0.),
                    (vec![], MouseDelivery::ReplayClick)
                );
            }
            assert_eq!(event(InputEvent::Side(2, false), 0.).1, MouseDelivery::Pass);
        }
    }

    #[test]
    fn unclaimed_side_buttons_pass_both_edges() {
        let at = Position { x: 0., y: 0. };
        let mut input = WheelInput::default();
        for (enabled, bit, modifiers, recording) in [
            (false, 2, 0, false),
            (true, 1, 0, false),
            (true, 2, CTRL, false),
            (true, 2, 0, true),
        ] {
            let config = Triggers {
                emote_key: TriggerKey::Mouse5,
                emote_enabled: enabled,
                ..Triggers::default()
            };
            for down in [true, false] {
                assert_eq!(
                    input
                        .event(
                            &config,
                            modifiers,
                            InputEvent::Side(bit, down),
                            at,
                            recording,
                            || 1.
                        )
                        .1,
                    MouseDelivery::Pass
                );
            }
            input.event(&config, 0, InputEvent::Modifier, at, false, || 1.);
        }
    }

    #[test]
    fn both_side_buttons_use_their_own_press_and_release() {
        let config = Triggers {
            ping_key: TriggerKey::Mouse4,
            emote_key: TriggerKey::Mouse5,
            ..Triggers::default()
        };
        let config: Triggers =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        let origin = Position { x: 0., y: 0. };
        let end = Position { x: 100., y: 0. };
        let mut input = WheelInput::default();
        for (bit, wheel) in [(1, Wheel::Ping), (2, Wheel::Emote)] {
            let mut event = |event, at| input.event(&config, 0, event, at, false, || 1.).0;
            assert!(event(InputEvent::Side(bit, true), origin).is_empty());
            assert_eq!(
                event(InputEvent::Move, end),
                [GestureEvent::Open(wheel, origin, Some(2))]
            );
            assert!(event(InputEvent::Side(3 - bit, false), end).is_empty());
            assert_eq!(
                event(InputEvent::Side(bit, false), end),
                [GestureEvent::Finish(Some((wheel, 2, origin)))]
            );
        }
    }

    #[test]
    fn capture_press_stays_suppressed_through_release() {
        let config = Triggers {
            ping_key: TriggerKey::Mouse5,
            click_ping: true,
            ..Triggers::default()
        };
        let at = Position { x: 0., y: 0. };
        let mut input = WheelInput::default();
        for (down, paused) in [(true, true), (false, false), (true, false)] {
            assert!(
                input
                    .event(&config, 0, InputEvent::Side(2, down), at, paused, || 1.)
                    .0
                    .is_empty()
            );
        }
        assert_eq!(
            input
                .event(&config, 0, InputEvent::Side(2, false), at, false, || 1.)
                .0,
            [GestureEvent::Finish(Some((Wheel::Ping, 8, at)))]
        );
    }

    #[test]
    fn custom_keyboard_binding_requires_left_drag_and_cancels_on_chords() {
        let config = Triggers {
            emote_key: TriggerKey::Keyboard {
                code: 42,
                label: "K".into(),
            },
            ..Triggers::default()
        };
        let origin = Position { x: 0., y: 0. };
        let end = Position { x: 100., y: 0. };
        let mut input = WheelInput::default();
        let mut event = |event, at| input.event(&config, 0, event, at, false, || 1.).0;
        for _ in 0..2 {
            assert!(event(InputEvent::Key(42, true), origin).is_empty());
        }
        assert!(event(InputEvent::Move, end).is_empty());
        assert!(event(InputEvent::Left(true), origin).is_empty());
        assert_eq!(
            event(InputEvent::Move, end),
            [GestureEvent::Open(Wheel::Emote, origin, Some(2))]
        );
        assert_eq!(
            event(InputEvent::Key(43, true), end),
            [GestureEvent::Finish(None)]
        );
        assert!(event(InputEvent::Key(43, false), end).is_empty());
        assert!(event(InputEvent::Left(false), end).is_empty());
    }

    #[test]
    fn hover_and_default_click_do_not_send() {
        let ping = Some(Wheel::Ping);
        let start = Position { x: 0., y: 0. };
        let end = Position { x: 200., y: 0. };
        let mut gesture = Gesture::default();
        for (action, at) in [
            (Pointer::Key, start),
            (Pointer::Move, end),
            (Pointer::Down, end),
            (Pointer::Up, end),
        ] {
            assert!(gesture.input(ping, at, action, false, 1., false).is_empty());
        }
        assert!(
            gesture
                .input(ping, end, Pointer::Down, false, 1., true)
                .is_empty()
        );
        assert_eq!(
            gesture.input(ping, end, Pointer::Up, false, 1., true),
            [GestureEvent::Finish(Some((Wheel::Ping, 8, end)))]
        );
    }

    #[test]
    fn interrupted_drag_needs_a_fresh_button_press() {
        let ping = Some(Wheel::Ping);
        let start = Position { x: 0., y: 0. };
        let end = Position { x: 200., y: 0. };
        let mut gesture = Gesture::default();
        gesture.input(ping, start, Pointer::Down, false, 1., true);
        gesture.input(ping, end, Pointer::Move, false, 1., true);
        assert_eq!(
            gesture.input(None, end, Pointer::Key, true, 1., true),
            [GestureEvent::Finish(None)]
        );
        assert!(
            gesture
                .input(ping, end, Pointer::Move, false, 1., true)
                .is_empty()
        );
        assert!(
            gesture
                .input(ping, end, Pointer::Up, false, 1., true)
                .is_empty()
        );
    }

    #[test]
    fn disabled_wheel_does_not_claim_its_shortcut() {
        let config = Triggers {
            ping_enabled: false,
            emote_key: TriggerKey::Shift,
            ..Triggers::default()
        };
        assert_eq!(config.match_wheel(|key| *key == TriggerKey::Alt), None);
        assert_eq!(config.match_wheel(|key| *key == TriggerKey::Ctrl), None);
        assert_eq!(
            config.match_wheel(|key| *key == TriggerKey::Shift),
            Some(Wheel::Emote)
        );
    }

    #[test]
    fn release_uses_final_coordinates_instead_of_the_last_painted_sector() {
        let origin = Position { x: 20.0, y: 30.0 };
        let mut gesture = Gesture::default();
        assert!(
            gesture
                .input(Some(Wheel::Ping), origin, Pointer::Down, false, 1.0, false)
                .is_empty()
        );
        let right = Position { x: 110.0, y: 30.0 };
        assert_eq!(
            gesture
                .input(Some(Wheel::Ping), right, Pointer::Move, false, 1.0, false)
                .last(),
            Some(&GestureEvent::Open(Wheel::Ping, origin, Some(2)))
        );
        let up = Position { x: 20.0, y: -70.0 };
        assert_eq!(
            gesture.input(Some(Wheel::Ping), up, Pointer::Up, false, 1.0, false),
            [GestureEvent::Finish(Some((Wheel::Ping, 0, origin)))]
        );
    }

    #[test]
    fn opens_before_the_scaled_selection_radius_and_cancels_inside_it() {
        let origin = Position { x: 0.0, y: 0.0 };
        let mut gesture = Gesture::default();
        gesture.input(Some(Wheel::Emote), origin, Pointer::Down, false, 1.5, false);
        let near = Position { x: 13.0, y: 0.0 };
        assert_eq!(
            gesture.input(Some(Wheel::Emote), near, Pointer::Move, false, 1.5, false),
            [GestureEvent::Open(Wheel::Emote, origin, None)]
        );
        let boundary = Position { x: 129.0, y: 0.0 };
        assert_eq!(
            gesture.input(
                Some(Wheel::Emote),
                boundary,
                Pointer::Move,
                false,
                1.5,
                false
            ),
            [GestureEvent::Select(Some(2))]
        );
        assert_eq!(
            gesture.input(Some(Wheel::Emote), near, Pointer::Up, false, 1.5, false),
            [GestureEvent::Finish(None)]
        );
    }
}
