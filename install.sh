#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# 在当前目录装一个篝火服务端，或者把装过的升级到最新版。一条命令：
#
#   curl -fsSL https://raw.githubusercontent.com/parz1/gouhuo/main/install.sh | sh
#
# 有域名就带上（推荐：换 IP 时旧邀请链接不会作废）：
#
#   curl -fsSL https://raw.githubusercontent.com/parz1/gouhuo/main/install.sh | GOUHUO_HOST=voice.example.com sh
#
# 做的事：下载 compose.yaml → 没有 .env 就建一个（公网地址自动探测）→ 拉镜像、
# 起服务 → 把邀请链接打出来。再跑一遍就是升级：.env 原样留着，只换 compose.yaml 和镜像。
#
# 不替你装 Docker、不动防火墙 —— 那两件事缺了会告诉你怎么做。

set -eu

RAW="${GOUHUO_RAW:-https://raw.githubusercontent.com/parz1/gouhuo/main}"

say() { printf '%s\n' "$*"; }
die() { printf '\n  ✗ %s\n\n' "$*" >&2; exit 1; }

# ---- Docker ----------------------------------------------------------------

command -v docker >/dev/null 2>&1 ||
    die "没装 Docker。装上再跑一次：curl -fsSL https://get.docker.com | sh"
docker compose version >/dev/null 2>&1 ||
    die "Docker 太旧，没有 compose 插件。装新版：curl -fsSL https://get.docker.com | sh"
docker info >/dev/null 2>&1 ||
    die "连不上 Docker。没开的话 systemctl start docker；不是 root 的话前面加 sudo，或者把自己加进 docker 组"

# ---- 公网地址 ---------------------------------------------------------------

# 内网 / 回环 / 运营商级 NAT 的地址写进邀请链接，外面的人连不上。
is_public() {
    case "$1" in
        '' | 10.* | 127.* | 169.254.* | 192.168.*) return 1 ;;
        172.1[6-9].* | 172.2[0-9].* | 172.3[01].*) return 1 ;;
        100.6[4-9].* | 100.[7-9][0-9].* | 100.1[01][0-9].* | 100.12[0-7].*) return 1 ;;
    esac
    return 0
}

detect_host() {
    # 先看本机网卡：独立服务器和一部分云主机，公网 IP 就挂在网卡上，不用问别人。
    ip=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')
    if is_public "$ip"; then
        say "$ip"
        return
    fi
    # 大多数云主机网卡上是内网地址，公网 IP 在外面做的 NAT —— 只能问外面。
    # 挨个试，国内国外各放几个。
    for url in https://api.ipify.org https://ifconfig.me/ip https://ip.3322.net https://4.ipw.cn; do
        ip=$(curl -4 -fsS --max-time 5 "$url" 2>/dev/null | tr -d ' \r\n') || continue
        case "$ip" in
            *[!0-9.]* | '') continue ;;
        esac
        if is_public "$ip"; then
            say "$ip"
            return
        fi
    done
}

# ---- 开始 -------------------------------------------------------------------

say ""
say "  篝火服务端 · 装在 $(pwd)"
say ""

say "  下载 compose.yaml"
curl -fsSL "$RAW/compose.yaml" -o compose.yaml.new || die "下载 $RAW/compose.yaml 失败"
mv compose.yaml.new compose.yaml

if [ -f .env ] && grep -q '^GOUHUO_HOST=.' .env; then
    if [ -n "${GOUHUO_HOST:-}" ]; then
        # 显式给了就换掉，其余设置不动。
        sed -i "s|^GOUHUO_HOST=.*|GOUHUO_HOST=$GOUHUO_HOST|" .env
        say "  .env 里的地址改成 $GOUHUO_HOST"
    else
        say "  沿用 .env（$(grep '^GOUHUO_HOST=' .env)）"
    fi
else
    host="${GOUHUO_HOST:-}"
    if [ -z "$host" ]; then
        say "  探测公网地址……"
        host=$(detect_host)
    fi
    if [ -z "$host" ] && [ -r /dev/tty ]; then
        # 管道进来的 stdin 是脚本本身，要从终端读。
        printf '  探测不到公网地址。输入服务器的公网 IP 或域名：' >/dev/tty
        read -r host </dev/tty || true
    fi
    [ -n "$host" ] || die "不知道公网地址。这样跑：curl ... | GOUHUO_HOST=你的公网IP或域名 sh"
    if [ -f .env ]; then
        printf 'GOUHUO_HOST=%s\n' "$host" >>.env
    else
        printf 'GOUHUO_HOST=%s\n' "$host" >.env
    fi
    say "  地址 $host 写进了 .env（不对就改 .env 再跑一次）"
fi

say "  拉镜像、起服务"
docker compose pull -q
docker compose up -d --remove-orphans

# ---- 邀请链接 ---------------------------------------------------------------

links=""
i=0
while [ $i -lt 30 ]; do
    links=$(docker compose logs --no-log-prefix gouhuo 2>/dev/null | grep -o 'gouhuo://[^ ]*' || true)
    [ -n "$links" ] && break
    i=$((i + 1))
    sleep 1
done
[ -n "$links" ] || die "30 秒了服务还没起来。看看日志：docker compose logs gouhuo"

invite=$(say "$links" | sed -n 1p)
admin=$(say "$links" | sed -n 2p)
port=$(sed -n 's/^GOUHUO_PORT=\([0-9]*\).*/\1/p' .env)
port="${port:-20800}"

say ""
say "  ✓ 起来了。把这一行发给朋友，粘进篝火就能进来："
say ""
say "      $invite"
say ""
if [ -n "$admin" ]; then
    say "  管理员链接（只给自己！用它连进去就成了管理员，用过一次作废）："
    say ""
    say "      $admin"
    say ""
fi
say "  还差一步：防火墙放行 $port 的 TCP 和 UDP。漏了 UDP = 能进频道、听不到声音。"
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q 'Status: active'; then
    say "    ufw:  ufw allow $port/tcp && ufw allow $port/udp"
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    say "    firewalld:  firewall-cmd --permanent --add-port=$port/tcp --add-port=$port/udp && firewall-cmd --reload"
fi
say "    云主机还要去控制台的「安全组」里放行，这个脚本碰不到。"
say ""
say "  升级：在这个目录再跑一次同一条命令。"
say "  数据在 Docker 卷 gouhuo-data 里（有 TLS 私钥，丢了旧链接全作废），别 docker compose down -v。"
say ""
