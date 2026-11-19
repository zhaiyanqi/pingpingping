//! Shared native wheel artwork and interruptible animation.
//! 两个平台共用的原生轮盘绘制与可打断动画。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{App, Task};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, IntSize, Paint, Path, PathBuilder, Pixmap,
    PixmapPaint, Point, RadialGradient, SpreadMode, Stroke, Transform,
};

use super::WheelContents;
use crate::assets::{EMOTE_HUB, EMOTES, PINGS};
use crate::emotes::{EmoteId, EmoteStore};
use crate::gesture::{INNER_RADIUS, WHEEL_SIZE, Wheel};
use crate::model::{EmotePage, WheelEmote};

const CENTER: f32 = WHEEL_SIZE as f32 / 2.0;
const INNER: f32 = INNER_RADIUS as f32;
const OUTER: f32 = 188.0;
const EXTENT: f32 = 288.0;
const WHEEL_TIME: u64 = 120;
const WEDGE_TIME: u64 = 80;
const ICON_TIME: u64 = 100;

type Present = Rc<dyn Fn(Option<&Pixmap>)>;

#[derive(Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    ease_out: bool,
}

impl Tween {
    fn fixed(value: f32, now: Instant) -> Self {
        Self {
            from: value,
            to: value,
            start: now,
            duration: Duration::ZERO,
            ease_out: false,
        }
    }

    fn sample(self, now: Instant) -> f32 {
        if !self.running(now) {
            return self.to;
        }
        let time =
            now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32();
        if time == 0.0 {
            return self.from;
        }
        let progress = css_ease(time, self.ease_out);
        self.from + (self.to - self.from) * progress
    }

    fn running(self, now: Instant) -> bool {
        self.from != self.to && now.saturating_duration_since(self.start) < self.duration
    }

    fn animate(&mut self, to: f32, millis: u64, ease_out: bool, now: Instant) {
        *self = Self {
            from: self.sample(now),
            to,
            start: now,
            duration: Duration::from_millis(millis),
            ease_out,
        };
    }
}

