//! Self-update from the project's GitHub Releases.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST: &str = "https://api.github.com/repos/Kirato77/cuespark/releases/latest";

/// Release asset name for this platform.
#[cfg(windows)]
const ASSET: &str = "CueSpark.exe";
#[cfg(not(windows))]
const ASSET: &str = "cuespark-linux-x86_64";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    /// "sha256:<hex>", filled in by GitHub for uploaded assets.
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Clone)]
pub struct Update {
    pub version: String,
    url: String,
    size: u64,
    sha256: Option<String>,
}

#[derive(Clone)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Update),
    Downloading(String),
    /// Installed; takes effect on restart.
    Installed(String),
    Failed(String),
}

pub type Shared = Arc<Mutex<State>>;

fn set(state: &Shared, s: State, ctx: &eframe::egui::Context) {
    if let Ok(mut g) = state.lock() {
        *g = s;
    }
    ctx.request_repaint();
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(format!("cuespark/{VERSION}"))
        .build()
        .into()
}

/// Ask GitHub for the latest release, in the background.
pub fn check(state: &Shared, ctx: &eframe::egui::Context) {
    let (state, ctx) = (state.clone(), ctx.clone());
    set(&state, State::Checking, &ctx);
    std::thread::spawn(move || {
        let result = fetch_latest(VERSION).map(|u| match u {
            Some(u) => State::Available(u),
            None => State::UpToDate,
        });
        set(&state, result.unwrap_or_else(State::Failed), &ctx);
    });
}

fn fetch_latest(current: &str) -> Result<Option<Update>, String> {
    let mut resp = agent()
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("cannot reach GitHub: {e}"))?;
    let release: Release = resp.body_mut().read_json().map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    pick(release, current)
}

fn pick(release: Release, current: &str) -> Result<Option<Update>, String> {
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == ASSET)
        .ok_or_else(|| format!("release {} has no {ASSET}", release.tag_name))?;
    Ok(Some(Update {
        version: release.tag_name.trim_start_matches('v').to_string(),
        url: asset.browser_download_url,
        size: asset.size,
        sha256: asset.digest.and_then(|d| d.strip_prefix("sha256:").map(str::to_lowercase)),
    }))
}

/// "v1.10.0" > "1.9.3"; anything unparsable is never newer.
fn is_newer(tag: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> { s.trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect() };
    matches!((parse(tag), parse(current)), (Some(a), Some(b)) if a > b)
}

/// Download, verify and swap the running executable, in the background.
pub fn install(update: Update, state: &Shared, ctx: &eframe::egui::Context) {
    let (state, ctx) = (state.clone(), ctx.clone());
    set(&state, State::Downloading(update.version.clone()), &ctx);
    std::thread::spawn(move || {
        let result = download_and_replace(&update).map(|_| State::Installed(update.version.clone()));
        set(&state, result.unwrap_or_else(State::Failed), &ctx);
    });
}

fn download(u: &Update) -> Result<Vec<u8>, String> {
    let mut resp = agent().get(&u.url).call().map_err(|e| format!("download failed: {e}"))?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(u.size + 1)
        .read_to_vec()
        .map_err(|e| format!("download failed: {e}"))?;
    verify(&bytes, u.size, u.sha256.as_deref())?;
    Ok(bytes)
}

fn download_and_replace(u: &Update) -> Result<(), String> {
    let bytes = download(u)?;
    let tmp = std::env::temp_dir().join(format!("cuespark-update-{}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let replaced = self_replace::self_replace(&tmp).map_err(|e| format!("cannot replace the program file: {e}"));
    let _ = std::fs::remove_file(&tmp);
    replaced
}

/// A truncated or corrupted download must never replace a working program.
fn verify(bytes: &[u8], size: u64, sha256: Option<&str>) -> Result<(), String> {
    if bytes.len() as u64 != size {
        return Err(format!("incomplete download ({} of {size} bytes)", bytes.len()));
    }
    if let Some(expected) = sha256 {
        let got: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
        if got != expected {
            return Err("download is corrupted (checksum mismatch)".into());
        }
    }
    Ok(())
}

/// Start the (new) program again, reopening `show` if given.
pub fn restart(show: Option<&std::path::Path>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(s) = show {
        cmd.arg(s);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("cannot restart: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.10.0", "0.9.0"), "numeric, not text");
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }

    #[test]
    fn picks_platform_asset_from_github_json() {
        let json = format!(
            r#"{{"tag_name":"v9.0.0","assets":[
                {{"name":"other.zip","browser_download_url":"https://x/o","size":1}},
                {{"name":"{ASSET}","browser_download_url":"https://x/a","size":42,"digest":"sha256:ABCD"}}]}}"#
        );
        let u = pick(serde_json::from_str(&json).unwrap(), "0.1.0").unwrap().unwrap();
        assert_eq!((u.version.as_str(), u.url.as_str(), u.size), ("9.0.0", "https://x/a", 42));
        assert_eq!(u.sha256.as_deref(), Some("abcd"));
        let same = format!(r#"{{"tag_name":"v0.1.0","assets":[]}}"#);
        assert!(pick(serde_json::from_str(&same).unwrap(), "0.1.0").unwrap().is_none());
    }

    /// Real GitHub round trip: `cargo test --release -- --ignored`
    #[test]
    #[ignore]
    fn live_latest_release_downloads_and_verifies() {
        let u = fetch_latest("0.0.1").unwrap().expect("a release with this platform's asset");
        assert!(u.sha256.is_some(), "GitHub provides a digest");
        let bytes = download(&u).unwrap();
        assert_eq!(bytes.len() as u64, u.size);
    }

    #[test]
    fn rejects_bad_downloads() {
        let data = b"hello";
        let sum: String = Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
        assert!(verify(data, 5, Some(&sum)).is_ok());
        assert!(verify(data, 6, Some(&sum)).is_err(), "truncated");
        assert!(verify(data, 5, Some("00")).is_err(), "corrupted");
        assert!(verify(data, 5, None).is_ok());
    }
}
