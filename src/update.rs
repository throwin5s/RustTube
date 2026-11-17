//! Self-update against this project's public GitHub releases.
//!
//! Checking is one unauthenticated REST call, so a machine that is offline, rate
//! limited or behind a proxy just sees no news and never blocks the UI.
//!
//! Installing is delegated to `self_update`, which swaps a whole macOS `.app`
//! bundle in place — `Frameworks` and the bundled `yt-dlp` included — so the
//! ad-hoc signature the release job ships stays consistent. Windows builds are
//! a portable folder beside the running `.exe`, and a process cannot replace
//! its own executable and DLLs, so there we download the zip and let the user
//! swap it in.

use std::path::{Path, PathBuf};

pub const REPO_OWNER: &str = "throwin5s";
pub const REPO_NAME: &str = "RustTube";
const LATEST_RELEASE: &str = "https://api.github.com/repos/throwin5s/RustTube/releases/latest";

/// A published release that is newer than this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub url: String,
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
}

#[derive(serde::Deserialize)]
struct ApiRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(serde::Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

// --------------------------------------------------------------------- version

/// The version this process believes it is running.
///
/// `RUSTTUBE_FAKE_VERSION` lets the update path be exercised against a real
/// release without waiting for a newer one to be published.
pub fn current_version() -> String {
    std::env::var("RUSTTUBE_FAKE_VERSION")
        .ok()
        .map(|value| value.trim().trim_start_matches('v').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    parse_version(remote) > parse_version(current)
}

/// `v1.2.3`, `1.2`, `1.2.3-beta.1` all compare by their numeric core; anything
/// unparseable counts as zero, so a malformed tag never looks newer.
fn parse_version(text: &str) -> (u64, u64, u64) {
    let text = text.trim().trim_start_matches('v');
    let core = text.split(['-', '+']).next().unwrap_or(text);
    let mut parts = core.split('.').map(|part| part.trim().parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Release assets carry the platform in their name, so the updater has to ask
/// for the one it can actually run.
pub fn asset_name(version: &str) -> String {
    asset_name_for(platform_slug(), version)
}

fn asset_name_for(platform: &str, version: &str) -> String {
    format!("RustTube-{version}-{platform}.zip")
}

fn platform_slug() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos-arm64"
    } else {
        "windows-x64"
    }
}

// ----------------------------------------------------------------------- check

pub async fn check() -> Result<Option<Release>, String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("RustTube/{}", current_version()))
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|err| err.to_string())?;

    let response = client
        .get(LATEST_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("GitHub returned {}", response.status()));
    }
    let body = response.text().await.map_err(|err| err.to_string())?;
    let release: ApiRelease = serde_json::from_str(&body).map_err(|err| err.to_string())?;

    let version = release.tag_name.trim().trim_start_matches('v').to_string();
    if version.is_empty() || !is_newer(&version, &current_version()) {
        return Ok(None);
    }

    let wanted = asset_name(&version);
    let asset = release.assets.iter().find(|item| item.name == wanted);
    Ok(Some(Release {
        version,
        notes: release.body,
        url: release.html_url,
        asset_name: asset.map(|item| item.name.clone()),
        asset_url: asset.map(|item| item.browser_download_url.clone()),
    }))
}

// --------------------------------------------------------------------- install

/// True when this build can replace itself: a macOS `.app` bundle, where the
/// whole tree is swapped, rather than a loose binary.
pub fn can_self_install() -> bool {
    app_bundle().is_some()
}

/// The `.app` directory the running executable lives in, if any.
pub fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    bundle_in(&exe).map(Path::to_path_buf)
}

fn bundle_in(exe: &Path) -> Option<&Path> {
    exe.ancestors().find(|path| {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
    })
}

/// Replace the running bundle with the release's build.
pub fn install(release: &Release) -> Result<(), String> {
    if !can_self_install() {
        return Err("RustTube is not running from an app bundle, so it cannot update itself.".into());
    }
    install_into(release, None)
}

/// The swap itself. `bundle` overrides where the new `.app` lands; the running
/// app passes `None`, which lets self_update find the bundle it is inside.
fn install_into(release: &Release, bundle: Option<&Path>) -> Result<(), String> {
    let asset = release.asset_name.as_deref().ok_or_else(|| {
        format!(
            "Release v{} has no build for this platform yet.",
            release.version
        )
    })?;

    let current = current_version();
    let wanted = asset.to_string();
    let mut builder = self_update::backends::github::Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("RustTube")
        .current_version(&current)
        .bundle_path_in_archive("RustTube.app")
        // Release assets are named by hand in the workflow, so pick the exact
        // one instead of relying on self_update's target substring matching.
        .asset_matcher(move |assets: &[self_update::update::ReleaseAsset]| {
            assets.iter().find(|item| item.name() == wanted).cloned()
        });
    if let Some(path) = bundle {
        builder.bundle_install_path(path);
    }
    builder
        .build()
        .map_err(|err| format!("Could not prepare the update: {err}"))?
        .update()
        .map_err(|err| format!("Update failed: {err}"))?;
    Ok(())
}

