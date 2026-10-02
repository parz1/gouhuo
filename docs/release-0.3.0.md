# 篝火 0.3.0：版本说明

0.3.0 重建通话页面，将语音运行层与 GUI 分离，并把音频准备、恢复、控制消息写入和设置保存移到后台。
Windows 下载与最终公开状态以 [v0.3.0 Release](https://github.com/parz1/gouhuo/releases/tag/v0.3.0) 为准，构建与检查状态见 [GitHub Actions](https://github.com/parz1/gouhuo/actions)。
兼容性与实测范围见 [0.3.0 验证记录](release-0.3.0-validation.md)。安装升级、跨机器声音与其他未测项继续如实保留，发布不将这些项目改为通过。

发布前最终全工作区复测为 556 passed / 0 failed / 13 ignored，格式、严格工作区 all-targets Clippy 与 112 个许可头检查通过。首次聊天入口夹具坐标失配及修正后的重跑均记录在验证附录。

以下 `source-final`、SelfDock 与 Header 产物及原生记录均为发布前的各阶段历史；发布前最终自动检查见验证记录附录，正式资产以 Release 为准。

2026-10-03 上轮 `source-final` 源码的 Windows 全工作区测试通过：556 项通过、0 失败、13 项忽略。
该历史源码的格式、严格工作区 Clippy、112 个许可头，以及软件最小恢复菜单指针 / 键盘与原生最小恢复菜单键盘操作通过。
Release 模式的双端真实 TLS / UDP 生命周期、WASAPI 实际采集 / 静音播放探针、本机双原生客户端加入与服务端重启后自动重连已有成功证据。
上轮两个原生客户端运行哈希均匹配当时 dist，实际进入同一大厅，名单、麦克风电平与 PTT 等待状态正常；不是本轮 SelfDock 的原生验收。
0.3.0 原生客户端已连接核验过来源的官方 0.2.2 服务端，完成认证和进入页面冒烟。
上轮历史安装包为 8,181,974 字节（8.181974 MB），低于体积回归闸；该轮产物的字节数、PE 版本和 SHA-256 已核验。
上轮 176 文件冻结清单、客户端 `3A58C3…7A3B1` 和安装包 `EADDF2…2E55` 已因 SelfDock 最新修改转为历史。
上一阶段底栏留白、语音激活入口和麦克风输入视觉已调整；80 张软件快照与操作断言、客户端 73 项定点测试、严格客户端 Clippy、格式及原生模式入口通过，均保留历史。
本轮 Header 宽操作区 232→172px、横向留白 10→12px，与 Dock 设置右边界一致；80 张软件快照与原生宽 / 最小窗口、More → Escape 焦点操作通过，未重跑 73 项 / Clippy / 全工作区。
176 文件清单 `header-spacing-source-files.sha256`（`80D26D…660DB`）构建后重检一致；客户端 dist 构建 3m04s、安装包 4.54s 均成功。
Header 阶段客户端 17,956,352 字节、SHA-256 `D656B7FE238FE7F4107A81E39D08CDED674A0FD1AEFEED6EBE8D175CA30A2A78`，安装包 8,182,762 字节（8.182762 MB）、SHA-256 `07D817D3A6E5ACD4CFA20AA4534E3A46B6FB8E83CF0544306614A0A318E1B98B`，PE 0.3.0 已核验。
该阶段 A PID 28592 / B PID 6716 运行哈希匹配当时客户端，已加入本地同一大厅；Header 右边界与两端模式 / 等待标签观察正常。SelfDock 阶段 `BFB08D…06B91` / `949576…C54AC` 产物及 A1000 / B16784 已为历史，旧两端正常退出并实际保存设置。
内核未因本轮布局修改而变化，本地 Linux 结果仍适用：延迟单测重复 5 次、标准并行管线重复 2 次、完整内核测试、严格 Clippy 与服务端默认功能测试成功。
先前失败、fixture 根因与修正范围保留在验证记录；安装升级与跨机器通话仍未验收。
游戏性能和声学端到端测量没有执行，本版不据此作性能改善声明。

## 本版变化

- 重建通话页面，保留官方像素篝火。自身状态与开关集中在固定底栏；频道名单、石座使用同一个成员菜单，窄窗口与键盘入口独立验收。
- 细化底栏的上下留白与垂直对齐；语音激活入口采用左侧标题和右侧箭头，PTT 快捷键进入第二行等待状态。麦克风输入标题、状态与左起电平条对齐，保留无障碍名称、焦点与操作回调。Header 操作区按三个按钮的实际宽度收紧，离开按钮与底栏设置右边界统一为 12px。
- 语音生命周期、意愿、状态投影和恢复策略迁到不依赖 Slint 的 `client-runtime`。桌面 WASAPI、设备枚举、热键、身份与托盘仍由桌面适配层提供。
- 音频准备、旧线程回收、控制消息写入和设置保存移到后台。界面使用轻控制句柄；候选链路接纳前禁止语音与 UDP 保活，接纳时应用最新静音意愿和成员音量。
- 自身发送状态采用实际发送结果，保留 VAD 尾音。静音、PTT 松开立即取消发送显示；输入、播放与传输故障分别表达，单独播放故障不掩盖仍在发送的事实。

社区场景包的加载、安装和公开插件接口留待后续版本。跨平台运行层已建立，其他平台的成品客户端仍需要音频、密钥存储、热键、窗口与托盘适配。

## 兼容性

0.3.0 是兼容的架构与界面升级。以下结论有源码依据，跨版本真实会话仍列入验证记录。

| 项目 | 0.3.0 状态 | 源码依据与升级含义 |
|---|---|---|
| 控制协议 | `PROTOCOL_VERSION = 2` | `crates/protocol/src/control.rs` 与 0.2.1 / 0.2.2 一致；本轮未修改消息定义或语音线格式。客户端与 0.2.x 服务端无需同步升级。 |
| 语音加密与序号 | 保持现有格式和 nonce 域 | 相对 `v0.2.1` / `v0.2.2`，protocol 唯一差异为 `crypto.rs` 中旧原子 API 的弃用警告说明与抑制；同会话替换设备继续复用连接的 `VoiceSequences`。 |
| SQLite 存档 | `SCHEMA_VERSION = 2` | `crates/server/src/store.rs` 的两条迁移未修改；没有新增存档迁移。角色、封禁、频道与 TLS 证书继续使用原数据目录。 |
| 邀请链接 | `VERSION = 1`，`gouhuo://j/` | `crates/protocol/src/invite.rs`；既有邀请、加入码与指纹格式保留。 |
| 浏览器加入页 | `DISCOVERY_VERSION = 1` | `crates/protocol/src/discovery.rs`；加入页由 0.2.1 引入，0.2.0 服务端需要继续使用原邀请链接。 |
| Windows 身份 | `FILE_VERSION = 1` | `crates/voice-core/src/identity.rs` 保持 `%APPDATA%\gouhuo\identity.key` 与 DPAPI 格式；本轮只补格式校验测试和平台测试条件。 |
| Windows 设置 | 保持原文本字段与路径 | `crates/client/src/settings.rs` 的 `path/parse/serialize` 格式不变；新增后台写入器，不执行格式迁移。设置仍位于身份目录旁的 `settings.txt`。 |
| 安装关联 | 保持原 AppId、互斥量与 HKCU 协议键 | `packaging/windows/gouhuo.iss` 的 AppId 为 `{0C88191F-C5FF-472E-B19E-B30DDF7F5280}`，AppMutex 为 `GouhuoClientRunning`；覆盖安装应识别为同一应用。 |

本轮没有改变协议或存档版本，不能仅因为产品版本从 0.2.x 变为 0.3.0 就要求所有用户同时升级。
从 0.1.x 升级仍按 [0.2.0 的不兼容变化](release-0.2.0.md) 处理。

## Windows 安装与升级

正式资产名称为 `gouhuo-setup-0.3.0.exe`，目标平台为 Windows 10 及以上的 x64 兼容系统。
从 [v0.3.0 Release](https://github.com/parz1/gouhuo/releases/tag/v0.3.0) 下载安装包。
安装包当前没有代码签名。

1. 退出正在运行的篝火，包括托盘中的实例。保留身份导出备份；同一 Windows 用户覆盖升级不应生成新身份。
2. 双击安装包，按用户安装到 `%LOCALAPPDATA%\Programs\gouhuo`，无需管理员权限。原安装目录由 Inno Setup 复用。
3. 检查程序及“已安装的应用”版本为 0.3.0；启动后确认原昵称、服务器、PTT、设备选择与个人音量保留，身份指纹相同。
4. 点击原 `gouhuo://` 邀请链接，检查协议关联；再点击一次链接，检查第二实例将邀请转交给现有窗口。
5. 卸载应移除程序、快捷方式和 `HKCU\Software\Classes\gouhuo`，保留 `%APPDATA%\gouhuo`。重装后确认原身份可继续使用。

需要回退时，退出 0.3.0 后覆盖安装已核验的 0.2.2 安装包。源码没有新增设置或身份格式迁移；
实际覆盖回退仍需在测试用户或测试机器验证。不要删除身份文件来解决升级问题。

更新检查仍只提示新正式版并打开发布页，由用户手动下载安装。尚未实现后台下载安装、安装包自动校验、自动替换或自动回滚。

## 服务端升级

本轮发布不修改生产服务器、数据卷或 `.env`；版本镜像与 `latest` 的发布由下述工作流处理，部署者自行安排升级。
0.3.0 客户端可以先连接现有 0.2.x 服务端；本轮已完成连接官方 0.2.2 服务端的冒烟。
跨版本双向声音与旧客户端连接新服务端的完整运行验收仍待执行。

日后由部署者安排升级时，先备份完整服务端数据目录，包括 SQLite 和 TLS 密钥，保留 `.env`。
Docker 部署可固定 `GOUHUO_VERSION=0.3.0` 再 pull / 重建；Windows 服务端资产名为
`gouhuo-server-0.3.0-windows-x64.exe`。重启会中断当前会话。
回退使用已核验的 0.2.2 服务端和同一数据目录；原证书不能被重新生成，否则旧邀请的指纹失效。
这些是部署说明，本次未执行升级、回退或生产操作。

## 版本与发布配置

| 文件 | 本次准确改动 | 禁止用来代替的动作 |
|---|---|---|
| 根 `Cargo.toml` 与 crate manifest | `[workspace.package] version = "0.3.0"`；`client` 已单独声明 Rust 1.92（Slint 1.18.1 要求），`voice-core` 已声明 1.91（sonora 要求），工作区基础值保留 1.80。各依赖组合的真正最低版本仍需独立 MSRV CI 验证。 | 为配合产品版本号修改 `PROTOCOL_VERSION`，或将单一新工具链通过写成所有最低版本已验。 |
| `Cargo.lock` | 通过 Cargo 刷新工作区包版本，包含新 `client-runtime`；保留已锁定依赖，不全局替换第三方的 `0.2.2`。 | 将所有同名版本字符串机械替换。 |
| `README.md` | 顶部使用 0.3.0 版本和 Release 链接，链接本说明及验证记录；版本策略说明较大架构升级也可升 minor，兼容性由线协议和存档格式独立判断。 | 继续沿用“0.X.0 必然不兼容”的结论或以本地构建代替公开发布状态。 |
| 本说明与验证记录 | 补最终源码标识、实际产物、哈希、通过与未测项，保持历史记录可追溯。 | 将先前 0.2.x 或单包结果写成 0.3.0 全工作区验收。 |
| `docs/measurements.md` | 构建、体积或性能实际测量后新增 0.3.0 行；注明机器、构建和测量范围。 | 沿用历史常量作为本轮实测。 |
| `redline.rs` / 打包脚本 | 本轮默认不放宽阈值；如候选包触发回归闸，先查明增长并记录差异。 | 为让包通过而直接抬高 10 MB 回归闸。 |
| 发布工作流与 `.iss` | 草稿策略、权限、AppId、安装路径和保留身份行为保持；若调整门禁接线必须另有验证。 | 将发布版本与部署生产或修改安装身份混为同一步。 |

## 自动验证与打包门禁

发布前自动检查与打包使用下列命令，保留退出码和原始日志；实际结果以验证记录为准。

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p server --no-default-features --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p voice-core --example quality-probe --locked
python scripts/check-spdx.py
.\packaging\windows\test-size.ps1
cargo check -p client --all-targets --locked
cargo check -p server --no-default-features --locked
```

另在 Linux runner 执行 `.github/workflows/redline.yml` 的 `linux-core` check / clippy / test，
包含 `client-runtime`，同时核对服务端默认与 `--no-default-features` 编法。
共享 runner 的编译通过不表示真实 Linux / Windows 语音设备可用。

Windows 发布构建使用 `dist` profile：

```powershell
.\scripts\win-buildenv.ps1 -Run "cargo build --profile dist --locked -p client --bin gouhuo"
.\scripts\win-buildenv.ps1 -Run "cargo build --profile dist --locked -p server --bin gouhuo-server"
.\packaging\windows\build.ps1 -SkipBuild
```

`-SkipBuild` 只能使用刚从同一候选源码构建且 PE 版本已经核对的 `target/dist/gouhuo.exe`。
安装包脚本不会检查旧 exe 与新文件名是否一致。记录客户端、服务端、完整安装包的字节数与 SHA-256，并核对客户端 PE 版本。
服务端当前没有嵌入 Windows 版本资源，也不提供版本命令；服务端版本来源以候选源码、构建日志和哈希为准，不能由文件名推断。
Windows 运行库依赖检查的结果见验证记录；全新安装仍未实测。静态 CRT 配置位于 `.cargo/config.toml`，不能把开发机可运行当成干净机器可运行。

完整安装包必须 **小于 10,000,000 字节**，产品线为 **小于 60,000,000 字节**。
`build.ps1` 打包后自动执行 `check-size.ps1`，阈值来自 `redline.rs`；恰好等于阈值也失败。
`MEASURED_INSTALLER_MB = 8.023011` 是历史 0.2.2 正式资产记录，不是 0.3.0 测量值。

UI 与运行验证的实际范围见验证记录：原生双端加入、512px 窄窗口、最小恢复菜单键盘操作、正常关闭 / 设置保存、关闭声音意愿、服务端重启后重连已观察；
软件预览覆盖断线和音频故障下 400×360 最小窗口、100% / 200% DPI 的音量拖动、键盘、个人静音与底栏操作。
SelfDock 阶段 dist 的宽窗口底栏与两端模式状态已观察，离线 fixture 另验证 760×520 / 400×360 的模式入口鼠标、Escape 返回焦点、Return 重开及完整 PTT 等待标签；本轮 Header 原生验证右边界对齐与 More → Escape 焦点返回。
这些是布局与控件操作结果，不替代实际语音激活发送灯或 PTT 双向声音验收。
上轮成员鼠标拖动和聊天点击曾被 Windows 防火墙提示阻挡，仍未复验；SelfDock 阶段提示已不在窗口列表，模式入口的原生鼠标与焦点返回通过，不能代替前两项。
软件断言不能替代硬件拔插、跨显示器或跨机器声音验收；安装 / 覆盖升级和跨机器基本通话仍为本版未测范围，不表示执行通过。

公网 UDP、游戏帧时间和专用机器 `latency-probe --assert-gate` 可作为专项测量，当前保持未测范围，不新增为本轮发布要求。
若比较性能，应使用同机、同人数、同设备、同窗口和构建配置的 0.2.2 基线与 0.3.0 dist，记录冷 / 热启动、进入通话、CPU / RSS。
共享机器上带后台编译的启动数字、不同场景的进程采样不能据此宣称回归或改善。

## 草稿发布与权限边界

- `.github/workflows/release.yml` 仅由 `v*` 标签触发，检查标签与 Cargo 产品版本匹配。Windows job 调用同一安装包体积门禁，再以 `gh release create --draft --verify-tag` 创建草稿。
- Windows job 的有效权限为 `contents: write`。Docker job 单独使用 `contents: read`、`packages: write`，推版本镜像 `ghcr.io/<owner>/gouhuo-server:0.3.0`。
- 标签触发后 Docker job 构建并上传版本镜像，即使 Release 仍是草稿；草稿不等于完全没有外部写入。Windows 与 Docker job 均成功后再公开 Release。
- `.github/workflows/docker-latest.yml` 在非预发布 Release 的 `published` 事件上，用 `packages: write` 将版本镜像提升到 latest。点击公开发布同时改变更新可见性和未来服务器 pull 的版本，不能当成单纯编辑文案。
- Release 构建没有依赖 redline 测试 job。发布前须核对同一候选提交的全工作区结果和独立 redline 状态，不能只看标签构建成功。
- Windows 与 Docker 发布 job 独立并行；草稿出现不代表镜像已完成。公开发布前须确认两个 job 及版本镜像都成功，并核对下载的实际资产。

0.3.0 正式发布按用户“发个版本”的明确授权执行，不另设用户验收或重复许可步骤。
最终公开状态与 `latest` 更新结果以 Release / Actions 为准；生产部署不在本轮范围，未测项目保留在验证记录中。
