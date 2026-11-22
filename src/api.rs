//! YouTube metadata access.
//!
//! Invidious and Piped are the primary metadata sources. The watch page can
//! fall back to public video metadata from the bundled yt-dlp.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoCard {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub channel_avatar: String,
    /// Length in seconds; 0 for live or unknown.
    pub duration: i64,
    pub views: u64,
    pub published: String,
    /// Unix seconds, used for sorting feeds. 0 when unknown.
    pub published_unix: i64,
    pub thumbnail: String,
    pub live: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoDetail {
    #[serde(flatten)]
    pub card: VideoCard,
    pub description: String,
    pub likes: u64,
    pub related: Vec<VideoCard>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ChannelCard {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub subscribers: u64,
    pub description: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchResults {
    pub videos: Vec<VideoCard>,
    pub channels: Vec<ChannelCard>,
    pub playlists: Vec<PlaylistCard>,
}

/// A playlist owned by someone else, found through search.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlaylistCard {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub video_count: i64,
    pub thumbnail: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemotePlaylist {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub thumbnail: String,
    pub videos: Vec<VideoCard>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chapter {
    pub seconds: i64,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesPoint {
    pub prefix: String,
    pub number: u32,
}

#[derive(Clone, Debug, Default)]
pub struct CaptionTrack {
    pub label: String,
    pub language: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub banner: String,
    pub subscribers: u64,
    pub description: String,
    pub videos: Vec<VideoCard>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Invidious,
    Piped,
    /// A user supplied instance whose flavour is unknown until it answers.
    Auto,
}

#[derive(Clone, Debug)]
struct Endpoint {
    base: String,
    kind: Kind,
}

/// Instances confirmed reachable when this build was written. Order matters:
/// the first entry answers every metadata endpoint, the Piped entry covers
/// cases where the Invidious instance is rate limited.
const DEFAULT_ENDPOINTS: &[(&str, Kind)] = &[
    ("https://invidious.materialio.us", Kind::Invidious),
    ("https://api.piped.private.coffee", Kind::Piped),
    ("https://pipedapi.adminforge.de", Kind::Piped),
    ("https://yewtu.be", Kind::Invidious),
];

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    endpoints: Arc<Mutex<Vec<Endpoint>>>,
    last_good: Arc<Mutex<Option<String>>>,
    pub last_used: Arc<Mutex<String>>,
}

impl Api {
    pub fn new(custom: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; RustTube/0.1; +https://github.com/freetubeapp/freetube)")
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(8))
            .build()
            .unwrap_or_default();

        let endpoints = Self::build_endpoints(custom);
        Self {
            http,
            endpoints: Arc::new(Mutex::new(endpoints)),
            last_good: Arc::new(Mutex::new(None)),
            last_used: Arc::new(Mutex::new(String::new())),
        }
    }

    fn build_endpoints(custom: Option<String>) -> Vec<Endpoint> {
        let mut list: Vec<Endpoint> = Vec::new();
        if let Some(base) = custom {
            let base = base.trim().trim_end_matches('/').to_string();
            if !base.is_empty() {
                list.push(Endpoint { base, kind: Kind::Auto });
            }
        }
        for (base, kind) in DEFAULT_ENDPOINTS {
            let base = base.to_string();
            if !list.iter().any(|e| e.base == base) {
                list.push(Endpoint { base, kind: *kind });
            }
        }
        list
    }

    pub fn set_custom(&self, custom: Option<String>) {
        if let Ok(mut guard) = self.endpoints.lock() {
            *guard = Self::build_endpoints(custom);
        }
        if let Ok(mut last) = self.last_good.lock() {
            *last = None;
        }
    }

    pub fn current_endpoint(&self) -> String {
        self.last_used.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn ordered(&self) -> Vec<Endpoint> {
        let all = match self.endpoints.lock() {
            Ok(g) => g.clone(),
            Err(_) => Vec::new(),
        };
        let good = self.last_good.lock().ok().and_then(|g| g.clone());
        let mut ordered = Vec::with_capacity(all.len());
        if let Some(good) = good {
            if let Some(pos) = all.iter().position(|e| e.base == good) {
                ordered.push(all[pos].clone());
                ordered.extend(all.iter().enumerate().filter(|(i, _)| *i != pos).map(|(_, e)| e.clone()));
                return ordered;
            }
        }
        ordered.extend(all);
        ordered
    }

    fn remember(&self, base: &str) {
        if let Ok(mut last) = self.last_good.lock() {
            *last = Some(base.to_string());
        }
        if let Ok(mut used) = self.last_used.lock() {
            *used = base.to_string();
        }
    }

    async fn get_one(&self, url: &str) -> Result<Value, String> {
        let res = self
            .http
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| friendly_transport_error(&e))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        let text = res.text().await.map_err(|e| e.to_string())?;
        let value = serde_json::from_str::<Value>(&text)
            .map_err(|_| "instance returned a non-JSON response".to_string())?;
        // Invidious and Piped both report failures as HTTP 200 with an
        // `error` field. Treating that as success stopped the endpoint loop,
        // so detail, descriptions and related videos stayed empty even when
        // the next instance could answer.
        if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
            return Err(short_error(error));
        }
        Ok(value)
    }

    /// Try Invidious and Piped variants across every endpoint until one answers.
    async fn fetch(&self, invidious_path: &str, piped_path: &str) -> Result<Value, String> {
        let mut last_err = "no API endpoints configured".to_string();
        for endpoint in self.ordered() {
            let candidates: Vec<(&str, Kind)> = match endpoint.kind {
                Kind::Invidious => vec![(invidious_path, Kind::Invidious)],
                Kind::Piped => vec![(piped_path, Kind::Piped)],
                Kind::Auto => vec![(invidious_path, Kind::Invidious), (piped_path, Kind::Piped)],
            };
            for (path, _kind) in candidates {
                if path.is_empty() {
                    continue;
                }
                let url = format!("{}{}", endpoint.base.trim_end_matches('/'), path);
                match self.get_one(&url).await {
                    Ok(value) => {
                        self.remember(&endpoint.base);
                        return Ok(value);
                    }
                    Err(err) => {
                        last_err = format!("{} ({})", endpoint.base, err);
                    }
                }
            }
        }
        Err(format!(
            "Could not reach a video API. Last error: {last_err}. \
             Check your connection, or set a working instance in Settings."
        ))
    }

    pub async fn trending(&self, region: &str) -> Result<Vec<VideoCard>, String> {
        let region = region.trim().to_uppercase();
        let region = if region.len() == 2 { region } else { "US".to_string() };
        let inv = format!("/api/v1/trending?region={region}");
        let piped = format!("/trending?region={region}");
        let value = self.fetch(&inv, &piped).await?;
        Ok(parse_video_list(&value))
    }

    pub async fn search(&self, query: &str, page: u32) -> Result<Vec<VideoCard>, String> {
        let encoded = urlencode(query);
        let inv = format!("/api/v1/search?q={encoded}&type=video&page={page}");
        let piped = format!("/search?q={encoded}&filter=videos");
        let value = self.fetch(&inv, &piped).await?;
        Ok(parse_video_list(&value))
    }

    /// Channel-only search, used both by the full search view and by the live
    /// suggestions under the search field.
    pub async fn search_channels(&self, query: &str) -> Result<Vec<ChannelCard>, String> {
        let encoded = urlencode(query);
        let invidious = format!("/api/v1/search?q={encoded}&type=channel");
        let piped = format!("/search?q={encoded}&filter=channels");
        let value = self.fetch(&invidious, &piped).await?;
        Ok(parse_channels(&value))
    }

    /// Videos, channels and playlists together. A channel or playlist failure
    /// must not hide the videos.
    pub async fn search_all(&self, query: &str, page: u32) -> Result<SearchResults, String> {
        let (videos, channels, playlists) = futures::join!(
            self.search(query, page),
            self.search_channels(query),
            self.search_playlists(query, page)
        );
        Ok(SearchResults {
            videos: videos?,
            channels: channels.unwrap_or_default(),
            playlists: playlists.unwrap_or_default(),
        })
    }

    /// Short list for the quick-select dropdown.
    pub async fn channel_suggestions(&self, query: &str) -> Result<Vec<ChannelCard>, String> {
        let mut channels = self.search_channels(query).await?;
        channels.truncate(6);
        Ok(channels)
    }

    /// Search terms and channels for the dropdown under the search field.
    /// Neither half is worth failing the other one over.
    pub async fn suggest(&self, query: &str) -> (Vec<String>, Vec<ChannelCard>) {
        let (terms, channels) = futures::join!(self.search_terms(query), self.channel_suggestions(query));
        (terms.unwrap_or_default(), channels.unwrap_or_default())
    }

    /// Autocomplete terms from the instance. Invidious returns an object with
    /// a `suggestions` array; Piped returns a bare array of strings.
    pub async fn search_terms(&self, query: &str) -> Result<Vec<String>, String> {
        let encoded = urlencode(query);
        let invidious = format!("/api/v1/search/suggestions?q={encoded}");
        let piped = format!("/suggestions?query={encoded}");
        let value = self.fetch(&invidious, &piped).await?;
        Ok(parse_suggestions(&value, query))
    }

    /// YouTube playlists only. Paged search repeats the same playlists on most
    /// instances, so the playlist section stays on its first page.
    pub async fn search_playlists(
        &self,
        query: &str,
        page: u32,
    ) -> Result<Vec<PlaylistCard>, String> {
        if page > 1 {
            return Ok(Vec::new());
        }
        let encoded = urlencode(query);
        let invidious = format!("/api/v1/search?q={encoded}&type=playlist");
        let piped = format!("/search?q={encoded}&filter=playlists");
        let value = self.fetch(&invidious, &piped).await?;
        Ok(parse_playlists(&value))
    }

    /// The full contents of a remote playlist, so a walkthrough can be queued
    /// and watched in order without a browser.
    pub async fn playlist(&self, id: &str) -> Result<RemotePlaylist, String> {
        let encoded = urlencode(id);
        let invidious = format!("/api/v1/playlists/{encoded}");
        let piped = format!("/playlists/{encoded}");
        let value = self.fetch(&invidious, &piped).await?;

        let is_piped = value.get("relatedStreams").is_some()
            || (value.get("name").is_some() && value.get("title").is_none());
        let title = if is_piped {
            s(&value, "name")
        } else {
            s(&value, "title")
        };
        if title.trim().is_empty() {
            return Err("Playlist unavailable".into());
        }
        let channel_name = if is_piped {
            s(&value, "uploader")
        } else {
            s(&value, "author")
        };
        let channel_id = if is_piped {
            s(&value, "uploaderUrl")
                .trim_start_matches("/channel/")
                .to_string()
        } else {
            s(&value, "authorId")
        };
        let thumbnail = {
            let raw = if is_piped {
                s(&value, "thumbnailUrl")
            } else {
                s(&value, "playlistThumbnail")
            };
            normalize_url(&raw)
        };
        let list_key = if is_piped { "relatedStreams" } else { "videos" };
        let videos = value
            .get(list_key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| if is_piped { piped_card(item) } else { invidious_card(item) })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(RemotePlaylist {
            id: id.to_string(),
            title,
            channel_id,
            channel_name,
            thumbnail,
            videos,
        })
    }

    /// Caption tracks for a video. The URLs are made absolute against the
    /// instance that answered, because Invidious returns them as paths.
    pub async fn caption_tracks(&self, video_id: &str) -> Result<Vec<CaptionTrack>, String> {
        let encoded = urlencode(video_id);
        let invidious = format!("/api/v1/captions/{encoded}");
        let piped = format!("/captions/{encoded}");
        let value = self.fetch(&invidious, &piped).await?;
        let base = self.current_endpoint();
        let items = value
            .as_array()
            .cloned()
            .or_else(|| value.get("captions").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();

        Ok(items
            .iter()
            .filter_map(|item| {
                let raw = s(item, "url");
                if raw.is_empty() {
                    return None;
                }
                let url = if raw.starts_with("http") {
                    raw
                } else if raw.starts_with("//") {
                    format!("https:{raw}")
                } else if raw.starts_with('/') {
                    format!("{base}{raw}")
                } else {
                    format!("{base}/{raw}")
                };
                let label = {
                    let label = s(item, "label");
                    if label.is_empty() { s(item, "name") } else { label }
                };
                let language = {
                    let code = s(item, "language_code");
                    if !code.is_empty() {
                        code
                    } else {
                        let language = s(item, "language");
                        if language.is_empty() { s(item, "code") } else { language }
                    }
                };
                Some(CaptionTrack { label, language, url })
            })
            .collect())
    }

    /// Plain-text transcript with `[h:mm:ss]` stamps, ready for a summary.
    pub async fn transcript(&self, video_id: &str) -> Result<String, String> {
        let tracks = self.caption_tracks(video_id).await?;
        let track = tracks
            .iter()
            .find(|track| {
                track.language.to_lowercase().starts_with("en")
                    || track.label.to_lowercase().contains("english")
            })
            .or_else(|| tracks.first())
            .ok_or("This video has no captions, so there is nothing to summarize.")?;
        let raw = self.text(&track.url).await?;
        if !raw.contains("-->") {
            return Err("The caption track could not be read.".into());
        }
        let transcript = vtt_to_transcript(&raw);
        if transcript.trim().is_empty() {
            return Err("The caption track was empty.".into());
        }
        Ok(transcript)
    }

    async fn text(&self, url: &str) -> Result<String, String> {
        let res = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| friendly_transport_error(&e))?;
        if !res.status().is_success() {
            return Err(format!("Caption request failed (HTTP {})", res.status().as_u16()));
        }
        res.text().await.map_err(|e| e.to_string())
    }

    pub async fn video(&self, id: &str) -> Result<VideoDetail, String> {
        let encoded = urlencode(id);
        let inv = format!("/api/v1/videos/{encoded}");
        let piped = format!("/streams/{encoded}");
        let value = self.fetch(&inv, &piped).await?;

        if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
            return Err(short_error(err));
        }

        // `/streams/{id}` has no `uploaderName` (it calls the field `uploader`)
        // and no `url`, so detect the payload by its shape, not by one field.
        let is_piped = value.get("uploaderName").is_some()
            || value.get("relatedStreams").is_some()
            || value.get("videoStreams").is_some();
        let mut card = if is_piped {
            piped_card_with_id(&value, id).ok_or_else(|| "Video unavailable".to_string())?
        } else {
            invidious_card(&value).ok_or_else(|| "Video unavailable".to_string())?
        };

        // Detail payloads sometimes omit fields the list payload carries.
        if card.title.is_empty() {
            card.title = "Untitled".into();
        }
        let description = clean_description(&value);
        let likes = value
            .get("likes")
            .or_else(|| value.get("likeCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let related_key = if is_piped { "relatedStreams" } else { "recommendedVideos" };
        let related = value
            .get(related_key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        if is_piped {
                            piped_card(item)
                        } else {
                            invidious_card(item)
                        }
                    })
                    .filter(|c| c.id != card.id)
                    .take(24)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(VideoDetail { card, description, likes, related })
    }

    pub async fn channel(&self, id: &str) -> Result<ChannelSummary, String> {
        let encoded = urlencode(id);
        let inv = format!("/api/v1/channels/{encoded}");
        let piped = format!("/channel/{encoded}");
        let value = self.fetch(&inv, &piped).await?;

        let is_piped = value.get("name").is_some() && value.get("author").is_none();
        let name = if is_piped {
            s(&value, "name")
        } else {
            s(&value, "author")
        };
        let avatar = if is_piped {
            s(&value, "avatarUrl")
        } else {
            pick_avatar(&value, "authorThumbnails")
        };
        let banner = if is_piped {
            s(&value, "bannerUrl")
        } else {
            pick_banner(&value, "authorBanners")
        };
        let subscribers = if is_piped {
            value.get("subscriberCount").and_then(|v| v.as_u64()).unwrap_or(0)
        } else {
            value.get("subCount").and_then(|v| v.as_u64()).unwrap_or(0)
        };
        let description = if is_piped {
            clean_description(&value)
        } else {
            s(&value, "description")
        };

        let list_key = if is_piped { "relatedStreams" } else { "latestVideos" };
        let videos = value
            .get(list_key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| if is_piped { piped_card(item) } else { invidious_card(item) })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if name.is_empty() {
            return Err("Channel unavailable".into());
        }

        Ok(ChannelSummary {
            id: if id.is_empty() { s(&value, "authorId") } else { id.to_string() },
            name,
            avatar,
            banner,
            subscribers,
            description,
            videos,
        })
    }

    pub async fn channel_videos(&self, id: &str, page: u32) -> Result<Vec<VideoCard>, String> {
        let encoded = urlencode(id);
        let inv = format!("/api/v1/channels/{encoded}/videos?page={page}");
        let value = self.fetch(&inv, "").await?;
        Ok(value
            .get("videos")
            .and_then(|v| v.as_array())
            .map(|items| items.iter().filter_map(invidious_card).collect())
            .unwrap_or_default())
    }
}

/// Pull chapters out of a description that lists leading timestamps, which is
/// how most long walkthroughs are organised. Two or more increasing stamps are
/// required so a stray "12:34" in prose is not mistaken for a chapter list.
pub fn parse_chapters(description: &str) -> Vec<Chapter> {
    let mut chapters: Vec<Chapter> = Vec::new();
    for line in description.lines().take(400) {
        let line = line.trim();
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        let Some((seconds, rest)) = leading_stamp(line) else {
            continue;
        };
        if !(0..=24 * 3600).contains(&seconds) {
            continue;
        }
        let title = rest
            .trim_start_matches(|ch: char| {
                ch.is_whitespace() || matches!(ch, '-' | '–' | '—' | ':' | '|' | '·')
            })
            .trim();
        if title.is_empty() || title.chars().count() > 120 {
            continue;
        }
        if chapters
            .last()
            .map(|chapter| seconds <= chapter.seconds)
            .unwrap_or(false)
        {
            continue;
        }
        chapters.push(Chapter {
            seconds,
            title: title.to_string(),
        });
        if chapters.len() >= 40 {
            break;
        }
    }
    if chapters.len() >= 2 {
        chapters
    } else {
        Vec::new()
    }
}

/// `0:00`, `12:34` or `1:02:03` at the start of a line, followed by a
/// separator or the chapter name.
fn leading_stamp(line: &str) -> Option<(i64, &str)> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    // Up to three digits before the colon: walkthroughs often pass 99
    // minutes, which is written as `102:15`.
    if index == 0 || index > 3 {
        return None;
    }
    let mut cursor = index;
    let mut parts = 1;
    while cursor < bytes.len() && (bytes[cursor] == b':' || bytes[cursor].is_ascii_digit()) {
        if bytes[cursor] == b':' {
            parts += 1;
        }
        cursor += 1;
    }
    if !(2..=3).contains(&parts) {
        return None;
    }
    let stamp = &line[..cursor];
    let rest = &line[cursor..];
    if rest
        .chars()
        .next()
        .is_some_and(|ch| {
            !ch.is_whitespace() && !matches!(ch, '-' | '–' | '—' | ':' | '|' | '·')
        })
    {
        return None;
    }
    let mut seconds = 0i64;
    for piece in stamp.split(':') {
        seconds = seconds * 60 + piece.parse::<i64>().ok()?;
    }
    Some((seconds, rest))
}

