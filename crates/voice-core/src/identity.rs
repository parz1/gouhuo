// SPDX-License-Identifier: MPL-2.0

//! 本地身份：一对 Ed25519 密钥。**痛点 #3 的答案：身份是本地文件，换机就没了。**
//!
//! # 没有账号系统
//!
//! 用户的身份就是一对本地生成的密钥，公钥即身份。没有注册、没有密码、没有邮箱、
//! 没有找回流程，也没有任何中心服务器知道你是谁。这跟「开源、可自部署」是一回事 ——
//! 一旦有账号系统，就必然有一台谁都得依赖的服务器。
//!
//! 代价很直接：**私钥丢了身份就没了，没有任何人能帮你找回。** 所以
//! [`Identity::export`] / [`Identity::import`] 不是附加功能，是这套设计成立的前提。
//!
//! # 磁盘上为什么用 DPAPI 加密
//!
//! 私钥明文躺在用户目录里，会被网盘同步走、被备份软件带走、被信息窃取器一把抓走，
//! 而这些拷贝在**任何机器上都能直接用**。
//!
//! Windows 的 DPAPI（`CryptProtectData`）把密文绑定到当前用户账户：
//! 文件拷到别的机器、别的账户下都解不开。而且全程零交互 —— 不用用户设密码，
//! 符合「装完即用」。
//!
//! 它挡不住什么要说清楚：**同一个账户下运行的程序照样能解密**。DPAPI 防的是
//! 「文件被搬走」，不是「本机被攻陷」。后者在没有用户密码的前提下无解，
//! 而要用户设密码就违背了「装完即用」—— 这是刻意的取舍。
//!
//! 另一个后果：**重装系统或换 Windows 账户之后，旧的密钥文件就解不开了。**
//! 所以首次生成身份时必须提示用户导出备份，见 [`Identity::export`]。

use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use protocol::text::strip_prefix_ignore_ascii_case;
use protocol::{base32, PublicKey};

/// 导出文本的前缀。**故意写得很吓人** —— 用户一眼就该知道这串东西不能随便发。
pub const EXPORT_PREFIX: &str = "gouhuo-secret-v1-";

/// 磁盘文件的格式版本。换存储格式就 +1。
const FILE_VERSION: u8 = 1;
const SECRET_LEN: usize = 32;
const CHECKSUM_LEN: usize = 2;

/// 可以克隆：断线重连要在后台线程上重新签名，得有自己的一份。
/// 克隆出来的副本跟原件一样随作用域结束清零（`SigningKey` 自己负责）。
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
}

/// **故意不 derive Debug。** 这个类型装着私钥，自动派生的 Debug 迟早会把它
/// 打进某条日志、某个 panic 消息、或者某个错误上下文里。
/// 这里只露公钥指纹 —— 排查问题够用，泄漏了也不要紧。
impl core::fmt::Debug for Identity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Identity({})", self.public_key().fingerprint())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// 没有那个前缀。八成是粘错了东西。
    MissingPrefix,
    BadCharacter(char),
    /// 长度不对，多半是复制时截断了。
    WrongLength,
    /// 校验和不对：粘漏了、粘多了，或者中间被改过。
    BadChecksum,
    UnsupportedVersion(u8),
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ImportError::MissingPrefix => {
                write!(f, "这不像是导出的身份，应该以 {EXPORT_PREFIX} 开头")
            }
            ImportError::BadCharacter(c) => write!(f, "里面有不认识的字符：{c:?}"),
            ImportError::WrongLength => f.write_str("长度不对，检查一下是不是复制全了"),
            ImportError::BadChecksum => f.write_str("校验不过，检查一下是不是复制全了"),
            ImportError::UnsupportedVersion(v) => {
                write!(f, "这是版本 {v} 导出的身份，当前客户端认不了，升级一下")
            }
        }
    }
}

impl std::error::Error for ImportError {}

impl Identity {
    /// 新生成一对密钥。
    ///
    /// 直接用操作系统的随机源，不经过任何用户态 RNG —— 这是长期身份，
    /// 不值得为了省事在种子上冒险。
    pub fn generate() -> io::Result<Self> {
        let mut secret = [0u8; SECRET_LEN];
        getrandom::fill(&mut secret)
            .map_err(|e| io::Error::other(format!("拿不到系统随机数: {e}")))?;
        let signing = SigningKey::from_bytes(&secret);
        // 原始种子已经被 SigningKey 拷走了，这份擦掉。
        zeroize(&mut secret);
        Ok(Self { signing })
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.signing.verifying_key().to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }

