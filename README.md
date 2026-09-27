# 篝火 / gouhuo

**低延迟语音聊天，满足日常开黑。开源、可自部署。**

一群人常驻一个频道，边打游戏边说话，一挂几小时 —— 所有设计都以这个场景为准绳：
进频道要快，延迟要低，挂一晚上不占资源、不打扰游戏。

另一个方向是**游戏开发者**。想在游戏里加语音的人，面对的往往是一堆要自己拼的
东西：采集、回声消除、编解码、抖动缓冲、加密、服务端。这里试着把这些收进
几个边界清楚的 crate —— 语音内核独立于界面、协议单独一份、服务端零配置 ——
看看能不能把接入语音的心智负担降下来。

> **当前状态：还不能给不懂技术的人用。** 语音链路、服务端、界面、加密都通了，
> 端到端延迟 91.9 ms 实测。缺的是安装包（M6）—— 现在只能自己 `cargo build`。
> 详见[路线图](#路线图)。

---

## 为什么再写一个

| 痛点 | 这里怎么解 |
|---|---|
| 加服务器要手输 IP、端口、证书指纹 | **一条链接**。`gouhuo://j/...` 粘进去就进频道了，指纹在链接里 |
| 身份是本地文件，换台机器就没了 | Ed25519 密钥对，一行文本导出导入；私钥用 DPAPI 落盘 |
| 自部署要配数据库、写配置文件 | **一个 1.7 MB 的二进制**，零配置。启动就把邀请链接打在屏幕上 |
| 权限是个矩阵，没人看得懂 | 四个预设角色，线上传的是角色不是权限位 |
| 延迟没人说得清是多少 | 每个数字都实测、都有存档、都能复现，[量法公开](docs/measurements.md) |

**隐私**：没有账号、没有手机号、没有邮箱。身份就是一对密钥。服务端只知道公钥和昵称。

---

## 性能

两类数字，性质完全不同，所以分开列：

- **产品线** —— 对用户的承诺。依据是「用户感知得到吗」和「同类产品在什么水平」
- **防回归闸** —— CI 卡的线，定在当前实测值加一点余量。作用只有一个：谁改代码让它变慢了，CI 立刻亮

| 指标 | 产品线 | 防回归闸 | 当前实测 |
|---|---|---|---|
| 端到端延迟（同城） | < 120 ms | < 100 ms | **91.9 ms** = 协议 46.5 + 设备 30.4 + APM 15.0 |
| └ 协议链路（CI 唯一卡的一段） | — | **< 55 ms** | **46.5 ms** |
| 通话中 CPU（单核占比） | < 6% | < 4% | **2.97%** = 编解码 2.18% + APM 0.79% |
| 静音时带宽（DTX 生效） | **< 5 kbps** | — | **1.2 kbps** |
| 说话时单条上行 | < 80 kbps | — | 10 ms 帧 61.3 kbps；20 ms 帧 39.0 kbps |
| voice-core 常驻内存 | < 100 MB | — | 未测 |
| 安装包 | < 60 MB | — | 未测 |

91.9 ms 在 ITU-T G.114 的「用户很满意」区间内（单向嘴到耳 150 ms 以内），
也好于 Discord / TeamSpeak 同类配置的常见水平（100–150 ms）。

数字来自一台机器：Ryzen 7 5700X3D / Windows 11 Pro / 共享模式引擎周期 10 ms。
**量法、每个数字的由来、以及它们不包括什么**，都在
[`docs/measurements.md`](docs/measurements.md)。
红线常量定义在 [`crates/voice-core/src/redline.rs`](crates/voice-core/src/redline.rs)，
CI 直接读那里（`--assert-gate`），不在别处重复写。

---

## 快速开始

需要 **Rust（MSVC toolchain）** 和 **CMake**（`audiopus_sys` 要从源码编 libopus）。
Visual Studio / BuildTools 自带的那份 cmake 就行；不在 PATH 上时跑
[`scripts/win-buildenv.ps1`](scripts/win-buildenv.ps1)，它会去 VS 里找。

回声消除、降噪、自动增益默认就带着，**不需要任何额外工具链**。

### 起一个服务器

```bash
cargo run --release -p server --bin gouhuo-server
```

它会打印一条邀请链接，还有证书指纹和数据目录的位置。要给外网的朋友用就设
`GOUHUO_HOST` 成公网 IP 或域名，并把端口转发过来。

全部配置走环境变量，没有配置文件格式要学：

| | 默认 |
|---|---|
| `GOUHUO_PORT` | `20800`（TCP 和 UDP 同一个号，[为什么不在 49152 以上](docs/design-notes.md#默认端口不在-49152-以上)） |
| `GOUHUO_DATA` | `./gouhuo-data`（证书、邀请码；**里面有 TLS 私钥**） |
| `GOUHUO_HOST` | 自动探测的局域网地址 |
| `GOUHUO_INVITE` | 首次启动随机生成；设成空串表示不要邀请码 |
| `GOUHUO_MAX_USERS` | `20` |

### 连上去

```bash
cargo run --release -p client --bin gouhuo -- gouhuo://j/...
```

不带参数启动也行，界面里有粘链接的框。

---

## 项目结构

```
crates/
  protocol/        客户端与服务端共享的唯一一份协议定义：包格式、序列化、加密
  voice-core/      语音内核：采集/APM/Opus/抖动缓冲/混音、WASAPI、全局热键、
                   实时时钟、socket 调优、度量、红线定义
  transport/       TLS 怎么建、UDP 密钥从哪来。两端共用
  server/          服务端。state = 纯状态机，conn = TLS/线程/分帧，voice = UDP 转发
  client-core/     客户端逻辑，不含界面：连接、认证、状态镜像
  client/          Slint 界面。只负责画和转发点击
  latency-probe/   协议链路延迟的测量工具
  device-probe/    WASAPI 设备延迟 + APM 逐块计价
docs/
  measurements.md      红线表的依据：量法、每个数字的由来
  design-notes.md      做过的选择和它的理由
  apm-backend.md       APM 用哪个实现，怎么选的，以及量它踩的坑
  m1-baseline.txt      协议链路的完整报告存档
  m2-baseline.txt      设备与 APM 的完整报告存档
```

`protocol` 两端都依赖，改一个字段两边同时编译报错 —— 这是把它独立成 crate 的
全部理由。`voice-core` 完全独立于 UI，将来实测内存不达标时把它拆成独立进程
是纯工程重构。

---

## 开发

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
python scripts/check-spdx.py     # 每个文件头的 SPDX 要跟所属 crate 的许可一致
```

需要真声卡才能跑的测试（试麦、设备扫描、声学回声）都标了 `#[ignore]`，
用 `-- --ignored` 显式跑。

### 测量工具

```bash
cargo run --release -p latency-probe       # 协议链路，约 3 分钟
cargo run --release -p device-probe        # 设备延迟 + APM 逐块计价
```

`device-probe` **不会出声**，但会短暂点亮「麦克风正在使用」指示灯 ——
测采集必须打开采集流。加 `--list` 只看设备能力，`--no-apm` 跳过 APM 那段。

要门禁级的可信数字就把时间拉长（p99 需要样本），并且**跑的时候别动这台机器**，
后台一个编译就能把 p99 毁掉：

```powershell
.\scripts\m1.ps1 -Seconds 30
```

### CI

`fmt` / `clippy -D warnings` / `cargo test` / SPDX 检查是门禁，跑在 pull request
和手动触发（`gh workflow run redline`）上，**不跑在每次 push 上**。

**延迟数字不在 GitHub 托管 runner 上门禁。** 那是虚拟化的共享机器，节拍器迟到是
常态，测出来的 p95 反映的是 runner 的负载而不是架构；在那种数字上设门禁只会训练出
「重跑一次就绿了」的习惯，比不测更糟。真门禁跑在专用机器上，见
[`.github/workflows/redline.yml`](.github/workflows/redline.yml)。

---

## 路线图

**M1** ✅ 协议链路延迟下限
**M2** ✅ 设备延迟 + APM 实测，端到端进产品线

**M3** 控制面、服务端、一键加入 —— 进行中

- ✅ 身份（Ed25519 + DPAPI + 导出导入）、证书指纹、邀请链接
- ✅ 控制面消息（protobuf）+ 分帧
- ✅ TLS 握手 + 证书固定 + 从 TLS 派生 UDP 密钥，整条信任链有端到端测试
- ✅ 服务端：频道树、成员状态、认证、文字消息
- ✅ UDP 语音转发（SFU 核心）：每连接密钥、防重放、保活探测
- ✅ 多频道：成员能建，建的人和频道管理能删
- ⬜ SQLite 持久化
- ⬜ UDP 不通时退回 TCP 传语音
- ⬜ 改频道、踢人、封禁

**M4** 客户端 —— 进行中

- ✅ 连接、认证、状态镜像（对着真服务端有端到端测试）
- ✅ Slint 界面：频道树、成员名单、文字、一键加入。冷启动 92 ms / 常驻 59.4 MB
- ✅ 音频链路端到端 36.7 ms 实测（只比理论下限大 0.2 ms）
- ✅ 全局热键（Raw Input）：按住说话、绑键、持久化
- ✅ 试麦、电平条、设备选择、语音激活灵敏度
- ✅ 建频道 / 删频道
- ⬜ 每个人单独调音量（链路里有了，界面还没接）
- ⬜ TTS 播报、提示音

**M5** 自适应抖动缓冲 + PLC。技术含量最高的一块，也是低延迟卖点的主战场
**M6** 打包、安装包体积实测、游戏帧时间影响实测

### 明确不做

视频、屏幕共享、动态 / 帖子 / 社区发现 / 好友系统、装扮和表情商店、富文本、
长期消息历史、直播。

客户端目前只有 Windows 实现（WASAPI、DPAPI、Raw Input 热键）。这是先后顺序，
不是范围：平台相关的代码基本收在 `voice-core`（音频设备、热键、时钟、身份落盘），
别的平台是往那里加实现。

---

## 许可

**协议宽松，引擎能嵌入，成品 copyleft。**

| 部分 | 许可 |
|---|---|
| `crates/protocol` | MIT OR Apache-2.0 |
| `crates/voice-core`、`crates/transport`、`crates/server` | MPL-2.0 |
| 其余（客户端、探针） | GPL-3.0-or-later |

`protocol` 放开是为了让别人能自由写第三方客户端、机器人、别的语言的实现。
引擎和服务端是 MPL，可以链进任何产品、可以随便自部署（包括闭源商业场景），
只有改了这些 crate 里的文件才要公开那些改动 —— 自部署是核心卖点，不该在公司 IT 的
许可白名单那里被卡住。客户端是 GPL，不想被套壳加广告再分发。

**对用户没有任何影响**：copyleft 的义务只在分发时产生。下载、安装、自己架服务器
给朋友用都没有义务；把改过的版本发给别人才要带源码。

完整说明见 [`LICENSING.md`](LICENSING.md)。

## 贡献

见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。用 DCO（`git commit -s`），不用 CLA。

提交说明用 [Conventional Commits](https://www.conventionalcommits.org/zh-hans/v1.0.0/)
格式（`feat(client): ...`、`fix(voice-core): ...`），正文写**为什么**，不写改了什么
—— 改了什么 diff 里看得见。涉及性能的改动请带上数字：哪个配置、测了多久、跟之前比是多少。