fn css_ease(time: f32, ease_out: bool) -> f32 {
    let (x1, y1, x2) = if ease_out {
        (0.0, 0.0, 0.58)
    } else {
        (0.25, 0.1, 0.25)
    };
    let (mut low, mut high) = (0.0_f32, 1.0);
    for _ in 0..12 {
        let t = (low + high) * 0.5;
        let x = 3.0 * (1.0 - t).powi(2) * t * x1 + 3.0 * (1.0 - t) * t * t * x2 + t * t * t;
        if x < time {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) * 0.5;
    3.0 * (1.0 - t).powi(2) * t * y1 + t * t * (3.0 - 2.0 * t)
}

#[derive(Clone)]
struct Motion {
    visibility: Tween,
    wedges: [Tween; 8],
    icons: [Tween; 8],
    selection: Option<usize>,
}

impl Motion {
    fn new(now: Instant) -> Self {
        let zero = Tween::fixed(0.0, now);
        Self {
            visibility: zero,
            wedges: [zero; 8],
            icons: [zero; 8],
            selection: None,
        }
    }

    fn select(&mut self, selected: Option<usize>, now: Instant) {
        if selected == self.selection {
            return;
        }
        for index in [self.selection, selected].into_iter().flatten() {
            let target = if Some(index) == selected { 1.0 } else { 0.0 };
            self.wedges[index].animate(target, WEDGE_TIME, false, now);
            self.icons[index].animate(target, ICON_TIME, false, now);
        }
        self.selection = selected;
    }

    fn running(&self, now: Instant) -> bool {
        self.visibility.running(now)
            || self
                .wedges
                .iter()
                .chain(&self.icons)
                .any(|value| value.running(now))
    }
}

struct Active {
    contents: WheelContents,
    page: usize,
    motion: Motion,
    present: Present,
}

impl Active {
    fn turn_page(&mut self, steps: i32) -> bool {
        let WheelContents::Emote(pages) = &self.contents else {
            return false;
        };
        let previous = self.page;
        self.page = (previous as i64 + i64::from(steps)).rem_euclid(pages.len() as i64) as usize;
        self.page != previous
    }

    fn frame(&self) -> WheelFrame {
        match &self.contents {
            WheelContents::Ping => WheelFrame::ping(),
            WheelContents::Emote(pages) => WheelFrame {
                kind: Wheel::Emote,
                slots: pages[self.page],
                page: self.page,
                page_count: pages.len(),
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct WheelFrame {
    kind: Wheel,
    slots: EmotePage,
    page: usize,
    page_count: usize,
}

impl WheelFrame {
    fn ping() -> Self {
        Self {
            kind: Wheel::Ping,
            slots: [None; 8],
            page: 0,
            page_count: 1,
        }
    }

    fn occupied(self, selected: Option<usize>) -> Option<usize> {
        selected.filter(|&slot| self.kind == Wheel::Ping || self.slots[slot].is_some())
    }
}

pub(super) struct WheelView {
    art: Rc<WheelArt>,
    posters: Rc<HashMap<EmoteId, Pixmap>>,
    canvas: Option<Rc<RefCell<Canvas>>>,
    active: Option<Active>,
    task: Option<Task<()>>,
}

impl WheelView {
    pub(super) fn new(size: u32) -> Self {
        Self {
            art: Rc::new(WheelArt::new(size)),
            posters: Rc::default(),
            canvas: None,
            active: None,
            task: None,
        }
    }

    pub(super) fn open(
        &mut self,
        contents: WheelContents,
        size: u32,
        selected: Option<usize>,
        present: Present,
        cx: &App,
    ) -> bool {
        self.task.take();
        if self.art.size != size {
            self.art = Rc::new(WheelArt::new(size));
            self.canvas = None;
        }
        let now = Instant::now();
        let mut motion = Motion::new(now);
        if let Some(previous) = self
            .active
            .as_ref()
            .filter(|previous| previous.contents.kind() == contents.kind())
        {
            motion = previous.motion.clone();
        }
        motion.visibility.animate(1.0, WHEEL_TIME, true, now);
        let mut active = Active {
            contents,
            page: 0,
            motion,
            present,
        };
        let selected = active.frame().occupied(selected);
        active.motion.select(selected, now);
        self.active = Some(active);
        self.draw(cx);
        selected.is_some()
    }

    pub(super) fn select(&mut self, selected: Option<usize>, cx: &App) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let selected = active.frame().occupied(selected);
        if active.motion.visibility.to == 0.0 || active.motion.selection == selected {
            return false;
        }
        active.motion.select(selected, Instant::now());
        self.draw(cx);
        selected.is_some()
    }

    pub(super) fn turn_page(&mut self, steps: i32, selected: Option<usize>, cx: &App) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.motion.visibility.to == 0.0 || !active.turn_page(steps) {
            return self.select(selected, cx);
        }
        let selected = active.frame().occupied(selected);
        active.motion.select(selected, Instant::now());
        self.draw(cx);
        true
    }

    pub(super) fn emote(&self, slot: usize) -> Option<WheelEmote> {
        let active = self.active.as_ref()?;
        if active.motion.visibility.to == 0.0 {
            return None;
        }
        active.frame().slots[slot]
    }

    pub(super) fn set_posters(&mut self, posters: HashMap<EmoteId, Pixmap>, cx: &App) {
        self.posters = Rc::new(posters);
        self.canvas = None;
        self.draw(cx);
    }

    pub(super) fn close(&mut self, cx: &App) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.motion.visibility.to == 0.0 {
            return;
        }
        let now = Instant::now();
        active.motion.select(None, now);
        active.motion.visibility.animate(0.0, WHEEL_TIME, true, now);
        self.draw(cx);
        self.canvas = None;
    }

    pub(super) fn hide(&mut self) {
        self.task.take();
        self.canvas = None;
        if let Some(active) = self.active.take() {
            (active.present)(None);
        }
    }

    fn draw(&mut self, cx: &App) {
        self.task.take();
        let Some(active) = &self.active else { return };
        let art = self.art.clone();
        let wheel = active.frame();
        let posters = self.posters.clone();
        let motion = active.motion.clone();
        let present = active.present.clone();
        let now = Instant::now();
        let canvas = self
            .canvas
            .get_or_insert_with(|| Rc::new(RefCell::new(Canvas::new(art.size))))
            .clone();
        let first = if motion.visibility.to == 1.0 && motion.visibility.from == 0.0 {
            now.max(motion.visibility.start + Duration::from_millis(8))
        } else {
            now
        };
        canvas
            .borrow_mut()
            .draw(&art, wheel, &posters, &motion, first, &present);
        if motion.running(now) {
            self.task = Some(cx.spawn(async move |cx| {
                let mut last_frame = now;
                loop {
                    cx.background_executor()
                        .timer(
                            Duration::from_millis(16)
                                .saturating_sub(last_frame.elapsed())
                                .max(Duration::from_millis(1)),
                        )
                        .await;
                    let now = Instant::now();
                    last_frame = now;
                    canvas
                        .borrow_mut()
                        .draw(&art, wheel, &posters, &motion, now, &present);
                    if !motion.running(now) {
                        break;
                    }
                }
            }));
        }
    }
}

struct Canvas {
    scene: Pixmap,
    frame: Pixmap,
    cached: Option<(WheelFrame, Option<usize>)>,
}

impl Canvas {
    fn new(size: u32) -> Self {
        Self {
            scene: square(size),
            frame: square(size),
            cached: None,
        }
    }

    fn draw(
        &mut self,
        art: &WheelArt,
        wheel: WheelFrame,
        posters: &HashMap<EmoteId, Pixmap>,
        motion: &Motion,
        now: Instant,
        present: &Present,
    ) {
        let opacity = motion.visibility.sample(now);
        if opacity <= 0.0 {
            present(None);
            return;
        }
        // Reuse the composed scene while only the whole wheel scales and fades.
        // 仅轮盘整体缩放淡入淡出时，复用已合成的图层。
        let settled = !motion
            .wedges
            .iter()
            .chain(&motion.icons)
            .any(|tween| tween.running(now));
        let key = (wheel, motion.selection);
        if !settled || self.cached != Some(key) {
            art.draw(&mut self.scene, wheel, posters, motion, now);
            self.cached = settled.then_some(key);
        }
        if opacity >= 1.0 {
            present(Some(&self.scene));
            return;
        }
        self.frame.fill(Color::TRANSPARENT);
        let scale = 0.85 + 0.15 * opacity;
        let offset = art.size as f32 * (1.0 - scale) * 0.5;
        self.frame.draw_pixmap(
            0,
            0,
            self.scene.as_ref(),
            &PixmapPaint {
                opacity,
                quality: FilterQuality::Bilinear,
                ..Default::default()
            },
            Transform::from_scale(scale, scale).post_translate(offset, offset),
            None,
        );
        present(Some(&self.frame));
    }
}

struct WheelArt {
    size: u32,
    background: Pixmap,
    foreground: [Pixmap; 2],
    wedges: [Path; 8],
    ping_icons: [(Pixmap, Pixmap); 8],
    emote_icons: [Pixmap; EMOTES.len()],
    font: Option<Vec<u8>>,
}

fn square(size: u32) -> Pixmap {
    Pixmap::new(size, size).expect("valid wheel dimensions")
}

fn circle(radius: f32) -> Path {
    PathBuilder::from_circle(CENTER, CENTER, radius).unwrap()
}

fn polar(radius: f32, angle: f32) -> Point {
    Point::from_xy(CENTER + radius * angle.cos(), CENTER + radius * angle.sin())
}

fn angle(index: usize) -> f32 {
    -std::f32::consts::FRAC_PI_2 + index as f32 * std::f32::consts::FRAC_PI_4
}

fn sector(inner: f32, outer: f32, index: usize) -> Path {
    let half = std::f32::consts::PI / 8.0;
    let start = angle(index) - half;
    let mut path = PathBuilder::new();
    let point = polar(inner, start);
    path.move_to(point.x, point.y);
    for step in 0..=16 {
        let point = polar(outer, start + half * step as f32 / 8.0);
        path.line_to(point.x, point.y);
    }
    for step in (0..=16).rev() {
        let point = polar(inner, start + half * step as f32 / 8.0);
        path.line_to(point.x, point.y);
    }
    path.close();
    path.finish().unwrap()
}

fn paint(color: [u8; 4]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    paint
}

fn gradient(radius: f32, vertical: f32, stops: &[(f32, [u8; 4])]) -> Paint<'static> {
    let center = Point::from_xy(CENTER, CENTER + vertical);
    let stops = stops
        .iter()
        .map(|(offset, [red, green, blue, alpha])| {
            GradientStop::new(*offset, Color::from_rgba8(*red, *green, *blue, *alpha))
        })
        .collect();
    Paint {
        shader: RadialGradient::new(
            center,
            center,
            radius,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        )
        .unwrap(),
        ..Default::default()
    }
}

fn fill(target: &mut Pixmap, path: &Path, paint: &Paint, transform: Transform) {
    target.fill_path(path, paint, FillRule::Winding, transform, None);
}

fn stroke(target: &mut Pixmap, path: &Path, paint: &Paint, width: f32, transform: Transform) {
    target.stroke_path(
        path,
        paint,
        &Stroke {
            width,
            ..Default::default()
        },
        transform,
        None,
    );
}

fn asset(bytes: &[u8], round: bool, tinted: bool) -> Pixmap {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .unwrap()
        .into_rgba8();
    pixmap(image, round, tinted)
}

fn pixmap(image: image::RgbaImage, round: bool, tinted: bool) -> Pixmap {
    let (width, height) = image.dimensions();
    let mut bytes = image.into_raw();
    for (index, [red, green, blue, alpha]) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let mut opacity = f32::from(*alpha) / 255.0;
        if round {
            let x = (index as u32 % width) as f32 + 0.5 - width as f32 / 2.0;
            let y = (index as u32 / width) as f32 + 0.5 - height as f32 / 2.0;
            opacity *= (width.min(height) as f32 * 0.5 + 0.5 - x.hypot(y)).clamp(0.0, 1.0);
        }
        if tinted {
            let gray =
                f32::from(*red) * 0.2126 + f32::from(*green) * 0.7152 + f32::from(*blue) * 0.0722;
            *red = (gray * 1.48).min(255.0) as u8;
            *green = (gray * 1.25).min(255.0) as u8;
            *blue = (gray * 0.87).min(255.0) as u8;
        }
        for channel in [red, green, blue] {
            *channel = (f32::from(*channel) * opacity).round() as u8;
        }
        *alpha = (255.0 * opacity).round() as u8;
    }
    Pixmap::from_vec(bytes, IntSize::from_wh(width, height).unwrap()).unwrap()
}

