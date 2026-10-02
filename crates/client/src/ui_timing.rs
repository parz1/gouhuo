// SPDX-License-Identifier: GPL-3.0-or-later
//! Small local phase ledger, available in diagnostics without recording content.
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

static PHASES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static TRACE_ENABLED: OnceLock<bool> = OnceLock::new();

fn trace_enabled() -> bool {
    *TRACE_ENABLED.get_or_init(|| std::env::var_os("GOUHUO_TRACE_UI").is_some())
}

/// Fixed, content-free names for the next actual renderer callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum FramePhase {
    Startup,
    CallEnter,
    Reconnecting,
    Reconnected,
    SceneResize,
}

const FRAME_PHASES: [&str; 5] = [
    "startup request -> first draw complete (before present)",
    "call enter request -> first draw complete (before present)",
    "reconnecting request -> first draw complete (before present)",
    "reconnected request -> first draw complete (before present)",
    "scene resize request -> next draw complete (before present)",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameProbeStatus {
    Disabled,
    Supported,
    Unsupported,
    AlreadySet,
}

#[derive(Default)]
struct FrameProbeState {
    installation: Option<FrameProbeStatus>,
    pending: [Option<Instant>; FRAME_PHASES.len()],
    rendering_started: Option<Instant>,
}

thread_local! {
    static FRAME_PROBE: RefCell<FrameProbeState> = RefCell::new(FrameProbeState::default());
}

/// Mark a UI transition before changing its properties. Repeated requests for
/// one phase before the next frame keep the earliest request, matching the
/// coalesced resize path. This does not force rendering or enter an event loop.
///
/// With tracing disabled this is just a cached boolean check. No timer, thread,
/// allocation, rendering callback, or disk write is added to normal operation.
pub fn mark_frame(phase: FramePhase) {
    if !trace_enabled() {
        return;
    }
    FRAME_PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        if matches!(
            probe.installation,
            Some(FrameProbeStatus::Unsupported | FrameProbeStatus::AlreadySet)
        ) {
            return;
        }
        probe.pending[phase as usize].get_or_insert_with(Instant::now);
    });
}

/// Install once after creating the window, before `show`/`run`. Slint 1.18.1's
/// femtovg renderer reports `AfterRendering` after submitting drawing commands,
/// before presenting the surface. This is not GPU completion, display latency,
/// or a frame interval. Its software renderer returns `Unsupported`; it must
/// never be represented as a successful frame measurement.
pub fn install_frame_probe(window: &slint::Window) -> FrameProbeStatus {
    if !trace_enabled() {
        return FrameProbeStatus::Disabled;
    }
    if let Some(status) = FRAME_PROBE.with(|probe| probe.borrow().installation) {
        return status;
    }
    let result = window.set_rendering_notifier(|state, _graphics_api| match state {
        slint::RenderingState::BeforeRendering => {
            FRAME_PROBE.with(|probe| {
                let mut probe = probe.borrow_mut();
                // Only tagged frames need a wall-clock span; ordinary animated
                // frames do not call Instant::now or add phase-ledger entries.
                if probe.pending.iter().any(Option::is_some) {
                    probe.rendering_started = Some(Instant::now());
                }
            });
        }
        slint::RenderingState::AfterRendering => {
            let measured = FRAME_PROBE.with(|probe| {
                let mut probe = probe.borrow_mut();
                if probe.pending.iter().all(Option::is_none) {
                    return None;
                }
                let now = Instant::now();
                let phases = probe
                    .pending
                    .map(|started| started.map(|at| now.duration_since(at)));
                probe.pending.fill(None);
                let draw_span = probe
                    .rendering_started
                    .take()
                    .map(|at| now.duration_since(at));
                Some((phases, draw_span))
            });
            if let Some((phases, draw_span)) = measured {
                for (name, elapsed) in FRAME_PHASES.into_iter().zip(phases) {
                    if let Some(elapsed) = elapsed {
                        record(name, elapsed);
                    }
                }
                if let Some(draw_span) = draw_span {
                    record(
                        "tagged draw callback wall span (BeforeRendering -> AfterRendering)",
                        draw_span,
                    );
                }
            }
        }
        slint::RenderingState::RenderingTeardown => {
            FRAME_PROBE.with(|probe| probe.borrow_mut().rendering_started = None);
        }
        _ => {}
    });
    let status = match result {
        Ok(()) => {
            eprintln!("[ui] rendering notifier supported: AfterRendering is before present/display; callback wall span excludes GPU completion and is not CPU time or a frame interval");
            FrameProbeStatus::Supported
        }
        Err(slint::SetRenderingNotifierError::Unsupported) => {
            eprintln!("[ui] rendering notifier unsupported: actual frame completion timing unavailable on this renderer");
            FrameProbeStatus::Unsupported
        }
        Err(slint::SetRenderingNotifierError::AlreadySet) => {
            eprintln!("[ui] rendering notifier already set: frame probe unavailable");
            FrameProbeStatus::AlreadySet
        }
        Err(_) => {
            eprintln!(
                "[ui] rendering notifier unavailable: actual frame completion timing unavailable"
            );
            FrameProbeStatus::Unsupported
        }
    };
    FRAME_PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        probe.installation = Some(status);
        if status != FrameProbeStatus::Supported {
            probe.pending.fill(None);
        }
    });
    status
}

pub fn record(phase: &'static str, elapsed: Duration) {
    let line = format!("{phase}: {:.2} ms", elapsed.as_secs_f64() * 1000.0);
    if trace_enabled() {
        eprintln!("[ui] {line}");
    }
    let mut phases = PHASES.lock().expect("phase ledger poisoned");
    if phases.len() == 16 {
        phases.pop_front();
    }
    phases.push_back(line);
}

pub fn measure<T>(phase: &'static str, work: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = work();
    record(phase, start.elapsed());
    result
}

pub fn snapshot() -> Vec<String> {
    PHASES
        .lock()
        .expect("phase ledger poisoned")
        .iter()
        .cloned()
        .collect()
}
