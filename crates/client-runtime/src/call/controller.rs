// SPDX-License-Identifier: GPL-3.0-or-later
//! Portable call commands. Desktop adapters retain files, device choices and UI focus.

use client_core::Client;
use protocol::control::Role;
use voice_types::TransmitMode;

use crate::{self_state::ConnectionState, VoiceControl};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallCommand {
    ToggleMute,
    ToggleDeafen,
    JoinChannel { channel: u32 },
    CreateChannel { name: String, parent: u32 },
    RenameChannel { channel: u32, name: String },
    DeleteChannel { channel: u32 },
    SendText { body: String },
    Kick { session: u32, reason: String },
    Ban { session: u32, reason: String },
    SetRole { session: u32, role: Role },
    Unban { public_key: Vec<u8> },
    SetVolume { session: u32, percent: u32 },
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    SoundClosed,
    MissingMember,
    OwnMember,
    MissingChannel,
    NotPermitted,
    EmptyText,
    InvalidRole,
    InvalidPublicKey,
    NotConnected,
    NotPushToTalk,
}

/// Command acceptance is local. The authenticated server still enforces all
/// permissions and publishes the actual roster/channel/text result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    Applied,
    Ignored(IgnoreReason),
    SelfStateChanged {
        muted: bool,
        deafened: bool,
    },
    VolumeChanged {
        session: u32,
        public_key: Vec<u8>,
        percent: u32,
    },
    Left,
}

/// No duplicated ownership or GUI state: commands use the caller's current
/// connection and runtime. All Client writes enter its bounded FIFO lane.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallController;

impl CallController {
    pub fn dispatch(
        command: CallCommand,
        client: &Client,
        runtime: &dyn VoiceControl,
    ) -> CommandResult {
        use CallCommand::*;
        match command {
            ToggleMute => {
                let intent = runtime.snapshot().intent;
                if intent.deafened {
                    return CommandResult::Ignored(IgnoreReason::SoundClosed);
                }
                runtime.set_muted(!intent.muted);
                Self::publish_self_state(client, runtime)
            }
            ToggleDeafen => {
                let intent = runtime.snapshot().intent;
                runtime.set_deafened(!intent.deafened);
                Self::publish_self_state(client, runtime)
            }
            JoinChannel { channel } => {
                if !client.roster().channels.contains_key(&channel) {
                    return CommandResult::Ignored(IgnoreReason::MissingChannel);
                }
                client.join_channel(channel);
                CommandResult::Applied
            }
            CreateChannel { name, parent } => {
                if name.trim().is_empty() {
                    return CommandResult::Ignored(IgnoreReason::EmptyText);
                }
                {
                    let roster = client.roster();
                    if !roster.channels.contains_key(&parent) {
                        return CommandResult::Ignored(IgnoreReason::MissingChannel);
                    }
                    if !roster.can_create_channel() {
                        return CommandResult::Ignored(IgnoreReason::NotPermitted);
                    }
                }
                client.create_channel(&name, parent);
                CommandResult::Applied
            }
            RenameChannel { channel, name } => {
                if name.trim().is_empty() {
                    return CommandResult::Ignored(IgnoreReason::EmptyText);
                }
                {
                    let roster = client.roster();
                    if !roster.channels.contains_key(&channel) {
                        return CommandResult::Ignored(IgnoreReason::MissingChannel);
                    }
                    if !roster.can_edit_channel(channel) {
                        return CommandResult::Ignored(IgnoreReason::NotPermitted);
                    }
                }
                client.rename_channel(channel, &name);
                CommandResult::Applied
            }
            DeleteChannel { channel } => {
                {
                    let roster = client.roster();
                    if !roster.channels.contains_key(&channel) {
                        return CommandResult::Ignored(IgnoreReason::MissingChannel);
                    }
                    if !roster.can_delete_channel(channel) {
                        return CommandResult::Ignored(IgnoreReason::NotPermitted);
                    }
                }
                client.delete_channel(channel);
                CommandResult::Applied
            }
            SendText { body } => {
                if body.trim().is_empty() {
                    return CommandResult::Ignored(IgnoreReason::EmptyText);
                }
                client.send_text(&body);
                CommandResult::Applied
            }
            Kick { session, reason } => {
                if let Some(reason) = member_action_denied(client, session, |r| r.can_kick(session))
                {
                    return CommandResult::Ignored(reason);
                }
                client.kick(session, &reason);
                CommandResult::Applied
            }
            Ban { session, reason } => {
                if let Some(reason) = member_action_denied(client, session, |r| r.can_ban(session))
                {
                    return CommandResult::Ignored(reason);
                }
                client.ban(session, &reason);
                CommandResult::Applied
            }
            SetRole { session, role } => {
                if role == Role::Unspecified {
                    return CommandResult::Ignored(IgnoreReason::InvalidRole);
                }
                if let Some(reason) =
                    member_action_denied(client, session, |r| r.can_set_role(session))
                {
                    return CommandResult::Ignored(reason);
                }
                client.set_role(session, role);
                CommandResult::Applied
            }
            Unban { public_key } => {
                if public_key.len() != 32 {
                    return CommandResult::Ignored(IgnoreReason::InvalidPublicKey);
                }
                if !client.roster().is_admin() {
                    return CommandResult::Ignored(IgnoreReason::NotPermitted);
                }
                client.unban(&public_key);
                CommandResult::Applied
            }
            SetVolume { session, percent } => {
                let public_key = {
                    let roster = client.roster();
                    if session == roster.me {
                        return CommandResult::Ignored(IgnoreReason::OwnMember);
                    }
                    let Some(user) = roster.users.get(&session) else {
                        return CommandResult::Ignored(IgnoreReason::MissingMember);
                    };
                    if user.public_key.len() != 32 {
                        return CommandResult::Ignored(IgnoreReason::InvalidPublicKey);
                    }
                    user.public_key.clone()
                };
                let percent = percent.min((voice_types::MAX_VOLUME * 100.0) as u32);
                runtime.set_volume(session, percent as f32 / 100.0);
                CommandResult::VolumeChanged {
                    session,
                    public_key,
                    percent,
                }
            }
            Leave => {
                // Stop local transmission first; TCP shutdown also bypasses its TLS lock.
                runtime.stop();
                client.disconnect();
                CommandResult::Left
            }
        }
    }

