//! Playback through a single long-lived mpv process.
//!
//! Starting a fresh mpv per video costs process startup, a socket handshake and
//! a full yt-dlp extraction inside mpv (measured ~7s to first frame). So one mpv
//! now runs in `--idle=yes` mode for the whole session: loading a video is a
//! `loadfile` of a direct stream URL, and stopping just returns it to idle.
//!
//! Two connections are used: a writer thread for commands, and a reader thread
//! that subscribes to `time-pos`, `duration`, `pause` and `eof-reached`. Keeping
//! them separate means a paused player (which emits no events) can still be
//! resumed, and the UI never blocks. The reader also applies a resume point once
//! mpv reports the file loaded, because a seek sent before that is dropped.

use crate::embed::{NowPlaying, Snapshot};
use serde_json::json;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(unix)]
mod ipc {
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};

    pub type Stream = UnixStream;

    pub fn connect(path: &Path) -> std::io::Result<Stream> {
        UnixStream::connect(path)
    }

    pub fn endpoint() -> PathBuf {
        std::env::temp_dir().join(format!("rusttube-mpv-{}.sock", std::process::id()))
    }

    pub fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
mod ipc {
    use std::fs::File;
    use std::path::{Path, PathBuf};

    pub type Stream = File;

    pub fn connect(path: &Path) -> std::io::Result<Stream> {
        File::options().read(true).write(true).open(path)
    }

    pub fn endpoint() -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\rusttube-mpv-{}", std::process::id()))
    }

    pub fn cleanup(_path: &Path) {}
}

use ipc::Stream;

enum Cmd {
    Load {
        video: String,
    },
    /// Must follow `file-loaded`: adding audio while the file is still loading
    /// makes mpv abort the load with "loading failed".
    AudioAdd(String),
    /// Let mpv resolve and play a YouTube URL with its own yt-dlp hook.
    LoadYouTube(String),
    SeekTo(f64),
    Pause(bool),
    Volume(i32),
    Speed(f64),
    Mute(bool),
    Stop,
    Quit,
}

pub struct Player {
    commands: Option<Sender<Cmd>>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Resume point waiting for `file-loaded`, then unconditional again.
    resume: Arc<Mutex<Option<f64>>>,
    /// Audio URL waiting for `file-loaded`, when the stream is split.
    pending_audio: Arc<Mutex<Option<String>>>,
    /// YouTube URL to fall back to when the direct stream URL is refused. mpv
    /// then resolves it with its own yt-dlp hook, which is slower but works
    /// even when YouTube rejects the extracted URL's TLS fingerprint.
    fallback: Arc<Mutex<Option<String>>>,
    error: Arc<Mutex<Option<String>>>,
    child: Option<Arc<Mutex<Option<Child>>>>,
    controller: Option<JoinHandle<()>>,
    /// Cleared when the controller exits, so a dead mpv can be replaced.
    running: Arc<AtomicBool>,
    endpoint: PathBuf,
    mpv: PathBuf,
    pub now: Option<NowPlaying>,
    pub volume: i32,
    pub speed: f64,
    pub muted: bool,
}

/// Pick the mpv executable to launch. A configured path wins; otherwise try a
/// player shipped next to RustTube, then the usual Homebrew locations (Finder
/// apps do not inherit the shell PATH), then plain `mpv` on PATH.
pub fn executable(configured: &str) -> PathBuf {
    let configured = configured.trim().trim_matches('"');
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        if path.is_dir() {
            return path.join(if cfg!(windows) { "mpv.exe" } else { "mpv" });
        }
        return path;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(if cfg!(windows) { "mpv.exe" } else { "mpv" });
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    #[cfg(target_os = "macos")]
    for candidate in ["/opt/homebrew/bin/mpv", "/usr/local/bin/mpv"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }

    PathBuf::from("mpv")
}

impl Player {
    pub fn new(volume: i32, mpv: PathBuf) -> Self {
        Self {
            commands: None,
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            resume: Arc::new(Mutex::new(None)),
            pending_audio: Arc::new(Mutex::new(None)),
            fallback: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
            child: None,
            controller: None,
            running: Arc::new(AtomicBool::new(false)),
            endpoint: ipc::endpoint(),
            mpv,
            now: None,
            volume: volume.clamp(0, 130),
            speed: 1.0,
            muted: false,
        }
    }

