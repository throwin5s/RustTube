//! Self-update against this project's public GitHub releases.
//!
//! Checking is one unauthenticated REST call, so a machine that is offline, rate
//! limited or behind a proxy just sees no news and never blocks the UI.
//!
//! On macOS installing is delegated to `self_update`, which swaps a whole
//! `.app` bundle in place — `Frameworks` and the bundled `yt-dlp` included — so
//! the ad-hoc signature the release job ships stays consistent.
//!
//! Windows cannot replace a running `.exe` or its loaded DLLs. There the update
//! is unpacked in a temp folder and a tiny PowerShell helper waits for this
//! process to exit, copies the new files over the folder RustTube.exe was
//! launched from (not a new sibling folder), and starts the new build.
//!
//! That hand-over is fail-safe: the helper touches a ready file before it waits,
//! and this process only quits once it sees that file — a helper that never
//! started means the app stays open. If the copy does fail, the helper puts the
//! previously installed files back and starts them, so a failed update never
//! ends with nothing running.

use std::path::{Path, PathBuf};

const WINDOWS_EXE: &str = "RustTube.exe";

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

/// True when this build can replace itself: a macOS `.app` bundle, or the
/// packaged `RustTube.exe` on Windows, which delegates the swap to a helper
/// once this process exits.
pub fn can_self_install() -> bool {
    if cfg!(windows) {
        windows_install_dir().is_some()
    } else {
        app_bundle().is_some()
    }
}

/// The folder the packaged Windows build runs from. The name is checked
/// exactly so a development build (`rusttube.exe` in `target/debug`) can never
/// overwrite itself with a release.
fn windows_install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if exe.file_name()? != std::ffi::OsStr::new(WINDOWS_EXE) {
        return None;
    }
    exe.parent().map(Path::to_path_buf)
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
    #[cfg(windows)]
    {
        install_windows(release)
    }
    #[cfg(not(windows))]
    {
        if !can_self_install() {
            return Err(
                "RustTube is not running from an app bundle, so it cannot update itself.".into(),
            );
        }
        install_into(release, None)
    }
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
        // A GUI app has no terminal to answer self_update's "continue?" prompt.
        .no_confirm(true)
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

/// How long the app waits for the helper to prove it started. PowerShell can be
/// slow to come up on a cold or busy machine, but waiting forever would leave
/// the update screen hanging with no way back.
#[cfg_attr(not(windows), allow(dead_code))]
const HELPER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How often the ready file is checked while waiting.
#[cfg_attr(not(windows), allow(dead_code))]
const HELPER_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The handshake: the helper writes its ready file before it waits for this
/// process, and the app only quits after seeing it. `alive` reports whether the
/// helper is still running, so one that exits at once fails fast instead of
/// burning the whole timeout.
///
/// A helper that cannot start is not a reason to close: the files would never be
/// replaced, and the user would be left with no window and no update.
#[cfg_attr(not(windows), allow(dead_code))]
fn await_helper_ready(
    ready: &Path,
    timeout: std::time::Duration,
    mut alive: impl FnMut() -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if ready.exists() {
            return true;
        }
        if !alive() {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(HELPER_POLL);
    }
}

