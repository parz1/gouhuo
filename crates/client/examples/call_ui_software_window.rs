// SPDX-License-Identifier: GPL-3.0-or-later

//! Native offline CallPage preview with the winit software fallback. F1 normal, F2 minimum, F3 next state, F4 member menu.
use slint::ComponentHandle;
use std::{cell::Cell, rc::Rc};

slint::include_modules!();
#[allow(dead_code)]
#[path = "../src/campfire/mod.rs"]
mod campfire;
#[allow(dead_code)]
#[path = "../src/call/fixture.rs"]
mod fixture;

fn main() -> Result<(), slint::PlatformError> {
    // This example selects its backend before creating any Slint component.
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let app = App::new()?;
    let fixture = fixture::Fixture::install(&app);
    app.window().set_size(slint::LogicalSize::new(760.0, 520.0));
    let state = Rc::new(Cell::new(0usize));
    let weak = app.as_weak();
    let preview = fixture.clone();
    app.on_preview_key(move |key| {
        if let Some(app) = weak.upgrade() {
            match key {
                1 => app.window().set_size(slint::LogicalSize::new(760.0, 520.0)),
                2 => app.window().set_size(slint::LogicalSize::new(400.0, 360.0)),
                3 => {
                    state.set((state.get() + 1) % fixture::CASES.len());
                    preview.apply(fixture::CASES[state.get()]);
                }
                4 => preview.open_member(4),
                _ => {}
            }
        }
    });
    app.run()
}
