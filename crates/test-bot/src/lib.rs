// SPDX-License-Identifier: GPL-3.0-or-later
pub mod audio;
pub mod network;

use client_core::{Client, Event};
use client_runtime::health::CallHealth;
use client_runtime::recovery::Action;
use client_runtime::{Devices, RuntimeHandle, StartVoice, VoiceRuntime};
use network::{Counters, Impairment, Proxy};
use serde_json::{json, Value};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use voice_core::identity::Identity;
use voice_core::pipeline::TransmitMode;

#[derive(Clone)]
pub struct Config {
    pub invite: String,
    pub room: Option<String>,
    pub count: usize,
    pub seconds: Duration,
    pub name: String,
    pub samples: Arc<Vec<f32>>,
    pub echo: bool,
    pub silent: bool,
    pub gain: f32,
    pub impairment: Impairment,
}

impl Config {
    pub fn new(invite: String) -> Self {
        Self {
            invite,
            room: None,
            count: 1,
            seconds: Duration::from_secs(60),
            name: "bot".into(),
            samples: audio::tone(),
            echo: false,
            silent: false,
            gain: 1.0,
            impairment: Impairment::default(),
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        let n = self.impairment.netem;
        if !(1..=256).contains(&self.count)
            || self.seconds.is_zero()
            || self.seconds.as_secs_f64() > 86400.0
            || !self.gain.is_finite()
            || !(0.0..=1.0).contains(&self.gain)
            || self.samples.is_empty()
            || !n.loss.is_finite()
            || !(0.0..=1.0).contains(&n.loss)
            || !n.owd_ms.is_finite()
            || !(0.0..=5000.0).contains(&n.owd_ms)
            || !n.jitter_ms.is_finite()
            || !(0.0..=5000.0).contains(&n.jitter_ms)
            || (self.echo && (self.count != 1 || self.silent))
            || self.name.is_empty()
            || self.name.chars().count() > 16
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid bot count, duration, gain, network settings or echo mode",
            ));
        }
        Ok(())
    }
}

fn send(tx: &mpsc::SyncSender<Value>, value: Value) -> io::Result<()> {
    tx.send(value)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "diagnostic output closed"))
}

fn channel(client: &Client, room: &str) -> io::Result<u32> {
    let roster = client.roster();
    if let Ok(id) = room.parse::<u32>() {
        if roster.channels.contains_key(&id) {
            return Ok(id);
        }
    }
    let mut found = roster.channels.values().filter(|c| c.name == room);
    let id = found.next().map(|c| c.id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "room must name an existing channel or ID",
        )
    })?;
    if found.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ambiguous room name; use its numeric channel ID",
        ));
    }
    Ok(id)
}

fn start(
    client: &Client,
    runtime: &RuntimeHandle,
    cfg: &Config,
    epoch: Instant,
    index: usize,
) -> io::Result<Proxy> {
    let (session_id, server, keys) = client.voice_endpoint_session();
    let mut impairment = cfg.impairment;
    impairment.netem.seed = impairment.netem.seed.wrapping_add(index as u64);
    let proxy = Proxy::start(server, impairment, epoch)?;
    runtime.start_voice(StartVoice {
        host: proxy.local.ip().to_string(),
        udp_port: proxy.local.port(),
        session_id,
        sequences: Arc::clone(&keys.sequences),
        upstream_key: *keys.upstream.as_bytes(),
        downstream_key: *keys.downstream.as_bytes(),
        devices: Devices::default(),
    });
    Ok(proxy)
}

fn net(c: &Counters) -> Value {
    json!({ "accepted": c.accepted.load(Ordering::Relaxed), "accepted_bytes": c.accepted_bytes.load(Ordering::Relaxed), "dropped": c.dropped.load(Ordering::Relaxed), "delivered": c.delivered.load(Ordering::Relaxed), "delivered_bytes": c.delivered_bytes.load(Ordering::Relaxed), "overflow": c.overflow.load(Ordering::Relaxed), "send_errors": c.send_errors.load(Ordering::Relaxed) })
}

