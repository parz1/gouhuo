// SPDX-License-Identifier: MPL-2.0

//! Audio fixtures that retain the actual clock tick for every frame.
//! Sample indices alone are not a clock when a shared runner misses a tick.

use std::fmt::Write;
use std::io;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use voice_core::audio::{Capture, Render, FRAME_MS, SAMPLE_RATE};
use voice_core::clock::Ticker;

pub type Trace = Arc<Mutex<FrameTrace>>;

#[derive(Clone, Debug)]
pub struct FrameStamp {
    pub sample_start: usize,
    pub samples: usize,
    pub at: Instant,
    pub late_ticks: u64,
    pub skipped_ticks: u64,
}

#[derive(Clone, Debug, Default)]
pub struct FrameTrace {
    pub samples: Vec<f32>,
    pub frames: Vec<FrameStamp>,
}

impl FrameTrace {
    fn record(&mut self, samples: &[f32], at: Instant, ticker: &Ticker) {
        self.frames.push(FrameStamp {
            sample_start: self.samples.len(),
            samples: samples.len(),
            at,
            late_ticks: ticker.late_ticks,
            skipped_ticks: ticker.skipped_ticks,
        });
        self.samples.extend_from_slice(samples);
    }

    pub fn sample_time(&self, sample: usize) -> Option<Instant> {
        let frame = self.frames.get(
            self.frames
                .partition_point(|f| f.sample_start <= sample)
                .checked_sub(1)?,
        )?;
        let offset = sample.checked_sub(frame.sample_start)?;
        if offset >= frame.samples {
            return None;
        }
        Some(frame.at + Duration::from_secs_f64(offset as f64 / SAMPLE_RATE as f64))
    }

    /// Candidate starts are bounded by physical time, rather than assuming
    /// both independently scheduled device clocks delivered the same number
    /// of frames since their first tick. The signal window remains unchanged.
    pub fn candidate_window(
        &self,
        input_at: Instant,
        max_delay: Duration,
        window_samples: usize,
    ) -> Option<RangeInclusive<usize>> {
        let last_start = self.samples.len().checked_sub(window_samples)?;
        let end = input_at + max_delay;
        let mut first = None;
        let mut last = None;
        for frame in &self.frames {
            for offset in 0..frame.samples {
                let sample = frame.sample_start + offset;
                if sample > last_start {
                    break;
                }
                let at = frame.at + Duration::from_secs_f64(offset as f64 / SAMPLE_RATE as f64);
                if (input_at..=end).contains(&at) {
                    first.get_or_insert(sample);
                    last = Some(sample);
                }
            }
        }
        Some(first?..=last?)
    }

    pub fn timing_report(&self, origin: Instant) -> String {
        let mut report = format!(
            "{} frames / {} samples; ",
            self.frames.len(),
            self.samples.len()
        );
        if let Some(last) = self.frames.last() {
            let _ = write!(
                report,
                "late_ticks={} skipped_ticks={}; ",
                last.late_ticks, last.skipped_ticks
            );
        }
        let mut previous = None;
        for frame in &self.frames {
            let gap = previous
                .map(|at| frame.at.duration_since(at).as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
            let _ = write!(
                report,
                "{}@{:.3}ms(+{gap:.3};late={},skip={}) ",
                frame.sample_start,
                frame.at.duration_since(origin).as_secs_f64() * 1000.0,
                frame.late_ticks,
                frame.skipped_ticks,
            );
            previous = Some(frame.at);
        }
        report
    }
}

pub struct MeasuredCapture {
    source: Vec<f32>,
    cursor: usize,
    ticker: Ticker,
    trace: Trace,
}

impl MeasuredCapture {
    pub fn new(source: Vec<f32>) -> (Self, Trace) {
        let trace = Trace::default();
        let capture = Self {
            source,
            cursor: 0,
            ticker: Ticker::start(Duration::from_millis(FRAME_MS as u64)).0,
            trace: Arc::clone(&trace),
        };
        (capture, trace)
    }
}

impl Capture for MeasuredCapture {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        let at = self.ticker.tick();
        for (offset, sample) in out.iter_mut().enumerate() {
            *sample = self
                .source
                .get(self.cursor + offset)
                .copied()
                .unwrap_or(0.0);
        }
        self.cursor += out.len();
        self.trace.lock().unwrap().record(out, at, &self.ticker);
        Ok(true)
    }
}

pub struct MeasuredRender {
    ticker: Ticker,
    trace: Trace,
}

impl MeasuredRender {
    pub fn new() -> (Self, Trace) {
        let trace = Trace::default();
        let render = Self {
            ticker: Ticker::start(Duration::from_millis(FRAME_MS as u64)).0,
            trace: Arc::clone(&trace),
        };
        (render, trace)
    }
}

impl Render for MeasuredRender {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        let at = self.ticker.tick();
        self.trace.lock().unwrap().record(frame, at, &self.ticker);
        Ok(())
    }
}
