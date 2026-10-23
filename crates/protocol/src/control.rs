// SPDX-License-Identifier: MIT OR Apache-2.0

//! 控制面：TCP + TLS 上跑的消息。
//!
//! 消息本身定义在 `proto/control.proto`，这里只管**怎么把它们放到 TCP 流上**。
//!
//! # 分帧
//!
//! TCP 是字节流，没有消息边界，所以每条消息前面加 4 字节大端长度：
//!
//! ```text
//! [长度 u32 BE][protobuf 字节]
//! ```
//!
//! **长度必须有上限。** 这是公网上的服务端，收到的是不可信输入 ——
//! 一个声称自己有 4 GB 的长度前缀，如果照着去 `Vec::with_capacity`，
//! 一条消息就能把服务端打死。见 [`MAX_FRAME_BODY`]。
//!
//! # 为什么控制面不自己加密
//!
//! 整条连接跑在 TLS 里，控制面消息不需要再套一层。语音面走 UDP 才需要
//! 自己的 ChaCha20-Poly1305（见 `crate::crypto`），而那把密钥是从这条 TLS
//! 连接里派生出来的 —— 不另起一套握手，也就没有第二套要审的密码学。

use prost::Message;

/// 生成的 protobuf 类型。`proto/control.proto` 里写的注释会一并搬过来。
mod generated {
    // 生成的代码不归我们管格式，也不该被 clippy 挑刺。
    #![allow(clippy::all, clippy::pedantic, missing_docs)]
    include!(concat!(env!("OUT_DIR"), "/gouhuo.control.v1.rs"));
}

pub use generated::*;

/// 连接协议版本。**控制面或语音加密规则不兼容时才动**，跟客户端版本号无关。
///
/// 加字段不算不兼容（protobuf 的旧端会忽略它），所以绝大多数改动都不该碰这个数。
pub const PROTOCOL_VERSION: u32 = 2;

/// 一条控制面消息的字节上限。
///
/// 1 MiB 对控制面是绰绰有余的：最大的那条是 `Welcome`，它带着整棵频道树和
/// 全部在线成员 —— 按 20 人上限、频道几十个算，撑死几十 KB。
///
/// 留这么大的余量只是为了不用为了协议演进反复调它；真正重要的是**有**上限。
pub const MAX_FRAME_BODY: usize = 1 << 20;

