// SPDX-License-Identifier: GPL-3.0-or-later
//! Offline native-window experiment. CPU is whole-process user + kernel time,
//! not GPU time. No engine, audio, network, or physical input is used.
use crate::{fixture::Fixture, App};
use slint::ComponentHandle;
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

const MODES: [&str; 4] = ["static", "fire_10hz", "meter_20hz", "fire_10hz_meter_20hz"];
const WARMUP: Duration = Duration::from_secs(2);
const SAMPLE: Duration = Duration::from_secs(10);

#[cfg(windows)]
fn cpu_seconds() -> Option<f64> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    // SAFETY: pseudo handle refers to this process, and all four output pointers
    // are valid writable FILETIME values. This never reads another app's state.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let seconds = |time: FILETIME| {
        ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) as f64
            / 10_000_000.0
    };
    (ok != 0).then(|| seconds(kernel) + seconds(user))
}

#[cfg(not(windows))]
fn cpu_seconds() -> Option<f64> {
    None
}

pub fn install(app: &App, fixture: Rc<Fixture>) -> Option<slint::Timer> {
    if !std::env::args().any(|arg| arg == "--measure-render-cost") {
        return None;
    }
    let draws = Rc::new(Cell::new(0_u64));
    let measured_draws = draws.clone();
    // A notifier adds a flush in FemtoVG. Keep CPU isolation uninstrumented;
    // callback counts are a separate, explicitly requested experiment.
    let notifier_requested = std::env::args().any(|arg| arg == "--count-render-callbacks");
    let notifier_supported = notifier_requested
        && app
            .window()
            .set_rendering_notifier(move |state, _| {
                if matches!(state, slint::RenderingState::AfterRendering) {
                    measured_draws.set(measured_draws.get() + 1);
                }
            })
            .is_ok();
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    let mut mode = 0;
    let mut mode_started = Instant::now();
    let mut measurement = None;
    let mut tick = 0_u64;
    let mut fire_updates = 0_u64;
    let mut meter_updates = 0_u64;
    let mut valid = true;
    let mut rows = Vec::new();
    eprintln!("Offline renderer isolation: 4 modes, 2 s warmup + 10 s sample each; synthetic meter, no real call.");
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(app) = weak.upgrade() else { return };
        let now = Instant::now();
        if measurement.is_none() && now.duration_since(mode_started) >= WARMUP {
            measurement = Some((now, cpu_seconds(), draws.get(), fire_updates, meter_updates, app.window().size(), app.window().scale_factor()));
            valid = true;
        }
        if let Some((started, initial_cpu, initial_draws, initial_fire, initial_meter, size, scale)) = measurement {
            valid &= app.window().size() == size && app.window().scale_factor() == scale
                && app.window().is_visible() && !app.window().is_minimized() && !app.get_show_settings();
            let elapsed = now.duration_since(started).as_secs_f64();
            if elapsed >= SAMPLE.as_secs_f64() {
                let cpu = initial_cpu.zip(cpu_seconds()).map(|(start, end)| (end - start) / elapsed * 100.0);
                rows.push(serde_json::json!({"mode":MODES[mode],"elapsed_s":elapsed,
                    "single_core_cpu_percent":cpu,"draw_callbacks":notifier_supported.then(|| draws.get() - initial_draws),
                    "fire_updates":fire_updates-initial_fire,"meter_updates":meter_updates-initial_meter,
                    "physical_width":size.width,"physical_height":size.height,"scale_factor":scale,
                    "window_size_visibility_unchanged":valid}));
                eprintln!("{}: CPU={cpu:?}, elapsed={elapsed:.3}s", MODES[mode]);
                mode += 1;
                if mode == MODES.len() {
                    let report = serde_json::json!({"schema":1,"scope":"offline_native_ui_render_isolation",
                        "cpu_basis":"whole process user + kernel, single-core percent; excludes engine and GPU time",
                        "draw_scope":"AfterRendering callback, before present; not GPU/display completion",
                        "foreground_scope":"not continuously verified; keep this window unobscured during the experiment",
                        "renderer_requested":std::env::var("SLINT_BACKEND").ok(),
                        "notifier_requested":notifier_requested,"notifier_supported":notifier_supported,"warmup_seconds":WARMUP.as_secs(),"samples":rows});
                    let text = serde_json::to_string_pretty(&report).expect("finite experiment data");
                    if let Some(path) = std::env::var_os("GOUHUO_RENDER_COST_OUTPUT") {
                        if let Err(error) = std::fs::write(path, &text) { eprintln!("Could not save experiment: {error}"); }
                    }
                    println!("{text}");
                    let _ = slint::quit_event_loop();
                    return;
                }
                app.set_input_level(0.34);
                mode_started = now;
                measurement = None;
                valid = true;
            }
        }
        tick += 1;
        if (mode == 1 || mode == 3) && tick.is_multiple_of(2) {
            fixture.advance_scene(0.1);
            fire_updates += 1;
        }
        if mode >= 2 {
            app.set_input_level((tick % 20) as f32 / 20.0);
            meter_updates += 1;
        }
    });
    Some(timer)
}
