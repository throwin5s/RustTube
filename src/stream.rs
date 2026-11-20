//! Stream resolution: direct InnerTube first, yt-dlp fallback, cached in memory.
//!
//! Two transports are possible, and only one of them gives both quality and
//! speed:
//!
//! * **HLS (used).** The visionOS InnerTube response (or yt-dlp when that is
//!   refused) exposes a master playlist, and the variant playlist
//!   inside it names one resolution. mpv plays that playlist through ffmpeg, so
//!   the media is fetched by ffmpeg rather than mpv's libcurl stack. Cold direct
//!   loads render their first frame in roughly 1.2-1.6s at 1080p, with
//!   everything up to 2160p available.
//! * **DASH (avoided).** Direct googlevideo `videoplayback` URLs from the usual
//!   clients answer 403 to any client that does not carry a PO token, which is
//!   why mpv's own yt-dlp hook used to take ~7s retrying before falling back.
//!
//! A direct URL from the Android client is kept as a fallback for videos without
//! an HLS manifest; it is fetched fine but only exists as a 360p muxed stream.

use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// googlevideo URLs carry their own expiry a few hours out; stay well inside it.
const TTL: Duration = Duration::from_secs(45 * 60);

/// Default ceiling for the variant picked out of the master playlist.
pub const DEFAULT_MAX_HEIGHT: u32 = 1080;

#[derive(Clone, Debug)]
pub struct Resolved {
    pub video: String,
    pub audio: Option<String>,
    pub at: Instant,
}

/// Shared cache plus in-flight reservations, so one video is never resolved twice.
#[derive(Clone, Default)]
pub struct Streams {
    cache: Arc<Mutex<HashMap<(String, u32), Resolved>>>,
    in_flight: Arc<Mutex<HashSet<(String, u32)>>>,
}

impl Streams {
    pub fn get(&self, id: &str, height: u32) -> Option<Resolved> {
        let cache = self.cache.lock().ok()?;
        cache
            .get(&(id.to_string(), height))
            .filter(|resolved| resolved.at.elapsed() < TTL)
            .cloned()
    }

    /// Reserve a video for resolution. False when it is cached or already running.
    pub fn claim(&self, id: &str, height: u32) -> bool {
        if self.get(id, height).is_some() {
            return false;
        }
        self.in_flight
            .lock()
            .map(|mut set| set.insert((id.to_string(), height)))
            .unwrap_or(false)
    }

    pub fn store(&self, id: &str, height: u32, resolved: Resolved) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(&(id.to_string(), height));
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert((id.to_string(), height), resolved);
        }
    }

    pub fn release(&self, id: &str, height: u32) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(&(id.to_string(), height));
        }
    }

    /// Drop every resolved quality for one video, used when a live quality
    /// change forces a fresh resolution.
    pub fn clear_cache(&self, id: &str) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|(cached_id, _), _| cached_id != id);
        }
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(BROWSER_USER_AGENT)
            .timeout(Duration::from_secs(12))
            .build()
            .unwrap_or_default()
    })
}

const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

/// YouTube's player API now treats a missing visitor context as a bot. The
/// value is public per-session data, not a credential; fetching it once and
/// reusing it is the difference between a 0.1s player response and a
/// `LOGIN_REQUIRED` refusal that costs several seconds in yt-dlp.
async fn visitor_data() -> Option<String> {
    static VISITOR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let slot = VISITOR.get_or_init(|| Mutex::new(None));
    if let Ok(cached) = slot.lock() {
        if let Some(value) = cached.as_ref() {
            return Some(value.clone());
        }
    }

    let started = Instant::now();
    let html = client()
        .get("https://www.youtube.com/")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let value = parse_visitor_data(&html);
    log_timing("visitor data", started);
    if let Ok(mut cached) = slot.lock() {
        *cached = value.clone();
    }
    value
}

/// Warm the visitor cache while the app is loading its first screen. This is
/// deliberately best-effort: a failed fetch just means the player request
/// falls back to its previous behavior.
pub async fn warm_visitor_data() {
    let _ = visitor_data().await;
}

