// SPDX-License-Identifier: MPL-2.0

//! 单独试麦：只开采集和播放，**完全不碰网络**。
//!
//! # 为什么不复用 `pipeline`
//!
//! [`crate::pipeline::Pipeline`] 要 session id、服务器地址、两把密钥 ——
//! 那些东西只有连上服务器之后才有。而「我的麦克风好不好使」这个问题，
//! **恰恰在连不上的时候最想知道**：是我麦克风坏了，还是服务器的问题？
//!
//! 硬凑的话就得给它一个假地址和假密钥，然后往一个死地址上发包。
//! 这里两个线程六十行，比那干净。
//!
//! # 跟 `pipeline` 里那套的关系
//!
//! 电平怎么算、试听队列怎么攒，两边是同一套规矩（见 [`crate::pipeline`]
//! 的模块文档）。两边都改的时候要一起改 —— 不一致的话，用户会发现
//! 「试麦时条子动，进了频道就不动了」。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::audio::{Capture, Render, FRAME_SAMPLES};
use crate::pipeline::AudioProcessor;

/// 试听队列最深攒几帧。跟 `pipeline` 里那个同理，见那边的注释。
const MONITOR_MAX_FRAMES: usize = 3;

const SILENT_DB_CENTI: i32 = -12_000;

struct Shared {
    input_centi_db: AtomicI32,
    monitoring: AtomicBool,
    monitor: Mutex<std::collections::VecDeque<Vec<f32>>>,
    /// 提示音的口子。没连服务器时在设置页上「试听提示音」要用。
    cues: Arc<crate::cue::CueQueue>,
    stop: AtomicBool,
    /// 采集那边有没有真的读到过一帧。界面上要分清「设备还没开起来」
    /// 和「开起来了但是静音」—— 这两个的下一步完全不一样。
    running: AtomicBool,
    /// 设备出错时的说明，直接显示给用户。
    error: Mutex<Option<String>>,
}

impl Shared {
    /// 采集那边挂了：整个试麦就没意义了，两边一起停。
    fn capture_failed(&self, e: io::Error) {
        *self.error.lock().expect("error poisoned") = Some(e.to_string());
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 播放那边挂了：**采集继续跑**。
    ///
    /// 看电平根本不需要扬声器 —— 而「我的麦克风有没有在收音」恰恰是用户
    /// 最想知道的那件事。第一版把两个线程写成了连坐，结果默认扬声器
    /// （96 kHz 的虚拟设备）打不开时，麦克风也跟着停，表现是
    /// 「麦克风灯闪一下就灭」，而界面上什么都不说。
    fn render_failed(&self, e: io::Error) {
        *self.error.lock().expect("error poisoned") = Some(format!(
            "听不到声音：{e}
（麦克风电平不受影响，还能看）"
        ));
    }
}

/// 一次试麦。丢掉它就会把两个线程收干净、把设备还回去。
pub struct MicCheck {
    control: MicCheckControl,
    threads: Vec<JoinHandle<()>>,
}

/// 无 GUI 依赖的轻量试麦控制句柄；释放它不会停止或等待设备线程。
#[derive(Clone)]
pub struct MicCheckControl {
    shared: Arc<Shared>,
}

impl MicCheck {
    /// 开始试麦。**立刻返回**，两个线程在后台跑。
    ///
    /// 设备打不开不在这里报错 —— 设备是在音频线程上打开的（COM 的线程亲和性，
    /// 见 `wasapi::live`），所以错误要从 [`MicCheckControl::error`] 取。
    pub fn start(
        mut capture: Box<dyn Capture>,
        mut render: Box<dyn Render>,
        processor: Option<Box<dyn AudioProcessor>>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            input_centi_db: AtomicI32::new(SILENT_DB_CENTI),
            monitoring: AtomicBool::new(false),
            monitor: Mutex::new(std::collections::VecDeque::new()),
            cues: Arc::new(crate::cue::CueQueue::new()),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let processor: Option<Arc<dyn AudioProcessor>> = processor.map(Arc::from);
        // 后续线程创建失败时，所有者负责回收已启动的线程。
        let mut check = Self {
            control: MicCheckControl {
                shared: Arc::clone(&shared),
            },
            threads: Vec::new(),
        };

        {
            let shared = Arc::clone(&shared);
            let processor = processor.clone();
            check.threads.push(
                std::thread::Builder::new()
                    .name("gouhuo-miccheck-in".into())
                    .spawn(move || {
                        struct RunningReset<'a>(&'a AtomicBool);
                        impl Drop for RunningReset<'_> {
                            fn drop(&mut self) {
                                self.0.store(false, Ordering::Relaxed);
                            }
                        }
                        let _running = RunningReset(&shared.running);
                        let mut frame = vec![0.0f32; FRAME_SAMPLES];
                        while !shared.stop.load(Ordering::Relaxed) {
                            match capture.read(&mut frame) {
                                Ok(true) => {}
                                Ok(false) => {
                                    if !shared.stop.load(Ordering::Relaxed) {
                                        shared.capture_failed(io::Error::other(
                                            "麦克风录音已停止，请检查设备连接后重试。",
                                        ));
                                    }
                                    break;
                                }
                                Err(e) => {
                                    if !shared.stop.load(Ordering::Relaxed) {
                                        shared.capture_failed(e);
                                    }
                                    return;
                                }
                            }
                            if shared.stop.load(Ordering::Relaxed) {
                                return;
                            }
                            shared.running.store(true, Ordering::Relaxed);
                            if let Some(processor) = &processor {
                                processor.process_capture(&mut frame);
                            }
                            shared.input_centi_db.store(
                                (crate::pipeline::frame_db(&frame) * 100.0) as i32,
                                Ordering::Relaxed,
                            );
                            if shared.monitoring.load(Ordering::Relaxed) {
                                let mut queue = shared.monitor.lock().expect("monitor poisoned");
                                if queue.len() >= MONITOR_MAX_FRAMES {
                                    queue.pop_front();
                                }
                                queue.push_back(frame.clone());
                            }
                        }
                    })?,
            );
        }

