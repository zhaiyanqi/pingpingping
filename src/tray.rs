use async_channel::Sender;
use gpui_kit::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Clone, Copy)]
pub(crate) enum Command {
    Chat,
    History,
    Settings,
    Quit,
}

pub(crate) fn install(sender: Sender<Command>) -> anyhow::Result<TrayIcon> {
    let menu = Menu::new();
    let items = [
        (Command::Chat, "聊天"),
        (Command::History, "历史记录"),
        (Command::Settings, "设置"),
        (Command::Quit, "退出"),
    ];
    let mut commands = Vec::new();
    for (command, label) in items {
        if matches!(command, Command::Quit) {
            menu.append(&PredefinedMenuItem::separator())?;
        }
        let item = MenuItem::new(label, true, None);
        commands.push((item.id().clone(), command));
        menu.append(&item)?;
    }
    let pixels = image::load_from_memory(include_bytes!("../assets/textures/omw_cyan.png"))?
        .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let tray = TrayIconBuilder::new()
        .with_tooltip("PingPingPing")
        .with_icon(Icon::from_rgba(pixels.into_raw(), 32, 32)?)
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
    Ok(tray)
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
