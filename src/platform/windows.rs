//! Windows input hooks and GDI-backed per-pixel alpha overlays.
//! Windows 全局输入钩子与基于 GDI 的逐像素透明悬浮层。

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use async_channel::Sender;
use gpui_kit::{App, Pixels, Point, Size, Window};
use image::RgbaImage;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    Graphics::{
        Dwm::{DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DwmSetWindowAttribute},
        Gdi::*,
    },
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        HiDpi::*,
        Input::KeyboardAndMouse::*,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

use super::playback::{
    FrameSize, FrameUpdate, SenderTag, TAG_BACKGROUND, TAG_FONT_SIZE, TAG_FOREGROUND, TAG_HEIGHT,
    TAG_PADDING, TAG_RADIUS, TAG_WIDTH, TEXT_FONT_SIZE, TEXT_MARGIN, TEXT_MAX_LINES, TEXT_PADDING,
    TEXT_RADIUS, TEXT_WIDTH, TextColors, TextFrame, text_anchor,
};
use super::{
    AnimationOverlay, AnimationPosition, AudioCue, WheelContents, start_audio, wheel::WheelView,
};
use crate::gesture::{
    ALT, CTRL, GestureEvent, InputEvent, MouseDelivery, Position, SHIFT, SUPER, TriggerMode,
    Triggers, WHEEL_SIZE, WheelInput,
};
use crate::model::{Location, TextPosition};

