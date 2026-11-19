# PingPingPing

[简体中文](README.md)

PingPingPing is a Rust desktop app for pings, emotes, and LAN chat on Windows and macOS. It uses GPUI Kit for settings, chat, and history, with Win32 or AppKit rendering for desktop overlays.

PingPingPing starts in the background. Click its icon in the Windows notification area or macOS menu bar to open chat, history, or settings. Closing a window hides it without losing your draft or disconnecting from the room. Select **退出** (Quit) in the tray menu to exit.

## Build on Windows

On Windows 11 x64, install [Rustup](https://rustup.rs/) and the **Desktop development with C++** workload in [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/). Include the MSVC x64 tools and a Windows 10 or 11 SDK. If the Rust MSVC toolchain is missing, install it with:

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc --profile minimal
```

Double-click the [Release script](script/build-windows-release.bat) or the [Debug script](script/build-windows-debug.bat). The Release script locates Visual Studio and the Windows SDK shader compiler `fxc.exe` required by GPUI. Pass `--no-pause` when running it from a terminal or automation.

```powershell
# Release
.\script\build-windows-release.bat --no-pause
.\target\x86_64-pc-windows-msvc\release\pingpingping.exe

# Debug
.\script\build-windows-debug.bat --no-pause
.\target\x86_64-pc-windows-msvc\debug\pingpingping.exe
```

The executable, notification area, taskbar, and windows use the gold question mark from the Enemy Missing ping. The executable embeds its icon and media. If `fxc.exe` is missing, check the SDK installation or set `GPUI_FXC_PATH` to the compiler's absolute path.

### Package for Windows

Run these commands from the repository root in PowerShell. Building Setup requires Inno Setup 6.3 or later. The script looks for `ISCC.exe` on PATH and in common installation directories. Use `-IsccPath` to supply the full path if needed. [Download Inno Setup](https://jrsoftware.org/isdl.php)

```powershell
# Build Setup and the portable ZIP with size optimization
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\script\package-windows.ps1

# Build only the portable ZIP, which requires no Inno Setup installation
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\script\package-windows.ps1 -Package Portable

# Build only Setup with the standard Release profile
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\script\package-windows.ps1 -Package Setup -StandardRelease
```

Packages appear under `dist/windows/` with the version from `Cargo.toml` in their names. With `-StandardRelease`, the filenames include `-release` so you can keep both builds.

The `*-setup.exe` installer adds Start menu and uninstall entries for the current user without requiring admin rights. To use the `*-portable.zip` package, extract it and double-click `Launch-PingPingPing.bat`. Settings, history, and custom emotes go in the `data` folder next to the launcher.

To build only the size-optimized executable, run `.\script\build-windows-release.bat --size --no-pause`. The executable goes into `target/x86_64-pc-windows-msvc/release-small/`. The profile optimizes for size, enables Thin LTO, and strips symbols. Setup uses LZMA2 compression, and Windows can extract the portable ZIP directly. The packaging script prints the size of each finished package.

## Build on macOS

Install stable Rust and Xcode Command Line Tools, then run:

```sh
# Release
./script/build-macos-release.sh
open dist/PingPingPing.app

# Debug
./script/build-macos-debug.sh
open dist/debug/PingPingPing.app
```

Finder and the Dock use the gold question mark. The menu bar uses a monochrome question mark that adapts to the system appearance. You can also run `cargo run --release --locked`. Request Input Monitoring through Settings for global gestures. When either wheel is enabled, also grant PingPingPing Accessibility access in Privacy & Security so it can intercept mouse events during a wheel gesture. Restart the app if gestures remain unavailable after granting access.

## Connect on a LAN

1. The **host** finds their active adapter's IPv4 address. On Windows, use `ipconfig`. Enter that address and the TCP port (default `30086`) in **Settings → LAN room**, then create the room. If prompted, allow the app through Windows Firewall on private networks.
2. **Guests** on the same LAN enter the host's IP and port and connect. The host keeps the app running and can accept up to 16 guests.
3. In **Chat**, press Enter to send text and Shift+Enter to start a new line. Choose an item from **Emotes** or **Pings** above the input to send it. While outside a room, messages stay on this device. History can show all conversations or filter to emotes.

Use **Emotes → Import image…** to add a GIF, PNG (including APNG), JPEG, or WebP. Images can be up to 8 MiB and 2048 pixels on either side. Select the thumbnail to send it. Room members automatically download missing images and play them according to their desktop animation settings. They reuse any cached copy and can click the thumbnail in chat or history to replay it. The host and all guests need a version that supports custom emotes.

In chat, select **Add to emote library** below a received custom emote to save it locally and send it again. For an emote already in your library, select **Add to wheel**. Emotes that are missing from the wheel are appended to its last page. The app creates a page if there's no room at the end.

The app accepts private, link-local, and loopback IPv4 addresses. LAN traffic is unauthenticated and unencrypted. Use it on a trusted LAN. For a same-machine test, use `127.0.0.1` and assign each instance a different `PINGPINGPING_DATA_DIR` to keep their identities and histories separate.

## Desktop animations

The ping and emote wheels take keyboard focus when opened and release it on selection or cancellation. Playing, previewing, or replaying an animation leaves the current window focused.

Under **Settings → Desktop animations**, set the size and timeout for pings and emotes separately and preview them before saving. Animations loop at their source frame rate, and emotes fade out once their timeout elapses. **Sound** controls the volume and has separate switches for emote sounds and wheel ticks.

Sent and received text appears in the bottom-right corner of the primary display by default. Under **Settings → Desktop animations → Text overlay**, set the position, hold time (four seconds by default), and default entrance and exit effects. Select **Preview text** to see the result before saving. New text replaces the message currently on screen, and chat and history retain the full message.

Use **Effect** next to the chat composer to choose a shake, fly-in, bounce, fade, slide, or no animation for the current message. Select **Preview** to play your draft with the selected effect on your desktop before sending. After a successful send, the next message uses the default effects. Each room member uses their own hold time. The room host and receiver both need per-message effect support for the selected effect to reach the receiver. Older versions can still exchange text.

Right-click the Windows system tray or macOS menu bar icon and select **Mute**. You can also toggle it under **Settings → Desktop animations**. Mute takes effect immediately and persists across restarts. Incoming content appears only in chat and history, while your own text, pings, and emotes continue to appear on the desktop.

## Shortcuts and local data

| Action | Windows | macOS |
| --- | --- | --- |
| Ping wheel | Hold Alt and left-drag | Hold Option and left-drag |
| Emote wheel | Hold Ctrl and left-drag | Hold Control and left-drag |
| Quick ping (off by default) | Alt-click | Option-click |
| Chat / History / Settings | Ctrl+1 / Ctrl+2 / Ctrl+, | ⌘1 / ⌘2 / ⌘, |

In **Settings → Desktop wheel**, turn each wheel on or off independently. Click the current binding, press a keyboard key or mouse button 4 (Back) or 5 (Forward), then save. Esc cancels recording. Enabled wheels must use different bindings.

Under **Emote wheel layout** in the same category, add or remove pages. Each page has eight directional slots. Drag an assigned emote to another slot to swap positions. Click or drag an emote from the library to assign it to a slot. You can also drag a single GIF, PNG, JPEG, or WebP file from your file manager to import it directly into a slot. Use **Swap positions** to move an emote across pages. Click **Save settings** to apply the layout. With the emote wheel open, scroll down for the next page or up for the previous page. Scrolling wraps between the first and last pages. Reopening the emote wheel returns to the last page you viewed while the app is running. Release over an emote on the current page to send it. Releasing over an empty slot cancels the send.

For a keyboard binding, hold the key and left-drag. Releasing the key first cancels. For a side-button binding, hold that button, drag, then release it to confirm. Returning to the center cancels either gesture. A plain click goes to the current app on release. The system pointer moves normally, and its position on release determines the selection. While the wheel is open, it consumes mouse movement, button presses, scrolling, and the release that confirms or cancels the gesture. If quick ping is enabled, a click of the ping wheel’s bound side button sends a marker.

The latest 600 text messages, pings, and emotes are stored locally as JSON. Custom emotes are stored in the `emotes` subdirectory. The app data directory is `%LOCALAPPDATA%\PingPingPing` on Windows or `~/Library/Application Support/PingPingPing` on macOS.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked clients_join_by_endpoint_exchange_messages_and_notice_disconnect -- --ignored
```

The final test requires TCP loopback access. See [AGENTS.md](AGENTS.md) for architecture and development conventions. Wheel artwork, sounds, and WebP animations come from lolPing's asset set and retain their respective ownership. Emote sounds were converted from Ogg Opus to PCM WAV.
