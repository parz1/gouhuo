# Cloudflare R2 更新分发

本项目的默认更新入口为 **https://downloads.gouhuo.minerei.dev/stable.json**。
客户端优先读取这个 HTTPS 清单，失败或清单无效时回退 GitHub 正式 Release。
清单有效时以它为准，包括「没有新版本」；不会因 GitHub 先发布而提前提示尚未同步的安装包。
下载按钮打开浏览器下载安装包，客户端不会下载、校验或执行安装包。

## 1. 确认 Cloudflare 域名

当前使用的 `minerei.dev` 已在 Cloudflare。确认站点处于 Active 状态，且与 R2 在同一个
Cloudflare 账号后，**直接进入第 2 节绑定 R2，无需迁移 DNS**。

下面的迁移步骤仅供以后使用尚未接入 Cloudflare 的其他域名参考。
域名的注册和续费继续留在原注册商，免费套餐采用完整 DNS 接入：

1. 在 Cloudflare 添加根域名 **`minerei.dev`**，选择 Free。
   `downloads.gouhuo.minerei.dev` 是这个根域名下的子域名，不用作为独立站点添加。
2. 从当前 DNS 服务商导出 DNS 记录，完整核对并复制到 Cloudflare：A、AAAA、CNAME、MX、TXT、
   SRV 及相关子域委派等。不要只依赖自动扫描，尤其要核对邮箱和验证记录。
3. 已有网站的 A/AAAA/CNAME 可以设为 **DNS only（灰云）**，保持访问原服务器。
   篝火语音服务器、邮箱等非普通 HTTP 服务的记录也保持 DNS only。
4. 如果原域名启用了 DNSSEC，先在域名注册商处关闭并移除旧 DS 记录，再修改 Nameserver。
5. 域名注册商控制台 → `minerei.dev` → DNS 服务器修改，替换成 Cloudflare 给出的
   **两个准确的 Nameserver**。不要照抄其他域名的 Nameserver。
6. 等 Cloudflare 显示 Active，确认原网站和邮箱记录仍正常。之后可按 Cloudflare
   指引重新启用 DNSSEC，并在注册商处填写它提供的新 DS 记录。

