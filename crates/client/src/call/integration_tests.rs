// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercise the real portable projection and Slint adapter without a device,
//! socket, settings file, native window, or the offline preview's fake state.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    pin::Pin,
    rc::Rc,
    time::Duration,
};

use client_core::{ChatLine, Roster};
use client_runtime::{
    call::{CallState, CallViewModel},
    self_state::SelfStatus,
    CaptureInfo, MicSnapshot, RuntimeSnapshot, RuntimeStage, VoiceIntent,
};
use protocol::control::{Channel, Role, User};
use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, PointerEventButton, WindowAdapter, WindowEvent,
    },
    // The pinned Slint version exposes its notification listener through the
    // generated-code API. This test observes the delegate update protocol;
    // production code only uses the public Model API.
    private_unstable_api::re_exports::{ModelChangeListener, ModelChangeListenerContainer},
    ComponentHandle,
    Model,
    ModelRc,
    VecModel,
};
use voice_types::{TransmitMode, VoiceStats};

use super::SlintAdapter;
use crate::{App, Row, Seat, SeatGeometry};

struct TestPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }

    fn duration_since_start(&self) -> Duration {
        Duration::from_secs(1)
    }
}

#[derive(Default)]
struct ModelEvents {
    changed: RefCell<Vec<usize>>,
    structural: Cell<usize>,
}

impl ModelEvents {
    fn clear(&self) {
        self.changed.borrow_mut().clear();
        self.structural.set(0);
    }
}

impl ModelChangeListener for ModelEvents {
    fn row_changed(self: Pin<&Self>, row: usize) {
        self.changed.borrow_mut().push(row);
    }

    fn row_added(self: Pin<&Self>, _: usize, _: usize) {
        self.structural.set(self.structural.get() + 1);
    }

    fn row_removed(self: Pin<&Self>, _: usize, _: usize) {
        self.structural.set(self.structural.get() + 1);
    }

    fn reset(self: Pin<&Self>) {
        self.structural.set(self.structural.get() + 1);
    }
}

fn observe<T: Clone + 'static>(
    model: &ModelRc<T>,
) -> Pin<Box<ModelChangeListenerContainer<ModelEvents>>> {
    let observer = Box::pin(ModelChangeListenerContainer::<ModelEvents>::default());
    model
        .model_tracker()
        .attach_peer(observer.as_ref().model_peer());
    observer
}

fn roster() -> Roster {
    let mut roster = Roster {
        me: 7,
        ..Default::default()
    };
    // Same display name, with both IDs also used by member sessions.
    for (id, parent_id) in [(7, 7), (8, 7)] {
        roster.channels.insert(
            id,
            Channel {
                id,
                parent_id,
                name: "篝火".into(),
                ..Default::default()
            },
        );
    }
    for (session, channel, name, role) in [
        (7, 7, "A", Role::Admin),
        (8, 7, "同名", Role::Member),
        (9, 7, "同名", Role::Member),
        (10, 8, "别的频道", Role::Member),
    ] {
        roster.users.insert(
            session,
            User {
                session_id: session,
                channel_id: channel,
                public_key: vec![session as u8; 32],
                name: name.into(),
                role: role as i32,
                ..Default::default()
            },
        );
    }
    roster.push_chat(chat_line("第一条", 1_000));
    roster
}

fn chat_line(body: &str, timestamp_ms: i64) -> ChatLine {
    ChatLine {
        sender_session: 8,
        sender_name: "同名".into(),
        body: body.into(),
        timestamp_ms,
    }
}

