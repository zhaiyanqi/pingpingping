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
    Page(i32, Option<usize>),
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
    fn modifier(&self) -> u8 {
        match self {
            Self::Alt => ALT,
            Self::Ctrl => CTRL,
            Self::Shift => SHIFT,
            Self::Super => SUPER,
            _ => 0,
        }
    }

    fn label(&self) -> &str {
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

    fn same_key(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Keyboard { code, .. }, Self::Keyboard { code: other, .. }) => code == other,
            _ => self == other,
        }
    }
}

// The single-key form also reads bindings saved before chord support.
// 单键形式也用于读取支持组合键之前保存的绑定。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum Shortcut {
    Single(TriggerKey),
    Chord {
        modifiers: u8,
        key: Option<TriggerKey>,
    },
}

const MODIFIER_KEYS: [TriggerKey; 4] = [
    TriggerKey::Ctrl,
    TriggerKey::Alt,
    TriggerKey::Shift,
    TriggerKey::Super,
];

impl Shortcut {
    pub(crate) fn new(modifiers: u8, key: Option<TriggerKey>) -> Self {
        match key {
            Some(key) if modifiers == 0 => Self::Single(key),
            None if let Some(key) = MODIFIER_KEYS
                .into_iter()
                .find(|key| key.modifier() == modifiers) =>
            {
                Self::Single(key)
            }
            key => Self::Chord { modifiers, key },
        }
    }

    fn parts(&self) -> (u8, Option<&TriggerKey>) {
        let (modifiers, key) = match self {
            Self::Single(key) => (0, Some(key)),
            Self::Chord { modifiers, key } => (*modifiers, key.as_ref()),
        };
        (
            modifiers | key.map_or(0, TriggerKey::modifier),
            key.filter(|key| key.modifier() == 0),
        )
    }

    pub(crate) fn label(&self) -> String {
        let (modifiers, key) = self.parts();
        MODIFIER_KEYS
            .iter()
            .filter(|key| modifiers & key.modifier() != 0)
            .chain(key)
            .map(TriggerKey::label)
            .collect::<Vec<_>>()
            .join(" + ")
    }

    pub(crate) fn same_key(&self, other: &Self) -> bool {
        let (mods, key) = self.parts();
        let (other_mods, other_key) = other.parts();
        mods == other_mods
            && match (key, other_key) {
                (Some(a), Some(b)) => a.same_key(b),
                (None, None) => true,
                _ => false,
            }
    }