/// Unpack the release beside a temp folder, then hand the swap to a helper that
/// runs after this process has exited. The new files land in the same folder as
/// the running `RustTube.exe`; nothing is left lying around as a second install.
#[cfg(windows)]
fn install_windows(release: &Release) -> Result<(), String> {
    let install = windows_install_dir().ok_or_else(|| {
        "RustTube is not running as RustTube.exe, so it cannot replace itself.".to_string()
    })?;
    let url = release.asset_url.clone().ok_or_else(|| {
        format!(
            "Release v{} has no Windows build yet.",
            release.version
        )
    })?;
    let name = release
        .asset_name
        .clone()
        .unwrap_or_else(|| asset_name(&release.version));

    // Fail while the app is still open if the install folder cannot be written;
    // otherwise the helper would exit and leave the user with no window at all.
    let probe = install.join(".rusttube-update-probe");
    std::fs::write(&probe, b"ok").map_err(|err| {
        format!(
            "Cannot write to {}. Move RustTube somewhere you can write, then update again ({err}).",
            install.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe);

    let work = std::env::temp_dir().join(format!("RustTube-update-{}", release.version));
    // A previous attempt may have left an archive or a half-unpacked folder.
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)
        .map_err(|err| format!("Could not create {}: {err}", work.display()))?;

    let archive = work.join(&name);
    download_blocking(&url, &archive)?;

    let unpacked = work.join("unpacked");
    std::fs::create_dir_all(&unpacked)
        .map_err(|err| format!("Could not create {}: {err}", unpacked.display()))?;
    // tar.exe ships with Windows 10+, and reads the zip GitHub produces.
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpacked)
        .status()
        .map_err(|err| format!("Could not unpack the update: {err}"))?;
    if !status.success() {
        return Err("Could not unpack the update archive.".into());
    }

    let staged = staged_root(&unpacked).ok_or_else(|| {
        "The update archive does not contain RustTube.exe.".to_string()
    })?;
    let ready = work.join("helper-ready");
    let backup = work.join("previous");
    let plan = SwapPlan::new(&staged, &install, &backup, &ready, std::process::id());
    let script = work.join("install-update.ps1");
    write_swap_script(&script, &plan)?;

    let mut command = std::process::Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script);
    // The app is a GUI process; do not flash a console window for the helper.
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|err| format!("Could not start the update helper: {err}"))?;

    // Wait for the helper to say it is running before reporting success, because
    // success is what makes the app quit. A helper that dies on start-up, or
    // never signals, means no swap is coming: keep the window open instead.
    let started = await_helper_ready(&ready, HELPER_START_TIMEOUT, || {
        child
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(true)
    });
    if !started {
        let _ = child.kill();
        let _ = child.wait();
        // A half-started helper must not be able to swap files later, so the
        // files it would copy are removed with it.
        let _ = std::fs::remove_dir_all(&work);
        return Err(
            "The update helper did not start, so RustTube kept running. Try updating again."
                .to_string(),
        );
    }
    Ok(())
}

/// Blocking download used by the Windows installer, which runs on its own
/// thread while the UI shows "Installing".
#[cfg(windows)]
fn download_blocking(url: &str, path: &Path) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("RustTube/{}", current_version()))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|err| err.to_string())?;
    let mut response = client.get(url).send().map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Download failed: GitHub returned {}",
            response.status()
        ));
    }
    let mut file = std::fs::File::create(path)
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;
    response
        .copy_to(&mut file)
        .map_err(|err| format!("Could not save {}: {err}", path.display()))?;
    Ok(())
}

/// The folder in an unpacked release that actually holds `RustTube.exe`. The
/// release zip has one `RustTube/` wrapper, but a future layout can nest a
/// little without breaking the update.
#[cfg_attr(not(windows), allow(dead_code))]
fn staged_root(unpacked: &Path) -> Option<PathBuf> {
    fn find(dir: &Path, depth: u8) -> Option<PathBuf> {
        if dir.join(WINDOWS_EXE).is_file() {
            return Some(dir.to_path_buf());
        }
        if depth == 0 {
            return None;
        }
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        subdirs.sort();
        subdirs.into_iter().find_map(|sub| find(&sub, depth - 1))
    }
    find(unpacked, 3)
}

/// Everything the generated helper needs to know. Grouped so the script can be
/// built, and tested, without running it.
#[cfg_attr(not(windows), allow(dead_code))]
struct SwapPlan<'a> {
    /// Unpacked release to copy from.
    staging: &'a Path,
    /// The folder the running `RustTube.exe` lives in.
    install: &'a Path,
    /// Holds the files that are installed now until the copy has worked.
    backup: &'a Path,
    /// Touched by the helper before it waits; the app quits once it exists.
    ready: &'a Path,
    /// Process to wait for. Zero means it is already gone (tests only).
    pid: u32,
    /// Start RustTube again afterwards. Off only in tests.
    launch: bool,
    /// Copy attempts, and the pause between them: antivirus or a slow shutdown
    /// can hold a file for a moment, so retrying is routine.
    attempts: u32,
    pause: std::time::Duration,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<'a> SwapPlan<'a> {
    fn new(
        staging: &'a Path,
        install: &'a Path,
        backup: &'a Path,
        ready: &'a Path,
        pid: u32,
    ) -> Self {
        Self {
            staging,
            install,
            backup,
            ready,
            pid,
            launch: true,
            attempts: 60,
            pause: std::time::Duration::from_millis(500),
        }
    }
}

