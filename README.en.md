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

The executable embeds its media. If `fxc.exe` is missing, check the SDK installation or set `GPUI_FXC_PATH` to the compiler's absolute path.

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

You can also run `cargo run --release --locked`. Request Input Monitoring through Settings for global gestures. When using a side-button binding, also grant PingPingPing Accessibility access in Privacy & Security. Restart the app if gestures remain unavailable after granting access.

## Connect on a LAN

1. The **host** finds their active adapter's IPv4 address. On Windows, use `ipconfig`. Enter that address and the TCP port (default `49378`) in **Settings → LAN room**, then create the room. If prompted, allow the app through Windows Firewall on private networks.
2. **Guests** on the same LAN enter the host's IP and port and connect. The host keeps the app running and can accept up to 16 guests.
3. **Chat** supports text and emotes. Room pings and emotes can play on the desktop. History can show all conversations or filter to emotes.

The app accepts private, link-local, and loopback IPv4 addresses. LAN traffic is unauthenticated and unencrypted. Use it on a trusted LAN. For a same-machine test, use `127.0.0.1` and assign each instance a different `PINGPINGPING_DATA_DIR` to keep their identities and histories separate.

## Desktop animations

Under **Settings → Desktop animations**, set the size and timeout for pings and emotes separately and preview them before saving. Animations loop at their source frame rate and disappear at the configured timeout. **Sound** controls the volume and has separate switches for emote sounds and wheel ticks.

Right-click the Windows system tray or macOS menu bar icon and select **Mute**. You can also toggle it under **Settings → Desktop animations**. Mute takes effect immediately and persists across restarts. Incoming content appears only in chat and history, and your own pings and emotes still play.

## Shortcuts and local data

| Action | Windows | macOS |
| --- | --- | --- |
| Ping wheel | Hold Alt and left-drag | Hold Option and left-drag |
| Emote wheel | Hold Ctrl and left-drag | Hold Control and left-drag |
| Quick ping (off by default) | Alt-click | Option-click |
| Chat / History / Settings | Ctrl+1 / Ctrl+2 / Ctrl+, | ⌘1 / ⌘2 / ⌘, |

In **Settings → Desktop wheel**, turn each wheel on or off independently. Click the current binding, press a keyboard key or mouse button 4 (Back) or 5 (Forward), then save. Esc cancels recording. Enabled wheels must use different bindings.

For a keyboard binding, hold the key and left-drag. Releasing the key first cancels. For a side-button binding, hold that button, drag, then release it to confirm. Returning to the center cancels either gesture. A plain side-button click goes to the current app on release. Once the wheel opens, the drag consumes the click, including when you return to the center to cancel. If quick ping is enabled, a click of the ping wheel’s bound side button sends a marker.

The latest 600 text messages, pings, and emotes are stored locally as JSON in `%LOCALAPPDATA%\PingPingPing` on Windows or `~/Library/Application Support/PingPingPing` on macOS.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked clients_join_by_endpoint_exchange_messages_and_notice_disconnect -- --ignored
```

The final test requires TCP loopback access. See [AGENTS.md](AGENTS.md) for architecture and development conventions. Wheel artwork, sounds, and WebP animations come from lolPing's asset set and retain their respective ownership. Emote sounds were converted from Ogg Opus to PCM WAV.