fn live_snapshot() -> RuntimeSnapshot {
    RuntimeSnapshot {
        stage: RuntimeStage::Ready,
        session_id: Some(7),
        voice: Some(VoiceStats {
            transmitting: true,
            input_available: true,
            input_db: -18.0,
            render_available: true,
            udp_ok: true,
            speaking: vec![8, 9],
            ..Default::default()
        }),
        // Intent alone is not proof that an audio packet was sent.
        intent: VoiceIntent {
            transmitting: false,
            ..Default::default()
        },
        capture: CaptureInfo {
            opened: true,
            name: "Test microphone".into(),
            is_virtual: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn project(
    state: &CallState,
    snapshot: &RuntimeSnapshot,
    roster: &Roster,
    volumes: &BTreeMap<String, u32>,
) -> CallViewModel {
    CallViewModel::project(state, snapshot, true, Some(roster), volumes)
}

fn apply(app: &App, view: &CallViewModel, image: &slint::Image) {
    SlintAdapter::apply_call(app, view);
    // Keep physical seat slots fixed, with a third member in the overflow list.
    // All business fields are produced by the production scene adapter.
    let members = SlintAdapter::scene_members(view);
    let mut seats = vec![Seat::default(); crate::campfire::SEATS];
    let mut waiting = Vec::new();
    for member in members {
        match member.id {
            7 => seats[0] = member,
            8 => seats[1] = member,
            _ => waiting.push(member),
        }
    }
    // Stable occupied image handles mirror the scene cache. Empty slots keep
    // their default image so this also covers the adapter's empty-slot fast path.
    for seat in seats
        .iter_mut()
        .chain(waiting.iter_mut())
        .filter(|seat| seat.present)
    {
        seat.stone = image.clone();
        seat.stone_talking = image.clone();
    }
    SlintAdapter::apply_scene(app, seats, waiting);
}

fn row(app: &App, channel: bool, id: i32) -> Row {
    app.get_rows()
        .iter()
        .find(|row| row.is_channel == channel && row.id == id)
        .expect("the typed channel/session row must exist")
}

fn settle(window: &MinimalSoftwareWindow) {
    // Real repeaters and changed handlers are evaluated entirely in memory.
    for _ in 0..3 {
        slint::platform::update_timers_and_animations();
        let size = window.size();
        let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(size.width, size.height);
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(pixels.make_mut_slice(), size.width as usize);
        });
    }
}

fn key(app: &App, text: impl Into<slint::SharedString>) {
    let text = text.into();
    app.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    app.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn click(app: &App, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    app.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    app.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}

#[test]
fn portable_call_state_reaches_actual_slint_models_and_preserves_delegates() {
    // Slint's platform is thread-local. Keep all transitions in one test and
    // provide a software window that never creates an operating-system window.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let app = App::new().unwrap();
    let weak = app.as_weak();
    app.on_campfire_resized(move |width, height| {
        let geometry: Vec<_> = crate::campfire::seat_geometry((width, height))
            .into_iter()
            .map(|(x, y, size, up)| SeatGeometry { x, y, size, up })
            .collect();
        weak.unwrap()
            .set_scene_geometry(ModelRc::new(VecModel::from(geometry)));
    });
    let opened = Rc::new(Cell::new(0));
    let counter = opened.clone();
    let weak = app.as_weak();
    app.on_open_member(move |id| {
        let app = weak.unwrap();
        app.set_member_data(row(&app, false, id));
        app.set_member_id(id);
        counter.set(counter.get() + 1);
    });

    let mut state = CallState::default();
    state.connected();
    let mut snapshot = live_snapshot();
    let mut roster = roster();
    let mut volumes = BTreeMap::new();
    let image = slint::Image::from_rgba8(slint::SharedPixelBuffer::new(1, 1));
    // Deliberately disagree with the GUI defaults: the adapter must overwrite them.
    app.set_ptt_mode(false);
    apply(&app, &project(&state, &snapshot, &roster, &volumes), &image);
    assert!(app.get_connected() && app.get_transmitting());
    assert_eq!(app.get_self_status(), SelfStatus::Sending.label());
    assert!(app.get_input_available() && app.get_render_available());
    assert!((app.get_input_level() - 0.7).abs() < f32::EPSILON);
    assert!(app.get_ptt_mode() && app.get_udp_ok());
    assert_eq!(app.get_capture_in_use(), "Test microphone");
    assert!(app.get_capture_is_virtual());
    assert_eq!(app.get_channel_count(), 3);
    assert_eq!(row(&app, true, 7).count, 3);
    assert_eq!(row(&app, true, 8).count, 1);
    assert!(row(&app, true, 7).is_current && !row(&app, true, 8).is_current);
    assert!(row(&app, false, 7).is_me && row(&app, false, 7).speaking);
    assert!(row(&app, false, 8).speaking && row(&app, false, 9).speaking);
    assert_eq!(row(&app, false, 8).name, row(&app, false, 9).name);
    assert!(app.get_seats().row_data(0).unwrap().speaking);
    assert!(app.get_seats().row_data(1).unwrap().speaking);
    assert!(app.get_waiting_talking());

    // Only the in-memory software window is marked visible so actual delegates
    // are instantiated; no native backend or event loop is entered.
    app.show().unwrap();
    window.set_size(slint::LogicalSize::new(760.0, 520.0));
    settle(&window);
    let rows = app.get_rows();
    let seats = app.get_seats();
    let waiting = app.get_waiting();
    let row_events = observe(&rows);
    let seat_events = observe(&seats);
    let waiting_events = observe(&waiting);

    // A local mute must survive a late, unmuted roster echo and stale live TX.
    snapshot.intent.muted = true;
    assert!(!roster.my_user().unwrap().self_muted);
    apply(&app, &project(&state, &snapshot, &roster, &volumes), &image);
    assert!(app.get_self_muted() && app.get_input_available());
    assert!(!app.get_transmitting());
    assert_eq!(app.get_self_status(), SelfStatus::Muted.label());
    assert!(row(&app, false, 7).muted && !row(&app, false, 7).speaking);
    let me = seats.row_data(0).unwrap();
    assert!(me.muted && !me.speaking && !me.deafened);
    assert!(seats.row_data(1).unwrap().speaking);
    assert_eq!(rows, app.get_rows());
    assert_eq!(seats, app.get_seats());

    // Give the real remote StoneSeat delegate focus, then close its menu.
    settle(&window);
    let geometry = app.get_scene_geometry().row_data(1).unwrap();
    click(&app, 209.0 + geometry.x, 57.0 + geometry.y);
    settle(&window);
    assert_eq!(
        app.get_member_id(),
        8,
        "session 8 must not select channel 8"
    );
    key(&app, slint::platform::Key::Escape);
    settle(&window);
    assert_eq!(app.get_member_id(), -1);
    let opens_before_volume = opened.get();
    row_events.clear();
    seat_events.clear();
    waiting_events.clear();

    volumes.insert(protocol::base32::encode(&roster.users[&8].public_key), 234);
    volumes.insert(protocol::base32::encode(&roster.users[&9].public_key), 0);
    apply(&app, &project(&state, &snapshot, &roster, &volumes), &image);
    assert_eq!(rows, app.get_rows());
    assert_eq!(seats, app.get_seats());
    assert_eq!(waiting, app.get_waiting());
    assert_eq!(row(&app, false, 8).volume, 234);
    assert_eq!(seats.row_data(1).unwrap().volume, 234);
    assert_eq!(waiting.row_data(0).unwrap().volume, 0);
    assert_eq!(seat_events.changed.borrow().as_slice(), &[1]);
    assert_eq!(waiting_events.changed.borrow().as_slice(), &[0]);
    assert_eq!(row_events.changed.borrow().len(), 2);
    for events in [&row_events, &seat_events, &waiting_events] {
        assert_eq!(
            events.structural.get(),
            0,
            "volume must not invalidate delegates"
        );
    }
    settle(&window);
    key(&app, " ");
    settle(&window);
    assert_eq!(
        opened.get(),
        opens_before_volume + 1,
        "focused seat delegate must survive volume updates"
    );
    assert_eq!(app.get_member_id(), 8);
    assert_eq!(app.get_member_data().volume, 234);
    assert_eq!(app.get_member_data().id, 8);
    assert!(!app.get_member_data().is_channel);

    // The frequent audio path shares the same rules and refreshes an open menu.
    let stats = snapshot.voice.as_mut().unwrap();
    stats.transmitting = false;
    stats.speaking = vec![9];
    let audio = CallViewModel::project_audio(&state, &snapshot, true);
    SlintAdapter::apply_audio(&app, &audio);
    SlintAdapter::apply_activity(&app, &audio);
    assert!(!row(&app, false, 8).speaking && !seats.row_data(1).unwrap().speaking);
    assert!(!app.get_member_data().speaking);
    assert!(waiting.row_data(0).unwrap().speaking && app.get_waiting_talking());
    key(&app, slint::platform::Key::Escape);

    // A stable chat ModelRc still needs count changes to mark open chat as read.
    let chat = app.get_chat();
    // At 760px, the header has 12px right padding and a 172px action group.
    let chat_x = 760.0 - 12.0 - 172.0 + 30.0;
    click(&app, chat_x, 28.0);
    settle(&window);
    assert!(app.get_chat_open());
    assert_eq!(app.get_chat_seen(), 1);
    roster.push_chat(chat_line("第二条", 2_000));
    apply(&app, &project(&state, &snapshot, &roster, &volumes), &image);
    settle(&window);
    assert_eq!(chat, app.get_chat());
    assert_eq!(app.get_chat_seen(), 2);
    click(&app, chat_x, 28.0);
    assert!(!app.get_chat_open());
    roster.push_chat(chat_line("第三条", 3_000));
    apply(&app, &project(&state, &snapshot, &roster, &volumes), &image);
    settle(&window);
    assert_eq!(chat, app.get_chat());
    assert_eq!(app.get_chat().row_count(), 3);
    assert_eq!(
        app.get_chat_seen(),
        2,
        "closed chat must retain its unread message"
    );

    // Read counts, draft text and overlays cannot cross an explicit call.
    // The first message on a new server must be unread even after reading
    // several messages on the previous server.
    app.set_draft("previous server draft".into());
    app.set_tree_open(true);
    app.set_diagnostics_open(true);
    app.invoke_reset_call_view();
    assert_eq!(app.get_chat_seen(), 0);
    assert!(app.get_draft().is_empty());
    assert!(!app.get_chat_open() && !app.get_tree_open() && !app.get_diagnostics_open());
    assert_eq!(app.get_member_id(), -1);
    let mut next_roster = roster.clone();
    next_roster.chat.clear();
    apply(
        &app,
        &project(&state, &snapshot, &next_roster, &volumes),
        &image,
    );
    settle(&window);
    next_roster.push_chat(chat_line("new server first message", 4_000));
    apply(
        &app,
        &project(&state, &snapshot, &next_roster, &volumes),
        &image,
    );
    settle(&window);
    assert_eq!(app.get_chat_seen(), 0);
    assert_eq!(app.get_chat().row_count(), 1);

    // Reconnect deliberately receives the previous session's stale snapshot.
    // Both full refresh and the poll path must extinguish every activity marker.
    let stats = snapshot.voice.as_mut().unwrap();
    stats.transmitting = true;
    stats.speaking = vec![8, 9];
    stats.udp_failed = true;
    snapshot.error = Some("旧链路错误".into());
    state.set_notice("较低优先级的提示");
    state.reconnecting(3, "控制连接中断");
    let recovering = project(&state, &snapshot, &roster, &volumes);
    apply(&app, &recovering, &image);
    SlintAdapter::apply_activity(&app, &recovering.audio);
    assert!(app.get_connected() && !app.get_connecting());
    assert!(!app.get_transmitting() && !app.get_input_available());
    assert_eq!(app.get_input_level(), 0.0);
    assert!(!app.get_udp_ok() && !app.get_udp_failed());
    assert_eq!(app.get_self_status(), SelfStatus::Reconnecting.label());
    assert_eq!(app.get_recovery_message(), recovering.audio.reconnecting);
    assert!(app.get_recovery_message().contains("第 3 次"));
    assert!(app.get_recovery_message().ends_with("控制连接中断"));
    assert!(app.get_rows().iter().all(|row| !row.speaking));
    assert!(app.get_seats().iter().all(|seat| !seat.speaking));
    assert!(app.get_waiting().iter().all(|seat| !seat.speaking));
    assert!(!app.get_waiting_talking());

    // Offline microphone readings remain useful, but cannot indicate sending.
    // Mode and monitoring are snapshot facts, regardless of stale GUI values.
    state.offline();
    snapshot = RuntimeSnapshot {
        stage: RuntimeStage::Ready,
        mic: Some(MicSnapshot {
            input_available: true,
            input_db: -30.0,
            error: None,
        }),
        intent: VoiceIntent {
            monitoring: true,
            transmitting: true,
            mode: TransmitMode::VoiceActivity {
                threshold_db: -35.0,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    SlintAdapter::apply_audio(
        &app,
        &CallViewModel::project_audio(&state, &snapshot, false),
    );
    assert!(!app.get_connected() && !app.get_transmitting());
    assert!(app.get_input_available() && app.get_monitoring());
    assert_eq!(app.get_input_level(), 0.5);
    assert!(!app.get_ptt_mode());
    assert_eq!(app.get_self_status(), SelfStatus::Offline.label());
    assert!(app.get_recovery_message().is_empty());
    snapshot.intent.mode = TransmitMode::PushToTalk;
    SlintAdapter::apply_audio(
        &app,
        &CallViewModel::project_audio(&state, &snapshot, false),
    );
    assert!(
        app.get_ptt_mode(),
        "PTT mode must come from the snapshot while offline too"
    );
    assert_eq!(app.get_input_level(), 0.5);
    app.hide().unwrap();
}
