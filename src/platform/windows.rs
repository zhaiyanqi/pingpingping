//! Windows input hooks and GDI-backed per-pixel alpha overlays.
//! Windows 全局输入钩子与基于 GDI 的逐像素透明悬浮层。

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_channel::Sender;
use gpui_kit::{App, Task};
use image::{RgbaImage, imageops};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

use super::{AudioCue, decode_animation, start_audio, wheel::WheelView};
use crate::assets::{EMOTES, PINGS};
use crate::gesture::{Gesture, GestureEvent, Position, WHEEL_SIZE, Wheel};
use crate::model::Location;

const EFFECT_SIZE: u32 = 138;

pub(crate) fn init_dpi() {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub(crate) struct InputMonitor {
    thread: u32,
    worker: Option<JoinHandle<()>>,
}

impl InputMonitor {
    // Run hook callbacks on their own message loop and pass every event through.
    // 在独立消息循环中运行钩子，并继续传递所有输入事件。
    pub(crate) fn start(events: Sender<GestureEvent>, enabled: Arc<AtomicBool>) -> Option<Self> {
        let (ready, result) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("global-input".into())
            .spawn(move || {
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
                let mut message = MSG::default();
                unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE) };
                INPUT.with(|state| {
                    *state.borrow_mut() = Some(Input {
                        events,
                        enabled,
                        gesture: Gesture::default(),
                        keys: initial_keys(),
                        suppressed: true,
                    });
                });
                let (keyboard, mouse) = unsafe {
                    let instance = GetModuleHandleW(null());
                    (
                        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0),
                        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0),
                    )
                };
                let installed = !keyboard.is_null() && !mouse.is_null();
                if ready
                    .send(installed.then(|| unsafe { GetCurrentThreadId() }))
                    .is_ok()
                    && installed
                {
                    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
                        unsafe { DispatchMessageW(&message) };
                    }
                }
                unsafe {
                    if !keyboard.is_null() {
                        UnhookWindowsHookEx(keyboard);
                    }
                    if !mouse.is_null() {
                        UnhookWindowsHookEx(mouse);
                    }
                }
            })
            .ok()?;
        let thread = result.recv().ok().flatten()?;
        Some(Self {
            thread,
            worker: Some(worker),
        })
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.thread, WM_QUIT, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

thread_local! {
    static INPUT: RefCell<Option<Input>> = const { RefCell::new(None) };
}

struct Input {
    events: Sender<GestureEvent>,
    enabled: Arc<AtomicBool>,
    gesture: Gesture,
    keys: u16,
    suppressed: bool,
}

const MODIFIERS: [u32; 8] = [
    VK_LMENU as u32,
    VK_RMENU as u32,
    VK_LCONTROL as u32,
    VK_RCONTROL as u32,
    VK_LSHIFT as u32,
    VK_RSHIFT as u32,
    VK_LWIN as u32,
    VK_RWIN as u32,
];
const ALT: u16 = 0b00000011;
const CTRL: u16 = 0b00001100;

fn initial_keys() -> u16 {
    MODIFIERS.iter().enumerate().fold(0, |keys, (index, key)| {
        if unsafe { GetAsyncKeyState(*key as i32) } < 0 {
            keys | (1 << index)
        } else {
            keys
        }
    })
}

impl Input {
    fn pointer(&mut self, position: Position, interrupted: bool, click: bool) {
        let held = self.keys & (ALT | CTRL);
        let kind = if self.keys == 0 {
            None
        } else if self.keys & !ALT == 0 {
            Some(Wheel::Ping)
        } else if self.keys & !CTRL == 0 {
            Some(Wheel::Emote)
        } else {
            self.suppressed = true;
            None
        };
        if interrupted || !self.enabled.load(Ordering::Relaxed) {
            self.suppressed = true;
        }
        let cancel = self.suppressed;
        if held == 0 {
            self.suppressed = false;
        }
        if click
            && !cancel
            && kind == Some(Wheel::Ping)
            && let Some(origin) = self.gesture.click(position)
        {
            self.suppressed = true;
            let _ = self
                .events
                .try_send(GestureEvent::Finish(Some((Wheel::Ping, 8, origin))));
            return;
        }
        let scale = if !cancel && kind.is_some() && self.gesture.pending() {
            f64::from(screen(point(position), false).1) / 96.0
        } else {
            1.0
        };
        for event in self.gesture.input(kind, position, cancel, scale) {
            let _ = self.events.try_send(event);
        }
    }
}