pub(crate) fn load_wheel_posters(
    store: &EmoteStore,
    pages: &[EmotePage],
) -> HashMap<EmoteId, Pixmap> {
    let ids: HashSet<_> = pages
        .iter()
        .flatten()
        .filter_map(|slot| match *slot {
            Some(WheelEmote::Custom(id)) => Some(id),
            _ => None,
        })
        .collect();
    ids.into_iter()
        .filter_map(|id| {
            let image = image::open(store.poster(id)).ok()?.into_rgba8();
            let edge = image.width().max(image.height());
            let mut square = image::RgbaImage::new(edge, edge);
            image::imageops::overlay(
                &mut square,
                &image,
                i64::from((edge - image.width()) / 2),
                i64::from((edge - image.height()) / 2),
            );
            Some((id, pixmap(square, false, false)))
        })
        .collect()
}

fn sprite(target: &mut Pixmap, sprite: &Pixmap, center: Point, edge: f32, dpi: f32, opacity: f32) {
    if opacity <= 0.0 {
        return;
    }
    let transform = Transform::from_scale(
        edge * dpi / sprite.width() as f32,
        edge * dpi / sprite.height() as f32,
    )
    .post_translate((center.x - edge / 2.0) * dpi, (center.y - edge / 2.0) * dpi);
    target.draw_pixmap(
        0,
        0,
        sprite.as_ref(),
        &PixmapPaint {
            opacity,
            quality: FilterQuality::Bilinear,
            ..Default::default()
        },
        transform,
        None,
    );
}