    fn child_is_alive(&self) -> bool {
        if let Some(slot) = &self.child {
            if let Ok(mut guard) = slot.lock() {
                if let Some(child) = guard.as_mut() {
                    return matches!(child.try_wait(), Ok(None));
                }
            }
        }
        false
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| *s).unwrap_or_default()
    }

    pub fn is_active(&self) -> bool {
        self.now.is_some()
    }

    /// Stop the controller thread and wait for it to finish shutdown. This
    /// must complete before a replacement can reuse the same IPC endpoint.
    fn stop_controller(&mut self) {
        if let Some(tx) = self.commands.take() {
            let _ = tx.send(Cmd::Quit);
        }
        if let Some(slot) = &self.child {
            if let Ok(mut guard) = slot.lock() {
                if let Some(child) = guard.as_mut() {
                    let _ = child.kill();
                }
            }
        }
        if let Some(handle) = self.controller.take() {
            let _ = handle.join();
        }
        // If the controller exited before its shutdown path took the child,
        // reap it here so nothing is left behind.
        if let Some(slot) = self.child.take() {
            if let Ok(mut guard) = slot.lock() {
                if let Some(mut child) = guard.take() {
                    let _ = child.wait();
                }
            }
        }
        self.running.store(false, Ordering::SeqCst);
    }

    /// Start mpv once, in the background, and keep it idle between videos.
    /// Called at launch so the first click never pays process startup.
    pub fn warm(&mut self) {
        if self.commands.is_some() {
            // A child that is still spawning counts as running; once it is
            // gone (window closed or crash), retire the old controller before
            // spawning its replacement.
            let starting = self.child.is_none() && self.running.load(Ordering::SeqCst);
            if self.child_is_alive() || starting {
                return;
            }
            self.stop_controller();
        }
        ipc::cleanup(&self.endpoint);

        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let snapshot = Arc::clone(&self.snapshot);
        let resume = Arc::clone(&self.resume);
        let pending_audio = Arc::clone(&self.pending_audio);
        let fallback = Arc::clone(&self.fallback);
        let error = Arc::clone(&self.error);
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let child_handle = Arc::clone(&child);
        let endpoint = self.endpoint.clone();
        let writer = tx.clone();
        let volume = self.volume;
        let mpv = self.mpv.clone();
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        let handle = std::thread::spawn(move || {
            controller(
                endpoint,
                rx,
                writer,
                snapshot,
                resume,
                pending_audio,
                fallback,
                error,
                child_handle,
                volume,
                mpv,
            );
            running.store(false, Ordering::SeqCst);
        });

        self.child = Some(child);
        self.controller = Some(handle);
        self.commands = Some(tx);
        // Queue the current playback preferences so a fresh mpv starts with
        // the same speed and mute state as the previous one.
        if let Some(tx) = &self.commands {
            let _ = tx.send(Cmd::Speed(self.speed));
            let _ = tx.send(Cmd::Mute(self.muted));
        }
    }

    /// Mark a video as the one playing, before its URL is known. The player bar
    /// appears immediately and shows "Starting…" until mpv reports a position.
    pub fn begin(
        &mut self,
        video_id: &str,
        title: &str,
        channel_id: &str,
        channel_name: &str,
        duration_hint: i64,
    ) -> Result<(), String> {
        self.warm();
        if let Some(err) = self.take_error() {
            return Err(err);
        }
        if let Ok(mut snap) = self.snapshot.lock() {
            *snap = Snapshot::default();
        }
        if let Ok(mut resume) = self.resume.lock() {
            *resume = None;
        }
        self.now = Some(NowPlaying {
            video_id: video_id.to_string(),
            title: title.to_string(),
            channel_id: channel_id.to_string(),
            channel_name: channel_name.to_string(),
            duration_hint,
        });
        Ok(())
    }

    /// Load direct stream URLs. The audio track and any resume point are applied
    /// once mpv reports the file loaded, which is the only safe moment for both.
    pub fn load(&mut self, video: &str, audio: Option<&str>, video_id: &str, start_at: f64) {
        if let Ok(mut resume) = self.resume.lock() {
            *resume = if start_at >= 5.0 {
                Some(start_at)
            } else {
                None
            };
        }
        if let Ok(mut pending) = self.pending_audio.lock() {
            *pending = audio.map(str::to_string);
        }
        if let Ok(mut slot) = self.fallback.lock() {
            *slot = Some(format!("https://www.youtube.com/watch?v={video_id}"));
        }
        self.send(Cmd::Load {
            video: video.to_string(),
        });
    }

    pub fn toggle_pause(&mut self) {
        let paused = self.snapshot().paused;
        self.set_paused(!paused);
    }

    pub fn set_paused(&mut self, paused: bool) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.paused = paused;
        }
        self.send(Cmd::Pause(paused));
    }

    pub fn seek_to(&mut self, seconds: f64) {
        self.send(Cmd::SeekTo(seconds));
    }

    pub fn set_volume(&mut self, volume: i32) {
        self.volume = volume.clamp(0, 130);
        self.send(Cmd::Volume(self.volume));
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(0.25, 4.0);
        self.send(Cmd::Speed(self.speed));
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.send(Cmd::Mute(muted));
    }

    fn send(&mut self, command: Cmd) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(command);
        }
    }

    /// Return mpv to idle. The process stays warm for the next video.
    pub fn stop(&mut self) {
        self.send(Cmd::Stop);
        self.now = None;
        if let Ok(mut snap) = self.snapshot.lock() {
            *snap = Snapshot::default();
        }
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.lock().ok().and_then(|mut error| error.take())
    }

    pub fn quit(&mut self) {
        self.now = None;
        self.stop_controller();
        ipc::cleanup(&self.endpoint);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.quit();
    }
}

