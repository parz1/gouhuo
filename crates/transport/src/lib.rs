// SPDX-License-Identifier: MPL-2.0

//! 传输层：TLS 连接怎么建、UDP 密钥从哪来。
//!
//! 客户端和服务端都用这个 crate，所以它不在 `voice-core` 里 ——
//! `voice-core` 是客户端的语音内核，不该被服务端依赖。
//!
//! # 信任是怎么建立的
//!
//! 整条链只有一跳：
//!
//! ```text
//! 发邀请链接的人  ->  邀请链接里的证书指纹  ->  TLS 连接  ->  UDP 密钥
//! ```
//!
//! 1. 服务端自签一张证书，**持久化**（`cert`）。指纹进邀请链接
//! 2. 客户端连上去，比对指纹 + 验签名（`pinning`）。两件事缺一不可
//! 3. 语音用的对称密钥从这条 TLS 连接里派生（`keys`），不另起握手
//!
//! 信任不是从 CA 来的，是从「拉你进来的那个人」来的。这跟现实一致。
//!
//! # 不自研密码学
//!
//! 这里没有一行自己写的密码学。TLS 用 rustls + ring，证书用 rcgen，
//! UDP 的密钥派生用 TLS 自己的 exporter（RFC 5705）。
//! 我们写的全部是「怎么把它们接起来」。

pub mod cert;
pub mod keys;
pub mod pinning;

pub use cert::ServerCert;
pub use keys::{derive_voice_key, VoiceKey, DOWNSTREAM, UPSTREAM, VOICE_KEY_LABEL};
pub use pinning::PinnedServerCert;

use std::sync::Arc;

use protocol::Fingerprint;
use rustls::crypto::CryptoProvider;

/// 全项目统一用 ring。
///
/// rustls 0.23 要求显式选一个 crypto provider。选 ring 而不是 aws-lc-rs 的理由
/// 很实际：**ring 在 Windows/MSVC 上纯 `cargo build` 就能编**，
/// 而 aws-lc-rs 要 cmake 和 nasm。M2 在外部构建工具上已经吃够苦头了。
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 客户端用的 TLS 配置：只认指纹是 `expected` 的那张证书。
pub fn client_config(expected: Fingerprint) -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = crypto_provider();
    let verifier = Arc::new(PinnedServerCert::new(expected, provider.clone()));
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        // 客户端不用 TLS 证书认证：身份是 Ed25519 密钥，在控制面上用
        // 挑战-应答证明（见 protocol 的 Challenge / Authenticate）。
        // 把身份放在控制面而不是 TLS 层，是为了让身份能导出、能换机器。
        .with_no_client_auth())
}

/// 只为取回对面证书指纹用的 TLS 配置：什么证书都收。
///
/// **握手完读出指纹就该把连接关掉。** 为什么、以及之后怎么办，见
/// [`pinning::UnpinnedServerCert`]。
pub fn probe_config() -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = crypto_provider();
    let verifier = Arc::new(pinning::UnpinnedServerCert::new(provider.clone()));
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

/// 服务端用的 TLS 配置。
pub fn server_config(cert: &ServerCert) -> Result<rustls::ServerConfig, rustls::Error> {
    rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.certificate_der()], cert.private_key_der())
}