fn parse_visitor_data(html: &str) -> Option<String> {
    let marker = "\"visitorData\":\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

pub fn timing_enabled() -> bool {
    std::env::var_os("RUSTTUBE_TIMING").is_some()
}

fn log_timing(label: &str, started: Instant) {
    if timing_enabled() {
        eprintln!(
            "[rusttube timing] {label}: {:.2}s",
            started.elapsed().as_secs_f64()
        );
    }
}

/// Resolve a playable URL: an HLS variant playlist when possible, else a direct
/// stream. Async because the playlist is fetched over HTTP; yt-dlp runs on a
/// blocking thread.
pub async fn resolve(id: &str, max_height: u32) -> Result<Resolved, String> {
    let force_ytdlp = std::env::var_os("RUSTTUBE_DISABLE_INNERTUBE").is_some();
    resolve_with(id, max_height, force_ytdlp).await
}

async fn resolve_with(
    id: &str,
    max_height: u32,
    force_ytdlp: bool,
) -> Result<Resolved, String> {
    let started = Instant::now();
    let max_height = max_height.max(1);
    let request = id.to_string();
    let manifest_started = Instant::now();
    let manifest = if force_ytdlp {
        None
    } else {
        innertube_manifest(&request).await
    };
    let manifest = match manifest {
        Some(manifest) => Some(manifest),
        None => {
            let request = request.clone();
            let legacy = std::env::var_os("RUSTTUBE_LEGACY_YTDLP").is_some();
            tokio::task::spawn_blocking(move || manifest_blocking(&request, legacy))
                .await
                .ok()
                .and_then(Result::ok)
        }
    };
    if manifest.is_some() {
        log_timing("manifest extraction total", manifest_started);
    }

    if let Some(master) = manifest {
        let playlist_started = Instant::now();
        if let Ok(playlist) = http_text(&master).await {
            log_timing("playlist fetch", playlist_started);
            log_timing("stream resolution total", started);
            return Ok(resolved_from_playlist(&master, &playlist, max_height));
        }
    }

    let request = id.to_string();
    let result = tokio::task::spawn_blocking(move || direct_blocking(&request, max_height))
        .await
        .unwrap_or_else(|err| Err(format!("Stream resolution failed: {err}")));
    log_timing("stream resolution total", started);
    result
}

/// Turn an HLS master and its variants into the stream shape consumed by the
/// player. Kept separate from the HTTP fetch so the exact quality/audio pairing
/// can be tested against a captured playlist without network access.
fn resolved_from_playlist(master: &str, playlist: &str, max_height: u32) -> Resolved {
    let picked = pick_variant(master, playlist, max_height);
    // HLS variants are video-only: the soundtrack is a separate
    // `#EXT-X-MEDIA:TYPE=AUDIO` rendition in the same master playlist, which
    // mpv has to be handed as an external audio track.
    let audio = picked
        .as_ref()
        .and_then(|variant| pick_audio(master, playlist, variant.audio_group.as_deref()));
    let video = picked
        .map(|variant| variant.url)
        .unwrap_or_else(|| master.to_string());
    Resolved {
        video,
        audio,
        at: Instant::now(),
    }
}

/// One lightweight player API request is enough to get the HLS master. The
/// visionOS client needs the visitor context above, but no player-JS challenge,
/// and measured ~0.1s here versus ~2.5s for a yt-dlp extraction. Any
/// non-playable response falls through to yt-dlp, which remains the
/// compatibility path for restricted or unusual videos.
async fn innertube_manifest(id: &str) -> Option<String> {
    let started = Instant::now();
    let visitor = visitor_data().await;
    let mut context = json!({
        "clientName": "VISIONOS",
        "clientVersion": "1.02",
        "deviceMake": "Apple",
        "deviceModel": "RealityDevice17,1",
        "userAgent": BROWSER_USER_AGENT,
        "osName": "visionOS",
        "osVersion": "26.5.23O471",
        "hl": "en",
        "timeZone": "UTC",
        "utcOffsetMinutes": 0
    });
    if let Some(visitor) = visitor.as_deref() {
        context["visitorData"] = json!(visitor);
    }
    let body = json!({
        "context": {"client": context},
        "videoId": id,
        "contentCheckOk": true,
        "racyCheckOk": true
    });
    let mut request = client()
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header("Content-Type", "application/json")
        .header("Origin", "https://www.youtube.com")
        .header("X-YouTube-Client-Name", "101")
        .header("X-YouTube-Client-Version", "1.02")
        .header("User-Agent", BROWSER_USER_AGENT);
    if let Some(visitor) = visitor {
        request = request.header("X-Goog-Visitor-Id", visitor);
    }
    let response = request.json(&body).send().await.ok()?;
    let value: serde_json::Value = response.json().await.ok()?;
    let manifest = parse_innertube_manifest(&value);
    log_timing("innertube manifest", started);
    manifest
}

/// Accept only a playable player response with a non-empty HLS manifest. Any
/// refusal, error response, or missing manifest routes to the yt-dlp fallback.
fn parse_innertube_manifest(value: &serde_json::Value) -> Option<String> {
    let status = value
        .pointer("/playabilityStatus/status")
        .and_then(|status| status.as_str())
        .unwrap_or("OK");
    if status != "OK" {
        return None;
    }
    value
        .pointer("/streamingData/hlsManifestUrl")
        .and_then(|url| url.as_str())
        .filter(|url| !url.trim().is_empty())
        .map(str::to_string)
}

/// Every HLS format in a video points at the same master playlist, so one
/// `--print` line replaces the megabyte of JSON `-j` produced: measured 2.0s
/// against 3.2s, which is the difference between a click feeling late and
/// feeling immediate.
fn manifest_blocking(id: &str, legacy: bool) -> Result<String, String> {
    // yt-dlp normally negotiates several player clients in one extraction. The
    // visionOS client alone still exposes the HLS manifest, but it needs the
    // webpage's visitor context; skipping the webpage makes it answer
    // `LOGIN_REQUIRED`. Keep the full extraction as the fallback for videos the
    // shorter path cannot resolve.
    let fast_client = if legacy {
        "youtube:player_skip=webpage"
    } else {
        "youtube:player_client=visionos"
    };
    let fast_label = if legacy {
        "yt-dlp legacy fast path"
    } else {
        "yt-dlp visionos fast path"
    };
    for (label, extractor) in [(fast_label, Some(fast_client)), ("yt-dlp full extraction", None)] {
        let started = Instant::now();
        let result = manifest_once(id, extractor);
        log_timing(label, started);
        if let Ok(url) = result {
            return Ok(url);
        }
    }
    Err("This video has no adaptive stream.".to_string())
}

fn manifest_once(id: &str, extractor: Option<&str>) -> Result<String, String> {
    let mut args = vec![
        "--no-warnings",
        "--no-playlist",
        "--no-progress",
        "-f",
        "bv*[protocol^=m3u8]/ba[protocol^=m3u8]/b[protocol^=m3u8]",
        "--print",
        "%(manifest_url)s",
    ];
    if let Some(rule) = extractor {
        args.push("--extractor-args");
        args.push(rule);
    }
    let stdout = run_ytdlp(&args, &watch_url(id))?;
    String::from_utf8_lossy(&stdout)
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http"))
        .map(str::to_string)
        .ok_or_else(|| "This video has no adaptive stream.".to_string())
}

fn direct_blocking(id: &str, max_height: u32) -> Result<Resolved, String> {
    let started = Instant::now();
    let format = format!("b[height<=?{max_height}]/b");
    let result = run_ytdlp(
        &[
            "--no-warnings",
            "--no-playlist",
            "--no-progress",
            "--extractor-args",
            "youtube:player_client=android",
            "-g",
            "-f",
            &format,
        ],
        &watch_url(id),
    );
    log_timing("yt-dlp direct fallback", started);
    let stdout = result?;
    parse_output(&String::from_utf8_lossy(&stdout))
}

/// Prefer the yt-dlp shipped next to the app (inside the macOS bundle and the
/// Windows zip), falling back to PATH for development builds.
fn ytdlp() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    std::path::PathBuf::from("yt-dlp")
}