        {
            let shared = Arc::clone(&shared);
            check.threads.push(
                std::thread::Builder::new()
                    .name("gouhuo-miccheck-out".into())
                    .spawn(move || {
                        let mut out = vec![0.0f32; FRAME_SAMPLES];
                        while !shared.stop.load(Ordering::Relaxed) {
                            out.fill(0.0);
                            if let Some(mine) =
                                shared.monitor.lock().expect("monitor poisoned").pop_front()
                            {
                                out.copy_from_slice(&mine);
                            }
                            shared.cues.mix_into(&mut out);
                            // **没开试听的时候也要照常播静音。**
                            // 播放线程的节拍来自设备（write 会阻塞），
                            // 不转的话打开试听时要先等设备预热，听起来像卡了一下。
                            if let Err(e) = render.write(&out) {
                                // **不停采集。** 见 Shared::render_failed。
                                shared.render_failed(e);
                                return;
                            }
                        }
                    })?,
            );
        }

        Ok(check)
    }

    pub fn control(&self) -> MicCheckControl {
        self.control.clone()
    }
}

impl std::ops::Deref for MicCheck {
    type Target = MicCheckControl;

    fn deref(&self) -> &Self::Target {
        &self.control
    }
}

impl MicCheckControl {
    /// 请求退出，不等待音频线程；等待退出由 MicCheck 的所有者完成。
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.running.store(false, Ordering::Relaxed);
    }
    /// 麦克风当前电平，分贝（满刻度 0 dB）。
    pub fn input_db(&self) -> f32 {
        self.shared.input_centi_db.load(Ordering::Relaxed) as f32 / 100.0
    }

    /// 试听。**用扬声器开会啸叫**，界面上要写清楚戴耳机。
    pub fn set_monitoring(&self, on: bool) {
        self.shared.monitoring.store(on, Ordering::Relaxed);
        if !on {
            self.shared
                .monitor
                .lock()
                .expect("monitor poisoned")
                .clear();
        }
    }

    /// 往播放里插提示音的口子，跟 `Pipeline::cues` 一样。
    pub fn cues(&self) -> Arc<crate::cue::CueQueue> {
        Arc::clone(&self.shared.cues)
    }

    pub fn is_monitoring(&self) -> bool {
        self.shared.monitoring.load(Ordering::Relaxed)
    }

    /// 采集那边真的转起来了没有。
    pub fn is_running(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed) && self.shared.running.load(Ordering::Relaxed)
    }

    /// 设备出了什么问题，能直接显示给用户。
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().expect("error poisoned").clone()
    }
}