/// 长度前缀本身的字节数。
pub const FRAME_HEADER_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// 对端声称的长度超过 [`MAX_FRAME_BODY`]。**直接断开，不要试图读完。**
    TooLarge(usize),
    /// protobuf 解不开：对端实现有问题，或者流已经错位了。
    Malformed,
    /// 要编码的消息自己就超了上限 —— 这是我们这边的 bug，不是对端的。
    OversizeOutgoing(usize),
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrameError::TooLarge(n) => {
                write!(
                    f,
                    "对端发来一条 {n} 字节的控制消息，超过上限 {MAX_FRAME_BODY}"
                )
            }
            FrameError::Malformed => f.write_str("控制消息解不开，连接已经错位了"),
            FrameError::OversizeOutgoing(n) => {
                write!(f, "要发的控制消息有 {n} 字节，超过上限 {MAX_FRAME_BODY}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// 把一条消息编成带长度前缀的帧，追加到 `out`。
pub fn encode_frame<M: Message>(message: &M, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = message.encoded_len();
    if len > MAX_FRAME_BODY {
        return Err(FrameError::OversizeOutgoing(len));
    }
    out.reserve(FRAME_HEADER_LEN + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    // 上面刚 reserve 过，encode 不会失败。
    message.encode(out).expect("buffer was reserved");
    Ok(())
}

/// 从缓冲区头部试着取一条消息。
///
/// 返回 `Ok(None)` 表示**字节还不够，再读一点再来** —— 这是流式读取的正常状态，
/// 不是错误。返回 `Ok(Some((msg, n)))` 时 `n` 是这条帧占掉的字节数，
/// 调用方要把它从缓冲区前面丢掉。
pub fn decode_frame<M: Message + Default>(buf: &[u8]) -> Result<Option<(M, usize)>, FrameError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

    // **先判上限再判够不够**。反过来的话，对端声称 4 GB 时我们会一直
    // "等更多数据"，缓冲区越攒越大，等于让对端决定我们用多少内存。
    if len > MAX_FRAME_BODY {
        return Err(FrameError::TooLarge(len));
    }
    let total = FRAME_HEADER_LEN + len;
    if buf.len() < total {
        return Ok(None);
    }

    let message = M::decode(&buf[FRAME_HEADER_LEN..total]).map_err(|_| FrameError::Malformed)?;
    Ok(Some((message, total)))
}

/// 从对端声称的长度看这一帧一共要多少字节。
///
/// 给读取循环用：知道还差多少就可以一次性读够，不用一个字节一个字节试。
pub fn peek_frame_len(buf: &[u8]) -> Result<Option<usize>, FrameError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_BODY {
        return Err(FrameError::TooLarge(len));
    }
    Ok(Some(FRAME_HEADER_LEN + len))
}

// 让构造信封少写点样板。
//
// 注意变体要传**整条路径**：`$x:path` 捕获之后就是一个不透明的片段，
// 后面再跟 `::Variant` 是编译不过的。
macro_rules! envelope_from {
    ($envelope:ty, $($ty:ty => $variant:path),* $(,)?) => {
        $(
            impl From<$ty> for $envelope {
                fn from(value: $ty) -> Self {
                    Self { payload: Some($variant(value)) }
                }
            }
        )*
    };
}

envelope_from!(
    ClientMessage,
    Hello => client_message::Payload::Hello,
    VoiceDatagram => client_message::Payload::VoiceDatagram,
    VoicePath => client_message::Payload::VoicePath,
    Authenticate => client_message::Payload::Authenticate,
    Ping => client_message::Payload::Ping,
    JoinChannel => client_message::Payload::JoinChannel,
    CreateChannel => client_message::Payload::CreateChannel,
    DeleteChannel => client_message::Payload::DeleteChannel,
    EditChannel => client_message::Payload::EditChannel,
    KickUser => client_message::Payload::KickUser,
    BanUser => client_message::Payload::BanUser,
    Unban => client_message::Payload::Unban,
    SetRole => client_message::Payload::SetRole,
    SelfState => client_message::Payload::SelfState,
);

envelope_from!(
    ServerMessage,
    Challenge => server_message::Payload::Challenge,
    VoiceDatagram => server_message::Payload::VoiceDatagram,
    VoicePath => server_message::Payload::VoicePath,
    Welcome => server_message::Payload::Welcome,
    Rejected => server_message::Payload::Rejected,
    Pong => server_message::Payload::Pong,
    UserState => server_message::Payload::UserState,
    UserLeft => server_message::Payload::UserLeft,
    ChannelState => server_message::Payload::ChannelState,
    Goodbye => server_message::Payload::Goodbye,
    BanList => server_message::Payload::BanList,
);

// TextMessage 两边都有，From 会冲突（一个类型只能有一个 From<TextMessage> 目标），
// 所以这两个显式写，名字上也把方向说清楚。
impl TextMessage {
    /// 包成客户端要发出去的消息。
    pub fn into_client(self) -> ClientMessage {
        ClientMessage {
            payload: Some(client_message::Payload::TextMessage(self)),
        }
    }

    /// 包成服务端要转发出去的消息。
    pub fn into_server(self) -> ServerMessage {
        ServerMessage {
            payload: Some(server_message::Payload::TextMessage(self)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_client() -> ClientMessage {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: "gouhuo 0.0.1".into(),
            public_key: vec![7u8; 32],
        }
        .into()
    }

    #[test]
    fn frame_roundtrips() {
        let msg = sample_client();
        let mut buf = Vec::new();
        encode_frame(&msg, &mut buf).unwrap();

        let (decoded, used) = decode_frame::<ClientMessage>(&buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(used, buf.len());
    }

    #[test]
    fn several_frames_in_one_buffer() {
        let mut buf = Vec::new();
        let a = sample_client();
        let b: ClientMessage = Ping {
            timestamp: 42,
            udp_packets_received: 7,
        }
        .into();
        encode_frame(&a, &mut buf).unwrap();
        encode_frame(&b, &mut buf).unwrap();

        let (first, used) = decode_frame::<ClientMessage>(&buf).unwrap().unwrap();
        assert_eq!(first, a);
        let (second, used2) = decode_frame::<ClientMessage>(&buf[used..])
            .unwrap()
            .unwrap();
        assert_eq!(second, b);
        assert_eq!(used + used2, buf.len());
    }

    /// TCP 会在任意位置切断，所以每一个前缀都必须是「还不够，再等等」而不是报错。
    #[test]
    fn partial_frames_are_not_errors() {
        let mut buf = Vec::new();
        encode_frame(&sample_client(), &mut buf).unwrap();
        for cut in 0..buf.len() {
            assert_eq!(
                decode_frame::<ClientMessage>(&buf[..cut]),
                Ok(None),
                "切在 {cut} 字节时不该报错，应该是等更多数据"
            );
        }
    }

    /// 最关键的一条：对端谎报一个巨大的长度，必须立刻拒绝。
    ///
    /// 如果先判「够不够」再判上限，我们会一直等下去、缓冲区一直涨 ——
    /// 等于让对端决定我们用多少内存。
    #[test]
    fn absurd_length_is_rejected_immediately() {
        let mut buf = u32::MAX.to_be_bytes().to_vec();
        buf.extend_from_slice(b"whatever");
        assert_eq!(
            decode_frame::<ClientMessage>(&buf),
            Err(FrameError::TooLarge(u32::MAX as usize))
        );
        // 只有 4 字节头也要当场拒绝，不能等凑够了再说
        assert_eq!(
            decode_frame::<ClientMessage>(&u32::MAX.to_be_bytes()),
            Err(FrameError::TooLarge(u32::MAX as usize))
        );
        assert_eq!(
            peek_frame_len(&u32::MAX.to_be_bytes()),
            Err(FrameError::TooLarge(u32::MAX as usize))
        );
    }

    #[test]
    fn exactly_at_the_limit_is_allowed() {
        let len = MAX_FRAME_BODY as u32;
        let mut buf = len.to_be_bytes().to_vec();
        buf.resize(FRAME_HEADER_LEN + MAX_FRAME_BODY, 0);
        // 全零不是合法 protobuf 消息体？空消息其实是合法的，这里只验长度检查放行。
        assert!(!matches!(
            decode_frame::<ClientMessage>(&buf),
            Err(FrameError::TooLarge(_))
        ));
        assert_eq!(
            peek_frame_len(&buf).unwrap(),
            Some(FRAME_HEADER_LEN + MAX_FRAME_BODY)
        );
    }

    #[test]
    fn garbage_body_is_malformed_not_panic() {
        // 长度说有 8 字节，内容是乱码
        let mut buf = 8u32.to_be_bytes().to_vec();
        buf.extend_from_slice(&[0xFF; 8]);
        assert_eq!(
            decode_frame::<ClientMessage>(&buf),
            Err(FrameError::Malformed)
        );
    }

    /// 按 protobuf 的规矩拼一个字段标签。
    ///
    /// 手算不靠谱：字段号 16 以上的 tag 就不止一个字节了，
    /// `(99 << 3) as u8` 会静默溢出成一个完全不相干的字节。
    fn tag(field: u32, wire_type: u32) -> Vec<u8> {
        let mut v = (field << 3) | wire_type;
        let mut out = Vec::new();
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
        out
    }

    /// 演进：旧客户端收到带未知字段的消息，必须当没看见，而不是报错。
    /// 这是 protobuf 的核心承诺，这里钉一条测试防止将来换掉序列化时悄悄丢掉它。
    #[test]
    fn unknown_fields_are_ignored_not_rejected() {
        let mut body = Vec::new();
        Hello {
            protocol_version: 1,
            client_version: "x".into(),
            public_key: vec![1, 2, 3],
        }
        .encode(&mut body)
        .unwrap();
        // 追加一个字段号 99 的 varint 字段 —— 模拟未来版本加的东西
        body.extend_from_slice(&tag(99, 0));
        body.push(42);

        let decoded = Hello::decode(&body[..]).expect("未知字段不该让解码失败");
        assert_eq!(decoded.client_version, "x");
        assert_eq!(decoded.public_key, vec![1, 2, 3]);
    }

    /// 未知的 oneof 分支同理：新服务端发来一条旧客户端不认识的消息，
    /// payload 会是 None，客户端应该忽略这条而不是断开。
    #[test]
    fn unknown_oneof_variant_decodes_as_none() {
        // 字段号 200 在 ServerMessage 的 oneof 里没定义
        let mut body = tag(200, 2);
        body.push(0); // 长度 0 的消息体
        let decoded = ServerMessage::decode(&body[..]).expect("未知分支不该让解码失败");
        assert_eq!(
            decoded.payload, None,
            "认不出来的分支应该是 None，让上层忽略"
        );
    }

    #[test]
    fn oversize_outgoing_is_our_bug_not_theirs() {
        let msg = TextMessage {
            channel_id: 1,
            sender_session_id: 0,
            body: "x".repeat(MAX_FRAME_BODY + 1),
            timestamp_ms: 0,
        }
        .into_client();
        let mut buf = Vec::new();
        assert!(matches!(
            encode_frame(&msg, &mut buf),
            Err(FrameError::OversizeOutgoing(_))
        ));
        assert!(buf.is_empty(), "失败时不该往缓冲区里写半条");
    }

    #[test]
    fn roles_are_ordered_by_power() {
        // 界面上「我能不能做这件事」经常写成比大小，所以数值顺序必须有意义
        assert!(Role::Guest < Role::Member);
        assert!(Role::Member < Role::ChannelAdmin);
        assert!(Role::ChannelAdmin < Role::Admin);
        assert_eq!(Role::Unspecified as i32, 0);
    }

    /// 任意字节都不该让解析器 panic。
    #[test]
    fn never_panics_on_arbitrary_bytes() {
        let mut state = 0x9e37_79b9u32;
        for _ in 0..4000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = (state >> 26) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|i| (state.wrapping_add(i as u32).wrapping_mul(2_654_435_761) >> 13) as u8)
                .collect();
            let _ = decode_frame::<ClientMessage>(&bytes);
            let _ = decode_frame::<ServerMessage>(&bytes);
            let _ = peek_frame_len(&bytes);
        }
    }
}

#[cfg(test)]
mod fallback_compatibility {
    use super::*;
    // Deliberately frozen pre-extension shape, not the generated current schema.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyWelcome {
        #[prost(uint32, tag = "1")]
        session_id: u32,
        #[prost(uint32, tag = "4")]
        udp_port: u32,
    }
    #[test]
    fn old_welcome_disables_fallback_and_old_reader_ignores_capability() {
        let old = LegacyWelcome {
            session_id: 7,
            udp_port: 20800,
        };
        let current = Welcome::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(!current.tcp_voice);
        let current = Welcome {
            tcp_voice: true,
            ..current
        };
        let decoded = LegacyWelcome::decode(current.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, old);
        assert_eq!(PROTOCOL_VERSION, 2);
    }
}
