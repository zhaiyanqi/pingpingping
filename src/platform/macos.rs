//! macOS desktop overlays and listen-only Quartz gesture input.
//! macOS 桌面悬浮层和只读 Quartz 手势输入。

use std::cell::{Cell, RefCell};
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{decode_animation, start_audio};
use async_channel::Sender;
use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
use core_graphics::color_space::CGColorSpace;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};
use foreign_types::ForeignType;
use gpui_kit::{App, Task};
use image::RgbaImage;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSImage, NSPanel, NSScreen,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::assets::{Clip, EMOTES, PINGS};
use crate::gesture::{Gesture, GestureEvent, Position, Wheel};
use crate::model::Location;

const WHEEL_SIZE: f64 = 222.0;
const EFFECT_SIZE: f64 = 138.0;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestListenEventAccess() -> bool;
    fn CGEventTapEnable(tap: CFMachPortRef, enabled: bool);
}

pub(crate) fn request_input_access() {
    unsafe { CGRequestListenEventAccess() };
}

pub(crate) struct InputMonitor {
    tap: CGEventTap<'static>,
    source: CFRunLoopSource,
}

impl InputMonitor {
    pub(crate) fn start(events: Sender<GestureEvent>, enabled: Arc<AtomicBool>) -> Option<Self> {
        use CGEventFlags as Flags;
        use CGEventType as Kind;
        let gesture = RefCell::new(Gesture::default());
        let suppressed = Cell::new(false);
        let port: Rc<Cell<CFMachPortRef>> = Rc::new(Cell::new(ptr::null_mut()));
        let callback_port = port.clone();
        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![
                Kind::FlagsChanged,
                Kind::KeyDown,
                Kind::MouseMoved,
                Kind::LeftMouseDragged,
                Kind::RightMouseDragged,
                Kind::OtherMouseDragged,
                Kind::LeftMouseDown,
                Kind::RightMouseDown,
                Kind::OtherMouseDown,
                Kind::ScrollWheel,
            ],
            move |_, kind, event| {
                if matches!(
                    kind,
                    Kind::TapDisabledByTimeout | Kind::TapDisabledByUserInput
                ) {
                    *gesture.borrow_mut() = Gesture::default();
                    suppressed.set(true);
                    let _ = events.try_send(GestureEvent::Finish(None));
                    let port = callback_port.get();
                    if !port.is_null() {
                        unsafe { CGEventTapEnable(port, true) };
                    }
                    return None;
                }
                let flags = event.get_flags();
                let held = flags & (Flags::CGEventFlagAlternate | Flags::CGEventFlagControl);
                let other = flags.intersects(
                    Flags::CGEventFlagCommand
                        | Flags::CGEventFlagShift
                        | Flags::CGEventFlagSecondaryFn,
                );
                let chord =
                    other || held == (Flags::CGEventFlagAlternate | Flags::CGEventFlagControl);
                let interrupt = chord
                    || !enabled.load(Ordering::Relaxed)
                    || matches!(
                        kind,
                        Kind::KeyDown
                            | Kind::ScrollWheel
                            | Kind::RightMouseDown
                            | Kind::OtherMouseDown
                    );
                if interrupt && !held.is_empty() {
                    suppressed.set(true);
                }
                let cancel = interrupt || suppressed.get();
                if held.is_empty() {
                    suppressed.set(false);
                }
                let wheel = match held {
                    Flags::CGEventFlagAlternate => Some(Wheel::Ping),
                    Flags::CGEventFlagControl => Some(Wheel::Emote),
                    _ => None,
                };
                if wheel.is_none() && !matches!(kind, Kind::FlagsChanged | Kind::KeyDown) {
                    return None;
                }
                let point = event.location();
                let position = Position {
                    x: point.x,
                    y: point.y,
                };
                let mut gesture = gesture.borrow_mut();
                if matches!(kind, Kind::LeftMouseDown)
                    && !cancel
                    && wheel == Some(Wheel::Ping)
                    && let Some(origin) = gesture.click(position)
                {
                    suppressed.set(true);
                    let _ = events.try_send(GestureEvent::Finish(Some((Wheel::Ping, 8, origin))));
                    return None;
                }
                for update in gesture.input(wheel, position, cancel) {
                    let _ = events.try_send(update);
                }
                None
            },
        )
        .ok()?;
        port.set(tap.mach_port.as_concrete_TypeRef());
        let source = tap.mach_port.create_runloop_source(0).ok()?;
        CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
        tap.enable();
        Some(Self { tap, source })
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        unsafe { CGEventTapEnable(self.tap.mach_port.as_concrete_TypeRef(), false) };
        CFRunLoop::get_current().remove_source(&self.source, unsafe { kCFRunLoopCommonModes });
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

// Quartz and AppKit use opposite vertical origins. Convert through the primary display.
// Quartz 与 AppKit 的纵轴原点相反，通过主显示器的坐标完成转换。
fn screen_for(position: Position, mtm: MainThreadMarker) -> Option<(NSPoint, NSRect)> {
    let screens = NSScreen::screens(mtm);
    let primary = screens.firstObject()?.frame();
    let point = NSPoint::new(
        position.x,
        primary.origin.y + primary.size.height - position.y,
    );
    let screen = screens
        .iter()
        .map(|screen| screen.frame())
        .find(|frame| {
            (frame.origin.x..frame.origin.x + frame.size.width).contains(&point.x)
                && (frame.origin.y..=frame.origin.y + frame.size.height).contains(&point.y)
        })
        .unwrap_or(primary);
    Some((point, screen))
}

pub(crate) fn normalized(position: Position) -> Location {
    let mtm = MainThreadMarker::new().expect("GUI must use the main thread");
    let Some((point, screen)) = screen_for(position, mtm) else {
        return Location { x: 0.5, y: 0.5 };
    };
    Location {
        x: ((point.x - screen.origin.x) / screen.size.width).clamp(0.0, 1.0),
        y: (1.0 - (point.y - screen.origin.y) / screen.size.height).clamp(0.0, 1.0),
    }
}

fn panel(size: f64, mtm: MainThreadMarker) -> (Retained<NSPanel>, Retained<CALayer>) {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect(0.0, 0.0, size, size),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(true);
    panel.setHidesOnDeactivate(false);
    panel.setLevel(NSFloatingWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    let layer = CALayer::layer();
    if let Some(view) = panel.contentView() {
        layer.setFrame(rect(0.0, 0.0, size, size));
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);
    }
    (panel, layer)
}

fn content(layer: &CALayer, image: Option<&CGImage>) {
    unsafe {
        layer.setContents(image.map(|image| &*(ptr::from_ref(image).cast::<AnyObject>())));
    }
}

fn picture(clip: &Clip) -> Option<Retained<CGImage>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(clip.image))?;
    unsafe { image.CGImageForProposedRect_context_hints(ptr::null_mut(), None, None) }
}