struct Glyphs(PathBuilder);

impl ttf_parser::OutlineBuilder for Glyphs {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

fn text(
    target: &mut Pixmap,
    face: &ttf_parser::Face,
    text: &str,
    center: Point,
    points: f32,
    color: [u8; 4],
    dpi: f32,
) {
    let scale = points / f32::from(face.units_per_em());
    let glyphs: Vec<_> = text
        .chars()
        .filter_map(|character| face.glyph_index(character))
        .collect();
    let advance =
        |glyph| f32::from(face.glyph_hor_advance(glyph).unwrap_or_default()) * scale + 0.7;
    let mut left = center.x - glyphs.iter().map(|glyph| advance(*glyph)).sum::<f32>() / 2.0;
    for glyph in glyphs {
        let mut builder = Glyphs(PathBuilder::new());
        if face.outline_glyph(glyph, &mut builder).is_some()
            && let Some(path) = builder.0.finish()
        {
            let transform = Transform::from_scale(scale * dpi, -scale * dpi)
                .post_translate(left * dpi, center.y * dpi);
            fill(target, &path, &paint(color), transform);
        }
        left += advance(glyph);
    }
}

impl WheelArt {
    fn new(size: u32) -> Self {
        let dpi = size as f32 / WHEEL_SIZE as f32;
        let transform = Transform::from_scale(dpi, dpi);
        let mut background = square(size);
        for index in [0, 2, 4, 6] {
            fill(
                &mut background,
                &sector(OUTER, EXTENT, index),
                &gradient(
                    EXTENT,
                    0.0,
                    &[
                        (OUTER / EXTENT, [92, 96, 102, 140]),
                        (1.0, [92, 96, 102, 0]),
                    ],
                ),
                transform,
            );
        }
        fill(
            &mut background,
            &circle(OUTER),
            &gradient(
                OUTER,
                0.0,
                &[(INNER / OUTER, [42, 45, 49, 235]), (1.0, [74, 78, 84, 235])],
            ),
            transform,
        );
        let font = if cfg!(target_os = "windows") {
            std::env::var_os("WINDIR")
                .map(std::path::PathBuf::from)
                .and_then(|directory| std::fs::read(directory.join("Fonts/seguisb.ttf")).ok())
        } else {
            std::fs::read("/System/Library/Fonts/HelveticaNeue.ttc").ok()
        };
        let face = font
            .as_deref()
            .and_then(|font| ttf_parser::Face::parse(font, 0).ok());
        let foreground = std::array::from_fn(|kind| {
            let mut image = square(size);
            for index in 0..8 {
                let direction = angle(index) - std::f32::consts::PI / 8.0;
                let start = polar(INNER, direction);
                let end = polar(OUTER, direction);
                let mut line = PathBuilder::new();
                line.move_to(start.x, start.y);
                line.line_to(end.x, end.y);
                stroke(
                    &mut image,
                    &line.finish().unwrap(),
                    &paint([160, 138, 92, 115]),
                    1.2,
                    transform,
                );
            }
            stroke(
                &mut image,
                &circle(INNER + 4.0),
                &paint([26, 26, 26, 153]),
                6.0,
                transform,
            );
            fill(
                &mut image,
                &circle(INNER),
                &gradient(
                    INNER,
                    -10.0,
                    &[(0.0, [44, 93, 96, 255]), (1.0, [17, 41, 44, 255])],
                ),
                transform,
            );
            stroke(
                &mut image,
                &circle(INNER),
                &paint([200, 170, 110, 255]),
                3.0,
                transform,
            );
            stroke(
                &mut image,
                &circle(INNER - 7.0),
                &paint([138, 116, 72, 153]),
                1.0,
                transform,
            );
            let hub = if kind == 0 {
                PINGS[8].image
            } else {
                EMOTES[EMOTE_HUB].image
            };
            sprite(
                &mut image,
                &asset(hub, kind == 1, false),
                Point::from_xy(CENTER, CENTER - 38.0),
                40.0,
                dpi,
                1.0,
            );
            if let Some(face) = &face {
                text(
                    &mut image,
                    face,
                    if kind == 0 { "PING" } else { "EMOTE" },
                    Point::from_xy(CENTER, CENTER + 6.0),
                    15.0,
                    [31, 169, 224, 255],
                    dpi,
                );
                text(
                    &mut image,
                    face,
                    "BACK",
                    Point::from_xy(CENTER + 6.0, CENTER + 50.0),
                    9.0,
                    [108, 122, 124, 255],
                    dpi,
                );
            }
            let mouse =
                tiny_skia::Rect::from_xywh(CENTER - 26.0, CENTER + 40.0, 11.0, 16.0).unwrap();
            let mut outline = PathBuilder::new();
            outline.push_oval(mouse);
            stroke(
                &mut image,
                &outline.finish().unwrap(),
                &paint([108, 122, 124, 140]),
                1.3,
                transform,
            );
            let button =
                tiny_skia::Rect::from_xywh(CENTER - 20.5, CENTER + 42.0, 4.0, 5.0).unwrap();
            fill(
                &mut image,
                &PathBuilder::from_rect(button),
                &paint([108, 122, 124, 140]),
                transform,
            );
            image
        });
        let wedges = std::array::from_fn(|index| {
            sector(INNER, if index % 2 == 0 { EXTENT } else { OUTER }, index)
        });
        let ping_icons = std::array::from_fn(|index| {
            let bytes = PINGS[index].image;
            (asset(bytes, false, true), asset(bytes, false, false))
        });
        Self {
            size,
            background,
            foreground,
            wedges,
            ping_icons,
            emote_icons: EMOTES.map(|clip| asset(clip.image, true, false)),
            font,
        }
    }