/// Find a part/episode number and the stable text before it, so "Part 12" and
/// "Episode 12" can be matched against the next item from the same channel.
pub fn series_point(title: &str) -> Option<SeriesPoint> {
    let lower = title.to_lowercase();
    let markers = ["episode", "part", "ep.", "pt.", "ep", "pt"];
    let mut found: Option<(usize, u32)> = None;

    for marker in markers {
        let mut from = 0;
        while let Some(offset) = lower[from..].find(marker) {
            let start = from + offset;
            let end = start + marker.len();
            let before_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
            let after = &lower[end..];
            let next = after.chars().next();
            let boundary_ok = next
                .map(|ch| {
                    ch.is_whitespace() || ch.is_ascii_digit() || matches!(ch, '#' | '.' | ':' | '-' | '–')
                })
                .unwrap_or(false);
            if before_ok && boundary_ok {
                let trimmed = after.trim_start_matches(|ch: char| {
                    ch.is_whitespace() || matches!(ch, '#' | '.' | ':' | '-' | '–')
                });
                let digits: String = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).collect();
                if let Ok(number) = digits.parse::<u32>() {
                    if (1..=9999).contains(&number) {
                        found = Some((start, number));
                        break;
                    }
                }
            }
            from = end;
        }
        if found.is_some() {
            break;
        }
    }

    // "Game #12" is common too.
    if found.is_none() {
        let mut from = 0;
        while let Some(offset) = lower[from..].find('#') {
            let start = from + offset;
            let digits: String = lower[start + 1..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            if let Ok(number) = digits.parse::<u32>() {
                if (1..=9999).contains(&number) {
                    found = Some((start, number));
                    break;
                }
            }
            from = start + 1;
        }
    }

    let (start, number) = found?;
    let prefix = title[..start]
        .trim_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, '-' | '–' | '—' | ':' | '|' | '·' | '.')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if prefix.chars().count() > 120 {
        return None;
    }
    Some(SeriesPoint { prefix, number })
}

