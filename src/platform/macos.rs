//! macOS desktop overlays and Quartz gesture input.
//! macOS 桌面悬浮层和Quartz 手势输入。

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{AnimationOverlay, AudioCue, WheelContents, start_audio, wheel::WheelView};
use async_channel::Sender;
use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortInvalidate, CFMachPortRef};
use core_foundation::runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
use core_graphics::color_space::CGColorSpace;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, EventField,
};
use foreign_types::ForeignType;
use gpui_kit::App;

use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSPanel, NSScreen,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::gesture::{
    ALT, CTRL, GestureEvent, InputEvent, MouseDelivery, Position, SCROLL_STEP, SHIFT, SUPER,
    Triggers, WHEEL_SIZE, WheelInput,
};
use crate::model::Location;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestListenEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
    fn CGEventTapCreate(
        location: CGEventTapLocation,
        placement: CGEventTapPlacement,
        options: CGEventTapOptions,
        mask: u64,
        callback: unsafe extern "C" fn(
            CGEventTapProxy,
            CGEventType,
            core_graphics::sys::CGEventRef,
            *mut c_void,
        ) -> core_graphics::sys::CGEventRef,
        context: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enabled: bool);
}

pub(crate) fn request_input_access(captures_mouse: bool) {
    if captures_mouse {
        unsafe { CGRequestPostEventAccess() };
    } else {
        unsafe { CGRequestListenEventAccess() };
    }
}

pub(crate) fn capture_key_code() -> Option<u16> {
    let app = objc2_app_kit::NSApplication::sharedApplication(MainThreadMarker::new()?);
    let event = app.currentEvent()?;
    (event.r#type() == objc2_app_kit::NSEventType::KeyDown).then(|| event.keyCode())
}

struct TapState {
    gesture: WheelInput,
    triggers: Option<Triggers>,
    paused: Arc<AtomicBool>,
    events: Sender<GestureEvent>,
    port: CFMachPortRef,
    function_held: bool,
    held_down: Option<CGEvent>,
}

impl TapState {
    fn dispatch(&mut self, kind: CGEventType, event: &CGEvent) -> MouseDelivery {
        use CGEventFlags as Flags;
        use CGEventType as Kind;
        let Some(config) = self.triggers.as_ref() else {
            return MouseDelivery::Pass;
        };
        let flags = event.get_flags();
        let mut modifiers = 0;
        for (flag, bit) in [
            (Flags::CGEventFlagAlternate, ALT),
            (Flags::CGEventFlagControl, CTRL),
            (Flags::CGEventFlagShift, SHIFT),
            (Flags::CGEventFlagCommand, SUPER),
        ] {
            if flags.intersects(flag) {
                modifiers |= bit;
            }
        }
        // Quartz also sets SecondaryFn on ordinary function-key events. Track the Fn key itself.
        // Quartz 也会为普通功能键事件设置 SecondaryFn，这里只跟踪 Fn 键本身。
        if matches!(kind, Kind::FlagsChanged)
            && event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) == 63
        {
            self.function_held = flags.intersects(Flags::CGEventFlagSecondaryFn);
        }
        let input = match kind {
            Kind::KeyDown | Kind::KeyUp => InputEvent::Key(
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16,
                matches!(kind, Kind::KeyDown),
            ),
            Kind::FlagsChanged => InputEvent::Modifier,
            Kind::LeftMouseDown | Kind::LeftMouseUp => {
                InputEvent::Left(matches!(kind, Kind::LeftMouseDown))
            }
            Kind::OtherMouseDown | Kind::OtherMouseUp => {
                match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                    3 => InputEvent::Side(1, matches!(kind, Kind::OtherMouseDown)),
                    4 => InputEvent::Side(2, matches!(kind, Kind::OtherMouseDown)),
                    n @ 0..64 => {
                        InputEvent::OtherButton(n as u8, matches!(kind, Kind::OtherMouseDown))
                    }
                    _ => InputEvent::Interrupt,
                }
            }
            Kind::RightMouseDown | Kind::RightMouseUp => {
                InputEvent::OtherButton(1, matches!(kind, Kind::RightMouseDown))
            }
            Kind::ScrollWheel => InputEvent::Scroll(scroll_delta(event)),
            Kind::MouseMoved
            | Kind::LeftMouseDragged
            | Kind::OtherMouseDragged
            | Kind::RightMouseDragged => InputEvent::Move,
            _ => InputEvent::Interrupt,
        };
        let point = event.location();
        let position = Position {
            x: point.x,
            y: point.y,
        };
        let (updates, delivery) = self.gesture.event(
            config,
            modifiers,
            input,
            position,
            self.paused.load(Ordering::Relaxed) || self.function_held,
            || 1.0,
        );
        for update in updates {
            let _ = self.events.try_send(update);
        }
        delivery
    }
}