    fn draw(
        &self,
        target: &mut Pixmap,
        wheel: WheelFrame,
        posters: &HashMap<EmoteId, Pixmap>,
        motion: &Motion,
        now: Instant,
    ) {
        target.data_mut().copy_from_slice(self.background.data());
        let dpi = self.size as f32 / WHEEL_SIZE as f32;
        for (index, wedge) in self.wedges.iter().enumerate() {
            let hot = motion.wedges[index].sample(now);
            if hot > 0.0 {
                let alpha = (hot * 140.0).round() as u8;
                fill(
                    target,
                    wedge,
                    &gradient(
                        EXTENT,
                        0.0,
                        &[
                            (INNER / EXTENT, [138, 143, 150, alpha]),
                            (OUTER / EXTENT, [154, 160, 168, alpha]),
                            (1.0, [154, 160, 168, 0]),
                        ],
                    ),
                    Transform::from_scale(dpi, dpi),
                );
            }
        }
        target.draw_pixmap(
            0,
            0,
            self.foreground[usize::from(wheel.kind == Wheel::Emote)].as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        let face = self
            .font
            .as_deref()
            .and_then(|font| ttf_parser::Face::parse(font, 0).ok());
        if wheel.page_count > 1
            && let Some(face) = &face
        {
            text(
                target,
                face,
                &format!("{} / {}", wheel.page + 1, wheel.page_count),
                Point::from_xy(CENTER, CENTER + 28.0),
                12.0,
                [200, 170, 110, 255],
                dpi,
            );
        }
        for index in 0..wheel.slots.len() {
            let active = motion.icons[index].sample(now);
            let center = polar(137.0, angle(index));
            let edge = 48.0 * (1.0 + 0.18 * active);
            if wheel.kind == Wheel::Ping {
                let (normal, hot) = &self.ping_icons[index];
                if active < 1.0 {
                    sprite(target, normal, center, edge, dpi, 1.0);
                }
                sprite(target, hot, center, edge, dpi, active);
            } else if let Some(emote) = wheel.slots[index] {
                let icon = match emote {
                    WheelEmote::BuiltIn(id) => self.emote_icons.get(id),
                    WheelEmote::Custom(id) => posters.get(&id),
                };
                if let Some(icon) = icon {
                    sprite(target, icon, center, edge, dpi, 1.0);
                } else if let Some(face) = &face {
                    text(
                        target,
                        face,
                        "?",
                        Point::from_xy(center.x, center.y + 6.0),
                        20.0,
                        [200, 170, 110, 255],
                        dpi,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displayed_emote_page_wraps_and_empty_slots_do_not_resolve() {
        let mut first = [None; 8];
        first[0] = Some(WheelEmote::BuiltIn(8));
        let mut second = [None; 8];
        second[6] = Some(WheelEmote::BuiltIn(3));
        let mut wheel = WheelView::new(WHEEL_SIZE);
        let now = Instant::now();
        let mut motion = Motion::new(now);
        motion.visibility.animate(1.0, WHEEL_TIME, true, now);
        wheel.active = Some(Active {
            contents: WheelContents::Emote(vec![first, second]),
            page: 0,
            motion,
            present: Rc::new(|_| {}),
        });
        assert_eq!(wheel.emote(0), first[0]);
        let active = wheel.active.as_mut().unwrap();
        assert!(active.turn_page(-1));
        assert_eq!(active.frame().occupied(Some(0)), None);
        assert_eq!(active.frame().occupied(Some(6)), Some(6));
        assert_eq!(wheel.emote(6), second[6]);
        assert_eq!(wheel.emote(0), None);
        assert!(wheel.active.as_mut().unwrap().turn_page(1));
        assert_eq!(wheel.emote(0), first[0]);
    }

    #[test]
    fn opening_wheel_grows_and_brightens_in_presented_frames() {
        let size = WHEEL_SIZE;
        let art = WheelArt::new(size);
        let mut canvas = Canvas::new(size);
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.visibility.animate(1.0, WHEEL_TIME, true, start);
        let samples = Rc::new(RefCell::new(Vec::new()));
        let capture = samples.clone();
        let present: Present = Rc::new(move |frame| {
            let frame = frame.unwrap();
            let center = size / 2;
            let top = (0..center)
                .find(|&y| frame.pixel(center, y).unwrap().alpha() > 4)
                .unwrap();
            capture
                .borrow_mut()
                .push((top, frame.pixel(center, center).unwrap().alpha()));
        });
        for ms in [16, 60, 120] {
            canvas.draw(
                &art,
                WheelFrame::ping(),
                &HashMap::new(),
                &motion,
                start + Duration::from_millis(ms),
                &present,
            );
        }
        assert!(
            samples
                .borrow()
                .windows(2)
                .all(|pair| pair[0].0 > pair[1].0 && pair[0].1 < pair[1].1)
        );
    }

    #[test]
    fn wheel_motion_reverses_from_the_visible_state_and_uses_reference_timings() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.visibility.animate(1.0, WHEEL_TIME, true, start);
        motion.select(Some(0), start);
        let middle = start + Duration::from_millis(60);
        let visible = motion.visibility.sample(middle);
        motion.visibility.animate(0.0, WHEEL_TIME, true, middle);
        assert_eq!(motion.visibility.sample(middle), visible);
        assert_eq!(
            motion.wedges[0].sample(start + Duration::from_millis(80)),
            1.0
        );
        assert_eq!(
            motion.icons[0].sample(start + Duration::from_millis(100)),
            1.0
        );
        assert_eq!(
            motion
                .visibility
                .sample(middle + Duration::from_millis(120)),
            0.0
        );
    }

    #[test]
    fn default_art_has_a_transparent_corner_and_alternating_outer_wedges() {
        let art = WheelArt::new(WHEEL_SIZE);
        let now = Instant::now();
        let mut target = square(WHEEL_SIZE);
        art.draw(
            &mut target,
            WheelFrame::ping(),
            &HashMap::new(),
            &Motion::new(now),
            now,
        );
        assert_eq!(target.pixel(0, 0).unwrap().alpha(), 0);
        assert_eq!(target.pixel(300, 300).unwrap().alpha(), 255);
        assert!(target.pixel(300, 60).unwrap().alpha() > 0);
        assert_eq!(target.pixel(470, 130).unwrap().alpha(), 0);
    }
}
