//! macOS desktop overlays and Quartz gesture input.
//! macOS 桌面悬浮层和Quartz 手势输入。

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::playback::{
    FrameSize, FrameUpdate, SenderTag, TAG_BACKGROUND, TAG_FONT_SIZE, TAG_FOREGROUND, TAG_HEIGHT,
    TAG_PADDING, TAG_RADIUS, TAG_WIDTH, TEXT_FONT_SIZE, TEXT_MARGIN, TEXT_MAX_LINES, TEXT_PADDING,
    TEXT_RADIUS, TEXT_WIDTH, TextColors, TextFrame, text_anchor,
};
use super::{
    AnimationOverlay, AnimationPosition, AudioCue, WheelContents, start_audio, wheel::WheelView,
};
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
use gpui_kit::{App, Pixels, Point, Size, Task, Window};
use image::RgbaImage;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, runtime::AnyObject,
};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEventType, NSFloatingWindowLevel, NSFont,
    NSFontWeightSemibold, NSLineBreakMode, NSNormalWindowLevel, NSPanel, NSScreen,
    NSStatusWindowLevel, NSTextField, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::gesture::{
    ALT, CTRL, GestureEvent, InputEvent, MouseDelivery, Position, SCROLL_STEP, SHIFT, SUPER,
    Triggers, WHEEL_SIZE, WheelInput,
};
use crate::model::{Location, TextPosition};

// Keep animations above menu bar items and the Dock while leaving pop-up menus above them.
// 动画显示在菜单栏项目和程序坞上方，弹出菜单仍在更上层。
const ANIMATION_WINDOW_LEVEL: isize = NSStatusWindowLevel + 1;

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

fn palette_panel(window: &Window) -> Option<Retained<NSPanel>> {
    let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    // SAFETY: GPUI lends its live main-thread view for the duration of this call.
    // 安全性：GPUI 在本次调用期间借出主线程上的有效视图。
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()?.downcast().ok()
}

pub(crate) fn configure_palette(window: &Window, pinned: bool) -> bool {
    let Some(panel) = palette_panel(window) else {
        return false;
    };
    // GPUI's popup still has a titled AppKit frame. Keep its geometry when removing the title bar.
    // GPUI 弹窗仍有 AppKit 标题栏边框，移除时保留窗口的大小和位置。
    let frame = panel.frame();
    panel.setStyleMask(NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel);
    panel.setFrame_display(frame, true);
    panel.setHidesOnDeactivate(false);
    panel.setBecomesKeyOnlyIfNeeded(true);
    set_palette_pinned(window, pinned)
}

pub(crate) fn set_palette_pinned(window: &Window, pinned: bool) -> bool {
    let Some(panel) = palette_panel(window) else {
        return false;
    };
    panel.setLevel(if pinned {
        NSFloatingWindowLevel
    } else {
        NSNormalWindowLevel
    });
    panel.setCollectionBehavior(if pinned {
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
    } else {
        NSWindowCollectionBehavior::Default
    });
    true
}

pub(crate) fn palette_outside(window: &Window, pointer: Point<Pixels>) -> Option<Position> {
    let panel = palette_panel(window)?;
    let viewport = window.viewport_size();
    if gpui_kit::Bounds::new(gpui_kit::Point::default(), viewport).contains(&pointer) {
        return None;
    }
    // GPUI reports pointer events in top-left content coordinates. Convert the event
    // through AppKit to the Quartz coordinate space consumed by the desktop overlays.
    // GPUI 返回以内容区左上角为原点的指针坐标。经 AppKit 转为悬浮层使用的 Quartz 坐标。
    let on_screen = panel.convertPointToScreen(NSPoint::new(
        f64::from(pointer.x),
        f64::from(viewport.height - pointer.y),
    ));
    let primary = NSScreen::screens(MainThreadMarker::new()?)
        .firstObject()?
        .frame();
    Some(Position {
        x: on_screen.x,
        y: primary.origin.y + primary.size.height - on_screen.y,
    })
}

