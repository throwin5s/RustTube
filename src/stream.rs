//! Stream resolution through yt-dlp, cached in memory.
//!
//! Two transports are possible, and only one of them gives both quality and
//! speed:
//!
//! * **HLS (used).** yt-dlp exposes a master playlist, and the variant playlist
//!   inside it names one resolution. mpv plays that playlist through ffmpeg, so
//!   the media is fetched by ffmpeg rather than mpv's libcurl stack. Measured at
//!   0.56s to first frame at 1080p, with everything up to 2160p available.
//! * **DASH (avoided).** Direct googlevideo `videoplayback` URLs from the usual
//!   clients answer 403 to any client that does not carry a PO token, which is
//!   why mpv's own yt-dlp hook used to take ~7s retrying before falling back.
//!
//! A direct URL from the Android client is kept as a fallback for videos without
//! an HLS manifest; it is fetched fine but only exists as a 360p muxed stream.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// googlevideo URLs carry their own expiry a few hours out; stay well inside it.
const TTL: Duration = Duration::from_secs(45 * 60);

/// Ceiling for the variant picked out of the master playlist.
const MAX_HEIGHT: u32 = 1080;

/// Android-client stream, used only when no HLS manifest exists.
const DIRECT_FORMAT: &str = "b[height<=720]/b";

#[derive(Clone, Debug)]
pub struct Resolved {
    pub video: String,
    pub audio: Option<String>,
    pub at: Instant,
}

/// Shared cache plus in-flight reservations, so one video is never resolved twice.
#[derive(Clone, Default)]
pub struct Streams {
    cache: Arc<Mutex<HashMap<String, Resolved>>>,
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl Streams {
    pub fn get(&self, id: &str) -> Option<Resolved> {
        let cache = self.cache.lock().ok()?;
        cache
            .get(id)
            .filter(|resolved| resolved.at.elapsed() < TTL)
            .cloned()
    }

    /// Reserve a video for resolution. False when it is cached or already running.
    pub fn claim(&self, id: &str) -> bool {
        if self.get(id).is_some() {
            return false;
        }
        self.in_flight
            .lock()
            .map(|mut set| set.insert(id.to_string()))
            .unwrap_or(false)
    }

    pub fn store(&self, id: &str, resolved: Resolved) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(id);
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(id.to_string(), resolved);
        }
    }

    pub fn release(&self, id: &str) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(id);
        }
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("RustTube/0.2")
            .timeout(Duration::from_secs(12))
            .build()
            .unwrap_or_default()
    })
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// Resolve a playable URL: an HLS variant playlist when possible, else a direct
/// stream. Async because the playlist is fetched over HTTP; yt-dlp runs on a
/// blocking thread.
pub async fn resolve(id: &str) -> Result<Resolved, String> {
    let request = id.to_string();
    let manifest = tokio::task::spawn_blocking(move || manifest_blocking(&request))
        .await
        .ok()
        .and_then(Result::ok);

    if let Some(master) = manifest {
        if let Ok(playlist) = http_text(&master).await {
            let picked = pick_variant(&master, &playlist, MAX_HEIGHT);
            // HLS variants are video-only: the soundtrack is a separate
            // `#EXT-X-MEDIA:TYPE=AUDIO` rendition in the same master playlist,
            // which mpv has to be handed as an external audio track.
            let audio = picked.as_ref().and_then(|variant| {
                pick_audio(&master, &playlist, variant.audio_group.as_deref())
            });
            let video = picked.map(|variant| variant.url).unwrap_or(master);
            return Ok(Resolved {
                video,
                audio,
                at: Instant::now(),
            });
        }
    }

    let request = id.to_string();
    tokio::task::spawn_blocking(move || direct_blocking(&request))
        .await
        .unwrap_or_else(|err| Err(format!("Stream resolution failed: {err}")))
}

/// Every HLS format in a video points at the same master playlist, so one
/// `--print` line replaces the megabyte of JSON `-j` produced: measured 2.0s
/// against 3.2s, which is the difference between a click feeling late and
/// feeling immediate.
fn manifest_blocking(id: &str) -> Result<String, String> {
    // Fetching the watch page costs ~0.5s and only matters for metadata we do
    // not use here, so try the short way first and keep the full extraction as
    // a fallback for videos where that shortcut yields no manifest.
    for extractor in [
        Some("youtube:player_skip=webpage"),
        None,
    ] {
        if let Ok(url) = manifest_once(id, extractor) {
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

fn direct_blocking(id: &str) -> Result<Resolved, String> {
    let stdout = run_ytdlp(
        &[
            "--no-warnings",
            "--no-playlist",
            "--no-progress",
            "--extractor-args",
            "youtube:player_client=android",
            "-g",
            "-f",
            DIRECT_FORMAT,
        ],
        &watch_url(id),
    )?;
    parse_output(&String::from_utf8_lossy(&stdout))
}

/// Prefer the yt-dlp shipped next to the app (inside the macOS bundle and the
/// Windows zip), falling back to PATH for development builds.
fn ytdlp() -> std::path::PathBuf {
    let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
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

fn run_ytdlp(args: &[&str], url: &str) -> Result<Vec<u8>, String> {
    let output = Command::new(ytdlp())
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| match err.kind() {
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
    let mut lines = playlist.lines().map(str::trim).filter(|line| !line.is_empty());
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
        if best.as_ref().map(|(best, _)| height > *best).unwrap_or(true) {
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
    let directory = base[..base.rfind('/').filter(|index| *index > host_end).unwrap_or(host_end)].to_string();
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
            pick_variant(MASTER, playlist, MAX_HEIGHT)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://host/1080/index.m3u8")
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
            pick_variant(MASTER, playlist, MAX_HEIGHT)
                .map(|variant| variant.url)
                .as_deref(),
            Some("https://manifest.googlevideo.com/api/manifest/hls_variant/abc/720/index.m3u8")
        );
    }

    #[test]
    fn no_variants_means_no_preference() {
        assert!(pick_variant(MASTER, "#EXTM3U\n", MAX_HEIGHT).is_none());
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
        let variant = pick_variant(MASTER, WITH_AUDIO, MAX_HEIGHT).expect("a variant");
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
    fn cache_respects_claims_and_returns_values() {
        let streams = Streams::default();
        assert!(streams.claim("abc"));
        assert!(!streams.claim("abc"), "a claimed video stays claimed");
        assert!(streams.get("abc").is_none());

        streams.store(
            "abc",
            Resolved {
                video: "https://v".into(),
                audio: None,
                at: Instant::now(),
            },
        );
        assert_eq!(streams.get("abc").map(|r| r.video), Some("https://v".to_string()));
        assert!(!streams.claim("abc"), "a cached video is not claimed again");
    }
}