impl Drop for MicCheck {
    fn drop(&mut self) {
        self.shutdown();
        // 两个线程都阻塞在设备上，最多一个设备周期（10 ms 上下）就会醒。
        // 不需要额外的叫醒机制 —— 这也是「让设备当时钟」的附带好处。
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// 一个麦克风的检测结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ScanResult {
    /// WASAPI 的设备 id，选中它的时候要用。
    pub id: String,
    pub name: String,
    pub is_hardware: bool,
    /// 这段时间里的峰值电平，分贝。打不开就是 `None`。
    pub peak_db: Option<f32>,
    /// 打不开的原因，能直接显示给用户。
    pub error: Option<String>,
}

impl ScanResult {
    /// 真的收到人声了没有。
    ///
    /// −50 dB 这条线是这么定的：真硬件的底噪在 −100 dB 上下，
    /// 虚拟设备没在路由时是 −120（纯数字零），而正常说话在 −30 到 −15 之间。
    /// 中间空得很开，怎么定都不会误判。
    pub fn hears_something(&self) -> bool {
        self.peak_db.is_some_and(|db| db > -50.0)
    }

    /// 一句话说清楚这个设备现在什么情况。
    pub fn verdict(&self) -> String {
        match (&self.error, self.peak_db) {
            (Some(e), _) => e.lines().next().unwrap_or("打不开").to_string(),
            (None, None) => "打不开".to_string(),
            (None, Some(db)) if db > -50.0 => format!("听到了（{db:.0} dB）"),
            (None, Some(db)) if db > PURE_SILENCE_DB => {
                format!("只有底噪（{db:.0} dB）—— 设备开着，但没收到人声")
            }
            (None, Some(_)) => "纯静音 —— 没在路由、被静音了、或者没插".to_string(),
        }
    }
}

/// 「这个设备送上来的全是零」的界线。
///
/// 实测过的三档差得很开：纯数字零正好是 −120（[`crate::pipeline::frame_db`]
/// 的下限），真硬件的底噪在 −104 上下，正常说话在 −30 到 −15。
/// 划在 −118 是为了只把「一个非零样点都没有」归进纯静音那一类 ——
/// 这两类的下一步完全不一样：底噪说明设备在工作但没收到人声（静音键、
/// 隐私设置），纯静音说明这个设备根本没在路由。
const PURE_SILENCE_DB: f32 = -118.0;

/// 挨个打开每个麦克风，看哪个真的收到声音。
///
/// **这个函数会阻塞**，每个设备占 `per_device`，要在后台线程上调。
///
/// # 为什么要有这个
///
/// 「为什么没声音」在 Windows 上有一堆长得一模一样的原因：选中的是没在路由的
/// 虚拟声卡、麦克风被系统静音了、隐私设置挡住了、耳机没开机。用户面对一个
/// 下拉框是猜不出来的 —— 而程序挨个试一遍只要几秒钟。
///
/// 调用方要提示用户**在检测期间一直说话**，否则每个设备都只会报底噪。
pub fn scan_microphones(per_device: std::time::Duration) -> Vec<ScanResult> {
    scan_microphones_cancellable(per_device, Arc::new(AtomicBool::new(false)))
}

/// 可取消的扫描。取消不算设备故障，返回此前完成的结果及当前已取得的电平。
///
/// 每个设备和每帧读取前检查标记。已进行的设备枚举/打开/读取仍需由操作系统
/// 返回，调用方必须在后台运行并等待此函数结束后才重新开启其他音频链路。
#[cfg(windows)]
pub fn scan_microphones_cancellable(
    per_device: std::time::Duration,
    cancel: Arc<AtomicBool>,
) -> Vec<ScanResult> {
    if cancel.load(Ordering::Acquire) {
        return Vec::new();
    }
    let Ok(endpoints) = crate::wasapi::list_endpoints(crate::wasapi::Direction::Capture) else {
        return Vec::new();
    };
    scan_devices(
        endpoints.into_iter().map(|endpoint| ScanDevice {
            id: endpoint.id,
            name: endpoint.name,
            is_hardware: endpoint.is_hardware,
        }),
        per_device,
        &cancel,
        |id| {
            Ok(Box::new(crate::wasapi::WasapiCapture::new(Some(
                id.to_owned(),
            ))))
        },
    )
}

/// 其他平台还没有麦克风扫描后端，返回空设备列表。
#[cfg(not(windows))]
pub fn scan_microphones_cancellable(
    _per_device: std::time::Duration,
    _cancel: Arc<AtomicBool>,
) -> Vec<ScanResult> {
    Vec::new()
}

#[cfg(any(windows, test))]
struct ScanDevice {
    id: String,
    name: String,
    is_hardware: bool,
}

#[cfg(any(windows, test))]
fn scan_devices(
    devices: impl IntoIterator<Item = ScanDevice>,
    per_device: std::time::Duration,
    cancel: &AtomicBool,
    mut open: impl FnMut(&str) -> io::Result<Box<dyn Capture>>,
) -> Vec<ScanResult> {
    let mut results = Vec::new();
    for device in devices {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let (peak_db, error) = match open(&device.id) {
            Ok(mut capture) => sample_capture(&mut *capture, per_device, cancel),
            Err(error) => (None, Some(error.to_string())),
        };
        // A cancelled device that produced no samples has no verdict. Do not
        // turn user cancellation into an artificial "cannot open" result.
        if cancel.load(Ordering::Acquire) && peak_db.is_none() {
            break;
        }
        results.push(ScanResult {
            id: device.id,
            name: device.name,
            is_hardware: device.is_hardware,
            peak_db,
            error,
        });
    }
    results
}

#[cfg(any(windows, test))]
fn sample_capture(
    capture: &mut dyn Capture,
    per_device: std::time::Duration,
    cancel: &AtomicBool,
) -> (Option<f32>, Option<String>) {
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut peak = f32::NEG_INFINITY;
    let mut error = None;
    let deadline = std::time::Instant::now() + per_device;
    while std::time::Instant::now() < deadline && !cancel.load(Ordering::Acquire) {
        match capture.read(&mut frame) {
            Ok(true) => peak = peak.max(crate::pipeline::frame_db(&frame)),
            Ok(false) => break,
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }
    (peak.is_finite().then_some(peak), error)
}

#[cfg(test)]
mod scan_cancellation {
    use super::*;
    use std::{
        sync::{atomic::AtomicUsize, mpsc},
        time::Duration,
    };

    fn device(id: &str) -> ScanDevice {
        ScanDevice {
            id: id.into(),
            name: id.into(),
            is_hardware: true,
        }
    }

    #[test]
    fn a_cancelled_scan_does_not_open_any_device_or_invent_a_failure() {
        let cancel = AtomicBool::new(true);
        let results = scan_devices(
            [device("first"), device("second")],
            Duration::from_secs(1),
            &cancel,
            |_| panic!("pre-cancelled scan opened a device"),
        );
        assert!(results.is_empty());
    }

    #[test]
    fn cancellation_while_reading_keeps_the_real_peak_and_skips_the_next_device() {
        struct ControlledCapture {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            reads: Arc<AtomicUsize>,
        }
        impl Capture for ControlledCapture {
            fn read(&mut self, frame: &mut [f32]) -> io::Result<bool> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                frame.fill(0.2);
                Ok(true)
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (entered, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let reads = Arc::new(AtomicUsize::new(0));
        let worker_reads = Arc::clone(&reads);
        let (finished, results) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut capture = Some(ControlledCapture {
                entered,
                release: released,
                reads: worker_reads,
            });
            let scanned = scan_devices(
                [device("first"), device("must-not-open")],
                Duration::from_secs(30),
                &worker_cancel,
                |id| {
                    assert_eq!(id, "first", "cancellation opened the next microphone");
                    Ok(Box::new(capture.take().unwrap()))
                },
            );
            finished.send(scanned).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        cancel.store(true, Ordering::Release);
        release.send(()).unwrap();
        let scanned = results
            .recv_timeout(Duration::from_secs(1))
            .expect("cancellation waited for the full device duration");
        worker.join().unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].id, "first");
        assert!(scanned[0].peak_db.unwrap() > -20.0);
        assert!(
            scanned[0].error.is_none(),
            "cancellation became a device error"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{CollectingRender, SyntheticCapture};
    use std::time::Duration;

    fn tone(frames: usize) -> Vec<f32> {
        (0..FRAME_SAMPLES * frames)
            .map(|i| 0.2 * (i as f32 * 0.05).sin())
            .collect()
    }

    #[test]
    fn the_level_follows_the_microphone() {
        let mut source = vec![0.0f32; FRAME_SAMPLES * 30];
        source.extend_from_slice(&tone(60));

        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(source).then_silence()),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(200));
        let silent = check.input_db();
        std::thread::sleep(Duration::from_millis(500));
        let loud = check.input_db();

        assert!(silent < -90.0, "静音时该贴底，实际 {silent:.1} dB");
        assert!(
            loud > silent + 40.0,
            "有声音时该抬起来：{silent:.1} -> {loud:.1}"
        );
    }

    #[test]
    fn monitoring_routes_the_mic_to_the_speakers() {
        let (render, played) = CollectingRender::new();
        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(tone(100)).then_silence()),
            Box::new(render),
            None,
        )
        .unwrap();

        // 没开试听：播出去的必须是纯静音
        std::thread::sleep(Duration::from_millis(300));
        let quiet: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
        assert_eq!(quiet, 0.0, "没开试听却有声音");

        check.set_monitoring(true);
        std::thread::sleep(Duration::from_millis(300));
        let loud: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
        assert!(loud > 0.0, "开了试听还是没声音");
    }

