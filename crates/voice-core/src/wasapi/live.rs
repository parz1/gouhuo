// SPDX-License-Identifier: MPL-2.0

//! 真声卡的采集和播放，实现 [`crate::audio`] 的两个 trait。
//!
//! 跟同目录下 `stream.rs` 的分工：那边是**探针**，开一路流量一下延迟就关掉；
//! 这边是**长跑**，开着几小时不停。
//!
//! # 设备在用它的那个线程上才打开
//!
//! WASAPI 全套是 COM 对象，COM 有线程亲和性，而且每个线程都要自己
//! `CoInitializeEx`。所以 [`WasapiCapture::new`] 只记下配置，真正打开设备
//! 是在第一次 [`read`](crate::audio::Capture::read) 的时候 —— 那时候已经在
//! 音频线程上了。
//!
//! 这样还有个好处：构造不会失败，界面那边不用为「设备打不开」单独写一条
//! 错误路径；真出问题时是音频线程报出来，而那里本来就要处理设备中途被拔掉。
//!
//! # 只走共享模式
//!
//! 独占模式延迟更低，但它会让**别的程序发不出声** —— 包括游戏本身。
//! 对一个开黑软件这基本不可接受，所以共享模式是唯一路径。M2 实测共享模式
//! 设备延迟 30.4 ms，在预算内。
//!
//! # 采样率交给 Windows 转
//!
//! 共享模式默认必须用引擎的混音格式，而那个格式**由用户在 Windows 声音设置里
//! 决定**。第一版直接拒绝非 48 kHz 的设备，结果在一台真实的玩家机器上撞墙了：
//! 默认扬声器是 SteelSeries Sonar 的虚拟设备、96000 Hz，默认麦克风里还有
//! 44100 Hz 的 —— 那台机器**开箱就用不了**。
//!
//! 现在用 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`，让音频引擎自己插重采样器。
//! 代价是它那一段的延迟，但比「不支持」好太多。
//!
//! 采样率检查留着当保险丝：正常情况下永远不该触发，触发了说明转换没生效。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, AUDCLNT_BUFFERFLAGS_SILENT,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::audio::{Capture, Render, FRAME_SAMPLES, SAMPLE_RATE};
use crate::wasapi::stream::{describe_error, initialize_with, Event, Opened, ShareMode};
use crate::wasapi::{Direction, Format};

/// 等设备事件最多等多久。
///
/// 正常情况下每个设备周期（10 ms 上下）就会醒一次。等到 200 ms 还没动静，
/// 说明设备出事了（拔了、驱动崩了、被独占了），该报错让上层去重开。
const WAIT_TIMEOUT_MS: u32 = 200;

/// 记下设备是在哪个线程上打开的。
///
/// # 为什么要记
///
/// COM 对象和事件句柄在 `windows` crate 里都不是 `Send`，而 [`Capture`] 和
/// [`Render`] 要求 `Send`。这里的 `unsafe impl Send` 成立，靠的是两条约定：
///
/// 1. **构造时不碰 COM。** 所以从造出来到搬去音频线程这一段，里面什么都没有。
/// 2. **打开、使用、释放全在同一个线程上。** COM 的 MTA 其实允许跨线程用
///    接口，但 `CoUninitialize` 必须在调用过 `CoInitializeEx` 的那个线程上，
///    而 [`crate::wasapi::ComGuard`] 的 Drop 就是它。
///
/// 第二条是个**不变量而不是类型保证**，所以这里花几纳秒在每帧上验一次。
/// 违反了就报错 —— 总比让 COM 在某个线程上悄悄地不干净要好。
struct OpenedOn(std::thread::ThreadId);

impl OpenedOn {
    fn here() -> Self {
        Self(std::thread::current().id())
    }

    fn check(&self, what: &str) -> io::Result<()> {
        if self.0 == std::thread::current().id() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{what}流被搬到别的线程上用了 —— WASAPI 不允许这样"
            )))
        }
    }
}

