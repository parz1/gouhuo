// SPDX-License-Identifier: GPL-3.0-or-later

//! Default low-frequency lifecycle journal. Callers never perform disk I/O.
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::connection::ConnectionCause;
use serde::{Deserialize, Serialize};

pub const MAX_RECORDS: usize = 512;
pub const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_FILE_BYTES: u64 = 1_000_000;
const QUEUE_LIMIT: usize = 128;
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    AwaitingTrust,
    AwaitingCode,
    Connected,
    ReconnectWaiting,
    Restored,
    Ended,
}

/// No free-text fields: records cannot carry invitations, identities or addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema: u32,
    pub run: [u8; 16],
    pub journey: [u8; 16],
    pub utc_unix_ms: u64,
    pub monotonic_ms: u64,
    pub previous: Option<Phase>,
    pub phase: Phase,
    pub generation: u64,
    pub attempt: u32,
    pub session: Option<u32>,
    pub retry_in_ms: Option<u64>,
    pub cause: Option<ConnectionCause>,
    pub client_version: [u32; 3],
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub records: VecDeque<Record>,
    pub loading: bool,
    pub dropped_queue_records: u64,
    pub rejected_commands: u64,
    pub disk_failures: u64,
    pub invalid_disk_documents: u64,
    pub clear_failed: bool,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    dropped: AtomicU64,
    rejected: AtomicU64,
}

enum Command {
    Record(Record),
    Clear,
    Barrier(mpsc::SyncSender<()>),
}

#[derive(Clone)]
pub struct Handle {
    sender: mpsc::SyncSender<Command>,
    shared: Arc<Shared>,
    run: [u8; 16],
    epoch: Instant,
}

pub struct Writer {
    handle: Handle,
    finished: mpsc::Receiver<()>,
}

impl Writer {
    /// Directory creation and reading happen on the worker, never on the caller.
    pub fn start(directory: PathBuf) -> io::Result<Self> {
        let mut run = [0; 16];
        getrandom::fill(&mut run).map_err(|e| io::Error::other(e.to_string()))?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        let (done_tx, finished) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(Snapshot {
                loading: true,
                ..Snapshot::default()
            }),
            dropped: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        });
        let handle = Handle {
            sender,
            shared: Arc::clone(&shared),
            run,
            epoch: Instant::now(),
        };
        std::thread::Builder::new()
            .name("connection-history".into())
            .spawn(move || {
                worker(directory, receiver, shared);
                let _ = done_tx.send(());
            })?;
        Ok(Self { handle, finished })
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// At application shutdown only; bounded wait after releasing every handle.
    pub fn finish(self, timeout: Duration) -> bool {
        let Self { handle, finished } = self;
        drop(handle);
        finished.recv_timeout(timeout).is_ok()
    }
}

impl Handle {
    pub fn new_journey(&self) -> io::Result<Journey> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Journey { id, previous: None })
    }

    fn enqueue(&self, command: Command) -> bool {
        match self.sender.try_send(command) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(command) | mpsc::TrySendError::Disconnected(command)) => {
                if matches!(command, Command::Record(_)) {
                    self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.shared.rejected.fetch_add(1, Ordering::Relaxed);
                }
                false
            }
        }
    }

    /// Accepted means queued, not saved. Snapshot exposes asynchronous failures.
    pub fn clear(&self) -> bool {
        self.enqueue(Command::Clear)
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self
            .shared
            .snapshot
            .lock()
            .expect("history poisoned")
            .clone();
        snapshot.dropped_queue_records = self.shared.dropped.load(Ordering::Relaxed);
        snapshot.rejected_commands = self.shared.rejected.load(Ordering::Relaxed);
        retain(&mut snapshot.records, utc_ms());
        snapshot
    }

    /// Explicit diagnostic/test flush. Never call from an audio or UI callback.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        self.enqueue(Command::Barrier(tx)) && rx.recv_timeout(timeout).is_ok()
    }
}

pub struct Journey {
    id: [u8; 16],
    previous: Option<Phase>,
}

pub struct Transition {
    pub phase: Phase,
    pub generation: u64,
    pub attempt: u32,
    pub session: Option<u32>,
    pub retry_in: Option<Duration>,
    pub cause: Option<ConnectionCause>,
}