pub(crate) fn init_dpi() {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub(crate) struct InputMonitor {
    thread: u32,
    worker: Option<JoinHandle<()>>,
    palette_pointer: Arc<Mutex<Option<Sender<PalettePointer>>>>,
}

pub(crate) struct PalettePointer {
    pub(crate) position: Position,
    pub(crate) released: bool,
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
        let palette_pointer = Arc::new(Mutex::new(None));
        let hook_pointer = palette_pointer.clone();
        let worker = std::thread::Builder::new()
            .name("global-input".into())
            .spawn(move || {
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
                let mut message = MSG::default();
                unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE) };
                let Some(move_window) = WheelMouseWindow::new() else {
                    return;
                };
                INPUT.with(|state| {
                    *state.borrow_mut() = Some(Input {
                        events,
                        triggers,
                        gesture: WheelInput::default(),
                        move_window,
                        paused,
                        keys: pressed_modifiers(),
                        palette_pointer: hook_pointer,
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
            palette_pointer,
        })
    }

    pub(crate) fn watch_palette_pointer(&self) -> Option<async_channel::Receiver<PalettePointer>> {
        let mut pending = self.palette_pointer.lock().ok()?;
        // A nonactivating window cannot rely on ordinary capture outside its client area.
        // 不激活的浮窗无法依赖普通鼠标捕获来接收客户区外的事件。
        if unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON)) } >= 0 {
            return None;
        }
        let (events, receiver) = async_channel::bounded(1);
        *pending = Some(events);
        Some(receiver)
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
    move_window: WheelMouseWindow,
    keys: u16,
    paused: std::sync::Arc<AtomicBool>,
    palette_pointer: Arc<Mutex<Option<Sender<PalettePointer>>>>,
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
fn pressed_modifiers() -> u16 {
    MODIFIERS.iter().enumerate().fold(0, |keys, (index, key)| {
        if unsafe { GetAsyncKeyState(*key as i32) } < 0 {
            keys | (1 << index)
        } else {
            keys
        }
    })
}

impl Input {
    fn reconcile_keyboard(&mut self) {
        if self.triggers.is_none() {
            return;
        }
        // A release can go to another input desktop, for example while Windows is locked.
        // Deliver missed releases before starting or confirming another gesture.
        // 锁屏等情况下，松键可能发生在另一个输入桌面。开始或确认手势前先补上遗漏的松键。
        let modifiers = pressed_modifiers();
        let released = self
            .gesture
            .release_missing_keys(|key| unsafe { GetAsyncKeyState(i32::from(key)) } < 0);
        if released || self.keys != modifiers {
            self.keys = modifiers;
            self.dispatch(Some(POINT::default()), InputEvent::SyncKeyboard);
        }
    }

    fn forward_palette_pointer(&self, message: u32, point: POINT) {
        match message {
            WM_MOUSEMOVE => {
                if let Ok(pending) = self.palette_pointer.try_lock()
                    && let Some(events) = pending.as_ref()
                {
                    let _ = events.force_send(PalettePointer {
                        position: position(point),
                        released: false,
                    });
                }
            }
            WM_LBUTTONUP => {
                if let Ok(mut pending) = self.palette_pointer.lock()
                    && let Some(events) = pending.take()
                {
                    // Replace any queued movement with the exact release point.
                    // 用精确的松键坐标替换队列中尚未处理的移动事件。
                    let _ = events.force_send(PalettePointer {
                        position: position(point),
                        released: true,
                    });
                }
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, cursor: Option<POINT>, event: InputEvent) -> MouseDelivery {
        let Some(config) = &self.triggers else {
            return MouseDelivery::Pass;
        };
        let mut modifiers = 0;
        for (mask, modifier) in [(0x03, ALT), (0x0c, CTRL), (0x30, SHIFT), (0xc0, SUPER)] {
            if self.keys & mask != 0 {
                modifiers |= modifier;
            }
        }
        let point = cursor.unwrap_or_default();
        let at = position(point);
        let (updates, delivery) = self.gesture.event(
            config,
            modifiers,
            event,
            at,
            self.paused.load(Ordering::Relaxed) || cursor.is_none(),
            || f64::from(screen(point, false).1) / 96.0,
        );
        for update in updates {
            match update {
                GestureEvent::Open(..) if !self.move_window.show() => {
                    self.gesture
                        .event(config, modifiers, InputEvent::Interrupt, at, false, || 1.0);
                    // Keep a claimed side-button down paired with its swallowed release.
                    // 保持已暂扣的侧键按下与其松开成对拦截。
                    return if delivery == MouseDelivery::WheelMove {
                        MouseDelivery::Pass
                    } else {
                        delivery
                    };
                }
                GestureEvent::Finish(_) => self.move_window.hide(),
                _ => {}
            }
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
    let mut pressed = (VK_BACK..=0xfe).filter(|code| {
        state[*code as usize] & 0x80 != 0
            && !matches!(*code, VK_SHIFT | VK_CONTROL | VK_MENU)
            && !MODIFIERS.contains(&u32::from(*code))
    });
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
            if down {
                // The asynchronous state still precedes this low-level keyboard event.
                // 此时系统异步按键状态仍对应本次低级键盘事件之前的状态。
                input.reconcile_keyboard();
            }
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
            // A key-owned wheel opens and confirms at the cursor. If the desktop is
            // unavailable, still consume the state transition and cancel the gesture.
            // 按键轮盘按光标位置开启和确认。桌面不可访问时仍更新按键状态并取消手势。
            let mut cursor = POINT::default();
            let needs_cursor = input
                .triggers
                .as_ref()
                .is_some_and(|t| t.mode == TriggerMode::HoldShortcut);
            let located = !needs_cursor || unsafe { GetCursorPos(&mut cursor) } != 0;
            input.dispatch(located.then_some(cursor), kind);
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
            input.forward_palette_pointer(message as u32, event.pt);
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
                WM_MOUSEWHEEL => InputEvent::Scroll(-i32::from((event.mouseData >> 16) as i16)),
                WM_MOUSEHWHEEL => InputEvent::Scroll(0),
                _ => return MouseDelivery::Pass,
            };
            if matches!(kind, InputEvent::Left(_) | InputEvent::Side(..)) {
                input.reconcile_keyboard();
            }
            input.dispatch(Some(event.pt), kind)
        });
        match delivery {
            // Let Windows advance the real cursor and route WM_MOUSEMOVE to the wheel's input window.
            // 让 Windows 更新系统光标，由轮盘输入窗口接收 WM_MOUSEMOVE。
            MouseDelivery::WheelMove => {}
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
    let (info, dpi) = monitor_info(point, primary);
    (info.rcMonitor, dpi)
}

fn monitor_info(point: POINT, primary: bool) -> (MONITORINFO, u32) {
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
    (info, dpi.clamp(96, 384))
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

fn window_hwnd(window: &Window) -> Option<HWND> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}

pub(crate) fn use_system_close_button(window: &Window) {
    if let Some(hwnd) = window_hwnd(window)
        && unsafe { SetWindowSubclass(hwnd, Some(system_close_button_proc), 1, 0) } == 0
    {
        eprintln!("无法注册系统标题栏关闭按钮处理程序");
    }
}

unsafe extern "system" fn system_close_button_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    match message {
        WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCLBUTTONDBLCLK if wparam == HTCLOSE as usize => {
            // Let Windows track the caption click through release. WM_CLOSE still goes to GPUI.
            // 由 Windows 跟踪标题栏点击直到松开，WM_CLOSE 仍交给 GPUI 处理。
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        WM_NCDESTROY => unsafe { RemoveWindowSubclass(hwnd, Some(system_close_button_proc), id) },
        _ => 0,
    };
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

pub(crate) struct PaletteMove {
    hwnd: HWND,
    grab: Point<Pixels>,
}

impl PaletteMove {
    pub(crate) fn new(window: &Window, grab: Point<Pixels>) -> Option<Self> {
        Some(Self {
            hwnd: window_hwnd(window)?,
            grab,
        })
    }

    pub(crate) fn update(&self, event: PalettePointer) -> bool {
        let at = point(event.position);
        let (_, dpi) = screen(at, false);
        let scale = f64::from(dpi) / 96.0;
        // Keep the same logical point under the pointer across displays with different DPI.
        // 跨不同 DPI 的显示器时，保持鼠标抓住浮窗上的同一逻辑位置。
        let x = at.x - (f64::from(self.grab.x) * scale).round() as i32;
        let y = at.y - (f64::from(self.grab.y) * scale).round() as i32;
        if event.released {
            release_palette_capture(self.hwnd);
        }
        unsafe {
            SetWindowPos(
                self.hwnd,
                null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            ) != 0
        }
    }
}

pub(crate) struct PaletteResize {
    hwnd: HWND,
    frame: RECT,
    press: POINT,
    minimum: SIZE,
}

impl PaletteResize {
    pub(crate) fn new(
        window: &Window,
        press: Point<Pixels>,
        minimum: Size<Pixels>,
    ) -> Option<Self> {
        let hwnd = window_hwnd(window)?;
        let scale = f64::from(window.scale_factor());
        let physical = |value: Pixels| (f64::from(value) * scale).round() as i32;
        let mut press = POINT {
            x: physical(press.x),
            y: physical(press.y),
        };
        let mut frame = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut frame) == 0 || ClientToScreen(hwnd, &mut press) == 0 }
        {
            return None;
        }
        Some(Self {
            hwnd,
            frame,
            press,
            minimum: SIZE {
                cx: physical(minimum.width),
                cy: physical(minimum.height),
            },
        })
    }

    pub(crate) fn update(&self, event: PalettePointer) -> bool {
        let point = point(event.position);
        let width =
            (self.frame.right - self.frame.left + point.x - self.press.x).max(self.minimum.cx);
        let height =
            (self.frame.bottom - self.frame.top + point.y - self.press.y).max(self.minimum.cy);
        if event.released {
            release_palette_capture(self.hwnd);
        }
        unsafe {
            SetWindowPos(
                self.hwnd,
                null_mut(),
                self.frame.left,
                self.frame.top,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            ) != 0
        }
    }
}

fn release_palette_capture(hwnd: HWND) {
    unsafe {
        if GetCapture() == hwnd {
            ReleaseCapture();
        }
    }
}

pub(crate) fn configure_palette(window: &Window, pinned: bool) -> bool {
    let Some(hwnd) = window_hwnd(window) else {
        return false;
    };
    unsafe {
        if SetWindowSubclass(hwnd, Some(palette_proc), 1, 0) == 0 {
            return false;
        }
        // The popup has no Win32 frame; Windows 11 can still draw a compositor border.
        // 弹出窗口没有 Win32 边框，Windows 11 仍可能由桌面合成器绘制描边。
        let color = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            (&raw const color).cast(),
            size_of_val(&color) as u32,
        );
    }
    set_palette_pinned(window, pinned)
}

pub(crate) fn set_palette_pinned(window: &Window, pinned: bool) -> bool {
    window_hwnd(window).is_some_and(|hwnd| unsafe {
        SetWindowPos(
            hwnd,
            if pinned { HWND_TOPMOST } else { HWND_NOTOPMOST },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        ) != 0
    })
}

unsafe extern "system" fn palette_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_NCDESTROY => unsafe { RemoveWindowSubclass(hwnd, Some(palette_proc), id) },
        _ => 0,
    };
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