// Disable implicit transitions so the wheel and its selection appear immediately.
// 禁用隐式过渡，让轮盘及其选中项立即显示。
fn update_layers(update: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    update();
    CATransaction::commit();
}

fn place(panel: &NSPanel, point: NSPoint, size: f64, screen: NSRect) {
    let x = (point.x - size / 2.0).clamp(
        screen.origin.x,
        screen.origin.x + (screen.size.width - size).max(0.0),
    );
    let y = (point.y - size / 2.0).clamp(
        screen.origin.y,
        screen.origin.y + (screen.size.height - size).max(0.0),
    );
    panel.setFrameOrigin(NSPoint::new(x, y));
    panel.orderFrontRegardless();
}

struct Effect {
    panel: Retained<NSPanel>,
    art: Retained<CALayer>,
    task: Option<Task<()>>,
}

pub(crate) struct Overlay {
    mtm: MainThreadMarker,
    wheel: Retained<NSPanel>,
    slots: Vec<(Retained<CALayer>, Retained<CALayer>)>,
    current_wheel: Option<Wheel>,
    selected: Option<usize>,
    ping_images: [Option<Retained<CGImage>>; 9],
    emote_images: [Option<Retained<CGImage>>; 8],
    effects: [Effect; 3],
    next_effect: usize,
    audio: Sender<(Wheel, usize)>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        let mtm = MainThreadMarker::new().expect("GUI must use the main thread");
        let (wheel, root) = panel(WHEEL_SIZE, mtm);
        root.setCornerRadius(WHEEL_SIZE / 2.0);
        root.setBackgroundColor(Some(
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.045, 0.066, 0.10, 0.94).CGColor(),
        ));
        root.setBorderColor(Some(
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.38, 0.53, 0.62, 0.9).CGColor(),
        ));
        root.setBorderWidth(1.0);
        let slots = (0..8)
            .map(|index| {
                let angle = index as f64 * std::f64::consts::FRAC_PI_4;
                let center = WHEEL_SIZE / 2.0;
                let badge = CALayer::layer();
                badge.setFrame(rect(
                    center + angle.sin() * 77.0 - 25.0,
                    center + angle.cos() * 77.0 - 25.0,
                    50.0,
                    50.0,
                ));
                badge.setCornerRadius(25.0);
                let icon = CALayer::layer();
                icon.setFrame(rect(6.0, 6.0, 38.0, 38.0));
                badge.addSublayer(&icon);
                root.addSublayer(&badge);
                (badge, icon)
            })
            .collect();
        let effects = std::array::from_fn(|_| {
            let (panel, art) = panel(EFFECT_SIZE, mtm);
            Effect {
                panel,
                art,
                task: None,
            }
        });
        Self {
            mtm,
            wheel,
            slots,
            current_wheel: None,
            selected: None,
            ping_images: PINGS.map(|clip| picture(&clip)),
            emote_images: EMOTES.map(|clip| picture(&clip)),
            effects,
            next_effect: 0,
            audio: start_audio(),
        }
    }

    pub(crate) fn open(&mut self, kind: Wheel, position: Position, selected: usize) {
        let Some((point, screen)) = screen_for(position, self.mtm) else {
            return;
        };
        if self.current_wheel != Some(kind) {
            let images: &[_] = match kind {
                Wheel::Ping => &self.ping_images,
                Wheel::Emote => &self.emote_images,
            };
            update_layers(|| {
                for ((_, icon), image) in self.slots.iter().zip(images) {
                    content(icon, image.as_deref());
                }
            });
            self.current_wheel = Some(kind);
        }
        self.select(Some(selected));
        place(&self.wheel, point, WHEEL_SIZE, screen);
    }

    pub(crate) fn select(&mut self, selected: Option<usize>) {
        if self.selected == selected {
            return;
        }
        update_layers(|| {
            if let Some(index) = self.selected {
                self.slots[index].0.setBackgroundColor(None);
            }
            if let Some(index) = selected {
                self.slots[index].0.setBackgroundColor(Some(
                    &NSColor::colorWithSRGBRed_green_blue_alpha(0.15, 0.68, 0.73, 0.58).CGColor(),
                ));
            }
        });
        self.selected = selected;
    }

    pub(crate) fn hide(&self) {
        self.wheel.orderOut(None);
    }

    pub(crate) fn play(
        &mut self,
        kind: Wheel,
        index: usize,
        local: Option<Position>,
        location: Option<Location>,
        sound: bool,
        cx: &App,
    ) {
        let (clip, image) = match kind {
            Wheel::Ping => (&PINGS[index], self.ping_images[index].as_deref()),
            Wheel::Emote => (&EMOTES[index], self.emote_images[index].as_deref()),
        };
        let target = local
            .and_then(|point| screen_for(point, self.mtm))
            .or_else(|| {
                let screen = NSScreen::screens(self.mtm).firstObject()?.frame();
                let location = location.unwrap_or(Location { x: 0.5, y: 0.5 });
                Some((
                    NSPoint::new(
                        screen.origin.x + location.x * screen.size.width,
                        screen.origin.y + (1.0 - location.y) * screen.size.height,
                    ),
                    screen,
                ))
            });
        let Some((point, screen)) = target else {
            return;
        };
        let slot = self.next_effect;
        self.next_effect = (self.next_effect + 1) % self.effects.len();
        let effect = &mut self.effects[slot];
        effect.task.take();
        update_layers(|| {
            effect.art.setOpacity(1.0);
            content(&effect.art, image);
        });
        place(&effect.panel, point, EFFECT_SIZE, screen);
        if sound {
            let _ = self.audio.try_send((kind, index));
        }

        let reduce_motion = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
        let animation = clip.animation.filter(|_| !reduce_motion);
        let panel = effect.panel.clone();
        let art = effect.art.clone();
        effect.task = Some(cx.spawn(async move |cx| {
            let started = Instant::now();
            if let Some(bytes) = animation {
                let frames = cx
                    .background_executor()
                    .spawn(async move { decode_animation(bytes, 192) })
                    .await;
                for (frame, delay) in frames {
                    if started.elapsed() > Duration::from_millis(1850) {
                        break;
                    }
                    update_layers(|| set_frame(&art, frame));
                    cx.background_executor().timer(delay).await;
                }
            }
            let remainder = Duration::from_millis(1400).saturating_sub(started.elapsed());
            if !remainder.is_zero() {
                cx.background_executor().timer(remainder).await;
            }
            if !reduce_motion {
                CATransaction::begin();
                CATransaction::setAnimationDuration(0.16);
                art.setOpacity(0.0);
                CATransaction::commit();
                cx.background_executor()
                    .timer(Duration::from_millis(160))
                    .await;
            }
            panel.orderOut(None);
            update_layers(|| content(&art, None));
        }));
    }
}

fn set_frame(layer: &CALayer, frame: RgbaImage) {
    let (width, height) = frame.dimensions();
    let provider = CGDataProvider::from_buffer(Arc::new(frame.into_raw()));
    let image = core_graphics::image::CGImage::new(
        width as usize,
        height as usize,
        8,
        32,
        width as usize * 4,
        &CGColorSpace::create_device_rgb(),
        3,
        &provider,
        true,
        0,
    );
    unsafe { layer.setContents(Some(&*image.as_ptr().cast::<AnyObject>())) };
}
