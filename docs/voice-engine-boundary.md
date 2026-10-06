# 声音内核独立迭代

对应 [#46](https://github.com/parz1/gouhuo/issues/46)。目标是同一个 UI 二进制使用不同兼容版本的声音内核。

## 当前实现

桌面安装包包含 `gouhuo.exe` 和 `gouhuo-voice.exe`。UI 通过 `client-process` 代理启动声音子进程；客户端 v0.3.2 随包内核为 **0.1.0**，正式独立内核为 **0.1.1**，IPC 为 **1**。声音内核可单独构建，UI 正式依赖树没有 Opus、APM 或 `voice-engine`。已实现独立发布查询、HTTPS 下载、签名校验、版本暂存、空闲切换和启动回退。2026-10-06 已完成正式签名配置、客户端与独立内核公开发布，以及真实 HTTPS 下载验收，见 [发布记录](release-0.3.2.md)。

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

资源测量脚本统计 UI 与声音子进程总开销。既有 CPU、内存和声卡性能数字来自拆分前，仍需重测。当前带正式公钥的双进程本地安装包为 8,364,816 字节，通过原有 10 MB 门禁，构建记录见 [测量记录](measurements.md)。

独立发布清单、可信签名、公钥固定、兼容版本选择、版本目录、空闲激活和启动回退的实现见下节。正式密钥配置、真实声卡设备生命周期与正式发布 HTTPS 验收已完成；人工人声／听感及真实声学 AEC 仍待后续验收。首版在通话、试麦和扫描结束后的安全空闲点切换；不承诺通话内无缝替换。

## 独立发布与安全切换

`client-process::update` 接受 Ed25519 签名的原始 manifest.json 字节；manifest.sig 为 64 字节签名的十六进制。清单固定 schema、平台、内核版本、IPC、服务器协议、UI 最低／最高版本、文件大小和 SHA-256。单清单最多 8 KiB、可执行文件最多 32 MiB，不解压归档。签名、公钥、兼容范围或哈希不匹配时不暂存、不执行。

版本安装到 `%APPDATA%\gouhuo\voice-engines\<version>`，state.json 保存 active、previous、pending、booting 和最高已接纳版本。目录名只能是严格的三段数字版本。完整文件写入并同步后才重命名成版本目录；状态指针用 Windows MoveFileExW 的 replace/write-through 或 Unix rename 原子替换。每次启动重新验证签名与文件哈希。失败回退不降低版本防重放下限。

UI 的编译期 `GOUHUO_VOICE_PUBLIC_KEY` 固定信任公钥；运行时配置和下载清单不能替换它。未配置公钥时使用安装包内核，不启用独立更新。packaging/windows/build.ps1 自动将随包内核版本写入编译配置，避免发布新 UI 时沿用旧基线。

配置公钥的 UI 在后台观察 pending。只有 Idle、无会话、无待执行生命周期命令及设备 RPC 时才退役旧进程；通话、试麦、扫描期间保留候选。旧进程完成 kill/wait 后，新进程必须通过 IPC 和清单版本一致性检查，再以有效快照确认启动。启动未确认或子进程退出会回退；已发送语音的会话继续要求重新认证，不能把密钥重放到替代进程。

离线发布工具：

```powershell
# 签名私钥由 secret manager / CI secret 注入 GOUHUO_VOICE_SIGNING_KEY，
# 不放在命令参数、文件或仓库里。GOUHUO_VOICE_PUBLIC_KEY 为对应的 64 字符十六进制公钥。
# 先将 voice-engine/Cargo.toml 版本改为 0.1.1 并单独构建；内核握手版本必须匹配清单。
cargo build --profile dist -p voice-engine --bin gouhuo-voice
cargo run -p client-process --bin gouhuo-voice-release -- sign target/dist/gouhuo-voice.exe 0.1.1 0.3.2 target/voice-release

# 暂存已下载并经过签名校验的版本；已有通话和试麦不会被打断。
cargo run -p client-process --bin gouhuo-voice-release -- stage "$env:APPDATA\gouhuo\voice-engines" $env:GOUHUO_VOICE_PUBLIC_KEY target/voice-release/gouhuo-voice-0.1.1-windows-x64.exe target/voice-release/manifest.json target/voice-release/manifest.sig 0.3.2
```

`.github/workflows/voice-release.yml` 响应 voice-v*，也支持通过 workflow_dispatch 指定现有内核标签重跑；检查标签与内核版本、测试真实进程、只编译内核、签署清单，创建 `--latest=false` 的独立草稿。正式密钥已配置为 GitHub secret GOUHUO_VOICE_SIGNING_KEY 与 variable GOUHUO_VOICE_PUBLIC_KEY；两者不匹配则发布构建失败。普通 UI release 使用同一公钥 variable；客户端构建和 docker-latest 显式排除 voice-v*。

### 自动发现与下载

配置公钥后，后台线程在启动 5 秒后查询固定仓库 `parz1/gouhuo` 的发布列表；成功后每 6 小时检查，失败按 1 分钟、5 分钟、30 分钟、6 小时退避。只考虑非草稿、非预发布的 `voice-v<三段数字版本>`，按数字版本降序选择兼容内核。列表最多 100 项、1 MiB，每次最多检查 8 个新候选。先校验清单签名、兼容范围和标签一致性，再下载清单指定的代码；暂存前再次核对大小、SHA-256 和版本下限。下载地址由固定仓库、标签和文件名生成，不使用发布索引提供的任意 URL。

Windows 使用 WinHTTP 的系统证书与代理。每跳都限制 HTTPS 和 GitHub／GitHub 发布资源域名，最多跟随 3 次重定向；不发送身份或发布凭证。每阶段超时 5 秒，单文件总期限 60 秒，读取时限制内存并检查取消。API 与 WinHTTP 行为分别见 [GitHub Releases API](https://docs.github.com/en/rest/releases) 和 [WinHTTP options](https://learn.microsoft.com/en-us/windows/win32/winhttp/option-flags)。

设置中的「检查更新」同时控制查询、下载和激活。关闭时取消正在进行的任务；快速关闭再打开也会使旧任务失效。已暂存候选保留，关闭期间重启也不激活它；已验证的当前内核仍可启动。重新开启后恢复查询与空闲激活。未配置公钥时不创建网络更新任务。

下载测试覆盖数字版本排序、兼容性筛选、签名拒绝、内容篡改、内存限制、取消和设置关闭后的候选保留；真实子进程测试覆盖暂存后的切换与回退。公开发布后另行运行 published_release_updates_idle_bundled_engine，通过真实 WinHTTP 下载正式签名内核，验证 0.1.0 → 0.1.1 空闲切换和更新后合成试麦。固定测试密钥仅用于 fixture。

### 可重复更新演练

先保留一个 `target/dist/gouhuo.exe`，然后运行 `./scripts/voice-update-rehearsal.ps1`。脚本在 `target/voice-update-rehearsal/<随机标识>` 中复制已跟踪源码，将副本内核的 patch 版本加一，并构建独立名称的测试可执行文件；真实仓库版本、锁文件和 UI 产物不改。原目录保留签名版本、状态和 `report.json`，方便核对；固定测试密钥不会写入正式配置。`-UiPath` 可指定另一个已构建客户端，Windows CI 使用 debug 客户端执行合成设备演练，并保存报告为 artifact。普通进程测试也从当前内核版本计算候选，独立内核升版无需同步改写测试常量。

演练将本地签名发布 fixture 交给生产查询／下载／暂存逻辑，用真实子进程、TLS 和加密 UDP 验证 0.1.0 → 0.1.1、通话与试麦期间延后切换、空闲激活、新内核通话、无法启动和握手版本不符时回退到 0.1.1、重启后缓存内核可用。报告记录父进程和 UI 前后相同的 SHA-256。它不启动图形 UI，不使用真实声卡，也不代表正式发布资源的 HTTPS 端到端验收。

增加 `-Hardware` 可运行同一路径的真实 WASAPI 验收：优先选择 Nova Pro 物理输入／输出，否则选择首个硬件端点，不修改系统默认设备或用户设置。额外断言更新前后的实际采集、渲染均可用，扫描设备时拒绝激活；重启后重新打开物理麦克风。报告包含设备、扫描结论及更新前后快照，不保存音频。2026-10-06 已在 Arctis Nova Pro Wireless 上通过功能验收；扫描仅有低电平底噪，实际人声、人工听感、AEC、声学延迟和长期稳定性仍需另行验收。

### 正式启用与人工验收

正式密钥已配置到 GitHub secret `GOUHUO_VOICE_SIGNING_KEY` 与 variable `GOUHUO_VOICE_PUBLIC_KEY`。客户端 v0.3.2 已固定正式公钥并公开发布；之后发布兼容的 `voice-v*` 内核即可。生产私钥不得使用 fixture seed，生产公钥不能取自测试向量。发布时复核 CI 生成的草稿与签名，实际 HTTPS 下载链路在隔离更新存储目录复验；首个正式发布 voice-v0.1.1 已完成该验证。

首次配置工具为 `./scripts/configure-voice-signing.ps1`，需要 Windows、PowerShell 7、Node.js、已认证的 gh，以及已构建的签名工具和 dist 内核。它生成 Ed25519 正式密钥，先验证 DPAPI 加密备份、Rust 签名及独立 Node 验签，再以标准输入上传私钥 Secret，并读取确认公钥 Variable。检测到已有任一配置就停止，不自动覆盖或轮换。私钥不输出、不进命令参数、不保存为明文文件或进入仓库；加密备份位于当前用户 `%LOCALAPPDATA%\gouhuo-release-signing\parz1-gouhuo\voice-signing-key.dpapi`，目录仅授予当前用户访问。

DPAPI 备份绑定当前 Windows 用户，不能当作跨机器备份；换机前需通过安全方式迁移密钥。以后公钥变化需要重新发布客户端，不能只改 GitHub Variable。手动本地打包先从 GitHub Variable 读取公钥到当前进程环境，release workflow 已自动注入。

人工试听另行安排。使用同一个带公钥的 UI，在更新前后分别说话、试听，确认接收端听感、PTT／闭麦和双讲；外放场景再确认真实回声消除。真实设备可开流、合成信号的 AEC 回归通过均不能替代这些结论。声学延迟和长期稳定性也需要单独测量，不沿用拆分前的单进程数据。