/// The next part in the same channel's series, when the related list has it.
pub fn next_episode(
    current_title: &str,
    cards: &[VideoCard],
    channel_id: &str,
) -> Option<VideoCard> {
    let current = series_point(current_title)?;
    let target = current.number + 1;
    let mut best: Option<(u8, &VideoCard)> = None;
    for card in cards {
        if !channel_id.is_empty() && !card.channel_id.is_empty() && card.channel_id != channel_id {
            continue;
        }
        let Some(point) = series_point(&card.title) else {
            continue;
        };
        if point.number != target {
            continue;
        }
        let score = if point.prefix == current.prefix {
            3
        } else if point.prefix.is_empty() || current.prefix.is_empty() {
            2
        } else if point.prefix.contains(&current.prefix)
            || current.prefix.contains(&point.prefix)
        {
            1
        } else {
            0
        };
        if score > 0 && best.map(|(best, _)| score > best).unwrap_or(true) {
            best = Some((score, card));
            if score == 3 {
                break;
            }
        }
    }
    best.map(|(_, card)| card.clone())
}

fn parse_suggestions(value: &Value, query: &str) -> Vec<String> {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => {
                for item in items {
                    collect(item, out);
                }
            }
            Value::Object(map) => {
                if let Some(suggestions) = map.get("suggestions") {
                    collect(suggestions, out);
                }
            }
            _ => {}
        }
    }

    let mut raw = Vec::new();
    collect(value, &mut raw);
    let mut terms: Vec<String> = Vec::new();
    for term in raw {
        let term = term.trim().to_string();
        if term.is_empty() || term.eq_ignore_ascii_case(query.trim()) {
            continue;
        }
        if !terms.iter().any(|existing| existing.eq_ignore_ascii_case(&term)) {
            terms.push(term);
        }
        if terms.len() >= 7 {
            break;
        }
    }
    terms
}

