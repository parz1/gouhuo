// SPDX-License-Identifier: GPL-3.0-or-later
use client_process::update::{hex, verify_manifest, Compatibility, EngineStore, Manifest, Version};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static ID: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    store: EngineStore,
    key: SigningKey,
    binary: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gouhuo-signed-engine-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let key = SigningKey::from_bytes(&[9; 32]); // Test fixture, never a release key.
        let store = EngineStore::new(
            root.join("versions"),
            key.verifying_key().to_bytes(),
            compatibility(),
        )
        .unwrap();
        let binary = root.join("source.exe");
        fs::write(&binary, b"signed test executable").unwrap();
        Self {
            root,
            store,
            key,
            binary,
        }
    }
    fn release(&self, version: &str) -> (Vec<u8>, [u8; 64]) {
        let bytes = fs::read(&self.binary).unwrap();
        let manifest = Manifest {
            schema: 1,
            version: version.into(),
            target: "windows-x64".into(),
            ipc: 1,
            server_protocol: 2,
            min_ui: "0.3.1".into(),
            max_ui: Some("0.4.0".into()),
            size: bytes.len() as u64,
            sha256: hex(&Sha256::digest(&bytes)),
        };
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let signature = self.key.sign(&manifest).to_bytes();
        (manifest, signature)
    }
    fn stage(&self, version: &str) {
        let (manifest, signature) = self.release(version);
        self.store
            .stage(&manifest, &signature, &self.binary)
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn compatibility() -> Compatibility {
    Compatibility {
        target: "windows-x64".into(),
        ipc: 1,
        server_protocol: 2,
        ui: "0.3.1".into(),
        bundled: "0.1.0".into(),
    }
}

#[test]
fn invalid_signature_checksum_and_compatibility_never_stage() {
    let fixture = Fixture::new();
    let (manifest, signature) = fixture.release("0.1.1");
    let wrong = SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
    assert!(verify_manifest(&manifest, &signature, &wrong, &compatibility()).is_err());
    let mut changed = manifest.clone();
    changed.push(b' ');
    assert!(fixture
        .store
        .stage(&changed, &signature, &fixture.binary)
        .is_err());
    fs::write(&fixture.binary, b"corrupted executable").unwrap();
    assert!(fixture
        .store
        .stage(&manifest, &signature, &fixture.binary)
        .is_err());
    let mut incompatible = compatibility();
    incompatible.ipc += 1;
    assert!(verify_manifest(
        &manifest,
        &signature,
        &fixture.key.verifying_key().to_bytes(),
        &incompatible
    )
    .is_err());
    incompatible = compatibility();
    incompatible.ui = "0.4.0".into();
    assert!(verify_manifest(
        &manifest,
        &signature,
        &fixture.key.verifying_key().to_bytes(),
        &incompatible
    )
    .is_err());
    assert!(fixture.store.begin_start().unwrap().is_none());
}

#[test]
fn interrupted_start_rolls_back_and_does_not_lower_the_replay_floor() {
    let fixture = Fixture::new();
    fixture.stage("0.1.1");
    let first = fixture.store.begin_start().unwrap().unwrap();
    assert_eq!(first.version, "0.1.1");
    fixture.store.confirm(&first.version).unwrap();
    fixture.stage("0.1.2");
    assert_eq!(
        fixture.store.begin_start().unwrap().unwrap().version,
        "0.1.2"
    );
    // Simulate loss of the process before confirmation: consume its boot marker.
    assert_eq!(
        fixture.store.begin_start().unwrap().unwrap().version,
        "0.1.1"
    );
    let (manifest, signature) = fixture.release("0.1.2");
    assert!(fixture
        .store
        .stage(&manifest, &signature, &fixture.binary)
        .is_err());
    fixture.store.confirm("0.1.1").unwrap();
    fixture.stage("0.1.3");
    assert_eq!(
        fixture.store.begin_start().unwrap().unwrap().version,
        "0.1.3"
    );
}

#[test]
fn modified_stored_executable_falls_back_without_executing_it() {
    let fixture = Fixture::new();
    fixture.stage("0.1.1");
    let selected = fixture.store.begin_start().unwrap().unwrap();
    fixture.store.confirm("0.1.1").unwrap();
    fs::write(selected.path, b"tampered").unwrap();
    assert!(fixture.store.begin_start().unwrap().is_none());
}

#[test]
fn a_new_installer_does_not_select_an_older_cached_engine() {
    let fixture = Fixture::new();
    fixture.stage("0.1.1");
    fixture.store.begin_start().unwrap();
    fixture.store.confirm("0.1.1").unwrap();
    let mut upgraded = compatibility();
    upgraded.bundled = "0.1.2".into();
    let store = EngineStore::new(
        fixture.root.join("versions"),
        fixture.key.verifying_key().to_bytes(),
        upgraded,
    )
    .unwrap();
    assert!(store.begin_start().unwrap().is_none());
}

#[test]
fn unsafe_versions_and_state_paths_are_rejected() {
    for version in [
        "../0.1.1",
        "0.01.1",
        "0.1.1/evil",
        "0.1.1-rc.1",
        "0.1.1.1",
        "0.1.+1",
    ] {
        assert!(Version::parse(version).is_err());
    }
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("versions")).unwrap();
    fs::write(
        fixture.root.join("versions/state.json"),
        br#"{"active":"../../other.exe"}"#,
    )
    .unwrap();
    assert!(fixture.store.begin_start().is_err());
}
