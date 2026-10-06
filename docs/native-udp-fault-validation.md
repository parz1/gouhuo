# 本机原生 UI 的 UDP 故障验收

配合 `crates/server/examples/ui_fault_server.rs` 和 `scripts/udp_fault_relay.py`，
使用实际安装客户端、真实 TLS 认证、加密 UDP 与 Opus。故障工具不进入正式安装包。
所有监听仅在 IPv4 回环地址，不改防火墙、正式服务器或客户端协议。

## 启动顺序

1. `cargo build --locked -p server --example ui_fault_server`。
2. 启动 `ui_fault_server <隔离证据目录> 20898`。证书和本地邀请写入该目录，
   TLS 绑定 `127.0.0.1:20898`，实际语音服务绑定 `127.0.0.2:20898`。
3. 在目录中建立 `udp-mode.txt`，初始内容为 `pass`。启动
   `python -u scripts/udp_fault_relay.py --port 20898 --mode-file <目录>/udp-mode.txt`。
   中继绑定 `127.0.0.1:20898`；Welcome 中的 UDP 端口保持真实值，未改写 TLS 数据。
4. **先连接一个 GUI，再连接且仅连接一个 talker**。确认中继正常报告一个 GUI 端点、
   已有下行回应后才启动 talker。第二个 UDP 端点不受故障影响；其余端点包含 GUI
   自动恢复时新建的通路，均受故障影响。独立验收重新启动中继并遵守相同顺序。
5. talker 使用真实 `client-core` 及声音管线；`--gain 0` 保持实际包和解码而无听感验证。

将模式文件内容改为 `uplink`、`downlink` 或 `both`；约 100 ms 内生效。
保持至少超过现有 UDP 8 秒检测阈值，并覆盖自动恢复新端点。`pass` 解除故障。
核查通话页、详情／诊断和恢复按钮，保存故障与恢复图像，不将模拟预览作为该项证据。

中继每个端点使用独立、连接到后端的 UDP socket，回复从原服务地址发给 GUI。
包内容原样搬运，不保存音频、密钥或包体。最多 32 个端点，60 秒闲置回收，记录
UTC、模式、活跃端点数、按方向投递／丢弃数量、容量丢弃及错误数。
模式无效或无法读取时保留上一模式并报告，不能假定已经恢复。日志保存于隔离目录。

## 自动检查与限制

`python -m unittest discover -s scripts -p test_udp_fault_relay.py -v` 验证真实回环
socket 的字节／回复源地址、不串端点、三种方向的丢弃、保持 talker 正常、GUI 新端点
不能绕过故障、恢复不需重绑 socket、无效模式和非回环地址拒绝。
CI 执行该检查，服务端示例进入工作区 all-targets Clippy。

工具只用于一个 GUI 和一个 talker；第二端点豁免是显式验收约束，不能当通用网络模拟器。
本机故障不是运营商网络、真实游戏或主观听感测试。普通自动重连会短暂重新认证并取得
新会话；必须从当前 GUI／诊断和计数共同判定，不能把新端点绕过故障误算作恢复通过。

首轮工具仅阻断第一个 UDP 端点，GUI 自动恢复创建的新端点绕过了故障；该轮记录已保留
并标为无效。当前工具覆盖重试端点，方向与恢复回归检查均通过。