impl Journey {
    pub fn record(&mut self, handle: &Handle, transition: Transition) -> bool {
        let Transition {
            phase,
            generation,
            attempt,
            session,
            retry_in,
            cause,
        } = transition;
        let record = Record {
            schema: SCHEMA,
            run: handle.run,
            journey: self.id,
            utc_unix_ms: utc_ms(),
            monotonic_ms: handle.epoch.elapsed().as_millis().min(u64::MAX as u128) as u64,
            previous: self.previous,
            phase,
            generation,
            attempt,
            session,
            retry_in_ms: retry_in.map(|d| d.as_millis().min(u64::MAX as u128) as u64),
            cause,
            client_version: version(),
            protocol_version: protocol::control::PROTOCOL_VERSION,
        };
        self.previous = Some(phase);
        handle.enqueue(Command::Record(record))
    }
}

fn version() -> [u32; 3] {
    [
        env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
    ]
}

fn utc_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn retain(records: &mut VecDeque<Record>, now: u64) {
    records.retain(|record| now.saturating_sub(record.utc_unix_ms) <= RETENTION_MS);
    while records.len() > MAX_RECORDS {
        records.pop_front();
    }
}

fn paths(directory: &Path) -> (PathBuf, PathBuf) {
    (
        directory.join("connection-history.json"),
        directory.join("connection-history.previous.json"),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u32,
    sequence: u64,
    records: VecDeque<Record>,
}

fn load(path: &Path, snapshot: &mut Snapshot) -> io::Result<Option<Document>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    let document = if bytes.len() as u64 > MAX_FILE_BYTES {
        None
    } else {
        serde_json::from_slice::<Document>(&bytes)
            .ok()
            .filter(|document| {
                document.schema == SCHEMA
                    && document.records.len() <= MAX_RECORDS
                    && document
                        .records
                        .iter()
                        .all(|record| record.schema == SCHEMA)
            })
    };
    if document.is_none() {
        snapshot.invalid_disk_documents += 1;
    }
    Ok(document)
}

fn write_document(path: &Path, document: &Document) -> io::Result<()> {
    let bytes = serde_json::to_vec(document).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other("history document exceeds bound"));
    }
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    crate::update::atomic_replace(&temporary, path)
}

fn persist(
    directory: &Path,
    prior: &VecDeque<Record>,
    current: &VecDeque<Record>,
    sequence: u64,
) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let (active, previous) = paths(directory);
    write_document(
        &previous,
        &Document {
            schema: SCHEMA,
            sequence: sequence.saturating_sub(1),
            records: prior.clone(),
        },
    )?;
    write_document(
        &active,
        &Document {
            schema: SCHEMA,
            sequence,
            records: current.clone(),
        },
    )
}

