# 篝火 0.3.0：验证记录

本记录建立并更新于 2026-10-03（Asia/Tokyo）。下文候选标识、产物、运行状态与“尚未发布”流程声明，均为当地发布前各阶段的历史快照，不作为现行发布限制。
用户已明确授权正式发布 0.3.0；最终公开状态以 [v0.3.0 Release](https://github.com/parz1/gouhuo/releases/tag/v0.3.0) 和 [GitHub Actions](https://github.com/parz1/gouhuo/actions) 为准，发布前最终检查另见末尾附录。
表中的“待测 / 待确认”没有执行通过含义；只有实际命令、产物或操作证据才能改为通过。
发布流程与兼容性依据见 [0.3.0 版本说明](release-0.3.0.md)。

Header 横向留白细化发生在 SelfDock `dock-refine` 原生产物验收之后，本轮仅 `header.slint` 变化。
该轮 176 文件清单、客户端 `3A58C3…7A3B1`、安装包 `EADDF2…2E55` 和 PID 25272 / 39204 均为历史记录。
本轮 80 张软件快照与原生 Header 操作通过；新清单、dist / 安装包与双原生通话实例已核验。SelfDock 阶段的 73 项客户端测试与 Clippy 保留历史，本轮没有重跑这些项目或全工作区。

## 发布前候选标识（历史快照）

| 项目 | Header 阶段发布前记录 |
|---|---|
| 产品版本 | 根 Cargo.toml 与九个工作区包的 Cargo.lock 条目已更新为 0.3.0；保持候选状态。 |
| 冻结源码 / commit | 在包含 #45 的未提交工作区开发；HEAD `a85dc9e39bb5acace93cfee7d4821bbd3654ce41` 只是基底，工作区 dirty、未提交 / 推送。本轮 `header-spacing-source-files.sha256` 共 176 文件，SHA-256 `80D26D6A6E0ECDF00828A56094240DEDE2A035B5FAB21BFC873FBC1FF96660DB` 已独立复核；相对 `dock-refine` 仅 `header.slint` 变化，相对 `source-final` 共三处代码文件变化。SelfDock 清单 `98AB0191D82DC3CAE3EF5421F6765999A18CC0A32D0FA5C95AB47BD38541CC57` 与旧清单 `8C0F926BF9A05FD46DBFE1EAABADFC62672BB0EBBF57F9520B21A4663E1A8DFF` 保留历史；发布 commit 尚未生成。 |
| 当前构建日期与时区 | 本轮执行日期 2026-10-03，Asia/Tokyo。Header 修改后的客户端 dist 构建 3m04s、打包 4.54s 均成功；176 文件重检全部匹配，产物字节数、PE / 哈希及新原生 A / B 运行标识已核验。上轮 Windows 全工作区结果仍保留历史。 |
| 操作系统 | 只读工具盘点：Windows NT 10.0.26200.0。实际测试系统信息待根代理确认。 |
| Rust / Cargo | 根代理实际执行 Rust 1.96 / Cargo 1.96，`stable-x86_64-pc-windows-msvc`；未使用 1.92 / 1.91 / 1.80 工具链单独验证。 |
| MSVC / Windows SDK | VS 18 BuildTools 与 `vcvars64.bat` 存在；实际编译工具和 SDK 版本待记录。 |
| CMake | VS BuildTools 的 `Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe` 存在，不在当前 PATH；`win-buildenv.ps1` 可设置 CMAKE 与 `CMAKE_POLICY_VERSION_MINIMUM=3.5`。 |
| Inno Setup | 精确文件 `C:\Users\admin\AppData\Local\Programs\Inno Setup 6\ISCC.exe` 存在，1,456,272 字节；不在 PATH。本轮重打 4.54s、退出 0，PE 0.3.0，8,182,762 字节；SelfDock 4.93s / 8,182,967 字节及更早 4.88s / 8,181,974 字节保留历史。 |
| PowerShell | 只读盘点 7.6.5。 |
| Linux / Docker | 实际 daemon 返回 `linux 28.5.1`；本地 Docker Linux / WSL2 6.6.87.2、x86_64，Rust / Cargo 1.96.1、CMake 3.25.1。逐帧计时 fixture 修正后最终 12 个验证步骤全通过，容器退出 0；不是 GitHub CI / Linux GUI 或声卡结果。先前沙箱只读权限失败已被后续实际运行取代。 |
| GitHub / 发布权限 | 官方 0.2.2 服务端已下载并由 API 资产元数据核验。发布前只读 API 已确认仓库为 public、默认分支 main，当前登录账号具有 admin / push 权限；检查时 v0.3.0 标签与 Release 均不存在。GHCR 实际写入结果以发布 job 为准。 |

Slint 1.18.1 要求 Rust 1.92，client manifest 已单独声明 1.92；voice-core 声明 1.91，
工作区基础声明保留 1.80。Rust 1.96 本轮通过不表示这些较低工具链及全部依赖组合已实测；MSRV CI 仍待建立。

冻结清单由 `rg --files --hidden crates .cargo Cargo.toml Cargo.lock packaging/windows .github/workflows LICENSE`
枚举构建相关文件，排除 ignored，逐文件 SHA-256 后保存为 `target/release-0.3.0-validation/source-files.sha256`。
docs 和运行数据不属于这份构建清单；不能将基底 HEAD 单独当作产物的完整源码标识。

## 只读源码核对

| 检查 | 结果 | 依据 |
|---|---|---|
| 旧版本基线解析 | 已核对源码 | `v0.2.2` 为 annotated tag；`git rev-parse 'v0.2.2^{}'` 得到 `9f70b87b1cbd192754e2e32855bb91c99144ed67`。tag object `23ea66a3917c402b47258292ddbcacbbd8a4c3cf` 不是源码 commit。 |
| 控制协议与语音格式 | 源码一致，旧服务端认证 / 入页冒烟通过 | 相对 v0.2.1 / v0.2.2 protocol 只有 `crypto.rs` 的弃用说明与 allow 属性；协议仍为 2。混合版本双向声音仍未验证。 |
| 存档 / 邀请 / 加入页 / 身份 | 已核对源码，升级运行待测 | SQLite 两条迁移不变；invite 1、discovery 1、identity 1。相对 v0.2.2 没有存档迁移或身份生产逻辑修改。 |
| 设置格式 | 源码一致，测试实例退出保存通过；升级 / 回退待测 | settings diff 添加后台写入器及测试，没有修改 path、parse 或 serialize 格式。原生 A/B 正常退出后，测试昵称、PTT 键和关闭提示音设置实际存在。 |
| 默认草稿策略 | 已核对工作流 | release.yml 使用 `--draft --verify-tag`，公开发布前不会经此流程提升 latest。 |
| 发布权限 | 声明与仓库权限已核对 | Windows contents write；Docker contents read / packages write；docker-latest packages write。发布前只读 API 确认仓库 admin / push 有效，未以资产可读推断 GHCR 实际写入通过。 |
| 自动测试与发布工作流分离 | 已核对工作流 | redline 做全 workspace 测试；release 不依赖 redline，必须人工核对同一候选提交结果。 |
| 完整安装包门禁 | 本轮产物通过 | 本轮包 8,182,762 字节，严格小于 10 MB 回归闸与 60 MB 产品线，PE / SHA-256 已核验；check-size 从 redline.rs 读唯一阈值。历史包 8,182,967 / 8,181,974 字节不作本轮产物；打包通过不表示实际安装通过。 |
| Windows 构建锁定依赖 | 已核对源码修改 | build.ps1 与 release.yml 的 Windows 服务端 build 均已加入 --locked，Dockerfile 原已有 --locked。 |
| 安装身份与权限 | 已核对 .iss，实际安装待测 | AppId 不变、PrivilegesRequired lowest、按用户路径、HKCU 协议、保留 APPDATA 身份。 |

## 旧版本基线取得

优先使用 v0.2.2 正式 Release 的安装包和服务端资产，记录资产名、下载来源、字节数、SHA-256 和 PE 版本。
本轮已下载并通过 API 资产元数据核验官方 0.2.2 Windows 服务端：3,657,216 字节，
SHA-256 `4FD4E74E3294E5F2FDD361A96EA6ED96F2D98CBA5B74B0431425CAEDBB3B3E27`。
它用于隔离本地 21944 的新客户端连接冒烟；未作为完整混合通话验收。
没有重新下载核验官方 0.2.2 安装包。历史 `docs/measurements.md` 记录正式包为 8,023,011 字节，仅作历史参考。

现有本地 `target/installer/gouhuo-setup-0.2.2.exe` 为 8,039,324 字节，时间为 2026-10-02；
这是历史本地重打包，不能标为正式资产。只读盘点到的旧 dist 可用于环境旁证，未经来源核对不能作为正式版本验收。

若无法取得正式资产，可从 `v0.2.2^{}` 的源码 commit 创建单独基线 checkout / 构建目录，使用该提交原有
Cargo.lock、相同机器与构建 profile，记录确切构建环境及任何构建适配。基线不得覆盖候选工作区和共享 target/dist。
自行编译的基线写为“源码重建 0.2.2”，与“GitHub 正式资产”分开。UI 结构变化的性能对照须保留相同人数、设备、窗口大小与渲染器。

## 自动验证

本表保留 0.3.0 各次源码的实际结果，先前单包或 UI 示例结果不混入总数。
`source-final` 的 556 项工作区汇总属于 SelfDock / Header 最新修改前的历史验证；本轮没有重跑工作区或客户端 73 项测试 / Clippy。
原始日志目录为 `target/release-0.3.0-validation/`，属于本地忽略的验证产物；发布前应保留或归档必要的非敏感日志。
本记录不包含用户身份、邀请凭据或服务端启动输出。

| 门禁 | 状态 | 命令 / 日志 / 结果 |
|---|---|---|
| 格式 | SelfDock 阶段通过 | 根代理执行 `cargo fmt --all --check`，退出 0；上轮 `source-final-fmt.log` / `source-final-status.tsv` 的 1.20s 记录保留历史。Header 阶段仅修改 Slint 布局。 |
| 工作区 Clippy | 上轮源码通过 | 历史 `source-final-clippy.log`：`cargo clippy --workspace --all-targets --locked --offline -- -D warnings`，退出 0，35.39s；先前 `clippy-final.log` 47.94s 另保留。 |
| 工作区全部测试 | 上轮源码通过 | 历史 `source-final-tests.log` / `source-final-status.tsv`：`cargo test --workspace --locked --offline`，退出 0，120.08s；27 个 suite / doc-test 汇总，556 passed / 0 failed / 13 ignored，包含九个工作区包。独立读日志汇总与根代理统计一致；先前 `workspace-tests.log` 留作历史。 |
| Quality probe 示例测试 | 通过 | `quality-tests.log`：1 passed / 0 failed / 0 ignored，已执行 `cargo test -p voice-core --example quality-probe --locked`。 |
| 许可头 | 上轮通过 | 根代理执行 SPDX 检查，112 个 Rust 文件通过，包含新增测量 helper；早期 111 文件结果留作历史。SelfDock 阶段修改既有 Slint 布局和 UI 预览 Rust 断言，Header 本轮仅修改 Slint，没有新增 Rust 文件。 |
| 差异空白检查 | 文档本轮通过；整体上轮通过 | 本轮三份更新文档的 `git diff --check` 退出 0；整体历史 diff 检查另保留，不宣称新增整体重跑。 |
| Windows 客户端独立目标 | 上轮通过 | `client-check.log`：`cargo check -p client --all-targets --locked`，33.88s，避免 workspace feature 统一掩盖遗漏。 |
| Windows 服务端无 web | 通过 | `server-no-web-clippy.log`：`cargo clippy -p server --no-default-features --all-targets --locked -- -D warnings`，6.25s。 |
| 体积门禁边界 | 通过 | 根代理执行 `packaging/windows/test-size.ps1`，6 项通过。 |
| dist / 安装包 | 本轮通过 | `header-spacing-dist.log`：锁定依赖的客户端 dist 构建成功，3m04s；`header-spacing-installer.log`：成功，4.54s。`header-spacing-artifacts.json` 与更新的 `artifacts-final.json` 记录当前 PE / 字节 / SHA-256，176 文件重检一致；`header-spacing-pe-dependencies.log` 无外部 CRT。服务端未变，SelfDock / source-final 构建结果保留历史。 |
| Header 本轮布局与操作 | 软件预览与原生操作通过 | `header-spacing-matrix.log`：完整 80 张软件快照与 pointer / keyboard 断言通过；dev 编译 27.86s，原生 fixture 编译 24.71s。原生 760×520 / 400×360 的右边界对齐及宽窗口 More → Escape 焦点返回直接观察通过。未使用真实音频，不能记为 VAD 发送验收。 |
| SelfDock 上一阶段布局与操作 | 软件预览通过 | `dock-refine-matrix.log`：80 张 Slint 软件快照，覆盖逻辑断点 / DPI 与 pointer / keyboard 操作断言，退出成功。未使用音频、网络或设置；不能记为原生或真实 VAD 发送验收。 |
| Windows 客户端 SelfDock 阶段定点复测 | 历史通过 | `dock-refine-client-tests.log`：73 passed / 0 failed / 3 ignored，测试 2.06s；`dock-refine-clippy.log`：客户端 all-targets Clippy `-D warnings` 通过，32.54s。Header 阶段没有重跑，两者不冒充完整工作区重跑，不累加到历史 556。 |
| Linux 内核和运行层 | 最终本地验证通过 | `linux-core/status-timing.tsv` 12 步全 PASS、容器退出 0：isolated 5 / 5（35.0–35.6ms、peak 0.998）、标准 parallel 两次各 23 passed / 0 failed / 0 ignored、完整 core 281 passed / 0 failed / 2 ignored、all-targets Clippy `-D warnings` 通过。check 与 runtime / latency 独立目标亦已通过；均 `--locked`。保留先前失败，修正的是测量 fixture。 |
| Linux 服务端 / Docker | 默认 / 无默认功能验证通过；发布镜像待构建 | 默认功能 check 0.82s / clippy 1.03s、测试 109 passed / 0 failed / 0 ignored，包含 Web HTTP 集成。无默认功能 check 3.62s / clippy 2.72s、quality-probe 1 项和隔离容器运行已通过；未使用生产卷或生产 .env。正式发布服务端镜像尚未构建 / 推送。 |
| 专用协议延迟专项 | 未执行 | 专用 Windows 机器 `latency-probe --seconds 30 --assert-gate`；不新增为本轮发布门禁，不作性能改善声明。 |

工作区汇总包括 client 73、client-core 106、client-runtime 31、device-probe 5、latency-probe 15、
protocol 59、server 109、transport 16、voice-core 142 项通过。13 项忽略包括客户端场景帧成本 / 外网更新检查 3 项、
voice_pipeline 真设备测试 1 项、voice-core 声学 / 实时节拍 / 热键注入 / 设备扫描 / TTS 等 8 项及 protocol 文档示例 1 项。
这些忽略项不算执行成功。

上轮菜单焦点与测量 fixture 修改后独立重跑：`client-tests-final.log` 为 73 passed / 0 failed / 3 ignored，2.80s；
`voice-pipeline-final-windows.log` 为 23 passed / 0 failed / 1 ignored，16.29s。
这些是定点复测，不再次累加到 556 的工作区汇总。
先前两次客户端测试因本轮自有预览 exe 被占用而报 LNK1104；停止预览后上述重跑通过，属于验证进程占用，不记录为产品运行故障。

Linux 首轮镜像 `gouhuo-linux-validation:0.3.0-runtime-20261003-01`，digest
`sha256:5bbe9c736360f37998b5051ebcbfc84db8213e29900949404b3be00fb853db53`。
它是旧冻结源码验证快照，不能冒充待复测修复或最终发布镜像。

Linux 历史额外复测保留在 `linux-core/status-regression.tsv`：旧源码 isolated / parallel 本次均通过，
仅修正启动顺序后 isolated 第一次通过、第二次失败，parallel 两次均 23 项通过，完整 core 测试和 clippy 通过。
`post-isolated-2.log` 中 correlation peak 0.377，但按首播放帧起点与 samples / fs 算出 0ms，低于 26.5ms floor，回归汇总退出 1。
原 correlation 约 0.05 失败和本次下限失败均保留，不能只用末次完整 suite 通过覆盖。
这轮仅先启动接收方 render、再启动发送方 capture，没有放宽相关性或延迟阈值，尚未修复时钟推算。
当时 `voice_pipeline.rs` 快照 SHA-256 `f3ef00c11ed07fa3fce5c54e09542ac139f7f93d0a2791eac015d6e506c2c016`，
原快照 `3cb9d8d04baeb9dcee065dcdbe6c7c4917356375c8b3f4f0f4c8ad3b379c526a`。

最终修正只改 `crates/client-core/tests/voice_pipeline.rs` 和新测试 helper `tests/support/measured_audio.rs`：
独立 capture / render Ticker 会迟到或跳 tick，不能用首帧时间加 `sample_index / sample_rate` 推算后续采样时刻，
最初只搜非负 sample lag 也依赖线程启动顺序。现在逐帧记录真实 `Instant`、采样位置与 late / skipped，
按物理 0–300ms 范围搜索，用命中帧的真实时间和帧内偏移测延迟；实际源帧与播放尾音完成后停管线，再做相关。
保持 200ms chirp 窗口、peak >0.3、intrinsic floor 减 2ms、上限 <150ms 和默认 adaptive jitter，未修改生产代码或放宽阈值。
最终 5 次单测与 2 次标准 parallel 均通过；第二次 parallel 双方 late / skipped 均 7 / 7，仍为 35.4ms、peak 0.998。

最终测试文件 SHA-256 为 `8ff814ac5f0789668344adb144f58e754579da07b251c6d1f22800e5485e6fb8`，
helper 为 `d62159c90237fb01f9ace0e57d554b24b88bae9a9864933f968f77be18beae01`，均与未修改的当前测试文件及上轮清单一致。
最终镜像 `gouhuo-linux-validation:0.3.0-runtime-20261003-04` digest
`sha256:f8cbc8a1774ef6874ae6858d50e454fa7b11b61ac42860a1b7c5394e9118f6c5`，复用首轮生产源码底座，仅叠加两个测试文件。
实际命令、各轮日志和源标识见本地忽略产物
[`linux-core/README.md`](../target/release-0.3.0-validation/linux-core/README.md)。容器限制 4 CPU / 4GB，未挂载声卡或发布端口。
这些是受控合成音频、真实 TLS / UDP / Opus 的本地结果；35ms 数字不含硬件或 APM，不能写成声学或 GitHub CI 验收。
SelfDock 阶段调整 Slint 布局与 UI 预览断言，本轮仅调整 Header Slint 布局；内核、runtime 和上述 Linux 测量文件未变，Linux 结果仍覆盖同一内核源码，未追加一次未执行的 Linux 重跑。

独立 Release 集成：`call-session.log` 中 `real_tls_udp_call_lifecycle_and_projection` 1 项通过，运行 1.92s。
测试使用真实 TLS / UDP / Opus 和受控合成音频后端，覆盖 PTT 按下 / 立即松开、VAD 尾音、静音 / 关闭声音回声、
TCP 重连与新会话密钥、过期候选、设备替换、离开再加入以及延迟 Stop 不停止新会话。
管理员禁言通过 Runtime 约束注入，未调用管理员禁言控制协议进行验证。
远端说话状态清除已断言，未抓取并验证 TERMINATOR 包头；捕获线程按所有者释放、VoiceRuntime drain 和 Hub 用户数归零已断言。
本地监听随测试进程结束；这是同机软件链路验证，不是双机声学验收。

## 发布产物

以下是 Header 修改后的发布前本地产物。该阶段 `artifacts-final.json` 已更新，独立证据为
`target/release-0.3.0-validation/header-spacing-artifacts.json`；根代理重检 176 文件清单一致，
产物字节数、PE 版本与 SHA-256 已核验，本文只读复核字节数、PE 和记录一致。当时仍未安装、提交、打标签、推送或公开发布；最终公开资产以 Release 为准。

| 产物 | 构建 / PE 版本 | 字节数 | SHA-256 | 状态 |
|---|---|---:|---|---|
| `target/dist/gouhuo.exe` | PE 0.3.0 | 17,956,352 | `D656B7FE238FE7F4107A81E39D08CDED674A0FD1AEFEED6EBE8D175CA30A2A78` | 本轮重建、版本 / 字节 / 哈希通过 |
| `target/dist/gouhuo-server.exe` | 无 PE 版本资源 | 3,694,080 | `665E649F7B2434AC2829D7282DA589D91FB264E3B5B39D66015C3816D237D13C` | 本轮核验通过；服务端源码与字节未变 |
| `target/installer/gouhuo-setup-0.3.0.exe` | PE 0.3.0 | 8,182,762 | `07D817D3A6E5ACD4CFA20AA4534E3A46B6FB8E83CF0544306614A0A318E1B98B` | 本轮严格 <10 MB 与版本 / 字节 / 哈希通过；实际安装未测 |
| `target/release-0.3.0-validation/gouhuo-server-0.3.0-windows-x64.exe` | 无 PE 版本资源 | 3,694,080 | `665E649F7B2434AC2829D7282DA589D91FB264E3B5B39D66015C3816D237D13C` | 当前发布副本与 dist 一致 |
| Docker 版本镜像 | 待记录 commit / digest | 不适用 | digest 待记录 | 未构建 / 未推送 |

记录实际构建命令和退出码；不能仅从文件存在、文件名或 PE 版本推断它包含最终源码。
客户端 build.rs 嵌入 Windows 版本资源，须核对为 0.3.0。服务端没有对应版本资源或版本命令；
其 PE VersionInfo 为空时照实记录，以候选源码、构建日志和 SHA-256 确认来源。
本轮 `header-spacing-dist.log` 客户端构建 3m04s，installer 4.54s、退出 0，8.182762 MB，gate <10 MB / product <60 MB；服务端字节与哈希未变。
SelfDock 阶段 `dock-refine-dist.log` 3m05s / installer 4.93s，客户端 17,956,352 字节、SHA-256 `BFB08D184E174130F400C7072251AE4BFA7A97B9B547DD76923DB19B7EC06B91`，
安装包 8,182,967 字节、SHA-256 `949576A3A4B914ED6A3013CA1BF196D7C69B6E9259CE5040953DC478B04C54AC`，独立清单 `dock-refine-artifacts.json` 保留历史，不作当前资产。
上轮清单另存 `artifacts-source-final.json`，`source-final-dist.log` 为 174.33s、installer 4.88s；
历史客户端 17,947,648 字节、SHA-256 `3A58C34ED11FEF23DD170D492EEB636996F9BAF44F39F0594E44554C4277A3B1`，
历史安装包 8,181,974 字节、SHA-256 `EADDF2FC090113FBC6CF6046E7A09AA02390D47BAB7F8A430915BE1C727A2E55`，均不包含本轮 SelfDock 修改。
更早候选 `dist-final-build.log` 3m26s、客户端 17,916,416 字节与 `installer-final-build.log` 8,177,722 字节也为历史：
旧客户端哈希 `6C9EA7E265873C40806A55E6D1BED18B57AA118254063A06790D0B58F8E937F0`、
旧安装包哈希 `C6128A0BA2D3BC7F34B2CE0F0236412144C3BBDA45E62EAE53AABAA965D1083D` 已被上轮重建取代。
日志名含 final 不改变后来又有源码修改的事实，不能将这些旧哈希用于当前资产。
首轮 `dist-build.log` 4m02s、客户端 17,913,856 字节与 `installer-build.log` 8,177,927 字节仅是中间测量；
旧安装包哈希 `87AF34D4D7E8A991B1244E8897B40EA56FA689F07E883974D6F0FD90DA0A2A2A` 已被重打覆盖，不代表最终资产。
`-SkipBuild` 必须核对刚构建的 exe。本轮 `header-spacing-pe-dependencies.log` 的 `dumpbin /DEPENDENTS` 通过，
仅 Windows 系统 DLL，没有外部 VCRUNTIME / MSVCP / api-ms-win-crt DLL；客户端与安装包 PE 0.3.0 通过。上轮 `pe-dependencies-final.log` 保留历史。
此结果不替代干净机器实际安装和启动。
草稿上传前再次下载候选资产并比对哈希；本地通过不代表 CI 生成的是同一份字节。

## 原生 UI 与真实会话

本轮 Header 的宽操作区由 232px 收到 172px，正好容纳三个按钮与两段 8px 间距；横向留白由 10px 改为 12px，纵向仍为 10px。
原生 fixture PID 45156 在 760×520 / 400×360 直接观察离开按钮与 Dock 设置的右边界对齐；宽窗口 More 指针打开后 Escape 返回 More 焦点，证据为 `header-spacing-native.txt`。

上一阶段 SelfDock 将宽布局高度由 80px 收到 65px、上下留白 12px；窄布局由 112px 收到 105px，
上下留白与行间距均为 8px，头像垂直居中。麦克风输入区的标题行高 24px、状态行高 16px、电平条高 5px 且左起；
语音激活入口改为左侧标题与右侧箭头，PTT 快捷键移到第二行等待状态。保留完整无障碍名称、焦点与回调。
`dock-refine-matrix.log` 的 80 张快照与鼠标 / 键盘控件断言通过。该阶段原生离线 fixture 编译 1.27s、PID 7160，
760×520 和 F2 400×360 截图经根代理直接观察，对称留白与两行基线正常。宽窗口鼠标打开模式入口，Escape 返回原入口，再按 Return 可重开；
最小窗口鼠标打开模式入口并选择 PTT，完整“鼠标侧键”和“等待 鼠标侧键”显示，焦点回入口。
证据为 [`dock-refine-native.txt`](../target/release-0.3.0-validation/dock-refine-native.txt)，含实际控件无障碍名称与焦点。
这是上一阶段离线布局与入口操作验收，不替代成员音量拖动、聊天或 VAD 发送灯；本轮 dist 会话状态另见下表，其余上轮观察仍标历史。

真实 compact 成员入口另发现并修复一次焦点回归：400×360 reconnecting 中实际点击 member 3，
Escape 后渲染销毁菜单再按 Space，修复前得到 member -1，预期为 3。
`crates/client/ui/scenes/host.slint` 的 CompactMemberButton 仅在 `restore-id` 与自身 member-id 匹配时恢复原按钮焦点，布局与业务状态未变。
修复后 100% / 200% DPI 的实际成员按钮 pointer 开启与 keyboard 开启均通过 Escape → 渲染 → Space 回到同成员，
全部 quick 检查、integration 1 项（2.37s）、example Clippy（30.50s）通过。
证据在本地忽略目录 [`call-ui-focus-validation/after.txt`](../target/call-ui-focus-validation/after.txt)，
同目录 before / after 文本与图片保留失败和修复。此项是 headless 实际控件断言，不能记为原生鼠标操作通过。

| 场景 | 状态 | 必须记录的观察 |
|---|---|---|
| Header 本轮原生入口 | 离线布局与鼠标 / 焦点通过 | PID 45156、760×520 / 400×360：leave 与 Dock settings 右边界同为 12px；宽 More 指针打开 → Escape 返回 More 焦点。`header-spacing-native.txt` 保留证据，不替代 Leave 会话操作或真实音频。 |
| SelfDock 上一阶段原生入口 | 历史离线布局与鼠标 / 键盘通过 | PID 7160、760×520 / 400×360：鼠标打开模式入口，Escape 恢复原焦点，Return 重开；最小窗口选择 PTT 后完整按键与等待标签可见，焦点回入口。未使用真实音频 / 网络，未扩展为其他成员菜单或聊天验收。 |
| dist 启动、进入通话 | 本轮双原生客户端通过 | A PID 28592 / B PID 6716 复制运行 exe 哈希均为当前 `D656B7…A2A78`。fresh AccessKit 均为本地 21943、大厅 2 人，A“语音激活 / 等待说话”，B“按住说话 / V / 等待 V”；A 真实宽窗口直接观察 leave 与 Dock 设置右边界同为 12px。`header-spacing-runtime-views.json` 保留两端状态；未新增性能 trace 或真实声音验收。 |
| SelfDock 阶段 dist 启动、进入通话 | 历史双原生客户端通过 | A PID 1000 / B PID 16784 运行 exe 哈希均为该阶段 `BFB08D…06B91`。fresh AccessKit 均为本地 21943、大厅 2 人，A“语音激活 / 等待说话”，B“按住说话 / V / 等待 V”，麦克风输入区存在；A 宽窗口新 Dock 直接观察正常。`dock-refine-runtime-views.json` 保留两端状态。A startup 389.07ms / call enter 35.79ms / tagged draw wall span 14.25ms；B 555.59 / 39.64 / 15.34ms，均非屏幕或声学延迟，不作公平性能比较。 |
| 宽、窄、400×360 最小窗口 | 软件矩阵 / 定点操作通过；原生窄 / 最小恢复菜单键盘操作通过 | `ui-preview.log` 80 张逻辑断点 / DPI 软件快照与操作断言通过。最后恢复菜单修复后 `ui-preview-final.log --quick` 验证 4 张基础快照与 8 个恢复场景。原生 512px 窄窗口最底两行正常；最终 native fixture F2 400×360、F3×8 reconnecting、F4 菜单完整可见，键盘可操作。软件预览不冒充原生全矩阵。 |
| DPI 与跨显示器 | 100% / 200% 恢复场景软件操作通过；跨显示器未测 | 最小 400×360 下 reconnecting / capture-failed / render-failed / udp-failed × 100% / 200%。图片 `docs/design/rebuild/recovery-member-controls-*.png`；原生跨显示器仍待验收。 |
| 成员音量连续拖动与保存 | 软件指针 / 键盘与原生键盘通过；原生鼠标 / 重启音量未测 | 8 个软件恢复场景 Slider 指针点击后音量 >150，Right 继续增加，个人静音归零。最终原生 fixture Right 100 → 101（fresh AccessKit 确认），Tab 到个人静音后 Return → 0。原生鼠标 drag 被现存系统防火墙提示和 Sky safety guard 阻挡，待用户手动处理；不宣称原生拖动通过。 |
| 键盘与菜单 | 软件断言与原生键盘通过 | quick 保留原焦点返回断言；8 个软件恢复场景 SelfDock 关闭菜单且保留恢复文案。最终原生 Escape 后恢复栏 / dock 保留，重新激活后 F4 可重开，不将临时根焦点记为生产 bug。content 高度 <160px 时借 header / recovery，host 止于固定 dock 上方。 |
| 聊天稳定模型 | 原生点击待确认 | chat click 也被现存系统防火墙提示和 Sky safety guard 阻挡，未完成原生点击验收；软件 / core 测试结果不替代原生消息已读和点击操作。 |
| 静音 / 关闭声音 / 服务器禁言 | 原生本地意愿与软件约束回归通过 | 原生先闭麦再关闭声音显示“声音已关闭”，开麦按钮 disabled，恢复声音后保持闭麦。软件 Release 测试覆盖旧回声 / runtime 禁言约束；管理员协议操作及托盘所有状态未独立验收。 |
| VAD 尾音 / PTT 松开 | 待测 | 实际音频与发送灯；重连 / 切换模式不会重放过期 PTT。 |
| WASAPI 设备流 | 实际采集 / 静音播放探针通过 | `device-streams-unsandboxed.log`：硬件 USB 采集 p95 10.44ms、零 glitch；硬件放音 shared@20ms 零欠载。原沙箱采集被拒绝，不算设备失败；放行后实际设备流成功。全程静音探针，没有听感或声学端到端测量。 |
| 音频故障与恢复 | 软件回归通过，硬件故障操作待测 | 真实拔插 / 换设备 / 试麦与实际 GUI 恢复仍待完成；Release 受控生命周期回归不替代硬件操作。 |
| TCP 断线、UDP 单向 / 双向丢失 | 原生服务端重启后自动重连通过；实际 UDP 丢失未测 | 本地 0.3.0 服务端重启后客户端 B 实际自动重连，首 draw（before present）5.06ms / 绘制回调跨度 1.18ms。最初测试脚本生成新 invite 导致预期认证拒绝，保留原测试 invite 后重跑通过，属于脚本条件修正。真实 UDP 故障仍未操作。 |
| 多机、多成员、公网 UDP | 待测 | 至少双机实时双向声音；记录人数、服务端版本、设备与网络。回环模拟不替代。 |
| 0.3.0 客户端 → 0.2.2 服务端 | 官方资产认证 / 进入页面冒烟通过 | 原生客户端 A 实际连接隔离 21944 官方 0.2.2 服务端，认证和入页截图已观察。未验证混合版本双向声音 / PTT / 聊天完整流程。旧服务端出现 Windows 防火墙人工提示，仍待用户手动处理；未代点或记为完成。 |
| 0.2.2 客户端 → 0.3.0 服务端 | 完整会话未测 | 来源未确认的旧本地 0.2.2 第二实例向已有 0.3.0 转交后 exit 0，仅为转交旁证，没有原生 / 语音断言，不算反向互通通过。 |
| 静音、托盘、游戏全屏负载专项 | 未测 | CPU、RSS、p95 / p99 帧时间与 1% low 需同机同场景重复对照；不新增为本轮发布门禁。 |
| 离开与退出 | 原生正常关闭 / 退出保存通过；Leave GUI 待测 | 中间原生 A/B 正常 Alt+F4 关闭，原进程消失，测试昵称 / PTT 键 / 关闭提示音设置实际存在。没有用有界等待返回代替落盘检查；Leave 控件与托盘退出仍未独立操作。受控协议测试另有停止发送 / drain 断言。 |

最终运行盘点仅 A PID 28592 / B PID 6716 与隔离服务端 PID 41416 保留查看，A 位于前台；离线 fixture PID 45156 已正常 Alt+F4 退出。上一阶段 A PID 1000 / B PID 16784 已正常 Sky Alt+F4 退出，实际保存昵称、模式、按键与提示音设置。
本轮真实宽窗口截图为 [`native-header-spacing-760x520.png`](design/rebuild/native-header-spacing-760x520.png)，SHA-256 `3F75E23BA4C4A0616FA28BCCA0E519B011BA1F07ECE843F4A4E6A19034ED16FD`；运行状态证据为 [`header-spacing-runtime-views.json`](../target/release-0.3.0-validation/header-spacing-runtime-views.json)。
上一阶段离线 fixture PID 7160 已正常退出；历史宽窗口截图为 [`native-dock-refined-760x520.png`](design/rebuild/native-dock-refined-760x520.png)，
SHA-256 `D491663F1D6000D4942E4A85153B3638050972AFD72923EC5CFF231DC1191DE7`。
该阶段运行状态证据为本地忽略产物 [`dock-refine-runtime-views.json`](../target/release-0.3.0-validation/dock-refine-runtime-views.json)。
当前原生会话只证明加入与所列状态 / 布局，不提升实际 VAD、成员拖动、聊天和听感项目。

上轮历史运行 A PID 25272 / B PID 39204，状态证据为本地忽略产物
[`native-final/runtime-view-final.json`](../target/release-0.3.0-validation/native-final/runtime-view-final.json)。
该轮 `client-a/stderr.log` 与 `client-b/stderr.log` 使用已修正的 draw callback wall span 标签。
当时旧离线 fixture 与来源未确认的基线实例已退出，A / B 与隔离服务端保留查看；PID 25272 / 39204 现已退出，这不是本轮新源码运行声明。
这些历史加入与状态显示证据不替代本轮原生重建、双机听感、原生鼠标 / 聊天操作或安装验收。
该轮 A startup / App::new / call enter / draw span 为 420.84 / 205.17 / 32.87 / 13.91ms，
B 为 362.63 / 230.30 / 35.18 / 14.24ms；旧哈希 `3A58C3…7A3B1` 的 trace 不与本轮混用。

更早入页 A 40.00ms / B 45.28ms、旧日志绘制跨度 A 17.59ms / B 22.36ms、
startup A 1522.40ms / B 2387.49ms 均来自双端与后台 Cargo 并行的观察，不与上轮或本轮样本混用。
上一候选 `6C9EA7…E937F0` 曾显示 3 人（包括旧本地基线），仅是当时的同机名单旁证。

上轮原生离线 fixture 从当时 source 构建（1.53s）并复制运行。最小恢复菜单截图为
[`native-final/min-recovery-member.png`](../target/release-0.3.0-validation/native-final/min-recovery-member.png)，
属于本地忽略的验证产物。当时 Windows 防火墙提示在场，鼠标 drag 与聊天 click 未完成。
SelfDock 阶段提示已不在窗口列表，模式入口鼠标操作成功；成员音量拖动与聊天尚未复验，不能据此改成通过。
上述键盘操作为原生 fixture，实际音频 / 网络会话则是独立的 dist 实例；两类结果分开记录。

`ui_timing.rs` 使用 `Instant` 量 request → `AfterRendering` 与 `BeforeRendering` → `AfterRendering`。
本轮较早日志将后者标为 CPU，实际是回调之间的墙钟跨度，不是线程 CPU 时间或 CPU 使用率；
源码现已将标签修正为 draw callback wall span，计时方式和 trace 关闭时的行为不变。
两者均在 present 之前，不含 GPU 完成、屏幕显示延迟，也不是帧间隔。

探针报告中的“协议 46.5ms + 本次设备预算”使用历史协议数字；本次没有重新量协议延迟、APM 或声学嘴到耳，
因此不将报告自动打印的合计作为 0.3.0 端到端进线证据。渲染表中的 p95 是欠载余量，不是渲染延迟；
shared@20ms 是维持的队列深度。

`baseline-home.csv` 仅记录本地来源未确认的 0.2.2 首页进程 15 个样本，单核 CPU 均值 1.128%、
Working Set 90.543 MiB、Private 105.184 MiB。候选观测是在双端通话场景且存在后台编译，不能用这两组跨场景数字宣布回归或改善。

## 安装、升级与回退

在测试 Windows 用户或 VM 中执行安装操作。仅修改 APPDATA 能隔离身份，不能隔离当前用户的 HKCU 协议关联；
不要把候选安装的注册表和快捷方式操作误认为纯离线预览。

| 项目 | 状态 | 证据要求 |
|---|---|---|
| 干净 Windows 安装与启动 | 待测 | 无管理员权限、无预装 VC 运行库；安装路径、版本与启动结果。 |
| 0.2.2 → 0.3.0 覆盖安装 | 待测 | 身份指纹一致；旧设置、服务器、设备、PTT、个人音量保留。 |
| 正在运行时安装 | 待测 | AppMutex / CloseApplications 能发现正在运行实例，更新后不自动恢复旧进程。 |
| gouhuo:// 与第二实例转交 | 安装后的关联未测；旧本地转交有旁证 | 旧本地 0.2.2 向正在运行 0.3.0 转交 exit 0，不证明安装注册表正确或完整会话成功。仍需核对候选安装后的协议命令、重复实例与旧邀请。 |
| 托盘退出 / 程序退出 | 上轮程序正常关闭通过；托盘退出待测 | 上轮 A/B 正常 Alt+F4 后进程消失，测试昵称 / PTT 键 / 关闭提示音设置实际存在，随后重启时哈希匹配当时 dist；不是托盘退出验收。本轮 A / B 保留运行供查看。 |
| 卸载 / 重装 | 待测 | 程序和协议键移除，APPDATA 身份保留，重装读取原身份。 |
| 0.3.0 → 0.2.2 覆盖回退 | 待测 | 同一身份与设置能被旧版读取，回退操作与实际产物来源记录。 |
| 服务端升级 / 回退 | 待测 | 仅隔离数据副本；证书指纹、SQLite、角色、封禁与频道不变。生产未操作。 |

## Header 阶段发布前判定（历史）

当时判定：保持候选。Header 80 快照、原生右边界与 More 焦点操作、新清单、dist / 安装包及双原生加入已核验；该阶段未重跑客户端 73 项测试、Clippy 或全工作区，SelfDock 阶段结果保留历史。
上轮源码的 Windows 全工作区测试、格式、严格 Clippy 与 SPDX 已通过；受控真实协议生命周期、WASAPI 采集 / 静音播放、
最小窗口恢复菜单操作、原生双端加入 / 自动重连、官方旧服务端冒烟和重建打包已有成功证据。
上轮原生最小恢复菜单键盘操作与正常关闭保存通过；上轮受提示阻挡的成员拖动和聊天操作仍未复验。
上轮 176 文件清单与 Windows 产物核验属于历史；内核未变，Linux 逐帧计时后的重复 / 完整验证仍适用同一内核源码。安装 / 覆盖升级及跨机器基本通话尚未完成。
跨显示器、硬件拔插、公网、声学和游戏专项的未测范围分别保留，不把它们写成新增发布许可或必需门禁。
截至该阶段记录时，尚未创建 / 推送标签、提交、草稿 Release、版本镜像，尚未公开发布、提升 latest 或部署生产；这些是历史状态，后续发布状态以 Release / Actions 为准。
新旧清单和各轮候选产物分别留存，本轮构建后 176 文件重检一致。历史失败与各轮成功均保留；上轮受提示阻挡的成员拖动 / 聊天仍未复验，后续实际结果再追加。

准备创建草稿时核对同一 commit 的 redline、DCO / SPDX、Windows 产物和体积门禁；
两个发布 job 均成功后再验下载资产和版本镜像。用户现已明确授权正式发布，未测范围继续保留，不增设用户验收或重复许可步骤。
失败、忽略、无法执行与未经测试分别记录，不将其中任何一种替换成通过。

## 发布前最终检查

最终全工作区复测首次发现 `publish-tests.log` 的客户端为 72 passed / 1 failed / 3 ignored。
失败来自通话 UI 集成测试仍在旧 x=548 位置点击聊天入口；Header 操作区收紧后入口位置已改变。
仅测试夹具的两处聊天点击改为 `chat_x = 760 - 12 - 172 + 30`，断言与操作含义不变，没有修改生产行为或放宽测试判据。
修正后的最终结果如下，原 `publish-tests.log` 失败记录保留；这些是最新复测，不用此前 `source-final` 的 556 项历史结果覆盖本次失败。

| 检查 | 实际结果 | 证据 |
|---|---|---|
| 格式 | PASS | `publish-final-fmt.log`，`cargo fmt --all --check`。 |
| 严格工作区 Clippy | PASS | `publish-final-clippy.log`，`--workspace --all-targets -- -D warnings`，13.06s；此前同一 Header 生产源码的 `publish-clippy.log` 36.58s 通过也保留。 |
| 完整工作区测试 | PASS | `publish-final-tests.log`，27 个 test summaries 合计 556 passed / 0 failed / 13 ignored；其中客户端 73 passed / 0 failed / 3 ignored。 |
| SPDX 许可头 | PASS | `publish-spdx.log`，112 个 Rust 文件通过。 |
| 服务端最小功能 Clippy | PASS | `publish-server-min-clippy.log`，`--no-default-features --all-targets -- -D warnings`。 |
| 音质探针测试 | PASS | `publish-quality.log`，1 passed / 0 failed。 |

本次源码清单 `publish-source-files.sha256` 共 176 文件，SHA-256 为 `58D0B9C1D6A4C4039CB63DDCE95A1A36F32EB2503366948F4E09E64BDAA4FD4B`；相对 Header 阶段仅集成测试坐标和 `types.slint` 文件末尾空行整理。最终提交另由 CI 核对格式、测试和许可。
安装包体积门禁的 6 项边界检查通过，证据为 `publish-size-tests.log`。
忽略项和既有未测范围不改为通过。最终公开状态与 CI 产物以 v0.3.0 Release / Actions 为准；本次发布前复测使用自动检查和离线验证。