fn ytdlp_command(args: &[&str], url: &str) -> Command {
    let mut command = Command::new(ytdlp());
    command
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Without CREATE_NO_WINDOW, every yt-dlp call from this GUI app opens
        // a visible console window over the player.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

/// Fetch only public metadata, with no downloads or user yt-dlp configuration.
pub async fn video_metadata(id: &str) -> Result<serde_json::Value, String> {
    if id.len() != 11 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("Invalid video ID".into());
    }
    let url = watch_url(id);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;

        let mut child = ytdlp_command(
            &[
                "--ignore-config", "--no-playlist", "--skip-download", "--no-warnings",
                "--socket-timeout", "5", "--retries", "0", "--extractor-retries", "0",
                "--print", "%(.{id,title,channel,channel_id,duration,view_count,upload_date,description})j",
            ],
            &url,
        )
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("Could not start yt-dlp: {err}"))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        let stdout = child.stdout.take().expect("yt-dlp stdout is piped");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.take(256 * 1024).read_to_end(&mut bytes).map(|_| bytes)
        });
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) => return Err("yt-dlp could not load video metadata".into()),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("yt-dlp metadata request timed out or was interrupted".into());
                }
            }
        }
        while !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if !reader.is_finished() {
            return Err("yt-dlp metadata output timed out".into());
        }
        let bytes = reader.join().map_err(|_| "Could not read yt-dlp output")?
            .map_err(|_| "Could not read yt-dlp output")?;
        serde_json::from_slice(&bytes).map_err(|_| "Invalid yt-dlp metadata".into())
    })
    .await
    .map_err(|_| "Metadata task was interrupted")?
}