    /// 丢掉就该把线程收干净，而且不能挂住。
    #[test]
    fn dropping_stops_promptly() {
        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(tone(1000)).then_silence()),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let start = std::time::Instant::now();
        drop(check);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "关试麦花了 {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_control_handle_is_send_sync_and_releasing_it_does_not_join_capture() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MicCheckControl>();
        struct WaitingCapture(std::sync::mpsc::Receiver<()>);
        impl Capture for WaitingCapture {
            fn read(&mut self, frame: &mut [f32]) -> io::Result<bool> {
                // 有界阻塞也让断言失败时的所有者清理能够完成。
                match self.0.recv_timeout(Duration::from_secs(2)) {
                    Ok(()) => {
                        frame.fill(0.2);
                        Ok(true)
                    }
                    Err(_) => Ok(false),
                }
            }
        }
        let (frames, input) = std::sync::mpsc::channel();
        let check = MicCheck::start(
            Box::new(WaitingCapture(input)),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();
        let control = check.control();
        let (finished, completion) = std::sync::mpsc::channel();
        let released = std::thread::spawn(move || {
            drop(control);
            finished.send(()).unwrap();
        });
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("试麦控制句柄等待了采集线程");
        released.join().unwrap();
        frames.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !check.is_running() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(check.input_db() > -45.0);
        let survivor = check.control();
        survivor.shutdown();
        assert!(!survivor.is_running());
        drop(frames);
        drop(check);
        assert!(!survivor.is_running());
    }

