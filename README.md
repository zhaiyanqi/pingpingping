# PingPingPing

[English](README.en.md)

PingPingPing 是用 Rust 编写的 Windows 和 macOS 桌面信号、表情轮盘与局域网聊天工具。设置、聊天和历史记录使用 GPUI Kit。轮盘、信号和表情由 Win32 或 AppKit 原生绘制。

启动后默认在后台运行。点击 Windows 通知区域或 macOS 菜单栏中的 PingPingPing 图标，可打开聊天、历史记录或设置。关闭窗口只会将其隐藏，草稿和房间连接都会保留；要结束程序，请在托盘菜单中选择「退出」。

## Windows 编译

适用环境为 Windows 11 x64。安装 [Rustup](https://rustup.rs/) 和 [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/) 中的 **使用 C++ 的桌面开发**工作负载，包含 MSVC x64 工具和 Windows 10/11 SDK。如果初次安装时选择了其他 Rust 工具链，请运行：

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc --profile minimal
```

双击 [Release 脚本](script/build-windows-release.bat) 或 [Debug 脚本](script/build-windows-debug.bat) 编译对应版本。Release 脚本会定位 Visual Studio 和 GPUI 所需的 Windows SDK 着色器编译器 `fxc.exe`。从 PowerShell 运行时可加 `--no-pause`。

```powershell
# Release
.\script\build-windows-release.bat --no-pause
.\target\x86_64-pc-windows-msvc\release\pingpingping.exe

# Debug
.\script\build-windows-debug.bat --no-pause
.\target\x86_64-pc-windows-msvc\debug\pingpingping.exe
```

素材已嵌入 exe。若脚本找不到 `fxc.exe`，在 Visual Studio Installer 中检查 SDK 是否已安装，或把环境变量 `GPUI_FXC_PATH` 设为该文件的完整路径。

## macOS 编译

安装稳定版 Rust 和 Xcode Command Line Tools。在项目根目录运行：

```sh
# Release
./script/build-macos-release.sh
open dist/PingPingPing.app

# Debug
./script/build-macos-debug.sh
open dist/debug/PingPingPing.app
```

也可以用 `cargo run --release --locked` 直接运行。首次使用全局轮盘时，在应用设置中请求输入监控权限。若授权后轮盘仍不可用，退出并重新打开应用。

## 局域网连接

1. **主机**找到正在使用的网卡的 IPv4 地址。Windows 可运行 `ipconfig`。在 **设置 → 局域网房间** 填写本机地址和 TCP 端口（默认 `49378`），点 **创建房间**。如 Windows 防火墙询问，允许应用在专用网络上通信。
2. **好友**连接同一局域网，在应用中填写主机的 IP 和端口，点 **连接**。主机保持程序运行，可接入最多 16 个客户端。
3. **聊天**可发送文字和表情。房间内的信号和表情可以在桌面播放，历史窗口可筛选「仅表情」或查看所有会话。

应用只接受私有、链路本地或回环 IPv4 地址。TCP 房间无认证和加密，适合可信局域网。同机测试可使用 `127.0.0.1`，为两个实例分别设置不同的 `PINGPINGPING_DATA_DIR`，以分开身份和历史。

## 桌面动画

在 **设置 → 桌面动画** 可分别调整信号和表情的大小、停留时长，并在保存前预览。素材按原始帧率循环，到设定时长自动消失。在 **音效** 中可以调节音量，并单独开关表情音效和轮盘提示音。

## 快捷键和本地数据

| 操作 | Windows | macOS |
| --- | --- | --- |
| 信号轮盘 | 按住 Alt 移动鼠标 | 按住 Option 移动鼠标 |
| 表情轮盘 | 按住 Ctrl 移动鼠标 | 按住 Control 移动鼠标 |
| 普通标记 | Alt + 单击 | Option + 单击 |
| 聊天 / 历史 / 设置 | Ctrl+1 / Ctrl+2 / Ctrl+, | ⌘1 / ⌘2 / ⌘, |

松开按键确认选中的扇区，移回中心再松开可取消。最近 600 条文字、信号和表情以 JSON 保存在本机。Windows 数据目录为 `%LOCALAPPDATA%\PingPingPing`，macOS 为 `~/Library/Application Support/PingPingPing`。

## 开发和验证

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked clients_join_by_endpoint_exchange_messages_and_notice_disconnect -- --ignored
```

最后一项需要本机 TCP 回环权限。模块说明和修改约定见 [AGENTS.md](AGENTS.md)。轮盘图像、音效和 WebP 动画来自 lolPing 素材，权利归原权利人所有。表情音效由 Ogg Opus 转成 PCM WAV。