    fn publish_self_state(client: &Client, runtime: &dyn VoiceControl) -> CommandResult {
        let intent = runtime.snapshot().intent;
        // Moderator mute remains a constraint, not the user's self-muted preference.
        client.set_self_state(intent.muted, intent.deafened);
        CommandResult::SelfStateChanged {
            muted: intent.muted,
            deafened: intent.deafened,
        }
    }

    /// The host supplies portable connection state, never a GUI property. Mode
    /// comes from the runtime. Reconnect/VAD also clear any stale pressed intent.
    pub fn set_ptt(
        runtime: &dyn VoiceControl,
        connection: ConnectionState,
        down: bool,
    ) -> CommandResult {
        let connected = connection == ConnectionState::Connected;
        let ptt_mode = matches!(runtime.mode(), TransmitMode::PushToTalk);
        runtime.set_transmitting(connected && ptt_mode && down);
        if !down {
            CommandResult::Applied
        } else if !connected {
            CommandResult::Ignored(IgnoreReason::NotConnected)
        } else if !ptt_mode {
            CommandResult::Ignored(IgnoreReason::NotPushToTalk)
        } else {
            CommandResult::Applied
        }
    }
}

fn member_action_denied(
    client: &Client,
    session: u32,
    permitted: impl FnOnce(&client_core::roster::Roster) -> bool,
) -> Option<IgnoreReason> {
    let roster = client.roster();
    if !roster.users.contains_key(&session) {
        Some(IgnoreReason::MissingMember)
    } else if session == roster.me {
        Some(IgnoreReason::OwnMember)
    } else if !permitted(&roster) {
        Some(IgnoreReason::NotPermitted)
    } else {
        None
    }
}

#[cfg(all(test, feature = "in-process"))]
mod tests {
    use super::*;
    use std::io;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::{mpsc::Receiver, Arc};
    use std::time::{Duration, Instant};