    #[test]
    fn unexpected_capture_eof_is_an_error_but_requested_shutdown_eof_is_not() {
        struct ControlledCapture {
            commands: std::sync::mpsc::Receiver<Option<f32>>,
            reads: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Capture for ControlledCapture {
            fn read(&mut self, frame: &mut [f32]) -> io::Result<bool> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                match self.commands.recv() {
                    Ok(Some(value)) => {
                        frame.fill(value);
                        Ok(true)
                    }
                    Ok(None) | Err(_) => Ok(false),
                }
            }
        }
        struct Fixture {
            check: Option<MicCheck>,
            commands: Option<std::sync::mpsc::Sender<Option<f32>>>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                // Unblock capture before joining, including when an assertion fails.
                self.commands.take();
                self.check.take();
            }
        }
        fn wait(predicate: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !predicate() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "controlled capture did not progress"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        for requested_shutdown in [false, true] {
            let (commands, input) = std::sync::mpsc::channel();
            let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let check = MicCheck::start(
                Box::new(ControlledCapture {
                    commands: input,
                    reads: Arc::clone(&reads),
                }),
                Box::new(crate::audio::NullRender::default()),
                None,
            )
            .unwrap();
            let fixture = Fixture {
                check: Some(check),
                commands: Some(commands),
            };
            let check = fixture.check.as_ref().unwrap();
            let commands = fixture.commands.as_ref().unwrap();
            commands.send(Some(0.2)).unwrap();
            // Starting the second read confirms the first frame's state is published.
            wait(|| reads.load(Ordering::SeqCst) == 2);
            assert!(check.is_running());
            if requested_shutdown {
                check.shutdown();
            }
            commands.send(None).unwrap();
            wait(|| check.threads[0].is_finished());
            assert!(
                !check.is_running(),
                "ended capture must not retain a running indicator"
            );
            if requested_shutdown {
                assert!(
                    check.error().is_none(),
                    "intentional stop was reported as a device failure"
                );
            } else {
                assert!(check
                    .error()
                    .is_some_and(|error| error.contains("录音已停止")));
            }
        }
    }

