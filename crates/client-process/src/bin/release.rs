// SPDX-License-Identifier: GPL-3.0-or-later
//! Offline signing/staging tool. Release secrets are never command arguments.

use client_process::update::{
    hex, unhex, Compatibility, EngineStore, Manifest, Version, MAX_BINARY,
};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::{env, fs, io, path::PathBuf};
use zeroize::Zeroizing;

fn main() {
    if let Err(error) = run() {
        eprintln!("voice release: {error}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sign") if args.len() == 5 => {
            let binary = PathBuf::from(&args[1]);
            let version = &args[2];
            let ui = &args[3];
            let output = PathBuf::from(&args[4]);
            Version::parse(version)?;
            Version::parse(ui)?;
            if fs::metadata(&binary)?.len() == 0 || fs::metadata(&binary)?.len() > MAX_BINARY {
                return Err(io::Error::other("invalid engine executable size"));
            }
            let bytes = fs::read(&binary)?;
            let secret = Zeroizing::new(env::var("GOUHUO_VOICE_SIGNING_KEY")
                .map_err(|_| io::Error::other("GOUHUO_VOICE_SIGNING_KEY is required"))?);
            let seed = Zeroizing::new(unhex::<32>(&secret)?);
            let key = SigningKey::from_bytes(&seed);
            let expected = env::var("GOUHUO_VOICE_PUBLIC_KEY")
                .map_err(|_| io::Error::other("GOUHUO_VOICE_PUBLIC_KEY is required"))?;
            if key.verifying_key().to_bytes() != unhex::<32>(&expected)? {
                return Err(io::Error::other("signing key does not match release public key"));
            }
            let manifest = Manifest {
                schema: 1, version: version.clone(), target: "windows-x64".into(),
                ipc: client_runtime::ipc::VERSION,
                server_protocol: protocol::control::PROTOCOL_VERSION,
                min_ui: ui.clone(), max_ui: None,
                size: bytes.len() as u64, sha256: hex(&Sha256::digest(&bytes)),
            };
            let manifest = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
            let signature = key.sign(&manifest).to_bytes();
            fs::create_dir_all(&output)?;
            fs::write(output.join(format!("gouhuo-voice-{version}-windows-x64.exe")), bytes)?;
            fs::write(output.join("manifest.json"), manifest)?;
            fs::write(output.join("manifest.sig"), hex(&signature))?;
            println!("Signed voice release {version}; IPC {}.", client_runtime::ipc::VERSION);
        }
        Some("stage") if args.len() == 7 => {
            let public_key = unhex::<32>(&args[2])?;
            let store = EngineStore::new(PathBuf::from(&args[1]), public_key, Compatibility {
                target: if cfg!(windows) { "windows-x64" } else { "linux-x64" }.into(),
                ipc: client_runtime::ipc::VERSION,
                server_protocol: protocol::control::PROTOCOL_VERSION,
                ui: args[6].clone(), bundled: "0.1.0".into(),
            })?;
            let manifest = fs::read(&args[4])?;
            let signature = fs::read_to_string(&args[5])?;
            let version = store.stage(&manifest, &unhex::<64>(&signature)?, &PathBuf::from(&args[3]))?;
            println!("Staged voice {version}; activation waits for the next engine startup.");
        }
        _ => return Err(io::Error::other(
            "usage: sign <exe> <engine-version> <min-ui-version> <output-dir>\n       stage <store-dir> <public-key-hex> <exe> <manifest.json> <manifest.sig> <ui-version>"
        )),
    }
    Ok(())
}
