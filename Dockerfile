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
# 浏览器加入页（crates/server 的 web feature）默认编进去，但**默认不启用**：
# 不设 GOUHUO_WEB_LISTEN 就不多一个线程、不多一个端口，镜像只大 0.2 MB 左右。
# 想要一个彻底不含 HTTP 服务的镜像：
#
#   docker build --build-arg GOUHUO_FEATURES=--no-default-features .
#
# 用 compose 从源码编的话，在 .env 里写 GOUHUO_FEATURES=--no-default-features。
ARG GOUHUO_FEATURES=""
# 缓存挂载：改一行代码不用重新下载、重新编译全部依赖。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --profile dist --locked -p server --bin gouhuo-server ${GOUHUO_FEATURES} \
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
