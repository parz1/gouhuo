# 连接历史与原因模型（#47，进行中）

当前只完成结构化原因接入，尚未交付完整连接历史功能。

## 已接入

protocol::connection 定义稳定的 ConnectionReason、EvidenceSource 与 ConnectionCause。原因码由类型或协议枚举产生，不从翻译后的文案、服务端自由文本推断；导出用 code()，不依赖 Rust Debug 格式。LocalObservation 表示本端看到的事实，ServerConfirmed 表示服务端明确告知；超时不代表服务器崩溃或用户拔网线。UserAction 为后续主动退出/取消事件保留。

ConnectError::connection_cause() 对连接失败、TLS、证书、协议及认证拒绝进行分类，不携带地址、邀请、错误明细或服务端自由文本。未说明的拒绝和 Goodbye 保留 Unknown；读取时的 UnexpectedEof 记为 ReadError，不误当作正常远端关闭。

Ended::Refused 保留 cause：服务端 Goodbye 映射踢人、封禁、同身份替换；重连遇到不可重试的 ConnectError 也保留结构化原因。现有用户文案及重连策略未改，UI 消费端继续显示原有说明。此阶段没有改变线上 protobuf 字段。

## 验证

- client-core 库测试 54 项通过，包括本端失败不能冒充服务端诊断、自由文本不改变原因、不进入原因元数据。
- against_server 22、moderation 3、reconnect 8 项真实服务端测试通过；顶号、踢人、封禁分别断言确切原因与 ServerConfirmed 来源，并保持不偷偷重连。
- protocol/client-core all-targets Clippy、SPDX 与格式检查通过。
- client、client-runtime、test-bot all-targets 编译检查通过。

## 后续完整范围

仍需将读写/心跳故障与重连等待、尝试、恢复、主动退出/取消全过程接入结构化生命周期；添加尝试/代次及两端关联标识；默认有界历史、UTC与单调耗时、异步落盘/轮转/清除/跨重启读取；分别记录控制连接、UDP/TLS语音、设备及引擎状态；提供历史、摘要、脱敏导出及服务端结构化记录。覆盖真实故障、旧版本兼容、磁盘/队列失败和脱敏边界后才可关闭 #47。