fn parse_playlists(value: &Value) -> Vec<PlaylistCard> {
    let items = if let Some(items) = value.as_array() {
        items.clone()
    } else if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        items.clone()
    } else {
        Vec::new()
    };

    items
        .iter()
        .filter_map(|item| {
            let id = {
                let direct = s(item, "playlistId");
                if !direct.is_empty() {
                    direct
                } else {
                    s(item, "url")
                        .split("list=")
                        .nth(1)
                        .unwrap_or("")
                        .split('&')
                        .next()
                        .unwrap_or("")
                        .to_string()
                }
            };
            if id.is_empty() {
                return None;
            }
            let title = {
                let title = s(item, "title");
                if title.is_empty() { s(item, "name") } else { title }
            };
            if title.is_empty() {
                return None;
            }
            let channel_name = {
                let author = s(item, "author");
                if author.is_empty() { s(item, "uploaderName") } else { author }
            };
            let channel_id = {
                let id = s(item, "authorId");
                if id.is_empty() {
                    s(item, "uploaderUrl")
                        .trim_start_matches("/channel/")
                        .to_string()
                } else {
                    id
                }
            };
            let video_count = item
                .get("videoCount")
                .or_else(|| item.get("videos"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0);
            let thumbnail = {
                let raw = s(item, "playlistThumbnail");
                if raw.is_empty() {
                    normalize_url(&s(item, "thumbnail"))
                } else {
                    normalize_url(&raw)
                }
            };
            Some(PlaylistCard {
                id,
                title,
                channel_id,
                channel_name,
                video_count,
                thumbnail,
            })
        })
        .take(12)
        .collect()
}