pub(crate) fn palette_left_released() -> bool {
    // GPUI maps Control-click to a right click even if Control was pressed mid-drag.
    // 即使在拖动途中才按下 Control，GPUI 也可能将松开左键映射为右键事件。
    MainThreadMarker::new().is_some_and(|mtm| {
        NSApplication::sharedApplication(mtm)
            .currentEvent()
            .is_some_and(|event| event.r#type() == NSEventType::LeftMouseUp)
    })
}

pub(crate) fn resize_palette(window: &Window, size: Size<Pixels>, cx: &App) -> Option<Task<()>> {
    let panel = palette_panel(window)?;
    // Resize outside the GPUI update because AppKit invokes native resize callbacks synchronously.
    // 在 GPUI 更新之外调整尺寸，因为 AppKit 会同步调用原生尺寸回调。
    Some(cx.spawn(async move |_| {
        if panel.isVisible() {
            let frame = panel.frame();
            let height = f64::from(size.height);
            panel.setFrame_display(
                rect(
                    frame.origin.x,
                    frame.origin.y + frame.size.height - height,
                    f64::from(size.width),
                    height,
                ),
                true,
            );
        }
    }))
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
        return Location::CENTER;
    };
    Location {
        x: ((point.x - screen.origin.x) / screen.size.width).clamp(0.0, 1.0),
        y: (1.0 - (point.y - screen.origin.y) / screen.size.height).clamp(0.0, 1.0),
    }
}

define_class!(
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    #[name = "PingPingPingWheelPanel"]
    struct WheelPanel;

    impl WheelPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool { true }
    }
);