    /// 采集出错要报出来，而且整个试麦停下。
    #[test]
    fn a_capture_error_stops_everything() {
        struct Broken;
        impl Capture for Broken {
            fn read(&mut self, _out: &mut [f32]) -> io::Result<bool> {
                Err(io::Error::other("麦克风拔了"))
            }
        }

        let check = MicCheck::start(
            Box::new(Broken),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(check.error().as_deref(), Some("麦克风拔了"));
        assert!(!check.is_running(), "出错了还报在跑");
    }

    /// **播放出错不能把采集也停掉。**
    ///
    /// 看电平根本不需要扬声器，而「麦克风有没有在收音」恰恰是用户最想知道的。
    /// 第一版两个线程连坐，结果默认扬声器打不开时麦克风也跟着停 ——
    /// 表现是「麦克风灯闪一下就灭」，界面上还什么都不说。
    #[test]
    fn a_render_error_leaves_the_level_meter_working() {
        struct BrokenSpeaker;
        impl Render for BrokenSpeaker {
            fn write(&mut self, _frame: &[f32]) -> io::Result<()> {
                Err(io::Error::other("扬声器打不开"))
            }
        }

        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(tone(200)).then_silence()),
            Box::new(BrokenSpeaker),
            None,
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(400));
        let error = check.error().expect("播放出错了却没报");
        assert!(error.contains("听不到声音"), "{error}");
        assert!(
            error.contains("电平不受影响"),
            "要告诉用户还能干什么：{error}"
        );

        assert!(check.is_running(), "播放挂了把采集也停了");
        assert!(
            check.input_db() > -90.0,
            "播放挂了之后电平表也不动了：{} dB",
            check.input_db()
        );
    }
}

#[cfg(test)]
mod verdicts {
    use super::*;

    fn result(peak_db: Option<f32>, error: Option<&str>) -> ScanResult {
        ScanResult {
            id: "x".into(),
            name: "某个麦克风".into(),
            is_hardware: true,
            peak_db,
            error: error.map(str::to_string),
        }
    }

    /// 三种情况必须分得开，因为下一步完全不一样：
    /// 有人声 → 就选它；只有底噪 → 麦克风没被收到音（静音键？隐私设置？）；
    /// 纯静音 → 这个设备根本没在路由。
    #[test]
    fn the_three_cases_are_distinguishable() {
        // 这三个数字是这台机器上实测出来的，不是编的：
        // 说话 −25 上下、Arctis 的底噪 −104、Sonar 虚拟麦 正好 −120
        assert!(result(Some(-25.0), None).hears_something());
        assert!(!result(Some(-104.0), None).hears_something());
        assert!(!result(Some(-120.0), None).hears_something());

        assert!(result(Some(-25.0), None).verdict().contains("听到了"));
        assert!(
            result(Some(-104.0), None).verdict().contains("底噪"),
            "真硬件的底噪被当成纯静音了：{}",
            result(Some(-104.0), None).verdict()
        );
        assert!(result(Some(-120.0), None).verdict().contains("纯静音"));
    }

    /// 打不开的时候要把原因原样报出来 —— 采样率不对那条提示尤其要留着。
    #[test]
    fn an_open_failure_keeps_its_reason() {
        let r = result(
            None,
            Some("录音设备现在是 44100 Hz，篝火这一版只支持 48000 Hz。\n去设置改"),
        );
        assert!(!r.hears_something());
        assert!(r.verdict().contains("44100"));
        // 只取第一行：界面上一行放得下，完整的那段在别处已经说过了
        assert!(!r.verdict().contains('\n'));
    }

    #[test]
    fn a_device_that_never_opened_is_not_reported_as_silent() {
        assert_eq!(result(None, None).verdict(), "打不开");
    }
}

#[cfg(all(test, windows))]
mod live_scan {
    use super::*;