/// One independent identity and actual client runtime. The caller owns output
/// and cancellation; every return closes TCP, audio and the proxy.
pub fn run_bot(
    cfg: Config,
    index: usize,
    tx: mpsc::SyncSender<Value>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    cfg.validate()?;
    let identity = Identity::generate()?;
    let (client, events) = Client::connect(
        &cfg.invite,
        &identity,
        &format!("{}-{}", cfg.name, index + 1),
    )
    .map_err(|e| io::Error::other(e.headline()))?;
    let backend = Arc::new(audio::Backend::new(
        Arc::clone(&cfg.samples),
        cfg.echo,
        cfg.gain,
    ));
    let metrics = Arc::clone(&backend.metrics);
    let runtime = match VoiceRuntime::new(backend) {
        Ok(r) => r,
        Err(e) => {
            client.disconnect();
            return Err(e);
        }
    };
    let handle = runtime.handle();
    let outcome = (|| {
        if let Some(room) = &cfg.room {
            let id = channel(&client, room)?;
            client.join_channel(id);
            let deadline = Instant::now() + Duration::from_secs(5);
            while client.roster().my_channel() != id {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "channel join was not confirmed",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        handle.set_mode(if cfg.silent {
            TransmitMode::PushToTalk
        } else if cfg.echo {
            TransmitMode::VoiceActivity {
                threshold_db: -55.0,
            }
        } else {
            TransmitMode::Always
        });
        let epoch = Instant::now();
        let mut proxy = Some(start(&client, &handle, &cfg, epoch, index)?);
        let mut health = CallHealth::default();
        let mut reconnecting = false;
        let mut generation = 1;
        let mut next_report = Duration::ZERO;
        send(
            &tx,
            json!({"event":"connected", "bot":index + 1, "session":client.session_id(), "channel":client.roster().my_channel(), "seed":cfg.impairment.netem.seed.wrapping_add(index as u64)}),
        )?;
        loop {
            for event in events.try_iter().take(64) {
                match event {
                    Event::Reconnecting {
                        attempt, reason, ..
                    } => {
                        reconnecting = true;
                        health.interrupt(epoch.elapsed());
                        handle.quiesce();
                        proxy.take();
                        send(
                            &tx,
                            json!({"event":"tcp_reconnecting", "bot":index + 1, "elapsed_ms":epoch.elapsed().as_millis() as u64, "attempt":attempt, "reason":reason}),
                        )?;
                    }
                    Event::Reconnected => {
                        proxy = Some(start(&client, &handle, &cfg, epoch, index)?);
                        generation += 1;
                        reconnecting = false;
                        send(
                            &tx,
                            json!({"event":"tcp_reconnected", "bot":index + 1, "elapsed_ms":epoch.elapsed().as_millis() as u64, "session":client.session_id()}),
                        )?;
                    }
                    Event::Disconnected(ended) => {
                        return Err(io::Error::other(format!("session ended: {ended:?}")))
                    }
                    _ => {}
                }
            }
            let snapshot = handle.snapshot();
            let update = health.tick(epoch.elapsed(), &snapshot, reconnecting);
            if let Some(action) = update.action {
                send(
                    &tx,
                    json!({"event":"recovery", "bot":index + 1, "elapsed_ms":epoch.elapsed().as_millis() as u64, "action":format!("{action:?}"), "stage":format!("{:?}",snapshot.stage), "error":snapshot.error, "voice_error":snapshot.voice.as_ref().and_then(|s| s.error.clone())}),
                )?;
                match action {
                    Action::RetryVoice => {
                        handle.quiesce();
                        proxy.take();
                        proxy = Some(start(&client, &handle, &cfg, epoch, index)?);
                        generation += 1;
                    }
                    Action::Reconnect => client.reconnect_transport(),
                    _ => {}
                }
            }
            let ending = stop.load(Ordering::Relaxed) || epoch.elapsed() >= cfg.seconds;
            if epoch.elapsed() >= next_report || ending {
                let voice = snapshot.voice.as_ref();
                send(
                    &tx,
                    json!({"event":if ending {"finished"} else {"sample"}, "bot":index + 1, "elapsed_ms":epoch.elapsed().as_millis() as u64, "generation":generation, "session":snapshot.session_id, "stage":format!("{:?}",snapshot.stage), "udp_ok":voice.is_some_and(|s|s.udp_ok), "udp_failed":voice.is_some_and(|s|s.udp_failed), "sent":voice.map_or(0,|s|s.packets_sent), "received":voice.map_or(0,|s|s.packets_received), "speaking_sessions":voice.map_or_else(Vec::new,|s|s.speaking.clone()), "underruns":voice.map_or(0,|s|s.underruns), "rtt_ms":voice.map_or(0.0,|s|s.rtt_ms), "audio_opens":metrics.opens.load(Ordering::Relaxed), "rendered_frames":metrics.rendered.load(Ordering::Relaxed), "audible_frames":metrics.audible.load(Ordering::Relaxed), "echo_overflow":metrics.echo_overflow.load(Ordering::Relaxed), "server_udp_received":client.server_udp_received(), "up":proxy.as_ref().map(|p|net(&p.up)), "down":proxy.as_ref().map(|p|net(&p.down)) }),
                )?;
                next_report = epoch.elapsed() + Duration::from_secs(1);
            }
            if ending {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    })();
    client.disconnect();
    handle.shutdown();
    if !runtime.wait_stopped(Duration::from_secs(2)) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "audio shutdown did not finish",
        ));
    }
    outcome
}