/// Fixed microphones must never silently fall back to another input.
/// Output retains fallback so a missing speaker does not prevent listening.
fn open_or_default(
    id: Option<&str>,
    direction: Direction,
) -> windows::core::Result<windows::Win32::Media::Audio::IMMDevice> {
    resolve_device(id, direction, |id| {
        crate::wasapi::open_device(id, direction)
    })
}

fn resolve_device<T, E>(
    id: Option<&str>,
    direction: Direction,
    mut open: impl FnMut(Option<&str>) -> Result<T, E>,
) -> Result<T, E> {
    match open(id) {
        Err(_) if id.is_some() && direction == Direction::Render => open(None),
        result => result,
    }
}

/// Checked on the owning audio thread, at most once a second. Returning an
/// error lets the lifecycle rebuild with the latest mute/PTT intentions.
struct DefaultWatch {
    id: Option<String>,
    checked_at: Instant,
    direction: Direction,
}

impl DefaultWatch {
    fn new(
        device: &windows::Win32::Media::Audio::IMMDevice,
        follows_default: bool,
        direction: Direction,
    ) -> io::Result<Self> {
        Ok(Self {
            id: if follows_default {
                Some(crate::wasapi::device::device_id(device).map_err(|e| err("读取设备标识", e))?)
            } else {
                None
            },
            checked_at: Instant::now(),
            direction,
        })
    }

    fn check(&mut self) -> io::Result<()> {
        let direction = self.direction;
        self.check_with(Instant::now(), || {
            let device = crate::wasapi::open_device(None, direction)
                .map_err(|e| err("系统默认音频设备不可用", e))?;
            crate::wasapi::device::device_id(&device).map_err(|e| err("读取默认设备标识", e))
        })
    }

    fn check_with(
        &mut self,
        now: Instant,
        current_default: impl FnOnce() -> io::Result<String>,
    ) -> io::Result<()> {
        let Some(id) = &self.id else {
            return Ok(());
        };
        if now.saturating_duration_since(self.checked_at) < Duration::from_secs(1) {
            return Ok(());
        }
        self.checked_at = now;
        let current = current_default()?;
        if current != *id {
            return Err(io::Error::other(
                "系统默认音频设备已切换，正在恢复语音；闭麦状态会保留。",
            ));
        }
        Ok(())
    }
}

fn err(context: &str, e: windows::core::Error) -> io::Error {
    io::Error::other(format!("{context}：{}", describe_error(&e)))
}

/// 保险丝：开了自动转换之后还拿到非 48 kHz，说明转换没生效。
///
/// 正常情况下永远不该走到这里。真走到了，给用户一条能照着做的指路，
/// 别让他对着一个「初始化失败」发呆。
fn wrong_rate(format: &Format, which: &str) -> io::Error {
    io::Error::other(format!(
        "{which}设备现在是 {} Hz，而且自动转换没生效。\n\
         去 Windows 设置 → 系统 → 声音 → 设备属性 → 高级，把格式改成 48000 Hz，\n\
         然后重开篝火。",
        format.sample_rate
    ))
}

// ===========================================================================
// 采集
// ===========================================================================

pub struct WasapiCapture {
    device_id: Option<String>,
    stream: Option<CaptureStream>,
    /// 实际打开的那个设备叫什么。**要显示给用户** —— 「为什么没声音」
    /// 十有八九是选错了设备，而用户根本不知道我们在用哪一个。
    name: Arc<Mutex<String>>,
    /// 实际打开的那个是不是真硬件。虚拟声卡要在界面上提醒。
    is_hardware: Arc<AtomicBool>,
    /// 设备明确标成「这一段是静音」的帧数。
    ///
    /// 全是它的话，说明设备那边真的什么都没收到（麦克风没插、被静音了、
    /// 或者是一个没在路由的虚拟设备），不是我们解错了数据。
    silent_frames: Arc<AtomicU64>,
    total_frames: Arc<AtomicU64>,
    /// 已经转成 48 kHz 单声道、还没被取走的采样点。
    ///
    /// 设备一次给多少帧由它的周期决定，跟我们要的 10 ms 不一定对得上，
    /// 所以中间必须有个余料池。
    pending: Vec<f32>,
}