    fn matches(&self, modifiers: u8, keys: &[u16], sides: u8, partial: bool) -> bool {
        let (required, key) = self.parts();
        if modifiers & !required != 0 || (!partial && modifiers != required) {
            return false;
        }
        if partial && keys.is_empty() && sides == 0 {
            return true;
        }
        match key {
            Some(TriggerKey::Keyboard { code, .. }) => keys == [*code] && sides == 0,
            Some(TriggerKey::Mouse4) => keys.is_empty() && sides == 1,
            Some(TriggerKey::Mouse5) => keys.is_empty() && sides == 2,
            None => keys.is_empty() && sides == 0 && required != 0,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum TriggerMode {
    #[default]
    MouseDrag,
    HoldShortcut,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Triggers {
    pub(crate) ping_key: Shortcut,
    pub(crate) emote_key: Shortcut,
    pub(crate) mode: TriggerMode,
    pub(crate) ping_enabled: bool,
    pub(crate) emote_enabled: bool,
    pub(crate) click_ping: bool,
}

impl Default for Triggers {
    fn default() -> Self {
        Self {
            ping_key: Shortcut::Single(TriggerKey::Alt),
            emote_key: Shortcut::Single(TriggerKey::Ctrl),
            mode: TriggerMode::MouseDrag,
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

    fn match_wheel(&self, matches: impl Fn(&Shortcut) -> bool) -> Option<Wheel> {
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
    #[cfg(any(target_os = "windows", test))]
    SyncKeyboard,
    Side(u8, bool),
    Left(bool),
    OtherButton(u8, bool),
    Move,
    Scroll(i32),
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
    modifiers: u8,
    sides: u8,
    other_buttons: u64,
    held_button: Option<u8>,
    swallowed_buttons: u64,
    left: bool,
    suppressed: bool,
}

impl WheelInput {
    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn release_missing_keys(&mut self, is_down: impl Fn(u16) -> bool) -> bool {
        let previous = self.keys.len();
        self.keys.retain(|&key| is_down(key));
        self.keys.len() != previous
    }

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
        // Only a new physical edge can open or confirm a held shortcut. Repeats and
        // keyboard-state reconciliation may cancel a gesture but must never send one.
        // 只有真实按下或松开才能开启或确认按键轮盘。按键重复和状态校正只能取消手势。
        let shortcut_edge = match event {
            InputEvent::Key(code, down) if self.keys.contains(&code) != down => Some(down),
            InputEvent::Side(bit, down) if (self.sides & bit != 0) != down => Some(down),
            InputEvent::Modifier if modifiers != self.modifiers => match (
                modifiers & !self.modifiers != 0,
                self.modifiers & !modifiers != 0,
            ) {
                (true, false) => Some(true),
                (false, true) => Some(false),
                _ => None,
            },
            _ => None,
        };
        self.modifiers = modifiers;
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
        let matches =
            |key: &Shortcut, partial| key.matches(modifiers, &self.keys, self.sides, partial);
        let mut wheel = config.match_wheel(|key| matches(key, false));
        let invalid_chord = count > 1
            && wheel.is_none()
            && !(config.ping_enabled && matches(&config.ping_key, true))
            && !(config.emote_enabled && matches(&config.emote_key, true));
        let hold = config.mode == TriggerMode::HoldShortcut;
        let action = match event {
            _ if hold && shortcut_edge == Some(false) && self.gesture.held.is_some() => {
                wheel = self.gesture.held.map(|(owner, _)| owner);
                Pointer::Up
            }
            _ if hold && shortcut_edge == Some(true) && wheel.is_some() => Pointer::Open,
            InputEvent::Move => Pointer::Move,
            InputEvent::Scroll(delta) => Pointer::Scroll(delta),
            _ if hold => Pointer::Key,
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
            _ => Pointer::Key,
        };
        self.suppressed |= paused
            || invalid_chord
            || (hold && self.left)
            || (self.sides != 0 && self.left)
            || matches!(
                event,
                InputEvent::Interrupt | InputEvent::OtherButton(_, true)
            );
        let starts =
            matches!(action, Pointer::Down | Pointer::Open) && wheel.is_some() && !self.suppressed;
        let scale = if starts { scale() } else { 1.0 };
        // Hold the initiating press so a drag never leaves the foreground app with a stuck button.
        // 暂扣起始按下事件，避免拖动时前台应用只收到按下、收不到松开。
        let mut delivery = match button {
            Some((id, true)) if self.held_button == Some(id) => MouseDelivery::Swallow,
            Some((id, true)) if was_open => {
                self.swallowed_buttons |= 1u64 << id;
                MouseDelivery::Swallow
            }
            Some((id, true)) if starts => {
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
                InputEvent::Scroll(_) | InputEvent::Interrupt => MouseDelivery::Swallow,
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
    Open,
    Down,
    Up,
    Move,
    Scroll(i32),
    Key,
}

#[derive(Default)]
pub(crate) struct Gesture {
    held: Option<(Wheel, Position)>,
    visible: bool,
    selected: Option<usize>,
    scale: f64,
    scroll_remainder: i32,
}

pub(crate) const WHEEL_SIZE: u32 = 600;
pub(crate) const INNER_RADIUS: f64 = 86.0;
pub(crate) const SCROLL_STEP: i32 = 120;
const OPEN_DISTANCE: f64 = 8.0;

// Sector zero is up. Screen coordinates increase downward, and sectors advance clockwise.
// 上方为第 0 个扇区。屏幕坐标向下递增，扇区按顺时针排列。
pub(crate) fn sector(origin: Position, position: Position, scale: f64) -> Option<usize> {
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
        if action == Pointer::Open
            && !cancel
            && let Some(wheel) = wheel
        {
            let mut updates = Vec::new();
            if self.visible {
                updates.push(GestureEvent::Finish(None));
            }
            *self = Self {
                held: Some((wheel, cursor)),
                visible: true,
                scale,
                ..Self::default()
            };
            updates.push(GestureEvent::Open(wheel, cursor, None));
            return updates;
        }
        let Some((owner, origin)) = self.held else {
            if !cancel && action == Pointer::Down {
                self.held = wheel.map(|wheel| (wheel, cursor));
                self.scale = scale;
            }
            return Vec::new();
        };
        let distance = (cursor.x - origin.x).hypot(cursor.y - origin.y);
        if cancel || wheel != Some(owner) || action == Pointer::Up {
            // The owning release confirms at its own coordinates, even without a last move event.
            // 按结束手势的松键坐标确认，即使最后一次移动事件尚未送达。
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
        if let Pointer::Scroll(delta) = action {
            if !self.visible || owner != Wheel::Emote {
                return Vec::new();
            }
            // Accumulate high-resolution scroll input into whole wheel steps.
            // 累积高精度滚动输入，达到一个滚轮刻度时再翻页。
            let total = self.scroll_remainder.saturating_add(delta);
            let steps = total / SCROLL_STEP;
            self.scroll_remainder = total % SCROLL_STEP;
            if steps != 0 {
                let selected = sector(origin, cursor, self.scale);
                self.selected = selected;
                return vec![GestureEvent::Page(steps, selected)];
            }
            return Vec::new();
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
    fn recorded_chords_and_legacy_bindings_round_trip() {
        let mut config: Triggers = serde_json::from_str(
            r#"{"ping_key":{"Keyboard":{"code":75,"label":"K"}},"emote_key":"Mouse4"}"#,
        )
        .unwrap();
        assert_eq!(config.mode, TriggerMode::MouseDrag);
        assert_eq!(config.ping_key.label(), "K");
        assert!(config.emote_key.matches(0, &[], 1, false));
        config.mode = TriggerMode::HoldShortcut;
        config.ping_key = Shortcut::new(
            CTRL | SHIFT,
            Some(TriggerKey::Keyboard {
                code: 75,
                label: "K".into(),
            }),
        );
        config.emote_key = Shortcut::new(CTRL | SHIFT, None);
        assert_eq!(config.ping_key.label(), "Ctrl + Shift + K");
        assert!(!config.ping_key.same_key(&config.emote_key));
        assert!(config.ping_key.same_key(&Shortcut::new(
            CTRL | SHIFT,
            Some(TriggerKey::Keyboard {
                code: 75,
                label: "Other label".into()
            })
        )));
        let restored: Triggers =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert!(restored == config);
    }

    #[test]
    fn hold_mode_uses_real_shortcut_edges_and_final_release_coordinates() {
        let config = Triggers {
            mode: TriggerMode::HoldShortcut,
            ping_key: Shortcut::new(
                CTRL | SHIFT,
                Some(TriggerKey::Keyboard {
                    code: 75,
                    label: "K".into(),
                }),
            ),
            emote_enabled: false,
            ..Triggers::default()
        };
        let origin = Position { x: 250., y: 300. };
        let right = Position { x: 450., ..origin };
        let up = Position { y: 100., ..origin };
        for release_modifier_first in [false, true] {
            let mut input = WheelInput::default();
            let mut event = |mods, kind, at| input.event(&config, mods, kind, at, false, || 2.).0;
            assert!(event(CTRL, InputEvent::Modifier, origin).is_empty());
            assert!(event(CTRL | SHIFT, InputEvent::Modifier, origin).is_empty());
            assert_eq!(
                event(CTRL | SHIFT, InputEvent::Key(75, true), origin),
                [GestureEvent::Open(Wheel::Ping, origin, None)]
            );
            assert!(event(CTRL | SHIFT, InputEvent::Key(75, true), right).is_empty());
            assert!(
                event(
                    CTRL | SHIFT,
                    InputEvent::Move,
                    Position {
                        x: origin.x + 100.,
                        ..origin
                    }
                )
                .is_empty()
            );
            assert_eq!(
                event(CTRL | SHIFT, InputEvent::Move, right),
                [GestureEvent::Select(Some(2))]
            );
            let (mods, release) = if release_modifier_first {
                (CTRL, InputEvent::Modifier)
            } else {
                (CTRL | SHIFT, InputEvent::Key(75, false))
            };
            assert_eq!(
                event(mods, release, up),
                [GestureEvent::Finish(Some((Wheel::Ping, 0, origin)))]
            );
            assert!(event(0, InputEvent::Modifier, up).is_empty());
            assert!(event(0, InputEvent::Key(75, false), up).is_empty());
        }
    }

    #[test]
    fn modifier_and_side_button_chord_obeys_the_shared_mode() {
        for mode in [TriggerMode::MouseDrag, TriggerMode::HoldShortcut] {
            let config = Triggers {
                mode,
                emote_key: Shortcut::new(CTRL | SHIFT, Some(TriggerKey::Mouse5)),
                ..Triggers::default()
            };
            let at = Position { x: 50., y: 10. };
            let mut input = WheelInput::default();
            let mut event = |mods, kind| input.event(&config, mods, kind, at, false, || 1.);
            event(SHIFT, InputEvent::Modifier);
            event(CTRL | SHIFT, InputEvent::Modifier);
            let (updates, delivery) = event(CTRL | SHIFT, InputEvent::Side(2, true));
            assert_eq!(delivery, MouseDelivery::HoldDown);
            if mode == TriggerMode::HoldShortcut {
                assert_eq!(updates, [GestureEvent::Open(Wheel::Emote, at, None)]);
                assert_eq!(
                    event(CTRL | SHIFT, InputEvent::Side(2, false)),
                    (vec![GestureEvent::Finish(None)], MouseDelivery::Swallow)
                );
            } else {
                assert!(updates.is_empty());
                assert_eq!(
                    event(CTRL | SHIFT, InputEvent::Side(2, false)),
                    (vec![], MouseDelivery::ReplayClick)
                );
            }
        }
    }

    #[test]
    fn hold_mode_switches_to_a_longer_binding_and_never_sends_on_resync() {
        let config = Triggers {
            mode: TriggerMode::HoldShortcut,
            emote_key: Shortcut::new(ALT | SHIFT, None),
            ..Triggers::default()
        };
        let at = Position { x: 120., y: 240. };
        let mut input = WheelInput::default();
        let mut event = |mods, kind| input.event(&config, mods, kind, at, false, || 1.).0;
        assert_eq!(
            event(ALT, InputEvent::Modifier),
            [GestureEvent::Open(Wheel::Ping, at, None)]
        );
        assert_eq!(
            event(ALT | SHIFT, InputEvent::Modifier),
            [
                GestureEvent::Finish(None),
                GestureEvent::Open(Wheel::Emote, at, None)
            ]
        );
        assert_eq!(
            event(ALT, InputEvent::SyncKeyboard),
            [GestureEvent::Finish(None)]
        );
        assert!(event(0, InputEvent::Modifier).is_empty());
        assert!(event(ALT, InputEvent::SyncKeyboard).is_empty());
    }

    #[test]
    fn scrolling_only_turns_open_emote_wheels_and_release_uses_final_coordinates() {
        let mut state = WheelInput::default();
        let mut event = |input, x| {
            state.event(
                &Triggers::default(),
                CTRL,
                input,
                Position { x, y: 0. },
                false,
                || 1.,
            )
        };
        let partial = InputEvent::Scroll(SCROLL_STEP / 2);
        assert_eq!(event(InputEvent::Left(true), 0.).1, MouseDelivery::HoldDown);
        assert_eq!(event(partial, 0.), (vec![], MouseDelivery::Pass));
        assert_eq!(event(InputEvent::Move, 100.).1, MouseDelivery::WheelMove);
        assert_eq!(event(partial, 100.), (vec![], MouseDelivery::Swallow));
        assert_eq!(
            event(partial, 100.),
            (vec![GestureEvent::Page(1, Some(2))], MouseDelivery::Swallow)
        );
        assert_eq!(
            event(InputEvent::Scroll(-SCROLL_STEP), 100.),
            (
                vec![GestureEvent::Page(-1, Some(2))],
                MouseDelivery::Swallow
            )
        );
        assert!(matches!(
            event(InputEvent::Left(false), -100.),
            (updates, MouseDelivery::Swallow) if matches!(updates.as_slice(), [GestureEvent::Finish(Some((Wheel::Emote, 6, _)))])
        ));
        assert_eq!(
            event(InputEvent::Scroll(SCROLL_STEP), -100.),
            (vec![], MouseDelivery::Pass)
        );
    }

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
            emote_key: Shortcut::Single(TriggerKey::Mouse5),
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
            emote_key: Shortcut::Single(TriggerKey::Mouse4),
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
    fn a_missed_key_release_rearms_the_next_shortcut_after_a_chord() {
        const L: u16 = 0x4c;
        let config = Triggers::default();
        let at = Position { x: 0., y: 0. };
        let mut input = WheelInput::default();
        let event = |state: &mut WheelInput, modifiers, kind| {
            state.event(&config, modifiers, kind, at, false, || 1.).1
        };

        event(&mut input, SUPER, InputEvent::Key(L, true));
        assert!(input.release_missing_keys(|_| false));
        event(&mut input, 0, InputEvent::Modifier);
        event(&mut input, ALT, InputEvent::Modifier);
        assert_eq!(
            event(&mut input, ALT, InputEvent::Left(true)),
            MouseDelivery::HoldDown
        );
    }

    #[test]
    fn reconciling_keys_keeps_a_held_chord_suppressed_and_cancels_a_lost_drag() {
        const K: u16 = 0x4b;
        const L: u16 = 0x4c;
        let config = Triggers {
            ping_key: Shortcut::Single(TriggerKey::Keyboard {
                code: K,
                label: "K".into(),
            }),
            ..Triggers::default()
        };
        let mut input = WheelInput::default();
        let event = |state: &mut WheelInput, kind, x| {
            state.event(&config, 0, kind, Position { x, y: 0. }, false, || 1.)
        };
        event(&mut input, InputEvent::Key(K, true), 0.);
        event(&mut input, InputEvent::Key(L, true), 0.);
        assert!(input.release_missing_keys(|key| key == K));
        event(&mut input, InputEvent::Modifier, 0.);
        assert_eq!(
            event(&mut input, InputEvent::Left(true), 0.).1,
            MouseDelivery::Pass
        );
        assert_eq!(
            event(&mut input, InputEvent::Left(false), 0.).1,
            MouseDelivery::Pass
        );

        event(&mut input, InputEvent::Key(K, false), 0.);
        event(&mut input, InputEvent::Key(K, true), 0.);
        assert_eq!(
            event(&mut input, InputEvent::Left(true), 0.).1,
            MouseDelivery::HoldDown
        );
        assert_eq!(
            event(&mut input, InputEvent::Move, 100.).1,
            MouseDelivery::WheelMove
        );
        assert!(input.release_missing_keys(|_| false));
        assert_eq!(
            event(&mut input, InputEvent::Modifier, 0.).0,
            [GestureEvent::Finish(None)]
        );
        assert_eq!(
            event(&mut input, InputEvent::Left(false), 100.),
            (vec![], MouseDelivery::Swallow)
        );
    }

    #[test]
    fn side_click_replays_once_but_opening_the_wheel_consumes_the_click_even_after_cancel() {
        let config = Triggers {
            emote_key: Shortcut::Single(TriggerKey::Mouse5),
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
                emote_key: Shortcut::Single(TriggerKey::Mouse5),
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
            ping_key: Shortcut::Single(TriggerKey::Mouse4),
            emote_key: Shortcut::Single(TriggerKey::Mouse5),
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
            ping_key: Shortcut::Single(TriggerKey::Mouse5),
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
    fn key_chord_in_mouse_mode_requires_left_drag_and_cancels_on_an_extra_key() {
        let config = Triggers {
            emote_key: Shortcut::new(
                CTRL | SHIFT,
                Some(TriggerKey::Keyboard {
                    code: 42,
                    label: "K".into(),
                }),
            ),
            ..Triggers::default()
        };
        let origin = Position { x: 0., y: 0. };
        let end = Position { x: 100., y: 0. };
        let mut input = WheelInput::default();
        let mut event = |event, at| {
            input
                .event(&config, CTRL | SHIFT, event, at, false, || 1.)
                .0
        };
        assert!(event(InputEvent::Modifier, origin).is_empty());
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
            emote_key: Shortcut::Single(TriggerKey::Shift),
            ..Triggers::default()
        };
        assert_eq!(
            config.match_wheel(|key| *key == Shortcut::Single(TriggerKey::Alt)),
            None
        );
        assert_eq!(
            config.match_wheel(|key| *key == Shortcut::Single(TriggerKey::Ctrl)),
            None
        );
        assert_eq!(
            config.match_wheel(|key| *key == Shortcut::Single(TriggerKey::Shift)),
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
