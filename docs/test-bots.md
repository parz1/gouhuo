# 无界面测试机器人

`gouhuo-bot` 是 issue #20 的第一阶段实现，用真实 TLS 认证、加密 UDP 和客户端运行层测试多人语音。每个机器人使用独立的临时身份，不打开麦克风或扬声器，也不保存身份。Windows 和 Linux 均可运行。

先将自己的测试服务器邀请码放进 `invite.txt`，再运行：

```powershell
cargo run --release -p test-bot -- --invite-file invite.txt --room 大厅 --count 8 --seconds 60 --log bots.jsonl
```

默认循环发送合成扫频音。`--play voice.wav` 可改为循环录音：16 位 PCM WAV，8–96 kHz，最多 10 MiB、300 秒，多声道会混为单声道并重采样至 48 kHz。所有机器人共享解码后的录音。

`--room` 接受已有频道名称或 ID，同名频道必须用 ID；省略时留在服务端默认频道。目前不创建临时房间。`--count` 为 1–256，默认运行 60 秒，结束后断开连接；这只是数量上限，尚未测定几百机器人所需的 CPU、内存和带宽。

回声测试只启动一个机器人：

```powershell
cargo run --release -p test-bot -- --invite-file invite.txt --echo --gain 0.5 --seconds 60
```

它将收到的混音延迟约 200 ms 后发回。真人加入同一频道即可听到回声。不要在同一频道启动多个回声机器人，以免互相反馈。`--silent` 仅接收，不能与 `--play` 或 `--echo` 一起使用。

## 模拟单向中断和重连

在一个终端启动正常发送者，在另一个终端启动以下接收者：

```powershell
cargo run --release -p test-bot -- --invite-file invite.txt --silent --seconds 30 --outage-at 2 --outage-seconds 18 --outage-direction up --log outage.jsonl
```

从语音启动后第 2 秒到第 20 秒，接收者的上行 UDP 全部丢弃，下行和 TCP 继续通行。这覆盖“还听得到队友，但服务器收不到我的保活”的恢复路径。运行层应报告失联、执行有限的 TCP 重连，并在中断结束后恢复；纯 UDP 故障期间不应反复重开音频。

代理支持 `--loss 5 --delay-ms 30 --jitter-ms 10 --seed 42`，分别表示每个方向 5% 丢包、30 ms 单向延迟、标准差 10 ms 的高斯抖动。延迟截在 0–5000 ms，各方向最多排队 512 包。中断方向可选 `up`、`down`、`both`。丢包与抖动复用 latency-probe 的 netem；同一输入包序列与种子可重放随机决策，真实网络调度和包到达时间仍会变化。

## 读诊断

输出为 JSONL；`--log` 只创建新文件，防止覆盖已有结果。包含 `connected`、每秒 `sample`、`recovery`、`tcp_reconnecting`、`tcp_reconnected`、`finished` 等事件。

- `udp_ok` / `udp_failed`、`stage`：当前语音链路状态。
- `sent` / `received`、`rtt_ms`、`underruns`：当前语音实例统计。
- `generation`：语音实例代数，重连后增加；`audio_opens`、`audible_frames` 是整次机器人运行的累计值。
- `up` / `down`：代理接受、丢弃、送达、队列溢出和发送失败计数；代理随重连重建，计数从零开始，尚未到期的排队包不计为送达。
- `server_udp_received`：服务端反馈的上行统计。

诊断不输出邀请码、密钥、聊天消息或音频内容。仍应妥善保管邀请码文件。正常到期会清理连接；强制结束进程依赖操作系统回收资源。

```powershell
cargo test -p test-bot
cargo run -p test-bot -- --help
```

测试包含真实服务端多人加密语音、回声、单向中断后的恢复、退出清理及参数校验。机器人替代了声卡，因此不能证明虚拟声卡的设备兼容性；该问题还需要异常设备上的诊断与真人回归。