struct CaptureStream {
    default_watch: DefaultWatch,
    _com: crate::wasapi::ComGuard,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    format: Format,
    opened_on: OpenedOn,
}

// SAFETY: 见 OpenedOn 的文档。构造时不碰 COM，打开之后只在那一个线程上用，
// 而且每帧都验一次。
unsafe impl Send for WasapiCapture {}

impl WasapiCapture {
    /// 只记配置，不碰设备。`None` 表示用系统默认的录音设备。
    pub fn new(device_id: Option<String>) -> Self {
        Self {
            device_id,
            stream: None,
            name: Arc::new(Mutex::new(String::new())),
            is_hardware: Arc::new(AtomicBool::new(true)),
            silent_frames: Arc::new(AtomicU64::new(0)),
            total_frames: Arc::new(AtomicU64::new(0)),
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
        }
    }

    /// 一个能从别的线程读的把手：设备名字和静音统计。
    ///
    /// 采集流本身跑在音频线程上，界面碰不得；这个只是几个共享的格子。
    pub fn diagnostics(&self) -> CaptureDiagnostics {
        CaptureDiagnostics {
            name: Arc::clone(&self.name),
            is_hardware: Arc::clone(&self.is_hardware),
            silent_frames: Arc::clone(&self.silent_frames),
            total_frames: Arc::clone(&self.total_frames),
        }
    }

    fn open(&mut self) -> io::Result<&mut CaptureStream> {
        if self.stream.is_none() {
            let com = crate::wasapi::ComGuard::new();
            let device =
                open_or_default(self.device_id.as_deref(), Direction::Capture).map_err(|e| {
                    err(
                        "指定或默认麦克风不可用；请检查连接或在设置里重新选择麦克风，不会自动换麦",
                        e,
                    )
                })?;
            let default_watch =
                DefaultWatch::new(&device, self.device_id.is_none(), Direction::Capture)?;
            *self.name.lock().expect("name poisoned") = crate::wasapi::device_name(&device);
            self.is_hardware.store(
                crate::wasapi::device_is_hardware(&device),
                Ordering::Relaxed,
            );
            let Opened {
                client,
                event,
                format,
                ..
            } = initialize_with(&device, Direction::Capture, ShareMode::Shared, 1, true)
                .map_err(|e| err("初始化录音流", e))?;
            if format.sample_rate != SAMPLE_RATE {
                return Err(wrong_rate(&format, "录音"));
            }
            let capture: IAudioCaptureClient =
                unsafe { client.GetService() }.map_err(|e| err("取采集接口", e))?;
            unsafe { client.Start() }.map_err(|e| err("启动录音流", e))?;
            self.stream = Some(CaptureStream {
                default_watch,
                _com: com,
                client,
                capture,
                event,
                format,
                opened_on: OpenedOn::here(),
            });
        }
        Ok(self.stream.as_mut().expect("刚设过"))
    }
}

impl Capture for WasapiCapture {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        self.open()?;
        // 分开借：这几个字段互不相干，整体借 self 会被借用检查挡住。
        let Self {
            stream,
            pending,
            silent_frames,
            total_frames,
            ..
        } = self;
        let stream = stream.as_mut().expect("刚打开过");
        stream.opened_on.check("录音")?;
        stream.default_watch.check()?;
        let channels = stream.format.channels as usize;

