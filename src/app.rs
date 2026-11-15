//! The whole interface, drawn with egui.
//!
//! Layout mirrors the native music-client look this app is modelled on: a
//! sidebar for navigation and subscriptions, a top bar with search, a scrolling
//! content area, and a persistent player bar at the bottom once something is
//! playing.

use crate::api::{Api, ChannelCard, ChannelSummary, SearchResults, VideoCard, VideoDetail};
use crate::db::{self, HistoryEntry, StatsOverview, Subscription};
use crate::embed::Player;
use crate::stream::{self, Resolved, Streams};
use crate::sync::{self, SyncReport, SyncSettings};
use crate::theme as t;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke,
    Ui, Vec2,
};
use futures::StreamExt;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REGIONS: [&str; 14] = [
    "US", "GB", "CA", "AU", "DE", "FR", "ES", "IT", "NL", "SE", "IN", "JP", "BR", "MX",
];

#[derive(Clone, PartialEq, Eq)]
enum Route {
    Trending,
    Search(String, u32),
    Subscriptions,
    Channel(String),
    Watch(String),
    History,
    Stats,
    Settings,
}

impl Route {
    fn nav_key(&self) -> &'static str {
        match self {
            Route::Trending => "trending",
            Route::Search(..) => "search",
            Route::Subscriptions => "subscriptions",
            Route::Channel(_) => "channel",
            Route::Watch(_) => "watch",
            Route::History => "history",
            Route::Stats => "stats",
            Route::Settings => "settings",
        }
    }
}

enum Load<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(value) => Some(value),
            _ => None,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self, Load::Idle)
    }
}

fn loaded<T>(result: Result<T, String>) -> Load<T> {
    match result {
        Ok(value) => Load::Ready(value),
        Err(err) => Load::Failed(err),
    }
}

enum Msg {
    Trending(Result<Vec<VideoCard>, String>),
    Search(String, u32, Result<SearchResults, String>),
    Suggestions(String, Result<Vec<ChannelCard>, String>),
    Stream(String, Result<Resolved, String>),
    Feed(Result<Vec<VideoCard>, String>),
    Channel(String, Result<ChannelSummary, String>),
    ChannelMore(Vec<VideoCard>),
    Detail(String, Result<Box<VideoDetail>, String>),
    Sync(Result<SyncReport, String>),
}

#[derive(Clone, Default)]
struct SettingsDraft {
    region: String,
    api_base: String,
    api_current: String,
    history_enabled: bool,
    sync_mode: String,
    sync_folder: String,
    webdav_url: String,
    webdav_user: String,
    webdav_pass: String,
    has_password: bool,
    sync_auto: bool,
}

#[derive(Clone, Default)]
struct SyncStatus {
    enabled: bool,
    target: String,
    last_at: i64,
    last_error: String,
}

#[derive(Clone)]
struct Thumbs {
    map: Arc<Mutex<HashMap<String, Option<egui::TextureHandle>>>>,
    client: reqwest::Client,
    rt: tokio::runtime::Handle,
}

impl Thumbs {
    fn new(rt: tokio::runtime::Handle) -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
            client: reqwest::Client::builder()
                .user_agent("RustTube/0.2")
                .timeout(Duration::from_secs(12))
                .build()
                .unwrap_or_default(),
            rt,
        }
    }

    /// Returns the texture if cached, otherwise starts a download and returns None.
    fn get(&self, ctx: &egui::Context, key: &str, url: String) -> Option<egui::TextureHandle> {
        {
            let map = self.map.lock().ok()?;
            if let Some(entry) = map.get(key) {
                return entry.clone();
            }
        }
        if let Ok(mut map) = self.map.lock() {
            map.insert(key.to_string(), None);
        }

        let map = Arc::clone(&self.map);
        let key = key.to_string();
        let ctx = ctx.clone();
        let client = self.client.clone();
        self.rt.spawn(async move {
            let Ok(response) = client.get(&url).send().await else {
                return;
            };
            let Ok(bytes) = response.bytes().await else {
                return;
            };
            let Ok(image) = image::load_from_memory(&bytes) else {
                return;
            };
            let rgba = image.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            let handle = ctx.load_texture(key.clone(), pixels, egui::TextureOptions::LINEAR);
            if let Ok(mut map) = map.lock() {
                map.insert(key, Some(handle));
            }
            ctx.request_repaint();
        });
        None
    }
}

struct PendingPlay {
    card: VideoCard,
    start_at: f64,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    db: Arc<Mutex<Connection>>,
    api: Api,
    device: String,
    db_path: PathBuf,
    thumbs: Thumbs,
    player: Player,
    streams: Streams,
    pending_play: Option<PendingPlay>,
    hover_card: Option<(String, Instant)>,
    /// Hover prefetch runs at most once per video. Failed attempts are retried
    /// only when the user clicks, never every frame.
    prefetched: HashSet<String>,
    /// First tile of the current grid, resolved before anyone clicks it.
    prefetch_top: Option<String>,

    route: Route,
    stack: Vec<Route>,
    trending: Load<Vec<VideoCard>>,
    feed: Load<Vec<VideoCard>>,
    search: Load<SearchResults>,
    search_query: String,
    search_page: u32,
    search_rect: Option<Rect>,
    suggestions: Vec<ChannelCard>,
    suggest_for: String,
    suggest_pending: Option<(String, Instant)>,
    suggest_visible: bool,
    /// Query the user already submitted; its suggestions must not reopen and
    /// paint over the results page.
    suggest_dismissed: Option<String>,
    channel: Load<ChannelSummary>,
    channel_id: String,
    channel_videos: Vec<VideoCard>,
    channel_page: u32,
    detail: Load<VideoDetail>,
    detail_id: String,
    history: Load<Vec<HistoryEntry>>,
    stats: Load<StatsOverview>,
    subs: Vec<Subscription>,

    settings: SettingsDraft,
    sync_status: SyncStatus,
    sync_busy: bool,
    search_input: String,
    toast: Option<(String, Instant)>,
    clear_armed: bool,
    /// Debug aid: when RUSTTUBE_SHOT is set, render once, save the viewport
    /// to that path and quit. Used to inspect the UI without a human.
    shot_path: Option<PathBuf>,
    shot_started: Instant,
    shot_delay: Duration,
    shot_requested: bool,

    watch_acc: f64,
    last_pos: f64,
    last_flush: Instant,
    attempted: Option<String>,
    autoplay_first: bool,
    total_watch: i64,
    total_watch_at: Instant,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        t::apply(&ctx);

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (tx, rx) = std::sync::mpsc::channel();

        let db_path = data_dir().join("rusttube.db");
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = db::open(&db_path).expect("Could not open the RustTube database");
        let device = db::device_id(&conn).unwrap_or_else(|_| "unknown".to_string());
        let api_base = db::get_setting(&conn, "api_base").unwrap_or_default();
        let volume = db::get_setting(&conn, "volume")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(80);

        let player = Player::new(volume, ctx.clone());

        let mut app = App {
            thumbs: Thumbs::new(rt.handle().clone()),
            rt,
            tx,
            rx,
            ctx,
            api: Api::new(if api_base.trim().is_empty() { None } else { Some(api_base) }),
            db: Arc::new(Mutex::new(conn)),
            device,
            db_path,
            player,
            streams: Streams::default(),
            pending_play: None,
            hover_card: None,
            prefetched: HashSet::new(),
            prefetch_top: None,
            route: Route::Trending,
            stack: Vec::new(),
            trending: Load::Idle,
            feed: Load::Idle,
            search: Load::Idle,
            search_query: String::new(),
            search_page: 0,
            channel: Load::Idle,
            channel_id: String::new(),
            channel_videos: Vec::new(),
            channel_page: 1,
            detail: Load::Idle,
            detail_id: String::new(),
            history: Load::Idle,
            stats: Load::Idle,
            subs: Vec::new(),
            settings: SettingsDraft::default(),
            sync_status: SyncStatus::default(),
            sync_busy: false,
            search_input: String::new(),
            toast: None,
            clear_armed: false,
            shot_path: std::env::var_os("RUSTTUBE_SHOT").map(PathBuf::from),
            shot_started: Instant::now(),
            shot_delay: std::env::var("RUSTTUBE_SHOT_DELAY_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(Duration::from_secs(4)),
            shot_requested: false,
            search_rect: None,
            suggestions: Vec::new(),
            suggest_for: String::new(),
            suggest_pending: None,
            suggest_visible: false,
            suggest_dismissed: None,
            watch_acc: 0.0,
            last_pos: 0.0,
            last_flush: Instant::now(),
            attempted: None,
            autoplay_first: std::env::var_os("RUSTTUBE_AUTOPLAY").is_some(),
            total_watch: 0,
            total_watch_at: Instant::now(),
        };

        app.refresh_subs();
        app.load_settings_draft();
        app.refresh_sync_status();
        app.ensure_loaded();
        if let Ok(query) = std::env::var("RUSTTUBE_SHOT_QUERY") {
            if !query.trim().is_empty() {
                app.search_input = query.clone();
                app.route = Route::Search(query.clone(), 1);
                // Mimic a submitted search, so the dropdown does not cover the
                // results in the capture.
                app.suggest_dismissed = Some(query);
                app.suggest_visible = false;
                app.ensure_loaded();
            }
        }
        // Warm mpv at launch so the first click never pays process startup.
        app.player.warm();
        app
    }