pub(crate) fn palette_drop(window: &Window, release: Position) -> Option<Position> {
    release_palette_capture(window_hwnd(window)?);
    palette_outside(window, release)
}

pub(crate) fn palette_outside(window: &Window, pointer: Position) -> Option<Position> {
    let hwnd = window_hwnd(window)?;
    let mut bounds = RECT::default();
    let mut client_point = point(pointer);
    let outside = unsafe {
        GetClientRect(hwnd, &mut bounds) != 0
            && ScreenToClient(hwnd, &mut client_point) != 0
            && PtInRect(&bounds, client_point) == 0
    };
    outside.then_some(pointer)
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

// A window without a redirection bitmap is visually transparent but still participates in hit testing.
// 无重定向位图的窗口在视觉上透明，同时仍能接收命中测试范围内的鼠标消息。
struct WheelMouseWindow(HWND);

impl WheelMouseWindow {
    fn new() -> Option<Self> {
        let class = windows_sys::w!("PingPingPingWheelMouse");
        unsafe {
            let instance = GetModuleHandleW(null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wheel_mouse_proc),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class,
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
            );
            (!hwnd.is_null()).then(|| Self(hwnd))
        }
    }

    fn show(&self) -> bool {
        unsafe {
            SetWindowPos(
                self.0,
                HWND_TOPMOST,
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            ) != 0
        }
    }

    fn hide(&self) {
        unsafe { ShowWindow(self.0, SW_HIDE) };
    }
}