        while pending.len() < out.len() {
            // SAFETY: 事件句柄是我们自己建的，超时之后不会有人再碰它。
            let wait = unsafe { WaitForSingleObject(stream.event.0, WAIT_TIMEOUT_MS) };
            if wait != WAIT_OBJECT_0 {
                return Err(io::Error::other(
                    "录音设备没反应了 —— 可能是拔掉了，或者被别的程序独占了",
                ));
            }

            loop {
                let available = unsafe { stream.capture.GetNextPacketSize() }
                    .map_err(|e| err("问设备有多少数据", e))?;
                if available == 0 {
                    break;
                }

                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                // SAFETY: 紧跟着的 ReleaseBuffer 保证这块内存不会被我们留着用。
                unsafe {
                    stream
                        .capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .map_err(|e| err("取录音数据", e))?;
                }

                total_frames.fetch_add(frames as u64, Ordering::Relaxed);
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    // 设备明确说「这一段是静音」，里面的字节没有意义，不能当数据用。
                    silent_frames.fetch_add(frames as u64, Ordering::Relaxed);
                    pending.extend(std::iter::repeat_n(0.0, frames as usize));
                } else {
                    // SAFETY: data 指向 frames * channels 个样点，格式由 format 描述。
                    unsafe {
                        downmix_into(data, frames as usize, channels, &stream.format, pending);
                    }
                }

                unsafe { stream.capture.ReleaseBuffer(frames) }
                    .map_err(|e| err("还录音缓冲", e))?;
            }
        }

        out.copy_from_slice(&pending[..out.len()]);
        pending.drain(..out.len());
        Ok(true)
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// 把设备给的多声道数据平均成单声道，追加到 `out`。
///
/// 取平均而不是只拿左声道：很多耳麦把人声放在其中一个声道上，
/// 只取左声道会在一部分设备上采到纯静音，而且极难查。
unsafe fn downmix_into(
    data: *const u8,
    frames: usize,
    channels: usize,
    format: &Format,
    out: &mut Vec<f32>,
) {
    if channels == 0 {
        return;
    }
    let scale = 1.0 / channels as f32;
    if format.float {
        let samples = unsafe { std::slice::from_raw_parts(data as *const f32, frames * channels) };
        for frame in samples.chunks_exact(channels) {
            out.push(frame.iter().sum::<f32>() * scale);
        }
    } else {
        let samples = unsafe { std::slice::from_raw_parts(data as *const i16, frames * channels) };
        for frame in samples.chunks_exact(channels) {
            let sum: f32 = frame.iter().map(|&s| s as f32 / i16::MAX as f32).sum();
            out.push(sum * scale);
        }
    }
}

/// 从别的线程看采集流的状态。
#[derive(Clone)]
pub struct CaptureDiagnostics {
    name: Arc<Mutex<String>>,
    is_hardware: Arc<AtomicBool>,
    silent_frames: Arc<AtomicU64>,
    total_frames: Arc<AtomicU64>,
}

impl CaptureDiagnostics {
    /// 实际打开的设备叫什么。设备还没打开时是空的。
    pub fn device_name(&self) -> String {
        self.name.lock().expect("name poisoned").clone()
    }

    /// 设备标成静音的采样点占比。
    ///
    /// 接近 1 就是**设备那边真的什么都没收到** —— 麦克风没插、被静音了、
    /// 或者选中的是一个没在路由的虚拟设备。这跟「我们解错了数据」是两回事，
    /// 而排查「为什么没声音」时分清这两者能省掉一半的来回。
    pub fn silent_ratio(&self) -> f32 {
        let total = self.total_frames.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.silent_frames.load(Ordering::Relaxed) as f32 / total as f32
    }

    /// 实际打开的是不是虚拟声卡。
    pub fn is_virtual(&self) -> bool {
        !self.is_hardware.load(Ordering::Relaxed)
    }

    pub fn has_opened(&self) -> bool {
        self.total_frames.load(Ordering::Relaxed) > 0
    }
}

// ===========================================================================
// 播放
// ===========================================================================

pub struct WasapiRender {
    device_id: Option<String>,
    stream: Option<RenderStream>,
    /// 转成设备格式、还没塞进去的字节。
    staging: Vec<u8>,
}

struct RenderStream {
    default_watch: DefaultWatch,
    _com: crate::wasapi::ComGuard,
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    format: Format,
    buffer_frames: u32,
    opened_on: OpenedOn,
}