fn panel(
    size: NSSize,
    mtm: MainThreadMarker,
    wheel: bool,
) -> (Retained<NSPanel>, Retained<CALayer>) {
    let bounds = NSRect::new(NSPoint::ZERO, size);
    // A nonactivating key panel borrows keyboard focus without bringing chat or settings forward.
    // 非激活面板可临时取得键盘焦点，同时保持聊天、设置窗口原有的前后顺序。
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = if wheel {
        let panel: Retained<WheelPanel> = unsafe {
            msg_send![WheelPanel::alloc(mtm),
            initWithContentRect: bounds, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        Retained::into_super(panel)
    } else {
        NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            bounds,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(!wheel);
    panel.setHidesOnDeactivate(false);
    panel.setLevel(NSFloatingWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    let layer = CALayer::layer();
    if let Some(view) = panel.contentView() {
        layer.setFrame(bounds);
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

struct Effect {
    panel: Retained<NSPanel>,
    art: Retained<CALayer>,
}

impl Drop for Effect {
    fn drop(&mut self) {
        self.panel.orderOut(None);
    }
}

pub(crate) struct PalettePreview {
    effect: Effect,
    mtm: MainThreadMarker,
}

impl PalettePreview {
    pub(crate) fn new(frame: RgbaImage) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let size = f64::from(super::PALETTE_PREVIEW_SIZE);
        let (panel, art) = panel(NSSize::new(size, size), mtm, false);
        panel.setLevel(ANIMATION_WINDOW_LEVEL);
        panel.setAlphaValue(0.85);
        update_layers(|| set_pixels(&art, frame.width(), frame.height(), frame.into_raw(), 3));
        Some(Self {
            effect: Effect { panel, art },
            mtm,
        })
    }

    pub(crate) fn show_at(&self, position: Option<Position>) {
        let panel = &self.effect.panel;
        let Some((center, _)) = position.and_then(|position| screen_for(position, self.mtm)) else {
            panel.orderOut(None);
            return;
        };
        let half = f64::from(super::PALETTE_PREVIEW_SIZE) / 2.0;
        panel.setFrameOrigin(NSPoint::new(center.x - half, center.y - half));
        if !panel.isVisible() {
            panel.orderFrontRegardless();
        }
    }
}

pub(crate) struct Overlay {
    mtm: MainThreadMarker,
    wheel: Retained<NSPanel>,
    wheel_art: Retained<CALayer>,
    previous_key: Option<Retained<NSWindow>>,
    pub(super) wheel_view: WheelView,
    pub(super) animations: AnimationOverlay,
    pub(super) audio: std::sync::mpsc::SyncSender<AudioCue>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        let mtm = MainThreadMarker::new().expect("GUI must use the main thread");
        let size = f64::from(WHEEL_SIZE);
        let (wheel, wheel_art) = panel(NSSize::new(size, size), mtm, true);
        let scale = NSScreen::screens(mtm)
            .firstObject()
            .map_or(1.0, |screen| screen.backingScaleFactor());
        let wheel_view = WheelView::new((f64::from(WHEEL_SIZE) * scale).round() as u32);
        Self {
            mtm,
            wheel,
            wheel_art,
            previous_key: None,
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
        let tick = self.wheel_view.open(contents, size, selected, present, cx);
        if !self.wheel.isKeyWindow() {
            let app = NSApplication::sharedApplication(self.mtm);
            self.previous_key = app.keyWindow().filter(|_| app.isActive());
            self.wheel.setIgnoresMouseEvents(false);
            self.wheel.makeKeyAndOrderFront(None);
        }
        if tick && let Some(volume) = tick_volume {
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

    fn release_focus(&mut self) {
        let previous = self.previous_key.take();
        self.wheel.setIgnoresMouseEvents(true);
        if self.wheel.isKeyWindow() {
            self.wheel.orderOut(None);
            if let Some(previous) = previous.filter(|window| {
                window.isVisible() && NSApplication::sharedApplication(self.mtm).isActive()
            }) {
                previous.makeKeyWindow();
            }
        }
    }

    pub(crate) fn close(&mut self, cx: &App) {
        self.release_focus();
        self.wheel_view.close(cx);
    }

    pub(crate) fn hide(&mut self) {
        self.release_focus();
        self.wheel_view.hide();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.release_focus();
    }
}

pub(super) fn animation_presenter(
    size: u32,
    tag: SenderTag<'_>,
    position: AnimationPosition,
) -> Option<(FrameSize, impl FnMut(FrameUpdate) + use<>)> {
    let mtm = MainThreadMarker::new()?;
    let center = match position {
        AnimationPosition::Local(local) => screen_for(local, mtm)?.0,
        AnimationPosition::Relative(location) => {
            let screen = NSScreen::screens(mtm).firstObject()?.frame();
            NSPoint::new(
                screen.origin.x + location.x * screen.size.width,
                screen.origin.y + (1.0 - location.y) * screen.size.height,
            )
        }
    };
    let size = f64::from(size);
    // An overlay can straddle displays, so allow enough pixels for the highest backing scale.
    // 浮层可能横跨显示器，按最高的屏幕缩放倍率保留足够像素。
    let scale = NSScreen::screens(mtm)
        .iter()
        .map(|screen| screen.backingScaleFactor())
        .fold(1.0, f64::max);
    let frame_size = FrameSize::FitWithin((size * scale).ceil() as u32);
    let half = size / 2.0;
    // Keep the gesture anchor when the art or sender tag extends past the display.
    // 动画或昵称条超出显示器边缘时，保留手势锚点。
    let origin = NSPoint::new(center.x - half, center.y - half);
    let name = sender_tag(tag.name, mtm);
    if let Some(name) = &name {
        let size = name.panel.frame().size;
        name.panel.setFrameOrigin(NSPoint::new(
            center.x - size.width / 2.0,
            center.y - tag.offset - size.height,
        ));
    }
    let mut effect = None;
    Some((frame_size, move |update| {
        let window = effect.get_or_insert_with(|| {
            let (panel, art) = panel(NSSize::new(size, size), mtm, false);
            panel.setLevel(ANIMATION_WINDOW_LEVEL);
            art.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspect });
            Effect { panel, art }
        });
        match update {
            FrameUpdate::Image(frame) => {
                update_layers(|| {
                    set_pixels(
                        &window.art,
                        frame.width(),
                        frame.height(),
                        frame.into_raw(),
                        3,
                    );
                });
                if !window.panel.isVisible() {
                    window.panel.setFrameOrigin(origin);
                    window.panel.orderFrontRegardless();
                    if let Some(name) = &name {
                        name.panel.orderFrontRegardless();
                    }
                }
            }
            FrameUpdate::Opacity(opacity) => {
                window.panel.setAlphaValue(f64::from(opacity));
                if let Some(name) = &name {
                    name.panel.setAlphaValue(f64::from(opacity));
                }
            }
        }
    }))
}

fn sender_tag(name: &str, mtm: MainThreadMarker) -> Option<Effect> {
    let label = NSTextField::labelWithString(&NSString::from_str(name), mtm);
    label.setSelectable(false);
    let weight = unsafe { NSFontWeightSemibold };
    label.setFont(Some(&NSFont::systemFontOfSize_weight(
        f64::from(TAG_FONT_SIZE),
        weight,
    )));
    label.setTextColor(Some(&text_color(gpui_kit::rgba(TAG_FOREGROUND))));
    label
        .cell()?
        .setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let text = label.fittingSize();
    let padding = f64::from(TAG_PADDING);
    let width = (text.width.ceil() + 2.0 * padding).min(f64::from(TAG_WIDTH));
    if width <= 2.0 * padding {
        return None;
    }
    let height = f64::from(TAG_HEIGHT);
    let (panel, art) = panel(NSSize::new(width, height), mtm, false);
    panel.setLevel(ANIMATION_WINDOW_LEVEL);
    let view = panel.contentView()?;
    art.setCornerRadius(f64::from(TAG_RADIUS));
    art.setBackgroundColor(Some(&text_color(gpui_kit::rgba(TAG_BACKGROUND)).CGColor()));
    label.setFrame(rect(
        padding,
        (height - text.height) / 2.0,
        width - 2.0 * padding,
        text.height,
    ));
    label.setWantsLayer(true);
    if let Some(layer) = label.layer() {
        layer.setShadowOpacity(1.0);
        layer.setShadowOffset(NSSize::new(0.0, -1.0));
        layer.setShadowRadius(1.0);
    }
    view.addSubview(&label);
    Some(Effect { panel, art })
}

pub(super) fn system_reduces_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

pub(super) fn text_presenter(
    text: &str,
    position: TextPosition,
    colors: TextColors,
) -> Option<impl FnMut(TextFrame) + use<>> {
    let mtm = MainThreadMarker::new()?;
    let work = NSScreen::screens(mtm).firstObject()?.visibleFrame();
    let width = f64::from(TEXT_WIDTH).min(work.size.width);
    let padding = f64::from(TEXT_PADDING);
    let inner_width = width - 2.0 * padding;
    let inner_height = work.size.height - 2.0 * padding;
    if inner_width <= 0.0 || inner_height <= 0.0 {
        return None;
    }
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setSelectable(false);
    label.setFont(Some(&NSFont::systemFontOfSize(f64::from(TEXT_FONT_SIZE))));
    label.setTextColor(Some(&text_color(colors.foreground)));
    label.setMaximumNumberOfLines(TEXT_MAX_LINES as isize);
    label.setPreferredMaxLayoutWidth(inner_width);
    label.cell()?.setTruncatesLastVisibleLine(true);
    let text_height = label.fittingSize().height.ceil().min(inner_height);
    let size = NSSize::new(width, text_height + 2.0 * padding);
    let (panel, art) = panel(size, mtm, false);
    let view = panel.contentView()?;
    art.setCornerRadius(f64::from(TEXT_RADIUS));
    art.setBackgroundColor(Some(&text_color(colors.background).CGColor()));
    label.setFrame(rect(padding, padding, inner_width, text_height));
    view.addSubview(&label);

    let free_x = work.size.width - size.width;
    let free_y = work.size.height - size.height;
    let (x, y) = text_anchor(position, free_x, free_y, f64::from(TEXT_MARGIN));
    let window = Effect { panel, art };
    Some(move |frame: TextFrame| {
        window.panel.setAlphaValue(f64::from(frame.opacity));
        window.panel.setFrameOrigin(NSPoint::new(
            work.origin.x + (x + frame.offset_x).clamp(0.0, free_x),
            work.origin.y + free_y - (y + frame.offset_y).clamp(0.0, free_y),
        ));
        if !window.panel.isVisible() {
            window.panel.orderFrontRegardless();
        }
    })
}

fn text_color(color: gpui_kit::Rgba) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(color.r),
        f64::from(color.g),
        f64::from(color.b),
        f64::from(color.a),
    )
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
