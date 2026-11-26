//! Over-the-air updates.
//!
//! Every release publishes `latest.json` next to its assets:
//!
//! ```json
//! { "version": "0.8.0", "url": ".../TIDE-0.8.0-macos-universal.app.zip",
//!   "sha256": "<hex>", "signature": "<base64 ed25519>" }
//! ```
//!
//! TIDE fetches it shortly after launch and every few hours. A newer version
//! that the listener has not skipped brings up the update toast. "Update now"
//! downloads the zip, checks its SHA-256 and the release key's signature over
//! `version + sha256`, unpacks it next to the running app, swaps the two
//! bundles and relaunches. Only release builds (built with `TIDE_UPDATES=1`)
//! check on their own; dev builds would otherwise replace themselves with the
//! public release.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::controller::{Ctx, Ui, on_ui};

/// The running version, as CI stamped it into `Cargo.toml` before building.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

const MANIFEST_URL: &str =
    "https://github.com/GameTL/tide-tidal-rust/releases/latest/download/latest.json";
/// Where "Download page" and the fallbacks send the listener.
pub const RELEASES_PAGE: &str = "https://github.com/GameTL/tide-tidal-rust/releases/latest";

/// The release key's public half. CI signs with the private half, kept in the
/// `TIDE_UPDATE_KEY` repository secret.
const PUBLIC_KEY: [u8; 32] = [
    0x32, 0xeb, 0x59, 0xf9, 0x3b, 0xa0, 0xa4, 0x24, 0x5f, 0x54, 0xf2, 0xd1, 0x5c, 0xeb, 0x78, 0x54,
    0x7e, 0x0d, 0xa3, 0x71, 0xc3, 0x24, 0x87, 0xfd, 0x1a, 0x48, 0xda, 0x55, 0x06, 0xc0, 0x2a, 0xc2,
];

/// The first check waits until the window and the first page have loaded.
const FIRST_CHECK: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// A stalled download gives up after this long without a byte.
const STALL: Duration = Duration::from_secs(30);
/// The app zip is about 30 MB; anything far larger is not ours.
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub signature: String,
}

/// Toast states, as `App.update-state` names them.
mod state {
    pub const AVAILABLE: &str = "available";
    pub const DOWNLOADING: &str = "downloading";
    pub const INSTALLING: &str = "installing";
    pub const FAILED: &str = "failed";
}

