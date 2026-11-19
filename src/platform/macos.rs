//! macOS desktop overlays and listen-only Quartz gesture input.
//! macOS 桌面悬浮层和只读 Quartz 手势输入。

use std::cell::{Cell, RefCell};
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{AnimationOverlay, AudioCue, start_audio, wheel::WheelView};
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
use gpui_kit::App;

use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSPanel, NSScreen,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::gesture::{Gesture, GestureEvent, Position, WHEEL_SIZE, Wheel};
use crate::model::Location;

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
                for update in gesture.input(wheel, position, cancel, 1.0) {
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
}

impl Drop for Effect {
    fn drop(&mut self) {
        self.panel.orderOut(None);
    }
}

pub(crate) struct Overlay {
    mtm: MainThreadMarker,
    wheel: Retained<NSPanel>,
    wheel_art: Retained<CALayer>,
    wheel_view: WheelView,
    pub(super) animations: AnimationOverlay,
    pub(super) audio: std::sync::mpsc::SyncSender<AudioCue>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        let mtm = MainThreadMarker::new().expect("GUI must use the main thread");
        let (wheel, wheel_art) = panel(f64::from(WHEEL_SIZE), mtm);
        let scale = NSScreen::screens(mtm)
            .firstObject()
            .map_or(1.0, |screen| screen.backingScaleFactor());
        let wheel_view = WheelView::new((f64::from(WHEEL_SIZE) * scale).round() as u32);
        Self {
            mtm,
            wheel,
            wheel_art,
            wheel_view,
            animations: AnimationOverlay::default(),
            audio: start_audio(),
        }
    }

    pub(crate) fn open(
        &mut self,
        kind: Wheel,
        position: Position,
        selected: Option<usize>,
        tick_volume: Option<f32>,
        cx: &App,
    ) {
        let Some((point, _)) = screen_for(position, self.mtm) else {
            return;
        };
        let panel = self.wheel.clone();
        let layer = self.wheel_art.clone();
        let half = f64::from(WHEEL_SIZE) / 2.0;
        panel.setFrameOrigin(NSPoint::new(point.x - half, point.y - half));
        let scale = panel.backingScaleFactor();
        let size = (f64::from(WHEEL_SIZE) * scale).round() as u32;
        let present = Rc::new(move |frame: Option<&tiny_skia::Pixmap>| {
            if let Some(frame) = frame {
                update_layers(|| {
                    layer.setContentsScale(scale);
                    set_pixels(
                        &layer,
                        frame.width(),
                        frame.height(),
                        frame.data().to_vec(),
                        1,
                    );
                });
                if !panel.isVisible() {
                    panel.orderFrontRegardless();
                }
            } else {
                panel.orderOut(None);
                update_layers(|| content(&layer, None));
            }
        });
        let animate = animation_enabled();
        self.wheel_view
            .open(kind, size, selected, animate, present, cx);
        if let Some(volume) = tick_volume.filter(|_| selected.is_some()) {
            let _ = self.audio.try_send(AudioCue::Tick(volume));
        }
    }

    pub(crate) fn select(&mut self, selected: Option<usize>, tick_volume: Option<f32>, cx: &App) {
        if self.wheel_view.select(selected, cx)
            && let Some(volume) = tick_volume
        {
            let _ = self.audio.try_send(AudioCue::Tick(volume));
        }
    }

    pub(crate) fn close(&mut self, cx: &App) {
        self.wheel_view.close(cx);
    }

    pub(crate) fn hide(&mut self) {
        self.wheel_view.hide();
    }
}

pub(super) fn animation_presenter(
    size: u32,
    local: Option<Position>,
    location: Option<Location>,
) -> Option<(Option<u32>, impl FnMut(image::RgbaImage))> {
    let mtm = MainThreadMarker::new()?;
    let (point, screen) = local.and_then(|point| screen_for(point, mtm)).or_else(|| {
        let screen = NSScreen::screens(mtm).firstObject()?.frame();
        let location = location.unwrap_or(Location { x: 0.5, y: 0.5 });
        Some((
            NSPoint::new(
                screen.origin.x + location.x * screen.size.width,
                screen.origin.y + (1.0 - location.y) * screen.size.height,
            ),
            screen,
        ))
    })?;
    let size = f64::from(size);
    let mut effect = None;
    Some((None, move |frame: image::RgbaImage| {
        let window = effect.get_or_insert_with(|| {
            let (panel, art) = panel(size, mtm);
            art.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspect });
            Effect { panel, art }
        });
        update_layers(|| {
            set_pixels(
                &window.art,
                frame.width(),
                frame.height(),
                frame.into_raw(),
                3,
            )
        });
        if !window.panel.isVisible() {
            place(&window.panel, point, size, screen);
        }
    }))
}

fn set_pixels(layer: &CALayer, width: u32, height: u32, bytes: Vec<u8>, alpha: u32) {
    let provider = CGDataProvider::from_buffer(Arc::new(bytes));
    let image = core_graphics::image::CGImage::new(
        width as usize,
        height as usize,
        8,
        32,
        width as usize * 4,
        &CGColorSpace::create_device_rgb(),
        alpha,
        &provider,
        true,
        0,
    );
    unsafe { layer.setContents(Some(&*image.as_ptr().cast::<AnyObject>())) };
}

pub(super) fn animation_enabled() -> bool {
    !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}
