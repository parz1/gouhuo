// SPDX-License-Identifier: GPL-3.0-or-later

//! Deterministic actual Slint software rendering + pointer/keyboard fixture checks.
//! cargo run -p client --example call_ui_preview --offline
use std::{rc::Rc, time::Duration};

use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, WindowAdapter, WindowEvent,
    },
    ComponentHandle, Model,
};

slint::include_modules!();
#[allow(dead_code)]
#[path = "../src/campfire/mod.rs"]
mod campfire;
#[path = "../src/call/fixture.rs"]
mod fixture;

struct PreviewPlatform {
    window: Rc<MinimalSoftwareWindow>,
}
impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        Duration::from_secs(1)
    }
}

fn render(window: &MinimalSoftwareWindow, w: u32, h: u32, scale: f32, path: &std::path::Path) {
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    window.set_size(slint::LogicalSize::new(w as f32, h as f32));
    let physical = window.size();
    // First draw resolves layout callbacks and prepares the official scene fixture.
    for iteration in 0..3 {
        let mut pixels =
            slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(physical.width, physical.height);
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(pixels.make_mut_slice(), physical.width as usize);
        });
        if iteration < 2 {
            continue;
        }
        let mut png = resvg::tiny_skia::Pixmap::new(physical.width, physical.height).unwrap();
        for (src, dst) in pixels
            .as_slice()
            .iter()
            .zip(png.data_mut().chunks_exact_mut(4))
        {
            dst.copy_from_slice(&[src.r, src.g, src.b, 255]);
        }
        png.save_png(path).unwrap();
    }
}