/// Turn a WebVTT caption file into `[h:mm:ss] text` lines, dropping cue
/// numbers, styling tags and the rolling duplicates YouTube's automatic
/// captions repeat on every cue.
pub fn vtt_to_transcript(raw: &str) -> String {
    let mut out = String::new();
    let mut cue = String::new();
    let mut last = String::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty()
            || line == "WEBVTT"
            || line.starts_with("NOTE")
            || line.starts_with("Kind:")
            || line.starts_with("Language:")
        {
            continue;
        }
        if line.contains("-->") {
            cue = line
                .split("-->")
                .next()
                .map(|start| format_stamp(start.trim()))
                .unwrap_or_default();
            continue;
        }
        if line.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let text = strip_vtt_tags(line);
        if text.is_empty() || text == last {
            continue;
        }
        last = text.clone();
        if !cue.is_empty() {
            out.push('[');
            out.push_str(&cue);
            out.push_str("] ");
        }
        out.push_str(&text);
        out.push('\n');
    }
    out.trim().to_string()
}

fn strip_vtt_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_tag = false;
    for ch in line.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .trim()
        .to_string()
}

/// `00:01:23.450` or `01:23.450` to `1:23`.
fn format_stamp(raw: &str) -> String {
    let raw = raw.split_whitespace().next().unwrap_or(raw);
    let mut parts = raw.split(':');
    let first = parts.next().unwrap_or("0");
    let second = parts.next();
    let third = parts.next();
    let (hours, minutes, seconds) = match (second, third) {
        (Some(minutes), Some(seconds)) => (first, minutes, seconds),
        (Some(seconds), None) => ("0", first, seconds),
        _ => ("0", "0", first),
    };
    let hours: i64 = hours.trim().parse().unwrap_or(0);
    let minutes: i64 = minutes.trim().parse().unwrap_or(0);
    let seconds: i64 = seconds.split('.').next().unwrap_or("0").trim().parse().unwrap_or(0);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn friendly_transport_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "request timed out".into()
    } else if err.is_connect() {
        "connection failed".into()
    } else {
        err.to_string()
    }
}

fn short_error(raw: &str) -> String {
    if raw.contains("SignInConfirmNotBot") || raw.contains("LOGIN_REQUIRED") {
        "This instance is blocked by YouTube's bot check. Try another instance in Settings.".into()
    } else if raw.contains("unavailable") || raw.contains("private") {
        "This video is unavailable on the current instance.".into()
    } else {
        let first = raw.lines().next().unwrap_or(raw);
        first.chars().take(160).collect()
    }
}

fn parse_video_list(value: &Value) -> Vec<VideoCard> {
    let items = if let Some(arr) = value.as_array() {
        arr.clone()
    } else if let Some(arr) = value.get("items").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = value.get("videos").and_then(|v| v.as_array()) {
        arr.clone()
    } else {
        Vec::new()
    };

    items
        .iter()
        .filter_map(|item| {
            if item.get("videoId").is_some() {
                invidious_card(item)
            } else {
                piped_card(item)
            }
        })
        .collect()
}