    /// 用别人的公钥验签。服务端拿它认证客户端，客户端也能拿它验服务端。
    pub fn verify(key: &PublicKey, message: &[u8], signature: &[u8; 64]) -> bool {
        let Ok(verifying) = VerifyingKey::from_bytes(&key.0) else {
            return false;
        };
        verifying
            .verify(message, &Signature::from_bytes(signature))
            .is_ok()
    }

    /// 导出成一串可以复制走的文本。**这串东西等于你的身份，谁拿到谁就是你。**
    ///
    /// 格式：`gouhuo-secret-v1-<base32(版本 + 私钥 + 校验和)>`。
    /// 校验和是为了让「粘贴时漏了一截」当场报错，而不是导入出一个错误的身份。
    pub fn export(&self) -> String {
        let mut payload = Vec::with_capacity(1 + SECRET_LEN + CHECKSUM_LEN);
        payload.push(FILE_VERSION);
        payload.extend_from_slice(&self.signing.to_bytes());
        payload.extend_from_slice(&checksum(&payload));
        let text = format!("{EXPORT_PREFIX}{}", base32::encode(&payload));
        zeroize(&mut payload);
        text
    }

    pub fn import(text: &str) -> Result<Self, ImportError> {
        let trimmed = text.trim();
        // 不能直接按字节切前缀 —— 用户往这个框里粘中文会 panic，
        // 见 protocol::text 的模块文档。
        let Some(body) = strip_prefix_ignore_ascii_case(trimmed, EXPORT_PREFIX) else {
            return Err(ImportError::MissingPrefix);
        };
        let mut payload = base32::decode(body).map_err(|e| ImportError::BadCharacter(e.0))?;

        // base32 解码末尾可能多出一个补位字节，允许多 1 但不能更多。
        let expected = 1 + SECRET_LEN + CHECKSUM_LEN;
        if payload.len() < expected || payload.len() > expected + 1 {
            zeroize(&mut payload);
            return Err(ImportError::WrongLength);
        }
        payload.truncate(expected);

        let (body, tail) = payload.split_at(expected - CHECKSUM_LEN);
        if tail != checksum(body) {
            zeroize(&mut payload);
            return Err(ImportError::BadChecksum);
        }
        if body[0] != FILE_VERSION {
            let version = body[0];
            zeroize(&mut payload);
            return Err(ImportError::UnsupportedVersion(version));
        }

        let mut secret = [0u8; SECRET_LEN];
        secret.copy_from_slice(&body[1..1 + SECRET_LEN]);
        let signing = SigningKey::from_bytes(&secret);
        zeroize(&mut secret);
        zeroize(&mut payload);
        Ok(Self { signing })
    }

    /// 身份文件的默认位置：`%APPDATA%\gouhuo\identity.key`。
    pub fn default_path() -> io::Result<PathBuf> {
        let base =
            std::env::var_os("APPDATA").ok_or_else(|| io::Error::other("找不到 APPDATA 目录"))?;
        Ok(PathBuf::from(base).join("gouhuo").join("identity.key"))
    }

    /// 读身份；没有就新建一个存下去。
    ///
    /// 返回值里的 `bool` 表示**是不是新建的** —— 新建时上层必须提示用户导出备份，
    /// 因为 DPAPI 绑在当前 Windows 账户上，重装系统就解不开了。
    pub fn load_or_create(path: &Path) -> io::Result<(Self, bool)> {
        match Self::load(path) {
            Ok(identity) => Ok((identity, false)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                identity.save(path)?;
                Ok((identity, true))
            }
            Err(e) => Err(e),
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let blob = std::fs::read(path)?;
        let mut plain = unprotect(&blob)?;

        let expected = 1 + SECRET_LEN;
        if plain.len() != expected || plain[0] != FILE_VERSION {
            let version = plain.first().copied().unwrap_or(0);
            zeroize(&mut plain);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("身份文件格式不对（版本 {version}）"),
            ));
        }
        let mut secret = [0u8; SECRET_LEN];
        secret.copy_from_slice(&plain[1..]);
        let signing = SigningKey::from_bytes(&secret);
        zeroize(&mut secret);
        zeroize(&mut plain);
        Ok(Self { signing })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut plain = Vec::with_capacity(1 + SECRET_LEN);
        plain.push(FILE_VERSION);
        plain.extend_from_slice(&self.signing.to_bytes());
        let blob = protect(&plain);
        zeroize(&mut plain);
        let blob = blob?;

