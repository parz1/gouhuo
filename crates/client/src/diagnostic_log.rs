// SPDX-License-Identifier: GPL-3.0-or-later
//! Automatic in-memory flight recorder. No names, addresses, text, keys or audio.
use client_runtime::{self_state::ConnectionState, RuntimeSnapshot};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

const MAX_RECORDS: usize = 1800;
const MAX_AGE: Duration = Duration::from_secs(300);

pub struct Recorder {
    started: Instant,
    started_unix_ms: u128,
    enabled: bool,
    sampled: Option<Instant>,
    entries: VecDeque<(Duration, String)>,
    signature: String,
    capture_fingerprint: Option<u64>,
    device_generation: u64,
    engine: String,
}
impl Default for Recorder {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            started_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            enabled: true,
            sampled: None,
            entries: VecDeque::new(),
            signature: String::new(),
            capture_fingerprint: None,
            device_generation: 0,
            engine: String::new(),
        }
    }
}
impl Recorder {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if enabled {
            self.sampled = None;
            self.signature.clear();
        }
    }
    pub fn set_engine(&mut self, version: &str, process_id: Option<u32>, requires_auth: bool) {
        let version: String = version
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .take(64)
            .collect();
        self.engine = format!(
            "engine={version} ipc={} pid={process_id:?} requires_auth={requires_auth}",
            client_runtime::ipc::VERSION
        );
    }
    pub fn sample(&mut self, connection: ConnectionState, snapshot: &RuntimeSnapshot) {
        self.sample_at(Instant::now(), connection, snapshot);
    }
    fn sample_at(&mut self, now: Instant, connection: ConnectionState, snapshot: &RuntimeSnapshot) {
        if !self.enabled {
            return;
        }
        let elapsed = now.saturating_duration_since(self.started);
        while self
            .entries
            .front()
            .is_some_and(|(at, _)| elapsed.saturating_sub(*at) > MAX_AGE)
        {
            self.entries.pop_front();
        }
        if !snapshot.capture.name.is_empty() {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            snapshot.capture.name.hash(&mut hasher);
            let fingerprint = hasher.finish();
            if self.capture_fingerprint != Some(fingerprint) {
                self.capture_fingerprint = Some(fingerprint);
                self.device_generation += 1;
            }
        }
        let mut signature = format!("connection={connection:?} stage={:?} request={} session={:?} preparation_error={} muted={} deafened={} mode={:?} capture_opened={} capture_virtual={}",
            snapshot.stage, snapshot.request_id, snapshot.session_id, snapshot.error.is_some(), snapshot.intent.muted,
            snapshot.intent.deafened, snapshot.intent.mode, snapshot.capture.opened, snapshot.capture.is_virtual);
        if let Some(s) = &snapshot.voice {
            signature.push_str(&format!(" udp_ok={} udp_failed={} tcp_active={} tcp_ok={} input={} render={} capture_error={} render_error={} transport_error={} sequences_exhausted={}",
                s.udp_ok, s.udp_failed, s.tcp_active, s.tcp_ok, s.input_available, s.render_available,
                s.capture_error.is_some(), s.render_error.is_some(), s.transport_error.is_some(), s.sequences_exhausted));
        }
        signature.push_str(&format!(" {}", self.engine));
        let progress = &snapshot.audio_progress;
        signature.push_str(&format!(
            " ptt_pressed={} server_muted={}",
            snapshot.intent.transmitting, snapshot.intent.server_muted
        ));
        signature.push_str(&format!(
            " processor={:?} capture_stalled={} render_stalled={} device_generation={}",
            snapshot.processor,
            progress.capture_stalled,
            progress.render_stalled,
            self.device_generation
        ));
        if let Some(stats) = &snapshot.voice {
            for speaker in &stats.receive_quality {
                signature.push_str(&format!(
                    " speaker={} quality={:?}",
                    speaker.session, speaker.quality.level
                ));
            }
        }
        let changed = self.signature != signature;
        if !changed
            && self
                .sampled
                .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        self.signature = signature;
        self.sampled = Some(now);
        let mut row = format!(
            "t_ms={} changed={} {}",
            elapsed.as_millis(),
            changed,
            self.signature
        );
        row.push_str(&format!(
            " capture_frames={} render_frames={} capture_age_ms={} render_age_ms={}",
            progress.capture_frames,
            progress.render_frames,
            progress.capture_age_ms,
            progress.render_age_ms
        ));
        if let Some(s) = &snapshot.voice {
            row.push_str(&format!(" udp_rtt_ms={:.1} sent={} received={} underruns={} tunnel_dropped={} transmitting={} silent_ratio={:.3}",
                s.rtt_ms, s.packets_sent, s.packets_received, s.underruns, s.tunnel_dropped, s.transmitting, snapshot.capture.silent_ratio));
            for speaker in &s.receive_quality {
                let q = &speaker.quality;
                row.push_str(&format!(" [speaker={} received={} played={} missing={} missing_pct={:.2} late={} reordered={} plc={} plc_pct={:.2} waits={} overflow={} stale_dropped={} burst={} jitter_peak_ms={:.1} buffered_ms={:.1} target_ms={:.1}]",
                    speaker.session, q.received, q.played, q.missing, q.missing_percent, q.late, q.reordered, q.concealed, q.plc_percent, q.waits, q.overflow, q.stale_dropped, q.max_loss_burst, q.jitter_ms, q.buffered_ms, q.target_ms));
            }
        }
        if self.entries.len() == MAX_RECORDS {
            self.entries.pop_front();
        }
        self.entries.push_back((elapsed, row));
    }
    pub fn report(&self) -> String {
        format!("gouhuo={} protocol={} os={} started_unix_ms={}\nIn-memory metadata only; last 5 minutes, at most 1800 records; status changes plus one sample per second. Receive estimates cover 10 seconds; missing is playout loss, not sender-confirmed network loss; jitter is peak transit variation; buffer is local queue span.\n{}\n",
            env!("CARGO_PKG_VERSION"), protocol::control::PROTOCOL_VERSION, std::env::consts::OS, self.started_unix_ms,
            self.entries.iter().map(|(_, row)| row.as_str()).collect::<Vec<_>>().join("\n"))
    }
}