/// Everything the controller sends to external mpv for one command. Kept
/// separate from the socket loop so the exact URLs and property writes can be
/// tested without launching a process.
fn command_payloads(command: &Cmd) -> Vec<serde_json::Value> {
    match command {
        Cmd::Load { video } | Cmd::LoadYouTube(video) => {
            vec![json!({ "command": ["loadfile", video, "replace"] })]
        }
        // `select` makes the external track current; HLS video variants carry
        // no audio of their own, so otherwise the file stays silent.
        Cmd::AudioAdd(audio) => vec![json!({ "command": ["audio-add", audio, "select"] })],
        Cmd::SeekTo(target) => vec![json!({ "command": ["seek", target, "absolute"] })],
        Cmd::Pause(paused) => vec![json!({
            "command": ["set", "pause", if *paused { "yes" } else { "no" }]
        })],
        // Numeric properties must use `set_property`; the shorthand `set`
        // expects a string and rejects a JSON number such as 1.5.
        Cmd::Volume(volume) => vec![json!({"command": ["set_property", "volume", volume]})],
        Cmd::Speed(speed) => vec![json!({"command": ["set_property", "speed", speed]})],
        Cmd::Mute(muted) => vec![json!({
            "command": ["set", "mute", if *muted { "yes" } else { "no" }]
        })],
        Cmd::Stop => vec![json!({ "command": ["stop"] })],
        Cmd::Quit => vec![json!({ "command": ["quit"] })],
    }
}

#[allow(clippy::too_many_arguments)]
fn controller(
    endpoint: PathBuf,
    commands: Receiver<Cmd>,
    sender: Sender<Cmd>,
    snapshot: Arc<Mutex<Snapshot>>,
    resume: Arc<Mutex<Option<f64>>>,
    pending_audio: Arc<Mutex<Option<String>>>,
    fallback: Arc<Mutex<Option<String>>>,
    error: Arc<Mutex<Option<String>>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    volume: i32,
    mpv: PathBuf,
) {
    let mut command = Command::new(&mpv);
    command
        .arg("--no-terminal")
        .arg("--really-quiet")
        .arg("--idle=yes")
        .arg("--force-window=no")
        .arg("--keep-open=no")
        .arg("--cache=yes")
        .arg("--hwdec=auto-safe")
        .arg("--ytdl-format=bv*[height<=?1080]+ba/b[height<=?1080]")
        .arg("--title=RustTube")
        .arg(format!("--volume={volume}"))
        .arg(format!("--input-ipc-server={}", endpoint.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Keep the console hidden; mpv still opens its own video window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = if err.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "mpv was not found ({}). Install mpv or set its path in Settings.",
                    mpv.display()
                )
            } else {
                format!("Could not start {}: {err}", mpv.display())
            };
            set_error(&error, message);
            return;
        }
    };
    if let Ok(mut slot) = child_slot.lock() {
        *slot = Some(child);
    }

    let mut writer = match wait_for_socket(&endpoint, &child_slot) {
        Some(stream) => stream,
        None => {
            set_error(
                &error,
                "mpv started but never opened its control socket.".to_string(),
            );
            shutdown(&endpoint, &child_slot);
            return;
        }
    };

    {
        let reader_endpoint = endpoint.clone();
        let reader_snapshot = Arc::clone(&snapshot);
        let reader_resume = Arc::clone(&resume);
        let reader_audio = Arc::clone(&pending_audio);
        let reader_fallback = Arc::clone(&fallback);
        let reader_sender = sender.clone();
        std::thread::spawn(move || {
            reader_loop(
                &reader_endpoint,
                reader_snapshot,
                reader_resume,
                reader_audio,
                reader_fallback,
                reader_sender,
            )
        });
    }

    while let Ok(command) = commands.recv() {
        if matches!(&command, Cmd::Quit) {
            let _ = writer.write_all(b"{\"command\": [\"quit\"]}\n");
            break;
        }
        let payloads = command_payloads(&command);

        let mut failed = false;
        for payload in payloads {
            if writer.write_all(format!("{payload}\n").as_bytes()).is_err() {
                failed = true;
                break;
            }
        }
        if failed {
            set_error(&error, "mpv stopped responding.".to_string());
            break;
        }
    }

    shutdown(&endpoint, &child_slot);
}

