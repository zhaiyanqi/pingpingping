# PingPingPing Agent Guide / 开发代理指南

## Scope / 项目范围

- Windows x64 is the primary desktop target. Keep macOS working when changing shared behavior. Windows x64 是首要桌面目标，修改共享行为时也要维护 macOS 支持。
- GPUI Kit owns chat, history, and settings. Native platform backends draw desktop overlays. 聊天、历史、设置使用 GPUI Kit，桌面悬浮层由原生平台后端绘制。
- Rooms use local IPv4 and TCP ports. Keep the host and guest workflow in the app and preserve local operation without a room. 房间使用局域网 IPv4 和 TCP 端口，保留应用内创建与连接流程以及无房间时的本地功能。

## Source Map / 代码结构

| Path | Responsibility / 职责 |
| --- | --- |
| `src/main.rs` | GPUI and platform initialization / GPUI 和平台初始化 |
| `src/ui.rs` | GPUI entities and windows / GPUI 实体与窗口 |
| `src/gesture.rs` | Shared gesture state and sector selection / 共享手势状态与扇区选择 |
| `src/platform/windows.rs`, `src/platform/macos.rs` | Global input and desktop overlays / 全局输入和桌面悬浮层 |
| `src/platform/playback.rs` | Shared animation overlay, WebP decoding, and timeout / 公共动画浮窗、WebP 解码与超时 |
| `src/platform.rs`, `src/assets.rs` | Shared media playback and embedded asset catalog / 共用媒体播放与内嵌素材目录 |
| `src/model.rs` | Settings, records, and session state / 设置、历史和会话状态 |
| `src/room.rs` | LAN TCP transport, frame validation, and forwarding / 局域网 TCP 传输、帧校验与转发 |
| `script/`, `packaging/` | Build scripts and app metadata / 构建脚本与应用元数据 |

## Working Rules / 修改约定

- Explain non-obvious behavior with paired English and Simplified Chinese comments. 用成对的英文和简体中文注释解释不直观的行为。
- Preserve input pass-through, focus, and the final release coordinates. Keep I/O and decoding out of input callbacks. 保证输入穿透、焦点不被抢占，并按松键时的坐标确认。输入回调中避免 I/O 和解码。
- Retain GPUI state and subscriptions in the owning entity. Use the GPUI Kit coding and design guides for window and component changes. 在所属实体中保留 GPUI 状态和订阅。修改窗口或组件时遵循 GPUI Kit 的开发与设计指南。
- Keep asset indices stable for the room protocol and history. Validate incoming frames before they can reach rendering or storage. 保持素材索引与房间协议、历史兼容。消息须通过帧校验后才能进入渲染或存储。
- Keep `Cargo.lock` tracked. Update both README files when build steps or user workflows change. Keep build outputs and personal data out of Git. 提交 `Cargo.lock`。构建或使用流程有变化时更新两份 README。构建产物和个人数据不纳入 Git。

## Commits / 提交约定

- After completing a code change and running the relevant checks, commit the files for that task before reporting completion. Keep unrelated changes out of the commit. 每次完成代码修改并运行相关检查后，在汇报完成前及时提交本次任务的改动，避免混入无关改动。
- Write every commit subject on a single line in the format `<English summary> | <简体中文摘要>`. Use exactly one literal ` | ` separator (one ASCII space on each side of `|`) so changelog scripts can split the two summaries. 每次提交的标题使用单行双语格式，英文在前，简体中文在后。分隔符 ` | ` 只能出现一次，竖线两侧各有一个半角空格，便于脚本拆分生成更新日志。
- Example / 示例 `Fix the settings crash | 修复设置页闪退`

## Checks / 验证

- `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`
- `cargo test --locked clients_join_by_endpoint_exchange_messages_and_notice_disconnect -- --ignored` requires TCP loopback access / 需要 TCP 回环权限。
- Run `script\build-windows-release.bat --no-pause` on Windows for the full MSVC Release build. Verify pointer pass-through, focus, mixed-DPI displays, and two-device LAN chat when changing those paths. Windows 完整 MSVC Release 构建使用此脚本。相关改动需验证鼠标穿透、焦点、混合 DPI 和双设备局域网聊天。