/// The helper script, with placeholders substituted instead of formatted so the
/// PowerShell braces stay readable.
const SWAP_SCRIPT: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
# Handshake: the app waits for this file, so it only quits once the helper is
# really running and the swap is going to happen.
Set-Content -LiteralPath @@READY@@ -Value 'ready'
# RustTube.exe and libmpv are still loaded by the old process; wait for it to go.
@@WAIT@@
Start-Sleep -Milliseconds 500
$src = @@SRC@@
$dst = @@DST@@
$exe = @@EXE@@
$backup = @@BACKUP@@
# Keep what is installed now, so a copy that stops halfway can be undone.
New-Item -ItemType Directory -Path $backup -Force | Out-Null
Copy-Item -Path (Join-Path $dst '*') -Destination $backup -Recurse -Force
$backed = $?
$copied = $false
for ($i = 0; $i -lt @@ATTEMPTS@@; $i++) {
    # Nothing left to copy from: retrying cannot help.
    if (-not (Test-Path -LiteralPath $src)) { break }
    Copy-Item -Path (Join-Path $src '*') -Destination $dst -Recurse -Force
    if ($?) { $copied = $true; break }
    Start-Sleep -Milliseconds @@PAUSE@@
}
if (-not $copied) {
    # Put the old files back, then reopen them: a failed update has to leave a
    # working app running, never an empty folder.
    if ($backed) {
        for ($i = 0; $i -lt @@ATTEMPTS@@; $i++) {
            Copy-Item -Path (Join-Path $backup '*') -Destination $dst -Recurse -Force
            if ($?) { break }
            Start-Sleep -Milliseconds @@PAUSE@@
        }
    }
    for ($i = 0; $i -lt 10; $i++) {
        if (-not (Test-Path -LiteralPath $exe)) { break }
        @@START@@
        if ($?) { break }
        Start-Sleep -Milliseconds 500
    }
    exit 1
}
for ($i = 0; $i -lt 10; $i++) {
    if (-not (Test-Path -LiteralPath $exe)) { break }
    @@START@@
    if ($?) { break }
    Start-Sleep -Milliseconds 500
}
Start-Sleep -Milliseconds 800
Remove-Item -LiteralPath $src -Recurse -Force
Remove-Item -LiteralPath $backup -Recurse -Force
"#;

