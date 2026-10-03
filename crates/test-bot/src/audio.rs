// SPDX-License-Identifier: GPL-3.0-or-later
use client_runtime::{AudioBackend, OpenedCapture};
use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use voice_core::audio::{Capture, Render, FRAME_SAMPLES, SAMPLE_RATE};

// Synthetic devices must yield CPU: a per-device precision spin would consume
// several cores before we even measure the server with dozens of bots.
struct Pace {
    next: Instant,
}

impl Pace {
    fn new() -> Self {
        Self {
            next: Instant::now() + Duration::from_millis(10),
        }
    }

    fn tick(&mut self) {
        if let Some(left) = self.next.checked_duration_since(Instant::now()) {
            std::thread::sleep(left);
        }
        self.next += Duration::from_millis(10);
        let now = Instant::now();
        // Missed deadlines are skipped, rather than producing a catch-up burst.
        if self.next < now {
            self.next = now + Duration::from_millis(10);
        }
    }
}

pub const MAX_WAV_BYTES: usize = 10 * 1024 * 1024;
const ECHO_DELAY_FRAMES: usize = 20;
const ECHO_MAX_FRAMES: usize = 100;

pub fn load_wav(path: &Path) -> io::Result<Arc<Vec<f32>>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_WAV_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_WAV_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WAV exceeds 10 MiB",
        ));
    }
    let (samples, rate) = voice_core::cue::decode_wav(&bytes).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a non-empty 16-bit PCM WAV",
        )
    })?;
    if !(8000..=96000).contains(&rate) || samples.is_empty() || samples.len() > rate as usize * 300
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WAV must be 8–96 kHz and at most 300 seconds",
        ));
    }
    Ok(Arc::new(voice_core::cue::resample_to_48k(&samples, rate)))
}

pub fn tone() -> Arc<Vec<f32>> {
    Arc::new(
        voice_core::signal::chirp(SAMPLE_RATE as usize, SAMPLE_RATE, 150.0, 4000.0, 0.15)
            .into_iter()
            .map(|s| s as f32 / 32768.0)
            .collect(),
    )
}

#[derive(Default)]
pub struct Metrics {
    pub opens: AtomicU64,
    pub captured: AtomicU64,
    pub rendered: AtomicU64,
    pub audible: AtomicU64,
    pub echo_overflow: AtomicU64,
}

pub struct Backend {
    pub samples: Arc<Vec<f32>>,
    pub echo: bool,
    pub gain: f32,
    pub metrics: Arc<Metrics>,
    queue: Arc<Mutex<VecDeque<Vec<f32>>>>,
}

impl Backend {
    pub fn new(samples: Arc<Vec<f32>>, echo: bool, gain: f32) -> Self {
        Self {
            samples,
            echo,
            gain,
            metrics: Arc::new(Metrics::default()),
            queue: Arc::default(),
        }
    }
}

struct Input {
    samples: Arc<Vec<f32>>,
    cursor: usize,
    echo: bool,
    gain: f32,
    metrics: Arc<Metrics>,
    queue: Arc<Mutex<VecDeque<Vec<f32>>>>,
    ticker: Pace,
}

impl Capture for Input {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        if out.len() != FRAME_SAMPLES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a 10 ms capture frame",
            ));
        }
        self.ticker.tick();
        if self.echo {
            let mut queue = self.queue.lock().unwrap();
            if queue.len() >= ECHO_DELAY_FRAMES {
                let frame = queue.pop_front().unwrap();
                for (slot, sample) in out.iter_mut().zip(frame) {
                    *slot = sample * self.gain;
                }
            } else {
                out.fill(0.0);
            }
        } else {
            for slot in out.iter_mut() {
                *slot = self.samples[self.cursor] * self.gain;
                self.cursor = (self.cursor + 1) % self.samples.len();
            }
        }
        self.metrics.captured.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

struct Output {
    echo: bool,
    metrics: Arc<Metrics>,
    queue: Arc<Mutex<VecDeque<Vec<f32>>>>,
    ticker: Pace,
}

impl Render for Output {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        self.ticker.tick();
        self.metrics.rendered.fetch_add(1, Ordering::Relaxed);
        if frame.iter().any(|s| s.abs() > 0.001) {
            self.metrics.audible.fetch_add(1, Ordering::Relaxed);
        }
        if self.echo {
            let mut queue = self.queue.lock().unwrap();
            if queue.len() == ECHO_MAX_FRAMES {
                queue.pop_front();
                self.metrics.echo_overflow.fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(frame.to_vec());
        }
        Ok(())
    }
}

impl AudioBackend for Backend {
    fn capture(&self, _: Option<&str>) -> io::Result<OpenedCapture> {
        if self.samples.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty audio clip",
            ));
        }
        self.queue.lock().unwrap().clear();
        self.metrics.opens.fetch_add(1, Ordering::Relaxed);
        Ok(OpenedCapture::new(Box::new(Input {
            samples: Arc::clone(&self.samples),
            cursor: 0,
            echo: self.echo,
            gain: self.gain,
            metrics: Arc::clone(&self.metrics),
            queue: Arc::clone(&self.queue),
            ticker: Pace::new(),
        })))
    }
    fn render(&self, _: Option<&str>) -> io::Result<Box<dyn Render>> {
        Ok(Box::new(Output {
            echo: self.echo,
            metrics: Arc::clone(&self.metrics),
            queue: Arc::clone(&self.queue),
            ticker: Pace::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clips_loop_and_echo_preserves_a_bounded_delayed_signal() {
        let backend = Backend::new(Arc::new(vec![0.1, 0.2, 0.3]), false, 0.5);
        let mut input = backend.capture(None).unwrap().stream;
        let mut frame = vec![0.0; FRAME_SAMPLES];
        input.read(&mut frame).unwrap();
        assert_eq!(&frame[..4], &[0.05, 0.1, 0.15, 0.05]);
        let backend = Backend::new(tone(), true, 0.5);
        let mut input = backend.capture(None).unwrap().stream;
        let mut output = backend.render(None).unwrap();
        input.read(&mut frame).unwrap();
        assert!(frame.iter().all(|s| *s == 0.0));
        for _ in 0..ECHO_MAX_FRAMES + 1 {
            output.write(&vec![0.2; FRAME_SAMPLES]).unwrap();
        }
        assert_eq!(backend.queue.lock().unwrap().len(), ECHO_MAX_FRAMES);
        assert_eq!(backend.metrics.echo_overflow.load(Ordering::Relaxed), 1);
        input.read(&mut frame).unwrap();
        assert!(frame.iter().all(|s| *s == 0.1));
    }
}