// SAFETY: 同 WasapiCapture。
unsafe impl Send for WasapiRender {}

impl WasapiRender {
    pub fn new(device_id: Option<String>) -> Self {
        Self {
            device_id,
            stream: None,
            staging: Vec::with_capacity(FRAME_SAMPLES * 8),
        }
    }

    fn open(&mut self) -> io::Result<&mut RenderStream> {
        if self.stream.is_none() {
            let com = crate::wasapi::ComGuard::new();
            let device = open_or_default(self.device_id.as_deref(), Direction::Render)
                .map_err(|e| err("打不开播放设备", e))?;
            let default_watch =
                DefaultWatch::new(&device, self.device_id.is_none(), Direction::Render)?;
            let Opened {
                client,
                event,
                format,
                buffer_frames,
                ..
            } = initialize_with(&device, Direction::Render, ShareMode::Shared, 2, true)
                .map_err(|e| err("初始化播放流", e))?;
            if format.sample_rate != SAMPLE_RATE {
                return Err(wrong_rate(&format, "播放"));
            }
            let render: IAudioRenderClient =
                unsafe { client.GetService() }.map_err(|e| err("取渲染接口", e))?;

            // 先灌一整个缓冲的静音再启动。
            //
            // 不这么做的话，从 Start 到我们第一帧数据到位之间，设备会播到
            // 未初始化的缓冲 —— 听感上是开头「咔」的一声。
            unsafe {
                let data = render
                    .GetBuffer(buffer_frames)
                    .map_err(|e| err("取播放缓冲", e))?;
                std::ptr::write_bytes(
                    data,
                    0,
                    buffer_frames as usize * format.channels as usize * (format.bits as usize / 8),
                );
                render
                    .ReleaseBuffer(buffer_frames, 0)
                    .map_err(|e| err("还播放缓冲", e))?;
            }

            unsafe { client.Start() }.map_err(|e| err("启动播放流", e))?;
            self.stream = Some(RenderStream {
                default_watch,
                _com: com,
                client,
                render,
                event,
                format,
                buffer_frames,
                opened_on: OpenedOn::here(),
            });
        }
        Ok(self.stream.as_mut().expect("刚设过"))
    }
}

impl Render for WasapiRender {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        self.open()?;
        let Self {
            stream, staging, ..
        } = self;
        let stream = stream.as_mut().expect("刚打开过");
        stream.opened_on.check("播放")?;
        stream.default_watch.check()?;
        let channels = stream.format.channels as usize;
        let sample_bytes = stream.format.bits as usize / 8;
        let bytes_per_frame = channels * sample_bytes;

        staging.clear();
        upmix_into(frame, channels, &stream.format, staging);

        let mut written_frames = 0usize;
        let total_frames = frame.len();

        while written_frames < total_frames {
            // 队列满的时候在这儿阻塞 —— 这就是播放线程的时钟。
            let wait = unsafe { WaitForSingleObject(stream.event.0, WAIT_TIMEOUT_MS) };
            if wait != WAIT_OBJECT_0 {
                return Err(io::Error::other(
                    "播放设备没反应了 —— 可能是拔掉了，或者被别的程序独占了",
                ));
            }

            let padding = unsafe { stream.client.GetCurrentPadding() }
                .map_err(|e| err("问播放队列有多满", e))?;
            let free = stream.buffer_frames.saturating_sub(padding) as usize;
            let n = free.min(total_frames - written_frames);
            if n == 0 {
                continue;
            }

            // SAFETY: 紧跟着 ReleaseBuffer，中间只往里写 n 帧。
            unsafe {
                let data = stream
                    .render
                    .GetBuffer(n as u32)
                    .map_err(|e| err("取播放缓冲", e))?;
                let src = &staging[written_frames * bytes_per_frame..][..n * bytes_per_frame];
                std::ptr::copy_nonoverlapping(src.as_ptr(), data, src.len());
                stream
                    .render
                    .ReleaseBuffer(n as u32, 0)
                    .map_err(|e| err("还播放缓冲", e))?;
            }
            written_frames += n;
        }
        Ok(())
    }
}

