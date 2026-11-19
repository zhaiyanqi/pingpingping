//! Application entry point and native platform bootstrap.
//! 应用入口与原生平台初始化。

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
compile_error!("The native input and overlay backend supports Windows and macOS.");

mod assets;
mod gesture;
mod model;
mod platform;
mod room;
mod tray;
mod ui;

fn main() {
    #[cfg(target_os = "windows")]
    platform::init_dpi();
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.on_reopen(ui::open_chat);
    app.run(|cx| {
        #[cfg(target_os = "macos")]
        install_unbundled_app_icon();
        ui::start(cx);
    });
}

#[cfg(target_os = "macos")]
fn install_unbundled_app_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{NSBundle, NSData};

    // AppKit loads the bundle icon automatically; a bare Cargo binary needs it set explicitly.
    // AppKit 会自动加载应用包图标，Cargo 直接运行的二进制文件需要显式设置。
    if NSBundle::mainBundle().bundleIdentifier().is_none()
        && let Some(main_thread) = MainThreadMarker::new()
    {
        let data = NSData::with_bytes(include_bytes!("../packaging/pingpingping.icns"));
        if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) {
            // SAFETY: The marker guarantees this runs on the AppKit main thread.
            // 安全性：标记保证此调用在 AppKit 主线程执行。
            unsafe {
                NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon))
            };
        }
    }
}
