use async_channel::Sender;
use gpui_kit::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Clone, Copy)]
pub(crate) enum Command {
    Chat,
    History,
    Settings,
    ToggleMute,
    Quit,
}

pub(crate) struct Tray {
    _icon: TrayIcon,
    mute: CheckMenuItem,
}

impl Tray {
    pub(crate) fn set_incoming_muted(&self, muted: bool) {
        self.mute.set_checked(muted);
    }
}

pub(crate) fn install(sender: Sender<Command>, muted: bool) -> anyhow::Result<Tray> {
    let menu = Menu::new();
    let chat = MenuItem::new("聊天", true, None);
    let history = MenuItem::new("历史记录", true, None);
    let settings = MenuItem::new("设置", true, None);
    let mute = CheckMenuItem::new("静音", true, muted, None);
    let quit = MenuItem::new("退出", true, None);
    menu.append_items(&[
        &chat,
        &history,
        &settings,
        &mute,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let commands = [
        (chat.id().clone(), Command::Chat),
        (history.id().clone(), Command::History),
        (settings.id().clone(), Command::Settings),
        (mute.id().clone(), Command::ToggleMute),
        (quit.id().clone(), Command::Quit),
    ];
    #[cfg(target_os = "windows")]
    let image = Icon::from_resource(1, Some((32, 32)))?;
    #[cfg(target_os = "macos")]
    let image = {
        let pixels = image::load_from_memory(include_bytes!("../assets/icons/macos-menu-bar.png"))?
            .into_rgba8();
        let (width, height) = pixels.dimensions();
        Icon::from_rgba(pixels.into_raw(), width, height)?
    };
    let icon = TrayIconBuilder::new()
        .with_tooltip("PingPingPing")
        .with_icon(image)
        .with_icon_as_template(cfg!(target_os = "macos"))
        .with_menu(Box::new(menu))
        .build()?;
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some((_, command)) = commands.iter().find(|(id, _)| *id == event.id) {
            let _ = sender.try_send(*command);
        }
    }));
    // Only menu commands matter; do not queue unused pointer events.
    // 仅处理菜单命令，避免未消费的指针事件积压。
    TrayIconEvent::set_event_handler(Some(|_| {}));
    Ok(Tray { _icon: icon, mute })
}

pub(crate) fn hide_window(window: &Window) -> bool {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    match handle.as_raw() {
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(handle) => {
            // GPUI lends its main-thread NSView for this callback only.
            // GPUI 仅在此次回调期间借出主线程上的 NSView。
            let view = unsafe { handle.ns_view.cast::<objc2_app_kit::NSView>().as_ref() };
            view.window().is_some_and(|native| {
                native.orderOut(None);
                true
            })
        }
        #[cfg(target_os = "windows")]
        RawWindowHandle::Win32(handle) => {
            use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindowAsync};
            unsafe { ShowWindowAsync(handle.hwnd.get() as _, SW_HIDE) != 0 }
        }
        _ => false,
    }
}

pub(crate) fn restore_window(window: &Window) {
    #[cfg(target_os = "windows")]
    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::Win32(handle) = handle.as_raw()
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOW, ShowWindowAsync};
        unsafe { ShowWindowAsync(handle.hwnd.get() as _, SW_SHOW) };
    }
    window.activate_window();
}