fn run_ytdlp(args: &[&str], url: &str) -> Result<Vec<u8>, String> {
    let output = ytdlp_command(args, url).output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => "yt-dlp is not installed. Install it with \
                `brew install yt-dlp` (macOS) or `winget install yt-dlp` (Windows)."
            .to_string(),
        _ => format!("Could not run yt-dlp: {err}"),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        return Err(if line.is_empty() {
            "yt-dlp could not resolve this video.".to_string()
        } else {
            format!("Could not resolve this video: {line}")
        });
    }
    Ok(output.stdout)
}

async fn http_text(url: &str) -> Result<String, String> {
    let response = client()
        .get(url)
        .send()
        .await
        .map_err(|err| format!("Could not load the stream playlist: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "The stream playlist returned HTTP {}",
            response.status().as_u16()
        ));
    }
    response
        .text()
        .await
        .map_err(|err| format!("Could not read the stream playlist: {err}"))
}

/// A chosen video variant plus the audio rendition group it expects.
struct Variant {
    url: String,
    audio_group: Option<String>,
}

/// Pick the best variant at or below `max_height`, so mpv opens one resolution
/// directly instead of parsing the master playlist and buffering subtitles.
fn pick_variant(master: &str, playlist: &str, max_height: u32) -> Option<Variant> {
    let mut best: Option<(u32, Variant)> = None;
    let mut lines = playlist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    while let Some(line) = lines.next() {
        if !line.starts_with("#EXT-X-STREAM-INF") {
            continue;
        }
        let height = line
            .split(',')
            .find_map(|part| part.strip_prefix("RESOLUTION="))
            .and_then(|resolution| resolution.split('x').nth(1))
            .and_then(|height| height.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let audio_group = attribute(line, "AUDIO").map(str::to_string);
        let Some(uri) = lines.next() else { break };
        if uri.starts_with('#') || height > max_height {
            continue;
        }
        if best
            .as_ref()
            .map(|(best, _)| height > *best)
            .unwrap_or(true)
        {
            best = Some((
                height,
                Variant {
                    url: absolute(master, uri),
                    audio_group,
                },
            ));
        }
    }
    best.map(|(_, variant)| variant)
}

#[cfg(test)]
pub(crate) fn pick_variant_url(master: &str, playlist: &str, max_height: u32) -> Option<String> {
    pick_variant(master, playlist, max_height).map(|variant| variant.url)
}

/// The audio rendition that belongs with the chosen variant. YouTube lists two
/// (233/234, different codecs); the variant's `AUDIO` group says which one pairs
/// with its video segments.
fn pick_audio(master: &str, playlist: &str, group: Option<&str>) -> Option<String> {
    let mut first: Option<String> = None;
    let mut preferred: Option<String> = None;
    for line in playlist.lines().map(str::trim) {
        if !line.starts_with("#EXT-X-MEDIA") || !line.contains("TYPE=AUDIO") {
            continue;
        }
        let Some(uri) = attribute(line, "URI") else {
            continue;
        };
        let resolved = absolute(master, uri);
        if first.is_none() {
            first = Some(resolved.clone());
        }
        let matches_group = match (group, attribute(line, "GROUP-ID")) {
            (Some(group), Some(candidate)) => group == candidate,
            _ => false,
        };
        if matches_group || preferred.is_none() && line.contains("DEFAULT=YES") {
            preferred = Some(resolved);
        }
    }
    preferred.or(first)
}

/// Read `KEY="value"` out of an HLS tag. Values can contain commas, so a plain
/// split on ',' would cut the URLs in half.
fn attribute<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

/// Playlist entries are usually absolute; resolve the relative form anyway.
fn absolute(base: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_string();
    }
    let Some(scheme_end) = base.find("://") else {
        return uri.to_string();
    };
    let after_scheme = scheme_end + 3;
    let host_end = base[after_scheme..]
        .find('/')
        .map(|index| after_scheme + index)
        .unwrap_or(base.len());
    let origin = &base[..host_end];
    if uri.starts_with('/') {
        return format!("{origin}{uri}");
    }
    let directory = base[..base
        .rfind('/')
        .filter(|index| *index > host_end)
        .unwrap_or(host_end)]
        .to_string();
    format!("{directory}/{uri}")
}