fn worker(directory: PathBuf, receiver: mpsc::Receiver<Command>, shared: Arc<Shared>) {
    let mut snapshot = Snapshot::default();
    let mut loaded: Option<Document> = None;
    let (active, previous) = paths(&directory);
    for path in [previous, active] {
        match load(&path, &mut snapshot) {
            Ok(Some(document))
                if loaded
                    .as_ref()
                    .map_or(true, |old| document.sequence >= old.sequence) =>
            {
                loaded = Some(document)
            }
            Ok(_) => (),
            Err(_) => snapshot.disk_failures += 1,
        }
    }
    let mut sequence = 0;
    if let Some(document) = loaded {
        sequence = document.sequence;
        snapshot.records = document.records;
    }
    retain(&mut snapshot.records, utc_ms());
    sequence = sequence.saturating_add(1);
    // Rewrite retained records at startup, including the recovery generation.
    if persist(&directory, &snapshot.records, &snapshot.records, sequence).is_err() {
        snapshot.disk_failures += 1;
    }
    *shared.snapshot.lock().expect("history poisoned") = snapshot.clone();
    loop {
        let command = match receiver.recv_timeout(Duration::from_secs(60)) {
            Ok(command) => Some(command),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut prior = snapshot.records.clone();
        retain(&mut prior, utc_ms());
        match command {
            Some(Command::Record(record)) => {
                snapshot.records.push_back(record);
                retain(&mut snapshot.records, utc_ms());
                sequence = sequence.saturating_add(1);
                if persist(&directory, &prior, &snapshot.records, sequence).is_err() {
                    snapshot.disk_failures += 1;
                }
            }
            Some(Command::Clear) => {
                snapshot.records.clear();
                sequence = sequence.saturating_add(1);
                // Both generations must be empty to prevent clear/restart resurrection.
                snapshot.clear_failed =
                    persist(&directory, &snapshot.records, &snapshot.records, sequence).is_err();
                if snapshot.clear_failed {
                    snapshot.disk_failures += 1;
                }
            }
            Some(Command::Barrier(tx)) => {
                let _ = tx.send(());
                continue;
            }
            None => {
                if prior.len() == snapshot.records.len() {
                    continue;
                }
                snapshot.records = prior.clone();
                sequence = sequence.saturating_add(1);
                if persist(&directory, &prior, &snapshot.records, sequence).is_err() {
                    snapshot.disk_failures += 1;
                }
            }
        }
        *shared.snapshot.lock().expect("history poisoned") = snapshot.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::connection::{ConnectionReason, EvidenceSource};

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut id = [0; 16];
            getrandom::fill(&mut id).unwrap();
            let name: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
            Self(std::env::temp_dir().join(format!("gouhuo-history-{name}")))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn transition(phase: Phase, generation: u64) -> Transition {
        Transition {
            phase,
            generation,
            attempt: 0,
            session: Some(7),
            retry_in: None,
            cause: None,
        }
    }
    fn record(utc_unix_ms: u64, monotonic_ms: u64) -> Record {
        Record {
            schema: SCHEMA,
            run: [1; 16],
            journey: [2; 16],
            utc_unix_ms,
            monotonic_ms,
            previous: None,
            phase: Phase::Connected,
            generation: 1,
            attempt: 0,
            session: Some(7),
            retry_in_ms: None,
            cause: None,
            client_version: [0, 3, 3],
            protocol_version: 1,
        }
    }

    #[test]
    fn lifecycle_survives_restart_and_clear_empties_both_generations() {
        let directory = Directory::new();
        let writer = Writer::start(directory.0.clone()).unwrap();
        let handle = writer.handle();
        let mut journey = handle.new_journey().unwrap();
        assert!(journey.record(&handle, transition(Phase::Preparing, 0)));
        assert!(journey.record(&handle, transition(Phase::Connected, 1)));
        let mut waiting = transition(Phase::ReconnectWaiting, 1);
        waiting.attempt = 1;
        waiting.retry_in = Some(Duration::from_secs(2));
        waiting.cause = Some(ConnectionCause::local(ConnectionReason::HeartbeatTimeout));
        assert!(journey.record(&handle, waiting));
        assert!(journey.record(&handle, transition(Phase::Restored, 2)));
        assert!(handle.flush(Duration::from_secs(5)));
        let before = handle.snapshot();
        assert_eq!(before.records.len(), 4);
        assert_eq!(before.records[2].previous, Some(Phase::Connected));
        assert_eq!(before.records[2].retry_in_ms, Some(2000));
        assert_eq!(before.records[3].generation, 2);
        assert_eq!(before.disk_failures, 0);
        drop(handle);
        assert!(writer.finish(Duration::from_secs(5)));
        let writer = Writer::start(directory.0.clone()).unwrap();
        let handle = writer.handle();
        assert!(handle.flush(Duration::from_secs(5)));
        assert_eq!(handle.snapshot().records, before.records);
        assert!(handle.clear());
        assert!(handle.flush(Duration::from_secs(5)));
        assert!(!handle.snapshot().clear_failed);
        drop(handle);
        assert!(writer.finish(Duration::from_secs(5)));
        let writer = Writer::start(directory.0.clone()).unwrap();
        let handle = writer.handle();
        assert!(handle.flush(Duration::from_secs(5)));
        assert!(handle.snapshot().records.is_empty());
        drop(handle);
        assert!(writer.finish(Duration::from_secs(5)));
    }

    #[test]
    fn unavailable_disk_keeps_memory_history_and_reports_failure() {
        let directory = Directory::new();
        fs::write(&directory.0, b"not a directory").unwrap();
        let writer = Writer::start(directory.0.clone()).unwrap();
        let handle = writer.handle();
        let mut journey = handle.new_journey().unwrap();
        assert!(journey.record(&handle, transition(Phase::Connected, 1)));
        assert!(handle.flush(Duration::from_secs(5)));
        assert_eq!(handle.snapshot().records.len(), 1);
        assert!(handle.snapshot().disk_failures > 0);
        assert!(handle.clear());
        assert!(handle.flush(Duration::from_secs(5)));
        assert!(handle.snapshot().clear_failed);
        drop(handle);
        assert!(writer.finish(Duration::from_secs(5)));
        fs::remove_file(&directory.0).unwrap();
    }

    #[test]
    fn full_queue_returns_failure_and_counts_missing_events() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let handle = Handle {
            sender,
            shared: Arc::new(Shared {
                snapshot: Mutex::new(Snapshot::default()),
                dropped: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
            run: [1; 16],
            epoch: Instant::now(),
        };
        let mut journey = handle.new_journey().unwrap();
        assert!(journey.record(&handle, transition(Phase::Preparing, 0)));
        assert!(!journey.record(&handle, transition(Phase::Connected, 1)));
        assert!(!handle.clear());
        assert_eq!(handle.snapshot().dropped_queue_records, 1);
        assert_eq!(handle.snapshot().rejected_commands, 1);
        receiver.try_recv().unwrap();
        assert!(journey.record(&handle, transition(Phase::Ended, 1)));
    }

    #[test]
    fn retention_is_bounded_even_when_wall_clock_moves_back() {
        let now = utc_ms();
        let mut records = VecDeque::from([record(now - RETENTION_MS - 1, 0)]);
        for index in 0..MAX_RECORDS + 10 {
            records.push_back(record(now + index as u64, index as u64));
        }
        retain(&mut records, now);
        assert_eq!(records.len(), MAX_RECORDS);
        assert_eq!(records.front().unwrap().monotonic_ms, 10);
        retain(&mut records, now - 1000);
        assert_eq!(records.len(), MAX_RECORDS);
    }

    #[test]
    fn corrupted_active_recovers_previous_and_rejects_sensitive_fields() {
        let directory = Directory::new();
        fs::create_dir_all(&directory.0).unwrap();
        let (active, previous) = paths(&directory.0);
        let expected = VecDeque::from([record(utc_ms(), 1)]);
        write_document(
            &previous,
            &Document {
                schema: SCHEMA,
                sequence: 4,
                records: expected.clone(),
            },
        )
        .unwrap();
        fs::write(&active, b"{truncated").unwrap();
        let writer = Writer::start(directory.0.clone()).unwrap();
        let handle = writer.handle();
        assert!(handle.flush(Duration::from_secs(5)));
        assert_eq!(handle.snapshot().records, expected);
        assert_eq!(handle.snapshot().invalid_disk_documents, 1);
        drop(handle);
        assert!(writer.finish(Duration::from_secs(5)));
        let mut value = serde_json::to_value(record(utc_ms(), 2)).unwrap();
        value["invitation"] = serde_json::json!("gouhuo://private-secret");
        assert!(serde_json::from_value::<Record>(value).is_err());
        let mut value = serde_json::to_value(record(utc_ms(), 2)).unwrap();
        value["cause"] =
            serde_json::json!({"reason":"gouhuo://private-secret", "source":"local_observation"});
        assert!(serde_json::from_value::<Record>(value).is_err());
        assert_eq!(
            serde_json::to_value(ConnectionCause::local(ConnectionReason::HeartbeatTimeout))
                .unwrap(),
            serde_json::json!({"reason":"heartbeat_timeout", "source":"local_observation"})
        );
        assert_eq!(
            ConnectionCause::server(ConnectionReason::Unknown).source,
            EvidenceSource::ServerConfirmed
        );
    }

    #[test]
    fn disk_generations_obey_age_count_and_size_limits() {
        let directory = Directory::new();
        fs::create_dir_all(&directory.0).unwrap();
        let mut records = VecDeque::new();
        for index in 0..MAX_RECORDS + 10 {
            records.push_back(record(utc_ms(), index as u64));
        }
        retain(&mut records, utc_ms());
        persist(&directory.0, &records, &records, 7).unwrap();
        for path in [paths(&directory.0).0, paths(&directory.0).1] {
            assert!(fs::metadata(&path).unwrap().len() <= MAX_FILE_BYTES);
            let document = load(&path, &mut Snapshot::default()).unwrap().unwrap();
            assert_eq!(document.records.len(), MAX_RECORDS);
            assert_eq!(document.records.front().unwrap().monotonic_ms, 10);
        }
        let (active, _) = paths(&directory.0);
        write_document(
            &active,
            &Document {
                schema: SCHEMA,
                sequence: 8,
                records: VecDeque::from([record(utc_ms() - RETENTION_MS - 1, 1)]),
            },
        )
        .unwrap();
        let writer = Writer::start(directory.0.clone()).unwrap();
        let handle = writer.handle();
        assert!(handle.flush(Duration::from_secs(5)));
        assert!(handle.snapshot().records.is_empty());
        for path in [paths(&directory.0).0, paths(&directory.0).1] {
            assert!(load(&path, &mut Snapshot::default())
                .unwrap()
                .unwrap()
                .records
                .is_empty());
        }
        drop(handle);
        assert!(writer.finish(Duration::from_secs(5)));
    }
}