    /// 真机上跑一遍「挨个试麦克风」，把界面会显示的东西打出来。
    ///
    /// ```bash
    /// cargo test -p voice-core --lib live_scan -- --ignored --nocapture
    /// ```
    ///
    /// **跑的时候要一直对着麦克风说话**，否则每个设备都只会报底噪。
    #[test]
    #[ignore = "要真声卡，而且要一边说话"]
    #[cfg(windows)]
    fn scan_reports_what_the_ui_would_show() {
        let results = scan_microphones(std::time::Duration::from_millis(1200));
        assert!(!results.is_empty(), "一个录音设备都没列出来");

        println!();
        for r in &results {
            let mark = if r.hears_something() { "●" } else { "○" };
            let kind = if r.is_hardware { "" } else { "（虚拟）" };
            println!("{mark} {}{kind}\n    {}", r.name, r.verdict());
        }
        println!();
        match results.iter().find(|r| r.hears_something()) {
            Some(r) => println!("能用的：{}", r.name),
            None => println!("一个都没收到人声 —— 要么没人说话，要么麦克风全被挡住了"),
        }
    }
}

#[cfg(all(test, windows))]
mod endurance {
    use super::*;

    /// 每个麦克风开着跑一段时间，看谁**中途死掉**。
    ///
    /// ```bash
    /// cargo test -p voice-core --lib endurance -- --ignored --nocapture
    /// ```
    ///
    /// 「麦克风灯闪一下就灭」对应的就是这里报出来的死亡 —— 设备打开了，
    /// 但某一次读失败，两个线程一起退出，设备被还回去。
    #[test]
    #[ignore = "要真声卡，而且要跑十几秒"]
    #[cfg(windows)]
    fn every_microphone_survives_being_held_open() {
        let endpoints =
            crate::wasapi::list_endpoints(crate::wasapi::Direction::Capture).unwrap_or_default();
        assert!(!endpoints.is_empty(), "一个录音设备都没有");

        const HOLD: std::time::Duration = std::time::Duration::from_secs(6);
        println!();
        for endpoint in &endpoints {
            let check = MicCheck::start(
                Box::new(crate::wasapi::WasapiCapture::new(Some(endpoint.id.clone()))),
                Box::new(crate::audio::NullRender::default()),
                None,
            )
            .expect("起不了试麦");

            // 每 200 ms 看一次还活着没有，记下死亡时刻
            let started = std::time::Instant::now();
            let mut died_at = None;
            while started.elapsed() < HOLD {
                std::thread::sleep(std::time::Duration::from_millis(200));
                if check.error().is_some() {
                    died_at = Some(started.elapsed());
                    break;
                }
            }

            let verdict = match (died_at, check.error()) {
                (Some(t), Some(e)) => format!(
                    "**{:.1} 秒后死了**：{}",
                    t.as_secs_f32(),
                    e.lines().next().unwrap_or("")
                ),
                _ if check.is_running() => format!("撑住了（{:.0} 秒）", HOLD.as_secs_f32()),
                _ => "一帧都没读到".to_string(),
            };
            let kind = if endpoint.is_hardware {
                ""
            } else {
                "（虚拟）"
            };
            println!("{}{kind}\n    {verdict}", endpoint.name);
        }
        println!();
    }
}

#[cfg(all(test, windows))]
mod render_check {
    use super::*;
    use crate::audio::Render;

    /// 挨个试播放设备。
    ///
    /// ```bash
    /// cargo test -p voice-core --lib render_check -- --ignored --nocapture
    /// ```
    ///
    /// 播放设备打不开的后果比看起来严重：试麦的两个线程是连坐的，
    /// 播放那边一死，采集也跟着停 —— 表现就是「麦克风灯闪一下就灭」。
    #[test]
    #[ignore = "要真声卡，会往扬声器写静音"]
    #[cfg(windows)]
    fn every_speaker_can_be_opened() {
        let endpoints =
            crate::wasapi::list_endpoints(crate::wasapi::Direction::Render).unwrap_or_default();
        assert!(!endpoints.is_empty(), "一个播放设备都没有");

        println!();
        for endpoint in &endpoints {
            let mut render = crate::wasapi::WasapiRender::new(Some(endpoint.id.clone()));
            let silence = vec![0.0f32; FRAME_SAMPLES];
            let outcome = match render.write(&silence) {
                Ok(()) => "能打开".to_string(),
                Err(e) => format!("**打不开**：{}", e.to_string().lines().next().unwrap_or("")),
            };
            let kind = if endpoint.is_hardware {
                ""
            } else {
                "（虚拟）"
            };
            let mark = if endpoint.is_default { "→ " } else { "  " };
            println!("{mark}{}{kind}\n     {outcome}", endpoint.name);
        }
        println!("\n（→ 是系统默认的那个）");
    }
}
