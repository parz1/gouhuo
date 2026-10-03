# Linux 服务端

发布包 `gouhuo-server-<版本>-linux-x64-musl.tar.gz` 面向 Linux x86_64。
服务端静态链接 musl，不需要安装 Rust、glibc 或额外动态库。

```bash
sha256sum -c gouhuo-server-<版本>-linux-x64-musl.tar.gz.sha256
tar -xzf gouhuo-server-<版本>-linux-x64-musl.tar.gz
GOUHUO_HOST=你的公网地址 GOUHUO_DATA=./gouhuo-data ./gouhuo-server
```

放行 `20800/tcp` 和 `20800/udp`；无需 root 运行。
首次启动会生成证书、存档与邀请链接。数据目录包含私钥和管理凭据，应只允许运行账号访问。
升级或回退前停止服务并备份整个数据目录；保持目录不变才能保留证书和用户权限。
完整部署说明、HTTPS 加入页、环境变量和源码见[篝火仓库](https://github.com/parz1/gouhuo)。

从源码构建（Ubuntu / Debian）：

```bash
sudo apt-get install musl-tools
rustup target add x86_64-unknown-linux-musl
bash packaging/linux/build.sh
# 不包含网页加入功能：
bash packaging/linux/build.sh --no-default-features
```

脚本检查 ELF 不含动态加载器和共享库依赖，生成压缩包及 SHA-256 文件。
关闭网页功能的包名带 `-no-web`，与默认构建分开保存。
Linux 会按系统 `net.core.rmem_max` 限制接收缓冲；设置成功不代表一定分配到请求的 1 MiB。