/// Write the PowerShell helper that performs the in-place swap after this
/// process exits. Kept separate from running it so the script can be tested.
#[cfg_attr(not(windows), allow(dead_code))]
fn write_swap_script(script: &Path, plan: &SwapPlan) -> Result<(), String> {
    // The BOM tells Windows PowerShell 5.1 the file is UTF-8; without it an
    // install path like `C:\Users\José` would be read as mojibake and the
    // handshake file would be written somewhere the app never looks.
    let body = format!("\u{feff}{}", swap_script(plan));
    std::fs::write(script, body)
        .map_err(|err| format!("Could not write {}: {err}", script.display()))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn swap_script(plan: &SwapPlan) -> String {
    let install = ps_quote(plan.install);
    let exe = ps_quote(&plan.install.join(WINDOWS_EXE));
    let start = if plan.launch {
        format!("Start-Process -FilePath {exe} -WorkingDirectory {install}")
    } else {
        String::new()
    };
    let wait = if plan.pid == 0 {
        String::new()
    } else {
        format!(
            "try {{ Wait-Process -Id {} -Timeout 90 -ErrorAction SilentlyContinue }} catch {{}}",
            plan.pid
        )
    };
    SWAP_SCRIPT
        .replace("@@READY@@", &ps_quote(plan.ready))
        .replace("@@WAIT@@", &wait)
        .replace("@@SRC@@", &ps_quote(plan.staging))
        .replace("@@DST@@", &install)
        .replace("@@EXE@@", &exe)
        .replace("@@BACKUP@@", &ps_quote(plan.backup))
        .replace("@@ATTEMPTS@@", &plan.attempts.max(1).to_string())
        .replace("@@PAUSE@@", &plan.pause.as_millis().to_string())
        .replace("@@START@@", &start)
}

/// PowerShell single-quoted literal: only `'` needs escaping (as `''`).
#[cfg_attr(not(windows), allow(dead_code))]
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
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

    #[test]
    fn finds_the_exe_inside_the_release_wrapper() {
        let root = std::env::temp_dir().join(format!("rusttube-staged-{}", std::process::id()));
        let inner = root.join("RustTube");
        std::fs::create_dir_all(&inner).expect("staging");
        std::fs::write(inner.join(WINDOWS_EXE), b"exe").expect("exe");

        assert_eq!(staged_root(&root), Some(inner.clone()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swap_script_replaces_the_install_folder_in_place() {
        let plan = SwapPlan::new(
            Path::new(r"C:\Temp\RustTube-update\RustTube"),
            Path::new(r"C:\Apps\RustTube"),
            Path::new(r"C:\Temp\RustTube-update\previous"),
            Path::new(r"C:\Temp\RustTube-update\helper-ready"),
            1234,
        );
        let script = swap_script(&plan);
        assert!(
            !script.contains("@@"),
            "every placeholder is substituted before the script is run"
        );
        assert!(script.contains("Wait-Process -Id 1234"));
        assert!(script.contains("Copy-Item -Path (Join-Path $src '*')"));
        assert!(script.contains(r"C:\Apps\RustTube"));
        assert!(script.contains("Start-Process -FilePath"));
        assert!(script.contains(WINDOWS_EXE));
        assert!(script.contains("Copy-Item -Path (Join-Path $backup '*')"));
    }

    /// The handshake is only a handshake if the helper signals first: with the
    /// wait first, the app could quit while the helper was still starting up.
    #[test]
    fn swap_script_signals_before_it_waits_for_the_app() {
        let plan = SwapPlan::new(
            Path::new(r"C:\Temp\RustTube-update\RustTube"),
            Path::new(r"C:\Apps\RustTube"),
            Path::new(r"C:\Temp\RustTube-update\previous"),
            Path::new(r"C:\Temp\RustTube-update\helper-ready"),
            1234,
        );
        let script = swap_script(&plan);
        let ready = script
            .find(r"Set-Content -LiteralPath 'C:\Temp\RustTube-update\helper-ready'")
            .expect("helper touches its ready file");
        let wait = script.find("Wait-Process -Id 1234").expect("helper waits");
        assert!(
            ready < wait,
            "the helper must signal before it waits, so the app never quits on a helper that is not running"
        );
    }

    /// A copy that fails has to put the previously installed files back and start
    /// them: quitting into a half-copied folder would leave nothing to reopen.
    #[test]
    fn swap_script_rolls_back_and_reopens_the_app_when_the_copy_fails() {
        let plan = SwapPlan::new(
            Path::new(r"C:\Temp\RustTube-update\RustTube"),
            Path::new(r"C:\Apps\RustTube"),
            Path::new(r"C:\Temp\RustTube-update\previous"),
            Path::new(r"C:\Temp\RustTube-update\helper-ready"),
            1234,
        );
        let script = swap_script(&plan);
        let failure = script.find("if (-not $copied) {").expect("failure branch");
        let branch = &script[failure..];
        let restore = branch
            .find("Copy-Item -Path (Join-Path $backup '*')")
            .expect("restore the old files");
        let reopen = branch
            .find("Start-Process -FilePath")
            .expect("reopen the app");
        assert!(restore < reopen, "files are put back before they are started");
        assert!(branch.contains("exit 1"), "a failed copy reports failure");
    }

    /// The app quits only once the ready file appears, which proves the helper is
    /// running and the swap is going to happen.
    #[test]
    fn helper_handshake_waits_for_the_ready_file() {
        let root = std::env::temp_dir().join(format!("rusttube-ready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("handshake dir");
        let ready = root.join("helper-ready");
        let writer = ready.clone();
        let helper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            std::fs::write(&writer, "ready").expect("ready file");
        });

        let start = std::time::Instant::now();
        assert!(await_helper_ready(
            &ready,
            std::time::Duration::from_secs(5),
            || true
        ));
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(100),
            "the app should wait for the helper to signal"
        );
        helper.join().expect("helper thread");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A helper that exits without signalling must fail immediately: the app has
    /// to know there is nobody to do the swap, and it may not sit on the update
    /// screen for the whole timeout.
    #[test]
    fn helper_handshake_gives_up_when_the_helper_exits() {
        let ready = std::env::temp_dir().join(format!("rusttube-gone-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let start = std::time::Instant::now();
        assert!(!await_helper_ready(
            &ready,
            std::time::Duration::from_secs(30),
            || false
        ));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "a helper that exited should not be waited for"
        );
    }

    #[test]
    fn helper_handshake_times_out_without_a_signal() {
        let ready = std::env::temp_dir().join(format!("rusttube-silent-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        assert!(!await_helper_ready(
            &ready,
            std::time::Duration::from_millis(150),
            || true
        ));
    }

    #[test]
    fn swap_script_escapes_apostrophes_in_paths() {
        let plan = SwapPlan {
            launch: false,
            ..SwapPlan::new(
                Path::new("/tmp/O'Brien/new"),
                Path::new("/Applications/O'Brien/RustTube"),
                Path::new("/tmp/O'Brien/previous"),
                Path::new("/tmp/O'Brien/ready"),
                7,
            )
        };
        let script = swap_script(&plan);
        assert!(script.contains("'/Applications/O''Brien/RustTube'"));
        assert!(!script.contains("Start-Process"));
    }

    /// Runs the generated helper for real. These are the Windows CI checks for
    /// the in-place swap, its handshake and its rollback.
    #[cfg(windows)]
    fn run_helper(script: &Path) -> std::process::ExitStatus {
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .status()
            .expect("powershell")
    }

    /// Runs the generated helper for real, with the wait disabled and no
    /// relaunch, and checks that the files in the existing folder were replaced,
    /// the app was told the helper started, and the staging folder was cleaned
    /// up.
    #[cfg(windows)]
    #[test]
    fn windows_helper_replaces_files_in_the_existing_folder() {
        let root = std::env::temp_dir().join(format!("rusttube-swap-{}", std::process::id()));
        let install = root.join("install");
        let staging = root.join("staging").join("RustTube");
        std::fs::create_dir_all(&install).expect("install dir");
        std::fs::create_dir_all(&staging).expect("staging dir");
        std::fs::write(install.join(WINDOWS_EXE), b"old exe").expect("old exe");
        std::fs::write(install.join("libmpv-2.dll"), b"old dll").expect("old dll");
        std::fs::write(staging.join(WINDOWS_EXE), b"new exe").expect("new exe");
        std::fs::write(staging.join("libmpv-2.dll"), b"new dll").expect("new dll");
        std::fs::write(staging.join("yt-dlp.exe"), b"new yt-dlp").expect("yt-dlp");

        let ready = root.join("helper-ready");
        let backup = root.join("previous");
        let plan = SwapPlan {
            launch: false,
            ..SwapPlan::new(&staging, &install, &backup, &ready, 0)
        };
        let script = root.join("swap.ps1");
        write_swap_script(&script, &plan).expect("script");
        let status = run_helper(&script);
        assert!(status.success(), "helper should exit successfully");
        assert!(
            ready.exists(),
            "the helper must touch its ready file, or the app never knows to quit"
        );
        assert_eq!(
            std::fs::read(install.join(WINDOWS_EXE)).unwrap(),
            b"new exe"
        );
        assert_eq!(
            std::fs::read(install.join("libmpv-2.dll")).unwrap(),
            b"new dll"
        );
        assert_eq!(
            std::fs::read(install.join("yt-dlp.exe")).unwrap(),
            b"new yt-dlp"
        );
        assert!(!staging.exists(), "staging folder should be removed");
        assert!(!backup.exists(), "backup should be removed once copied");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A copy that cannot happen has to report failure and leave the folder as
    /// it was, so reopening the old build still works. The restore branch
    /// itself is checked by `swap_script_rolls_back_and_reopens_the_app_when_the_copy_fails`.
    #[cfg(windows)]
    #[test]
    fn windows_helper_keeps_the_install_intact_when_the_copy_fails() {
        let root = std::env::temp_dir().join(format!("rusttube-rollback-{}", std::process::id()));
        let install = root.join("install");
        std::fs::create_dir_all(&install).expect("install dir");
        std::fs::write(install.join(WINDOWS_EXE), b"old exe").expect("old exe");
        std::fs::write(install.join("libmpv-2.dll"), b"old dll").expect("old dll");
        // Nothing was unpacked, so there is nothing to copy from.
        let staging = root.join("staging").join("RustTube");

        let ready = root.join("helper-ready");
        let backup = root.join("previous");
        let plan = SwapPlan {
            launch: false,
            attempts: 2,
            pause: std::time::Duration::from_millis(20),
            ..SwapPlan::new(&staging, &install, &backup, &ready, 0)
        };
        let script = root.join("swap.ps1");
        write_swap_script(&script, &plan).expect("script");
        let status = run_helper(&script);
        assert!(!status.success(), "a failed copy reports failure");
        assert!(ready.exists(), "the app is told the helper started either way");
        assert_eq!(
            std::fs::read(install.join(WINDOWS_EXE)).unwrap(),
            b"old exe",
            "the previous build must still be there to reopen"
        );
        assert_eq!(
            std::fs::read(install.join("libmpv-2.dll")).unwrap(),
            b"old dll"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