impl Drop for WheelMouseWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

unsafe extern "system" fn wheel_mouse_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_MOUSEMOVE => 0,
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

// The wheel is reused across gestures; Alt+F4 cancels the gesture without destroying its HWND.
// 轮盘窗口会被后续手势复用，Alt+F4 取消手势时要保留 HWND。
unsafe extern "system" fn wheel_layer_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => 0,
        WM_SYSCOMMAND if wparam as u32 & 0xfff0 == SC_KEYMENU => 0,
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
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
    width: u32,
    height: u32,
}

impl Surface {
    fn new(width: u32, height: u32) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
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
                width,
                height,
            })
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.bits,
                self.width as usize * self.height as usize * 4,
            )
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
    previous_foreground: Option<HWND>,
}

impl Layer {
    fn new(wheel: bool) -> Self {
        let class = if wheel {
            windows_sys::w!("PingPingPingWheel")
        } else {
            windows_sys::w!("PingPingPingOverlay")
        };
        let hwnd = unsafe {
            let instance = GetModuleHandleW(null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: if wheel {
                    Some(wheel_layer_proc)
                } else {
                    Some(overlay_proc)
                },
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            });
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | if wheel { 0 } else { WS_EX_NOACTIVATE }
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW,
                class,
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
            previous_foreground: None,
        }
    }

    fn focus_wheel(&mut self) {
        unsafe {
            let previous = GetForegroundWindow();
            if previous != self.hwnd
                && IsWindowVisible(self.hwnd) != 0
                && SetForegroundWindow(self.hwnd) != 0
            {
                self.previous_foreground = Some(previous);
                SetFocus(self.hwnd);
            }
        }
    }

    fn release_focus(&mut self) {
        let Some(previous) = self.previous_foreground.take() else {
            return;
        };
        // Only return focus we still own. A user may already have switched to a third window.
        // 仅在轮盘仍持有焦点时归还，避免打断用户已切换到的其他窗口。
        unsafe {
            if GetForegroundWindow() == self.hwnd
                && (IsWindowVisible(previous) == 0 || SetForegroundWindow(previous) == 0)
            {
                ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    fn warm(&mut self, size: u32) {
        if !self.hwnd.is_null()
            && !self
                .surface
                .as_ref()
                .is_some_and(|s| (s.width, s.height) == (size, size))
        {
            self.surface = Surface::new(size, size);
        }
    }

    fn frame(&mut self, image: &RgbaImage) {
        self.warm(image.width());
        if let Some(surface) = &mut self.surface {
            surface.bytes_mut().copy_from_slice(image.as_raw());
        }
    }

    fn wheel_frame(&mut self, image: &tiny_skia::Pixmap, origin: POINT) {
        self.warm(image.width());
        if let Some(surface) = &mut self.surface {
            let bytes = surface.bytes_mut();
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
            cx: surface.width as i32,
            cy: surface.height as i32,
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
        self.release_focus();
        if !self.hwnd.is_null() {
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

pub(crate) struct PalettePreview {
    layer: Layer,
    half: i32,
}

impl PalettePreview {
    pub(crate) fn new(frame: RgbaImage) -> Option<Self> {
        let mut layer = Layer::new(false);
        layer.frame(&frame);
        layer.surface.as_ref()?;
        Some(Self {
            layer,
            half: frame.width() as i32 / 2,
        })
    }

    pub(crate) fn show_at(&self, position: Option<Position>) {
        if let Some(position) = position {
            let center = point(position);
            self.layer.present(
                POINT {
                    x: center.x - self.half,
                    y: center.y - self.half,
                },
                217,
            );
        } else {
            unsafe { ShowWindow(self.layer.hwnd, SW_HIDE) };
        }
    }
}

pub(crate) struct Overlay {
    wheel: Rc<RefCell<Layer>>,
    pub(super) wheel_view: WheelView,
    pub(super) animations: AnimationOverlay,
    pub(super) audio: std::sync::mpsc::SyncSender<AudioCue>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        let (_, dpi) = screen(POINT::default(), true);
        let wheel = Layer::new(true);
        Self {
            wheel: Rc::new(RefCell::new(wheel)),
            wheel_view: WheelView::new(pixels(WHEEL_SIZE, dpi)),
            animations: AnimationOverlay::default(),
            audio: start_audio(),
        }
    }

    pub(crate) fn open(
        &mut self,
        contents: WheelContents,
        position: Position,
        selected: Option<usize>,
        mode: TriggerMode,
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
        let tick = self.wheel_view.open(contents, size, selected, present, cx);
        if mode == TriggerMode::MouseDrag {
            self.wheel.borrow_mut().focus_wheel();
        }
        if tick && let Some(volume) = tick_volume {
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
        self.wheel.borrow_mut().release_focus();
        self.wheel_view.close(cx);
    }

    pub(crate) fn hide(&mut self) {
        self.wheel.borrow_mut().release_focus();
        self.wheel_view.hide();
    }
}

pub(super) fn animation_presenter(
    size: u32,
    tag: SenderTag<'_>,
    position: AnimationPosition,
) -> Option<(FrameSize, impl FnMut(FrameUpdate) + use<>)> {
    let (center, dpi) = match position {
        AnimationPosition::Local(local) => {
            let point = point(local);
            let (_, dpi) = screen(point, false);
            (point, dpi)
        }
        AnimationPosition::Relative(location) => {
            let (bounds, dpi) = screen(POINT::default(), true);
            let point = POINT {
                x: bounds.left + (location.x * f64::from(bounds.right - bounds.left)) as i32,
                y: bounds.top + (location.y * f64::from(bounds.bottom - bounds.top)) as i32,
            };
            (point, dpi)
        }
    };
    let size = pixels(size, dpi);
    let half = f64::from(size) / 2.0;
    let offset = tag.offset * f64::from(dpi) / 96.0;
    // Keep the gesture anchor when the art or sender tag extends past the display.
    // 动画或昵称条超出显示器边缘时，保留手势锚点。
    let origin = POINT {
        x: (f64::from(center.x) - half).round() as i32,
        y: (f64::from(center.y) - half).round() as i32,
    };
    let name = tag_surface(tag.name, dpi).map(|surface| {
        let point = POINT {
            x: (f64::from(center.x) - f64::from(surface.width) / 2.0).round() as i32,
            y: center.y + offset.round() as i32,
        };
        let mut layer = Layer::new(false);
        layer.surface = Some(surface);
        (layer, point)
    });
    let mut layer = None;
    Some((FrameSize::Square(size), move |update| {
        let first_frame = layer.is_none();
        let layer = layer.get_or_insert_with(|| Layer::new(false));
        match update {
            FrameUpdate::Image(frame) => {
                layer.frame(&frame);
                layer.present(origin, 255);
                if first_frame && let Some((layer, point)) = &name {
                    layer.present(*point, 255);
                }
            }
            FrameUpdate::Opacity(opacity) => {
                let alpha = alpha_byte(opacity);
                layer.present(origin, alpha);
                if let Some((layer, point)) = &name {
                    layer.present(*point, alpha);
                }
            }
        }
    }))
}

fn tag_surface(name: &str, dpi: u32) -> Option<Surface> {
    let padding = pixels(TAG_PADDING, dpi);
    let max_width = pixels(TAG_WIDTH, dpi);
    let text: Vec<u16> = name.encode_utf16().collect();
    let measure = Surface::new(1, 1)?;
    let font = unsafe {
        CreateFontW(
            -(pixels(TAG_FONT_SIZE, dpi) as i32),
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET),
            0,
            0,
            u32::from(ANTIALIASED_QUALITY),
            0,
            windows_sys::w!("Segoe UI"),
        )
    };
    if font.is_null() {
        return None;
    }
    let surface = (|| {
        let flags = DT_NOPREFIX | DT_SINGLELINE;
        let mut bounds = RECT::default();
        unsafe {
            let old_font = SelectObject(measure.dc, font);
            DrawTextW(
                measure.dc,
                text.as_ptr(),
                text.len() as i32,
                &mut bounds,
                flags | DT_CALCRECT,
            );
            SelectObject(measure.dc, old_font);
        }
        let width = (bounds.right.max(0) as u32 + 2 * padding).min(max_width);
        let height = pixels(TAG_HEIGHT, dpi);
        let mut surface = Surface::new(width, height)?;
        surface.bytes_mut().fill(0);
        let mut bounds = RECT {
            left: padding as i32,
            top: 0,
            right: (width - padding) as i32,
            bottom: height as i32,
        };
        unsafe {
            let old_font = SelectObject(surface.dc, font);
            SetTextColor(surface.dc, 0xffffff);
            SetBkMode(surface.dc, TRANSPARENT as i32);
            DrawTextW(
                surface.dc,
                text.as_ptr(),
                text.len() as i32,
                &mut bounds,
                flags | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
            );
            SelectObject(surface.dc, old_font);
            GdiFlush();
        }

        // GDI leaves alpha unset. Use the grayscale glyphs as coverage over the translucent card.
        // GDI 不写入透明度，将灰度字形用作半透明底色上的覆盖率。
        let mask: Vec<_> = surface
            .bytes_mut()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[0])
            .collect();
        fill_text_card(
            &mut surface,
            pixels(TAG_RADIUS, dpi),
            gpui_kit::rgba(TAG_BACKGROUND),
        );
        let color = gpui_kit::rgba(TAG_FOREGROUND);
        let shadow_offset = (width * pixels(1, dpi)) as usize;
        for (i, [b, g, r, a]) in surface
            .bytes_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let ink = f32::from(mask[i]) / 255.0;
            let shadow = i
                .checked_sub(shadow_offset)
                .map_or(0.0, |i| f32::from(mask[i]) / 255.0);
            let background = (1.0 - ink) * (1.0 - shadow);
            *b = alpha_byte(ink * color.b + background * f32::from(*b) / 255.0);
            *g = alpha_byte(ink * color.g + background * f32::from(*g) / 255.0);
            *r = alpha_byte(ink * color.r + background * f32::from(*r) / 255.0);
            *a = alpha_byte(1.0 - background * (1.0 - f32::from(*a) / 255.0));
        }
        Some(surface)
    })();
    unsafe { DeleteObject(font) };
    surface
}

fn alpha_byte(opacity: f32) -> u8 {
    (opacity * 255.0).round() as u8
}

pub(super) fn system_reduces_motion() -> bool {
    let mut enabled = 1_i32;
    unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut enabled).cast(), 0) != 0
            && enabled == 0
    }
}

