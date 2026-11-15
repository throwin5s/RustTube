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

use serde_json::json;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
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

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub ended: bool,
    /// True once mpv has reported a real playback position.
    pub started: bool,
}

#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub video_id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub duration_hint: i64,
}

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
    Seek(f64),
    Volume(i32),
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
    endpoint: PathBuf,
    pub now: Option<NowPlaying>,
    pub volume: i32,
}

impl Player {
    pub fn new(volume: i32) -> Self {
        Self {
            commands: None,
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            resume: Arc::new(Mutex::new(None)),
            pending_audio: Arc::new(Mutex::new(None)),
            fallback: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
            child: None,
            endpoint: ipc::endpoint(),
            now: None,
            volume: volume.clamp(0, 130),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| *s).unwrap_or_default()
    }

    pub fn is_active(&self) -> bool {
        self.now.is_some()
    }

    /// Start mpv once, in the background, and keep it idle between videos.
    /// Called at launch so the first click never pays process startup.
    pub fn warm(&mut self) {
        if self.commands.is_some() {
            return;
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

        std::thread::spawn(move || {
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
            )
        });

        self.child = Some(child);
        self.commands = Some(tx);
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
            *resume = if start_at > 5.0 { Some(start_at) } else { None };
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
        self.send(Cmd::Pause(!paused));
    }

    pub fn seek(&mut self, seconds: f64) {
        self.send(Cmd::Seek(seconds));
    }

    pub fn set_volume(&mut self, volume: i32) {
        self.volume = volume.clamp(0, 130);
        self.send(Cmd::Volume(self.volume));
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
        if let Some(tx) = self.commands.take() {
            let _ = tx.send(Cmd::Quit);
        }
        // Also kill directly: if the controller is still waiting for the socket,
        // it would otherwise leave an orphaned mpv behind.
        if let Some(slot) = self.child.take() {
            if let Ok(mut guard) = slot.lock() {
                if let Some(mut child) = guard.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        ipc::cleanup(&self.endpoint);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.quit();
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
) {
    let spawned = Command::new("mpv")
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
        .stderr(Stdio::null())
        .spawn();

    let child = match spawned {
        Ok(child) => child,
        Err(err) => {
            let message = if err.kind() == std::io::ErrorKind::NotFound {
                "mpv is not installed. Install it with `brew install mpv` or from mpv.io.".to_string()
            } else {
                format!("Could not start mpv: {err}")
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
        let payloads: Vec<serde_json::Value> = match command {
            Cmd::Load { video } | Cmd::LoadYouTube(video) => {
                vec![json!({ "command": ["loadfile", video, "replace"] })]
            }
            // `select` makes the external track current; HLS video variants carry
            // no audio of their own, so otherwise the file stays silent.
            Cmd::AudioAdd(audio) => vec![json!({ "command": ["audio-add", audio, "select"] })],
            Cmd::SeekTo(target) => vec![json!({ "command": ["seek", target, "absolute"] })],
            Cmd::Pause(paused) => vec![json!({ "command": ["set_property", "pause", paused] })],
            Cmd::Seek(delta) => vec![json!({ "command": ["seek", delta, "relative"] })],
            Cmd::Volume(volume) => vec![json!({ "command": ["set_property", "volume", volume] })],
            Cmd::Stop => vec![json!({ "command": ["stop"] })],
            Cmd::Quit => {
                let _ = writer.write_all(b"{\"command\": [\"quit\"]}\n");
                break;
            }
        };

        let mut failed = false;
        for payload in payloads {
            if writer
                .write_all(format!("{payload}\n").as_bytes())
                .is_err()
            {
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

fn wait_for_socket(
    endpoint: &PathBuf,
    child_slot: &Arc<Mutex<Option<Child>>>,
) -> Option<Stream> {
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
    for (id, name) in [(1, "time-pos"), (2, "duration"), (3, "pause"), (4, "eof-reached")] {
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

    /// Real playback check: resolves the stream, loads it into the warm player,
    /// and asserts both that the position advances and that it starts quickly.
    #[test]
    #[ignore = "launches mpv, runs yt-dlp and needs network access"]
    fn mpv_plays_a_video_quickly() {
        let stream = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(crate::stream::resolve("dQw4w9WgXcQ"))
            .expect("stream should resolve");

        let mut player = Player::new(25);
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
