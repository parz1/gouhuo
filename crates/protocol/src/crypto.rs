// SPDX-License-Identifier: MIT OR Apache-2.0

//! 语音包加密。**不自研密码学** —— 这里只是把 RustCrypto 的 ChaCha20-Poly1305
//! 按我们的包格式接上去。
//!
//! - 头部明文，同时作为 AAD：服务端要按 session 查是谁在说话、往哪转。
//! - v2 nonce = `session(4) || seq(4) || domain(4)`，全部由头部推出，不上线传。
//!   domain 为 0（语音，包括结束包）或 1（保活），不能把可变 flags 当成域。
//! - 就地加解密，每帧零分配（语音路径 50 次/秒 × N 人，分配器是能测出来的）。
//!
//! **nonce 唯一性**：同一把 key 下 `(session, domain, seq)` 必须永不重复。
//! 每个域的序号随密钥存活，不能随音频设备重建归零；耗尽后停止发送，必须重连换密钥。
//! 密钥从控制面 TLS 派生，上下行密钥独立 —— 见 transport。

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};

use crate::{ProtocolError, VoiceHeader, MAX_DATAGRAM, VOICE_HEADER_LEN};

/// Poly1305 认证标签长度。每包固定开销，算带宽时别忘了它。
pub const TAG_LEN: usize = 16;

/// 一次 TLS 连接的发送序号。与密钥一起保存，所有 Pipeline 重建共享它。
/// 语音和保活各有一条连续序列，避免抖动缓冲把保活误算为丢帧。
#[derive(Default)]
pub struct VoiceSequences {
    voice: std::sync::atomic::AtomicU64,
    keepalive: std::sync::atomic::AtomicU64,
    samples: std::sync::atomic::AtomicU32,
}

impl VoiceSequences {
    /// 采样时钟也跨设备重建连续；时间戳允许回绕，不参与 nonce。
    pub fn advance_samples(&self, samples: u32) -> u32 {
        self.samples
            .fetch_add(samples, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(samples)
    }

    pub fn exhausted(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.voice.load(Ordering::Relaxed) > u32::MAX as u64
            || self.keepalive.load(Ordering::Relaxed) > u32::MAX as u64
    }

    /// 分配一次加密使用的序号；u32 耗尽后永不回绕。
    // 新 Rust 将 fetch_update 改名为 try_update；保留旧名以兼容 Rust 1.80。
    #[allow(deprecated)]
    pub fn next(&self, keepalive: bool) -> Option<u32> {
        use std::sync::atomic::Ordering;
        let counter = if keepalive {
            &self.keepalive
        } else {
            &self.voice
        };
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n <= u32::MAX as u64).then_some(n + 1)
            })
            .ok()
            .map(|n| n as u32)
    }
}

pub struct VoiceCipher {
    aead: ChaCha20Poly1305,
}

impl VoiceCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    fn nonce(h: &VoiceHeader) -> Nonce {
        let mut n = [0u8; 12];
        n[0..4].copy_from_slice(&h.session.to_le_bytes());
        n[4..8].copy_from_slice(&h.seq.to_le_bytes());
        n[8..12].copy_from_slice(&u32::from(h.is_keepalive()).to_le_bytes());
        Nonce::from(n)
    }

    /// 打包并加密到 `out`（会先 clear）。布局：`头部(明文) || 密文 || tag`。
    pub fn seal(
        &self,
        h: VoiceHeader,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), ProtocolError> {
        if VOICE_HEADER_LEN + payload.len() + TAG_LEN > MAX_DATAGRAM {
            return Err(ProtocolError::TooLarge);
        }
        let hdr = h.to_bytes();
        out.clear();
        out.extend_from_slice(&hdr);
        out.extend_from_slice(payload);
        let tag = self
            .aead
            .encrypt_in_place_detached(&Self::nonce(&h), &hdr, &mut out[VOICE_HEADER_LEN..])
            .map_err(|_| ProtocolError::Crypto)?;
        out.extend_from_slice(&tag);
        Ok(())
    }

    /// 校验并解密到 `out`（会先 clear），返回头部。
    ///
    /// 校验失败一律丢包，不回错误给对端 —— 语音面不给攻击者任何 oracle。
    pub fn open(&self, packet: &[u8], out: &mut Vec<u8>) -> Result<VoiceHeader, ProtocolError> {
        let (h, rest) = VoiceHeader::parse(packet)?;
        if rest.len() < TAG_LEN {
            return Err(ProtocolError::Truncated);
        }
        let (ct, tag) = rest.split_at(rest.len() - TAG_LEN);
        out.clear();
        out.extend_from_slice(ct);
        self.aead
            .decrypt_in_place_detached(&Self::nonce(&h), &h.to_bytes(), out, Tag::from_slice(tag))
            .map_err(|_| ProtocolError::Crypto)?;
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(seq: u32) -> VoiceHeader {
        VoiceHeader {
            session: 42,
            seq,
            timestamp: seq * 960,
            flags: 0,
        }
    }

    #[test]
    fn seal_open_roundtrip() {
        let c = VoiceCipher::new(&[9u8; 32]);
        let payload: Vec<u8> = (0..80u8).collect();
        let mut wire = Vec::new();
        c.seal(hdr(1), &payload, &mut wire).unwrap();
        assert_eq!(wire.len(), VOICE_HEADER_LEN + payload.len() + TAG_LEN);

        let mut back = Vec::new();
        assert_eq!(c.open(&wire, &mut back).unwrap(), hdr(1));
        assert_eq!(back, payload);
    }

    #[test]
    fn tampered_header_is_rejected() {
        let c = VoiceCipher::new(&[9u8; 32]);
        let mut wire = Vec::new();
        c.seal(hdr(1), &[1, 2, 3, 4], &mut wire).unwrap();
        wire[0] ^= 0x01; // 改 session：冒名转发
        let mut back = Vec::new();
        assert_eq!(c.open(&wire, &mut back), Err(ProtocolError::Crypto));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let c = VoiceCipher::new(&[9u8; 32]);
        let mut wire = Vec::new();
        c.seal(hdr(1), &[1, 2, 3, 4], &mut wire).unwrap();
        let n = wire.len();
        wire[n - TAG_LEN - 1] ^= 0x80;
        let mut back = Vec::new();
        assert_eq!(c.open(&wire, &mut back), Err(ProtocolError::Crypto));
    }

    #[test]
    fn wrong_key_is_rejected() {
        let a = VoiceCipher::new(&[1u8; 32]);
        let b = VoiceCipher::new(&[2u8; 32]);
        let mut wire = Vec::new();
        a.seal(hdr(1), &[1, 2, 3, 4], &mut wire).unwrap();
        let mut back = Vec::new();
        assert_eq!(b.open(&wire, &mut back), Err(ProtocolError::Crypto));
    }
}

