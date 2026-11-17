//! Cross-device synchronisation.
//!
//! RustTube stores one JSON snapshot in the storage you already have: the folder
//! a cloud client (iCloud Drive, Google Drive, OneDrive, Dropbox, pCloud, Box)
//! keeps in sync, any other synced folder, or a WebDAV server. There is no
//! RustTube account and no server of ours in the middle.
//!
//! Cloud mode stores the *provider*, not a path, so the same setup means the
//! same place on macOS and Windows: whichever folder that client mounts, the
//! snapshot lives in `RustTube/rusttube-sync.json` inside it.
//!
//! A sync is always: pull -> merge into SQLite -> write merged snapshot back.
//! Merging is record level (see `db::merge_payload`), so adding a subscription
//! on your laptop and watching a video on your desktop both survive.

use crate::db;
use reqwest::Client;
use rusqlite::Connection;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;

pub const SNAPSHOT_NAME: &str = "rusttube-sync.json";
/// Folder RustTube owns inside a cloud provider's mount.
const CLOUD_SUBFOLDER: &str = "RustTube";

#[derive(Clone, Debug, Default)]
pub struct SyncSettings {
    /// "off" | "cloud" | "folder" | "webdav"
    pub mode: String,
    /// Provider key for cloud mode, e.g. "gdrive". Portable across platforms.
    pub cloud: String,
    pub folder: String,
    pub webdav_url: String,
    pub webdav_user: String,
    pub webdav_pass: String,
    pub auto: bool,
}

#[derive(Serialize)]
pub struct SyncReport {
    pub mode: String,
    pub target: String,
    pub records_applied: usize,
    pub subscriptions: usize,
    pub history: usize,
    pub watch_stats: usize,
    pub synced_at: i64,
    pub bytes: usize,
}

#[derive(Serialize)]
pub struct SyncInfo {
    pub mode: String,
    pub cloud: String,
    pub folder: String,
    pub webdav_url: String,
    pub webdav_user: String,
    pub has_password: bool,
    pub auto: bool,
    pub target: String,
    pub enabled: bool,
    pub last_sync_at: i64,
    pub last_error: String,
}

#[derive(Serialize)]
pub struct SyncPreset {
    pub label: String,
    pub path: String,
    pub exists: bool,
}

impl SyncSettings {
    pub fn load(conn: &Connection) -> Self {
        let get = |key: &str| db::get_setting(conn, key).unwrap_or_default();
        let mode = get("sync_mode");
        SyncSettings {
            mode: if mode.is_empty() { "off".into() } else { mode },
            cloud: get("sync_cloud"),
            folder: get("sync_folder"),
            webdav_url: get("sync_webdav_url"),
            webdav_user: get("sync_webdav_user"),
            webdav_pass: get("sync_webdav_pass"),
            auto: get("sync_auto") == "1",
        }
    }

    pub fn save(&self, conn: &Connection) -> rusqlite::Result<()> {
        db::set_setting(conn, "sync_mode", &self.mode)?;
        db::set_setting(conn, "sync_cloud", &self.cloud)?;
        db::set_setting(conn, "sync_folder", &self.folder)?;
        db::set_setting(conn, "sync_webdav_url", &self.webdav_url)?;
        db::set_setting(conn, "sync_webdav_user", &self.webdav_user)?;
        db::set_setting(conn, "sync_webdav_pass", &self.webdav_pass)?;
        db::set_setting(conn, "sync_auto", if self.auto { "1" } else { "0" })?;
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        match self.mode.as_str() {
            "cloud" => cloud_folder(&self.cloud).is_some(),
            "folder" => !self.folder.trim().is_empty(),
            "webdav" => !self.webdav_url.trim().is_empty(),
            _ => false,
        }
    }

    pub fn target(&self) -> String {
        match self.mode.as_str() {
            "cloud" => cloud_folder(&self.cloud)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| format!("{} (desktop app not found)", cloud_label(&self.cloud))),
            "folder" => expand_path(&self.folder).map(|p| p.display().to_string()).unwrap_or_default(),
            "webdav" => self.webdav_url.trim().to_string(),
            _ => String::new(),
        }
    }

    pub fn info(&self, conn: &Connection) -> SyncInfo {
        SyncInfo {
            mode: self.mode.clone(),
            cloud: self.cloud.clone(),
            folder: self.folder.clone(),
            webdav_url: self.webdav_url.clone(),
            webdav_user: self.webdav_user.clone(),
            has_password: !self.webdav_pass.is_empty(),
            auto: self.auto,
            target: self.target(),
            enabled: self.enabled(),
            last_sync_at: db::get_setting(conn, "sync_last_at")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0),
            last_error: db::get_setting(conn, "sync_last_error").unwrap_or_default(),
        }
    }
}