    use client_core::{Event, Options};
    use protocol::Invite;
    use server::conn::Hub;
    use server::state::{Config, Server};
    use transport::{server_config, ServerCert};
    use voice_core::audio::Render;
    use voice_core::identity::Identity;

    use crate::{AudioBackend, OpenedCapture, VoiceRuntime};

    // Command tests never start a device. Any accidental opening is an error.
    struct NoDevices;
    impl AudioBackend for NoDevices {
        fn capture(&self, _: Option<&str>) -> io::Result<OpenedCapture> {
            Err(io::Error::other("command test must not open capture"))
        }
        fn render(&self, _: Option<&str>) -> io::Result<Box<dyn Render>> {
            Err(io::Error::other("command test must not open render"))
        }
    }

    struct Fixture {
        client: Client,
        events: Receiver<Event>,
        runtime: VoiceRuntime,
        invite: Invite,
    }

    impl Fixture {
        fn new() -> Self {
            let cert = ServerCert::generate().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let invite = Invite {
                host: addr.ip().to_string(),
                port: addr.port(),
                cert: cert.fingerprint(),
                code: None,
            };
            let tls = Arc::new(server_config(&cert).unwrap());
            let hub = Arc::new(Hub::new(
                Server::new(Config::default()),
                UdpSocket::bind("127.0.0.1:0").unwrap(),
            ));
            std::thread::spawn(move || server::accept_loop(listener, tls, hub));
            let (client, events) = Client::connect_with(
                &invite.to_url().unwrap(),
                &Identity::generate().unwrap(),
                "portable-controller",
                Options::default(),
            )
            .unwrap();
            Self {
                client,
                events,
                runtime: VoiceRuntime::new(Arc::new(NoDevices)).unwrap(),
                invite,
            }
        }

        fn dispatch(&self, command: CallCommand) -> CommandResult {
            CallController::dispatch(command, &self.client, &self.runtime.handle())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.client.disconnect();
            self.runtime.handle().shutdown();
            let _ = self.runtime.wait_stopped(Duration::from_secs(2));
        }
    }

    fn wait(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "portable command was not acknowledged"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn mute_deafen_commands_preserve_local_intent_and_publish_final_self_state() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime.handle();
        runtime.set_server_muted(true);
        assert_eq!(
            fixture.dispatch(CallCommand::ToggleDeafen),
            CommandResult::SelfStateChanged {
                muted: true,
                deafened: true
            }
        );
        wait(|| {
            fixture
                .client
                .roster()
                .my_user()
                .is_some_and(|user| user.self_muted && user.self_deafened)
        });
        assert_eq!(
            fixture.dispatch(CallCommand::ToggleMute),
            CommandResult::Ignored(IgnoreReason::SoundClosed)
        );
        assert_eq!(
            fixture.dispatch(CallCommand::ToggleDeafen),
            CommandResult::SelfStateChanged {
                muted: true,
                deafened: false
            }
        );
        assert_eq!(
            fixture.dispatch(CallCommand::ToggleMute),
            CommandResult::SelfStateChanged {
                muted: false,
                deafened: false
            }
        );
        let intent = runtime.snapshot().intent;
        assert!(
            intent.server_muted,
            "local toggles must not erase a moderator constraint"
        );
        assert!(!intent.muted && !intent.deafened);
        wait(|| {
            fixture
                .client
                .roster()
                .my_user()
                .is_some_and(|user| !user.self_muted && !user.self_deafened)
        });
        assert!(!runtime
            .snapshot()
            .voice
            .is_some_and(|stats| stats.transmitting));
    }