        // 先写临时文件再改名：中途断电不会留下一个半截的身份文件，
        // 那会让用户彻底失去身份而不是回到上一个状态。
        let tmp = path.with_extension("key.tmp");
        std::fs::write(&tmp, &blob)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn checksum(body: &[u8]) -> [u8; CHECKSUM_LEN] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body);
    let mut out = [0u8; CHECKSUM_LEN];
    out.copy_from_slice(&digest[..CHECKSUM_LEN]);
    out
}

/// 把缓冲区清零。
///
/// `write_volatile` 是为了不让优化器把这段"没人读的写入"整个删掉 ——
/// 普通的 `fill(0)` 在 release 下是可能被优化没的。
fn zeroize(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[cfg(windows)]
mod dpapi {
    use std::io;

    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    /// 描述串会被 DPAPI 原样存进密文里，纯粹是给人排查用的。
    const DESCRIPTION: &str = "gouhuo identity";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// 把 DPAPI 返回的 blob 拷出来，然后立刻 LocalFree —— 那块内存是它分配的。
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let slice = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        let owned = slice.to_vec();
        unsafe { LocalFree(Some(HLOCAL(out.pbData as *mut _))) };
        owned
    }

    pub fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
        let description: Vec<u16> = DESCRIPTION
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &blob(plain),
                windows::core::PCWSTR(description.as_ptr()),
                None,
                None,
                None,
                0,
                &mut out,
            )
        }
        .map_err(|e| io::Error::other(format!("DPAPI 加密失败: {e}")))?;
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(cipher: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe { CryptUnprotectData(&blob(cipher), None, None, None, None, 0, &mut out) }.map_err(
            |e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "DPAPI 解密失败: {e}。\
                         身份文件是绑定到当前 Windows 账户的 —— \
                         换了账户或重装过系统的话，只能用导出的备份恢复。"
                    ),
                )
            },
        )?;
        Ok(unsafe { take(out) })
    }
}

#[cfg(windows)]
use dpapi::{protect, unprotect};

/// 非 Windows 上不加密。目前只有 Windows 客户端，这条分支纯粹是为了
/// 让测试和 CI 能在别的平台上跑通 —— 客户端上别的平台之前，这里必须换成
/// 对应平台的密钥库（Keychain / Secret Service / Keystore），而不是就这么明文存着。
#[cfg(not(windows))]
fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    Ok(plain.to_vec())
}