thread_local! {
    /// The release the toast is offering. UI thread only.
    static OFFER: RefCell<Option<Manifest>> = const { RefCell::new(None) };
    /// Closed with ✕: not offered again until the next launch (or a manual check).
    static DISMISSED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Whether this build checks for updates on its own. `TIDE_UPDATE_MANIFEST`
/// points any build at a test manifest; the signature check still applies.
pub fn enabled() -> bool {
    option_env!("TIDE_UPDATES") == Some("1") || std::env::var_os("TIDE_UPDATE_MANIFEST").is_some()
}

fn manifest_url() -> String {
    std::env::var("TIDE_UPDATE_MANIFEST").unwrap_or_else(|_| MANIFEST_URL.to_string())
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("TIDE/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

/// `major.minor.patch`, with an optional leading `v`. Pre-release and build
/// suffixes are ignored: releases never carry them.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// Whether `candidate` is a later release than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// What the release key signs: the version and the zip's hash, so a valid
/// signature cannot be moved to another version's manifest.
fn signed_message(version: &str, sha256: &str) -> Vec<u8> {
    format!(
        "TIDE update v1\n{version}\n{}\n",
        sha256.to_ascii_lowercase()
    )
    .into_bytes()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check the downloaded zip against the manifest and the manifest against the key.
fn verify(manifest: &Manifest, zip: &[u8], key: &[u8; 32]) -> anyhow::Result<()> {
    let actual = hex(&Sha256::digest(zip));
    if !actual.eq_ignore_ascii_case(manifest.sha256.trim()) {
        bail!("the download is corrupt (SHA-256 mismatch)");
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(manifest.signature.trim())
        .context("the update signature is not valid base64")?;
    let signature = Signature::from_slice(&raw).context("the update signature is malformed")?;
    let key = VerifyingKey::from_bytes(key).context("the release key is malformed")?;
    key.verify_strict(&signed_message(&manifest.version, &actual), &signature)
        .map_err(|_| anyhow!("the update is not signed by the TIDE release key"))
}

async fn fetch_manifest() -> anyhow::Result<Manifest> {
    let manifest = client()?
        .get(manifest_url())
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json::<Manifest>()
        .await?;
    if parse_version(&manifest.version).is_none() {
        bail!(
            "the update manifest has no valid version ({:?})",
            manifest.version
        );
    }
    Ok(manifest)
}

/// Check for a newer release and offer it. `manual` is the menu's Check for
/// Updates…: it ignores a skipped version and always reports back.
pub async fn check(ctx: &Arc<Ctx>, manual: bool) {
    match fetch_manifest().await {
        Ok(manifest) if is_newer(&manifest.version, CURRENT) => {
            let skipped = ctx.settings.read().unwrap().skipped_version == manifest.version;
            if skipped && !manual {
                tracing::debug!(version = %manifest.version, "update available but skipped");
                return;
            }
            tracing::info!(version = %manifest.version, "update available");
            on_ui(move |ui| offer(ui, manifest, manual));
        }
        Ok(_) => {
            tracing::debug!("TIDE {CURRENT} is up to date");
            if manual {
                on_ui(|ui| {
                    crate::alerts::notice(
                        &ui.app(),
                        &format!("TIDE {CURRENT} is the latest version"),
                    );
                });
            }
        }
        Err(e) if manual => tracing::error!("Could not check for updates: {e}"),
        Err(e) => tracing::warn!("could not check for updates: {e}"),
    }
}

/// Run the automatic checks for as long as TIDE runs.
pub async fn run_checks(ctx: Arc<Ctx>) {
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        check(&ctx, false).await;
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

fn offer(ui: &Rc<Ui>, manifest: Manifest, manual: bool) {
    let app = ui.app();
    let busy = [state::DOWNLOADING, state::INSTALLING].contains(&app.get_update_state().as_str());
    if busy {
        return;
    }
    let dismissed = DISMISSED.with(|d| d.borrow().as_deref() == Some(manifest.version.as_str()));
    if dismissed && !manual {
        return;
    }
    app.set_update_from(CURRENT.into());
    app.set_update_to(manifest.version.as_str().into());
    app.set_update_error("".into());
    app.set_update_progress(0.0);
    app.set_update_state(state::AVAILABLE.into());
    OFFER.with(|o| *o.borrow_mut() = Some(manifest));
}

/// The toast's ✕: hide it until the next launch without skipping the version.
pub fn dismiss(ui: &Rc<Ui>) {
    let app = ui.app();
    if app.get_update_state() == state::AVAILABLE {
        let version = app.get_update_to().to_string();
        DISMISSED.with(|d| *d.borrow_mut() = Some(version));
    }
    app.set_update_state("".into());
}

/// The version the toast offers, for Skip this version.
pub fn offered_version() -> Option<String> {
    OFFER.with(|o| o.borrow().as_ref().map(|m| m.version.clone()))
}

/// Update now: download, verify, swap the bundles and relaunch.
pub fn install_now(ui: &Rc<Ui>) {
    let Some(manifest) = OFFER.with(|o| o.borrow().clone()) else {
        return;
    };
    let app = ui.app();
    if app.get_update_state() != state::AVAILABLE && app.get_update_state() != state::FAILED {
        return;
    }
    let bundle = match replaceable_bundle() {
        Ok(bundle) => bundle,
        Err(why) => {
            // Nothing to swap in place: hand over to the download page.
            tracing::warn!("cannot update in place: {why}");
            let _ = open::that(RELEASES_PAGE);
            app.set_update_state("".into());
            crate::alerts::notice(&app, &format!("Opened the download page: {why}"));
            return;
        }
    };
    app.set_update_progress(0.0);
    app.set_update_state(state::DOWNLOADING.into());
    tokio::spawn(async move {
        let result = download_and_install(&manifest, &bundle).await;
        on_ui(move |ui| match result {
            Ok(work) => relaunch(&bundle, &work),
            Err(e) => {
                tracing::warn!("update failed: {e:#}");
                let app = ui.app();
                app.set_update_error(format!("{e}").into());
                app.set_update_state(state::FAILED.into());
            }
        });
    });
}

async fn download_and_install(manifest: &Manifest, bundle: &Path) -> anyhow::Result<PathBuf> {
    let zip = download(manifest).await?;
    verify(manifest, &zip, &PUBLIC_KEY)?;
    on_ui(|ui| ui.app().set_update_state(state::INSTALLING.into()));
    let bundle = bundle.to_path_buf();
    tokio::task::spawn_blocking(move || swap_in(&bundle, &zip)).await?
}

async fn download(manifest: &Manifest) -> anyhow::Result<Vec<u8>> {
    let mut response = client()?
        .get(&manifest.url)
        .send()
        .await?
        .error_for_status()
        .context("could not download the update")?;
    let total = response.content_length().unwrap_or(0);
    if total > MAX_DOWNLOAD {
        bail!("the update is unexpectedly large ({total} bytes)");
    }
    let mut zip = Vec::with_capacity(total as usize);
    let mut shown = -1i32;
    loop {
        let chunk = tokio::time::timeout(STALL, response.chunk())
            .await
            .map_err(|_| anyhow!("the download stalled"))??;
        let Some(chunk) = chunk else { break };
        zip.extend_from_slice(&chunk);
        if zip.len() as u64 > MAX_DOWNLOAD {
            bail!("the update is unexpectedly large");
        }
        if let Some(percent) = (zip.len() as u64 * 100).checked_div(total) {
            let percent = percent as i32;
            if percent != shown {
                shown = percent;
                on_ui(move |ui| ui.app().set_update_progress(percent as f32 / 100.0));
            }
        }
    }
    Ok(zip)
}

/// The running app bundle, when it can be replaced in place.
fn replaceable_bundle() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot locate TIDE ({e})"))?;
    let bundle = bundle_of(&exe).ok_or("TIDE is not running from an app bundle")?;
    let text = bundle.to_string_lossy();
    if text.contains("/AppTranslocation/") {
        return Err("macOS is running TIDE from a temporary copy; move it to Applications".into());
    }
    if text.starts_with("/Volumes/") {
        return Err("TIDE is running from the disk image".into());
    }
    let parent = bundle.parent().ok_or("TIDE has no parent folder")?;
    if !writable(parent) || !writable(&bundle) {
        return Err(format!("{} is not writable", parent.display()));
    }
    Ok(bundle)
}

/// `…/TIDE.app/Contents/MacOS/TIDE` → `…/TIDE.app`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    is_bundle.then(|| bundle.to_path_buf())
}

/// Off Unix there is no app bundle to replace, so nothing counts as writable.
#[cfg(not(unix))]
fn writable(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// Unpack `zip` next to `bundle` and swap the two. On return the new app is at
/// `bundle` and the old one waits in the returned folder (as `old.app`) for
/// the relaunch helper to remove it once TIDE has exited.
fn swap_in(bundle: &Path, zip: &[u8]) -> anyhow::Result<PathBuf> {
    let parent = bundle.parent().context("TIDE has no parent folder")?;
    let pid = std::process::id();
    // Same folder, so the renames below stay on one volume and are atomic.
    let work = parent.join(format!(".TIDE-update-{pid}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir(&work).with_context(|| format!("cannot write to {}", parent.display()))?;
    let result = (|| {
        let archive = work.join("update.zip");
        std::fs::write(&archive, zip)?;
        let unpacked = work.join("app");
        // ditto keeps the bundle's symlinks, permissions and code signature intact.
        let status = std::process::Command::new("/usr/bin/ditto")
            .arg("-x")
            .arg("-k")
            .arg(&archive)
            .arg(&unpacked)
            .status()?;
        if !status.success() {
            bail!("could not unpack the update ({status})");
        }
        let fresh = unpacked.join("TIDE.app");
        if !fresh.join("Contents/MacOS/TIDE").is_file() {
            bail!("the update has no TIDE.app inside");
        }
        // Files TIDE writes are not quarantined, but clear it if anything did.
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&fresh)
            .status();
        let old = work.join("old.app");
        std::fs::rename(bundle, &old).context("could not move the current TIDE aside")?;
        if let Err(e) = std::fs::rename(&fresh, bundle) {
            // Put the running version back so TIDE still opens next time.
            let _ = std::fs::rename(&old, bundle);
            return Err(anyhow!(e).context("could not move the new TIDE into place"));
        }
        let _ = std::fs::remove_file(&archive);
        Ok(())
    })();
    match result {
        Ok(()) => Ok(work),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work);
            Err(e)
        }
    }
}

/// Quit and reopen the new TIDE. A detached shell waits for this process to
/// exit, removes the old bundle and opens the new one.
fn relaunch(bundle: &Path, work: &Path) {
    tracing::info!("update installed, relaunching");
    let script = r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done
rm -rf "$2"
exec /usr/bin/open "$3""#;
    let spawned = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(work)
        .arg(bundle)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        // The new version is already in place; it opens next time.
        tracing::error!("Updated TIDE, but could not restart it ({e}). Quit and reopen TIDE.");
        return;
    }
    // Same as Quit: the exit handler hands the output device back.
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.8.0", "0.7.9"));
        assert!(is_newer("v0.7.10", "0.7.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.7.9", "0.7.9"));
        assert!(!is_newer("0.7.8", "0.7.9"));
        assert!(!is_newer("garbage", "0.7.9"));
        assert!(!is_newer("0.8", "0.7.9"));
        assert!(!is_newer("0.8.0.1", "0.7.9"));
        assert_eq!(parse_version("0.8.0-beta+1"), Some((0, 8, 0)));
    }

    fn signed(key: &SigningKey, version: &str, zip: &[u8]) -> Manifest {
        let sha256 = hex(&Sha256::digest(zip));
        let signature = key.sign(&signed_message(version, &sha256));
        Manifest {
            version: version.into(),
            url: "https://example.invalid/TIDE.app.zip".into(),
            sha256,
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        }
    }

    #[test]
    fn accepts_only_what_the_release_key_signed() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key.verifying_key().to_bytes();
        let zip = b"pretend this is TIDE.app";
        let manifest = signed(&key, "0.8.0", zip);
        verify(&manifest, zip, &public).unwrap();

        // A different file under the same manifest.
        assert!(verify(&manifest, b"something else", &public).is_err());
        // The signature moved to another version.
        let moved = Manifest {
            version: "9.9.9".into(),
            ..manifest.clone()
        };
        assert!(verify(&moved, zip, &public).is_err());
        // Signed by someone else.
        let stranger = SigningKey::from_bytes(&[9; 32]);
        assert!(verify(&signed(&stranger, "0.8.0", zip), zip, &public).is_err());
    }

    /// The release workflow signs with `scripts/sign-update.sh` (OpenSSL), not
    /// with this crate: check that what it writes is what `verify` accepts.
    #[test]
    #[ignore = "needs OpenSSL 3 or later on PATH (macOS ships LibreSSL)"]
    fn accepts_what_the_release_script_signs() {
        let root = std::env::temp_dir().join(format!("tide-sign-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        // PKCS#8 for an ed25519 seed: a fixed 16-byte header, then the seed.
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&key.to_bytes());
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        );
        let key_file = root.join("key.pem");
        std::fs::write(&key_file, pem).unwrap();
        let zip = b"pretend this is TIDE.app";
        let zip_file = root.join("TIDE.app.zip");
        std::fs::write(&zip_file, zip).unwrap();

        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sign-update.sh");
        let output = std::process::Command::new("/bin/sh")
            .arg(script)
            .arg(&zip_file)
            .arg("v1.2.3")
            .arg("https://example.invalid/TIDE.app.zip")
            .env("TIDE_UPDATE_KEY_FILE", &key_file)
            .env_remove("TIDE_UPDATE_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest: Manifest = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(manifest.version, "1.2.3");
        verify(&manifest, zip, &key.verifying_key().to_bytes()).unwrap();
        assert!(is_newer(&manifest.version, "1.2.2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn release_key_is_a_valid_point() {
        VerifyingKey::from_bytes(&PUBLIC_KEY).unwrap();
    }

    #[test]
    fn finds_the_bundle_around_the_executable() {
        assert_eq!(
            bundle_of(Path::new("/Applications/TIDE.app/Contents/MacOS/TIDE")),
            Some(PathBuf::from("/Applications/TIDE.app"))
        );
        assert_eq!(bundle_of(Path::new("/Users/me/target/release/TIDE")), None);
    }

    #[test]
    fn swaps_the_bundle_in_place() {
        let root = std::env::temp_dir().join(format!("tide-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let make_app = |dir: &Path, marker: &str| {
            let exe = dir.join("TIDE.app/Contents/MacOS");
            std::fs::create_dir_all(&exe).unwrap();
            std::fs::write(exe.join("TIDE"), marker).unwrap();
        };
        let installed = root.join("Applications");
        make_app(&installed, "old");
        let source = root.join("source");
        make_app(&source, "new");
        let zip_path = root.join("update.zip");
        let status = std::process::Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(source.join("TIDE.app"))
            .arg(&zip_path)
            .status()
            .unwrap();
        assert!(status.success());

        let bundle = installed.join("TIDE.app");
        let work = swap_in(&bundle, &std::fs::read(&zip_path).unwrap()).unwrap();
        let read = |p: &Path| std::fs::read_to_string(p.join("Contents/MacOS/TIDE")).unwrap();
        assert_eq!(read(&bundle), "new");
        assert_eq!(read(&work.join("old.app")), "old");
        assert_eq!(work.parent(), Some(installed.as_path()));
        assert!(!work.join("update.zip").exists());

        // A bad archive leaves the installed app alone and cleans up after itself.
        assert!(swap_in(&bundle, b"not a zip").is_err());
        assert_eq!(read(&bundle), "new");
        let leftovers = std::fs::read_dir(&installed)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".TIDE-update-"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