/// A cloud service whose desktop client keeps a normal folder (or mounted
/// drive) on this machine. Being signed in to one of these is the whole setup:
/// the snapshot lives in `RustTube/` inside it.
pub struct CloudMount {
    pub key: &'static str,
    pub label: &'static str,
    pub path: PathBuf,
}

pub fn detect_clouds() -> Vec<CloudMount> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let onedrive = std::env::var_os("OneDrive").map(PathBuf::from);
    discover(&home, onedrive.as_deref())
        .into_iter()
        .map(|(key, label, path)| CloudMount { key, label, path })
        .collect()
}

/// Where the snapshot goes for a provider key, or `None` when that client is not
/// installed (the desktop app also does the signing in).
pub fn cloud_folder(key: &str) -> Option<PathBuf> {
    let home = home_dir()?;
    let onedrive = std::env::var_os("OneDrive").map(PathBuf::from);
    cloud_folder_in(&home, onedrive.as_deref(), key)
}

fn cloud_folder_in(
    home: &std::path::Path,
    onedrive: Option<&std::path::Path>,
    key: &str,
) -> Option<PathBuf> {
    discover(home, onedrive)
        .into_iter()
        .find(|(found, _, _)| *found == key)
        .map(|(_, _, path)| path.join(CLOUD_SUBFOLDER))
}

pub fn cloud_label(key: &str) -> &'static str {
    match key {
        "gdrive" => "Google Drive",
        "onedrive" => "OneDrive",
        "icloud" => "iCloud Drive",
        "dropbox" => "Dropbox",
        "pcloud" => "pCloud",
        "box" => "Box",
        _ => "a cloud folder",
    }
}

pub fn presets() -> Vec<SyncPreset> {
    detect_clouds()
        .into_iter()
        .map(|mount| SyncPreset {
            label: mount.label.to_string(),
            path: mount.path.display().to_string(),
            exists: true,
        })
        .collect()
}

/// Every provider mount that exists right now, at most one per provider.
fn discover(
    home: &std::path::Path,
    onedrive: Option<&std::path::Path>,
) -> Vec<(&'static str, &'static str, PathBuf)> {
    fn add(
        found: &mut Vec<(&'static str, &'static str, PathBuf)>,
        key: &'static str,
        label: &'static str,
        path: PathBuf,
    ) {
        if path.is_dir() && !found.iter().any(|(existing, _, _)| *existing == key) {
            found.push((key, label, path));
        }
    }

    let mut found: Vec<(&'static str, &'static str, PathBuf)> = Vec::new();

    // iCloud Drive (macOS), iCloud for Windows.
    add(&mut found, "icloud", "iCloud Drive", home.join("Library/Mobile Documents/com~apple~CloudDocs"));
    add(&mut found, "icloud", "iCloud Drive", home.join("iCloudDrive"));
    // Dropbox and the older OneDrive/Google Drive locations.
    add(&mut found, "dropbox", "Dropbox", home.join("Dropbox"));
    if let Some(path) = onedrive {
        add(&mut found, "onedrive", "OneDrive", path.to_path_buf());
    }
    add(&mut found, "onedrive", "OneDrive", home.join("OneDrive"));
    add(&mut found, "gdrive", "Google Drive", home.join("Google Drive"));
    add(&mut found, "pcloud", "pCloud", home.join("pCloud Drive"));
    add(&mut found, "box", "Box", home.join("Box"));

    // Google Drive, OneDrive and Box for desktop each mount an account under
    // ~/Library/CloudStorage on macOS.
    for mount in child_dirs(&home.join("Library/CloudStorage")) {
        let Some(name) = mount.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("GoogleDrive-") {
            add(&mut found, "gdrive", "Google Drive", mount.join("My Drive"));
        } else if name.starts_with("OneDrive-") {
            add(&mut found, "onedrive", "OneDrive", mount);
        } else if name.starts_with("Box-") {
            add(&mut found, "box", "Box", mount);
        }
    }

    // On Windows those clients mount a virtual drive instead: Google Drive as a
    // letter holding "My Drive", pCloud as the P: drive.
    if cfg!(target_os = "windows") {
        for letter in b'C'..=b'Z' {
            let drive = PathBuf::from(format!("{}:\\", letter as char));
            add(&mut found, "gdrive", "Google Drive", drive.join("My Drive"));
        }
        add(&mut found, "pcloud", "pCloud", PathBuf::from("P:\\"));
    }

    // Stable order for the picker, independent of which path found the mount.
    found.sort_by_key(|(key, _, _)| match *key {
        "gdrive" => 0,
        "onedrive" => 1,
        "icloud" => 2,
        "dropbox" => 3,
        "pcloud" => 4,
        _ => 5,
    });
    found
}