fn parse_channels(value: &Value) -> Vec<ChannelCard> {
    let items = value
        .as_array()
        .cloned()
        .or_else(|| value.get("items").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();

    items
        .iter()
        .filter_map(|item| {
            // Invidious: author/authorId. Piped: name + /channel/<id> url.
            let id = if let Some(id) = item.get("authorId").and_then(|v| v.as_str()) {
                id.to_string()
            } else {
                s(item, "url").trim_start_matches("/channel/").to_string()
            };
            let name = match item.get("author").and_then(|v| v.as_str()) {
                Some(author) if !author.is_empty() => author.to_string(),
                _ => s(item, "name"),
            };
            if id.is_empty() || name.is_empty() {
                return None;
            }
            let invidious_avatar = pick_avatar(item, "authorThumbnails");
            let avatar = if invidious_avatar.is_empty() {
                s(item, "thumbnail")
            } else {
                normalize_url(&invidious_avatar)
            };
            let subscribers = item
                .get("subCount")
                .or_else(|| item.get("subscribers"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(ChannelCard {
                id,
                name,
                avatar,
                subscribers,
                description: clean_description(item),
            })
        })
        .take(24)
        .collect()
}

/// Invidious returns protocol-relative avatar URLs (`//yt3...`).
fn normalize_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

fn invidious_card(v: &Value) -> Option<VideoCard> {
    let id = s(v, "videoId");
    if id.is_empty() {
        return None;
    }
    let published_unix = v.get("published").and_then(|x| x.as_i64()).unwrap_or(0);
    let published = {
        let text = s(v, "publishedText");
        if text.is_empty() { relative_date(published_unix) } else { text }
    };
    let live = v.get("liveNow").and_then(|x| x.as_bool()).unwrap_or(false)
        || v.get("isLiveNow").and_then(|x| x.as_bool()).unwrap_or(false);
    Some(VideoCard {
        thumbnail: thumb(&id, "mq"),
        id,
        title: s(v, "title"),
        channel_id: s(v, "authorId"),
        channel_name: s(v, "author"),
        channel_avatar: pick_avatar(v, "authorThumbnails"),
        duration: v.get("lengthSeconds").and_then(|x| x.as_i64()).unwrap_or(0).max(0),
        views: v.get("viewCount").and_then(|x| x.as_u64()).unwrap_or(0),
        published,
        published_unix,
        live,
    })
}

fn piped_card(v: &Value) -> Option<VideoCard> {
    piped_card_with_id(v, "")
}

/// `fallback_id` covers the `/streams/{id}` payload, which carries no `url` and
/// no `videoId`: the caller already knows which video it asked for.
fn piped_card_with_id(v: &Value, fallback_id: &str) -> Option<VideoCard> {
    let url = s(v, "url");
    let mut id = if !url.is_empty() {
        url.split("v=").last().unwrap_or("").split('&').next().unwrap_or("").to_string()
    } else {
        s(v, "videoId")
    };
    if id.is_empty() {
        id = fallback_id.to_string();
    }
    if id.len() != 11 {
        return None;
    }
    let channel_id = s(v, "uploaderUrl").trim_start_matches("/channel/").to_string();
    let uploaded_ms = v.get("uploaded").and_then(|x| x.as_i64()).unwrap_or(0);
    let published_unix = if uploaded_ms > 10_000_000_000 { uploaded_ms / 1000 } else { uploaded_ms };
    let published = {
        let text = s(v, "uploadedDate");
        if text.is_empty() { relative_date(published_unix) } else { text }
    };
    let duration = v.get("duration").and_then(|x| x.as_i64()).unwrap_or(0).max(0);
    // Piped uses duration -1 for ordinary trending items too; only an explicit
    // live flag means the item is actually live.
    let live = v.get("livestream").and_then(|x| x.as_bool()).unwrap_or(false)
        || v.get("isLive").and_then(|x| x.as_bool()).unwrap_or(false);
    Some(VideoCard {
        thumbnail: thumb(&id, "mq"),
        id,
        title: s(v, "title"),
        channel_id,
        channel_name: {
            let named = s(v, "uploaderName");
            if named.is_empty() { s(v, "uploader") } else { named }
        },
        channel_avatar: s(v, "uploaderAvatar"),
        duration,
        views: v.get("views").and_then(|x| x.as_u64()).unwrap_or(0),
        published,
        published_unix,
        live,
    })
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).map(|x| x.to_string()).unwrap_or_default()
}

fn thumb(id: &str, quality: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/{quality}default.jpg")
}

fn pick_avatar(v: &Value, key: &str) -> String {
    let arr = match v.get(key).and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return String::new(),
    };
    // Prefer a crisp mid-size avatar; fall back to the largest available.
    let mut best: Option<(&Value, u64)> = None;
    for item in arr {
        if item.get("url").and_then(Value::as_str).unwrap_or("").trim().is_empty() {
            continue;
        }
        let width = item.get("width").and_then(|w| w.as_u64()).unwrap_or(0);
        let score = if (76..=176).contains(&width) { u64::MAX - (176 - width) } else { width };
        if best.map(|(_, b)| score > b).unwrap_or(true) {
            best = Some((item, score));
        }
    }
    best.map(|(item, _)| s(item, "url")).unwrap_or_default()
}

fn pick_banner(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .map(|item| s(item, "url"))
        .unwrap_or_default()
}

fn clean_description(v: &Value) -> String {
    ["description", "shortDescription", "descriptionHtml"]
        .iter()
        .filter_map(|key| v.get(*key)?.as_str())
        .map(strip_html)
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

fn strip_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                // Preserve line breaks for the tags that imply them.
                let rest: String = chars.clone().take(4).collect();
                if rest.starts_with("br") || rest.starts_with("/p") {
                    out.push('\n');
                }
                in_tag = true;
            }
            '>' => in_tag = false,
            _ if in_tag => {}
            '&' => {
                let mut entity = String::new();
                while let Some(&next) = chars.peek() {
                    if next == ';' || entity.len() > 8 {
                        chars.next();
                        break;
                    }
                    entity.push(next);
                    chars.next();
                }
                let decoded = match entity.as_str() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    "#39" => "'",
                    "nbsp" => " ",
                    _ => "",
                };
                if decoded.is_empty() {
                    if entity.starts_with('#') {
                        if let Ok(code) = entity[1..].parse::<u32>() {
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            }
                        }
                    } else {
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                } else {
                    out.push_str(decoded);
                }
            }
            _ => out.push(ch),
        }
    }
    out.trim().to_string()
}

