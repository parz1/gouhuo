// SPDX-License-Identifier: GPL-3.0-or-later
//! Signed raw executables in immutable version directories. No archive extraction.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_BINARY: u64 = 32 * 1024 * 1024;
pub const MAX_MANIFEST: u64 = 8192;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    pub target: String,
    pub ipc: u32,
    pub server_protocol: u32,
    pub min_ui: String,
    /// Exclusive upper bound. None means the IPC contract has no UI ceiling.
    pub max_ui: Option<String>,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone)]
pub struct Compatibility {
    pub target: String,
    pub ipc: u32,
    pub server_protocol: u32,
    pub ui: String,
    pub bundled: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);
impl Version {
    pub fn parse(text: &str) -> io::Result<Self> {
        if text.len() > 64 {
            return Err(invalid("invalid engine version"));
        }
        let parts: Vec<_> = text.split('.').collect();
        if parts.len() != 3
            || parts
                .iter()
                .any(|s| s.is_empty() || s.len() > 1 && s.starts_with('0'))
        {
            return Err(invalid("invalid engine version"));
        }
        let number = |s: &str| {
            if !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("invalid engine version"));
            }
            s.parse::<u64>()
                .map_err(|_| invalid("invalid engine version"))
        };
        Ok(Self(
            number(parts[0])?,
            number(parts[1])?,
            number(parts[2])?,
        ))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex<const N: usize>(text: &str) -> io::Result<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("invalid hexadecimal data"));
    }
    let mut result = [0; N];
    for (index, value) in result.iter_mut().enumerate() {
        *value = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid("invalid hexadecimal data"))?;
    }
    Ok(result)
}

pub fn verify_manifest(
    bytes: &[u8],
    signature: &[u8; 64],
    public_key: &[u8; 32],
    compatibility: &Compatibility,
) -> io::Result<Manifest> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_MANIFEST {
        return Err(invalid("invalid engine manifest length"));
    }
    VerifyingKey::from_bytes(public_key)
        .map_err(|_| invalid("invalid engine signing public key"))?
        .verify_strict(bytes, &Signature::from_bytes(signature))
        .map_err(|_| invalid("invalid engine signature"))?;
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|_| invalid("invalid engine manifest"))?;
    Version::parse(&manifest.version)?;
    let ui = Version::parse(&compatibility.ui)?;
    if manifest.schema != 1
        || manifest.target != compatibility.target
        || manifest.ipc != compatibility.ipc
        || manifest.server_protocol != compatibility.server_protocol
        || ui < Version::parse(&manifest.min_ui)?
        || manifest
            .max_ui
            .as_ref()
            .map(|s| Version::parse(s))
            .transpose()?
            .is_some_and(|max| ui >= max)
        || manifest.size == 0
        || manifest.size > MAX_BINARY
    {
        return Err(invalid("incompatible engine release"));
    }
    unhex::<32>(&manifest.sha256)?;
    Ok(manifest)
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    active: Option<String>,
    previous: Option<String>,
    pending: Option<String>,
    booting: Option<String>,
    highest: Option<String>,
}

pub struct Selection {
    pub path: PathBuf,
    pub version: String,
}

/// One instance per GUI. The application's existing single-instance gate also
/// prevents two supervisors from consuming the persisted boot marker.
pub struct EngineStore {
    root: PathBuf,
    public_key: [u8; 32],
    compatibility: Compatibility,
    guard: Mutex<()>,
}
impl EngineStore {
    pub fn has_pending(&self) -> io::Result<bool> {
        let _guard = self.guard.lock().expect("engine store poisoned");
        Ok(self.state()?.pending.is_some())
    }
    pub fn new(
        root: PathBuf,
        public_key: [u8; 32],
        compatibility: Compatibility,
    ) -> io::Result<Self> {
        Version::parse(&compatibility.ui)?;
        Version::parse(&compatibility.bundled)?;
        VerifyingKey::from_bytes(&public_key).map_err(|_| invalid("invalid engine public key"))?;
        Ok(Self {
            root,
            public_key,
            compatibility,
            guard: Mutex::new(()),
        })
    }