fn child_dirs(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

pub async fn run(
    conn: &Mutex<Connection>,
    settings: &SyncSettings,
    device: &str,
) -> Result<SyncReport, String> {
    if !settings.enabled() {
        return Err("Sync is not configured. Pick a folder or a WebDAV server first.".into());
    }

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let remote = pull(settings, &http).await?;

    let (applied, snapshot, counts) = {
        let mut conn = conn.lock().map_err(|_| "database lock poisoned".to_string())?;
        let mut applied = 0usize;
        if let Some(raw) = remote.as_ref().filter(|raw| !raw.trim().is_empty()) {
            let incoming: db::SyncPayload = serde_json::from_str(raw)
                .map_err(|e| format!("The sync file is not valid JSON: {e}"))?;
            applied = db::merge_payload(&mut conn, &incoming)?.records_applied;
        }
        let payload = db::export_payload(&conn, device);
        let counts = (payload.subscriptions.len(), payload.history.len(), payload.watch_stats.len());
        let snapshot = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
        (applied, snapshot, counts)
    };

    push(settings, &http, &snapshot).await?;

    let synced_at = db::now_ms();
    if let Ok(conn) = conn.lock() {
        let _ = db::set_setting(&conn, "sync_last_at", &synced_at.to_string());
        let _ = db::set_setting(&conn, "sync_last_error", "");
    }

    Ok(SyncReport {
        mode: settings.mode.clone(),
        target: settings.target(),
        records_applied: applied,
        subscriptions: counts.0,
        history: counts.1,
        watch_stats: counts.2,
        synced_at,
        bytes: snapshot.len(),
    })
}

pub fn record_failure(conn: &Mutex<Connection>, message: &str) {
    let text = message.chars().take(400).collect::<String>();
    if let Ok(conn) = conn.lock() {
        let _ = db::set_setting(&conn, "sync_last_error", &text);
    }
}

async fn pull(settings: &SyncSettings, http: &Client) -> Result<Option<String>, String> {
    match settings.mode.as_str() {
        "folder" | "cloud" => {
            let path = snapshot_file(settings)?;
            match std::fs::read_to_string(&path) {
                Ok(text) => Ok(Some(text)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(format!("Could not read {}: {err}", path.display())),
            }
        }
        "webdav" => {
            let (_, file_url) = webdav_urls(settings)?;
            let mut request = http.get(&file_url).header("Accept", "application/json");
            if !settings.webdav_user.trim().is_empty() {
                request = request.basic_auth(&settings.webdav_user, Some(&settings.webdav_pass));
            }
            let response = request.send().await.map_err(|e| format!("WebDAV request failed: {e}"))?;
            match response.status().as_u16() {
                200..=299 => Ok(Some(response.text().await.map_err(|e| e.to_string())?)),
                404 => Ok(None),
                401 | 403 => Err("WebDAV rejected the credentials (401/403). Check the username and password.".into()),
                other => Err(format!("WebDAV server answered HTTP {other}")),
            }
        }
        _ => Err("Sync is turned off.".into()),
    }
}

async fn push(settings: &SyncSettings, http: &Client, body: &str) -> Result<(), String> {
    match settings.mode.as_str() {
        "folder" | "cloud" => {
            let path = snapshot_file(settings)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            }
            // Write then rename so a crash mid-write cannot truncate the snapshot.
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, body).map_err(|e| format!("Could not write {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &path)
                .map_err(|e| format!("Could not replace {}: {e}", path.display()))?;
            Ok(())
        }
        "webdav" => {
            let (dir_url, file_url) = webdav_urls(settings)?;
            let send = |url: String| {
                let mut request = http.put(&url).header("Content-Type", "application/json").body(body.to_string());
                if !settings.webdav_user.trim().is_empty() {
                    request = request.basic_auth(&settings.webdav_user, Some(&settings.webdav_pass));
                }
                async move { request.send().await }
            };

            let response = send(file_url.clone()).await.map_err(|e| format!("WebDAV upload failed: {e}"))?;
            let status = response.status().as_u16();
            if (200..=299).contains(&status) {
                return Ok(());
            }
            if status == 401 || status == 403 {
                return Err("WebDAV rejected the credentials (401/403). Check the username and password.".into());
            }
            if status == 409 || status == 404 {
                // Missing collection: create it, then retry once.
                let mut mkcol = http.request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &dir_url);
                if !settings.webdav_user.trim().is_empty() {
                    mkcol = mkcol.basic_auth(&settings.webdav_user, Some(&settings.webdav_pass));
                }
                let _ = mkcol.send().await;
                let retry = send(file_url).await.map_err(|e| format!("WebDAV upload failed: {e}"))?;
                if (200..=299).contains(&retry.status().as_u16()) {
                    return Ok(());
                }
                return Err(format!("WebDAV refused the upload (HTTP {})", retry.status().as_u16()));
            }
            Err(format!("WebDAV refused the upload (HTTP {status})"))
        }
        _ => Err("Sync is turned off.".into()),
    }
}

/// The file inside the synced folder. Cloud mode resolves the provider's mount
/// on this machine, so the same setting works on macOS and Windows.
fn snapshot_file(settings: &SyncSettings) -> Result<PathBuf, String> {
    let dir = match settings.mode.as_str() {
        "cloud" => cloud_folder(&settings.cloud).ok_or_else(|| {
            format!(
                "{} is not installed on this device. Sign in to its desktop app, or pick another provider.",
                cloud_label(&settings.cloud)
            )
        })?,
        _ => expand_path(&settings.folder)?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    Ok(dir.join(SNAPSHOT_NAME))
}

fn webdav_urls(settings: &SyncSettings) -> Result<(String, String), String> {
    let base = settings.webdav_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err("Set a WebDAV address first.".into());
    }
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return Err("The WebDAV address must start with https:// or http://".into());
    }
    if base.ends_with(".json") {
        let dir = base.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap_or_default();
        return Ok((dir, base));
    }
    Ok((base.clone(), format!("{base}/{SNAPSHOT_NAME}")))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Expand `~` and Windows-style `%VAR%` segments so users can paste portable paths.
pub fn expand_path(input: &str) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("No folder set.".into());
    }

    let mut expanded = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let mut name = String::new();
            let mut closed = false;
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '%' {
                    closed = true;
                    break;
                }
                name.push(next);
            }
            if closed && !name.is_empty() {
                match std::env::var(&name) {
                    Ok(value) => expanded.push_str(&value),
                    Err(_) => {
                        expanded.push('%');
                        expanded.push_str(&name);
                        expanded.push('%');
                    }
                }
            } else {
                expanded.push('%');
                expanded.push_str(&name);
            }
        } else {
            expanded.push(ch);
        }
    }

    if expanded == "~" || expanded.starts_with("~/") || expanded.starts_with("~\\") {
        let home = home_dir().ok_or_else(|| "Could not find your home folder.".to_string())?;
        let rest = expanded.trim_start_matches('~').trim_start_matches(['/', '\\']);
        return Ok(if rest.is_empty() { home } else { home.join(rest) });
    }

    Ok(PathBuf::from(expanded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_prefix() {
        let home = home_dir().expect("home");
        let expanded = expand_path("~/Documents/RustTube").unwrap();
        assert_eq!(expanded, home.join("Documents/RustTube"));
        assert_eq!(expand_path("~").unwrap(), home);
    }

    #[test]
    fn keeps_absolute_paths() {
        let path = if cfg!(target_os = "windows") { "C:\\Users\\me\\Sync" } else { "/tmp/rusttube-sync" };
        assert_eq!(expand_path(path).unwrap().display().to_string(), path);
    }

    #[test]
    fn rejects_empty_folder() {
        assert!(expand_path("   ").is_err());
    }

    #[tokio::test]
    async fn folder_sync_round_trip_between_two_devices() {
        let dir = std::env::temp_dir().join(format!("rusttube-sync-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let settings = SyncSettings {
            mode: "folder".into(),
            folder: dir.display().to_string(),
            ..Default::default()
        };

        let a = Mutex::new(db::open(&dir.join("a.db")).expect("db a"));
        let b = Mutex::new(db::open(&dir.join("b.db")).expect("db b"));

        {
            let conn = a.lock().unwrap();
            db::set_subscription(&conn, "UC1", "Channel One", "", true).unwrap();
            db::record_watch(
                &conn,
                &db::WatchUpdate {
                    video_id: "v1",
                    title: "One",
                    channel_id: "UC1",
                    channel_name: "Channel One",
                    duration: 300,
                    position: 100,
                    seconds: 100,
                    new_view: true,
                },
            )
            .unwrap();
        }

        let first = run(&a, &settings, "dev-a").await.expect("device a sync");
        assert_eq!(first.records_applied, 0, "nothing remote to merge yet");
        assert_eq!(first.subscriptions, 1);
        assert!(dir.join(SNAPSHOT_NAME).exists());

        let pulled = run(&b, &settings, "dev-b").await.expect("device b sync");
        assert_eq!(pulled.records_applied, 3, "one subscription, one history row, one stat row");
        {
            let conn = b.lock().unwrap();
            assert!(db::is_subscribed(&conn, "UC1"));
            assert_eq!(db::stats_overview(&conn).total_seconds, 100);
        }

        {
            let conn = b.lock().unwrap();
            db::record_watch(
                &conn,
                &db::WatchUpdate {
                    video_id: "v2",
                    title: "Two",
                    channel_id: "UC2",
                    channel_name: "Channel Two",
                    duration: 200,
                    position: 50,
                    seconds: 50,
                    new_view: true,
                },
            )
            .unwrap();
        }
        run(&b, &settings, "dev-b").await.expect("device b second sync");

        let merged = run(&a, &settings, "dev-a").await.expect("device a second sync");
        assert!(merged.records_applied >= 1, "device a should learn about device b's watch time");
        {
            let conn = a.lock().unwrap();
            let overview = db::stats_overview(&conn);
            assert_eq!(overview.total_seconds, 150, "watch time must add up, not double count");
            assert_eq!(overview.devices, 2);
        }

        // Running again with no new data is a no-op, which keeps auto-sync cheap.
        let idle = run(&a, &settings, "dev-a").await.expect("idle sync");
        assert_eq!(idle.records_applied, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finds_cloud_mounts_by_their_platform_paths() {
        let home = std::env::temp_dir().join(format!("rusttube-home-{}", uuid::Uuid::new_v4()));
        let mount = home.join("Library/CloudStorage/GoogleDrive-me@example.com/My Drive");
        std::fs::create_dir_all(&mount).expect("gdrive mount");
        std::fs::create_dir_all(home.join("pCloud Drive")).expect("pcloud mount");
        std::fs::create_dir_all(home.join("Box")).expect("box mount");

        let found = discover(&home, None);
        let keys: Vec<&str> = found.iter().map(|(key, _, _)| *key).collect();
        assert_eq!(keys, vec!["gdrive", "pcloud", "box"]);
        assert_eq!(found[0].2, mount);

        // A provider that is not installed is simply absent, not an error.
        assert!(!keys.contains(&"onedrive"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn keeps_one_mount_per_provider() {
        let home = std::env::temp_dir().join(format!("rusttube-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("Dropbox")).expect("dropbox");
        std::fs::create_dir_all(home.join("OneDrive")).expect("onedrive");
        let other = home.join("elsewhere");
        std::fs::create_dir_all(&other).expect("other onedrive");

        let found = discover(&home, Some(&other));
        let onedrives = found.iter().filter(|(key, _, _)| *key == "onedrive").count();
        assert_eq!(onedrives, 1, "OneDrive should resolve to one folder");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cloud_folder_lands_inside_the_provider_mount() {
        let home = std::env::temp_dir().join(format!("rusttube-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("OneDrive")).expect("onedrive");
        assert_eq!(
            cloud_folder_in(&home, None, "onedrive"),
            Some(home.join("OneDrive/RustTube"))
        );
        assert_eq!(cloud_folder_in(&home, None, "dropbox"), None, "not installed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cloud_mode_resolves_the_provider_folder() {
        let settings = SyncSettings {
            mode: "cloud".into(),
            cloud: "gdrive".into(),
            ..Default::default()
        };
        // Either the provider is installed here, or the mode reports itself
        // unavailable — never a path from another platform.
        assert_eq!(settings.enabled(), cloud_folder("gdrive").is_some());
        assert_eq!(cloud_label("gdrive"), "Google Drive");
        assert_eq!(cloud_label("unknown"), "a cloud folder");
    }

    #[test]
    fn builds_webdav_urls() {
        let mut settings = SyncSettings { mode: "webdav".into(), ..Default::default() };
        settings.webdav_url = "https://cloud.example.com/remote.php/dav/files/me/RustTube/".into();
        let (dir, file) = webdav_urls(&settings).unwrap();
        assert_eq!(dir, "https://cloud.example.com/remote.php/dav/files/me/RustTube");
        assert_eq!(file, "https://cloud.example.com/remote.php/dav/files/me/RustTube/rusttube-sync.json");

        settings.webdav_url = "https://cloud.example.com/dav/custom-name.json".into();
        let (_, file) = webdav_urls(&settings).unwrap();
        assert_eq!(file, "https://cloud.example.com/dav/custom-name.json");

        settings.webdav_url = "ftp://nope".into();
        assert!(webdav_urls(&settings).is_err());
    }
}
