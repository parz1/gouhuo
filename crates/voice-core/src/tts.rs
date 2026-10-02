// SPDX-License-Identifier: MPL-2.0

//! 把「某某进来了」念出来。用 Windows 自带的语音合成（WinRT
//! `SpeechSynthesizer`），不带任何模型：中文 Windows 自带中文语音，
//! 安装包一个字节都不用多。
//!
//! 合成出来的不是直接播，而是解成 48 kHz 采样塞进 [`CueQueue`]，
//! 跟提示音走同一条路 —— 理由见 `cue` 模块的文档：只有混进语音链路的播放
//! 那一路，回声消除才认得它，外放的人才不会把它又发给全频道。
//!
//! # 线程
//!
//! 合成一句话要几十到几百毫秒，而且 WinRT 的异步接口要一个初始化过的线程。
//! 所以开一个专门的线程，界面那边 [`Announcer::say`] 只是往它的队列里丢一句话，
//! 立刻返回。
//!
//! 队列很短（[`PENDING_LIMIT`]）：满了新的就丢。一大群人同时进来时，
//! 念到第五个人已经晚了好几秒，后面的不如不念。

use std::io;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;

use crate::cue::CueQueue;
#[cfg(windows)]
use crate::cue::{decode_wav, resample_to_48k, trim_silence};

/// 最多排几句还没合成的话。
pub const PENDING_LIMIT: usize = 3;

/// 念出来的名字最多几个字。
///
/// 昵称是别人起的：一个五十字的昵称念完要十秒，而且什么都可能有。
/// 截短了照样认得出是谁。
pub const MAX_NAME_CHARS: usize = 12;

struct Request {
    text: String,
    sink: Arc<CueQueue>,
    gain: f32,
}

/// 念名字的那个后台线程的把手。丢掉它，线程在处理完手头那句之后退出。
pub struct Announcer {
    tx: SyncSender<Request>,
}

impl Announcer {
    /// 开后台线程。合成引擎在线程里才初始化，所以这里不会失败；
    /// 引擎用不了的话，[`Announcer::say`] 只是什么都不念。
    pub fn start() -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Request>(PENDING_LIMIT);
        std::thread::Builder::new()
            .name("gouhuo-tts".into())
            .spawn(move || {
                let engine = match Engine::new() {
                    Ok(engine) => engine,
                    Err(e) => {
                        eprintln!("语音合成用不了，不念名字了：{e}");
                        // 把队列排空到发送端都没了为止，免得 say 那边一直被挡
                        for _ in rx {}
                        return;
                    }
                };
                for request in rx {
                    match engine.speak(&request.text) {
                        Ok(samples) => {
                            request.sink.push(&samples, request.gain);
                        }
                        Err(e) => eprintln!("念「{}」失败：{e}", request.text),
                    }
                }
            })?;
        Ok(Self { tx })
    }

    /// 念一句话，念好了排进 `sink`。**不阻塞**：排不上（前面还压着好几句）
    /// 就直接丢掉，返回 `false`。
    pub fn say(&self, text: &str, sink: Arc<CueQueue>, gain: f32) -> bool {
        let request = Request {
            text: text.to_string(),
            sink,
            gain,
        };
        match self.tx.try_send(request) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// 把昵称截到能念的长度。
pub fn speakable_name(name: &str) -> String {
    let trimmed = name.trim();
    let mut out: String = trimmed.chars().take(MAX_NAME_CHARS).collect();
    if out.is_empty() {
        out = "有人".into();
    }
    out
}

#[cfg(windows)]
struct Engine {
    synth: windows::Media::SpeechSynthesis::SpeechSynthesizer,
}

#[cfg(windows)]
impl Engine {
    fn new() -> windows::core::Result<Self> {
        use windows::Media::SpeechSynthesis::SpeechSynthesizer;
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

        // 这个线程专门做合成，MTA 就行，也允许在上面阻塞等异步结果。
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        let synth = SpeechSynthesizer::new()?;

        // 默认语音跟着系统显示语言走。英文系统上默认是英文语音，
        // 念中文名字会一个字一个字地拼 —— 有中文语音就优先用它。
        if let Ok(voices) = SpeechSynthesizer::AllVoices() {
            for voice in voices {
                let is_chinese = voice
                    .Language()
                    .map(|lang| lang.to_string().to_ascii_lowercase().starts_with("zh"))
                    .unwrap_or(false);
                if is_chinese {
                    let _ = synth.SetVoice(&voice);
                    break;
                }
            }
        }
        // 稍微念快一点。默认语速是给朗读准备的；「某某进来了」是个通知，
        // 拖长了反而盖住频道里正在说话的人。老系统没有这个选项，失败就算了。
        if let Ok(options) = synth.Options() {
            let _ = options.SetSpeakingRate(1.25);
        }
        Ok(Self { synth })
    }

    fn speak(&self, text: &str) -> io::Result<Vec<f32>> {
        let wav = self.synthesize(text).map_err(io::Error::other)?;
        let (samples, rate) =
            decode_wav(&wav).ok_or_else(|| io::Error::other("合成结果不是认得的 WAV"))?;
        Ok(resample_to_48k(trim_silence(&samples, 0.03), rate))
    }

    fn synthesize(&self, text: &str) -> windows::core::Result<Vec<u8>> {
        use windows::core::HSTRING;
        use windows::Storage::Streams::DataReader;

        let stream = self
            .synth
            .SynthesizeTextToStreamAsync(&HSTRING::from(text))?
            .join()?;
        let size = stream.Size()? as u32;
        let input = stream.GetInputStreamAt(0)?;
        let reader = DataReader::CreateDataReader(&input)?;
        reader.LoadAsync(size)?.join()?;
        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes)?;
        Ok(bytes)
    }
}

/// 别的平台还没有合成引擎：`start` 照样成功，只是什么都不念。
///
/// 做成空枚举：根本造不出来，`speak` 也就不可能被调到，编译器能证明这一点。
#[cfg(not(windows))]
enum Engine {}

#[cfg(not(windows))]
impl Engine {
    fn new() -> io::Result<Self> {
        Err(io::Error::other("这个平台还没接语音合成"))
    }

    fn speak(&self, _text: &str) -> io::Result<Vec<f32>> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_cut_short() {
        let long = "一".repeat(50);
        assert_eq!(speakable_name(&long).chars().count(), MAX_NAME_CHARS);
        assert_eq!(speakable_name("  阿狸 "), "阿狸");
        assert_eq!(speakable_name("   "), "有人");
    }

    /// 真的调一次系统的语音合成。要装了语音包，所以默认不跑：
    ///
    ///   cargo test -p voice-core tts -- --ignored --nocapture
    #[test]
    #[ignore = "要系统语音合成"]
    fn the_system_voice_actually_speaks() {
        let queue = Arc::new(CueQueue::new());
        let announcer = Announcer::start().unwrap();
        assert!(announcer.say("阿狸进来了", Arc::clone(&queue), 1.0));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while queue.pending() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let seconds = queue.pending() as f32 / crate::audio::SAMPLE_RATE as f32;
        println!("「阿狸进来了」合成出来 {seconds:.2} 秒");
        assert!(
            (0.4..5.0).contains(&seconds),
            "合成出来的长度不对劲：{seconds} 秒"
        );
    }
}