/// yt-dlp descriptions are plain text (keep literal angle brackets and ampersands).
pub fn ytdlp_detail(value: &Value, id: &str) -> Result<VideoDetail, String> {
    let description = value.get("description").and_then(Value::as_str);
    let title = s(value, "title");
    if value.get("id").and_then(Value::as_str) != Some(id) || title.trim().is_empty() {
        return Err("Invalid video metadata".into());
    }
    let description = description.ok_or("The source did not return a description field")?;
    let published_unix = chrono::NaiveDate::parse_from_str(&s(value, "upload_date"), "%Y%m%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map_or(0, |date| date.and_utc().timestamp());
    Ok(VideoDetail {
        card: VideoCard {
            id: id.to_string(),
            title,
            channel_name: s(value, "channel"),
            channel_id: s(value, "channel_id"),
            channel_avatar: String::new(),
            duration: value.get("duration").and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as i64,
            views: value.get("view_count").and_then(Value::as_u64).unwrap_or(0),
            published: relative_date(published_unix),
            published_unix,
            thumbnail: thumb(id, "mq"),
            live: false,
        },
        description: description.trim().to_string(),
        likes: 0,
        related: Vec::new(),
    })
}

fn relative_date(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    let then = match chrono::DateTime::from_timestamp(unix, 0) {
        Some(dt) => dt,
        None => return String::new(),
    };
    let now = chrono::Utc::now();
    let days = (now - then).num_days();
    match days {
        0 => "today".into(),
        1 => "1 day ago".into(),
        2..=30 => format!("{days} days ago"),
        31..=364 => {
            let months = days / 30;
            if months <= 1 { "1 month ago".into() } else { format!("{months} months ago") }
        }
        _ => {
            let years = days / 365;
            if years <= 1 { "1 year ago".into() } else { format!("{years} years ago") }
        }
    }
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_invidious_item() {
        let raw = serde_json::json!({
            "type": "video",
            "title": "Example",
            "videoId": "abcdefghijk",
            "author": "Someone",
            "authorId": "UC123",
            "lengthSeconds": 120,
            "viewCount": 42,
            "published": 1700000000i64,
            "publishedText": "2 days ago",
            "authorThumbnails": [{"url": "https://example/a=s48", "width": 48}, {"url": "https://example/b=s100", "width": 100}]
        });
        let card = invidious_card(&raw).expect("card");
        assert_eq!(card.id, "abcdefghijk");
        assert_eq!(card.channel_id, "UC123");
        assert_eq!(card.duration, 120);
        assert_eq!(card.channel_avatar, "https://example/b=s100");
        assert_eq!(card.published, "2 days ago");
    }

    #[test]
    fn parses_piped_item() {
        let raw = serde_json::json!({
            "url": "/watch?v=abcdefghijk",
            "title": "Example",
            "uploaderName": "Someone",
            "uploaderUrl": "/channel/UC123",
            "duration": -1,
            "livestream": true,
            "views": 7,
            "uploaded": 1700000000000i64,
            "uploadedDate": "2 days ago"
        });
        let card = piped_card(&raw).expect("card");
        assert_eq!(card.id, "abcdefghijk");
        assert_eq!(card.channel_id, "UC123");
        assert_eq!(card.duration, 0);
        assert!(card.live);
        assert_eq!(card.published_unix, 1700000000);
    }

    #[test]
    fn parses_invidious_channel_results() {
        let raw = serde_json::json!([{
            "type": "channel",
            "author": "CboysTV",
            "authorId": "UCBNnILlexKYtJu-EGUvq_iA",
            "subCount": 5660000,
            "description": "New videos every Thursday!",
            "authorThumbnails": [
                {"url": "//yt3.googleusercontent.com/a=s48", "width": 48},
                {"url": "//yt3.googleusercontent.com/b=s100", "width": 100}
            ]
        }]);
        let channels = parse_channels(&raw);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "CboysTV");
        assert_eq!(channels[0].id, "UCBNnILlexKYtJu-EGUvq_iA");
        assert_eq!(channels[0].subscribers, 5_660_000);
        assert_eq!(channels[0].avatar, "https://yt3.googleusercontent.com/b=s100");
    }

    #[test]
    fn parses_piped_stream_detail_without_uploader_name() {
        // Shape of GET /streams/<id>: no url, no videoId, no uploaderName.
        let raw = serde_json::json!({
            "title": "Rick Astley - Never Gonna Give You Up",
            "uploader": "Rick Astley",
            "uploaderUrl": "/channel/UCuAXFkgsw1L7xaCfnd5JJOw",
            "duration": 213,
            "views": 1600000000u64,
            "uploaded": 1766955925459u64,
            "relatedStreams": []
        });
        let card = piped_card_with_id(&raw, "dQw4w9WgXcQ").expect("a card from /streams");
        assert_eq!(card.id, "dQw4w9WgXcQ");
        assert_eq!(card.channel_name, "Rick Astley");
        assert_eq!(card.duration, 213);
        assert_eq!(card.channel_id, "UCuAXFkgsw1L7xaCfnd5JJOw");
    }

    #[test]
    fn piped_trending_negative_duration_is_not_live() {
        let raw = serde_json::json!({
            "url": "/watch?v=dQw4w9WgXcQ",
            "title": "Regular trending video",
            "duration": -1,
            "views": 1000,
            "uploaderName": "Some Channel"
        });
        let card = piped_card_with_id(&raw, "dQw4w9WgXcQ").expect("a card");
        assert!(!card.live);
        assert_eq!(card.duration, 0);
    }

    #[test]
    fn parses_piped_channel_results() {
        let raw = serde_json::json!({
            "items": [{
                "type": "channel",
                "url": "/channel/UCBNnILlexKYtJu-EGUvq_iA",
                "name": "CboysTV",
                "thumbnail": "https://proxy.example/ytc/abc=s176",
                "subscribers": 5660000,
                "videos": -1,
                "description": "New videos every Thursday!"
            }]
        });
        let channels = parse_channels(&raw);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id, "UCBNnILlexKYtJu-EGUvq_iA");
        assert_eq!(channels[0].avatar, "https://proxy.example/ytc/abc=s176");
        assert_eq!(channels[0].subscribers, 5_660_000);
    }

    #[test]
    fn strips_html_and_entities() {
        let cleaned = strip_html("Hello<br>world &amp; friends <b>bold</b>");
        assert_eq!(cleaned, "Hello\nworld & friends bold");
    }

    #[tokio::test]
    #[ignore = "requires internet access to a live Invidious/Piped instance"]
    async fn live_instance_smoke_test() {
        let api = Api::new(None);
        let trending = api.trending("US").await.expect("trending should load");
        assert!(!trending.is_empty(), "expected trending videos");
        assert_eq!(trending[0].id.len(), 11);
        assert!(trending[0].thumbnail.contains("i.ytimg.com"));

        // Detail and channel endpoints are the ones instances disable first, so
        // accept either a real answer or a clean error rather than hanging.
        match api.video(&trending[0].id).await {
            Ok(detail) => assert!(!detail.card.title.is_empty()),
            Err(err) => eprintln!("detail endpoint unavailable: {err}"),
        }
        match api.channel(&trending[0].channel_id).await {
            Ok(channel) => assert!(!channel.name.is_empty()),
            Err(err) => eprintln!("channel endpoint unavailable: {err}"),
        }

        // The case that motivated channel search: a creator name, not a video.
        let channels = api.search_channels("cboys").await.expect("channel search");
        assert!(!channels.is_empty(), "channel search returned nothing");
        assert!(
            channels
                .iter()
                .any(|channel| channel.name.to_lowercase().contains("cboys")),
            "expected a Cboys channel, got {:?}",
            channels.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        assert!(!channels[0].id.is_empty());

        let combined = api.search_all("cboys", 1).await.expect("combined search");
        assert!(!combined.channels.is_empty(), "combined search lost channels");
    }

    #[test]
    fn parses_invidious_playlist_results() {
        let raw = serde_json::json!([{
            "type": "playlist",
            "title": "Full Walkthrough",
            "playlistId": "PLabcdefghijklmnop",
            "playlistThumbnail": "//i.ytimg.com/vi/abcdefghijk/mqdefault.jpg",
            "author": "Someone",
            "authorId": "UC123",
            "videoCount": 42
        }]);
        let playlists = parse_playlists(&raw);
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].id, "PLabcdefghijklmnop");
        assert_eq!(playlists[0].video_count, 42);
        assert_eq!(playlists[0].thumbnail, "https://i.ytimg.com/vi/abcdefghijk/mqdefault.jpg");
    }

    #[test]
    fn parses_piped_playlist_results() {
        let raw = serde_json::json!({ "items": [{
            "type": "playlist",
            "url": "/playlist?list=PLzzzzzzzzzzzzzzzz",
            "name": "Boss guide",
            "uploaderName": "Guide Maker",
            "uploaderUrl": "/channel/UC999",
            "videos": 7
        }] });
        let playlists = parse_playlists(&raw);
        assert_eq!(playlists[0].id, "PLzzzzzzzzzzzzzzzz");
        assert_eq!(playlists[0].title, "Boss guide");
        assert_eq!(playlists[0].channel_id, "UC999");
        assert_eq!(playlists[0].video_count, 7);
    }

    #[test]
    fn parses_suggestions_from_both_shapes() {
        let invidious = serde_json::json!({
            "query": "zelda",
            "suggestions": ["zelda walkthrough", "zelda tears of the kingdom"]
        });
        assert_eq!(
            parse_suggestions(&invidious, "zelda"),
            vec!["zelda walkthrough", "zelda tears of the kingdom"]
        );
        let piped = serde_json::json!(["zelda", ["zelda bosses", "zelda map"]]);
        assert_eq!(parse_suggestions(&piped, "zelda"), vec!["zelda bosses", "zelda map"]);
    }

    #[test]
    fn turns_vtt_into_stamped_transcript() {
        let raw = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:03.000\nHello <c>there</c>\n\n2\n00:01:05.500 --> 00:01:08.000\nHello there\n\n3\n00:01:05.500 --> 00:01:08.000\nSecond line &amp; more\n";
        let text = vtt_to_transcript(raw);
        assert_eq!(text, "[0:01] Hello there\n[1:05] Second line & more");
    }

    #[test]
    fn parses_chapters_only_from_leading_stamps() {
        let description = "Support the channel: https://example.com\n\n00:00 Intro\n02:15 First boss\n1:04:30 Ending\n\nThanks for watching!";
        let chapters = parse_chapters(description);
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[0].seconds, 0);
        assert_eq!(chapters[1].title, "First boss");
        assert_eq!(chapters[2].seconds, 3870);
    }

    #[test]
    fn chapters_need_two_increasing_entries() {
        assert!(parse_chapters("12:34 one line only").is_empty());
        assert!(parse_chapters("10:00 A\n05:00 B").is_empty());
    }

    #[test]
    fn detects_part_and_episode_numbers() {
        assert_eq!(
            series_point("Elden Ring Walkthrough - Part 12: Liurnia"),
            Some(SeriesPoint {
                prefix: "elden ring walkthrough".into(),
                number: 12
            })
        );
        assert_eq!(
            series_point("Hollow Knight Ep. 7 - Mantis Lords"),
            Some(SeriesPoint {
                prefix: "hollow knight".into(),
                number: 7
            })
        );
        assert!(series_point("An epic review, no numbers").is_none());
    }

    #[test]
    fn finds_the_next_part_on_the_same_channel() {
        let card = |id: &str, title: &str, channel: &str| VideoCard {
            id: id.into(),
            title: title.into(),
            channel_id: channel.into(),
            channel_name: String::new(),
            channel_avatar: String::new(),
            duration: 0,
            views: 0,
            published: String::new(),
            published_unix: 0,
            thumbnail: String::new(),
            live: false,
        };
        let cards = vec![
            card("aaaaaaaaaaa", "Elden Ring Walkthrough - Part 13", "UC1"),
            card("bbbbbbbbbbb", "Elden Ring Walkthrough - Part 12", "UC1"),
            card("ccccccccccc", "Other Game - Part 12", "UC2"),
        ];
        let next = next_episode("Elden Ring Walkthrough - Part 12", &cards, "UC1")
            .expect("next part");
        assert_eq!(next.id, "aaaaaaaaaaa");
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(urlencode("rust lang"), "rust+lang");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