fn click(app: &App, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = slint::platform::PointerEventButton::Left;
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    app.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    app.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}
fn key(app: &App, text: impl Into<slint::SharedString>) {
    let text = text.into();
    app.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    app.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn warm_focus_pixels(path: &std::path::Path, bounds: (u32, u32, u32, u32)) -> usize {
    let pixmap = resvg::tiny_skia::Pixmap::load_png(path).unwrap();
    let (left, top, right, bottom) = bounds;
    let mut count = 0;
    for y in top..bottom {
        for x in left..right {
            let offset = ((y * pixmap.width() + x) * 4) as usize;
            let rgba = &pixmap.data()[offset..offset + 4];
            if rgba[0] > 215 && (150..190).contains(&rgba[1]) && (65..115).contains(&rgba[2]) {
                count += 1;
            }
        }
    }
    count
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform {
        window: window.clone(),
    }))?;
    let app = App::new()?;
    let fixture = fixture::Fixture::install(&app);
    app.show()?;
    let folder = std::path::Path::new("docs/design/rebuild");
    std::fs::create_dir_all(folder)?;
    let quick = std::env::args().any(|arg| arg == "--quick");
    if !quick {
        for (w, h) in [
            (400, 360),
            (400, 520),
            (639, 360),
            (640, 360),
            (641, 360),
            (735, 520),
            (736, 520),
            (737, 520),
            (760, 520),
            (1024, 720),
        ] {
            for scale in [1.0, 1.25, 1.5, 2.0] {
                fixture.apply("long-nickname-key");
                render(
                    &window,
                    w,
                    h,
                    scale,
                    &folder.join(format!(
                        "geometry-{w}x{h}-{}pct.png",
                        (scale * 100.0) as u32
                    )),
                );
            }
        }
    }
    let cases = if quick {
        &fixture::CASES[..2]
    } else {
        fixture::CASES
    };
    for case in cases {
        for (w, h) in [(760, 520), (400, 360)] {
            fixture.apply(case);
            assert!(
                !app.get_self_status().is_empty(),
                "each fixture must supply its view-model status"
            );
            let recovery_expected = matches!(
                *case,
                "reconnecting" | "capture-failed" | "render-failed" | "udp-failed"
            );
            assert_eq!(
                !app.get_recovery_message().is_empty(),
                recovery_expected,
                "fixture recovery projection for {case}"
            );
            if *case == "render-failed" {
                assert!(
                    app.get_transmitting(),
                    "output failure still projects actual sending"
                );
            }
            if matches!(
                *case,
                "capture-failed" | "udp-failed" | "reconnecting" | "deafened" | "muted"
            ) {
                assert!(
                    !app.get_transmitting(),
                    "the fixture model must supply its effective sending fact"
                );
            }
            render(
                &window,
                w,
                h,
                1.0,
                &folder.join(format!("{case}-{w}x{h}.png")),
            );
        }
    }
    fixture.apply("vad-waiting");
    render(&window, 400, 360, 1.0, &folder.join("pointer-keyboard.png"));
    click(&app, 70.0, 331.0);
    assert!(
        app.get_self_muted(),
        "minimum dock mute button must be reachable"
    );
    key(&app, " ");
    assert!(
        !app.get_self_muted(),
        "Space must activate the focused mute button"
    );
    click(&app, 235.0, 331.0);
    assert!(
        app.get_self_deafened() && app.get_self_muted(),
        "closing sound also mutes"
    );
    click(&app, 70.0, 331.0);
    assert!(
        app.get_self_muted(),
        "disabled open mic cannot bypass deafening"
    );
    click(&app, 235.0, 331.0);
    assert!(
        !app.get_self_deafened() && app.get_self_muted(),
        "opening sound retains mute intent"
    );
    click(&app, 40.0, 56.0);
    assert!(app.get_tree_open(), "channel navigation is reachable");
    click(&app, 70.0, 331.0);
    assert!(
        !app.get_self_muted(),
        "dock remains reachable under navigation"
    );
    key(&app, slint::platform::Key::Escape);
    assert!(
        !app.get_tree_open(),
        "Escape closes navigation without leaving"
    );
    key(&app, " ");
    assert!(
        app.get_tree_open(),
        "closing navigation returns focus to its trigger"
    );
    key(&app, slint::platform::Key::Escape);
    fixture.open_member(4);
    render(&window, 400, 360, 1.0, &folder.join("pointer-member.png"));
    key(&app, slint::platform::Key::Escape);
    assert_eq!(
        app.get_member_id(),
        -1,
        "Escape closes the host member menu"
    );
    // Use the actual compact member button, rather than the F4 fixture shortcut.
    // Rendering between Escape and Space destroys the old menu before testing
    // that focus returned to the same member's existing delegate.
    for scale in [1.0, 2.0] {
        fixture.apply("reconnecting");
        let path = folder.join(format!(
            "focus-compact-recovery-{}pct.png",
            (scale * 100.0) as u32
        ));
        render(&window, 400, 360, scale, &path);
        let member = app
            .get_seats()
            .iter()
            .find(|seat| seat.present && !seat.is_me)
            .unwrap();
        click(&app, 200.0, 213.0);
        assert_eq!(
            app.get_member_id(),
            member.id,
            "the visible compact member button must open its own member"
        );
        render(&window, 400, 360, scale, &path);
        for opening in ["pointer", "keyboard"] {
            key(&app, slint::platform::Key::Escape);
            render(&window, 400, 360, scale, &path);
            assert_eq!(app.get_member_id(), -1);
            key(&app, " ");
            assert_eq!(
                app.get_member_id(),
                member.id,
                "Escape after a {opening} opening must return focus to the same compact member ({scale}x DPI)"
            );
            render(&window, 400, 360, scale, &path);
        }
        key(&app, slint::platform::Key::Escape);
        render(&window, 400, 360, scale, &path);
    }
    // A recovery banner leaves only ~95 logical pixels of ordinary content.
    // The host must borrow the header area so these actual controls can be
    // reached without scrolling, while the fixed dock still closes the menu.
    for case in [
        "reconnecting",
        "capture-failed",
        "render-failed",
        "udp-failed",
    ] {
        for scale in [1.0, 2.0] {
            fixture.apply(case);
            render(
                &window,
                400,
                360,
                scale,
                &folder.join("recovery-menu-warmup.png"),
            );
            fixture.open_member(4);
            let path = folder.join(format!(
                "recovery-member-controls-{case}-{}pct.png",
                (scale * 100.0) as u32
            ));
            render(&window, 400, 360, scale, &path);
            assert_eq!(app.get_member_id(), 4);
            assert_eq!(app.get_member_data().volume, 100);
            click(&app, 300.0, 120.0);
            let pointer_volume = app.get_member_data().volume;
            assert!(
                pointer_volume > 150,
                "the fully visible recovery-menu slider must accept a pointer click ({case}, {scale})"
            );
            key(&app, slint::platform::Key::RightArrow);
            assert!(
                app.get_member_data().volume > pointer_volume,
                "the recovery-menu slider must retain keyboard focus"
            );
            click(&app, 200.0, 170.0);
            assert_eq!(
                app.get_member_data().volume,
                0,
                "personal mute must be fully reachable without scrolling"
            );
            render(&window, 400, 360, scale, &path);
            let recovery = app.get_recovery_message();
            click(&app, 70.0, 331.0);
            assert!(
                app.get_self_muted(),
                "SelfDock must remain reachable below the expanded host"
            );
            assert_eq!(
                app.get_member_id(),
                -1,
                "using SelfDock must close the expanded member menu"
            );
            assert_eq!(
                app.get_recovery_message(),
                recovery,
                "closing the menu must preserve recovery presentation"
            );
        }
    }
    // Check the actual focus border and keyboard reactivation after both close paths.
    // A plain clickable callback would not prove that the original dock entry has focus.
    for width in [400, 760] {
        fixture.apply("vad-waiting");
        render(
            &window,
            width,
            520,
            1.0,
            &folder.join("focus-layout-warmup.png"),
        );
        // Use the documented minimum height for the two-line dock fixture.
        let height = if width == 400 { 360 } else { 520 };
        render(
            &window,
            width,
            height,
            1.0,
            &folder.join("focus-layout-warmup.png"),
        );
        let (mode_x, dock_y, close_x, mode_close_y, identity_close_y, mode_bounds, identity_bounds) =
            if width == 400 {
                (
                    310.0,
                    276.0,
                    334.0,
                    113.0,
                    113.0,
                    (246, 262, 390, 290),
                    (48, 264, 110, 270),
                )
            } else {
                (
                    370.0,
                    480.0,
                    619.0,
                    160.0,
                    175.0,
                    (280, 466, 474, 494),
                    (48, 468, 120, 474),
                )
            };
        click(&app, mode_x, dock_y);
        key(&app, slint::platform::Key::Escape);
        let path = folder.join(format!("focus-mode-escape-{width}.png"));
        render(&window, width, height, 1.0, &path);
        assert!(
            warm_focus_pixels(&path, mode_bounds) > 40,
            "Escape must restore the talk-mode focus border"
        );
        key(&app, " ");
        click(&app, close_x, mode_close_y);
        let path = folder.join(format!("focus-mode-close-{width}.png"));
        render(&window, width, height, 1.0, &path);
        assert!(
            warm_focus_pixels(&path, mode_bounds) > 40,
            "the close button must restore the talk-mode entry"
        );
        key(&app, " ");
        key(&app, slint::platform::Key::Tab);
        key(&app, slint::platform::Key::Tab);
        key(&app, " ");
        assert!(
            app.get_ptt_mode(),
            "restored mode focus must reopen the menu and select PTT with the keyboard"
        );
        click(&app, 60.0, dock_y);
        key(&app, slint::platform::Key::Escape);
        let path = folder.join(format!("focus-identity-escape-{width}.png"));
        render(&window, width, height, 1.0, &path);
        assert!(
            warm_focus_pixels(&path, identity_bounds) > 40,
            "Escape must restore the identity focus border"
        );
        key(&app, " ");
        click(&app, close_x, identity_close_y);
        let path = folder.join(format!("focus-identity-close-{width}.png"));
        render(&window, width, height, 1.0, &path);
        assert!(
            warm_focus_pixels(&path, identity_bounds) > 40,
            "the close button must restore the identity entry"
        );
    }
    fixture.write_timings(&folder.join(if quick {
        "offline-scene-timings-quick.csv"
    } else {
        "offline-scene-timings.csv"
    }))?;
    println!(
        "{} Slint software snapshots at logical breakpoints/DPI, and pointer/keyboard checks passed. No audio/network/settings used.",
        if quick { 4 } else { 80 }
    );
    Ok(())
}