pub fn export(report: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| std::io::Error::other("LOCALAPPDATA unavailable"))?;
    let directory = std::path::PathBuf::from(base)
        .join("gouhuo")
        .join("diagnostics");
    std::fs::create_dir_all(&directory)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = directory.join(format!("diagnostics-{timestamp}.txt"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    // Hash the executable on this background export worker, not the UI/audio threads.
    // This distinguishes locally modified builds with the same version string.
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let fingerprint = (|| -> std::io::Result<String> {
        let mut executable = std::fs::File::open(std::env::current_exe()?)?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = executable.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            digest.update(&buffer[..n]);
        }
        Ok(format!("{:x}", digest.finalize()))
    })()
    .unwrap_or_else(|_| "unavailable".into());
    writeln!(file, "executable_sha256={fingerprint}")?;
    file.write_all(report.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_and_apm_transitions_are_recorded_without_per_frame_flooding() {
        let mut recorder = Recorder::default();
        recorder.set_enabled(true);
        let mut snapshot = RuntimeSnapshot::default();
        recorder.sample(ConnectionState::Connected, &snapshot);
        snapshot.audio_progress.capture_frames = 1;
        recorder.sample(ConnectionState::Connected, &snapshot);
        assert_eq!(recorder.entries.len(), 1);
        snapshot.audio_progress.capture_stalled = true;
        snapshot.audio_progress.capture_age_ms = 2000;
        recorder.sample(ConnectionState::Connected, &snapshot);
        snapshot.processor = client_runtime::ProcessorStatus::Failed;
        recorder.sample(ConnectionState::Connected, &snapshot);
        assert_eq!(recorder.entries.len(), 3);
        let report = recorder.report();
        assert!(report.contains("capture_stalled=true"));
        assert!(report.contains("capture_age_ms=2000"));
        assert!(report.contains("processor=Failed"));
    }
    #[test]
    fn records_transitions_immediately_and_keeps_a_fixed_history() {
        let mut recorder = Recorder::default();
        recorder.set_enabled(true);
        let mut snapshot = RuntimeSnapshot::default();
        for id in 0..MAX_RECORDS + 5 {
            snapshot.request_id = id as u64;
            recorder.sample(ConnectionState::Connected, &snapshot);
        }
        assert_eq!(recorder.entries.len(), MAX_RECORDS);
        assert!(recorder.entries.front().unwrap().1.contains("request=5 "));
        recorder.sample(ConnectionState::Connected, &snapshot);
        assert_eq!(recorder.entries.len(), MAX_RECORDS);
    }
    #[test]
    fn automatic_recording_and_export_never_include_raw_errors_or_device_names() {
        let mut recorder = Recorder::default();
        let mut snapshot = RuntimeSnapshot {
            error: Some("secret invite and IP".into()),
            ..Default::default()
        };
        snapshot.capture.name = "private device name".into();
        recorder.sample(ConnectionState::Connected, &snapshot);
        assert_eq!(recorder.entries.len(), 1);
        assert!(recorder.report().contains("preparation_error=true"));
        assert!(!recorder.report().contains("secret"));
        assert!(!recorder.report().contains("private device"));
        recorder.set_enabled(false);
        recorder.sample(ConnectionState::Offline, &snapshot);
        assert_eq!(recorder.entries.len(), 1);
    }

    #[test]
    fn time_window_and_pause_resume_preserve_recent_evidence() {
        let mut recorder = Recorder::default();
        let start = recorder.started;
        let mut snapshot = RuntimeSnapshot::default();
        for second in 0..310 {
            recorder.sample_at(
                start + Duration::from_secs(second),
                ConnectionState::Connected,
                &snapshot,
            );
        }
        assert!(recorder.entries.len() <= 301);
        assert!(recorder.entries.front().unwrap().0 >= Duration::from_secs(9));
        let count = recorder.entries.len();
        recorder.set_enabled(false);
        snapshot.request_id = 7;
        recorder.sample_at(
            start + Duration::from_secs(310),
            ConnectionState::Offline,
            &snapshot,
        );
        assert_eq!(recorder.entries.len(), count);
        recorder.set_enabled(true);
        recorder.sample_at(
            start + Duration::from_secs(311),
            ConnectionState::Connected,
            &snapshot,
        );
        assert!(
            recorder.entries.len() > 1,
            "resume must not clear the incident"
        );
        assert!(recorder.entries.back().unwrap().1.contains("t_ms=311000"));
    }

    #[test]
    fn quality_changes_and_recovery_remain_ordered_without_manual_recording() {
        use client_runtime::RuntimeStage;
        use voice_types::{QualityLevel, ReceiveQuality, SpeakerQuality, VoiceStats};
        let mut recorder = Recorder::default();
        let start = recorder.started;
        let mut snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Ready,
            voice: Some(VoiceStats {
                udp_ok: true,
                receive_quality: vec![SpeakerQuality {
                    session: 8,
                    quality: ReceiveQuality {
                        level: QualityLevel::Good,
                        ..Default::default()
                    },
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        recorder.sample_at(start, ConnectionState::Connected, &snapshot);
        snapshot.voice.as_mut().unwrap().receive_quality[0]
            .quality
            .level = QualityLevel::Poor;
        recorder.sample_at(
            start + Duration::from_millis(50),
            ConnectionState::Connected,
            &snapshot,
        );
        snapshot.stage = RuntimeStage::Failed;
        snapshot.audio_progress.capture_stalled = true;
        recorder.sample_at(
            start + Duration::from_millis(100),
            ConnectionState::Connected,
            &snapshot,
        );
        snapshot.stage = RuntimeStage::Preparing;
        snapshot.request_id = 2;
        recorder.sample_at(
            start + Duration::from_millis(150),
            ConnectionState::Connected,
            &snapshot,
        );
        snapshot.stage = RuntimeStage::Ready;
        snapshot.audio_progress.capture_stalled = false;
        recorder.sample_at(
            start + Duration::from_millis(200),
            ConnectionState::Connected,
            &snapshot,
        );
        assert_eq!(
            recorder.entries.len(),
            5,
            "transitions must not wait for the one-second sample"
        );
        let report = recorder.report();
        assert!(report.find("quality=Good").unwrap() < report.find("quality=Poor").unwrap());
        assert!(report.find("stage=Failed").unwrap() < report.find("stage=Preparing").unwrap());
        assert!(report.contains("t_ms=200 changed=true"));
    }
}
