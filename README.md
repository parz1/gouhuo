# 篝火 / gouhuo

**低延迟语音聊天，满足日常开黑。开源、可自部署。**

一群人常驻一个频道，边打游戏边说话，一挂几小时 —— 所有设计都以这个场景为准绳：
进频道要快，延迟要低，挂一晚上不占资源、不打扰游戏。

另一个方向是**游戏开发者**。想在游戏里加语音的人，面对的往往是一堆要自己拼的
东西：采集、回声消除、编解码、抖动缓冲、加密、服务端。这里试着把这些收进
几个边界清楚的 crate —— 语音内核独立于界面、协议单独一份、服务端零配置 ——
看看能不能把接入语音的心智负担降下来。

> **当前版本：0.2.1。** Windows 安装包和 Linux Docker
> 部署已经可用。0.2.1 跟 0.2.0 协议兼容，可以分开升级（[这一版有什么](docs/release-0.2.1.md)）；
> 从 0.1.x 升上来仍要客户端和服务端一起换。
> 91.9 ms 是协议、设备和 APM 的分项实测合计，完整双机嘴到耳延迟尚未测量。
> 下一步是小规模开黑验证，见 [0.2.0 升级与验收](docs/release-0.2.0.md)。

---

## 为什么再写一个

| 痛点 | 这里怎么解 |
|---|---|
| 加服务器要手输 IP、端口、证书指纹 | **一条链接**。`gouhuo://j/...` 粘进去就进频道了，指纹在链接里 |
| 身份是本地文件，换台机器就没了 | Ed25519 密钥对，一行文本导出导入；私钥用 DPAPI 落盘 |
| 自部署要配数据库、写配置文件 | **一个 3 MB 的二进制**，零配置，存档是编进去的 SQLite。启动就把邀请链接打在屏幕上 |
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
| 端到端延迟（同城） | < 120 ms | < 100 ms | **91.9 ms（分项实测合计）** = 协议 46.5 + 设备 30.4 + APM 15.0 |
| └ 协议链路（CI 唯一卡的一段） | — | **< 55 ms** | **46.5 ms** |
| 通话中 CPU（单核占比） | < 6% | < 4% | 历史配置 **2.97%**；64 kbps + 自适应 AGC 的完整客户端配置待重测 |
| 静音时带宽（DTX 生效） | **< 5 kbps** | — | **1.2 kbps** |
| 说话时单条上行 | < 120 kbps | — | 当前 64 kbps / 10 ms；合成诊断 IPv4 上行 **109.7 kbps**，真人验收见 [音质记录](docs/audio-quality.md) |
| 常驻内存（在频道里） | voice-core < 100 MB | — | **73 MB** 整个进程，语音那一半约 13 MB；最小化、收在托盘里一样，挂 48 分钟不涨 |
| 安装包 | < 60 MB | — | **0.2.0 本地验收包：7,942,844 字节，约 7.94 MB**（本地构建记录） |

91.9 ms 是分项合计，用于延迟预算评估；软件链路端到端另测得 36.7 ms
（本机回环、合成设备、不含声卡和 APM）。完整双机真实设备嘴到耳延迟尚未测量，
也没有与其他语音软件在相同条件下做过对比。

数字来自一台机器：Ryzen 7 5700X3D / Windows 11 Pro / 共享模式引擎周期 10 ms。
61.3 / 39.0 kbps 是早期 16 kbps 编码的历史结果，不能代表当前默认码率。
音质修复、离线 WAV 对比工具和待完成的真人验收见 [`docs/audio-quality.md`](docs/audio-quality.md)。
**量法、每个数字的由来、以及它们不包括什么**，都在
[`docs/measurements.md`](docs/measurements.md)。
红线常量定义在 [`crates/voice-core/src/redline.rs`](crates/voice-core/src/redline.rs)，
CI 直接读那里（`--assert-gate`），不在别处重复写。

---

## 安装

