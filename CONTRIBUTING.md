# 参与开发

## 提交要签 DCO

这个项目用 **DCO**（Developer Certificate of Origin），**不用 CLA**。

区别：CLA 是把版权授予项目方，DCO 只是你声明「这段代码我有权提交」。
**贡献者保留自己的版权。**

做法就是提交时加一行：

```bash
git commit -s -m "你的提交说明"
```

`-s` 会自动加上：

```
Signed-off-by: 你的名字 <你的邮箱@example.com>
```

加上这一行，意思是你确认下面这份声明（原文见
[developercertificate.org](https://developercertificate.org/)）：

> 1. 这段代码整体或部分是我写的，我有权按本文件里标明的开源许可提交它；或者
> 2. 这段代码基于我知晓的、许可合适的已有作品，我有权按该许可（或本项目的许可）提交它；或者
> 3. 这段代码是别人按上面两条之一提供给我的，我没有修改过它；并且
> 4. 我明白这次贡献和它附带的信息是公开的，会被永久保留，并且可能被再分发。

名字用真名或你长期使用的网名都行，邮箱要是能联系到你的。

CI 会检查每个提交有没有 `Signed-off-by`。忘了的话：

```bash
git commit --amend -s        # 最近一个提交
git rebase --signoff main    # 一串提交
```

## 你的代码会是什么许可

取决于改的是哪一部分，见 [`LICENSING.md`](LICENSING.md)：

- `crates/protocol` → **MIT OR Apache-2.0**
- `crates/voice-core`、`crates/transport`、`crates/server` → **MPL-2.0**
- 其余（客户端、探针）→ **GPL-3.0-or-later**

签了 DCO 就表示你同意按对应的许可提交。**新文件请照抄同目录下已有文件的
`SPDX-License-Identifier` 头** —— MPL 是文件级 copyleft，漏了那行会让边界变模糊。

## 本地要过的检查

CI 卡这四条，本地先跑一遍省事：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python scripts/check-spdx.py
```

最后那条查的是每个 `.rs` 文件头的 SPDX 跟所属 crate 的许可对不对得上。
期望值是从 `cargo metadata` 读出来的，所以改了某个 crate 的许可，
这个检查会自动跟着变，不用改脚本。

构建不需要额外准备 —— `cargo build` 就行。唯一可能缺的是 cmake
（`audiopus_sys` 从源码编 libopus 要用），缺了就跑
[`scripts/win-buildenv.ps1`](scripts/win-buildenv.ps1)，它会去 Visual Studio
里找一份。APM 曾经要一整套 C++ 工具链，换成纯 Rust 之后不需要了，
经过见 [`docs/apm-backend.md`](docs/apm-backend.md)。

## 改到红线相关的东西

`crates/voice-core/src/redline.rs` 里的常量分两类，**性质不一样，别混**：

- **产品线**：对用户的承诺。改它是产品决定，要在 PR 里说清楚理由
- **防回归闸**（`GATE_*`）：CI 卡的线。**它变松只有一个正当理由：
  重新测过，并且把 `MEASURED_*` 一起更新了。** 为了让 CI 变绿而调松闸，
  等于把这套东西的作用整个抹掉

常量之间的不变量是编译期断言，调错了当场编译失败。

## 提交说明

格式用 [Conventional Commits](https://www.conventionalcommits.org/zh-hans/v1.0.0/)：

```
<type>(<scope>): <一句话说清楚这次提交>

<正文：为什么>

Signed-off-by: ...
```

- **type**：`feat` 新功能、`fix` 修 bug、`perf` 性能、`refactor` 重构、
  `test` 测试、`docs` 文档、`ci` CI、`build` 构建和依赖、`chore` 杂项
- **scope**：改的是哪个 crate，比如 `voice-core`、`server`、`client`、`protocol`；
  跨好几个或者不属于任何 crate 就省掉
- 摘要行可以写中文，不加句号。破坏兼容的改动在 type 后面加 `!`，
  并在正文里写 `BREAKING CHANGE: ...`（改 `protocol` 的线上格式尤其要写）

例子：

```
fix(voice-core): 采样率交给 Windows 转，播放坏了不再拖死采集
perf(voice-core): APM 换成纯 Rust 的 sonora
docs: README 重写成项目说明
```

正文说清楚**为什么**，不是**改了什么** —— 改了什么 diff 里看得见。

这个项目的每个结论都是测出来的，涉及性能的改动请带上数字：
哪个配置、测了多久、跟之前比是多少。