/// `-g` prints the video URL first, then the audio URL for split formats.
fn parse_output(stdout: &str) -> Result<Resolved, String> {
    let mut urls = stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("http"));
    let video = urls
        .next()
        .ok_or_else(|| "yt-dlp returned no stream URL.".to_string())?;
    let audio = urls.next();
    Ok(Resolved {
        video: video.to_string(),
        audio: audio.map(str::to_string),
        at: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "https://manifest.googlevideo.com/api/manifest/hls_variant/abc/index.m3u8";

    #[test]
    fn picks_the_best_variant_within_the_ceiling() {
        let playlist = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=426x240
https://host/240/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=3840x2160
https://host/2160/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000,RESOLUTION=1920x1080
https://host/1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000,RESOLUTION=1280x720
https://host/720/index.m3u8
";
        assert_eq!(
            pick_variant(MASTER, playlist, DEFAULT_MAX_HEIGHT)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://host/1080/index.m3u8")
        );
    }

    #[test]
    fn can_pick_above_the_auto_ceiling() {
        let playlist = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=3840x2160
https://host/2160/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000,RESOLUTION=1920x1080
https://host/1080/index.m3u8
";
        assert_eq!(
            pick_variant(MASTER, playlist, 2160)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://host/2160/index.m3u8")
        );
    }

    #[test]
    fn honors_a_lower_quality_ceiling() {
        let playlist = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=5000,RESOLUTION=1920x1080
https://host/1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000,RESOLUTION=1280x720
https://host/720/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=426x240
https://host/240/index.m3u8
";
        assert_eq!(
            pick_variant(MASTER, playlist, 720)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://host/720/index.m3u8")
        );
        assert_eq!(
            pick_variant(MASTER, playlist, 360)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://host/240/index.m3u8")
        );
    }

    #[test]
    fn resolves_relative_variant_urls() {
        let playlist = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=3000,RESOLUTION=1280x720
720/index.m3u8
";
        assert_eq!(
            pick_variant(MASTER, playlist, DEFAULT_MAX_HEIGHT)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://manifest.googlevideo.com/api/manifest/hls_variant/abc/720/index.m3u8")
        );
    }

    #[test]
    fn no_variants_means_no_preference() {
        assert!(pick_variant(MASTER, "#EXTM3U\n", DEFAULT_MAX_HEIGHT).is_none());
    }

    /// A master shaped like YouTube's: video-only variants plus two audio
    /// renditions and subtitle groups, with commas inside the URLs.
    const WITH_AUDIO: &str = "\
#EXTM3U
#EXT-X-MEDIA:URI=\"https://host/aud/233/index.m3u8?sig=a,b\",TYPE=AUDIO,GROUP-ID=\"233\",DEFAULT=YES,AUTOSELECT=YES
#EXT-X-MEDIA:URI=\"https://host/aud/234/index.m3u8?sig=c,d\",TYPE=AUDIO,GROUP-ID=\"234\",DEFAULT=YES,AUTOSELECT=YES
#EXT-X-MEDIA:URI=\"https://host/subs/en.m3u8\",TYPE=SUBTITLES,GROUP-ID=\"vtt\",DEFAULT=NO
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO=\"234\"
https://host/720/index.m3u8?sig=e,f
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO=\"233\"
https://host/1080/index.m3u8
";

    #[test]
    fn picks_audio_from_the_variants_own_group() {
        let variant = pick_variant(MASTER, WITH_AUDIO, DEFAULT_MAX_HEIGHT).expect("a variant");
        assert_eq!(variant.url, "https://host/1080/index.m3u8");
        assert_eq!(
            pick_audio(MASTER, WITH_AUDIO, variant.audio_group.as_deref()).as_deref(),
            Some("https://host/aud/233/index.m3u8?sig=a,b"),
            "the 1080p variant pairs with audio group 233"
        );
    }

    #[test]
    fn audio_falls_back_to_the_default_rendition() {
        assert_eq!(
            pick_audio(MASTER, WITH_AUDIO, Some("999")).as_deref(),
            Some("https://host/aud/233/index.m3u8?sig=a,b")
        );
        assert!(pick_audio(MASTER, "#EXTM3U\n", None).is_none());
    }

    #[test]
    fn split_streams_yield_video_and_audio() {
        let resolved = parse_output("https://video.example/a\nhttps://audio.example/b\n").unwrap();
        assert_eq!(resolved.video, "https://video.example/a");
        assert_eq!(resolved.audio.as_deref(), Some("https://audio.example/b"));
    }

    #[test]
    fn progressive_streams_yield_video_only() {
        let resolved = parse_output("https://video.example/combined\n").unwrap();
        assert_eq!(resolved.video, "https://video.example/combined");
        assert!(resolved.audio.is_none());
    }

    #[test]
    fn empty_output_is_an_error() {
        assert!(parse_output("\n\n").is_err());
    }

    #[test]
    fn extracts_visitor_data_from_the_watch_page_config() {
        let html = r#"<script>ytcfg.set({"VISITOR_DATA":"unused","visitorData":"CgtlcG5qckFCUDNPRSjXje_XBjIKCgJVUxIEGgAgJGKvAgqsAkN0NEJDQVVTa0FFQkRCNlBEQ1VkX0JG"});</script>"#;
        assert_eq!(
            parse_visitor_data(html).as_deref(),
            Some("CgtlcG5qckFCUDNPRSjXje_XBjIKCgJVUxIEGgAgJGKvAgqsAkN0NEJDQVVTa0FFQkRCNlBEQ1VkX0JG")
        );
        assert!(parse_visitor_data("<html></html>").is_none());
    }

    const INNERTUBE_FIXTURE: &str = include_str!("../tests/fixtures/innertube-player.json");
    const HLS_FIXTURE: &str = include_str!("../tests/fixtures/hls-master.m3u8");

    #[test]
    fn innertube_response_parses_into_the_stream_shape() {
        let value: serde_json::Value = serde_json::from_str(INNERTUBE_FIXTURE).unwrap();
        let master = parse_innertube_manifest(&value).expect("playable manifest");
        assert_eq!(master, "https://fixture.invalid/master.m3u8");

        let resolved = resolved_from_playlist(&master, HLS_FIXTURE, 720);
        assert_eq!(resolved.video, "https://fixture.invalid/video/720/index.m3u8");
        assert_eq!(
            resolved.audio.as_deref(),
            Some("https://fixture.invalid/audio/234/index.m3u8")
        );
    }

    #[test]
    fn hls_fixture_keeps_quality_and_audio_groups_together() {
        let cases = [
            (720, "https://fixture.invalid/video/720/index.m3u8", "234"),
            (1080, "https://fixture.invalid/video/1080/index.m3u8", "234"),
            (1440, "https://fixture.invalid/video/1440/index.m3u8", "234"),
            (2160, "https://fixture.invalid/video/2160/index.m3u8", "233"),
        ];
        for (height, video, audio_group) in cases {
            let resolved = resolved_from_playlist(MASTER, HLS_FIXTURE, height);
            assert_eq!(resolved.video, video, "selected {height}p video");
            let expected_audio = format!("https://fixture.invalid/audio/{audio_group}/index.m3u8");
            assert_eq!(
                resolved.audio.as_deref(),
                Some(expected_audio.as_str()),
                "selected audio for {height}p"
            );
        }
    }

    #[test]
    fn refused_innertube_response_falls_back_to_a_ytdlp_manifest() {
        let refused = json!({
            "playabilityStatus": {"status": "UNPLAYABLE"},
            "streamingData": {"hlsManifestUrl": "https://fixture.invalid/refused.m3u8"}
        });
        assert!(parse_innertube_manifest(&refused).is_none());

        // This is the shape yt-dlp hands to the resolver after InnerTube refuses:
        // a master playlist which must still honour the chosen ceiling.
        let resolved = resolved_from_playlist(MASTER, HLS_FIXTURE, 720);
        assert_eq!(resolved.video, "https://fixture.invalid/video/720/index.m3u8");
        assert_eq!(
            resolved.audio.as_deref(),
            Some("https://fixture.invalid/audio/234/index.m3u8")
        );
    }

    #[test]
    fn empty_or_missing_manifest_also_falls_back() {
        let empty = json!({
            "playabilityStatus": {"status": "OK"},
            "streamingData": {"hlsManifestUrl": ""}
        });
        assert!(parse_innertube_manifest(&empty).is_none());
        assert!(parse_innertube_manifest(&json!({"playabilityStatus": {"status": "OK"}})).is_none());
    }

    #[test]
    #[ignore = "needs network, yt-dlp, and a working mpv"]
    fn forced_innertube_failure_plays_through_the_ytdlp_fallback() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let resolved = runtime
            .block_on(resolve_with("dQw4w9WgXcQ", 720, true))
            .expect("yt-dlp fallback resolution");
        assert!(resolved.video.starts_with("http"));
        assert!(resolved.audio.is_some(), "the fallback keeps the audio track");

        let higher = runtime
            .block_on(resolve_with("dQw4w9WgXcQ", 1440, true))
            .expect("yt-dlp fallback at 1440p");
        assert_ne!(
            higher.video, resolved.video,
            "the fallback honours the quality-selected variant"
        );
        assert!(higher.audio.is_some(), "the higher variant keeps its audio");

        let mut player = crate::player::Player::new(0, crate::player::executable(""));
        player
            .begin("dQw4w9WgXcQ", "fallback test", "", "", 0)
            .expect("external mpv starts");
        player.load(
            &resolved.video,
            resolved.audio.as_deref(),
            "dQw4w9WgXcQ",
            0.0,
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && !player.snapshot().started {
            std::thread::sleep(Duration::from_millis(100));
        }
        let snapshot = player.snapshot();
        player.quit();
        assert!(
            snapshot.started && snapshot.duration > 0.0,
            "fallback stream reached playback: {snapshot:?}"
        );
    }

    #[test]
    fn cache_respects_claims_and_returns_values() {
        let streams = Streams::default();
        assert!(streams.claim("abc", 1080));
        assert!(!streams.claim("abc", 1080), "a claimed video stays claimed");
        assert!(streams.get("abc", 1080).is_none());

        streams.store(
            "abc",
            1080,
            Resolved {
                video: "https://v".into(),
                audio: None,
                at: Instant::now(),
            },
        );
        assert_eq!(
            streams.get("abc", 1080).map(|r| r.video),
            Some("https://v".to_string())
        );
        assert!(
            streams.get("abc", 720).is_none(),
            "a different quality is resolved separately"
        );
        assert!(
            !streams.claim("abc", 1080),
            "a cached video is not claimed again"
        );
    }
}