    #[test]
    fn ptt_reads_portable_connection_and_runtime_mode_and_clears_stale_press() {
        let runtime = VoiceRuntime::new(Arc::new(NoDevices)).unwrap();
        let handle = runtime.handle();
        handle.set_mode(TransmitMode::PushToTalk);
        assert_eq!(
            CallController::set_ptt(&handle, ConnectionState::Connected, true),
            CommandResult::Applied
        );
        assert!(handle.snapshot().intent.transmitting);
        assert_eq!(
            CallController::set_ptt(&handle, ConnectionState::Reconnecting, true),
            CommandResult::Ignored(IgnoreReason::NotConnected)
        );
        assert!(!handle.snapshot().intent.transmitting);
        handle.set_mode(TransmitMode::VoiceActivity {
            threshold_db: -45.0,
        });
        handle.set_transmitting(true);
        assert_eq!(
            CallController::set_ptt(&handle, ConnectionState::Connected, true),
            CommandResult::Ignored(IgnoreReason::NotPushToTalk)
        );
        assert!(!handle.snapshot().intent.transmitting);
        assert_eq!(
            CallController::set_ptt(&handle, ConnectionState::Offline, false),
            CommandResult::Applied
        );
        handle.shutdown();
        assert!(runtime.wait_stopped(Duration::from_secs(2)));
    }

    #[test]
    fn volume_returns_current_member_identity_and_never_creates_a_missing_target() {
        let fixture = Fixture::new();
        let identity = Identity::generate().unwrap();
        let expected_key = identity.public_key().0.to_vec();
        let (other, _events) =
            Client::connect(&fixture.invite.to_url().unwrap(), &identity, "other").unwrap();
        let session = other.session_id();
        wait(|| fixture.client.roster().users.contains_key(&session));
        assert_eq!(
            fixture.dispatch(CallCommand::SetVolume {
                session,
                percent: 10_000
            }),
            CommandResult::VolumeChanged {
                session,
                public_key: expected_key,
                percent: 400
            }
        );
        assert_eq!(
            fixture.dispatch(CallCommand::SetVolume {
                session: fixture.client.session_id(),
                percent: 0
            }),
            CommandResult::Ignored(IgnoreReason::OwnMember)
        );
        assert_eq!(
            fixture.dispatch(CallCommand::Kick {
                session,
                reason: String::new()
            }),
            CommandResult::Ignored(IgnoreReason::NotPermitted)
        );
        other.disconnect();
        wait(|| !fixture.client.roster().users.contains_key(&session));
        assert_eq!(
            fixture.dispatch(CallCommand::SetVolume {
                session,
                percent: 0
            }),
            CommandResult::Ignored(IgnoreReason::MissingMember)
        );
    }

    #[test]
    fn channel_text_and_leave_commands_work_without_a_gui_or_audio_device() {
        let fixture = Fixture::new();
        let parent = fixture.client.roster().root().unwrap();
        assert_eq!(
            fixture.dispatch(CallCommand::CreateChannel {
                name: "portable-room".into(),
                parent
            }),
            CommandResult::Applied
        );
        wait(|| {
            fixture
                .client
                .roster()
                .channels
                .values()
                .any(|channel| channel.name == "portable-room")
        });
        let channel = fixture
            .client
            .roster()
            .channels
            .values()
            .find(|channel| channel.name == "portable-room")
            .unwrap()
            .id;
        assert_eq!(
            fixture.dispatch(CallCommand::JoinChannel { channel }),
            CommandResult::Applied
        );
        wait(|| fixture.client.roster().my_channel() == channel);
        assert_eq!(
            fixture.dispatch(CallCommand::RenameChannel {
                channel,
                name: "renamed-room".into()
            }),
            CommandResult::Applied
        );
        wait(|| {
            fixture
                .client
                .roster()
                .channels
                .get(&channel)
                .is_some_and(|channel| channel.name == "renamed-room")
        });
        assert_eq!(
            fixture.dispatch(CallCommand::SendText {
                body: "  portable text  ".into()
            }),
            CommandResult::Applied
        );
        wait(|| {
            fixture
                .client
                .roster()
                .chat
                .back()
                .is_some_and(|line| line.body == "portable text")
        });
        assert_eq!(
            fixture.dispatch(CallCommand::DeleteChannel { channel }),
            CommandResult::Applied
        );
        wait(|| !fixture.client.roster().channels.contains_key(&channel));
        assert_eq!(fixture.dispatch(CallCommand::Leave), CommandResult::Left);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let event = fixture
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if let Event::Disconnected(client_core::Ended::ByUser) = event {
                break;
            }
        }
        assert!(!fixture
            .runtime
            .handle()
            .snapshot()
            .voice
            .is_some_and(|stats| stats.transmitting));
    }
}
