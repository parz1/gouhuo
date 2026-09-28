# 篝火服务端镜像。只编 gouhuo-server，不碰音频链路。
#
# 平时不用自己编：发版时 CI 会推到 ghcr.io/parz1/gouhuo-server，compose.yaml
# 默认拉那个。这个文件是给 CI 用的，也给想从源码编的人用（docker compose up -d --build）。
#
# 数据目录 /data 里有 TLS 私钥 —— 证书指纹写在邀请链接里，丢了它所有旧链接
# 都作废，所以务必挂卷。

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
# 缓存挂载：改一行代码不用重新下载、重新编译全部依赖。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --profile dist --locked -p server --bin gouhuo-server \
    && cp target/dist/gouhuo-server /gouhuo-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 gouhuo \
    && mkdir /data && chown gouhuo /data
COPY --from=build /gouhuo-server /usr/local/bin/gouhuo-server
USER gouhuo
ENV GOUHUO_DATA=/data
VOLUME /data
EXPOSE 20800/tcp 20800/udp
ENTRYPOINT ["gouhuo-server"]