fn position(point: POINT) -> Position {
    Position {
        x: f64::from(point.x),
        y: f64::from(point.y),
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && data != 0 {
        let event = unsafe { &*(data as *const KBDLLHOOKSTRUCT) };
        let down = matches!(message as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        INPUT.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(input) = cell.as_mut() else { return };
            let modifier = MODIFIERS.iter().position(|key| *key == event.vkCode);
            if let Some(index) = modifier {
                let bit = 1 << index;
                if down {
                    if input.keys == 0 {
                        input.suppressed = false;
                    }
                    input.keys |= bit;
                } else {
                    input.keys &= !bit;
                }
            }
            if modifier.is_some() || (down && input.keys & (ALT | CTRL) != 0) {
                let mut cursor = POINT::default();
                if unsafe { GetCursorPos(&mut cursor) } != 0 {
                    input.pointer(position(cursor), modifier.is_none(), false);
                }
            }
        });
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && data != 0 {
        let event = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        INPUT.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(input) = cell.as_mut() else { return };
            if input.keys & (ALT | CTRL) != 0 {
                match message as u32 {
                    WM_MOUSEMOVE | WM_LBUTTONDOWN => {
                        input.pointer(position(event.pt), false, message as u32 == WM_LBUTTONDOWN)
                    }
                    WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_MOUSEWHEEL
                    | WM_MOUSEHWHEEL => input.pointer(position(event.pt), true, false),
                    _ => {}
                }
            }
        });
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

fn screen(point: POINT, primary: bool) -> (RECT, u32) {
    let monitor = unsafe {
        MonitorFromPoint(
            point,
            if primary {
                MONITOR_DEFAULTTOPRIMARY
            } else {
                MONITOR_DEFAULTTONEAREST
            },
        )
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mut dpi = 96;
    let mut vertical = 96;
    unsafe {
        GetMonitorInfoW(monitor, &mut info);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut vertical);
    }
    (info.rcMonitor, dpi.clamp(96, 384))
}

fn pixels(value: u32, dpi: u32) -> u32 {
    (value * dpi + 48) / 96
}

fn point(position: Position) -> POINT {
    POINT {
        x: position.x.round() as i32,
        y: position.y.round() as i32,
    }
}

pub(crate) fn normalized(position: Position) -> Location {
    let point = point(position);
    let (bounds, _) = screen(point, false);
    Location {
        x: (f64::from(point.x - bounds.left) / f64::from((bounds.right - bounds.left).max(1)))
            .clamp(0.0, 1.0),
        y: (f64::from(point.y - bounds.top) / f64::from((bounds.bottom - bounds.top).max(1)))
            .clamp(0.0, 1.0),
    }
}

fn fit(point: POINT, bounds: RECT, size: u32) -> POINT {
    let edge = size as i32;
    POINT {
        x: (point.x - edge / 2).clamp(bounds.left, (bounds.right - edge).max(bounds.left)),
        y: (point.y - edge / 2).clamp(bounds.top, (bounds.bottom - edge).max(bounds.top)),
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    size: u32,
}

impl Surface {
    fn new(size: u32) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return None;
            }
            let mut bits: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() {
                DeleteDC(dc);
                return None;
            }
            let original = SelectObject(dc, bitmap);
            if original.is_null() || bits.is_null() {
                DeleteDC(dc);
                DeleteObject(bitmap);
                return None;
            }
            Some(Self {
                dc,
                bitmap,
                original,
                bits: bits.cast(),
                size,
            })
        }
    }

    // UpdateLayeredWindow expects premultiplied BGRA pixels in the DIB.
    // UpdateLayeredWindow 要求 DIB 使用预乘透明度的 BGRA 像素。
    fn write(&mut self, image: &RgbaImage) {
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(self.bits, self.size as usize * self.size as usize * 4)
        };
        for (dest, source) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(image.pixels()) {
            let alpha = u32::from(source[3]);
            let premultiply = |channel: u8| ((u32::from(channel) * alpha + 127) / 255) as u8;
            *dest = [
                premultiply(source[2]),
                premultiply(source[1]),
                premultiply(source[0]),
                source[3],
            ];
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.original);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct Layer {
    hwnd: HWND,
    surface: Option<Surface>,
}