pub(super) fn text_presenter(
    text: &str,
    position: TextPosition,
    colors: TextColors,
) -> Option<impl FnMut(TextFrame) + use<>> {
    let (info, dpi) = monitor_info(POINT::default(), true);
    let work = info.rcWork;
    let work_width = (work.right - work.left).max(0) as u32;
    let work_height = (work.bottom - work.top).max(0) as u32;
    let width = pixels(TEXT_WIDTH, dpi).min(work_width);
    let surface = text_surface(text, colors, width, work_height, dpi)?;
    let free_x = f64::from(work_width - surface.width);
    let free_y = f64::from(work_height - surface.height);
    let (x, y) = text_anchor(
        position,
        free_x,
        free_y,
        f64::from(pixels(TEXT_MARGIN, dpi)),
    );
    let mut layer = Layer::new(false);
    if layer.hwnd.is_null() {
        return None;
    }
    layer.surface = Some(surface);
    let scale = f64::from(dpi) / 96.0;
    Some(move |frame: TextFrame| {
        layer.present(
            POINT {
                x: work.left + (x + frame.offset_x * scale).clamp(0.0, free_x).round() as i32,
                y: work.top + (y + frame.offset_y * scale).clamp(0.0, free_y).round() as i32,
            },
            alpha_byte(frame.opacity),
        );
    })
}

