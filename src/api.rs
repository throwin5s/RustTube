//! YouTube metadata access.
//!
//! RustTube never talks to youtube.com directly for metadata. Instead it speaks
//! the two common front-end APIs (Invidious and Piped) and transparently falls
//! back between a list of instances. Every call tries the last instance that
//! worked first, then the rest in order, so a single dead instance only costs
//! one timeout.

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

    /// Videos and channels together. A channel failure must not hide the videos.
    pub async fn search_all(&self, query: &str, page: u32) -> Result<SearchResults, String> {
        let (videos, channels) = futures::join!(self.search(query, page), self.search_channels(query));
        Ok(SearchResults {
            videos: videos?,
            channels: channels.unwrap_or_default(),
        })
    }

    /// Short list for the quick-select dropdown.
    pub async fn channel_suggestions(&self, query: &str) -> Result<Vec<ChannelCard>, String> {
        let mut channels = self.search_channels(query).await?;
        channels.truncate(6);
        Ok(channels)
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
        let width = item.get("width").and_then(|w| w.as_u64()).unwrap_or(0);
        let score = if width >= 76 && width <= 176 { width } else { 0 };
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
    let raw = v
        .get("description")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("shortDescription").and_then(|x| x.as_str()))
        .unwrap_or("");
    strip_html(raw)
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
    fn encodes_queries() {
        assert_eq!(urlencode("rust lang"), "rust+lang");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
