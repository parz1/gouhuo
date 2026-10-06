// SPDX-License-Identifier: GPL-3.0-or-later
//! Versioned control IPC over private inherited pipes. PCM stays in the engine.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{Devices, RuntimeSnapshot, VoiceIntent};

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_VOLUMES: usize = 2048;

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub engine_version: String,
}

/// Deliberately has no Debug: session keys must never enter diagnostics.
#[derive(Clone, Serialize, Deserialize)]
pub struct StartVoice {
    pub host: String,
    pub udp_port: u16,
    pub session_id: u32,
    pub upstream_key: [u8; 32],
    pub downstream_key: [u8; 32],
    pub devices: Devices,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Cue {
    CameIn,
    WentOut,
    Lost,
    Recovered,
}

#[derive(Serialize, Deserialize)]
pub enum Command {
    Controls,
    StartVoice(StartVoice),
    StartMic(Devices),
    Stop,
    Quiesce,
    StopAfter(u64),
    Shutdown,
    Devices {
        rpc: u64,
    },
    Scan {
        rpc: u64,
        per_device_ms: u64,
    },
    CancelScan {
        rpc: u64,
    },
    Notice {
        cue: Cue,
        name: Option<String>,
        sound: bool,
        gain: f32,
    },
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub revision: u64,
    pub intent: VoiceIntent,
    pub volumes: BTreeMap<u32, f32>,
    pub command: Command,
}

impl Request {
    pub fn validate(&self) -> io::Result<()> {
        let valid = self.volumes.len() <= MAX_VOLUMES
            && self
                .volumes
                .values()
                .all(|v| v.is_finite() && (0.0..=voice_types::MAX_VOLUME).contains(v))
            && match self.intent.mode {
                voice_types::TransmitMode::VoiceActivity { threshold_db } => {
                    threshold_db.is_finite() && (-120.0..=0.0).contains(&threshold_db)
                }
                _ => true,
            }
            && match &self.command {
                Command::StartVoice(v) => v.udp_port != 0 && v.host.len() <= 1024,
                Command::Scan { per_device_ms, .. } => (1..=5000).contains(per_device_ms),
                Command::StopAfter(ms) => *ms <= 5000,
                Command::Notice { name, gain, .. } => {
                    gain.is_finite()
                        && (0.0..=1.0).contains(gain)
                        && name.as_ref().map_or(true, |s| s.len() <= 1024)
                }
                _ => true,
            };
        if valid {
            Ok(())
        } else {
            Err(invalid("invalid voice control request"))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_hardware: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub id: String,
    pub name: String,
    pub is_hardware: bool,
    pub verdict: String,
    pub hears_something: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcReply {
    Devices {
        capture: Vec<Device>,
        render: Vec<Device>,
    },
    Scan(Vec<ScanResult>),
    Error(String),
}

#[derive(Serialize, Deserialize)]
pub enum Reply {
    Snapshot {
        revision: u64,
        requires_auth: bool,
        snapshot: Box<RuntimeSnapshot>,
    },
    Rpc {
        rpc: u64,
        reply: RpcReply,
    },
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let data = serde_json::to_vec(value).map_err(|_| invalid("cannot encode voice IPC"))?;
    if data.is_empty() || data.len() > MAX_FRAME {
        return Err(invalid("voice IPC frame too large"));
    }
    writer.write_all(&(data.len() as u32).to_le_bytes())?;
    writer.write_all(&data)?;
    writer.flush()
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(invalid("invalid voice IPC frame length"));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid voice IPC message"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_rejects_oversize_truncated_and_malformed_input() {
        for bytes in [
            Vec::new(),
            vec![1, 0],
            vec![1, 0, 0, 0],
            vec![1, 0, 0, 0, b'{'],
            ((MAX_FRAME as u32) + 1).to_le_bytes().to_vec(),
        ] {
            assert!(read_frame::<Hello>(&mut bytes.as_slice()).is_err());
        }
    }

    #[test]
    fn concatenated_frames_preserve_message_boundaries() {
        let mut bytes = Vec::new();
        for version in [VERSION, VERSION + 1] {
            write_frame(
                &mut bytes,
                &Hello {
                    protocol: version,
                    engine_version: "0.1.0".into(),
                },
            )
            .unwrap();
        }
        let mut reader = bytes.as_slice();
        assert_eq!(read_frame::<Hello>(&mut reader).unwrap().protocol, VERSION);
        assert_eq!(
            read_frame::<Hello>(&mut reader).unwrap().protocol,
            VERSION + 1
        );
        assert!(reader.is_empty());
    }
}
