# 发布前检查 · 2026-10-08

## 结论与分支边界

本次检查对象为 `codex/test-bots`，基点 `850aadc`，包含已有未提交的 TLS 语音回退、诊断记录、设置页与 R2 分发改动。测试结果只适用于该工作区，不能代表当前主线。

核对远端后发现，`origin/main` 已到 `a5e0350163a2dc101fccf29311eefb778122c7ef`，最新正式发布为 0.3.3；本地 Cargo 产品版本仍为 0.3.1。主线已经交付独立语音进程、设置页、R2 更新与连接历史，但没有本分支新增的 `VoicePath` / `VoiceDatagram` / `Welcome.tcp_voice`。

因此，本次产物用于本地验证，不作为新的正式 0.3.1 发布。下一项开发应把 TLS 回退和诊断能力接入主线的独立语音进程及 IPC 边界，复用已经发布的设置、更新和连接历史功能；迁移后必须重新测试。不要直接用旧分支覆盖主线，也不要把已有 0.3.3 功能再次标成待发布。

## 本次修复

R2 发布脚本原先只确认说明页返回了内容，旧缓存、验证页和错误内容也可能通过检查。现在要求公开读取的说明页字节与本地生成的 `index.html` 一致；缺失、验证页或旧内容都会阻止推进 `stable.json`。新增回归测试覆盖这三种失败场景。当前远端主线也仍有此缺口，该修复可独立迁移。

README 路线图改为当前集成和实战验收任务，移除过期的“下一步 0.2.0”描述。

## 自动验证

Windows / MSVC，日志和预览保存在 `target/release-readiness-20261008/`。

| 检查 | 结果 |
|---|---|
| `cargo test --workspace --locked` | 589 passed / 0 failed / 13 ignored |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| 服务端 `--no-default-features` 严格 Clippy | 通过 |
| `cargo fmt --all --check` | 通过 |
| `python scripts/check-spdx.py` | 120 个 Rust 文件通过 |
| 更新分发 Python 测试 | 6 项通过 |
| Windows 安装包体积边界测试 | 6 项通过 |
| `cargo build --locked --profile dist -p client -p server` | 通过，耗时 2 分 58 秒 |
| Inno Setup 本地验收安装包 | 8,297,044 字节，低于本分支严格 10 MB 门禁和 60 MB 产品线 |
| 真实 GitHub 更新检查 | 生产 WinHTTP 的 `asks_our_release_repository` 通过 |
| 设置页软件预览 | 400 / 760 / 1000 宽度，导航与更新弹窗交互检查通过；37 张图片 |
| TLS 回退 / 诊断软件预览 | 400×360 / 760×520、100% / 200%，生成 8 张图片；抽查窄窗口更新详情和诊断长路径显示 |
| 完整通话页软件预览 | 80 组布局 / DPI 状态矩阵和附加指针、键盘检查通过，共 104 张图片 |

未将声卡、外网或性能忽略项改成通过。联网专项另行显式运行了一项，未改动默认忽略状态。软件预览不等于 Windows 原生跨屏 DPI 或真实设备验收。

安装包位于 `target/release-readiness-20261008/installer/gouhuo-setup-0.3.1.exe`。文件名和内嵌版本来自旧分支，包含未发布源码改动，仅供本地验收，不应作为正式 0.3.1 分发；本次没有执行安装或真人试听。

## 线上只读核对

GitHub 的 R2 所需变量和 Secret 名称均存在；未读取 Secret 明文，未修改线上配置。

官方 `stable.json` 实际返回 HTTP 200、schema 1、版本 0.3.3。说明页可访问，下载 Windows 安装包后核对大小与 SHA-256，均与清单及 GitHub 资产一致：

- 字节数：8,519,631。
- SHA-256：`39c715a94967afa9d70993e340a808e5665eee3e7d502b00135821a1555de56b`。
- GitHub 正式版：[v0.3.3](https://github.com/parz1/gouhuo/releases/tag/v0.3.3)。
- 本地下载保存为 `target/release-readiness-20261008/published-installer.exe`，没有执行安装。

这只验证当前机器线路，不代表大陆电信、联通、移动全部通过，也不代表本地 TLS 回退功能已发布。

## 后续验收

1. 在当前主线接入 TLS 回退与诊断，验证语音进程生命周期、IPC 状态、旧句柄失效及更新 / 回退边界。
2. 双机真实设备检查 UDP 阻断后互听、恢复后切回 UDP、闭麦和 PTT 队列撤销、诊断导出。
3. 用迁移后的安装包检查覆盖升级、身份与设置保留、更新入口、托盘和全屏游戏。
4. 完成真人音质、声学 AEC、完整嘴到耳延迟和游戏帧时间测量；高人数全员发声问题使用分机负载定位。

本次没有推送、打标签或发布，也没有变更用户身份、客户端设置和已安装程序。
