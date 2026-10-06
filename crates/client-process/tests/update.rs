// SPDX-License-Identifier: GPL-3.0-or-later
use client_process::update::{hex, verify_manifest, Compatibility, EngineStore, Manifest, Version};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct FakeFetch {
    responses: BTreeMap<String, Vec<u8>>,
    seen: Vec<(String, usize)>,
}
impl client_process::update::download::Fetch for FakeFetch {
    fn get(
        &mut self,
        url: &str,
        maximum: usize,
        allowed: &dyn Fn() -> bool,
    ) -> std::io::Result<Vec<u8>> {
        client_process::update::download::check_allowed(allowed)?;
        self.seen.push((url.into(), maximum));
        let bytes = self
            .responses
            .get(url)
            .cloned()
            .ok_or_else(|| std::io::Error::other("fixture unavailable"))?;
        if bytes.len() > maximum {
            return Err(std::io::Error::other("fixture response too large"));
        }
        Ok(bytes)
    }
}

fn download_fixture(fixture: &Fixture, versions: &[&str]) -> FakeFetch {
    use client_process::update::download::REPO;
    let mut responses = BTreeMap::new();
    let mut index = Vec::new();
    for version in versions {
        let (manifest, signature) = fixture.release(version);
        let binary = fs::read(&fixture.binary).unwrap();
        let name = format!("gouhuo-voice-{version}-windows-x64.exe");
        index.push(serde_json::json!({"tag_name": format!("voice-v{version}"), "draft": false, "prerelease": false,
            "assets": [{"name":"manifest.json", "size":manifest.len()}, {"name":"manifest.sig", "size":128}, {"name":name, "size":binary.len()}],
            "browser_download_url":"https://attacker.invalid/unsigned.exe"}));
        let base = format!("https://github.com/{REPO}/releases/download/voice-v{version}");
        responses.insert(format!("{base}/manifest.json"), manifest);
        responses.insert(format!("{base}/manifest.sig"), hex(&signature).into_bytes());
        responses.insert(format!("{base}/{name}"), binary);
    }
    responses.insert(
        format!("https://api.github.com/repos/{REPO}/releases?per_page=100"),
        serde_json::to_vec(&index).unwrap(),
    );
    FakeFetch {
        responses,
        seen: Vec::new(),
    }
}

#[test]
fn downloader_selects_numeric_latest_and_does_not_redownload_it() {
    use client_process::update::download::{check_and_stage, MAX_INDEX};
    let fixture = Fixture::new();
    let mut fetch = download_fixture(&fixture, &["0.1.9", "0.1.10", "0.1.1"]);
    assert_eq!(
        check_and_stage(&mut fetch, &fixture.store, &|| true)
            .unwrap()
            .as_deref(),
        Some("0.1.10")
    );
    assert_eq!(fetch.seen.len(), 4);
    assert_eq!(fetch.seen[0].1, MAX_INDEX);
    assert_eq!(
        fetch.seen[3].1,
        fs::metadata(&fixture.binary).unwrap().len() as usize
    );
    assert!(fetch
        .seen
        .iter()
        .all(|(url, _)| !url.contains("attacker.invalid")));
    fetch.seen.clear();
    assert!(check_and_stage(&mut fetch, &fixture.store, &|| true)
        .unwrap()
        .is_none());
    assert_eq!(fetch.seen.len(), 1);
}

#[test]
fn downloader_skips_unsigned_incompatible_and_nonfinal_releases() {
    use client_process::update::download::{check_and_stage, REPO};
    let fixture = Fixture::new();
    let mut fetch = download_fixture(&fixture, &["0.1.4", "0.1.3", "0.1.2", "0.1.1"]);
    let api = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    let mut index: Vec<serde_json::Value> = serde_json::from_slice(&fetch.responses[&api]).unwrap();
    index[0]["draft"] = true.into();
    index[1]["prerelease"] = true.into();
    fetch
        .responses
        .insert(api, serde_json::to_vec(&index).unwrap());
    let bad_base = format!("https://github.com/{REPO}/releases/download/voice-v0.1.2");
    let mut manifest: Manifest =
        serde_json::from_slice(&fetch.responses[&format!("{bad_base}/manifest.json")]).unwrap();
    manifest.ipc += 1;
    let manifest = serde_json::to_vec(&manifest).unwrap();
    fetch.responses.insert(
        format!("{bad_base}/manifest.sig"),
        hex(&fixture.key.sign(&manifest).to_bytes()).into_bytes(),
    );
    fetch
        .responses
        .insert(format!("{bad_base}/manifest.json"), manifest);
    assert_eq!(
        check_and_stage(&mut fetch, &fixture.store, &|| true)
            .unwrap()
            .as_deref(),
        Some("0.1.1")
    );
    assert!(!fetch
        .seen
        .iter()
        .any(|(url, _)| url.ends_with("0.1.2-windows-x64.exe")
            || url.contains("voice-v0.1.3")
            || url.contains("voice-v0.1.4")));
}

#[test]
fn invalid_signature_never_downloads_code_and_corrupt_code_never_stages() {
    use client_process::update::download::{check_and_stage, REPO};
    let fixture = Fixture::new();
    let mut fetch = download_fixture(&fixture, &["0.1.1"]);
    let base = format!("https://github.com/{REPO}/releases/download/voice-v0.1.1");
    fetch
        .responses
        .insert(format!("{base}/manifest.sig"), vec![b'0'; 128]);
    assert!(check_and_stage(&mut fetch, &fixture.store, &|| true)
        .unwrap()
        .is_none());
    assert_eq!(fetch.seen.len(), 3);
    assert!(!fixture.store.has_pending().unwrap());
    let mut fetch = download_fixture(&fixture, &["0.1.1"]);
    fetch
        .responses
        .get_mut(&format!("{base}/gouhuo-voice-0.1.1-windows-x64.exe"))
        .unwrap()[0] ^= 1;
    assert!(check_and_stage(&mut fetch, &fixture.store, &|| true).is_err());
    assert!(!fixture.store.has_pending().unwrap());
}

#[test]
fn cancelled_or_oversized_download_does_not_publish_a_candidate() {
    use client_process::update::download::{check_and_stage, MAX_INDEX, REPO};
    let fixture = Fixture::new();
    let mut fetch = download_fixture(&fixture, &["0.1.1"]);
    let calls = std::cell::Cell::new(0);
    let allowed = || {
        calls.set(calls.get() + 1);
        calls.get() < 7
    };
    assert_eq!(
        check_and_stage(&mut fetch, &fixture.store, &allowed)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(!fixture.store.has_pending().unwrap());
    let mut fetch = download_fixture(&fixture, &["0.1.1"]);
    fetch.responses.insert(
        format!("https://api.github.com/repos/{REPO}/releases?per_page=100"),
        vec![b' '; MAX_INDEX + 1],
    );
    assert!(check_and_stage(&mut fetch, &fixture.store, &|| true).is_err());
    assert!(!fixture.store.has_pending().unwrap());
}

#[test]
fn disabled_updates_do_not_consume_an_already_staged_version() {
    let fixture = Fixture::new();
    fixture.stage("0.1.1");
    fixture.store.set_activation_allowed(false);
    assert!(fixture.store.begin_start().unwrap().is_none());
    assert!(fixture.store.has_pending().unwrap());
    fixture.store.set_activation_allowed(true);
    assert_eq!(
        fixture.store.begin_start().unwrap().unwrap().version,
        "0.1.1"
    );
}

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