impl Layer {
    fn new() -> Self {
        let class: Vec<u16> = "PingPingPingOverlay\0".encode_utf16().collect();
        let hwnd = unsafe {
            let instance = GetModuleHandleW(null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..Default::default()
            });
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        Self {
            hwnd,
            surface: None,
        }
    }

    fn warm(&mut self, size: u32) {
        if !self.hwnd.is_null() && self.surface.as_ref().map(|surface| surface.size) != Some(size) {
            self.surface = Surface::new(size);
        }
    }

    fn frame(&mut self, image: &RgbaImage, origin: POINT) {
        self.warm(image.width());
        if let Some(surface) = &mut self.surface {
            surface.write(image);
            self.present(origin, 255);
        }
    }

    fn wheel_frame(&mut self, image: &tiny_skia::Pixmap, origin: POINT) {
        self.warm(image.width());
        if let Some(surface) = &mut self.surface {
            let bytes = unsafe { std::slice::from_raw_parts_mut(surface.bits, image.data().len()) };
            for (dest, [red, green, blue, alpha]) in bytes
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(image.data().as_chunks::<4>().0)
            {
                *dest = [*blue, *green, *red, *alpha];
            }
            self.present(origin, 255);
        }
    }

    fn present(&self, origin: POINT, opacity: u8) {
        let Some(surface) = &self.surface else { return };
        let size = SIZE {
            cx: surface.size as i32,
            cy: surface.size as i32,
        };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: opacity,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            if UpdateLayeredWindow(
                self.hwnd,
                null_mut(),
                &origin,
                &size,
                surface.dc,
                &source,
                0,
                &blend,
                ULW_ALPHA,
            ) != 0
                && IsWindowVisible(self.hwnd) == 0
            {
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    }

    fn hide(&self) {
        if !self.hwnd.is_null() {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        if !self.hwnd.is_null() {
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

struct Effect {
    layer: Rc<RefCell<Layer>>,
    task: Option<Task<()>>,
}

pub(crate) struct Overlay {
    wheel: Rc<RefCell<Layer>>,
    wheel_view: WheelView,
    pings: [RgbaImage; 9],
    emotes: [RgbaImage; EMOTES.len()],
    effects: [Effect; 3],
    next_effect: usize,
    audio: Sender<AudioCue>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        let decode = |clip: crate::assets::Clip| {
            image::load_from_memory_with_format(clip.image, image::ImageFormat::Png)
                .map_or_else(|_| RgbaImage::new(1, 1), |image| image.to_rgba8())
        };
        let pings = PINGS.map(decode);
        let emotes = EMOTES.map(decode);
        let (_, dpi) = screen(POINT::default(), true);
        let mut wheel = Layer::new();
        wheel.warm(pixels(WHEEL_SIZE, dpi));
        // Reuse hidden windows and pixel buffers for the first visible frame.
        // 复用隐藏窗口和像素缓冲区以缩短首帧等待。
        let effects = std::array::from_fn(|_| {
            let mut layer = Layer::new();
            layer.warm(pixels(EFFECT_SIZE, dpi));
            Effect {
                layer: Rc::new(RefCell::new(layer)),
                task: None,
            }
        });
        Self {
            wheel: Rc::new(RefCell::new(wheel)),
            wheel_view: WheelView::new(pixels(WHEEL_SIZE, dpi)),
            pings,
            emotes,
            effects,
            next_effect: 0,
            audio: start_audio(),
        }
    }

    pub(crate) fn open(
        &mut self,
        wheel: Wheel,
        position: Position,
        selected: Option<usize>,
        sound: bool,
        cx: &App,
    ) {
        let point = point(position);
        let (_, dpi) = screen(point, false);
        let size = pixels(WHEEL_SIZE, dpi);
        let origin = POINT {
            x: point.x - size as i32 / 2,
            y: point.y - size as i32 / 2,
        };
        let layer = self.wheel.clone();
        let present = Rc::new(move |frame: Option<&tiny_skia::Pixmap>| {
            if let Some(frame) = frame {
                layer.borrow_mut().wheel_frame(frame, origin);
            } else {
                layer.borrow().hide();
            }
        });
        self.wheel_view
            .open(wheel, size, selected, animation_enabled(), present, cx);
        if sound && selected.is_some() {
            let _ = self.audio.try_send(AudioCue::Tick);
        }
    }

    pub(crate) fn select(&mut self, selection: Option<usize>, sound: bool, cx: &App) {
        if self.wheel_view.select(selection, cx) && sound {
            let _ = self.audio.try_send(AudioCue::Tick);
        }
    }

    pub(crate) fn close(&mut self, cx: &App) {
        self.wheel_view.close(cx);
    }

    pub(crate) fn hide(&mut self) {
        self.wheel_view.hide();
    }

    pub(crate) fn play(
        &mut self,
        wheel: Wheel,
        index: usize,
        local: Option<Position>,
        location: Option<Location>,
        sound: bool,
        cx: &App,
    ) {
        let (image, clip) = match wheel {
            Wheel::Ping => (&self.pings[index], &PINGS[index]),
            Wheel::Emote => (&self.emotes[index], &EMOTES[index]),
        };
        let (point, bounds, dpi) = if let Some(local) = local {
            let point = point(local);
            let (bounds, dpi) = screen(point, false);
            (point, bounds, dpi)
        } else {
            let (bounds, dpi) = screen(POINT::default(), true);
            let location = location.unwrap_or(Location { x: 0.5, y: 0.5 });
            let point = POINT {
                x: bounds.left + (location.x * f64::from(bounds.right - bounds.left)) as i32,
                y: bounds.top + (location.y * f64::from(bounds.bottom - bounds.top)) as i32,
            };
            (point, bounds, dpi)
        };
        let size = pixels(EFFECT_SIZE, dpi);
        let origin = fit(point, bounds, size);
        let slot = self.next_effect;
        self.next_effect = (slot + 1) % self.effects.len();
        let effect = &mut self.effects[slot];
        effect.task.take();
        effect.layer.borrow_mut().frame(
            &imageops::resize(image, size, size, imageops::FilterType::Triangle),
            origin,
        );
        if sound {
            let _ = self.audio.try_send(AudioCue::Clip(wheel, index));
        }
        let animation_enabled = animation_enabled();
        let bytes = clip.animation.filter(|_| animation_enabled);
        let layer = effect.layer.clone();
        effect.task = Some(cx.spawn(async move |cx| {
            let started = Instant::now();
            if let Some(bytes) = bytes {
                let frames = cx
                    .background_executor()
                    .spawn(async move { decode_animation(bytes, size) })
                    .await;
                for (frame, delay) in frames {
                    if started.elapsed() > Duration::from_millis(1850) {
                        break;
                    }
                    layer.borrow_mut().frame(&frame, origin);
                    cx.background_executor().timer(delay).await;
                }
            }
            let remainder = Duration::from_millis(1400).saturating_sub(started.elapsed());
            if !remainder.is_zero() {
                cx.background_executor().timer(remainder).await;
            }
            if animation_enabled {
                for opacity in [215, 175, 135, 95, 55, 15] {
                    layer.borrow().present(origin, opacity);
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                }
            }
            layer.borrow().hide();
        }));
    }
}

fn animation_enabled() -> bool {
    let mut enabled = 1i32;
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        )
    };
    enabled != 0
}
