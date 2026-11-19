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
    app.run(ui::start);
}