#[cfg(test)]
mod sequence_regressions {
    use super::*;
    use crate::{FLAG_KEEPALIVE, FLAG_TERMINATOR};
    use std::sync::{atomic::Ordering, Arc};

    fn header(seq: u32, flags: u8) -> VoiceHeader {
        VoiceHeader {
            session: 7,
            seq,
            timestamp: 123,
            flags,
        }
    }

    #[test]
    fn domains_separate_keepalive_but_not_voice_flags() {
        let voice = header(0, 0);
        let probe = header(0, FLAG_KEEPALIVE);
        assert_ne!(VoiceCipher::nonce(&voice), VoiceCipher::nonce(&probe));
        assert_eq!(
            VoiceCipher::nonce(&voice),
            VoiceCipher::nonce(&header(0, FLAG_TERMINATOR))
        );
        let cipher = VoiceCipher::new(&[7; 32]);
        let mut a = Vec::new();
        let mut b = Vec::new();
        cipher.seal(voice, &[1, 2, 3], &mut a).unwrap();
        cipher.seal(probe, &[1, 2, 3], &mut b).unwrap();
        assert_ne!(
            &a[VOICE_HEADER_LEN..VOICE_HEADER_LEN + 3],
            &b[VOICE_HEADER_LEN..VOICE_HEADER_LEN + 3]
        );
    }

    #[test]
    fn concurrent_senders_and_rebuilds_never_reuse_nonces() {
        let seq = Arc::new(VoiceSequences::default());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let seq = Arc::clone(&seq);
                std::thread::spawn(move || {
                    (0..128)
                        .map(|_| {
                            let keepalive = i % 2 == 0;
                            VoiceCipher::nonce(&header(
                                seq.next(keepalive).unwrap(),
                                if keepalive { FLAG_KEEPALIVE } else { 0 },
                            ))
                            .to_vec()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let nonces: Vec<_> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        let unique: std::collections::BTreeSet<_> = nonces.iter().collect();
        assert_eq!(unique.len(), nonces.len());
        let rebuilt = Arc::clone(&seq);
        assert_eq!(rebuilt.next(false), Some(512));
        assert_eq!(rebuilt.next(true), Some(512));
        assert_eq!(seq.advance_samples(480), 480);
        assert_eq!(rebuilt.advance_samples(480), 960);
    }

    #[test]
    fn exhausted_sequences_never_wrap_even_after_rebuild() {
        for keepalive in [false, true] {
            let seq = VoiceSequences::default();
            let counter = if keepalive {
                &seq.keepalive
            } else {
                &seq.voice
            };
            counter.store(u32::MAX as u64, Ordering::Relaxed);
            assert_eq!(seq.next(keepalive), Some(u32::MAX));
            assert!(seq.exhausted());
            for _ in 0..3 {
                assert_eq!(seq.next(keepalive), None);
            }
        }
    }
}
