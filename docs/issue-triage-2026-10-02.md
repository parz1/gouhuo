# Issue 核查与实施顺序（2026-10-02）

基线：GitHub main `9f70b87b1cbd192754e2e32855bb91c99144ed67`，正式版 0.2.2。
核查了全部 17 个开放 issue、已有评论、开放 PR（无）、发布资产与工作流。
本地尚未提交的连接恢复等修改不作为完成依据。本次没有重新运行测试；
自动验证依据为 [main CI](https://github.com/parz1/gouhuo/actions/runs/36806821305)：
deterministic、license 成功，专用 gate 跳过。仓库未配置专用 runner 或变量。

## 可关闭

- [#26](https://github.com/parz1/gouhuo/issues/26)：五项修复已进入 0.2.0 并保留在 main。
  `protocol/src/crypto.rs` 隔离语音/保活 nonce 域，并测试并发唯一性；发送序号跨
  Pipeline 重建保存且不回绕。`server/src/conn.rs` 覆盖四阶段认证静默、待认证上限、
  慢客户端队列隔离、提交/广播排序与 Welcome 订阅衔接；`transport` 覆盖 IO 关闭/期限。
  `client-core/tests/voice_pipeline.rs` 覆盖同会话重建设备及闭麦/PTT。
  README 和测量文档已区分延迟分项合计与真实双机测量。
  真实设备开黑与发布人工验收仍按 `docs/release-0.2.0.md` 跟进。

## 部分完成，保持开放

| Issue | 已有实现 | 剩余工作与建议 |
|---|---|---|
| [#14 Linux 服务端](https://github.com/parz1/gouhuo/issues/14) | Dockerfile、标签构建发布、compose、一键部署和防火墙文档 | 增加 Ubuntu 独立 build/test（含 web 默认和禁用两种编法）；补 Unix `SO_RCVBUF`，当前非 Windows 分支直接 `Ok(())`；发布 x86_64 musl 二进制；Linux 服务端 + Windows 客户端实测及升级/回退验收。Docker 构建不能替代测试。 |
| [#7 TCP 回退](https://github.com/parz1/gouhuo/issues/7) | UDP 超时提示已实现 | TLS 语音封装、服务端转发、路径切换、UI 提示和测量均未实现。先定有界语音队列与控制消息优先级，避免语音积压堵住控制面；验证双向/单向 UDP 阻断、恢复切换、无重复播放和退出。加协议字段与帧类型时核对旧端兼容，不直接假定可无缝加帧。 |
| [#35 音质](https://github.com/parz1/gouhuo/issues/35) | 连续限幅、真实启用 AGC、64 kbps Audio+Auto、VAD 尾音保护、离线 WAV 对比和合成回归 | 真人语料盲听、当前配置 CPU/完整双机延迟/外放验证、可信音质门禁，以及 DTX 接线仍未完成。`FLAG_DTX`/`is_dtx` 尚无线上调用，不能以 VAD 静音带宽验收代替。文档末尾仍有第一轮 32 kbps 复测说明，后续应按当前 64 kbps 验收。 |

## 未完成的客户端与验证工作

| Issue | 实施建议与验收 |
|---|---|
| [#24 FEC](https://github.com/parz1/gouhuo/issues/24) | `decode_fec`、`set_expected_loss` 存在但生产路径没有调用。先用真实 Opus 包确认当前 10 ms Audio+Auto 配置能提供什么恢复能力，再加 jitter 下一包窥视与接收 FEC 路径，避免提前消费下一帧。最后加逐说话人滑窗统计、Ping/Pong 可选反馈字段、同频道汇总与反馈过期规则、平滑/上限。用相同语料和丢包序列对照 FEC/PLC，不能只验证输出有声音。 |
| [#12 游戏帧时间](https://github.com/parz1/gouhuo/issues/12) | 需要真实游戏和设备。固定地图、画质、帧率上限与后台负载；不开/静音/多人通话三组重复采样，报告平均、p95/p99、1%/0.1% low，并核对热键输入影响。保存环境与原始数据，不用合成测试宣称完成。 |
| [#13 专用延迟 runner](https://github.com/parz1/gouhuo/issues/13) | workflow 已留 gate，但没有 runner/变量，当前跳过。准备独占 Windows 主机后先手动启用 `DEDICATED_RUNNER`，跑 30 秒门禁并存基线，再确定自动触发策略和互斥；主机资源及调度未落实前保持开放。 |
| [#20 测试机器人](https://github.com/parz1/gouhuo/issues/20) | 第一阶段直接复用现有身份/邀请/频道与 `client-core`，无声卡播放 WAV、回声模式、人数与确定性损伤参数；不必等新的房间模型才能测试当前产品。随后支持 #16/#17 并补负载、资源及广播延迟统计。机器人用的收发路径需与真实客户端一致。 |

## SDK 依赖与实施拆分

当前没有 sdk crate、C 头文件或引擎插件；这些 issue 不能因底层 crate 已存在而关闭。

1. [#15 C ABI](https://github.com/parz1/gouhuo/issues/15)：先交付 Windows MVP，封装
   现有邀请/频道接入、轮询事件、闭麦/PTT、逐人音量与设备操作。定义句柄生命周期、
   缓冲区所有权、UTF-8、错误码、ABI 版本与 panic 边界；用 C 双进程通话验收。
   `join(room,ticket)` 在 #16/#17 完成后交付，避免它们与 #15 形成阻塞循环。
2. [#16 票据鉴权](https://github.com/parz1/gouhuo/issues/16)：协议先确定签名字节、
   版本、过期、签发者和权限约束；离线验签与现有邀请模式共存。开发密钥应显式
   启用并限制本地开发用途；覆盖篡改、过期、错误签发者、越权房间和身份绑定。
3. [#17 多房间](https://github.com/parz1/gouhuo/issues/17)：先状态机多对多、空房销毁、
   每房间听/说权限，再统一频道兼容与 UDP 路由。覆盖权限收回、离房、断线清理、
   临时房不落库、同包多房订阅去重以及旧客户端行为。更新历史 39 kbps 验收口径。
4. [#18 位置语音与 PCM 输出](https://github.com/parz1/gouhuo/issues/18)：先分离每人
   解码和最终混音，定义 PCM 拉取时钟/欠载行为，再做声像、衰减与参考信号回灌。
   服务端距离裁剪依赖 #17，最终混音/AEC 一起测试，报告多人 CPU 增量。
5. [#19 Unity](https://github.com/parz1/gouhuo/issues/19)：#15–#17 成立后先做
   P/Invoke、组件、事件轮询与本地 server 生命周期；验证 Play/Stop、编辑器崩溃、
   多实例共用与构建产物。近距离示例依赖 #18，不必阻塞第一版频道通话体验。
6. [#22 跨平台](https://github.com/parz1/gouhuo/issues/22)：先抽象设备和密钥存储并
   保持 Windows 性能，再按需求逐平台实现/实测。非 Windows 身份明文存储仅为
   CI 用途，不能直接作为产品交付；各平台原生密钥库和设备测试是发布条件。
7. [#21 Steam](https://github.com/parz1/gouhuo/issues/21)：依赖 #16/#17，后续调研
   ticket 生命周期、大厅成员验证与 SDK 接入；Relay 路径独立测量后决定，不能
   先把“无需端口转发”作为已支持功能。开工时查阅官方当期接口文档。
8. [#23 Unreal/Godot](https://github.com/parz1/gouhuo/issues/23)：在 #15 ABI 稳定、
   #19 示例体验跑通后，按实际引擎需求分别实现薄包装和示例。

## 建议执行顺序

先补 #14 的 Ubuntu 测试/缓冲设置，同时完成 #35 真人音质验收。
然后做 #20 当前协议机器人，为 #24 损伤测试和多人资源验证提供可复现输入。
#7 根据实际 UDP 阻断反馈排期；#12/#13 分别需要游戏和专用机器，不能仅靠代码完成。
SDK 按 #15 → #16 → #17 → #18/#19 推进，#22 与目标平台需求配合，最后接 #21/#23。

## 同日后续：#10 已完成

已补 `MEASURED_INSTALLER_MB = 8.023011` 和 `GATE_INSTALLER_MB = 10.0`。
`build.ps1` 调用新增 `check-size.ps1`，统一从 `redline.rs` 读取阈值，按十进制 MB
检查完整安装包；达到回归闸或 60 MB 产品线时抛错，阻断 Windows Release 上传。
PR CI 新增 `test-size.ps1`，验证闸下、恰好达到闸、恰好达到产品线、空文件、
缺失常量与无效阈值六个场景。本地六项验证通过，Inno Setup 重打包
8,039,324 字节并通过实际体积检查；与正式 Release 包的大小分别记录。
格式检查、voice-core 无默认 feature 独立编译和 diff 空白检查通过。

其余为核查与实施建议，尚未实现上述剩余功能。
