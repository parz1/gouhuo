# TLS 语音回退与诊断主线迁移 · 2026-10-22

## 保存与基线

- 原工作区全部已跟踪和非忽略未跟踪改动已保存到 `codex/tls-local-backup-20261022`，提交 `d4bdaa8`；原 `codex/test-bots` 分支仍在 `850aadc`。
- 集成分支：`codex/tls-voice-integration`，基于本次 fetch 后的 `origin/main`：`a5e0350163a2dc101fccf29311eefb778122c7ef`（正式应用 0.3.3）。
- 沿用主线的独立语音进程、设置页、R2 更新及连接历史；重叠代码按主线保留。已有 R2 说明页字节校验修复独立保留。
- 没有推送、打标签、发布、安装，也没有修改用户身份、设置或线上配置。

## 集成结果

语音服务端与 `client-core` 保留原 TLS 控制连接；设备、APM、Opus、UDP、语音序号和健康探测仍在声音内核。
IPC v2 批量中继已加密数据报、路径请求/确认、队列撤销与丢弃计数。每批最多 32 个包；队列有界，
100 ms 过期截止时间贯穿本地 IPC 与 TLS 待发队列，不因进入另一进程而重新计时。

主进程优先发送生命周期与控制命令。闭麦、PTT 松开及会话回收立即撤销 TLS 待发队列；
来自旧进程、旧生命周期请求、旧控制版本的回复不会进入当前 TLS 会话。轮询不重新应用音频控制，避免持续清掉保活。
设备替换保留已确认的 TLS 路径，语音序号继续由原认证会话的分配器递增。进程更换要求重新认证。

诊断值类型移入 `voice-types` 与运行层 API，前端无需链接 Opus/APM。快照携带 APM 状态、设备 I/O 进度、
UDP/TLS 健康与接收质量；进程间序列化有专门回归测试。自动时间线保留最近 5 分钟、最多 1800 条，
记录实际内核版本、IPC、PID 和重新认证标志，并保留暂停/继续、后台脱敏导出及 GUI 可执行文件 SHA-256。
主线连接历史继续独立保留。另修正即时快照中语音健康已恢复但运行阶段尚未同步的短暂矛盾。

## 兼容边界

应用版本仍为 0.3.3；本地声音内核版本为 0.1.2，IPC 从 1 升至 2，两者均未发布。
必须配套构建客户端与声音内核；历史 IPC v1 内核不兼容，签名更新清单也需 IPC v2。
服务端控制协议仍为 v2，TLS 回退依赖可选能力协商，旧服务端继续使用 UDP；存档格式不变。

IPC 批处理会增加调度等待，本次没有重新测定或承诺公网延迟、实际听感、设备兼容性或服务器容量。

## 验证

验证日志保存在本地 `target/tls-integration-20261022/`。Windows / MSVC：

| 检查 | 结果 |
|---|---|
| `cargo test --workspace --locked` | 657 passed / 0 failed / 16 ignored（含文档测试，共 45 组结果） |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `cargo check -p client --locked` | 通过；独立前端构建，正常依赖树无 Opus、sonora 或 voice-engine |
| 服务端 `--no-default-features` / 全目标严格 Clippy | 通过 |
| `cargo fmt --all --check` / `git diff --check` | 通过 |
| `python scripts/check-spdx.py` | 141 个 Rust 文件通过 |
| `python -m unittest discover -s packaging/updates -p 'test_*.py'` | 6 项通过 |
| `call_ui_preview --fallback-only` | 400×360 / 760×520、100% / 200%，8 张图片；抽查回退状态和长路径显示 |
| `call_ui_preview --weak-network-only` | 同尺寸/DPI，12 张图片；暂停、继续与暂停期间导出指针交互全部通过，抽查暂停布局 |

界面预览使用软件渲染，并非原生 Windows 跨屏 DPI 或真实设备验收。
最后只读复核远端主线仍为上述 `a5e0350`。

新增真实双子进程、真实 TLS 服务端和加密 Opus 回归覆盖：阻断 UDP 后 TLS/UDP 混合互通、
初始闭麦、PTT 松开/恢复、闭麦撤销、设备替换、UDP 自动恢复、进程不因路径变化重启、
进程回收后旧句柄失效及重新认证后保持闭麦。声卡采用合成设备，不代表真人声学验收。
进程测试连接辅助函数同时保留并消费事件接收器，避免另一成员加入时因丢弃接收器而意外放弃控制连接。

原有真声卡、联网和独立发布演练等忽略项继续保留。真实 USB/蓝牙、声学 AEC、真实双机嘴到耳延迟、
游戏帧时间、覆盖升级安装及小规模真人开黑仍待独立验收。
