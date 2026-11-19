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
    Open(Wheel, Position, usize),
    Select(Option<usize>),
    Finish(Option<(Wheel, usize, Position)>),
}

#[derive(Default)]
pub(crate) struct Gesture {
    held: Option<(Wheel, Position)>,
    visible: bool,
    selected: Option<usize>,
}

// Sector zero is up. Screen coordinates increase downward, and sectors advance clockwise.
// 上方为第 0 个扇区。屏幕坐标向下递增，扇区按顺时针排列。
pub(crate) fn sector(origin: Position, position: Position) -> Option<usize> {
    let dx = position.x - origin.x;
    let dy = position.y - origin.y;
    if dx.hypot(dy) < 18.0 {
        return None;
    }
    let eighth = std::f64::consts::FRAC_PI_4;
    Some(((dx.atan2(-dy) / eighth + 0.5).floor() as i32).rem_euclid(8) as usize)
}

impl Gesture {
    pub(crate) fn click(&mut self, cursor: Position) -> Option<Position> {
        let (wheel, origin) = self.held?;
        if wheel == Wheel::Ping && !self.visible && sector(origin, cursor).is_none() {
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
                Vec::new()
            }
            (Some((wheel, origin)), Some(next)) if wheel == next => {
                let selected = sector(origin, cursor);
                if !self.visible
                    && let Some(index) = selected
                {
                    self.visible = true;
                    self.selected = selected;
                    vec![GestureEvent::Open(wheel, origin, index)]
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
                    sector(origin, cursor).map(|index| (wheel, index, origin))
                } else {
                    None
                };
                let was_visible = self.visible;
                *self = Self::default();
                self.held = next.map(|wheel| (wheel, cursor));
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
        assert!(gesture.input(Some(Wheel::Ping), origin, false).is_empty());
        let right = Position { x: 50.0, y: 30.0 };
        assert_eq!(
            gesture.input(Some(Wheel::Ping), right, false).last(),
            Some(&GestureEvent::Open(Wheel::Ping, origin, 2))
        );
        let up = Position { x: 20.0, y: 0.0 };
        assert_eq!(
            gesture.input(None, up, false),
            [GestureEvent::Finish(Some((Wheel::Ping, 0, origin)))]
        );
    }
}