    /// Verify before staging. An unsigned pointer never authorizes execution.
    pub fn stage(
        &self,
        manifest: &[u8],
        signature: &[u8; 64],
        binary: &Path,
    ) -> io::Result<String> {
        let _guard = self.guard.lock().expect("engine store poisoned");
        let release = verify_manifest(manifest, signature, &self.public_key, &self.compatibility)?;
        let mut state = self.state()?;
        let floor = state
            .highest
            .as_deref()
            .unwrap_or(&self.compatibility.bundled);
        if Version::parse(&release.version)? <= Version::parse(floor)?
            || Version::parse(&release.version)? <= Version::parse(&self.compatibility.bundled)?
        {
            return Err(invalid("engine release is not newer"));
        }
        let bytes = bounded_read(binary, MAX_BINARY)?;
        check_binary(&bytes, &release)?;
        fs::create_dir_all(&self.root)?;
        let destination = self.root.join(&release.version);
        if destination.exists() {
            let old = self.selection(&release.version)?;
            if bounded_read(&old.path, MAX_BINARY)? != bytes
                || bounded_read(&destination.join("manifest.json"), MAX_MANIFEST)? != manifest
                || bounded_read(&destination.join("manifest.sig"), 128)?
                    != hex(signature).as_bytes()
            {
                return Err(invalid("engine version already has different contents"));
            }
        } else {
            let temporary = self.temporary("incoming");
            fs::create_dir(&temporary)?;
            let result = (|| {
                durable_write(&temporary.join(binary_name()), &bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        temporary.join(binary_name()),
                        fs::Permissions::from_mode(0o700),
                    )?;
                }
                durable_write(&temporary.join("manifest.json"), manifest)?;
                durable_write(&temporary.join("manifest.sig"), hex(signature).as_bytes())?;
                fs::rename(&temporary, &destination)
            })();
            if result.is_err() {
                let _ = fs::remove_dir_all(&temporary);
            }
            result?;
        }
        state.pending = Some(release.version.clone());
        state.highest = Some(release.version.clone());
        self.save(&state)?;
        Ok(release.version)
    }

    /// Called only before a new process owns audio. Pending versions are consumed
    /// at application startup, never during an authenticated call or scan.
    pub fn begin_start(&self) -> io::Result<Option<Selection>> {
        let _guard = self.guard.lock().expect("engine store poisoned");
        let mut state = self.state()?;
        if state.booting.take().is_some() {
            state.active = state.previous.take();
        } else if let Some(next) = state.pending.take() {
            state.previous = state.active.take();
            state.active = Some(next);
        }
        let selected = match state
            .active
            .as_deref()
            .map(|v| self.selection(v))
            .transpose()
        {
            Ok(selection) => selection,
            Err(_) => {
                state.active = state.previous.take();
                state.active.as_deref().and_then(|v| self.selection(v).ok())
            }
        };
        state.active = selected.as_ref().map(|s| s.version.clone());
        state.booting = state.active.clone();
        // Leave the installer-only path completely free of filesystem writes.
        if self.root.exists() {
            self.save(&state)?;
        }
        Ok(selected)
    }

    pub fn confirm(&self, version: &str) -> io::Result<()> {
        let _guard = self.guard.lock().expect("engine store poisoned");
        let mut state = self.state()?;
        if state.booting.as_deref() == Some(version) && state.active.as_deref() == Some(version) {
            state.booting = None;
            self.save(&state)?;
        }
        Ok(())
    }

    /// A process crash cannot reset the anti-downgrade floor. Only its already
    /// validated previous version (or the bundled engine) can be a fallback.
    pub fn reject(&self, version: &str) -> io::Result<()> {
        let _guard = self.guard.lock().expect("engine store poisoned");
        let mut state = self.state()?;
        if state.active.as_deref() == Some(version) {
            state.active = state.previous.take();
            state.booting = None;
            self.save(&state)?;
        }
        Ok(())
    }

    fn selection(&self, version: &str) -> io::Result<Selection> {
        if Version::parse(version)? <= Version::parse(&self.compatibility.bundled)? {
            return Err(invalid(
                "installed engine is older than the bundled baseline",
            ));
        }
        let directory = self.root.join(version);
        let manifest = bounded_read(&directory.join("manifest.json"), MAX_MANIFEST)?;
        let signature = bounded_read(&directory.join("manifest.sig"), 128)?;
        let signature = unhex::<64>(
            std::str::from_utf8(&signature).map_err(|_| invalid("invalid signature"))?,
        )?;
        let release =
            verify_manifest(&manifest, &signature, &self.public_key, &self.compatibility)?;
        if release.version != version {
            return Err(invalid("engine version directory mismatch"));
        }
        let path = directory.join(binary_name());
        check_binary(&bounded_read(&path, MAX_BINARY)?, &release)?;
        Ok(Selection {
            path,
            version: version.into(),
        })
    }

    fn state(&self) -> io::Result<State> {
        let bytes = match bounded_read(&self.root.join("state.json"), MAX_MANIFEST) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            other => other?,
        };
        let state: State =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid engine state"))?;
        for version in [
            &state.active,
            &state.previous,
            &state.pending,
            &state.booting,
            &state.highest,
        ]
        .into_iter()
        .flatten()
        {
            Version::parse(version)?;
        }
        Ok(state)
    }

    fn save(&self, state: &State) -> io::Result<()> {
        let bytes = serde_json::to_vec(state).map_err(|_| invalid("cannot encode engine state"))?;
        let temporary = self.temporary("state");
        durable_write(&temporary, &bytes)?;
        let result = atomic_replace(&temporary, &self.root.join("state.json"));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    fn temporary(&self, label: &str) -> PathBuf {
        self.root.join(format!(
            ".{label}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "gouhuo-voice.exe"
    } else {
        "gouhuo-voice"
    }
}
fn check_binary(bytes: &[u8], manifest: &Manifest) -> io::Result<()> {
    if bytes.len() as u64 != manifest.size
        || Sha256::digest(bytes).as_slice() != unhex::<32>(&manifest.sha256)?
    {
        return Err(invalid("engine executable checksum mismatch"));
    }
    Ok(())
}
fn bounded_read(path: &Path, maximum: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > maximum {
        return Err(invalid("engine file too large"));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("engine file too large"));
    }
    Ok(bytes)
}
fn durable_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<_> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: both UTF-16 paths are terminated and live for the call.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