fn set_error(error: &Arc<Mutex<Option<String>>>, message: String) {
    if let Ok(mut slot) = error.lock() {
        *slot = Some(message);
    }
}

fn wait_for_socket(endpoint: &PathBuf, child_slot: &Arc<Mutex<Option<Child>>>) -> Option<Stream> {
    for _ in 0..120 {
        if let Ok(stream) = ipc::connect(endpoint) {
            return Some(stream);
        }
        if let Ok(mut slot) = child_slot.lock() {
            if let Some(child) = slot.as_mut() {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return None;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn shutdown(endpoint: &PathBuf, child_slot: &Arc<Mutex<Option<Child>>>) {
    if let Ok(mut slot) = child_slot.lock() {
        if let Some(mut child) = slot.take() {
            std::thread::sleep(Duration::from_millis(120));
            if let Ok(None) = child.try_wait() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
    ipc::cleanup(endpoint);
}

fn reader_loop(
    endpoint: &PathBuf,
    snapshot: Arc<Mutex<Snapshot>>,
    resume: Arc<Mutex<Option<f64>>>,
    pending_audio: Arc<Mutex<Option<String>>>,
    fallback: Arc<Mutex<Option<String>>>,
    sender: Sender<Cmd>,
) {
    let mut reader = match ipc::connect(endpoint) {
        Ok(reader) => reader,
        Err(_) => return,
    };
    for (id, name) in [
        (1, "time-pos"),
        (2, "duration"),
        (3, "pause"),
        (4, "eof-reached"),
    ] {
        let command = json!({ "command": ["observe_property", id, name] });
        if reader.write_all(format!("{command}\n").as_bytes()).is_err() {
            return;
        }
    }

    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => {
                if let Ok(mut snap) = snapshot.lock() {
                    snap.ended = true;
                }
                return;
            }
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=index).collect();
                    if let Ok(text) = std::str::from_utf8(&line) {
                        apply_event(text, &snapshot, &resume, &pending_audio, &fallback, &sender);
                    }
                }
            }
            Err(_) => return,
        }
    }
}

fn apply_event(
    text: &str,
    snapshot: &Arc<Mutex<Snapshot>>,
    resume: &Arc<Mutex<Option<f64>>>,
    pending_audio: &Arc<Mutex<Option<String>>>,
    fallback: &Arc<Mutex<Option<String>>>,
    sender: &Sender<Cmd>,
) {
    let value: serde_json::Value = match serde_json::from_str(text.trim()) {
        Ok(value) => value,
        Err(_) => return,
    };
    let event = value.get("event").and_then(|e| e.as_str()).unwrap_or("");

    // Both the external audio track and a resume seek only work once mpv has
    // the new file loaded.
    if event == "file-loaded" {
        // The direct URL worked, so no hook fallback is needed for this load.
        if let Ok(mut slot) = fallback.lock() {
            *slot = None;
        }
        let audio = pending_audio.lock().ok().and_then(|mut slot| slot.take());
        if let Some(audio) = audio {
            let _ = sender.send(Cmd::AudioAdd(audio));
        }
        let target = resume.lock().ok().and_then(|mut slot| slot.take());
        if let Some(target) = target {
            let _ = sender.send(Cmd::SeekTo(target));
        }
        return;
    }

    if event == "end-file" {
        // `loadfile replace` and `stop` also end a file; only a real end of
        // playback should make the app close out the watch session.
        let reason = value.get("reason").and_then(|r| r.as_str()).unwrap_or("");
        if reason == "error" {
            // A refused direct URL fails almost instantly. Hand the video to
            // mpv's yt-dlp hook instead of leaving the user with nothing.
            let url = fallback.lock().ok().and_then(|mut slot| slot.take());
            if let Some(url) = url {
                let _ = sender.send(Cmd::LoadYouTube(url));
                return;
            }
        }
        if matches!(reason, "eof" | "error") {
            if let Ok(mut snap) = snapshot.lock() {
                snap.ended = true;
            }
        }
        return;
    }

    if event != "property-change" {
        return;
    }
    let name = value.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let data = value.get("data");
    let Ok(mut snap) = snapshot.lock() else {
        return;
    };
    match name {
        "time-pos" => {
            if let Some(position) = data.and_then(|d| d.as_f64()) {
                snap.position = position;
                snap.started = true;
                snap.ended = false;
            }
        }
        "duration" => {
            if let Some(duration) = data.and_then(|d| d.as_f64()) {
                snap.duration = duration;
            }
        }
        "pause" => {
            if let Some(paused) = data.and_then(|d| d.as_bool()) {
                snap.paused = paused;
            }
        }
        "eof-reached" => {
            if data.and_then(|d| d.as_bool()).unwrap_or(false) {
                snap.ended = true;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_live_child(player: &Player) -> Option<u32> {
        for _ in 0..100 {
            if let Some(slot) = &player.child {
                if let Ok(mut guard) = slot.lock() {
                    if let Some(child) = guard.as_mut() {
                        if matches!(child.try_wait(), Ok(None)) {
                            return Some(child.id());
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn external_payloads_use_quality_url_and_playback_controls() {
        let master = "https://host/master.m3u8";
        let playlist = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720
720/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080
1080/index.m3u8
";
        let selected = crate::stream::pick_variant_url(master, playlist, 720)
            .expect("720p variant should be selected");
        assert_eq!(selected, "https://host/720/index.m3u8");
        assert_eq!(
            command_payloads(&Cmd::Load {
                video: selected.clone()
            }),
            vec![json!({"command": ["loadfile", selected, "replace"]})]
        );
        assert_eq!(
            command_payloads(&Cmd::Speed(1.5)),
            vec![json!({"command": ["set_property", "speed", 1.5]})]
        );
        assert_eq!(
            command_payloads(&Cmd::Volume(85)),
            vec![json!({"command": ["set_property", "volume", 85]})]
        );
        assert_eq!(
            command_payloads(&Cmd::Mute(true)),
            vec![json!({"command": ["set", "mute", "yes"]})]
        );
        assert_eq!(
            command_payloads(&Cmd::Mute(false)),
            vec![json!({"command": ["set", "mute", "no"]})]
        );
    }

    #[test]
    fn external_player_queues_selected_quality_url_and_controls() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut player = Player::new(0, PathBuf::from("mpv"));
        player.commands = Some(tx);
        let selected = "https://host/720/index.m3u8";
        player.load(selected, None, "video", 0.0);
        player.set_speed(1.5);
        player.set_muted(true);

        assert!(matches!(
            rx.try_recv(),
            Ok(Cmd::Load { video }) if video == selected
        ));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Speed(speed)) if speed == 1.5));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Mute(true))));
    }

    #[cfg(unix)]
    fn external_property(player: &Player, name: &str) -> serde_json::Value {
        use std::io::BufRead;

        let mut stream = None;
        for _ in 0..100 {
            if let Ok(candidate) = ipc::connect(&player.endpoint) {
                stream = Some(candidate);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut stream = stream.expect("external mpv control socket should open");
        stream
            .write_all(format!("{{\"command\":[\"get_property\",\"{name}\"]}}\n").as_bytes())
            .expect("write property query");
        let mut reader = std::io::BufReader::new(stream);
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read property response");
            assert!(read > 0, "mpv closed before answering {name}");
            let value: serde_json::Value =
                serde_json::from_str(line.trim()).expect("property response is JSON");
            if let Some(data) = value.get("data") {
                return data.clone();
            }
            if let Some(error) = value.get("error").and_then(|error| error.as_str()) {
                assert_eq!(error, "success", "mpv rejected get_property {name}");
            }
        }
    }

    #[cfg(unix)]
    fn silent_wav() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("rusttube-external-{}.wav", std::process::id()));
        let sample_rate = 8_000u32;
        let samples = sample_rate * 4;
        let data_len = samples * 2;
        let mut bytes = Vec::with_capacity((44 + data_len) as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize((44 + data_len) as usize, 0);
        std::fs::write(&path, bytes).expect("write silent wav");
        path
    }

    #[test]
    #[ignore = "launches a real external mpv process"]
    #[cfg(unix)]
    fn external_mpv_applies_speed_and_mute() {
        let mut player = Player::new(0, executable(""));
        let wav = silent_wav();
        player
            .begin("test", "Test", "UC_test", "Test channel", 4)
            .expect("player should accept the video");
        player.load(wav.to_str().expect("wav path"), None, "test", 0.0);

        let loaded = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < loaded && !player.snapshot().started {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            player.snapshot().started,
            "external mpv never loaded the local test media"
        );
        player.set_speed(1.5);
        player.set_muted(true);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut speed = serde_json::Value::Null;
        let mut mute = serde_json::Value::Null;
        while std::time::Instant::now() < deadline {
            speed = external_property(&player, "speed");
            mute = external_property(&player, "mute");
            if speed.as_f64() == Some(1.5) && mute.as_bool() == Some(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("external properties before quit: speed={speed} mute={mute}");
        player.quit();
        let _ = std::fs::remove_file(&wav);

        assert_eq!(
            speed.as_f64(),
            Some(1.5),
            "external mpv speed was not applied"
        );
        assert_eq!(
            mute.as_bool(),
            Some(true),
            "external mpv mute was not applied"
        );
        println!("external mpv applied speed={speed} mute={mute}");
    }

    #[test]
    #[ignore = "launches mpv"]
    fn warm_replaces_an_exited_mpv_process() {
        let mut player = Player::new(0, executable(""));
        player.warm();
        let first = wait_for_live_child(&player).expect("mpv should start");

        {
            let slot = player.child.as_ref().expect("child slot");
            let mut guard = slot.lock().expect("child lock");
            let child = guard.as_mut().expect("mpv child");
            child.kill().expect("kill mpv");
            let _ = child.wait();
        }

        player.warm();
        let second = wait_for_live_child(&player).expect("mpv should restart");
        assert_ne!(first, second, "warm must launch a replacement process");
        player.quit();
    }

    /// Real playback check: resolves the stream, loads it into the warm player,
    /// and asserts both that the position advances and that it starts quickly.
    #[test]
    #[ignore = "launches mpv, runs yt-dlp and needs network access"]
    fn mpv_plays_a_video_quickly() {
        let stream = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(crate::stream::resolve(
                "dQw4w9WgXcQ",
                crate::stream::DEFAULT_MAX_HEIGHT,
            ))
            .expect("stream should resolve");

        let mut player = Player::new(25, PathBuf::from("mpv"));
        player
            .begin("dQw4w9WgXcQ", "Test", "UC_test", "Test channel", 213)
            .expect("player should accept the video");
        let started = std::time::Instant::now();
        player.load(&stream.video, stream.audio.as_deref(), "dQw4w9WgXcQ", 0.0);

        let mut reached = 0.0_f64;
        let mut paused_state = None;
        let mut first_frame = None;
        while started.elapsed() < Duration::from_secs(30) {
            let snap = player.snapshot();
            reached = reached.max(snap.position);
            if snap.started && first_frame.is_none() {
                first_frame = Some(started.elapsed());
                paused_state = Some(snap.paused);
            }
            if reached > 3.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        player.quit();

        assert!(
            reached > 3.0,
            "mpv never reported playback progress (position {reached})"
        );
        assert_eq!(
            paused_state,
            Some(false),
            "player should report playing, not paused"
        );
        let first_frame = first_frame.expect("no first frame");
        assert!(
            first_frame < Duration::from_secs(5),
            "first frame took {first_frame:?}, expected under 5s"
        );
        println!("first frame after {first_frame:?}");
    }
}
