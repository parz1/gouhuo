# 篝火 0.3.2

客户端声音处理运行在独立进程。UI 固定正式发布公钥，后台发现兼容内核，验证 Ed25519 签名、大小和 SHA-256，在通话、试麦和扫描结束后的空闲点切换；启动失败自动回退。

客户端 [v0.3.2](https://github.com/parz1/gouhuo/releases/tag/v0.3.2) 随包内核为 0.1.0。首个正式独立发布 [voice-v0.1.1](https://github.com/parz1/gouhuo/releases/tag/voice-v0.1.1) 要求 UI 至少 0.3.2，IPC 为 1，服务端协议为 2。其音频实现与随包内核保持一致，版本递增用于首次独立交付验收。后续兼容内核可独立发布，无需更新 UI。

## 下载与产物

- Windows 安装包：`gouhuo-setup-0.3.2.exe`，8,364,859 字节，SHA-256 `8230b85fefd1f1e4629b10e9fb143a96acc5c19c7a13328467b532839009938d`，通过原有 10 MB 门禁。
- 独立内核：`gouhuo-voice-0.1.1-windows-x64.exe`，1,869,824 字节，SHA-256 `e147a9e7b1dcaf3b425182126fca9ffbcf8023536f54ef7be6cbd0540439d924`。
- 服务端提供 Windows 可执行文件、Linux x64 musl 静态包和 `ghcr.io/parz1/gouhuo-server:0.3.2`。容器 `latest` 已成功指向本版。

安装包尚未做 Windows Authenticode 代码签名，SmartScreen 可能提示。声音内核更新使用独立的 Ed25519 签名验证。

## 验证范围

客户端及内核版本提交的 Windows/Linux CI 均通过，涵盖完整回归、Clippy、格式、许可、DCO、音质探针和独立版本更新/回退演练。[客户端检查](https://github.com/parz1/gouhuo/actions/runs/37402511019)、[内核检查](https://github.com/parz1/gouhuo/actions/runs/37402960434)、[客户端发布构建](https://github.com/parz1/gouhuo/actions/runs/37403294264)、[正式内核签名构建](https://github.com/parz1/gouhuo/actions/runs/37404012138)。

正式草稿在公开发布前，经过生产 EngineStore 校验签名、公钥、兼容范围、大小及 SHA-256。公开发布后，单独执行 `published_release_updates_idle_bundled_engine`：使用生产 WinHTTP 下载器访问公开 GitHub 发布，正式公钥验签，从冻结的 0.1.0 可执行文件空闲切换到下载的 0.1.1，更新后合成试麦可用。原内核 PID 11288，新内核 PID 4168，测试通过。没有启动图形 UI；此结果验证生产下载、存储和监督进程路径，不构成人工试听验收。

本地报告位于 `target/voice-release-verification/live-*/report.json`，最新目录保存于 `target/voice-release-verification/live-root.txt`。GitHub 最新客户端在独立内核发布后仍为 v0.3.2。

真实 WASAPI 设备生命周期和更新/回退演练已通过，记录见 [测量记录](measurements.md)。人工人声、耳机听感、真实声学 AEC、声学延迟及长期运行验收仍待后续完成。

首次内核发布发现 PowerShell 版本读取位置参数错误，以及 v* 标签过滤误匹配 voice-v*。已修正显式参数、补充现有标签重跑入口和独立内核排除条件。未移动已创建标签，也未覆盖容器 latest。
