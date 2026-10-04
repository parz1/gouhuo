# 声音内核独立迭代

对应 [#46](https://github.com/parz1/gouhuo/issues/46)。目标是同一个 UI 二进制使用不同兼容版本的声音内核。

## 当前实现

桌面安装包包含 `gouhuo.exe` 和 `gouhuo-voice.exe`。UI 通过 `client-process` 代理启动同目录的声音子进程；内核独立版本目前为 **0.1.0**，IPC 为 **1**。声音内核可单独构建，UI 正式依赖树没有 Opus、APM 或 `voice-engine`。进程与构建产物已分离，自动下载安装、签名验证和激活回滚仍待实现。

| 所有者 | 内容 |
| --- | --- |
| UI | TLS 控制连接、长期身份、名单、设置、热键、本地意愿、通话投影 |
| 声音子进程 | 声卡、COM 音频对象、APM、Opus、UDP、序号分配、提示音、TTS、枚举与扫描 |
| voice-types | 发送模式、语音事实和音量上限；默认无依赖，可选 serde |
| client-runtime | API、投影和恢复策略；in-process 启用 worker，ipc 启用线格式 |
| client-process | 缓存快照、立即应用本地意愿、有界命令、进程代次和退出监督 |

工作区依赖关闭 client-runtime 默认 feature。声音可执行文件、机器人和语音测试显式启用 in-process。Cargo 会合并同次构建的 feature，因此打包分别构建 UI 和内核，并对三个前端 crate 分别检查正式依赖树。身份和热键仍使用 voice-core 的基础模块。

## IPC 与生命周期

使用父进程创建并交给子进程的 stdin/stdout 管道，没有可发现的命名端点。先交换版本握手，再传长度前缀的 JSON 帧，单帧上限 256 KiB。密钥只进入私有管道，不进入启动参数、诊断或日志。PCM、COM 对象、Rust trait 和设备句柄不跨进程；声卡对象在打开它的线程上使用和销毁。

请求携带生命周期 ID、意愿 revision 和完整控制状态。UI 立即反映闭麦、关闭声音和 PTT release，并清除失效的发送事实；只有匹配当前进程代次、请求和 revision 的内核快照才能进入缓存。PTT 按下不冒充实际发送。高频控制与统计只保存最新值；辅助队列最多 16 项、未完成 RPC 最多 8 项，用户音量最多 2048 项。切换会话丢弃旧提示音及未开始的扫描。

管道读取不等待 DNS、声卡初始化或扫描。扫描在 worker 完成退出后打开设备；新生命周期会取消扫描，等扫描线程释放设备后再启动音频。提示音和 TTS 在内核的原有播放/AEC 路径运行。

控制确认超过 500 ms、写入阻塞超过 2 s、有效快照超过 5 s 或 IPC 断开都会退役子进程。旧进程 kill/wait 完成后才能启动替代进程。正常退出发送 Shutdown；父管道 EOF 使内核关闭音频并退出，父监督线程提供有界退出和强制回收。

## 密钥和恢复

同一内核会话内更换设备复用 `Arc<VoiceSequences>`，上行／下行计数持续递增。相同密钥不能以不同会话上下文重建计数器；内核和代理都拒绝复用已退役的方向密钥。

崩溃或主动更换子进程后，UI 先重新建立 TLS 认证，获取全新会话密钥，再提交 StartVoice。代理保存已使用密钥的哈希和进程代次。不得把旧 StartVoice 自动重放给新的内核进程；手动更新也必须遵守这个规则。

## 验证与剩余工作

```powershell
cargo check -p client-runtime --no-default-features --lib --locked
cargo test -p client-runtime --no-default-features --locked
python scripts/check-voice-boundary.py
cargo test -p voice-engine --test process --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

实际子进程测试使用合成采集与空播放，经过真实 TLS 认证和加密 UDP，验证初始闭麦、PTT、管理员闭麦、设备重建序号连续、父管道 EOF、版本不兼容，以及退役后拒绝旧密钥并通过新认证恢复。它们不代替真实 WASAPI、设备扫描、AEC 和长时间运行验收。

资源测量脚本统计 UI 与声音子进程总开销。既有 CPU、内存和声卡性能数字来自拆分前，仍需重测。双进程本地安装包为 8,325,955 字节，通过原有 10 MB 门禁，构建记录见 [测量记录](measurements.md)。

后续实现独立发布清单、可信签名、公钥固定、兼容版本选择、下载校验、版本目录、空闲激活和原子回滚。首版在通话、试麦和扫描结束后的安全空闲点切换；不承诺通话内无缝替换。
