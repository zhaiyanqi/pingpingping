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
    #[cfg(target_os = "windows")]
    pub(crate) fn pending(&self) -> bool {
        self.held.is_none()
    }

    pub(crate) fn click(&mut self, cursor: Position) -> Option<Position> {
        let (wheel, origin) = self.held?;
        if wheel == Wheel::Ping
            && !self.visible
            && (cursor.x - origin.x).hypot(cursor.y - origin.y) <= OPEN_DISTANCE * self.scale
        {
            *self = Self::default();
            Some(cursor)
        } else {
            None
        }
    }

    pub(crate) fn input(
        &mut self,
        modifier: Option<Wheel>,
        cursor: Position,
        cancel: bool,
        scale: f64,
    ) -> Vec<GestureEvent> {
        if cancel {
            let visible = self.visible;
            *self = Self::default();
            return if visible {
                vec![GestureEvent::Finish(None)]
            } else {
                Vec::new()
            };
        }
        match (self.held, modifier) {
            (None, Some(wheel)) => {
                self.held = Some((wheel, cursor));
                self.scale = scale;
                Vec::new()
            }
            (Some((wheel, origin)), Some(next)) if wheel == next => {
                let selected = sector(origin, cursor, self.scale);
                if !self.visible
                    && (cursor.x - origin.x).hypot(cursor.y - origin.y) > OPEN_DISTANCE * self.scale
                {
                    self.visible = true;
                    self.selected = selected;
                    vec![GestureEvent::Open(wheel, origin, selected)]
                } else if self.visible && selected != self.selected {
                    self.selected = selected;
                    vec![GestureEvent::Select(selected)]
                } else {
                    Vec::new()
                }
            }
            (Some((wheel, origin)), next) => {
                // Commit using the release position so rendering can lag without changing the result.
                // 用松键时的坐标确认选择，避免界面刷新滞后影响结果。
                let committed = if self.visible && next.is_none() {
                    sector(origin, cursor, self.scale).map(|index| (wheel, index, origin))
                } else {
                    None
                };
                let was_visible = self.visible;
                *self = Self::default();
                self.held = next.map(|wheel| (wheel, cursor));
                self.scale = scale;
                if was_visible {
                    vec![GestureEvent::Finish(committed)]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_uses_final_coordinates_instead_of_the_last_painted_sector() {
        let origin = Position { x: 20.0, y: 30.0 };
        let mut gesture = Gesture::default();
        assert!(
            gesture
                .input(Some(Wheel::Ping), origin, false, 1.0)
                .is_empty()
        );
        let right = Position { x: 110.0, y: 30.0 };
        assert_eq!(
            gesture.input(Some(Wheel::Ping), right, false, 1.0).last(),
            Some(&GestureEvent::Open(Wheel::Ping, origin, Some(2)))
        );
        let up = Position { x: 20.0, y: -70.0 };
        assert_eq!(
            gesture.input(None, up, false, 1.0),
            [GestureEvent::Finish(Some((Wheel::Ping, 0, origin)))]
        );
    }

    #[test]
    fn opens_before_the_scaled_selection_radius_and_cancels_inside_it() {
        let origin = Position { x: 0.0, y: 0.0 };
        let mut gesture = Gesture::default();
        gesture.input(Some(Wheel::Emote), origin, false, 1.5);
        let near = Position { x: 13.0, y: 0.0 };
        assert_eq!(
            gesture.input(Some(Wheel::Emote), near, false, 1.5),
            [GestureEvent::Open(Wheel::Emote, origin, None)]
        );
        let boundary = Position { x: 129.0, y: 0.0 };
        assert_eq!(
            gesture.input(Some(Wheel::Emote), boundary, false, 1.5),
            [GestureEvent::Select(Some(2))]
        );
        assert_eq!(
            gesture.input(None, near, false, 1.5),
            [GestureEvent::Finish(None)]
        );
    }
}