fn scroll_delta(event: &CGEvent) -> i32 {
    // Quartz SDK fields missing from core-graphics 0.24's EventField constants.
    // Quartz SDK 中的字段，core-graphics 0.24 未在 EventField 中提供。
    const IS_CONTINUOUS: u32 = 88;
    const MOMENTUM_PHASE: u32 = 123;
    if event.get_integer_value_field(MOMENTUM_PHASE) != 0 {
        return 0;
    }
    if event.get_integer_value_field(IS_CONTINUOUS) == 0 {
        let lines = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
        return -(lines.signum() as i32) * SCROLL_STEP;
    }
    // Forty points of trackpad motion are equivalent to one mechanical notch.
    // 触控板滚动四十点相当于机械滚轮转动一格。
    let points = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
    points.clamp(-480, 480) as i32 * -(SCROLL_STEP / 40)
}

// Quartz needs a null return to drop an event. The Rust convenience callback maps None to pass-through.
// Quartz 要求返回空指针才能拦截事件，Rust 便捷接口的 None 会继续传递事件。
unsafe extern "C" fn tap_callback(
    proxy: CGEventTapProxy,
    kind: CGEventType,
    raw: core_graphics::sys::CGEventRef,
    context: *mut c_void,
) -> core_graphics::sys::CGEventRef {
    let state = unsafe { &mut *context.cast::<TapState>() };
    if matches!(
        kind,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        state.gesture.reset_after_gap();
        state.held_down = None;
        state.function_held = false;
        let _ = state.events.try_send(GestureEvent::Finish(None));
        unsafe { CGEventTapEnable(state.port, true) };
        return raw;
    }
    if raw.is_null() {
        return raw;
    }
    let event = ManuallyDrop::new(unsafe { CGEvent::from_ptr(raw) });
    match state.dispatch(kind, &event) {
        MouseDelivery::Pass => raw,
        MouseDelivery::HoldDown => {
            state.held_down = Some(CGEvent::clone(&event));
            ptr::null_mut()
        }
        MouseDelivery::WheelMove => {
            // Move the visible cursor without dispatching a drag to the window underneath.
            // 移动可见光标，同时避免向下方窗口派发拖动事件。
            let _ = core_graphics::display::CGDisplay::warp_mouse_cursor_position(event.location());
            ptr::null_mut()
        }
        MouseDelivery::Swallow => {
            if matches!(kind, CGEventType::LeftMouseUp | CGEventType::OtherMouseUp) {
                state.held_down = None;
            }
            ptr::null_mut()
        }
        MouseDelivery::ReplayClick => {
            // Posting from this tap places the original down before this up, without re-entering the tap.
            // 从当前 tap 投递原始按下事件会排在这次松开之前，并跳过当前 tap。
            if let Some(down) = state.held_down.take() {
                down.post_from_tap(proxy);
            }
            raw
        }
    }
}

pub(crate) struct InputMonitor {
    tap: CFMachPort,
    source: CFRunLoopSource,
    _state: Box<TapState>,
}

impl InputMonitor {
    pub(crate) fn start(
        events: Sender<GestureEvent>,
        triggers: Option<Triggers>,
        paused: Arc<AtomicBool>,
    ) -> Option<Self> {
        use CGEventType as Kind;
        let options = if triggers.as_ref().is_some_and(Triggers::captures_mouse) {
            CGEventTapOptions::Default
        } else {
            CGEventTapOptions::ListenOnly
        };
        let mask = [
            Kind::FlagsChanged,
            Kind::KeyDown,
            Kind::KeyUp,
            Kind::MouseMoved,
            Kind::LeftMouseDragged,
            Kind::RightMouseDragged,
            Kind::OtherMouseDragged,
            Kind::LeftMouseDown,
            Kind::LeftMouseUp,
            Kind::RightMouseDown,
            Kind::RightMouseUp,
            Kind::OtherMouseDown,
            Kind::OtherMouseUp,
            Kind::ScrollWheel,
        ]
        .into_iter()
        .fold(0, |mask, kind| mask | (1u64 << kind as u32));
        let mut state = Box::new(TapState {
            gesture: WheelInput::default(),
            events,
            triggers,
            paused,
            port: ptr::null_mut(),
            function_held: false,
            held_down: None,
        });
        let port = unsafe {
            CGEventTapCreate(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                options,
                mask,
                tap_callback,
                ptr::from_mut(&mut *state).cast(),
            )
        };
        if port.is_null() {
            return None;
        }
        state.port = port;
        let tap = unsafe { CFMachPort::wrap_under_create_rule(port) };
        let source = tap.create_runloop_source(0).ok()?;
        CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
        unsafe { CGEventTapEnable(port, true) };
        Some(Self {
            tap,
            source,
            _state: state,
        })
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        let port = self.tap.as_concrete_TypeRef();
        unsafe { CGEventTapEnable(port, false) };
        CFRunLoop::get_current().remove_source(&self.source, unsafe { kCFRunLoopCommonModes });
        unsafe { CFMachPortInvalidate(port) };
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
    pub(super) wheel_view: WheelView,
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
        contents: WheelContents,
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
        if self.wheel_view.open(contents, size, selected, present, cx)
            && let Some(volume) = tick_volume
        {
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