到 [Releases](https://github.com/parz1/gouhuo/releases) 下载 `gouhuo-setup-<版本>.exe`，双击。

- **不要管理员权限**：装在你自己的用户目录下（`%LOCALAPPDATA%\Programs\gouhuo`），
  网吧、公司电脑上也能装
- 装完之后，别人发来的 `gouhuo://` 链接点一下就能进频道。篝火已经开着的话，
  链接会交给开着的那个，不会再开一个
- **Windows 会拦一下**：安装包还没有代码签名，SmartScreen 会说「Windows 已保护你的
  电脑」。点「更多信息」→「仍要运行」。签名证书要钱，等用的人多了再买
- **检查更新**：启动时去 GitHub 看一眼最新的版本号，有新版本就在窗口里提一句，
  不会自己下载、自己装。什么都不上传；设置里能关
- **卸载**：在「设置 → 应用」里卸。身份（`%APPDATA%\gouhuo`）**不会**删 ——
  那是你的密钥，删了就再也找不回这个身份。真不要了自己删那个目录

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
| `GOUHUO_DATA` | `./gouhuo-data`（证书、邀请码、频道存档 `gouhuo.db`；**里面有 TLS 私钥**） |
| `GOUHUO_HOST` | 自动探测的局域网地址 |
| `GOUHUO_INVITE` | 首次启动随机生成；设成空串表示不要邀请码 |
| `GOUHUO_MAX_USERS` | `20` |
| `GOUHUO_WEB_LISTEN` | 不启用；设成 `127.0.0.1:20801` 开启浏览器加入页，供 HTTPS 反向代理使用 |
| `GOUHUO_JOIN_URL` | 对外的 HTTPS 加入页地址；设置后在日志里打印浏览器邀请 |
| `GOUHUO_NAME` | `朋友的篝火`，加入页显示的服务器名称 |

### 用 Docker 部署

放到一台 Linux 服务器上，最省事的是 Docker。服务器上不用放代码，也不用编译。
找个目录，一条命令：

```bash
mkdir -p ~/gouhuo && cd ~/gouhuo
curl -fsSL https://raw.githubusercontent.com/parz1/gouhuo/main/install.sh | sh
```

它会下载 [`compose.yaml`](compose.yaml)、探测公网 IP 写进 `.env`、拉镜像起服务，
最后把邀请链接和管理员链接打出来。有域名就带上（推荐，理由见下面）：

```bash
curl -fsSL https://raw.githubusercontent.com/parz1/gouhuo/main/install.sh | GOUHUO_HOST=voice.example.com sh
```

脚本不替你装 Docker、不动防火墙，缺了会告诉你怎么做。见 [`install.sh`](install.sh)。

**升级**：新版本发布之后，在同一个目录里再跑一次同一条命令。`.env` 原样留着。

<details>
<summary>不想用脚本：手动三步</summary>

```bash
curl -fsSLO https://raw.githubusercontent.com/parz1/gouhuo/main/compose.yaml
echo "GOUHUO_HOST=你的公网IP或域名" > .env
docker compose up -d && docker compose logs gouhuo   # 邀请链接在这里
```

升级：`docker compose pull && docker compose up -d`。

</details>

重启要一两秒，频道里的人会断一下。想钉住某个版本，在 `.env` 里加 `GOUHUO_VERSION=0.1.0`。

- **防火墙 / 安全组要放行 20800 的 TCP 和 UDP**。漏了 UDP 的症状是能进频道、听不到声音
- 地址得在 `.env` 里写明（容器里只探测得到内网地址）。脚本替你写了，但它探到的是
  出网 IP，机器有多个公网 IP 时可能不是你想要的那个 —— 看一眼打出来的链接
- **有域名就用域名。** 地址是写死在邀请链接里的，写 IP 的话换机器、换 IP 所有旧链接都作废；
  写域名只要改 DNS。不用申请证书（链接里带的是证书指纹，不看主机名）。
  Cloudflare 要选「仅 DNS」（灰云）—— 代理只转 HTTP，UDP 过不去
- **`compose.yaml` 别改**，所有设置都写进 `.env`（见[上面的表](#起一个服务器)，比如
  `GOUHUO_PORT=20900`）。改了 `.env` 之后 `docker compose up -d` 生效。改 `.env` 用编辑器，
  别再 `echo ... > .env` —— 那会把别的设置冲掉
- 数据在卷 `gouhuo-data` 里，升级不会动它。**里面有 TLS 私钥**，丢了所有旧邀请链接
  都作废。别用 `docker compose down -v`；备份：
  `docker run --rm -v gouhuo-data:/d alpine tar c -C /d . > gouhuo-backup.tar`
- 从源码编：在仓库里 `docker compose up -d --build`
- [`compose.yaml`](compose.yaml) 用的是 host 网络，只在 Linux 上有效；Docker Desktop
  上改用里面注释掉的 `ports`

Railway 这类只转发 HTTP/TCP 的平台跑不了：语音走 UDP，而「UDP 不通时退回 TCP」还没做。

### 用域名打开 HTTPS 加入页

浏览器访问 `https://voice.example.com/`，点击「用篝火加入」即可通过已有的
`gouhuo://` 协议唤起 Windows 客户端。没有安装时可下载客户端；浏览器没有唤起时，
可复制邀请链接并粘贴到篝火。加入页采用动态像素篝火，切到后台或选择减少动态效果时暂停。

域名的 DNS 需要指向服务器。除语音所需的 TCP/UDP 20800 外，还需放行
**80/tcp、443/tcp**，供 Caddy 申请和自动续期证书。域名不要开启会影响 UDP 的代理。
80/443 已经有反向代理在使用时，按下方「已有代理」配置，不要再启动一份 Caddy。

一键安装或升级并启用 HTTPS（启用状态会保存在 `.env`，以后升级继续保留）：

```bash
curl -fsSL https://raw.githubusercontent.com/parz1/gouhuo/main/install.sh | GOUHUO_HOST=voice.example.com GOUHUO_HTTPS=1 sh
```

手动部署时，下载仓库的 `compose.yaml`、`compose.https.yaml`、`Caddyfile`，在同目录
创建 `.env`，写入 `GOUHUO_HOST=voice.example.com`，可加 `GOUHUO_NAME=周末开黑`。运行：

```bash
docker compose -f compose.yaml -f compose.https.yaml up -d
# 在源码仓库中验证本次改动：同一命令追加 --build
# 升级需要同时带上两个文件：
docker compose -f compose.yaml -f compose.https.yaml pull
docker compose -f compose.yaml -f compose.https.yaml up -d
```

这组配置使用 Linux host 网络，HTTP 加入页仅监听 `127.0.0.1:20801`，Caddy 对外提供
HTTPS。Caddy 的证书保存在独立数据卷里。启动日志会给出浏览器加入页和私人浏览器邀请。
**新功能需要包含本次改动的服务端镜像**；旧版本镜像没有 HTTP 加入页，可先从源码 `--build`。

**私人服务器**：直接打开域名会要求输入朋友给的加入码；日志中的浏览器邀请形如
`https://voice.example.com/#code=…`，可以让朋友免输入。加入码位于 URL fragment，
不会发送给 HTTP 服务，也不会进入代理访问日志；页面读取后会从地址栏移除它。
这仍是一条私人凭据，不要公开发布。公开页面不提供加入码，更不会提供管理员认领链接。
服务端的加入验证仍由原有语音协议完成，页面不会宣称填写的码一定有效。

**已有 HTTPS 代理**：在 `.env` 中设置 `GOUHUO_WEB_LISTEN=127.0.0.1:20801`、
`GOUHUO_JOIN_URL=https://voice.example.com/`，重启语音服务；让现有代理将这个域名的
HTTP 请求转发到 `http://127.0.0.1:20801`。代理若在另一容器网络中，需要调整监听和
容器网络，使它能访问加入页。加入页的 HTTP 端口不应直接暴露在公网。
HTTPS 的网页证书与语音协议的自签名证书独立管理，不改变现有语音邀请或端口。

nginx 的站点配置大致是这样（证书自己准备，比如 certbot）。它只反代加入页；
语音的 20800（TCP/UDP）照旧直连，不经过代理：

```nginx
server {
    listen 443 ssl;
    server_name voice.example.com;

    ssl_certificate     /etc/letsencrypt/live/voice.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/voice.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:20801;
        proxy_set_header Host $host;
    }
}
```

**不用就不占东西**：加入页默认是关的。只有设了 `GOUHUO_WEB_LISTEN`（上面的
`compose.https.yaml` 会替你设）才会起，没设的话服务端不多开一个线程、一个端口。
它给服务端二进制加了约 0.2 MB；开着的时候多 6 个空闲线程、约 0.3 MB 内存，没人访问时不耗 CPU。
真正占地方的是 Caddy 那个容器，而它只在你用 `compose.https.yaml` 时才有 ——
已经有反向代理的话不需要它。想要一个彻底不含 HTTP 服务的镜像，从源码编时在 `.env`
里写 `GOUHUO_FEATURES=--no-default-features`；这样的服务端碰到 `GOUHUO_WEB_LISTEN`
会直接报错退出，不会悄悄不起。

**在客户端里直接输域名**：加入页旁边还有一份给客户端读的说明
（`/.well-known/gouhuo`：语音地址、端口、证书指纹、服务器名，**不含加入码**）。
朋友在篝火里输 `voice.example.com` 就能加入，不用传那一长串邀请链接 ——
指纹由域名的 HTTPS 证书担保。把日志里的浏览器邀请（带 `#code=…`）整条粘进客户端也行，
加入码同样不会被发给网页服务。

### 管理员

第一次启动时，邀请链接下面还会多打一条**管理员链接**。自己用它连进去就成了
管理员（按公钥记进存档），链接随即作废。管理员能改频道、踢人、封禁，还能在
成员名单里给别人升降角色（访客 / 成员 / 频道管理 / 管理员）。

管理员身份丢了（重装系统又没导出身份）的话，带上 `--new-admin-link` 启动，会再发一条。

### 连上去

```bash
cargo run --release -p client --bin gouhuo -- gouhuo://j/...
```

不带参数启动停在首页：上面是上次去的服务器，一点（或者回车）就回去；下面是别的
存着的服务器和「加入新服务器」。加入的那个框认四样东西：

| 输入 | 怎么确认是这台服务器 |
|---|---|
| `gouhuo://j/…` 邀请链接 | 证书指纹就在链接里 |
| `https://voice.example.com/` 加入页地址 | 加入页给出指纹，域名的 HTTPS 证书担保 |
| `voice.example.com` 域名 | 先找它的加入页；没有就按下一行处理 |
| `203.0.113.7`、`host:20800` | 没人担保：客户端把服务器的指纹取回来，**你跟服主核对过再点加入**（服务端启动日志里有「证书指纹」一行），之后记住不再问 |

没写端口就是 20800。私人服务器会再问一次加入码。加入成功的服务器记在
`%APPDATA%\gouhuo\settings.txt` 里（私人服务器的加入码也在里面，别把这个文件发给别人）。

### 自己打安装包

要装 [Inno Setup 6](https://jrsoftware.org/isinfo.php)（`winget install JRSoftware.InnoSetup`）。

```powershell
.\packaging\windows\build.ps1
```

出来的是 `target\installer\gouhuo-setup-<版本>.exe`。正式发版不用手动打，见下面[发版](#发版)。

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
cargo run --release -p latency-probe -- sim  # 固定 vs 自适应抖动缓冲，离线模拟一小时，几秒跑完
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

### 发版

有两个版本号，别混：

| | 在哪 | 管什么 | 什么时候动 |
|---|---|---|---|
| 产品版本 | `Cargo.toml` 的 `[workspace.package] version` | 安装包文件名、exe 属性、检查更新、镜像标签 | 每次发版 |
| 协议版本 | `PROTOCOL_VERSION`（[`control.rs`](crates/protocol/src/control.rs)） | 客户端和服务端连不连得上 | 控制面或语音加密规则不兼容时 |

客户端和服务端共用一个产品版本，一起发。1.0 之前这样定号：

- **`0.x.Y`**：修 bug、加功能，**协议和存档格式都兼容**。客户端无需同步升级；服务器 `pull` 后重启仍会短暂断线
- **`0.X.0`**：动了 `PROTOCOL_VERSION`，或者 SQLite 表结构变了。服务器一升级旧客户端就连不上，
  换回旧镜像也可能读不了新存档 —— Release 说明里写清楚「先升客户端」「升级前先备份
  `gouhuo-data`」

步骤：

```bash
# 1. 改 Cargo.toml 的 version，cargo check 顺带更新 Cargo.lock，走 PR 合进 main
# 2. 在更新后的 main 上打标签（必须跟 Cargo.toml 一致，CI 会查）
git tag -a v0.1.0 -m "篝火 0.1.0"
git push origin v0.1.0
```

推标签后 [`release.yml`](.github/workflows/release.yml) 编安装包和 Windows 服务端，建一个
**草稿** Release 挂上去，同时推 `ghcr.io/parz1/gouhuo-server:<版本>` 镜像。自己下下来装一遍、
写好说明，再在网页上点「发布」。这时才会：

- 客户端「检查更新」看得到它（草稿不算）
- `latest` 指过去（[`docker-latest.yml`](.github/workflows/docker-latest.yml)），跟着 `latest`
  的服务器下次 `pull` 升级

**预发布**：标签和 `Cargo.toml` 都写成 `0.2.0-beta.1` 这种，Release 勾上 pre-release。
`latest` 不挪，客户端检查更新也会跳过带 `-` 的版本；想试的服务器在 `.env` 里写
`GOUHUO_VERSION=0.2.0-beta.1`。

第一次推完镜像要去 GitHub → Packages → gouhuo-server → Package settings 把它改成
**Public**，否则服务器上 pull 会报 `unauthorized`。只要做一次。

---

## 路线图

**下一步：0.2.0 小规模实战验证。**
协议 v2 分离语音/保活 nonce 域，发送序号与采样时钟随连接密钥保存；
服务端限制未认证连接、覆盖整条认证截止时间、隔离慢客户端，并保证状态广播顺序。
客户端增加 UDP 超时说明、音频故障重试和可复制诊断信息。
自动测试与真人验收分开记录，见 [0.2.0 升级与验收](docs/release-0.2.0.md)。

**M1** ✅ 协议链路延迟下限
**M2** ✅ 设备延迟 + APM 分项实测，合计预算在产品线内（完整双机嘴到耳待测）

**M3** 控制面、服务端、一键加入 —— 进行中

- ✅ 身份（Ed25519 + DPAPI + 导出导入）、证书指纹、邀请链接
- ✅ 控制面消息（protobuf）+ 分帧
- ✅ TLS 握手 + 证书固定 + 从 TLS 派生 UDP 密钥，整条信任链有端到端测试
- ✅ 服务端：频道树、成员状态、认证、文字消息
- ✅ UDP 语音转发（SFU 核心）：每连接密钥、防重放、保活探测
- ✅ 多频道：成员能建，建的人和频道管理能删
- ✅ SQLite 持久化：频道树和谁建的，重启还在
- ⬜ UDP 不通时退回 TCP 传语音
- ✅ 改频道、踢人、封禁；管理员链接开荒，管理员能升降角色

**M4** 客户端 —— 进行中

- ✅ 连接、认证、状态镜像（对着真服务端有端到端测试）
- ✅ Slint 界面：频道树、成员名单、文字、一键加入。冷启动 92 ms / 常驻 59.4 MB
- ✅ 音频链路端到端 36.7 ms 实测（只比理论下限大 0.2 ms）
- ✅ 全局热键（Raw Input）：按住说话、绑键、持久化
- ✅ 试麦、电平条、设备选择、语音激活灵敏度
- ✅ 建频道 / 删频道
- ✅ 断线自动重连：回到原频道、保持闭麦；顶号 / 被踢不重连；半死的连接 15 秒内发现
- ✅ 每个人单独调音量（0–400%、一键静音；按公钥记住，换服务器也认得）
- ✅ 进出提示音 + 念名字（Windows 自带语音合成）：只报自己所在的频道，混进播放那一路让回声消除认得它
- ✅ 托盘：点 × 第一次问「收到托盘还是退出」，能记住；托盘图标看得出麦克风开没开

**M5** ✅ 自适应抖动缓冲 + PLC：抖一小时，延迟回到起点（固定缓冲 30 → 220 ms，自适应 33 → 38 ms，[量法](docs/measurements.md#m5-结论自适应抖动缓冲)）
**M6** 打包、安装包体积实测、游戏帧时间影响实测 —— 进行中

- ✅ 静态链接 CRT：没装 VC++ 运行库的机器上也能开
- ✅ 内存和后台开销实测：在频道里 73 MB，收到托盘挂 48 分钟不涨（[量法](docs/measurements.md#m6客户端的内存和后台开销)）
- ✅ 安装包：按用户装、不弹 UAC；注册 `gouhuo://` 协议；只开一个实例，链接转交给开着的那个；启动时检查更新
- ✅ 安装包体积：0.1.1 发布包约 7.9 MB；0.2.0 本地验收包约 7.94 MB
- ⬜ 跟真游戏一起跑时的帧时间影响
**M7** 游戏 SDK：让游戏开发者把篝火嵌进自己的游戏当语音系统 —— 不注册、不收费、服务器自己架

### 平台

不按平台划线，**性能是唯一的门槛**：哪个平台能达到上面的产品线，就可以做；
达不到的，先不发。

客户端目前只有 Windows 实现（WASAPI、DPAPI、Raw Input 热键）。这是先后顺序，
不是范围：平台相关的代码基本收在 `voice-core`（音频设备、热键、时钟、身份落盘），
别的平台是往那里加实现。服务端要能跑在 Linux 上（专用服务器基本都是 Linux）。

### 明确不做

视频、屏幕共享、动态 / 帖子 / 社区发现 / 好友系统、装扮和表情商店、富文本、
长期消息历史、直播。

---

## 许可

**协议宽松，引擎能嵌入，成品 copyleft。**

| 部分 | 许可 |
|---|---|
| `crates/protocol` | MIT OR Apache-2.0 |
| `crates/voice-core`、`crates/transport`、`crates/server`、`crates/client-core` | MPL-2.0 |
| 其余（客户端界面、探针） | GPL-3.0-or-later |

`protocol` 放开是为了让别人能自由写第三方客户端、机器人、别的语言的实现。
引擎、服务端和客户端逻辑是 MPL，可以链进任何产品（包括把语音嵌进闭源游戏）、
可以随便自部署（包括闭源商业场景），只有改了这些 crate 里的文件才要公开那些改动 ——
自部署是核心卖点，不该在公司 IT 的许可白名单那里被卡住。客户端成品是 GPL，
不想被套壳加广告再分发。

**对用户没有任何影响**：copyleft 的义务只在分发时产生。下载、安装、自己架服务器
给朋友用都没有义务；把改过的版本发给别人才要带源码。

完整说明见 [`LICENSING.md`](LICENSING.md)。

## 贡献

见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。用 DCO（`git commit -s`），不用 CLA。

提交说明用 [Conventional Commits](https://www.conventionalcommits.org/zh-hans/v1.0.0/)
格式（`feat(client): ...`、`fix(voice-core): ...`），正文写**为什么**，不写改了什么
—— 改了什么 diff 里看得见。涉及性能的改动请带上数字：哪个配置、测了多久、跟之前比是多少。
