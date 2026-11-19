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

素材和程序图标已嵌入 exe。程序、通知区域、任务栏和窗口使用「敌人消失」信号的金色问号图案。若脚本找不到 `fxc.exe`，在 Visual Studio Installer 中检查 SDK 是否已安装，或把环境变量 `GPUI_FXC_PATH` 设为该文件的完整路径。

### Windows 打包

在项目根目录的 PowerShell 中运行以下命令。生成安装包需要 Inno Setup 6.3 或更新版本。脚本会从 PATH 和常见安装目录查找 `ISCC.exe`，也可以通过 `-IsccPath` 指定完整路径。 [Inno Setup 下载](https://jrsoftware.org/isdl.php)

```powershell
# 同时生成 Setup 和便携 ZIP，默认开启体积优化
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\script\package-windows.ps1

# 只生成便携 ZIP，无需安装 Inno Setup
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\script\package-windows.ps1 -Package Portable

# 只生成 Setup，使用普通 Release
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\script\package-windows.ps1 -Package Setup -StandardRelease
```

产物位于 `dist/windows/`，文件名包含 `Cargo.toml` 中的版本号。使用 `-StandardRelease` 时，文件名会多一个 `-release`，方便同时保留两种构建的包。

安装包 `*-setup.exe` 无需管理员权限，为当前用户安装并添加开始菜单入口和卸载项。便携包 `*-portable.zip` 解压后双击 `Launch-PingPingPing.bat`，设置、历史和自定义表情会保存在同目录的 `data` 文件夹中。

若只需编译体积优化版 exe，运行 `.\script\build-windows-release.bat --size --no-pause`，产物位于 `target/x86_64-pc-windows-msvc/release-small/`。该配置优先优化体积，使用 Thin LTO 并剥离符号。安装包使用 LZMA2 压缩，便携包使用 Windows 可直接解压的 ZIP。打包完成后会显示各产物的大小。

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

访达和 Dock 使用金色问号图标，菜单栏使用随系统外观着色的单色问号。也可以用 `cargo run --release --locked` 直接运行。首次使用全局轮盘时，在应用设置中请求输入监控权限。启用任一轮盘时，还需在「隐私与安全性 → 辅助功能」中允许 PingPingPing 拦截轮盘手势的鼠标事件。若授权后轮盘仍不可用，退出并重新打开应用。

## 局域网连接

1. **主机**找到正在使用的网卡的 IPv4 地址。Windows 可运行 `ipconfig`。在 **设置 → 局域网房间** 填写本机地址和 TCP 端口（默认 `30086`），点 **创建房间**。如 Windows 防火墙询问，允许应用在专用网络上通信。
2. **好友**连接同一局域网，在应用中填写主机的 IP 和端口，点 **连接**。主机保持程序运行，可接入最多 16 个客户端。
3. **聊天**支持 Enter 发送文字、Shift+Enter 换行。用输入框上方的 **表情** 或 **信号** 选择并发送。未入房时，消息只保存在本机。历史窗口可筛选「仅表情」或查看所有会话。

在 **表情 → 导入图片…** 选择 GIF、PNG（含 APNG）、JPEG 或 WebP。每张图片最多 8 MiB，宽高最多 2048 像素。导入后点选缩略图即可发送。房间成员会自动下载本机缺少的图片，并按桌面动画设置播放。已有同一张图时直接用缓存播放，也可点击聊天或历史里的缩略图重播。主机与房间成员均需使用支持自定义表情的新版本。

应用只接受私有、链路本地或回环 IPv4 地址。TCP 房间无认证和加密，适合可信局域网。同机测试可使用 `127.0.0.1`，为两个实例分别设置不同的 `PINGPINGPING_DATA_DIR`，以分开身份和历史。

## 桌面动画

信号和表情轮盘打开时临时取得键盘焦点，确认或取消后归还。播放、预览或重播动画时，焦点留在当前窗口。

在 **设置 → 桌面动画** 可分别调整信号和表情的大小、停留时长，并在保存前预览。素材按原始帧率循环，表情到设定时长后淡出。在 **音效** 中可以调节音量，并单独开关表情音效和轮盘提示音。

发送或收到的文字默认在主屏幕右下角显示。可在 **设置 → 桌面动画 → 文字浮层** 调整显示位置、停留时长（默认 4 秒）和默认入场、退场动效。点击 **预览文字** 可在保存前查看效果。新消息会接替上一条浮层，长消息的全文可在聊天和历史记录中查看。

点击聊天输入框旁的 **动效**，可为当前这条文字选择爆炸抖动、飞入、弹跳、淡入淡出、滑入滑出或无动效，也可以跟随设置。点击 **预览** 可在本机桌面播放当前草稿及所选动效。成功发送后，下一条文字默认跟随设置。房间成员的桌面浮层各自使用本机的停留时长。要在房间里传递逐条选择的动效，主机和接收方都需支持此功能。旧版仍可收发文字。

右键单击 Windows 系统托盘或 macOS 菜单栏图标，选择 **静音**。也可以在 **设置 → 桌面动画** 中切换。静音立即生效并在重启后保留。其他人发来的内容只在聊天和历史记录中显示，自己发送的文字、信号和表情仍可正常显示。

## 快捷键和本地数据

| 操作 | Windows | macOS |
| --- | --- | --- |
| 信号轮盘 | 按住 Alt 并用左键拖动 | 按住 Option 并用左键拖动 |
| 表情轮盘 | 按住 Ctrl 并用左键拖动 | 按住 Control 并用左键拖动 |
| 普通标记（默认关闭） | Alt + 单击 | Option + 单击 |
| 聊天 / 历史 / 设置 | Ctrl+1 / Ctrl+2 / Ctrl+, | ⌘1 / ⌘2 / ⌘, |

在「设置 → 桌面轮盘」中可分别开关两个轮盘。点击信号或表情旁的「重新录入」，直接按一个键盘键或鼠标侧键 4 / 5，然后保存。按 Esc 可放弃录入。两个轮盘同时启用时需使用不同的键。

在同一分类的 **表情轮盘布局** 中，可新增或移除页面。每页有八个方向槽位。把已有表情拖到另一个槽位可换位。点击或拖动表情库中的表情可以填入槽位，也可从文件管理器拖入一张 GIF、PNG、JPEG 或 WebP 图片，直接导入并填入目标槽位。可用 **交换位置** 跨页移动表情。点击 **保存设置** 应用布局。打开表情轮盘后，向下滚动进入下一页，向上滚动返回上一页，首尾循环。应用运行期间，再次呼出表情轮盘会回到上次停留的页码。松手发送当前页光标所指的表情，空槽位会取消本次发送。

键盘绑定使用左键拖动，先松开快捷键可取消。侧键绑定直接按住侧键拖动，松开同一侧键确认。两种方式都可以拖回中心取消。未打开轮盘的普通单击在松开时交给当前应用。轮盘打开后，系统光标照常移动，松开时按光标位置确认选择。鼠标移动、点击和滚动由轮盘接收；确认或取消时的松键也会被拦截。如果开启普通标记，单击绑定的信号侧键也会发送标记。

最近 600 条文字、信号和表情以 JSON 保存在本机，自定义表情保存在数据目录的 `emotes` 子目录。Windows 数据目录为 `%LOCALAPPDATA%\PingPingPing`，macOS 为 `~/Library/Application Support/PingPingPing`。

## 开发和验证

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked clients_join_by_endpoint_exchange_messages_and_notice_disconnect -- --ignored
```

最后一项需要本机 TCP 回环权限。模块说明和修改约定见 [AGENTS.md](AGENTS.md)。轮盘图像、音效和 WebP 动画来自 lolPing 素材，权利归原权利人所有。表情音效由 Ogg Opus 转成 PCM WAV。