#[cfg(not(windows))]
fn unprotect(cipher: &[u8]) -> io::Result<Vec<u8>> {
    Ok(cipher.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("gouhuo-test-{name}-{}", std::process::id()));
        p.push("identity.key");
        p
    }

    #[test]
    fn generated_identities_are_distinct() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.public_key(), b.public_key(), "两次生成不该撞车");
    }

    #[test]
    fn signature_roundtrip() {
        let id = Identity::generate().unwrap();
        let msg = b"login challenge";
        let sig = id.sign(msg);
        assert!(Identity::verify(&id.public_key(), msg, &sig));
        // 换消息、换密钥、改签名，都必须验不过
        assert!(!Identity::verify(&id.public_key(), b"other", &sig));
        let other = Identity::generate().unwrap();
        assert!(!Identity::verify(&other.public_key(), msg, &sig));
        let mut bad = sig;
        bad[0] ^= 1;
        assert!(!Identity::verify(&id.public_key(), msg, &bad));
    }

    #[test]
    fn export_import_preserves_the_same_identity() {
        let id = Identity::generate().unwrap();
        let text = id.export();
        let restored = Identity::import(&text).unwrap();
        assert_eq!(restored.public_key(), id.public_key());
        // 恢复出来的必须能产生同样有效的签名
        let msg = b"still me";
        assert!(Identity::verify(&id.public_key(), msg, &restored.sign(msg)));
    }

    #[test]
    fn export_looks_alarming_and_fits_one_line() {
        let text = Identity::generate().unwrap().export();
        assert!(
            text.starts_with(EXPORT_PREFIX),
            "前缀要让人一眼知道不能乱发"
        );
        assert!(text.contains("secret"));
        assert!(text.len() < 100, "{} 字符，太长了", text.len());
        assert!(
            !text.contains(char::is_whitespace),
            "不能有空白，会被聊天软件折断"
        );
    }

    #[test]
    fn import_survives_copy_paste_damage() {
        let id = Identity::generate().unwrap();
        let text = id.export();
        for mangled in [
            format!("  {text}  "),
            text.to_uppercase(),
            format!("{}-{}", &text[..30], &text[30..]),
        ] {
            assert_eq!(
                Identity::import(&mangled).unwrap().public_key(),
                id.public_key(),
                "没扛住：{mangled:?}"
            );
        }
    }

    #[test]
    fn import_rejects_damaged_input() {
        let text = Identity::generate().unwrap().export();
        // 粘中文/emoji 进来不能崩 —— 这个框最容易被粘进奇怪的东西
        for junk in [
            "随便一串东西",
            "你发我的那个码呢？",
            "🎮",
            "gouhuo-secret-v1-中文",
        ] {
            assert!(Identity::import(junk).is_err(), "{junk:?} 不该被接受");
        }
        assert_eq!(
            Identity::import("随便一串东西").unwrap_err(),
            ImportError::MissingPrefix
        );
        assert_eq!(
            Identity::import(&text[..text.len() - 2]).unwrap_err(),
            ImportError::WrongLength
        );

        // 改中间一个字符：必须是校验和拦下，绝不能导入出另一个身份
        let mut chars: Vec<char> = text.chars().collect();
        let mid = chars.len() / 2;
        chars[mid] = if chars[mid] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(
            matches!(
                Identity::import(&tampered).unwrap_err(),
                ImportError::BadChecksum | ImportError::WrongLength
            ),
            "改了一个字符却没被发现"
        );
    }

    /// Debug 输出里绝不能出现私钥。
    #[test]
    fn debug_does_not_leak_the_secret() {
        let id = Identity::generate().unwrap();
        let rendered = format!("{id:?}");
        let exported = id.export();
        let raw = base32::decode(&exported[EXPORT_PREFIX.len()..]).unwrap();
        let secret_hex: String = raw[1..1 + SECRET_LEN]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            !rendered.contains(&secret_hex),
            "Debug 把私钥打出来了：{rendered}"
        );
        assert!(!rendered.contains(&exported), "Debug 把导出串打出来了");
        // 但要有点有用的信息，否则排查问题时等于没有
        assert!(rendered.contains(&id.public_key().fingerprint().to_grouped_hex()));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_path("save-load");
        let _ = std::fs::remove_file(&path);
        let id = Identity::generate().unwrap();
        id.save(&path).unwrap();
        let loaded = Identity::load(&path).unwrap();
        assert_eq!(loaded.public_key(), id.public_key());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_or_create_reports_whether_it_made_a_new_one() {
        let path = temp_path("load-or-create");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let (first, created) = Identity::load_or_create(&path).unwrap();
        assert!(created, "第一次必须是新建");
        let (second, created) = Identity::load_or_create(&path).unwrap();
        assert!(!created, "第二次必须是读出来的");
        assert_eq!(first.public_key(), second.public_key());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn stored_file_is_not_the_plaintext_key() {
        let path = temp_path("dpapi");
        let _ = std::fs::remove_file(&path);
        let id = Identity::generate().unwrap();
        id.save(&path).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        let secret_text = id.export();
        // 私钥的原始字节不该在文件里出现
        let raw = base32::decode(&secret_text[EXPORT_PREFIX.len()..]).unwrap();
        let secret = &raw[1..1 + SECRET_LEN];
        assert!(
            !on_disk.windows(SECRET_LEN).any(|w| w == secret),
            "私钥明文直接躺在磁盘上了"
        );
        assert!(on_disk.len() > SECRET_LEN, "DPAPI 的密文应该比明文长");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // 字节篡改的完整性保护来自 DPAPI；非 Windows 的测试替身只复制明文，
    // 任意 32 字节种子都是合法密钥，不具备这条保证。
    #[cfg(windows)]
    #[test]
    fn corrupted_file_is_rejected_not_silently_accepted() {
        let path = temp_path("corrupt");
        let _ = std::fs::remove_file(&path);
        Identity::generate().unwrap().save(&path).unwrap();

        let mut blob = std::fs::read(&path).unwrap();
        let mid = blob.len() / 2;
        blob[mid] ^= 0xFF;
        std::fs::write(&path, &blob).unwrap();

        assert!(Identity::load(&path).is_err(), "文件被改过却照样读出来了");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn malformed_file_is_rejected_without_replacing_the_identity() {
        let path = temp_path("malformed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // 格式校验在所有平台都要成立；先经过各平台的 protect，单独验证
        // 明文格式，而不是让 DPAPI 提前拦下损坏的密文。
        for plain in [
            vec![FILE_VERSION; SECRET_LEN],
            vec![FILE_VERSION; SECRET_LEN + 2],
            vec![FILE_VERSION + 1; SECRET_LEN + 1],
        ] {
            let blob = protect(&plain).unwrap();
            std::fs::write(&path, &blob).unwrap();
            assert_eq!(
                Identity::load(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                Identity::load_or_create(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(std::fs::read(&path).unwrap(), blob, "不能覆盖坏的身份文件");
        }

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