impl Drop for RenderStream {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// 把单声道铺到设备的声道数上，按设备的样点格式写进 `out`。
fn upmix_into(mono: &[f32], channels: usize, format: &Format, out: &mut Vec<u8>) {
    if format.float {
        for &sample in mono {
            let bytes = sample.to_le_bytes();
            for _ in 0..channels {
                out.extend_from_slice(&bytes);
            }
        }
    } else {
        for &sample in mono {
            // 截断到 ±1 再转定点。不截的话溢出会绕回去，
            // 一个偏大的样点会变成一个反相的尖峰 —— 听起来是「啪」的一声。
            let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            let bytes = value.to_le_bytes();
            for _ in 0..channels {
                out.extend_from_slice(&bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_microphone_failure_never_opens_default_input() {
        let mut attempts = Vec::new();
        let result = super::resolve_device(Some("missing"), super::Direction::Capture, |id| {
            attempts.push(id.map(str::to_owned));
            if id.is_some() {
                Err("missing microphone")
            } else {
                Ok("other microphone")
            }
        });
        assert_eq!(result, Err("missing microphone"));
        assert_eq!(attempts, vec![Some("missing".into())]);
    }

    #[test]
    fn fixed_output_failure_can_fall_back_but_success_keeps_selection() {
        let mut attempts = Vec::new();
        let result = super::resolve_device(Some("missing"), super::Direction::Render, |id| {
            attempts.push(id.map(str::to_owned));
            if id.is_some() {
                Err("missing speaker")
            } else {
                Ok("default speaker")
            }
        });
        assert_eq!(result, Ok("default speaker"));
        assert_eq!(attempts, vec![Some("missing".into()), None]);
        let result = super::resolve_device(Some("fixed"), super::Direction::Capture, |id| {
            assert_eq!(id, Some("fixed"));
            Ok::<_, ()>("fixed")
        });
        assert_eq!(result, Ok("fixed"));
    }

    #[test]
    fn default_watch_detects_switch_and_removal_without_polling_fixed_devices() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut watch = super::DefaultWatch {
            id: Some("a".into()),
            checked_at: start,
            direction: super::Direction::Capture,
        };
        watch
            .check_with(start + Duration::from_millis(999), || panic!("too early"))
            .unwrap();
        watch
            .check_with(start + Duration::from_secs(1), || Ok("a".into()))
            .unwrap();
        assert!(watch
            .check_with(start + Duration::from_secs(2), || Ok("b".into()))
            .unwrap_err()
            .to_string()
            .contains("已切换"));
        assert!(watch
            .check_with(start + Duration::from_secs(3), || Err(
                std::io::Error::other("no default")
            ))
            .is_err());
        watch.id = None;
        watch
            .check_with(start + Duration::from_secs(4), || {
                panic!("fixed device queried default")
            })
            .unwrap();
    }
    use super::*;

    fn format(channels: u16, float: bool) -> Format {
        Format {
            sample_rate: SAMPLE_RATE,
            channels,
            bits: if float { 32 } else { 16 },
            float,
        }
    }

    #[test]
    fn upmix_repeats_the_mono_sample_on_every_channel() {
        let mut out = Vec::new();
        upmix_into(&[0.5, -0.5], 2, &format(2, true), &mut out);
        let samples: Vec<f32> = out
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        assert_eq!(samples, vec![0.5, 0.5, -0.5, -0.5]);
    }

    /// 定点输出必须先截断。溢出绕回去会把一个偏大的样点变成反相尖峰，
    /// 听起来是「啪」的一声，而且只在音量大的时候出现 —— 最难查的那种。
    #[test]
    fn upmix_clamps_instead_of_wrapping() {
        let mut out = Vec::new();
        upmix_into(&[2.0, -2.0], 1, &format(1, false), &mut out);
        let samples: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(samples, vec![i16::MAX, -i16::MAX]);
    }

    /// 多声道要取平均，不能只拿左声道 —— 很多耳麦把人声放在其中一个声道上。
    #[test]
    fn downmix_averages_all_channels() {
        let stereo: Vec<f32> = vec![1.0, 0.0, 0.5, 0.5];
        let bytes: Vec<u8> = stereo.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        unsafe { downmix_into(bytes.as_ptr(), 2, 2, &format(2, true), &mut out) };
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn downmix_handles_sixteen_bit() {
        let samples: Vec<i16> = vec![i16::MAX, i16::MAX, 0, 0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        unsafe { downmix_into(bytes.as_ptr(), 2, 2, &format(2, false), &mut out) };
        assert_eq!(out.len(), 2);
        assert!((out[0] - 1.0).abs() < 0.001);
        assert_eq!(out[1], 0.0);
    }

    /// 采样率不对的时候，报错要告诉用户去哪儿改。
    #[test]
    fn a_wrong_sample_rate_says_where_to_fix_it() {
        let text = wrong_rate(&format(2, true), "录音").to_string();
        // format() 里写死了 48000，这里单独造一个不对的
        let bad = Format {
            sample_rate: 44_100,
            ..format(2, true)
        };
        let text2 = wrong_rate(&bad, "录音").to_string();
        assert!(text2.contains("44100"), "{text2}");
        assert!(text2.contains("48000"), "{text2}");
        assert!(text2.contains("设置"), "要说清楚去哪儿改：{text2}");
        let _ = text;
    }
}

#[cfg(test)]
mod device_scan {
    use super::*;
    use crate::audio::Capture;

    /// 挨个打开每个**真硬件**麦克风，报一下它到底在不在出声。
    ///
    /// 默认跳过 —— 要真设备，而且会把每个麦克风都开一遍。
    ///
    /// ```bash
    /// cargo test -p voice-core --lib device_scan -- --ignored --nocapture
    /// ```
    ///
    /// 「为什么没声音」的排查顺序是：先看这个表，找出哪个设备有电平，
    /// 然后在设置里选它。系统默认那个经常是虚拟声卡，而虚拟声卡没在路由的
    /// 时候是纯静音的。
    #[test]
    #[ignore = "要真声卡"]
    fn which_microphones_actually_hear_something() {
        let endpoints = crate::wasapi::list_endpoints(Direction::Capture).unwrap_or_default();
        assert!(!endpoints.is_empty(), "一个录音设备都没有");

        println!();
        println!("{:<52} {:>8}  说明", "设备", "峰值 dB");
        for endpoint in &endpoints {
            let mut capture = WasapiCapture::new(Some(endpoint.id.clone()));
            let mut frame = vec![0.0f32; FRAME_SAMPLES];
            let mut peak = f32::NEG_INFINITY;
            let mut note = String::new();

            // 半秒够看出有没有信号了
            for _ in 0..50 {
                match capture.read(&mut frame) {
                    Ok(true) => {
                        let rms =
                            (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
                        let db = if rms <= 1e-9 {
                            -120.0
                        } else {
                            20.0 * rms.log10()
                        };
                        peak = peak.max(db);
                    }
                    Ok(false) => break,
                    Err(e) => {
                        // 打不开是常事：设备被独占了、驱动不认共享格式。
                        // 这正是要报出来的东西。
                        note = format!("{e}").lines().next().unwrap_or("").to_string();
                        break;
                    }
                }
            }

            let kind = if endpoint.is_hardware {
                ""
            } else {
                "（虚拟）"
            };
            let name = format!("{}{kind}", endpoint.name);
            let level = if peak.is_finite() {
                format!("{peak:>8.1}")
            } else {
                "       -".to_string()
            };
            println!("{name:<52} {level}  {note}");
        }
        println!();
        println!("峰值在 -50 dB 以上才算真的收到音。-120 是纯数字静音。");
    }
}