    /// Render one frame, capture it to `RUSTTUBE_SHOT`, then close. Returns true
    /// once the capture has been handled, so the frame is not drawn twice.
    fn capture_if_requested(&mut self, ctx: &egui::Context) -> bool {
        let Some(path) = self.shot_path.clone() else {
            return false;
        };
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = screenshot {
            let [width, height] = image.size;
            let mut bytes = Vec::with_capacity(width * height * 4);
            for pixel in &image.pixels {
                bytes.extend_from_slice(&pixel.to_array());
            }
            if let Some(buffer) = image::RgbaImage::from_raw(width as u32, height as u32, bytes) {
                let _ = buffer.save(&path);
            }
            eprintln!("saved screenshot to {}", path.display());
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return true;
        }
        if !self.shot_requested && self.shot_started.elapsed() > self.shot_delay {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        false
    }

    fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let message = future.await;
            let _ = tx.send(message);
            ctx.request_repaint();
        });
    }

    fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
    }

    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> T) -> Option<T> {
        let conn = self.db.lock().ok()?;
        Some(f(&conn))
    }

    // ------------------------------------------------------------- navigation

    fn navigate(&mut self, route: Route) {
        if self.route != route {
            if matches!(self.route, Route::Watch(_)) {
                self.flush_watch();
            }
            self.stack.push(self.route.clone());
            if self.stack.len() > 40 {
                self.stack.remove(0);
            }
        }
        self.route = route;
        self.ensure_loaded();
    }

    fn back(&mut self) {
        if let Some(previous) = self.stack.pop() {
            if matches!(self.route, Route::Watch(_)) {
                self.flush_watch();
            }
            self.route = previous;
            self.ensure_loaded();
        }
    }

    fn ensure_loaded(&mut self) {
        match self.route.clone() {
            Route::Trending if self.trending.is_idle() => self.load_trending(),
            Route::Search(query, page) => {
                if self.search.is_idle() || query != self.search_query {
                    self.search_query = query.clone();
                    self.search_page = page;
                    self.load_search(query, page);
                }
            }
            Route::Subscriptions if self.feed.is_idle() => self.load_feed(),
            Route::Channel(id) => {
                if self.channel.is_idle() || self.channel_id != id {
                    self.channel_id = id.clone();
                    self.channel_page = 1;
                    self.channel_videos.clear();
                    self.channel = Load::Loading;
                    let api = self.api.clone();
                    self.spawn(async move {
                        Msg::Channel(id.clone(), api.channel(&id).await)
                    });
                }
            }
            Route::Watch(id) => {
                if self.detail.is_idle() || self.detail_id != id {
                    self.detail_id = id.clone();
                    self.detail = Load::Loading;
                    let api = self.api.clone();
                    let requested = id.clone();
                    self.spawn(async move {
                        Msg::Detail(requested.clone(), api.video(&requested).await.map(Box::new))
                    });
                }
                if let Some(card) = self.find_card(&id) {
                    self.maybe_start(&card);
                }
            }
            Route::History if self.history.is_idle() => {
                self.history = loaded(Ok(self
                    .with_db(|conn| db::list_history(conn, 300))
                    .unwrap_or_default()));
            }
            Route::Stats if self.stats.is_idle() => {
                self.stats = Load::Ready(
                    self.with_db(|conn| db::stats_overview(conn))
                        .unwrap_or_default(),
                );
            }
            Route::Settings => {
                self.load_settings_draft();
                self.refresh_sync_status();
            }
            _ => {}
        }
    }

    fn load_trending(&mut self) {
        self.trending = Load::Loading;
        let api = self.api.clone();
        let region = if self.settings.region.is_empty() {
            "US".to_string()
        } else {
            self.settings.region.clone()
        };
        self.spawn(async move { Msg::Trending(api.trending(&region).await) });
    }

    fn load_search(&mut self, query: String, page: u32) {
        self.search = Load::Loading;
        let api = self.api.clone();
        self.spawn(async move { Msg::Search(query.clone(), page, api.search_all(&query, page).await) });
    }

    fn load_feed(&mut self) {
        let ids: Vec<String> = self.subs.iter().map(|s| s.channel_id.clone()).collect();
        self.feed = Load::Loading;
        if ids.is_empty() {
            self.feed = Load::Ready(Vec::new());
            return;
        }
        let api = self.api.clone();
        self.spawn(async move {
            let mut newest = ids;
            newest.truncate(12);
            let results = futures::stream::iter(newest.into_iter().map(|id| {
                let api = api.clone();
                async move { api.channel_videos(&id, 1).await.unwrap_or_default() }
            }))
            .buffer_unordered(4)
            .collect::<Vec<Vec<VideoCard>>>()
            .await;

            let mut seen = std::collections::HashSet::new();
            let mut feed: Vec<VideoCard> = results
                .into_iter()
                .flatten()
                .filter(|card| seen.insert(card.id.clone()))
                .collect();
            feed.sort_by(|a, b| b.published_unix.cmp(&a.published_unix));
            feed.truncate(60);
            Msg::Feed(Ok(feed))
        });
    }

    fn find_card(&self, id: &str) -> Option<VideoCard> {
        for list in [
            self.trending.ready(),
            self.search.ready().map(|results| &results.videos),
            self.feed.ready(),
        ] {
            if let Some(list) = list {
                if let Some(card) = list.iter().find(|card| card.id == id) {
                    return Some(card.clone());
                }
            }
        }
        if let Some(channel) = self.channel.ready() {
            if let Some(card) = channel.videos.iter().find(|card| card.id == id) {
                return Some(card.clone());
            }
        }
        if let Some(card) = self.channel_videos.iter().find(|card| card.id == id) {
            return Some(card.clone());
        }
        if let Some(detail) = self.detail.ready() {
            if detail.card.id == id {
                return Some(detail.card.clone());
            }
        }
        None
    }

    // ---------------------------------------------------------------- settings

    fn load_settings_draft(&mut self) {
        let Some(mut draft) = self.with_db(|conn| {
            let sync = SyncSettings::load(conn).info(conn);
            SettingsDraft {
                region: db::get_setting(conn, "region").unwrap_or_else(|| "US".into()),
                api_base: db::get_setting(conn, "api_base").unwrap_or_default(),
                api_current: self.api.current_endpoint(),
                history_enabled: db::get_setting(conn, "history_enabled")
                    .map(|v| v != "0")
                    .unwrap_or(true),
                sync_mode: sync.mode,
                sync_folder: sync.folder,
                webdav_url: sync.webdav_url,
                webdav_user: sync.webdav_user,
                webdav_pass: String::new(),
                has_password: sync.has_password,
                sync_auto: sync.auto,
            }
        }) else {
            return;
        };
        draft.api_current = self.api.current_endpoint();
        self.settings = draft;
    }

    fn save_settings(&mut self) {
        let draft = self.settings.clone();
        let saved = self.with_db(|conn| {
            let _ = db::set_setting(conn, "region", &draft.region);
            let _ = db::set_setting(conn, "api_base", draft.api_base.trim());
            let _ = db::set_setting(
                conn,
                "history_enabled",
                if draft.history_enabled { "1" } else { "0" },
            );
            let mut sync = SyncSettings::load(conn);
            sync.mode = match draft.sync_mode.as_str() {
                "folder" | "webdav" => draft.sync_mode.clone(),
                _ => "off".to_string(),
            };
            sync.folder = draft.sync_folder.trim().to_string();
            sync.webdav_url = draft.webdav_url.trim().to_string();
            sync.webdav_user = draft.webdav_user.clone();
            if !draft.webdav_pass.is_empty() {
                sync.webdav_pass = draft.webdav_pass.clone();
            }
            sync.auto = draft.sync_auto;
            sync.save(conn).is_ok()
        });
        self.api.set_custom(if draft.api_base.trim().is_empty() {
            None
        } else {
            Some(draft.api_base.trim().to_string())
        });
        self.load_settings_draft();
        self.refresh_sync_status();
        self.trending = Load::Idle;
        if matches!(self.route, Route::Trending) {
            self.ensure_loaded();
        }
        match saved {
            Some(true) => self.toast("Settings saved"),
            _ => self.toast("Could not save settings"),
        }
    }

    fn refresh_sync_status(&mut self) {
        if let Some(status) = self.with_db(|conn| {
            let info = SyncSettings::load(conn).info(conn);
            SyncStatus {
                enabled: info.enabled,
                target: info.target,
                last_at: info.last_sync_at,
                last_error: info.last_error,
            }
        }) {
            self.sync_status = status;
        }
    }

    fn refresh_subs(&mut self) {
        self.subs = self
            .with_db(|conn| db::list_subscriptions(conn))
            .unwrap_or_default();
    }

    fn sync_now(&mut self) {
        if self.sync_busy {
            return;
        }
        let Some(settings) = self.with_db(|conn| SyncSettings::load(conn)) else {
            return;
        };
        if !settings.enabled() {
            self.toast("Set a sync folder or WebDAV address first");
            return;
        }
        self.sync_busy = true;
        let db = Arc::clone(&self.db);
        let device = self.device.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let result = sync::run(&db, &settings, &device).await;
            if let Err(err) = &result {
                sync::record_failure(&db, err);
            }
            let _ = tx.send(Msg::Sync(result));
            ctx.request_repaint();
        });
    }

    // ---------------------------------------------------------------- playback

    fn maybe_start(&mut self, card: &VideoCard) {
        if self.attempted.as_deref() == Some(card.id.as_str()) {
            return;
        }
        let already_playing = self
            .player
            .now
            .as_ref()
            .map(|now| now.video_id == card.id)
            .unwrap_or(false);
        if already_playing {
            return;
        }
        self.start_playback(card);
    }

    fn start_playback(&mut self, card: &VideoCard) {
        self.flush_watch();
        let resume = self
            .with_db(|conn| db::history_position(conn, &card.id))
            .unwrap_or(0);
        let start_at = if resume > 5 && (card.duration == 0 || resume < card.duration - 10) {
            resume as f64
        } else {
            0.0
        };
        // The player bar appears now, before the stream URL is even known.
        if let Err(err) = self.player.begin(
            &card.id,
            &card.title,
            &card.channel_id,
            &card.channel_name,
            card.duration,
        ) {
            self.attempted = Some(card.id.clone());
            self.toast(err);
            return;
        }

        self.attempted = Some(card.id.clone());
        self.last_pos = start_at;
        self.watch_acc = 0.0;
        self.last_flush = Instant::now();
        self.record_view(card, start_at);
        if start_at > 5.0 {
            self.toast(format!("Resuming at {}", t::format_clock(start_at)));
        }

        // Hover prefetch usually has this ready, so the load goes out at once.
        if let Some(stream) = self.streams.get(&card.id) {
            self.player
                .load(&stream.video, stream.audio.as_deref(), &card.id, start_at);
        } else {
            self.pending_play = Some(PendingPlay {
                card: card.clone(),
                start_at,
            });
            self.resolve(&card.id);
        }
    }

    /// Resolve a video's stream URLs in the background. Deduplicated by the
    /// cache, so hovering and then clicking only ever runs yt-dlp once.
    fn resolve(&mut self, id: &str) {
        if !self.streams.claim(id) {
            return;
        }
        self.player.warm();
        let id = id.to_string();
        self.spawn(async move {
            let result = stream::resolve(&id).await;
            Msg::Stream(id, result)
        });
    }

    /// Prefetch only once the pointer has rested on a card, so sweeping across
    /// the grid does not launch a resolver per card.
    fn prefetch_hover(&mut self, id: &str) {
        if self.streams.get(id).is_some() {
            return;
        }
        match &self.hover_card {
            Some((hovered, since)) if hovered == id => {
                if since.elapsed() >= Duration::from_millis(140) && !self.prefetched.contains(id) {
                    self.prefetched.insert(id.to_string());
                    self.resolve(id);
                }
            }
            _ => {
                self.hover_card = Some((id.to_string(), Instant::now()));
                self.ctx.request_repaint_after(Duration::from_millis(150));
            }
        }
    }

    fn record_view(&mut self, card: &VideoCard, position: f64) {
        let _ = self.with_db(|conn| {
            db::record_watch(
                conn,
                &db::WatchUpdate {
                    video_id: &card.id,
                    title: &card.title,
                    channel_id: &card.channel_id,
                    channel_name: &card.channel_name,
                    duration: card.duration,
                    position: position as i64,
                    seconds: 0,
                    new_view: true,
                },
            )
        });
    }

    fn toggle_subscription(&mut self, channel_id: &str, name: &str) {
        if channel_id.is_empty() {
            self.toast("Channel is still loading");
            return;
        }
        let now = self.with_db(|conn| {
            let subscribed = db::is_subscribed(conn, channel_id);
            db::set_subscription(conn, channel_id, name, "", !subscribed)
                .map(|state| state)
                .unwrap_or(false)
        });
        self.refresh_subs();
        if matches!(now, Some(true)) {
            self.toast(format!("Subscribed to {name}"));
            self.feed = Load::Idle;
        } else {
            self.toast("Unsubscribed");
            self.feed = Load::Idle;
        }
    }

    fn account_watch(&mut self) {
        if !self.player.is_active() {
            return;
        }
        let snapshot = self.player.snapshot();
        let delta = snapshot.position - self.last_pos;
        if !snapshot.paused && delta > 0.0 && delta < 2.5 {
            self.watch_acc += delta;
        }
        self.last_pos = snapshot.position;

        if snapshot.ended {
            self.flush_watch();
            self.player.stop();
            self.attempted = None;
            return;
        }
        if self.last_flush.elapsed() > Duration::from_secs(15) {
            self.flush_watch();
        }
        self.ctx.request_repaint_after(Duration::from_millis(400));
    }

    fn flush_watch(&mut self) {
        if !self.player.is_active() || self.watch_acc < 1.0 {
            return;
        }
        let Some(now) = self.player.now.clone() else {
            return;
        };
        let seconds = self.watch_acc.floor() as i64;
        let position = self.player.snapshot().position as i64;
        self.watch_acc = 0.0;
        self.last_flush = Instant::now();
        let _ = self.with_db(|conn| {
            db::record_watch(
                conn,
                &db::WatchUpdate {
                    video_id: &now.video_id,
                    title: &now.title,
                    channel_id: &now.channel_id,
                    channel_name: &now.channel_name,
                    duration: now.duration_hint,
                    position,
                    seconds,
                    new_view: false,
                },
            )
        });
        if matches!(self.route, Route::Stats) {
            self.stats = Load::Idle;
        }
    }

    fn handle(&mut self, message: Msg) {
        match message {
            Msg::Trending(result) => self.trending = loaded(result),
            Msg::Search(query, page, result) => {
                if self.route == Route::Search(query, page) {
                    self.search = loaded(result);
                    // Debug aid for screenshots: open the first result so an
                    // embedded video can be inspected without a click.
                    if self.autoplay_first && !self.player.is_active() {
                        self.autoplay_first = false;
                        if let Some(card) = self
                            .search
                            .ready()
                            .and_then(|results| results.videos.first())
                            .cloned()
                        {
                            self.navigate(Route::Watch(card.id.clone()));
                            self.start_playback(&card);
                        }
                    }
                }
            }
            Msg::Suggestions(query, result) => {
                if self.search_input.trim() == query {
                    self.suggestions = result.unwrap_or_default();
                    // A reply can land after Enter; it must not reopen the panel
                    // on top of the results the submit just produced.
                    let dismissed = self.suggest_dismissed.as_deref() == Some(query.as_str());
                    self.suggest_visible = !dismissed && !self.suggestions.is_empty();
                    self.ctx.request_repaint();
                }
            }
            Msg::Stream(id, result) => {
                let waiting = self
                    .pending_play
                    .as_ref()
                    .map(|pending| pending.card.id == id)
                    .unwrap_or(false);
                match result {
                    Ok(stream) => {
                        self.streams.store(&id, stream.clone());
                        if waiting {
                            if let Some(pending) = self.pending_play.take() {
                                self.player.load(
                                    &stream.video,
                                    stream.audio.as_deref(),
                                    &pending.card.id,
                                    pending.start_at,
                                );
                            }
                        }
                    }
                    Err(err) => {
                        self.streams.release(&id);
                        if waiting {
                            self.pending_play = None;
                            self.player.stop();
                            self.attempted = None;
                            self.toast(err);
                        }
                    }
                }
            }
            Msg::Feed(result) => self.feed = loaded(result),
            Msg::ChannelMore(mut more) => {
                // Clicking "Load more" twice used to append the same page twice.
                for card in more.drain(..) {
                    if !self
                        .channel_videos
                        .iter()
                        .any(|existing| existing.id == card.id)
                    {
                        self.channel_videos.push(card);
                    }
                }
            }
            Msg::Channel(id, result) => {
                if self.route == Route::Channel(id.clone()) {
                    if let Ok(channel) = &result {
                        if self.channel_videos.is_empty() {
                            self.channel_videos = channel.videos.clone();
                        }
                    }
                    self.channel = loaded(result);
                }
            }
            Msg::Detail(id, result) => {
                if self.route == Route::Watch(id.clone()) {
                    if let Ok(detail) = &result {
                        self.detail = Load::Ready((**detail).clone());
                        let card = detail.card.clone();
                        self.maybe_start(&card);
                    } else {
                        self.detail = loaded(result.map(|boxed| *boxed));
                    }
                }
            }
            Msg::Sync(result) => {
                self.sync_busy = false;
                match result {
                    Ok(report) => {
                        self.toast(format!(
                            "Synced · {} record{} merged",
                            report.records_applied,
                            if report.records_applied == 1 { "" } else { "s" }
                        ));
                        self.refresh_subs();
                        self.history = Load::Idle;
                        self.stats = Load::Idle;
                        self.feed = Load::Idle;
                        self.ensure_loaded();
                    }
                    Err(err) => self.toast(err),
                }
                self.refresh_sync_status();
            }
        }
    }

    // ------------------------------------------------------------------ chrome

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .exact_height(t::TOP_BAR_HEIGHT)
            .frame(
                egui::Frame::NONE
                    .fill(t::BG)
                    .inner_margin(Margin::symmetric(20, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let can_go_back = !self.stack.is_empty();
                    // Painted chevron, not a "←" glyph: Inter's Latin subset
                    // has no arrow characters, so the button rendered as an
                    // empty box.
                    let (back_rect, back) =
                        ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                    let back_tint = if !can_go_back {
                        t::DIM_2.gamma_multiply(0.6)
                    } else if back.hovered() {
                        t::TEXT
                    } else {
                        t::DIM
                    };
                    if can_go_back && back.hovered() {
                        ui.painter().rect_filled(
                            back_rect,
                            CornerRadius::same(t::RADIUS),
                            t::PANEL_2,
                        );
                    }
                    t::paint_icon(ui.painter(), back_rect, &t::Icon::Back, back_tint);
                    if can_go_back && back.clicked() {
                        self.back();
                    }

                    let field = egui::TextEdit::singleline(&mut self.search_input)
                        .hint_text("Search videos or channels")
                        .desired_width(430.0)
                        .margin(Vec2::new(10.0, 8.0));
                    let response = ui.add(field);
                    self.search_rect = Some(response.rect);
                    if response.changed() {
                        // Editing again re-arms the dropdown for this query.
                        self.suggest_dismissed = None;
                    }
                    // egui's single-line TextEdit surrenders focus when Enter is
                    // pressed, so both branches are required to catch a submit.
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if enter && (response.lost_focus() || response.has_focus()) {
                        let query = self.search_input.trim().to_string();
                        if !query.is_empty() {
                            self.suggest_visible = false;
                            self.suggestions.clear();
                            self.suggest_dismissed = Some(query.clone());
                            self.navigate(Route::Search(query, 1));
                            response.surrender_focus();
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (label, color) = if self.sync_busy {
                            ("Syncing…".to_string(), t::ACCENT_SOFT)
                        } else if !self.sync_status.enabled {
                            ("Sync off".to_string(), t::DIM_2)
                        } else if !self.sync_status.last_error.is_empty() {
                            ("Sync needs attention".to_string(), t::DANGER)
                        } else if self.sync_status.last_at > 0 {
                            (
                                format!("Synced {}", t::format_relative(self.sync_status.last_at)),
                                t::OK,
                            )
                        } else {
                            ("Ready to sync".to_string(), t::DIM)
                        };
                        let button = egui::Button::new(RichText::new(label).size(12.0).color(color))
                            .fill(t::PANEL_2)
                            .stroke(Stroke::new(1.0_f32, t::LINE))
                            .corner_radius(CornerRadius::same(t::RADIUS));
                        if ui.add(button).clicked() {
                            if self.sync_status.enabled {
                                self.sync_now();
                            } else {
                                self.navigate(Route::Settings);
                            }
                        }

                        if self.total_watch > 0 {
                            ui.label(
                                RichText::new(format!(
                                    "{} watched",
                                    t::format_long(self.total_watch)
                                ))
                                .size(12.0)
                                .color(t::DIM_2),
                            );
                        }
                    });
                });
            });
    }

    fn side_bar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("side")
            .exact_width(t::SIDEBAR_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(t::PANEL)
                    .inner_margin(Margin::symmetric(12, 12)),
            )
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    // Circle mark: a rounded square at this size read as a
                    // squircle and clashed with the 8px radius used everywhere
                    // else. White glyph because dark-maroon-on-red vanished.
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 14.0, t::ACCENT);
                    t::paint_icon(ui.painter(), rect, &t::Icon::Play, Color32::WHITE);
                    ui.add_space(10.0);
                    ui.label(RichText::new("RustTube").font(t::bold(17.0)));
                });
                ui.add_space(20.0);

                let active = self.route.nav_key();
                let items = [
                    ("trending", "Trending", Route::Trending),
                    ("subscriptions", "Subscriptions", Route::Subscriptions),
                    ("history", "History", Route::History),
                    ("stats", "Statistics", Route::Stats),
                    ("settings", "Settings", Route::Settings),
                ];
                for (key, label, route) in items {
                    if t::nav_row(ui, label, active == key).clicked() {
                        if key == "stats" {
                            self.stats = Load::Idle;
                        }
                        self.navigate(route);
                    }
                }

                ui.add_space(14.0);
                ui.separator();
                t::section(ui, "Subscriptions");

                let subs = self.subs.clone();
                let height = (ui.available_height() - 16.0).max(60.0);
                egui::ScrollArea::vertical()
                    .id_salt("subs")
                    .max_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if subs.is_empty() {
                            ui.label(
                                RichText::new("Channels you subscribe to appear here and sync across devices.")
                                    .size(12.0)
                                    .color(t::DIM_2),
                            );
                        }
                        for sub in subs {
                            let label = if sub.channel_name.is_empty() {
                                sub.channel_id.clone()
                            } else {
                                sub.channel_name.clone()
                            };
                            if t::nav_row(ui, &label, false).clicked() {
                                self.navigate(Route::Channel(sub.channel_id.clone()));
                            }
                        }
                    });
            });
    }

    fn player_bar(&mut self, ctx: &egui::Context) {
        if !self.player.is_active() {
            return;
        }
        let snapshot = self.player.snapshot();
        let now = self.player.now.clone();
        let Some(now) = now else {
            return;
        };

        egui::TopBottomPanel::bottom("player")
            .exact_height(t::PLAYER_BAR_HEIGHT)
            .frame(
                egui::Frame::NONE
                    .fill(t::PANEL)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (thumb_rect, _) = ui.allocate_exact_size(Vec2::new(96.0, 54.0), Sense::hover());
                    self.paint_thumb(ui, thumb_rect, &now.video_id, "mq");

                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.set_width(240.0);
                        ui.add(
                            egui::Label::new(RichText::new(&now.title).font(t::semibold(13.5)))
                                .truncate(),
                        )
                        .on_hover_text(&now.title);
                        ui.label(
                            RichText::new(&now.channel_name).font(t::regular(12.0)).color(t::DIM),
                        );
                    });

                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let (kind, label) = if snapshot.paused {
                                (t::Icon::Play, "Play")
                            } else {
                                (t::Icon::Pause, "Pause")
                            };
                            if t::icon_button(ui, kind, 30.0)
                                .on_hover_text(label)
                                .clicked()
                            {
                                self.player.toggle_pause();
                            }
                            if t::icon_button(ui, t::Icon::Back, 26.0)
                                .on_hover_text("Back 10 seconds")
                                .clicked()
                            {
                                self.player.seek(-10.0);
                            }
                            if t::icon_button(ui, t::Icon::Forward, 26.0)
                                .on_hover_text("Forward 10 seconds")
                                .clicked()
                            {
                                self.player.seek(10.0);
                            }
                            if t::icon_button(ui, t::Icon::Stop, 24.0)
                                .on_hover_text("Stop")
                                .clicked()
                            {
                                self.flush_watch();
                                self.player.stop();
                                self.attempted = None;
                            }

                            let duration = if snapshot.duration > 0.0 {
                                snapshot.duration
                            } else {
                                now.duration_hint as f64
                            };
                            let fraction = if duration > 0.0 {
                                (snapshot.position / duration) as f32
                            } else {
                                0.0
                            };
                            let (response, line) =
                                t::progress(ui, fraction, 4.0, t::ACCENT);
                            if let Some(pos) = response.interact_pointer_pos() {
                                if duration > 0.0 {
                                    let ratio =
                                        ((pos.x - line.left()) / line.width()).clamp(0.0, 1.0) as f64;
                                    let target = ratio * duration;
                                    self.player.seek(target - snapshot.position);
                                }
                            }
                            let time_text = if snapshot.started {
                                format!(
                                    "{} / {}",
                                    t::format_clock(snapshot.position),
                                    t::format_clock(duration)
                                )
                            } else {
                                "Starting…".to_string()
                            };
                            ui.label(RichText::new(time_text).size(11.5).color(t::DIM_2));
                        });
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut volume = self.player.volume as f32;
                        let slider = egui::Slider::new(&mut volume, 0.0..=130.0)
                            .show_value(false)
                            .trailing_fill(true);
                        if ui.add_sized([110.0, 16.0], slider).changed() {
                            self.player.set_volume(volume as i32);
                            let _ = self.with_db(|conn| {
                                db::set_setting(conn, "volume", &(volume as i32).to_string())
                            });
                        }
                        ui.label(RichText::new("Volume").size(11.0).color(t::DIM_2));
                    });
                });
            });
    }

    fn paint_thumb(&self, ui: &mut Ui, rect: Rect, video_id: &str, quality: &str) {
        let url = format!("https://i.ytimg.com/vi/{video_id}/{quality}default.jpg");
        let key = format!("{video_id}-{quality}");
        match self.thumbs.get(&self.ctx, &key, url) {
            Some(texture) => {
                egui::Image::from_texture(egui::load::SizedTexture::from_handle(&texture))
                    .corner_radius(CornerRadius::same(t::RADIUS))
                    .paint_at(ui, rect);
            }
            None => t::placeholder_thumbnail(ui.painter(), rect),
        }
    }

    // -------------------------------------------------------------------- views

    fn view_trending(&mut self, ui: &mut Ui) {
        t::h1(ui, "Trending");
        t::sub(
            ui,
            &format!(
                "YouTube's trending list for {} · change it under Settings > Trending region",
                self.settings.region
            ),
        );
        ui.add_space(14.0);

        let failure = match &self.trending {
            Load::Failed(err) => Some(err.clone()),
            _ => None,
        };
        if let Some(err) = failure {
            self.error_block(ui, &err, Which::Trending);
            return;
        }
        let Some(cards) = self.trending.ready().cloned() else {
            ui.spinner();
            return;
        };
        if cards.is_empty() {
            ui.label(RichText::new("Nothing came back from the video source.").color(t::DIM));
            return;
        }
        self.video_grid(ui, cards);
    }

    fn view_search(&mut self, ui: &mut Ui, query: String, page: u32) {
        t::h1(ui, &format!("Results for \"{query}\""));
        t::sub(ui, "Channels and videos from the metadata source");
        ui.add_space(14.0);

        let failure = match &self.search {
            Load::Failed(err) => Some(err.clone()),
            _ => None,
        };
        if let Some(err) = failure {
            self.error_block(ui, &err, Which::Search);
            return;
        }
        let Some(results) = self.search.ready().cloned() else {
            ui.spinner();
            return;
        };

        if !results.channels.is_empty() {
            t::section(ui, "Channels");
            let mut open: Option<String> = None;
            // Explicit rows. The old `horizontal_wrapped` re-flowed whenever an
            // avatar finished downloading, because `paint_channel_icon` used
            // `ui.put`, which allocates inside the row: cards jumped and
            // overlapped a frame after the images arrived.
            let shown = &results.channels[..results.channels.len().min(6)];
            let per_row = (((ui.available_width() + 12.0) / 288.0).floor() as usize).clamp(1, 3);
            for row_channels in shown.chunks(per_row) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    for channel in row_channels {
                        let (row, response) =
                            ui.allocate_exact_size(Vec2::new(276.0, 56.0), Sense::click());
                        if response.hovered() {
                            ui.painter()
                                .rect_filled(row, CornerRadius::same(t::RADIUS), t::PANEL_2);
                        }
                        let icon = Rect::from_min_size(
                            egui::pos2(row.left() + 8.0, row.center().y - 18.0),
                            Vec2::splat(36.0),
                        );
                        Self::paint_channel_icon(&self.thumbs, &self.ctx, ui, icon, channel);
                        // Clip to the row so a long channel name cannot spill
                        // into its neighbour.
                        let painter = ui
                            .painter()
                            .with_clip_rect(row.shrink2(Vec2::new(4.0, 0.0)));
                        painter.text(
                            egui::pos2(icon.right() + 10.0, row.center().y - 8.0),
                            Align2::LEFT_CENTER,
                            &channel.name,
                            t::medium(13.5),
                            t::TEXT,
                        );
                        painter.text(
                            egui::pos2(icon.right() + 10.0, row.center().y + 10.0),
                            Align2::LEFT_CENTER,
                            if channel.subscribers > 0 {
                                format!("{} subscribers", t::format_compact(channel.subscribers))
                            } else {
                                "Channel".to_string()
                            },
                            t::regular(10.5),
                            t::DIM_2,
                        );
                        if response.clicked() {
                            open = Some(channel.id.clone());
                        }
                    }
                });
                ui.add_space(4.0);
            }
            if let Some(id) = open {
                self.navigate(Route::Channel(id));
                return;
            }
            ui.add_space(14.0);
        }

        if results.videos.is_empty() {
            if results.channels.is_empty() {
                ui.label(RichText::new("No results.").color(t::DIM));
            }
            return;
        }

        t::section(ui, "Videos");
        self.video_grid(ui, results.videos.clone());
        ui.add_space(10.0);
        if t::quiet_button(ui, "Load more").clicked() {
            let next = page + 1;
            self.search = Load::Loading;
            self.search_page = next;
            self.load_search(query.clone(), next);
        }
    }

    fn view_subscriptions(&mut self, ui: &mut Ui) {
        t::h1(ui, "Subscriptions");
        t::sub(ui, "Latest uploads from channels you follow");
        ui.add_space(14.0);

        if self.subs.is_empty() {
            self.empty_block(
                ui,
                "No subscriptions yet",
                "Open a video and press Subscribe. Your channels sync across devices.",
            );
            return;
        }

        let failure = match &self.feed {
            Load::Failed(err) => Some(err.clone()),
            _ => None,
        };
        if let Some(err) = failure {
            self.error_block(ui, &err, Which::Feed);
            return;
        }
        let Some(cards) = self.feed.ready().cloned() else {
            ui.spinner();
            return;
        };
        if cards.is_empty() {
            ui.label(RichText::new("Nothing new right now.").color(t::DIM));
            return;
        }
        self.video_grid(ui, cards);
    }

    fn view_channel(&mut self, ui: &mut Ui, id: String) {
        let failure = match &self.channel {
            Load::Failed(err) => Some(err.clone()),
            _ => None,
        };
        let Some(channel) = self.channel.ready().cloned() else {
            if let Some(err) = failure {
                self.error_block(ui, &err, Which::Channel);
            } else {
                ui.spinner();
            }
            return;
        };

        let subscribed = self
            .with_db(|conn| db::is_subscribed(conn, &id))
            .unwrap_or(false);

        // Only claim the space when there is a banner to show: an empty grey
        // 120px band read as a broken image.
        if !channel.banner.is_empty() {
            let (banner, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 120.0), Sense::hover());
            match self.thumbs.get(
                &self.ctx,
                &format!("banner-{id}"),
                channel.banner.clone(),
            ) {
                Some(texture) => {
                    egui::Image::from_texture(egui::load::SizedTexture::from_handle(&texture))
                        .corner_radius(CornerRadius::same(t::RADIUS))
                        .paint_at(ui, banner);
                }
                None => {
                    ui.painter()
                        .rect_filled(banner, CornerRadius::same(t::RADIUS), t::PANEL_2);
                }
            }
            ui.add_space(10.0);
        }

        ui.horizontal(|ui| {
            t::avatar(ui, &channel.name, 64.0);
            ui.vertical(|ui| {
                t::h1(ui, &channel.name);
                let mut meta = Vec::new();
                if channel.subscribers > 0 {
                    meta.push(format!("{} subscribers", t::format_compact(channel.subscribers)));
                }
                if !channel.videos.is_empty() {
                    meta.push(format!("{} recent videos", channel.videos.len()));
                }
                t::sub(ui, &meta.join(" · "));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if subscribed { "Subscribed" } else { "Subscribe" };
                let button = if subscribed {
                    t::quiet_button(ui, label)
                } else {
                    t::accent_button(ui, label)
                };
                if button.clicked() {
                    self.toggle_subscription(&id, &channel.name);
                }
            });
        });

        if !channel.description.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new(&channel.description).size(12.5).color(t::DIM));
        }

        ui.add_space(18.0);
        let videos = if self.channel_videos.is_empty() {
            channel.videos.clone()
        } else {
            self.channel_videos.clone()
        };
        let has_videos = !videos.is_empty();
        self.video_grid(ui, videos);

        if has_videos && t::quiet_button(ui, "Load more").clicked() {
            let next = self.channel_page + 1;
            self.channel_page = next;
            let api = self.api.clone();
            let channel_id = id.clone();
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.rt.spawn(async move {
                if let Ok(more) = api.channel_videos(&channel_id, next).await {
                    let _ = tx.send(Msg::ChannelMore(more));
                    ctx.request_repaint();
                }
            });
        }
    }

    fn view_watch(&mut self, ui: &mut Ui, id: String) {
        let detail = self.detail.ready().filter(|d| d.card.id == id).cloned();
        let card = detail
            .as_ref()
            .map(|d| d.card.clone())
            .or_else(|| self.find_card(&id));

        let Some(card) = card else {
            let failure = match &self.detail {
                Load::Failed(err) => Some(err.clone()),
                _ => None,
            };
            if let Some(err) = failure {
                self.error_block(ui, &err, Which::Detail);
            } else {
                ui.spinner();
            }
            return;
        };

        let playing_this = self
            .player
            .now
            .as_ref()
            .map(|now| now.video_id == id)
            .unwrap_or(false);
        let snapshot = self.player.snapshot();

        let available = ui.available_width();
        // Keep the related rail usable without ever producing clamp(min > max).
        let left_width = (available * 0.64).max(320.0).min((available - 300.0).max(320.0));

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_width, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    let width = ui.available_width();
                    let height = width * 0.5625;
                    let (poster, response) =
                        ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
                    // Once mpv has produced a frame, the video surface replaces
                    // the poster; the paint callback composites it in this rect.
                    let video_ready = playing_this && snapshot.started;
                    if video_ready {
                        ui.painter().rect_filled(
                            poster,
                            CornerRadius::same(t::RADIUS),
                            Color32::BLACK,
                        );
                        self.player.paint(ui, poster);
                        if response.hovered() {
                            let center = poster.center();
                            ui.painter().circle_filled(
                                center,
                                26.0,
                                Color32::from_black_alpha(170),
                            );
                            let icon = if snapshot.paused {
                                t::Icon::Play
                            } else {
                                t::Icon::Pause
                            };
                            t::paint_icon(
                                ui.painter(),
                                Rect::from_center_size(center, Vec2::splat(28.0)),
                                &icon,
                                t::TEXT,
                            );
                        }
                    } else {
                        self.paint_thumb(ui, poster, &id, "maxres");
                        if playing_this {
                            ui.painter().rect_filled(
                                poster,
                                CornerRadius::same(t::RADIUS),
                                Color32::from_black_alpha(96),
                            );
                        }
                        let center = poster.center();
                        ui.painter()
                            .circle_filled(center, 30.0, Color32::from_black_alpha(190));
                        let icon = if playing_this && !snapshot.paused {
                            t::Icon::Pause
                        } else {
                            t::Icon::Play
                        };
                        t::paint_icon(
                            ui.painter(),
                            Rect::from_center_size(center, Vec2::splat(34.0)),
                            &icon,
                            t::TEXT,
                        );
                    }

                    if response.clicked() {
                        if playing_this {
                            self.player.toggle_pause();
                        } else {
                            self.start_playback(&card);
                        }
                    }

                    ui.add_space(14.0);
                    ui.label(RichText::new(&card.title).size(19.0).strong());
                    ui.add_space(4.0);

                    let mut meta = Vec::new();
                    if card.views > 0 {
                        meta.push(format!("{} views", t::format_compact(card.views)));
                    }
                    if let Some(detail) = &detail {
                        if detail.likes > 0 {
                            meta.push(format!("{} likes", t::format_compact(detail.likes)));
                        }
                    }
                    if !card.published.is_empty() {
                        meta.push(card.published.clone());
                    }
                    ui.label(RichText::new(meta.join(" · ")).size(12.5).color(t::DIM));

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        t::avatar(ui, &card.channel_name, 38.0);
                        ui.vertical(|ui| {
                            let name = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new(&card.channel_name).font(t::semibold(13.5)),
                                    )
                                    .truncate()
                                    .sense(Sense::click()),
                                )
                                .on_hover_text("Open channel");
                            if name.clicked() && !card.channel_id.is_empty() {
                                self.navigate(Route::Channel(card.channel_id.clone()));
                            }
                            if !card.channel_id.is_empty() {
                                ui.label(
                                    RichText::new("Channel").size(11.0).color(t::DIM_2),
                                );
                            }
                        });
                        ui.add_space(6.0);
                        let subscribed = self
                            .with_db(|conn| db::is_subscribed(conn, &card.channel_id))
                            .unwrap_or(false);
                        let label = if subscribed { "Subscribed" } else { "Subscribe" };
                        let button = if subscribed {
                            t::quiet_button(ui, label)
                        } else {
                            t::accent_button(ui, label)
                        };
                        if button.clicked() {
                            self.toggle_subscription(&card.channel_id, &card.channel_name);
                        }
                        if !card.channel_id.is_empty() && t::quiet_button(ui, "Channel").clicked() {
                            self.navigate(Route::Channel(card.channel_id.clone()));
                        }
                    });

                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(if playing_this {
                            if snapshot.paused {
                                "Paused — click the video to resume"
                            } else {
                                "Playing in RustTube"
                            }
                        } else {
                            "Press the poster to play"
                        })
                        .size(11.5)
                        .color(t::DIM_2),
                    );

                    ui.add_space(12.0);
                    let description = detail
                        .as_ref()
                        .map(|d| d.description.clone())
                        .unwrap_or_else(|| "No description available.".into());
                    t::panel_frame().show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("description")
                            .max_height(150.0)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(description)
                                        .size(12.8)
                                        .color(Color32::from_rgb(0xc6, 0xc6, 0xd0)),
                                );
                            });
                    });
                },
            );

            ui.add_space(18.0);
            ui.vertical(|ui| {
                t::section(ui, "Up next");
                let related = detail
                    .as_ref()
                    .map(|d| d.related.clone())
                    .unwrap_or_default();
                if related.is_empty() {
                    ui.label(RichText::new("Nothing to recommend.").size(12.0).color(t::DIM_2));
                }
                egui::ScrollArea::vertical()
                    .id_salt("related")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for video in related.iter().take(20) {
                            self.related_row(ui, video);
                        }
                    });
            });
        });
    }

    fn related_row(&mut self, ui: &mut Ui, video: &VideoCard) {
        let width = ui.available_width();
        let response = ui
            .allocate_ui_with_layout(
                Vec2::new(width, 74.0),
                Layout::left_to_right(Align::Min),
                |ui| {
                    let (thumb, _) = ui.allocate_exact_size(Vec2::new(128.0, 72.0), Sense::hover());
                    self.paint_thumb(ui, thumb, &video.id, "mq");
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.set_width((width - 142.0).max(80.0));
                        ui.add(
                            egui::Label::new(RichText::new(&video.title).size(12.5))
                                .truncate(),
                        )
                        .on_hover_text(&video.title);
                        ui.label(
                            RichText::new(&video.channel_name).size(11.0).color(t::DIM_2),
                        );
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                t::format_compact(video.views),
                                video.published
                            ))
                            .size(10.5)
                            .color(t::DIM_2),
                        );
                    });
                },
            )
            .response;
        if response.hovered() {
            self.prefetch_hover(&video.id);
        }
        if response.interact(Sense::click()).clicked() {
            self.navigate(Route::Watch(video.id.clone()));
        }
    }

    fn view_history(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                t::h1(ui, "History");
                t::sub(ui, "Newest first · resume points sync across devices");
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.history.ready().map(|h| !h.is_empty()).unwrap_or(false) {
                    let label = if self.clear_armed {
                        "Click again to confirm"
                    } else {
                        "Clear history"
                    };
                    if t::danger_button(ui, label).clicked() {
                        if self.clear_armed {
                            let _ = self.with_db(|conn| db::clear_history(conn));
                            self.clear_armed = false;
                            self.history = Load::Idle;
                            self.ensure_loaded();
                            self.toast("Watch history cleared");
                        } else {
                            self.clear_armed = true;
                        }
                    }
                }
            });
        });
        ui.add_space(14.0);

        match &self.history {
            Load::Loading | Load::Idle => {
                ui.spinner();
            }
            Load::Failed(err) => {
                let message = err.clone();
                self.error_block(ui, &message, Which::History);
            }
            Load::Ready(items) => {
                let items = items.clone();
                if items.is_empty() {
                    self.empty_block(
                        ui,
                        "Nothing watched yet",
                        "Videos you open show up here with resume points.",
                    );
                    return;
                }
                let mut remove: Option<String> = None;
                for item in &items {
                    let width = ui.available_width();
                    let response = ui
                        .allocate_ui_with_layout(
                            Vec2::new(width, 78.0),
                            Layout::left_to_right(Align::Min),
                            |ui| {
                                let (thumb, _) =
                                    ui.allocate_exact_size(Vec2::new(132.0, 74.0), Sense::hover());
                                self.paint_thumb(ui, thumb, &item.video_id, "mq");
                                if item.duration > 0 && item.position > 0 {
                                    let fraction =
                                        (item.position as f32 / item.duration as f32).clamp(0.0, 1.0);
                                    let bar = Rect::from_min_size(
                                        egui::pos2(thumb.left(), thumb.bottom() - 3.0),
                                        Vec2::new(thumb.width() * fraction, 3.0),
                                    );
                                    ui.painter()
                                        .rect_filled(bar, CornerRadius::same(t::RADIUS), t::ACCENT);
                                }
                                ui.add_space(10.0);
                                ui.vertical(|ui| {
                                    ui.set_width((width - 210.0).max(120.0));
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(if item.title.is_empty() {
                                                "Untitled"
                                            } else {
                                                &item.title
                                            })
                                            .size(13.5),
                                        )
                                        .truncate(),
                                    );
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · watched {}",
                                            item.channel_name,
                                            t::format_relative(item.watched_at)
                                        ))
                                        .size(11.5)
                                        .color(t::DIM_2),
                                    );
                                    if item.position > 30 {
                                        ui.label(
                                            RichText::new(format!(
                                                "Resume at {}",
                                                t::format_clock(item.position as f64)
                                            ))
                                            .size(11.0)
                                            .color(t::ACCENT_SOFT),
                                        );
                                    }
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if t::danger_button(ui, "Remove").clicked() {
                                        remove = Some(item.video_id.clone());
                                    }
                                });
                            },
                        )
                        .response;
                    if response.interact(Sense::click()).clicked() && remove.is_none() {
                        self.navigate(Route::Watch(item.video_id.clone()));
                    }
                    ui.separator();
                }
                if let Some(id) = remove {
                    let _ = self.with_db(|conn| db::delete_history_item(conn, &id));
                    self.history = Load::Idle;
                    self.ensure_loaded();
                }
            }
        }
    }

    fn view_stats(&mut self, ui: &mut Ui) {
        t::h1(ui, "Statistics");
        t::sub(ui, "Watch time is counted only while mpv reports playback");
        ui.add_space(14.0);

        let stats = match &self.stats {
            Load::Ready(stats) => stats.clone(),
            Load::Failed(err) => {
                let message = err.clone();
                self.error_block(ui, &message, Which::Stats);
                return;
            }
            _ => {
                ui.spinner();
                return;
            }
        };

        let cards = [
            ("Total watch time", t::format_long(stats.total_seconds), if stats.devices > 1 { format!("Across {} devices", stats.devices) } else { "On this device".to_string() }),
            ("This week", t::format_long(stats.week_seconds), format!("Today: {}", t::format_long(stats.today_seconds))),
            ("Videos opened", stats.total_videos.to_string(), format!("Last 30 days: {}", stats.daily.iter().map(|d| d.videos).sum::<i64>())),
            ("Day streak", stats.streak_days.to_string(), if stats.streak_days > 0 { "Keep it going".to_string() } else { "Watch something today".to_string() }),
        ];

        ui.horizontal(|ui| {
            let gap = 12.0;
            let width = (ui.available_width() - gap * 3.0) / 4.0;
            for (label, value, hint) in cards {
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 92.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        t::card_frame().show(ui, |ui| {
                            ui.set_width(width - 32.0);
                            ui.label(RichText::new(label.to_uppercase()).size(10.5).color(t::DIM_2).strong());
                            ui.label(RichText::new(value).size(24.0).strong());
                            ui.label(RichText::new(hint).size(11.0).color(t::DIM_2));
                        });
                    },
                );
                ui.add_space(gap);
            }
        });

        ui.add_space(18.0);
        t::section(ui, "Last 30 days");
        t::panel_frame().show(ui, |ui| {
            let height = 160.0;
            let width = ui.available_width();
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
            let painter = ui.painter();
            let count = stats.daily.len().max(1);
            let step = rect.width() / count as f32;
            let max = stats
                .daily
                .iter()
                .map(|day| day.seconds)
                .max()
                .unwrap_or(0)
                .max(1) as f32;

            for (index, day) in stats.daily.iter().enumerate() {
                let bar_height = (day.seconds as f32 / max) * (height - 22.0);
                let bar = Rect::from_min_size(
                    egui::pos2(
                        rect.left() + index as f32 * step + 2.0,
                        rect.bottom() - 16.0 - bar_height.max(3.0),
                    ),
                    Vec2::new((step - 4.0).max(2.0), bar_height.max(3.0)),
                );
                let color = if day.seconds > 0 {
                    t::ACCENT
                } else {
                    t::PANEL_3
                };
                painter.rect_filled(bar, CornerRadius::same(t::RADIUS), color);
            }
            painter.text(
                egui::pos2(rect.left(), rect.bottom()),
                Align2::LEFT_BOTTOM,
                stats.daily.first().map(|d| d.day.clone()).unwrap_or_default(),
                FontId::proportional(10.0),
                t::DIM_2,
            );
            painter.text(
                egui::pos2(rect.right(), rect.bottom()),
                Align2::RIGHT_BOTTOM,
                stats.daily.last().map(|d| d.day.clone()).unwrap_or_default(),
                FontId::proportional(10.0),
                t::DIM_2,
            );

            if let Some(pos) = response.hover_pos() {
                let index = (((pos.x - rect.left()) / step) as usize).min(count - 1);
                if let Some(day) = stats.daily.get(index) {
                    response.on_hover_text(format!(
                        "{} · {}",
                        day.day,
                        t::format_long(day.seconds)
                    ));
                }
            }
        });

        ui.add_space(18.0);
        t::section(ui, "Most watched channels");
        t::panel_frame().show(ui, |ui| {
            if stats.top_channels.is_empty() {
                ui.label(RichText::new("No channel data yet.").size(12.0).color(t::DIM_2));
                return;
            }
            let top = stats.top_channels[0].seconds.max(1);
            for channel in &stats.top_channels {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [170.0, 16.0],
                        egui::Label::new(
                            RichText::new(if channel.channel_name.is_empty() {
                                &channel.channel_id
                            } else {
                                &channel.channel_name
                            })
                            .size(12.5),
                        )
                        .truncate(),
                    );
                    let width = (ui.available_width() - 80.0).max(40.0);
                    let (bar, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), Sense::hover());
                    ui.painter()
                        .rect_filled(bar, CornerRadius::same(t::RADIUS), t::PANEL_3);
                    let fill = Rect::from_min_size(
                        bar.min,
                        Vec2::new(
                            bar.width() * (channel.seconds as f32 / top as f32).clamp(0.0, 1.0),
                            8.0,
                        ),
                    );
                    ui.painter()
                        .rect_filled(fill, CornerRadius::same(t::RADIUS), t::ACCENT);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(t::format_long(channel.seconds))
                                .size(11.5)
                                .color(t::DIM),
                        );
                    });
                });
            }
        });
    }

    fn view_settings(&mut self, ui: &mut Ui) {
        t::h1(ui, "Settings");
        t::sub(ui, "Video source, cloud sync and local data");
        ui.add_space(14.0);

        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width().min(820.0), 0.0),
            Layout::top_down(Align::Min),
            |ui| {
                // Video source
                t::card_frame().show(ui, |ui| {
                    ui.label(RichText::new("Video source").size(14.5).strong());
                    ui.label(
                        RichText::new(
                            "Metadata comes from Invidious or Piped instances. Playback goes through mpv and yt-dlp.",
                        )
                        .size(11.8)
                        .color(t::DIM_2),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new("Instance URL").size(12.0).color(t::DIM));
                    let width = ui.available_width();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.api_base)
                            .hint_text("Automatic (recommended)")
                            .desired_width(width),
                    );
                    ui.label(
                        RichText::new(format!(
                            "Currently answering: {}",
                            if self.settings.api_current.is_empty() {
                                "not contacted yet"
                            } else {
                                &self.settings.api_current
                            }
                        ))
                        .size(11.0)
                        .color(t::DIM_2),
                    );

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Trending region").size(12.0).color(t::DIM));
                            egui::ComboBox::from_id_salt("region")
                                .selected_text(self.settings.region.clone())
                                .show_ui(ui, |ui| {
                                    for region in REGIONS {
                                        ui.selectable_value(
                                            &mut self.settings.region,
                                            region.to_string(),
                                            region,
                                        );
                                    }
                                });
                        });
                        ui.add_space(18.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Watch history").size(12.0).color(t::DIM));
                            ui.checkbox(
                                &mut self.settings.history_enabled,
                                "Record history and watch time",
                            );
                        });
                    });
                });

                ui.add_space(14.0);

                // Sync
                t::card_frame().show(ui, |ui| {
                    ui.label(RichText::new("Cross-device sync").size(14.5).strong());
                    ui.label(
                        RichText::new(
                            "Subscriptions, history and watch time are written as one JSON snapshot into storage you control. No account, no server in the middle.",
                        )
                        .size(11.8)
                        .color(t::DIM_2),
                    );
                    ui.add_space(12.0);

                    ui.horizontal(|ui| {
                        for (value, label) in [
                            ("off", "Off"),
                            ("folder", "Cloud folder"),
                            ("webdav", "WebDAV"),
                        ] {
                            ui.selectable_value(
                                &mut self.settings.sync_mode,
                                value.to_string(),
                                label,
                            );
                        }
                    });
                    ui.add_space(10.0);

                    if self.settings.sync_mode == "folder" {
                        ui.label(
                            RichText::new("Folder kept in sync by iCloud Drive, OneDrive, Dropbox…")
                                .size(12.0)
                                .color(t::DIM),
                        );
                        let width = ui.available_width();
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.sync_folder)
                                .hint_text("~/Library/Mobile Documents/com~apple~CloudDocs/RustTube")
                                .desired_width(width),
                        );
                        let presets = sync::presets();
                        if !presets.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                for preset in presets {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(preset.label).size(11.5).color(t::DIM),
                                            )
                                            .fill(Color32::TRANSPARENT)
                                            .stroke(Stroke::new(1.0_f32, t::LINE))
                                            .corner_radius(CornerRadius::same(t::RADIUS)),
                                        )
                                        .clicked()
                                    {
                                        self.settings.sync_folder = preset.path;
                                    }
                                }
                            });
                        }
                    }

                    if self.settings.sync_mode == "webdav" {
                        ui.label(RichText::new("WebDAV address").size(12.0).color(t::DIM));
                        let width = ui.available_width();
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.webdav_url)
                                .hint_text("https://cloud.example.com/remote.php/dav/files/you/RustTube")
                                .desired_width(width),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new("Username").size(12.0).color(t::DIM));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.webdav_user)
                                        .desired_width(240.0),
                                );
                            });
                            ui.vertical(|ui| {
                                ui.label(RichText::new("Password").size(12.0).color(t::DIM));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.webdav_pass)
                                        .password(true)
                                        .hint_text(if self.settings.has_password {
                                            "Stored — leave blank to keep"
                                        } else {
                                            "App password"
                                        })
                                        .desired_width(240.0),
                                );
                            });
                        });
                        ui.label(
                            RichText::new("Stored locally in RustTube's database, never in the synced file.")
                                .size(11.0)
                                .color(t::DIM_2),
                        );
                    }

                    ui.add_space(8.0);
                    ui.checkbox(&mut self.settings.sync_auto, "Sync automatically");

                    ui.add_space(6.0);
                    let status = if !self.sync_status.enabled {
                        "Sync is off — everything stays on this device.".to_string()
                    } else if !self.sync_status.last_error.is_empty() {
                        self.sync_status.last_error.clone()
                    } else if self.sync_status.last_at > 0 {
                        format!(
                            "Last synced {} · {}",
                            t::format_relative(self.sync_status.last_at),
                            self.sync_status.target
                        )
                    } else {
                        format!("Ready to sync through {}", self.sync_status.target)
                    };
                    ui.label(
                        RichText::new(status).size(11.5).color(if self.sync_status.last_error.is_empty() {
                            t::DIM_2
                        } else {
                            t::DANGER
                        }),
                    );

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if t::accent_button(ui, "Save settings").clicked() {
                            self.save_settings();
                        }
                        if t::quiet_button(ui, if self.sync_busy { "Syncing…" } else { "Sync now" })
                            .clicked()
                            && !self.sync_busy
                        {
                            self.sync_now();
                        }
                    });
                });

                ui.add_space(14.0);

                // Local data
                t::card_frame().show(ui, |ui| {
                    ui.label(RichText::new("Local data").size(14.5).strong());
                    ui.add_space(6.0);
                    for (label, value) in [
                        ("Database", self.db_path.display().to_string()),
                        ("Device ID", self.device.clone()),
                        ("App version", env!("CARGO_PKG_VERSION").to_string()),
                    ] {
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [110.0, 16.0],
                                egui::Label::new(RichText::new(label).size(11.5).color(t::DIM_2)),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(value).size(11.5).color(t::DIM))
                                    .truncate(),
                            );
                        });
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Sync copies only subscriptions, history and watch time — never settings or credentials.",
                        )
                        .size(11.0)
                        .color(t::DIM_2),
                    );
                });
            },
        );
    }

    // ------------------------------------------------------------------ shared

    fn video_grid(&mut self, ui: &mut Ui, cards: Vec<VideoCard>) {
        if cards.is_empty() {
            return;
        }
        // Extraction takes 2.5-3s, which no hover can hide, so warm the first
        // tile as soon as its grid appears: it is the most likely click, and
        // resolving more than one at a time would just contend for CPU and
        // make that first tile slower.
        if let Some(first) = cards.first() {
            if self.prefetch_top.as_deref() != Some(first.id.as_str()) {
                self.prefetch_top = Some(first.id.clone());
                self.resolve(&first.id);
            }
        }
        let available = ui.available_width();
        let gap = 18.0;
        let min_width = 232.0;
        let columns = (((available + gap) / (min_width + gap)).floor() as usize).max(1);
        let card_width = (available - gap * (columns.saturating_sub(1)) as f32) / columns as f32;
        let row_height = card_width * 0.5625 + 58.0;

        let mut index = 0;
        while index < cards.len() {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for _ in 0..columns {
                    if index >= cards.len() {
                        break;
                    }
                    let card = cards[index].clone();
                    index += 1;
                    ui.allocate_ui_with_layout(
                        Vec2::new(card_width, row_height),
                        Layout::top_down(Align::Min),
                        |ui| {
                            if self.card(ui, &card, card_width) {
                                self.navigate(Route::Watch(card.id.clone()));
                            }
                        },
                    );
                }
            });
            ui.add_space(14.0);
        }
    }

    fn card(&mut self, ui: &mut Ui, card: &VideoCard, width: f32) -> bool {
        let height = (width * 0.5625).round();
        let (thumb, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        self.paint_thumb(ui, thumb, &card.id, "mq");
        if response.hovered() {
            self.prefetch_hover(&card.id);
        }

        let badge = if card.live {
            "LIVE".to_string()
        } else if card.duration > 0 {
            t::format_clock(card.duration as f64)
        } else {
            String::new()
        };
        if !badge.is_empty() {
            let galley =
                ui.painter()
                    .layout(badge, t::semibold(10.5), t::TEXT, 120.0);
            let padding = Vec2::new(6.0, 2.0);
            let size = galley.size() + padding * 2.0;
            let rect = Rect::from_min_size(
                egui::pos2(thumb.right() - size.x - 7.0, thumb.bottom() - size.y - 7.0),
                size,
            );
            ui.painter()
                .rect_filled(rect, CornerRadius::same(t::RADIUS_SMALL), Color32::from_black_alpha(215));
            ui.painter().galley(rect.min + padding, galley, t::TEXT);
        }
        ui.add_space(8.0);
        ui.set_max_width(width);
        ui.add(egui::Label::new(RichText::new(&card.title).font(t::medium(13.5))).truncate())
            .on_hover_text(&card.title);
        // Live cards carry no meaningful publish date; they used to show
        // "0 seconds ago" instead of saying what they are.
        let meta = if card.live {
            "Live now".to_string()
        } else if card.views > 0 {
            format!("{} views", t::format_compact(card.views))
        } else if card.published.is_empty() || card.published.starts_with("0 seconds") {
            // Instances label live items this way; "0 seconds ago" is noise.
            "Live now".to_string()
        } else {
            card.published.clone()
        };
        let subtitle = format!(
            "{} · {}",
            if card.channel_name.is_empty() {
                "Unknown"
            } else {
                &card.channel_name
            },
            meta
        );
        ui.add(egui::Label::new(RichText::new(subtitle).font(t::regular(12.5)).color(t::DIM)).truncate());

        response.clicked()
    }

    /// Debounced channel lookup for the quick-select dropdown.
    fn tick_suggestions(&mut self) {
        let query = self.search_input.trim().to_string();
        if query.chars().count() < 2 {
            self.suggestions.clear();
            self.suggest_visible = false;
            self.suggest_for.clear();
            self.suggest_pending = None;
            return;
        }
        if query == self.suggest_for {
            return;
        }

        let now = Instant::now();
        let due = match &self.suggest_pending {
            Some((pending, at)) if pending == &query => *at,
            _ => {
                let at = now + Duration::from_millis(220);
                self.suggest_pending = Some((query.clone(), at));
                at
            }
        };
        if now < due {
            self.ctx.request_repaint_after(due - now);
            return;
        }
        self.suggest_pending = None;
        self.suggest_for = query.clone();
        let api = self.api.clone();
        self.spawn(async move {
            Msg::Suggestions(query.clone(), api.channel_suggestions(&query).await)
        });
    }

    fn suggestions_layer(&mut self, ctx: &egui::Context) {
        if !self.suggest_visible || self.suggestions.is_empty() {
            return;
        }
        let Some(anchor) = self.search_rect else {
            return;
        };

        let channels = self.suggestions.clone();
        let width = anchor.width().max(380.0);
        let row_width = width - 12.0;
        let mut chosen: Option<String> = None;

        let area = egui::Area::new(egui::Id::new("search-suggestions"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(anchor.left(), anchor.bottom() + 6.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(t::PANEL_3)
                    .stroke(Stroke::new(1.0_f32, t::LINE))
                    .corner_radius(CornerRadius::same(t::RADIUS))
                    .inner_margin(Margin::same(6))
                    .show(ui, |ui| {
                        ui.set_width(row_width);
                        for channel in &channels {
                            let (row, response) = ui.allocate_exact_size(
                                Vec2::new(row_width, 46.0),
                                Sense::click(),
                            );
                            if response.hovered() {
                                ui.painter().rect_filled(
                                    row,
                                    CornerRadius::same(t::RADIUS),
                                    t::PANEL_2,
                                );
                            }
                            let icon = Rect::from_min_size(
                                egui::pos2(row.left() + 8.0, row.center().y - 16.0),
                                Vec2::splat(32.0),
                            );
                            Self::paint_channel_icon(&self.thumbs, &self.ctx, ui, icon, channel);
                            ui.painter().text(
                                egui::pos2(icon.right() + 10.0, row.center().y - 7.0),
                                Align2::LEFT_CENTER,
                                &channel.name,
                                FontId::proportional(13.0),
                                t::TEXT,
                            );
                            if channel.subscribers > 0 {
                                ui.painter().text(
                                    egui::pos2(icon.right() + 10.0, row.center().y + 9.0),
                                    Align2::LEFT_CENTER,
                                    format!(
                                        "{} subscribers",
                                        t::format_compact(channel.subscribers)
                                    ),
                                    FontId::proportional(10.5),
                                    t::DIM_2,
                                );
                            }
                            if response.clicked() {
                                chosen = Some(channel.id.clone());
                            }
                        }
                    });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.suggest_visible = false;
        }
        if ctx.input(|i| i.pointer.any_click()) && !area.response.hovered() {
            self.suggest_visible = false;
        }
        if let Some(id) = chosen {
            self.suggest_visible = false;
            self.suggest_dismissed = Some(self.search_input.trim().to_string());
            self.navigate(Route::Channel(id));
        }
    }

    fn paint_channel_icon(
        thumbs: &Thumbs,
        ctx: &egui::Context,
        ui: &mut Ui,
        rect: Rect,
        channel: &ChannelCard,
    ) {
        let texture = if channel.avatar.is_empty() {
            None
        } else {
            thumbs.get(ctx, &format!("channel-{}", channel.id), channel.avatar.clone())
        };
        match texture {
            Some(texture) => {
                // `paint_at` draws without touching the layout; `ui.put` here
                // used to allocate space mid-row and shift everything.
                egui::Image::from_texture(egui::load::SizedTexture::from_handle(&texture))
                    .corner_radius(CornerRadius::same((rect.width() * 0.5) as u8))
                    .paint_at(ui, rect);
            }
            None => {
                ui.painter()
                    .circle_filled(rect.center(), rect.width() * 0.5, t::PANEL_3);
                let initials = t::initials(&channel.name);
                if initials.is_empty() {
                    // A nameless result still needs a visible mark, not a
                    // blank grey dot.
                    t::paint_icon(ui.painter(), rect, &t::Icon::Play, t::DIM_2);
                } else {
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        initials,
                        t::semibold(rect.width() * 0.36),
                        t::ACCENT_SOFT,
                    );
                }
            }
        }
    }

    fn empty_block(&mut self, ui: &mut Ui, title: &str, body: &str) {
        ui.add_space(40.0);
        ui.label(RichText::new(title).size(17.0).strong());
        ui.add_space(4.0);
        ui.label(RichText::new(body).size(12.8).color(t::DIM));
    }

    fn error_block(&mut self, ui: &mut Ui, message: &str, which: Which) {
        ui.add_space(20.0);
        ui.label(RichText::new("Something went wrong").size(16.0).strong());
        ui.add_space(4.0);
        ui.label(RichText::new(message).size(12.5).color(t::DIM));
        ui.add_space(10.0);
        if t::quiet_button(ui, "Try again").clicked() {
            match which {
                Which::Trending => self.load_trending(),
                Which::Search => {
                    self.search = Load::Idle;
                    self.load_search(self.search_query.clone(), self.search_page.max(1));
                }
                Which::Feed => {
                    self.feed = Load::Idle;
                    self.load_feed();
                }
                Which::Channel => {
                    self.channel = Load::Idle;
                    self.ensure_loaded();
                }
                Which::Detail => {
                    self.detail = Load::Idle;
                    self.ensure_loaded();
                }
                Which::History => self.history = Load::Idle,
                Which::Stats => self.stats = Load::Idle,
            }
            self.ensure_loaded();
        }
        if t::quiet_button(ui, "Video source settings").clicked() {
            self.navigate(Route::Settings);
        }
    }

    fn toast_layer(&mut self, ctx: &egui::Context) {
        let Some((message, at)) = self.toast.clone() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(4) {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::new("toast"))
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -110.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(t::PANEL_3)
                    .stroke(Stroke::new(1.0_f32, t::LINE))
                    .corner_radius(CornerRadius::same(t::RADIUS))
                    .inner_margin(Margin::symmetric(18, 10))
                    .show(ui, |ui| {
                        ui.label(RichText::new(message).size(12.5));
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

enum Which {
    Trending,
    Search,
    Feed,
    Channel,
    Detail,
    History,
    Stats,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(message) = self.rx.try_recv() {
            self.handle(message);
        }
        if self.capture_if_requested(ctx) {
            return;
        }
        self.account_watch();
        if let Some(err) = self.player.take_error() {
            self.toast(err);
        }
        self.tick_suggestions();
        if self.total_watch_at.elapsed() > Duration::from_secs(5) {
            self.total_watch = self
                .with_db(|conn| db::stats_overview(conn).total_seconds)
                .unwrap_or(self.total_watch);
            self.total_watch_at = Instant::now();
        }

        self.top_bar(ctx);
        self.side_bar(ctx);
        self.player_bar(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(t::BG)
                    .inner_margin(Margin::symmetric(24, 20)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("content")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.route.clone() {
                        Route::Trending => self.view_trending(ui),
                        Route::Search(query, page) => self.view_search(ui, query, page),
                        Route::Subscriptions => self.view_subscriptions(ui),
                        Route::Channel(id) => self.view_channel(ui, id),
                        Route::Watch(id) => self.view_watch(ui, id),
                        Route::History => self.view_history(ui),
                        Route::Stats => self.view_stats(ui),
                        Route::Settings => self.view_settings(ui),
                    });
            });

        self.suggestions_layer(ctx);
        self.toast_layer(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_watch();
        self.player.stop();
    }
}

fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join("Library/Application Support/RustTube")
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("RustTube")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".local/share/rusttube")
    }
}
