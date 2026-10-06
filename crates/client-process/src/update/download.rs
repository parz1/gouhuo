// SPDX-License-Identifier: GPL-3.0-or-later
//! Untrusted release discovery, signed selection, bounded download and staging.

use super::{unhex, EngineStore, Version, MAX_MANIFEST};
use serde::Deserialize;
use std::io;

pub const REPO: &str = "parz1/gouhuo";
pub const MAX_INDEX: usize = 1024 * 1024;

pub mod https;

pub trait Fetch {
    /// HTTPS only. Implementations enforce maximum, cancellation and deadlines.
    fn get(&mut self, url: &str, maximum: usize, allowed: &dyn Fn() -> bool)
        -> io::Result<Vec<u8>>;
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
}

pub fn check_and_stage(
    fetch: &mut impl Fetch,
    store: &EngineStore,
    allowed: &dyn Fn() -> bool,
) -> io::Result<Option<String>> {
    check_allowed(allowed)?;
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    let bytes = fetch.get(&url, MAX_INDEX, allowed)?;
    if bytes.len() > MAX_INDEX {
        return Err(invalid("release index too large"));
    }
    let releases: Vec<Release> =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid release index"))?;
    if releases.len() > 100 {
        return Err(invalid("too many releases"));
    }
    let mut candidates = Vec::new();
    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        let Some(version) = release.tag_name.strip_prefix("voice-v") else {
            continue;
        };
        let Ok(parsed) = Version::parse(version) else {
            continue;
        };
        if store.accepts_version(version)? {
            candidates.push((parsed, release));
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    // Bound the work even if a repository publishes many invalid candidates.
    for (_, release) in candidates.into_iter().take(8) {
        check_allowed(allowed)?;
        let version = release
            .tag_name
            .strip_prefix("voice-v")
            .expect("validated tag");
        let binary_name = format!("gouhuo-voice-{version}-windows-x64.exe");
        let asset = |name: &str| {
            let mut found = release.assets.iter().filter(|asset| asset.name == name);
            let item = found.next()?;
            found.next().is_none().then_some(item)
        };
        if !asset("manifest.json").is_some_and(|a| a.size > 0 && a.size <= MAX_MANIFEST)
            || !asset("manifest.sig").is_some_and(|a| a.size == 128)
        {
            continue;
        }
        let base = format!(
            "https://github.com/{REPO}/releases/download/{}",
            release.tag_name
        );
        let manifest = fetch.get(
            &format!("{base}/manifest.json"),
            MAX_MANIFEST as usize,
            allowed,
        )?;
        let signature = fetch.get(&format!("{base}/manifest.sig"), 128, allowed)?;
        let signature = match std::str::from_utf8(&signature)
            .ok()
            .and_then(|s| unhex::<64>(s).ok())
        {
            Some(signature) => signature,
            None => continue,
        };
        // Authenticity and compatibility are established before fetching code.
        let signed = match store.validate_release(&manifest, &signature) {
            Ok(signed) if signed.version == version => signed,
            _ => continue,
        };
        if !asset(&binary_name).is_some_and(|a| a.size == signed.size) {
            continue;
        }
        check_allowed(allowed)?;
        if !store.accepts_version(version)? {
            continue;
        }
        let binary = fetch.get(
            &format!("{base}/{binary_name}"),
            signed.size as usize,
            allowed,
        )?;
        check_allowed(allowed)?;
        return store.stage_bytes(&manifest, &signature, &binary).map(Some);
    }
    Ok(None)
}

pub fn check_allowed(allowed: &dyn Fn() -> bool) -> io::Result<()> {
    if allowed() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "voice update cancelled",
        ))
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