fn text_surface(
    text: &str,
    colors: TextColors,
    width: u32,
    max_height: u32,
    dpi: u32,
) -> Option<Surface> {
    let padding = pixels(TEXT_PADDING, dpi) as i32;
    let inner_width = width as i32 - 2 * padding;
    let inner_height = max_height as i32 - 2 * padding;
    if inner_width <= 0 || inner_height <= 0 {
        return None;
    }
    let measure = Surface::new(1, 1)?;
    let text: Vec<u16> = text.replace('\0', "�").encode_utf16().collect();
    let font = unsafe {
        CreateFontW(
            -(pixels(TEXT_FONT_SIZE, dpi) as i32),
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET),
            0,
            0,
            u32::from(ANTIALIASED_QUALITY),
            0,
            windows_sys::w!("Segoe UI"),
        )
    };
    if font.is_null() {
        return None;
    }
    let result = (|| {
        let mut bounds = RECT {
            right: inner_width,
            ..Default::default()
        };
        let mut metrics = TEXTMETRICW::default();
        let flags = DT_NOPREFIX | DT_WORDBREAK | DT_EDITCONTROL | DT_END_ELLIPSIS;
        let height = unsafe {
            let old_font = SelectObject(measure.dc, font);
            let measured = DrawTextW(
                measure.dc,
                text.as_ptr(),
                text.len() as i32,
                &mut bounds,
                flags | DT_CALCRECT,
            );
            GetTextMetricsW(measure.dc, &mut metrics);
            SelectObject(measure.dc, old_font);
            measured
        };
        let height = height
            .min(metrics.tmHeight * TEXT_MAX_LINES as i32)
            .min(inner_height);
        if height <= 0 {
            return None;
        }
        let mut card = Surface::new(width, (height + 2 * padding) as u32)?;
        fill_text_card(
            &mut card,
            pixels(TEXT_RADIUS, dpi),
            gpui_kit::Rgba {
                a: 1.0,
                ..colors.background
            },
        );
        let mut bounds = RECT {
            left: padding,
            top: padding,
            right: width as i32 - padding,
            bottom: padding + height,
        };
        let color = colors.foreground;
        let rgb = u32::from(alpha_byte(color.r))
            | (u32::from(alpha_byte(color.g)) << 8)
            | (u32::from(alpha_byte(color.b)) << 16);
        unsafe {
            let old_font = SelectObject(card.dc, font);
            SetTextColor(card.dc, rgb);
            SetBkMode(card.dc, TRANSPARENT as i32);
            DrawTextW(
                card.dc,
                text.as_ptr(),
                text.len() as i32,
                &mut bounds,
                flags,
            );
            SelectObject(card.dc, old_font);
            GdiFlush();
        }
        // GDI writes RGB only. Restore alpha in the opaque text area before layered composition.
        // GDI 只写入 RGB，分层合成前恢复不透明文字区域的 alpha。
        for row in card
            .bytes_mut()
            .chunks_exact_mut(width as usize * 4)
            .skip(padding as usize)
            .take(height as usize)
        {
            for pixel in row
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .skip(padding as usize)
                .take(inner_width as usize)
            {
                pixel[3] = 255;
            }
        }
        Some(card)
    })();
    unsafe { DeleteObject(font) };
    result
}

fn fill_text_card(surface: &mut Surface, radius: u32, color: gpui_kit::Rgba) {
    let width = surface.width as usize;
    let height = surface.height as usize;
    let radius = radius as f32;
    for (y, row) in surface.bytes_mut().chunks_exact_mut(width * 4).enumerate() {
        let dy = (radius - (y.min(height - 1 - y) as f32 + 0.5)).max(0.0);
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let dx = (radius - (x.min(width - 1 - x) as f32 + 0.5)).max(0.0);
            let alpha = if dx > 0.0 && dy > 0.0 {
                (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let alpha = alpha * color.a;
            *pixel = [
                alpha_byte(color.b * alpha),
                alpha_byte(color.g * alpha),
                alpha_byte(color.r * alpha),
                alpha_byte(alpha),
            ];
        }
    }
}
