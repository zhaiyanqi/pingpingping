//! Windows input hooks and GDI-backed per-pixel alpha overlays.
//! Windows 全局输入钩子与基于 GDI 的逐像素透明悬浮层。

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use async_channel::Sender;
use gpui_kit::App;
use image::RgbaImage;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

use super::{AnimationOverlay, AudioCue, start_audio, wheel::WheelView};
use crate::gesture::{
    ALT, CTRL, GestureEvent, InputEvent, MouseDelivery, Position, SHIFT, SUPER, Triggers,
    WHEEL_SIZE, Wheel, WheelInput,
};
use crate::model::Location;

pub(crate) fn init_dpi() {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub(crate) struct InputMonitor {
    thread: u32,
    worker: Option<JoinHandle<()>>,
}

impl InputMonitor {
    // Run hooks on a dedicated message loop.
    // 在独立消息循环中运行钩子。
    pub(crate) fn start(
        events: Sender<GestureEvent>,
        triggers: Option<Triggers>,
        paused: std::sync::Arc<AtomicBool>,
    ) -> Option<Self> {
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
                        triggers,
                        gesture: WheelInput::default(),
                        wheel_cursor: None,
                        paused,
                        keys: initial_keys(),
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
                        if message.message == REPLAY_MOUSE_CLICK && message.wParam <= 2 {
                            replay_mouse_click(message.wParam as u32);
                        } else {
                            unsafe { DispatchMessageW(&message) };
                        }
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
    triggers: Option<Triggers>,
    gesture: WheelInput,
    wheel_cursor: Option<Position>,
    keys: u16,
    paused: std::sync::Arc<AtomicBool>,
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
    fn dispatch(&mut self, point: POINT, event: InputEvent) -> MouseDelivery {
        let Some(config) = &self.triggers else {
            return MouseDelivery::Pass;
        };
        let mut modifiers = 0;
        for (mask, modifier) in [(0x03, ALT), (0x0c, CTRL), (0x30, SHIFT), (0xc0, SUPER)] {
            if self.keys & mask != 0 {
                modifiers |= modifier;
            }
        }
        let mut at = position(point);
        if let Some(previous) = self.wheel_cursor {
            // A swallowed move does not advance the Windows cursor. Accumulate its proposed displacement.
            // 吞掉移动事件后系统光标不会前进，因此用建议坐标与实际坐标的差累积轮盘位置。
            let mut cursor = POINT::default();
            if unsafe { GetCursorPos(&mut cursor) } != 0 {
                at = Position {
                    x: previous.x + at.x - f64::from(cursor.x),
                    y: previous.y + at.y - f64::from(cursor.y),
                };
            }
        }
        let (updates, delivery) = self.gesture.event(
            config,
            modifiers,
            event,
            at,
            self.paused.load(Ordering::Relaxed),
            || f64::from(screen(point, false).1) / 96.0,
        );
        self.wheel_cursor = self.gesture.visible().then_some(at);
        for update in updates {
            let _ = self.events.try_send(update);
        }
        delivery
    }
}

pub(crate) fn capture_key_code() -> Option<u16> {
    // The focused window's message queue reflects the current key event, including keypad keys.
    // 焦点窗口消息队列中的按键状态与当前事件对应，也能区分小键盘按键。
    let mut state = [0_u8; 256];
    if unsafe { GetKeyboardState(state.as_mut_ptr()) } == 0 {
        return None;
    }
    let mut pressed = (VK_BACK..=0xfe).filter(|code| state[*code as usize] & 0x80 != 0);
    let code = pressed.next()?;
    pressed.next().is_none().then_some(code)
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
            let kind = if let Some(index) = MODIFIERS.iter().position(|key| *key == event.vkCode) {
                if down {
                    input.keys |= 1 << index;
                } else {
                    input.keys &= !(1 << index);
                }
                InputEvent::Modifier
            } else {
                InputEvent::Key(event.vkCode as u16, down)
            };
            let mut cursor = POINT::default();
            if unsafe { GetCursorPos(&mut cursor) } != 0 {
                input.dispatch(cursor, kind);
            }
        });
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

const REPLAY_MOUSE_CLICK: u32 = WM_APP + 1;
const REPLAY_MARKER: usize = 0x50505058;

fn replay_mouse_click(bit: u32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT as WinInput;
    let flags = if bit == 0 {
        [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP]
    } else {
        [MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP]
    };
    let pair = flags.map(|flags| WinInput {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                mouseData: bit,
                dwExtraInfo: REPLAY_MARKER,
                ..Default::default()
            },
        },
    });
    let size = size_of::<WinInput>() as i32;
    let sent = unsafe { SendInput(2, pair.as_ptr(), size) };
    if sent < 2 {
        let remaining = &pair[sent as usize..];
        if unsafe { SendInput(remaining.len() as u32, remaining.as_ptr(), size) }
            != remaining.len() as u32
        {
            eprintln!("无法还原鼠标单击");
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && data != 0 {
        let event = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        if event.dwExtraInfo == REPLAY_MARKER && event.flags & LLMHF_INJECTED != 0 {
            return unsafe { CallNextHookEx(null_mut(), code, message, data) };
        }
        let delivery = INPUT.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(input) = cell.as_mut() else {
                return MouseDelivery::Pass;
            };
            let kind = match message as u32 {
                WM_MOUSEMOVE if input.keys != 0 || input.gesture.active() => InputEvent::Move,
                WM_LBUTTONDOWN => InputEvent::Left(true),
                WM_LBUTTONUP => InputEvent::Left(false),
                WM_XBUTTONDOWN | WM_XBUTTONUP => match event.mouseData >> 16 {
                    bit @ (1 | 2) => InputEvent::Side(bit as u8, message as u32 == WM_XBUTTONDOWN),
                    _ => InputEvent::Interrupt,
                },
                WM_RBUTTONDOWN | WM_RBUTTONUP => {
                    InputEvent::OtherButton(1, message as u32 == WM_RBUTTONDOWN)
                }
                WM_MBUTTONDOWN | WM_MBUTTONUP => {
                    InputEvent::OtherButton(2, message as u32 == WM_MBUTTONDOWN)
                }
                WM_MOUSEWHEEL | WM_MOUSEHWHEEL => InputEvent::Interrupt,
                _ => return MouseDelivery::Pass,
            };
            input.dispatch(event.pt, kind)
        });
        match delivery {
            MouseDelivery::HoldDown | MouseDelivery::Swallow => return 1,
            // SendInput can block on our own hook. Replay after this callback returns.
            // SendInput 可能等待当前钩子返回，因此把单击交给消息循环回放。
            MouseDelivery::ReplayClick
                if unsafe {
                    PostThreadMessageW(
                        GetCurrentThreadId(),
                        REPLAY_MOUSE_CLICK,
                        if message as u32 == WM_LBUTTONUP {
                            0
                        } else {
                            (event.mouseData >> 16) as usize
                        },
                        0,
                    )
                } != 0 =>
            {
                return 1;
            }
            _ => {}
        }
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
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    surface.bits,
                    surface.size as usize * surface.size as usize * 4,
                )
            };
            bytes.copy_from_slice(image.as_raw());
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

    fn hide(&mut self) {
        self.surface = None;
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

pub(crate) struct Overlay {
    wheel: Rc<RefCell<Layer>>,
    wheel_view: WheelView,
    pub(super) animations: AnimationOverlay,
    pub(super) audio: std::sync::mpsc::SyncSender<AudioCue>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        let (_, dpi) = screen(POINT::default(), true);
        let wheel = Layer::new();
        Self {
            wheel: Rc::new(RefCell::new(wheel)),
            wheel_view: WheelView::new(pixels(WHEEL_SIZE, dpi)),
            animations: AnimationOverlay::default(),
            audio: start_audio(),
        }
    }

    pub(crate) fn open(
        &mut self,
        wheel: Wheel,
        position: Position,
        selected: Option<usize>,
        tick_volume: Option<f32>,
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
                layer.borrow_mut().hide();
            }
        });
        self.wheel_view.open(wheel, size, selected, present, cx);
        if let Some(volume) = tick_volume.filter(|_| selected.is_some()) {
            let _ = self.audio.try_send(AudioCue::Tick(volume));
        }
    }

    pub(crate) fn select(&mut self, selection: Option<usize>, tick_volume: Option<f32>, cx: &App) {
        if self.wheel_view.select(selection, cx)
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
    let size = pixels(size, dpi);
    let origin = fit(point, bounds, size);
    let mut layer = None;
    Some((Some(size), move |frame: RgbaImage| {
        layer.get_or_insert_with(Layer::new).frame(&frame, origin);
    }))
}