官方步骤：[完整 DNS 接入](https://developers.cloudflare.com/dns/zone-setups/full-setup/setup/)。
如果保留原来的权威 DNS，官方 Partial/CNAME 接入需要 Business 或 Enterprise，
见 [套餐限制](https://developers.cloudflare.com/dns/zone-setups/partial-setup/)。

## 2. 创建 R2 Bucket 并绑定下载域名

1. 同一个 Cloudflare 账号 → R2 → 创建 Bucket，建议名 **`gouhuo-releases`**。
   选择普通 Default jurisdiction，本仓库发布脚本使用对应的标准 S3 endpoint。
2. Bucket → Settings → Custom Domains → Add，填写
   **`downloads.gouhuo.minerei.dev`**，确认并等待域名状态变为 Active。
3. 使用 R2 的 Custom Domains 接入，不要自行 CNAME 到 `r2.dev`。
   `r2.dev` 是开发测试入口，正式分发不需要开启它。
4. 这个下载域名要允许普通 HTTPS 客户端公开读取。不要对下载域名启用登录限制、
   JavaScript 验证或交互式挑战；WinHTTP 更新检查不能完成浏览器挑战。

官方说明：[R2 自定义域名](https://developers.cloudflare.com/r2/buckets/public-buckets/)。
域名原来能在大陆访问不代表改接 Cloudflare 后仍有相同表现；绑定后再测大陆线路。

## 3. 配置缓存

Cloudflare → `minerei.dev` → Caching → Cache Rules：

- **更新清单**：匹配 Hostname 等于 `downloads.gouhuo.minerei.dev`，且 URI Path 等于
  `/stable.json`，选择 **Bypass cache**。若有覆盖整个下载域名的规则，确保清单的
  Bypass 规则最终生效，清理之前缓存过的 `stable.json`。
- **安装包**：匹配同一 Hostname、路径以 `/releases/` 开头且以 `.exe` 或 `.tar.gz`
  结尾，选择 Eligible for cache，Edge TTL 尊重源站 Cache-Control。
  脚本写入 `public, max-age=31536000, immutable`。
- **说明页**：脚本写入 `public, max-age=300`。不用把它和安装包一起强制长缓存。

清单的响应头是 `no-store, no-cache, max-age=0, must-revalidate`。
每版包使用独立路径，已存在的包必须有相同 SHA-256；脚本拒绝用不同字节覆盖它。
官方说明：[Cache Rules](https://developers.cloudflare.com/cache/how-to/cache-rules/settings/)。

## 4. 创建发布凭据

R2 → Manage R2 API Tokens → 创建 Token：

- 权限选择 **Object Read & Write**。
- 只允许操作 `gouhuo-releases` 这个 Bucket。
- 保存生成的 **Access Key ID** 和 **Secret Access Key**，给下面 GitHub Secrets 使用。
- 记录 Cloudflare 的 Account ID（不是 Zone ID，也不是 Token）。

不把密钥发到聊天、不放进仓库或客户端。
官方说明：[R2 凭据](https://developers.cloudflare.com/r2/api/tokens/)。

## 5. 配置 GitHub Actions

GitHub 仓库 → Settings → Secrets and variables → Actions。

在 **Variables** 中添加：

| 名称 | 值 |
|---|---|
| `UPDATE_BASE_URL` | `https://downloads.gouhuo.minerei.dev`（只有源站地址，不带路径） |
| `R2_ACCOUNT_ID` | Cloudflare 的 32 位 Account ID |
| `R2_BUCKET` | `gouhuo-releases` |

在 **Secrets** 中添加：

| 名称 | 值 |
|---|---|
| `R2_ACCESS_KEY_ID` | R2 Access Key ID |
| `R2_SECRET_ACCESS_KEY` | R2 Secret Access Key |

`R2_BUCKET` 未配置时，R2 发布 job 会跳过；原 GitHub 发布不受影响。
`UPDATE_BASE_URL` 同时用于安装包构建时的更新入口和 R2 清单中的公开 URL。
未设置时两者默认使用 `https://downloads.gouhuo.minerei.dev`。

## 6. 第一次发布与以后更新

1. 本次改动合入默认分支，按原流程更新 `Cargo.toml` 版本、打 `v*` 标签。
2. `release.yml` 构建含新更新逻辑的客户端，并创建草稿 Release。
3. 下载安装验证后，填写发布说明、确认是正式版，在 GitHub 点 Publish。
4. `publish-updates` 自动下载 **当前最新正式 Release** 的全部附件，生成更新清单与
   说明页，上传到 R2，逐个验证公开下载的文件大小与 SHA-256。
5. 安装包及说明页可访问后，最后写入 `stable.json`。再次公开读取清单，确认缓存没有
   返回旧版本。失败时 workflow 标红，不继续推进尚未验证的发布。

首次补传或修复配置后的重试：GitHub → Actions → `publish-updates` → Run workflow，
选择默认分支，在 `tag` 填当前最新正式版本，例如 `v0.3.1`。
草稿、预发布和旧版本不能被这条流程提升为 stable。
补传现有 Release 不会改变安装包内部逻辑；旧客户端要先安装包含本次改动的新版本，
才能通过这个域名检查更新。

Bucket 的结构：

```text
stable.json
releases/0.4.0/index.html
releases/0.4.0/gouhuo-setup-0.4.0.exe
releases/0.4.0/<其他 GitHub Release 附件>
```

这里的 `0.4.0` 是目录示例，不代表已发布版本。

## 7. 验证与本地开发

打开 `https://downloads.gouhuo.minerei.dev/stable.json` 应返回 JSON，而不是登录页、
验证页或 HTML 错误页。访问 JSON 里的 `release_url` 和安装包 URL，确认下载正常。
在大陆电信、联通、移动网络、不使用代理的情况下，重复测试清单和完整安装包下载。

本地可覆盖默认更新入口；URL 必须是 HTTPS 源站地址：

```powershell
$env:GOUHUO_UPDATE_BASE_URL = 'https://downloads.gouhuo.minerei.dev'
cargo run -p client --bin gouhuo
```

运行时环境变量优先于编译时配置。发布构建默认检查，调试构建默认不自动检查；
手动检查始终可用，明确设置运行时环境变量也可启用调试构建的启动检查。
为空的运行时变量可跳过 R2、直接走 GitHub，方便排查。

只在本地生成清单与说明页，不上传：先准备 GitHub Release API JSON 和对应的附件目录，
然后运行：

```powershell
python packaging/updates/publish.py --release-json release.json --assets release-assets --output update-output --base-url https://downloads.gouhuo.minerei.dev
python -m unittest discover -s packaging/updates -p 'test_*.py'
cargo test -p client --bin gouhuo update::tests
```

清单 schema 1 含 `version`、`notes`、`release_url` 和 `files.windows-x64` 的 URL、
字节数、SHA-256。客户端限制清单为 64 KiB、要求包与说明页 URL 和更新源同源，
拒绝预发布版本、无效哈希和交互式验证页。当前 SHA-256 由发布流程校验，客户端
仅验证字段格式；将来做应用内下载时，还需要在客户端校验文件字节和签名。

本地验证不代表已完成 Cloudflare、域名注册商或 GitHub 配置；只有实际绑定和发布后才能
验证线上更新链路。