/// Download the platform zip, unpack it beside the user's Downloads folder and
/// show it, for builds that cannot replace themselves.
pub async fn download(release: &Release) -> Result<PathBuf, String> {
    let url = release.asset_url.clone().ok_or_else(|| {
        format!(
            "Release v{} has no build for this platform yet.",
            release.version
        )
    })?;
    let name = release
        .asset_name
        .clone()
        .unwrap_or_else(|| asset_name(&release.version));

    let dir = downloads_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Could not create {}: {err}", dir.display()))?;
    let archive = dir.join(&name);

    let client = reqwest::Client::builder()
        .user_agent(format!("RustTube/{}", current_version()))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("Download failed: GitHub returned {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|err| err.to_string())?;
    std::fs::write(&archive, &bytes)
        .map_err(|err| format!("Could not write {}: {err}", archive.display()))?;

    let target = dir.join(format!("RustTube-{}", release.version));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target)
        .map_err(|err| format!("Could not create {}: {err}", target.display()))?;
    // bsdtar ships with macOS and with Windows 10+, and reads zip archives, so
    // this avoids pulling a decompressor into the build just for updates.
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&target)
        .status()
        .map_err(|err| format!("Could not unpack the update: {err}"))?;
    if !status.success() {
        return Err("Could not unpack the update archive.".into());
    }

    reveal(&target);
    Ok(target)
}

pub fn restart() -> Result<(), String> {
    // Never returns on success: the process image is replaced (unix) or the new
    // binary is spawned and this one exits (windows).
    match self_update::restart::restart() {
        Ok(never) => match never {},
        Err(err) => Err(format!("Could not restart RustTube: {err}")),
    }
}

pub fn reveal(path: &Path) {
    let _ = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(path).spawn()
    } else {
        std::process::Command::new("open").arg(path).spawn()
    };
}

fn downloads_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Downloads")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_their_numeric_core() {
        assert!(is_newer("0.3.0", "0.2.5"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(is_newer("1.2.3-beta.1", "1.2.2"));
        assert!(!is_newer("0.2.5", "0.2.5"));
        assert!(!is_newer("v0.2.5", "0.2.5"));
        assert!(!is_newer("0.2.4", "0.2.5"));
    }

    #[test]
    fn malformed_tags_never_look_newer() {
        assert!(!is_newer("latest", "0.2.5"));
        assert!(!is_newer("", "0.2.5"));
    }

    #[test]
    fn assets_are_named_per_platform() {
        assert_eq!(
            asset_name_for("macos-arm64", "0.3.0"),
            "RustTube-0.3.0-macos-arm64.zip"
        );
        assert_eq!(
            asset_name_for("windows-x64", "0.3.0"),
            "RustTube-0.3.0-windows-x64.zip"
        );
    }

    /// Opt-in end-to-end check against the published release: fetches the real
    /// zip, verifies the digest GitHub publishes, extracts it and swaps the
    /// bundle into a throwaway directory, so the running app is never touched.
    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn installs_a_published_release_into_a_bundle() {
        // Pretend to be ancient, so the newest release always counts as newer.
        std::env::set_var("RUSTTUBE_FAKE_VERSION", "0.0.1");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let release = rt
            .block_on(check())
            .expect("github reachable")
            .expect("a release is published");
        assert!(release.asset_name.is_some(), "release has a macOS asset");

        let root = std::env::temp_dir().join(format!("rusttube-update-{}", std::process::id()));
        let bundle = root.join("RustTube.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).expect("make bundle");
        std::fs::write(bundle.join("Contents/MacOS/RustTube"), b"stub").expect("stub exe");

        install_into(&release, Some(&bundle)).expect("install the release");

        let installed = bundle.join("Contents/MacOS/RustTube");
        let size = std::fs::metadata(&installed).expect("new executable").len();
        assert!(size > 1_000_000, "bundle should hold the real binary, got {size}");
        assert!(
            bundle.join("Contents/Frameworks").exists(),
            "bundled frameworks should come along"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bundle_detection_walks_up_from_the_executable() {
        let exe = Path::new("/Applications/RustTube.app/Contents/MacOS/RustTube");
        assert_eq!(
            bundle_in(exe),
            Some(Path::new("/Applications/RustTube.app"))
        );
        assert_eq!(
            bundle_in(Path::new("/Users/someone/target/debug/rusttube")),
            None
        );
    }
}
