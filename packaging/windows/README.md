# Windows 安装包

| 文件 | 是什么 |
|---|---|
| `build.ps1` | 入口。编 dist 版的 `gouhuo.exe`，从 `Cargo.toml` 读版本号，调 Inno Setup 打包 |
| `gouhuo.iss` | Inno Setup 脚本：装到哪、注册 `gouhuo://`、快捷方式、卸载时删什么 |
| `check-size.ps1` | 检查完整安装包体积，读取 redline.rs 的产品线与防回归闸 |
| `test-size.ps1` | 无需打包器的体积门禁边界测试，PR CI 自动运行 |
| `ChineseSimplified.isl` | 安装向导的简体中文界面文字 |

```powershell
.\packaging\windows\build.ps1              # → target\installer\gouhuo-setup-<版本>.exe
.\packaging\windows\build.ps1 -SkipBuild   # exe 已经编好了，只打包
```

打包结束自动检查完整安装包：防回归闸 **< 10 MB**，产品线 **< 60 MB**，
MB = 1,000,000 字节。阈值只定义在 `crates/voice-core/src/redline.rs`，
超限会使脚本失败；release.yml 调用此脚本，因此失败时不会上传 Windows 发布资产。
已有包可单独检查：`./packaging/windows/check-size.ps1 -InstallerPath target/installer/gouhuo-setup-0.2.2.exe`。
调整回归闸需重新测量并更新实测记录。

## 几个不能随手改的地方

- **`AppId` 的 GUID 永远别改。** Windows 靠它认出「这是同一个程序的新版本」，改了会装出两份
- **`AppMutex=GouhuoClientRunning`** 要跟 `crates/client/src/single_instance.rs` 里的
  `RUNNING_MUTEX` 一字不差。安装和卸载靠它发现篝火还开着，先请用户关掉
- **卸载不删 `%APPDATA%\gouhuo`**：那里是身份密钥

## `ChineseSimplified.isl` 的来源

Inno Setup 官方不自带简体中文，这份是社区维护的非官方翻译，原样取自上游仓库跟本机
Inno Setup 版本对应的标签：

<https://raw.githubusercontent.com/jrsoftware/issrc/is-6_7_3/Files/Languages/Unofficial/ChineseSimplified.isl>

跟 Inno Setup 一起按它的许可分发（<https://jrsoftware.org/files/is/license.txt>）。
升级 Inno Setup 大版本时，从对应标签重新取一份。
