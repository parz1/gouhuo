# Linux 服务端与 socket 验证（2026-10-04）

对应 #14 的自动化部分与 #45 的 socket 边界；工作区同时包含上一轮连接恢复修复。
本轮源码未提交、未推送，也未修改已有 Release。版本仍为 0.3.0，新增资产是本地源码构建，不能当作 0.3.0 正式发布资产。

## 实现

- Unix 的 `set_recv_buffer` 实际调用 `setsockopt(SO_RCVBUF)`，请求仍为 1 MiB，服从宿主机上限；不使用需要额外权限的 `SO_RCVBUFFORCE`。Windows / Unix 均拒绝零值和超出 i32 的值。
- UDP 唤醒包按目标地址族绑定 IPv4 或 IPv6 回环 socket，修复 IPv6 接收线程无法唤醒的问题。
- redline 新增独立 Linux 服务端矩阵，默认 / 无网页功能分别构建、Clippy、测试协议栈及 Unix socket，并检查 musl 静态产物。
- `packaging/linux/build.sh` 构建 x86_64 musl 服务端，拒绝带 ELF INTERP 或 NEEDED 依赖的产物，打包二进制、MPL 许可和部署说明，生成 SHA-256。无网页包带 `-no-web` 后缀。
- release 的 Linux 构建独立进行，等待 Windows 已创建草稿后才上传 Linux 压缩包与校验文件，保持已有草稿发布策略。实际 GitHub 上传需后续标签构建验证。

## 已执行验证

Linux 环境：Docker Desktop Linux / WSL2 `6.6.87.2-microsoft-standard-WSL2`，Rust 1.96.1；复用隔离验证镜像，未操作生产服务或数据。

| 项目 | 结果 |
|---|---|
| Windows 工作区严格 Clippy | 通过 |
| Windows 工作区测试 | 561 passed / 0 failed / 13 ignored |
| Linux 独立 socket 测试 | 8 passed / 0 failed |
| Linux 默认服务端 build / Clippy / 协议栈测试 | 通过；184 passed / 0 failed / 1 ignored |
| Linux 无网页服务端 build / Clippy / 协议栈测试 | 通过；179 passed / 0 failed / 1 ignored |
| Linux 内核 / client-core / client-runtime Clippy 与测试 | 通过；272 passed / 0 failed / 2 ignored |
| 工作流 actionlint | 通过；配置已有 dedicated-audio 自托管标签 |
| Rust 格式与 SPDX | 通过 |
| 默认 / 无网页 musl dist 构建与 ELF 静态检查 | 均通过 |
| 发布包解压后运行 | 默认包正常监听并提供 HTTP 发现端点；无网页包正常监听，设网页配置时明确退出 |
| 两份压缩包 SHA-256 | 容器校验通过；宿主机另行复核通过 |

证据保存在本地忽略目录 `target/issue-14-validation/`；最终 Linux 日志在 `final/`。
首次 Linux 测试因容器没有 `/proc/sys/net/core/rmem_max` 而失败，`unix-socket.log` / `run.log` 保留原记录。测试已改为验证真实 socket 的缓冲变化，不依赖容器是否公开 sysctl 文件；最终日志未覆盖首次失败。

首次 musl 构建和仅显式启用 `crt-static` 的复测都因 `musl-gcc` 包装器插入 ELF INTERP 而被静态检查拒绝，原始日志保留于 `final/musl-default.log`、`musl-run.log` 与 `musl-final/build-default-failed.log`。最终使用系统 `cc` 作为链接器、Rust 自带 musl CRT，显式 `crt-static` 与 `link-self-contained=yes`；本机最小程序和实际服务端均验证无 INTERP / NEEDED。最终脚本另验证 `readelf` 本身成功，不能以读取失败当作无动态依赖。

两份本地产物及构建 / 解压启动日志位于 `target/issue-14-validation/musl-final/`，总体验证输出为 `musl-smoke.log`。它们保留 0.3.0 版本号但包含未发布源码修复，仅供本轮验证，不覆盖正式发布文件。

## 尚待现场验证

Linux 服务端 + Windows 客户端的真实双机声卡通话、同城延迟与 Windows 服务端的公平对照、生产升级 / 回退，以及发布资产实际上传仍待完成。Docker 内的协议测试和合成声源不能替代这些项目；#14 应保持开放。#45 的原有 Linux 开发支持已在 0.3.0 中实现，本次补足 socket 设置和 IPv6 唤醒。
