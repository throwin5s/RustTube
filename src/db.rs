//! Local SQLite storage.
//!
//! Sync design in one paragraph: subscriptions and history are records with an
//! `updated_at` stamp and a `deleted` tombstone, merged last-write-wins per
//! record so deletions propagate across machines. Watch statistics are keyed by
//! (day, device, channel) and every device only ever raises its own counters,
//! so merging takes the maximum per row instead of summing. That makes the merge
//! idempotent: syncing twice, or from three machines at once, never inflates
//! your watch time.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SYNC_VERSION: u32 = 2;

/// Preferences that follow you to other machines. Device-specific state
/// (volume, player path, sync credentials, device id) deliberately stays local.
pub const SYNCED_SETTINGS: &[&str] = &[
    "region",
    "playback",
    "wide_player",
    "playback_speed",
    "quality",
    "previews",
    "sidebar_visible",
    "sidebar_hover",
    "watch_zoom",
    "anim_speed",
    "history_enabled",
];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Subscription {
    pub channel_id: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub channel_avatar: String,
    #[serde(default)]
    pub subscribed_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HistoryEntry {
    pub video_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub watched_at: i64,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WatchStat {
    pub day: String,
    pub device_id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub seconds: i64,
    #[serde(default)]
    pub videos: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SavedEntry {
    pub video_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub saved_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Playlist {
    pub playlist_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub deleted: bool,
    /// Computed locally for display; not meaningful in a sync payload.
    #[serde(default)]
    pub item_count: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PlaylistItem {
    pub playlist_id: String,
    pub video_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub added_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SyncedSetting {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct SyncPayload {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub generated_at: i64,
    #[serde(default)]
    pub generated_by: String,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub watch_stats: Vec<WatchStat>,
    #[serde(default)]
    pub saved: Vec<SavedEntry>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    #[serde(default)]
    pub playlist_items: Vec<PlaylistItem>,
    #[serde(default)]
    pub settings: Vec<SyncedSetting>,
}

#[derive(Serialize, Default, Debug)]
pub struct MergeReport {
    pub records_applied: usize,
    pub subscriptions: usize,
    pub history: usize,
    pub watch_stats: usize,
    pub saved: usize,
    pub playlists: usize,
    pub playlist_items: usize,
    pub settings: usize,
}

#[derive(Serialize, Clone)]
pub struct DayStat {
    pub day: String,
    pub seconds: i64,
    pub videos: i64,
}

#[derive(Serialize, Clone)]
pub struct ChannelStat {
    pub channel_id: String,
    pub channel_name: String,
    pub seconds: i64,
    pub videos: i64,
}

#[derive(Serialize, Clone, Default)]
pub struct StatsOverview {
    pub total_seconds: i64,
    pub today_seconds: i64,
    pub week_seconds: i64,
    pub month_seconds: i64,
    pub total_videos: i64,
    pub streak_days: i64,
    pub devices: i64,
    pub daily: Vec<DayStat>,
    pub top_channels: Vec<ChannelStat>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    init(&conn)?;
    Ok(conn)
}

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS subscriptions (
            channel_id     TEXT PRIMARY KEY,
            channel_name   TEXT NOT NULL DEFAULT '',
            channel_avatar TEXT NOT NULL DEFAULT '',
            subscribed_at  INTEGER NOT NULL DEFAULT 0,
            updated_at     INTEGER NOT NULL DEFAULT 0,
            deleted        INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS history (
            video_id     TEXT PRIMARY KEY,
            title        TEXT NOT NULL DEFAULT '',
            channel_id   TEXT NOT NULL DEFAULT '',
            channel_name TEXT NOT NULL DEFAULT '',
            duration     INTEGER NOT NULL DEFAULT 0,
            watched_at   INTEGER NOT NULL DEFAULT 0,
            position     INTEGER NOT NULL DEFAULT 0,
            updated_at   INTEGER NOT NULL DEFAULT 0,
            deleted      INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS history_watched ON history(watched_at DESC);

        CREATE TABLE IF NOT EXISTS watch_stats (
            day          TEXT NOT NULL,
            device_id    TEXT NOT NULL,
            channel_id   TEXT NOT NULL DEFAULT '',
            channel_name TEXT NOT NULL DEFAULT '',
            seconds      INTEGER NOT NULL DEFAULT 0,
            videos       INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, device_id, channel_id)
        );

        CREATE TABLE IF NOT EXISTS video_watch (
            video_id     TEXT NOT NULL,
            day          TEXT NOT NULL,
            device_id    TEXT NOT NULL,
            channel_id   TEXT NOT NULL DEFAULT '',
            channel_name TEXT NOT NULL DEFAULT '',
            seconds      INTEGER NOT NULL DEFAULT 0,
            videos       INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (video_id, day, device_id)
        );

        CREATE TABLE IF NOT EXISTS saved (
            video_id     TEXT PRIMARY KEY,
            title        TEXT NOT NULL DEFAULT '',
            channel_id   TEXT NOT NULL DEFAULT '',
            channel_name TEXT NOT NULL DEFAULT '',
            duration     INTEGER NOT NULL DEFAULT 0,
            saved_at     INTEGER NOT NULL DEFAULT 0,
            updated_at   INTEGER NOT NULL DEFAULT 0,
            deleted      INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS playlists (
            playlist_id TEXT PRIMARY KEY,
            name        TEXT NOT NULL DEFAULT '',
            created_at  INTEGER NOT NULL DEFAULT 0,
            updated_at  INTEGER NOT NULL DEFAULT 0,
            deleted     INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS playlist_items (
            playlist_id  TEXT NOT NULL,
            video_id     TEXT NOT NULL,
            title        TEXT NOT NULL DEFAULT '',
            channel_id   TEXT NOT NULL DEFAULT '',
            channel_name TEXT NOT NULL DEFAULT '',
            duration     INTEGER NOT NULL DEFAULT 0,
            added_at     INTEGER NOT NULL DEFAULT 0,
            updated_at   INTEGER NOT NULL DEFAULT 0,
            deleted      INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (playlist_id, video_id)
        );

        -- When each synced preference was last changed here, so a merge can tell
        -- a real edit from another machine's default.
        CREATE TABLE IF NOT EXISTS settings_meta (
            key        TEXT PRIMARY KEY,
            updated_at INTEGER NOT NULL DEFAULT 0
        );

        PRAGMA user_version = 2;
        "#,
    )
}

pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |row| row.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    if SYNCED_SETTINGS.contains(&key) {
        conn.execute(
            "INSERT INTO settings_meta(key, updated_at) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at",
            params![key, now_ms()],
        )?;
    }
    Ok(())
}

/// Stable per-install identifier used to partition watch statistics.
pub fn device_id(conn: &Connection) -> rusqlite::Result<String> {
    if let Some(existing) = get_setting(conn, "device_id") {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    set_setting(conn, "device_id", &id)?;
    Ok(id)
}

// ---------------------------------------------------------------- subscriptions

pub fn is_subscribed(conn: &Connection, channel_id: &str) -> bool {
    conn.query_row(
        "SELECT deleted FROM subscriptions WHERE channel_id = ?1",
        params![channel_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|deleted| deleted == 0)
    .unwrap_or(false)
}

/// Backfill the artwork for a subscription that was saved without one.
pub fn set_subscription_avatar(
    conn: &Connection,
    channel_id: &str,
    avatar: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE subscriptions SET channel_avatar = ?2, updated_at = ?3 \
         WHERE channel_id = ?1",
        params![channel_id, avatar, now_ms()],
    )?;
    Ok(())
}

/// Subscribe or unsubscribe. Unsubscribing writes a tombstone so the change
/// survives a merge with a machine that still has the old record.
pub fn set_subscription(
    conn: &Connection,
    channel_id: &str,
    channel_name: &str,
    channel_avatar: &str,
    subscribe: bool,
) -> rusqlite::Result<bool> {
    let now = now_ms();
    if subscribe {
        conn.execute(
            "INSERT INTO subscriptions(channel_id, channel_name, channel_avatar, subscribed_at, updated_at, deleted)
             VALUES(?1, ?2, ?3, ?4, ?4, 0)
             ON CONFLICT(channel_id) DO UPDATE SET
                channel_name   = excluded.channel_name,
                channel_avatar = excluded.channel_avatar,
                subscribed_at  = CASE WHEN subscriptions.deleted = 1 THEN excluded.subscribed_at
                                      ELSE subscriptions.subscribed_at END,
                updated_at     = MAX(excluded.updated_at, subscriptions.updated_at + 1),
                deleted        = 0",
            params![channel_id, channel_name, channel_avatar, now],
        )?;
        Ok(true)
    } else {
        conn.execute(
            // Strictly newer than the row it replaces: an unsubscribe in the same
            // millisecond as the subscribe must still win a last-write-wins merge.
            "UPDATE subscriptions SET deleted = 1, updated_at = MAX(?2, updated_at + 1)
             WHERE channel_id = ?1",
            params![channel_id, now],
        )?;
        Ok(false)
    }
}

pub fn list_subscriptions(conn: &Connection) -> Vec<Subscription> {
    let mut stmt = match conn.prepare(
        "SELECT channel_id, channel_name, channel_avatar, subscribed_at, updated_at, deleted
         FROM subscriptions WHERE deleted = 0
         ORDER BY channel_name COLLATE NOCASE ASC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |row| {
        Ok(Subscription {
            channel_id: row.get(0)?,
            channel_name: row.get(1)?,
            channel_avatar: row.get(2)?,
            subscribed_at: row.get(3)?,
            updated_at: row.get(4)?,
            deleted: row.get::<_, i64>(5)? != 0,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

// --------------------------------------------------------------------- history

pub struct WatchUpdate<'a> {
    pub video_id: &'a str,
    pub title: &'a str,
    pub channel_id: &'a str,
    pub channel_name: &'a str,
    pub duration: i64,
    pub position: i64,
    pub seconds: i64,
    pub new_view: bool,
}

pub fn record_watch(conn: &Connection, update: &WatchUpdate<'_>) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO history(video_id, title, channel_id, channel_name, duration, watched_at, position, updated_at, deleted)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, 0)
         ON CONFLICT(video_id) DO UPDATE SET
            title        = excluded.title,
            channel_id   = excluded.channel_id,
            channel_name = excluded.channel_name,
            duration     = excluded.duration,
            watched_at   = excluded.watched_at,
            position     = excluded.position,
            updated_at   = MAX(excluded.updated_at, history.updated_at + 1),
            deleted      = 0",
        params![
            update.video_id,
            update.title,
            update.channel_id,
            update.channel_name,
            update.duration,
            now,
            update.position
        ],
    )?;

    if update.seconds > 0 || update.new_view {
        let seconds = update.seconds.max(0);
        let videos = if update.new_view { 1 } else { 0 };
        let day = today_local();
        let device = device_id(conn)?;
        conn.execute(
            "INSERT INTO watch_stats(day, device_id, channel_id, channel_name, seconds, videos)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(day, device_id, channel_id) DO UPDATE SET
                seconds      = watch_stats.seconds + excluded.seconds,
                videos       = watch_stats.videos + excluded.videos,
                channel_name = CASE WHEN excluded.channel_name <> '' THEN excluded.channel_name
                                    ELSE watch_stats.channel_name END",
            params![
                &day,
                &device,
                update.channel_id,
                update.channel_name,
                seconds,
                videos
            ],
        )?;
        // Per-video copy of this device's contribution, so deleting the video
        // from history can take exactly that time back out of the totals.
        conn.execute(
            "INSERT INTO video_watch(video_id, day, device_id, channel_id, channel_name, seconds, videos)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(video_id, day, device_id) DO UPDATE SET
                seconds      = video_watch.seconds + excluded.seconds,
                videos       = video_watch.videos + excluded.videos,
                channel_name = CASE WHEN excluded.channel_name <> '' THEN excluded.channel_name
                                    ELSE video_watch.channel_name END",
            params![
                update.video_id,
                &day,
                &device,
                update.channel_id,
                update.channel_name,
                seconds,
                videos
            ],
        )?;
    }
    Ok(())
}

pub fn history_position(conn: &Connection, video_id: &str) -> i64 {
    conn.query_row(
        "SELECT position FROM history WHERE video_id = ?1 AND deleted = 0",
        params![video_id],
        |row| row.get(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(0)
}

pub fn list_history(conn: &Connection, limit: i64) -> Vec<HistoryEntry> {
    let mut stmt = match conn.prepare(
        "SELECT video_id, title, channel_id, channel_name, duration, watched_at, position, updated_at, deleted
         FROM history WHERE deleted = 0
         ORDER BY watched_at DESC LIMIT ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![limit], |row| {
        Ok(HistoryEntry {
            video_id: row.get(0)?,
            title: row.get(1)?,
            channel_id: row.get(2)?,
            channel_name: row.get(3)?,
            duration: row.get(4)?,
            watched_at: row.get(5)?,
            position: row.get(6)?,
            updated_at: row.get(7)?,
            deleted: row.get::<_, i64>(8)? != 0,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn delete_history_item(conn: &Connection, video_id: &str) -> rusqlite::Result<()> {
    let device = device_id(conn)?;
    let rows: Vec<(String, String, i64, i64)> = match conn.prepare(
        "SELECT day, channel_id, seconds, videos FROM video_watch
         WHERE video_id = ?1 AND device_id = ?2",
    ) {
        Ok(mut stmt) => stmt
            .query_map(params![video_id, &device], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    for (day, channel_id, seconds, videos) in rows {
        conn.execute(
            "UPDATE watch_stats
             SET seconds = MAX(seconds - ?4, 0), videos = MAX(videos - ?5, 0)
             WHERE day = ?1 AND device_id = ?2 AND channel_id = ?3",
            params![day, &device, channel_id, seconds, videos],
        )?;
    }
    conn.execute(
        "DELETE FROM watch_stats WHERE seconds <= 0 AND videos <= 0",
        [],
    )?;
    conn.execute(
        "DELETE FROM video_watch WHERE video_id = ?1 AND device_id = ?2",
        params![video_id, &device],
    )?;
    conn.execute(
        "UPDATE history SET deleted = 1, updated_at = ?2 WHERE video_id = ?1",
        params![video_id, now_ms()],
    )?;
    Ok(())
}

pub fn clear_history(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE history SET deleted = 1, updated_at = ?1 WHERE deleted = 0",
        params![now_ms()],
    )?;
    Ok(())
}

pub fn reset_watch_stats(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM watch_stats", [])?;
    conn.execute("DELETE FROM video_watch", [])?;
    Ok(())
}

// ---------------------------------------------------------------------- saved

pub fn is_saved(conn: &Connection, video_id: &str) -> bool {
    conn.query_row(
        "SELECT deleted FROM saved WHERE video_id = ?1",
        params![video_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|deleted| deleted == 0)
    .unwrap_or(false)
}

pub fn set_saved(
    conn: &Connection,
    video_id: &str,
    title: &str,
    channel_id: &str,
    channel_name: &str,
    duration: i64,
    save: bool,
) -> rusqlite::Result<bool> {
    let now = now_ms();
    if save {
        conn.execute(
            "INSERT INTO saved(video_id, title, channel_id, channel_name, duration, saved_at, updated_at, deleted)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?6, 0)
             ON CONFLICT(video_id) DO UPDATE SET
                title        = excluded.title,
                channel_id   = excluded.channel_id,
                channel_name = excluded.channel_name,
                duration     = excluded.duration,
                saved_at     = CASE WHEN saved.deleted = 1 THEN excluded.saved_at
                                    ELSE saved.saved_at END,
                updated_at   = MAX(excluded.updated_at, saved.updated_at + 1),
                deleted      = 0",
            params![video_id, title, channel_id, channel_name, duration, now],
        )?;
        Ok(true)
    } else {
        conn.execute(
            "UPDATE saved SET deleted = 1, updated_at = MAX(?2, updated_at + 1)
             WHERE video_id = ?1",
            params![video_id, now],
        )?;
        Ok(false)
    }
}

pub fn list_saved(conn: &Connection, limit: i64) -> Vec<SavedEntry> {
    let mut stmt = match conn.prepare(
        "SELECT video_id, title, channel_id, channel_name, duration, saved_at, updated_at, deleted
         FROM saved WHERE deleted = 0 ORDER BY saved_at DESC LIMIT ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![limit], |row| {
        Ok(SavedEntry {
            video_id: row.get(0)?,
            title: row.get(1)?,
            channel_id: row.get(2)?,
            channel_name: row.get(3)?,
            duration: row.get(4)?,
            saved_at: row.get(5)?,
            updated_at: row.get(6)?,
            deleted: row.get::<_, i64>(7)? != 0,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

// ------------------------------------------------------------------ playlists

pub fn create_playlist(conn: &Connection, name: &str) -> rusqlite::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    conn.execute(
        "INSERT INTO playlists(playlist_id, name, created_at, updated_at, deleted)
         VALUES(?1, ?2, ?3, ?3, 0)",
        params![id, name.trim(), now],
    )?;
    Ok(id)
}

pub fn list_playlists(conn: &Connection) -> Vec<Playlist> {
    let mut stmt = match conn.prepare(
        "SELECT p.playlist_id, p.name, p.created_at, p.updated_at, p.deleted,
                (SELECT COUNT(*) FROM playlist_items i
                 WHERE i.playlist_id = p.playlist_id AND i.deleted = 0)
         FROM playlists p WHERE p.deleted = 0
         ORDER BY p.created_at ASC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |row| {
        Ok(Playlist {
            playlist_id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            updated_at: row.get(3)?,
            deleted: row.get::<_, i64>(4)? != 0,
            item_count: row.get(5)?,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn delete_playlist(conn: &Connection, playlist_id: &str) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "UPDATE playlists SET deleted = 1, updated_at = MAX(?2, updated_at + 1)
         WHERE playlist_id = ?1",
        params![playlist_id, now],
    )?;
    conn.execute(
        "UPDATE playlist_items SET deleted = 1, updated_at = MAX(?2, updated_at + 1)
         WHERE playlist_id = ?1 AND deleted = 0",
        params![playlist_id, now],
    )?;
    Ok(())
}

pub fn add_to_playlist(
    conn: &Connection,
    playlist_id: &str,
    video_id: &str,
    title: &str,
    channel_id: &str,
    channel_name: &str,
    duration: i64,
) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO playlist_items(playlist_id, video_id, title, channel_id, channel_name, duration, added_at, updated_at, deleted)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 0)
         ON CONFLICT(playlist_id, video_id) DO UPDATE SET
            title        = excluded.title,
            channel_id   = excluded.channel_id,
            channel_name = excluded.channel_name,
            duration     = excluded.duration,
            added_at     = CASE WHEN playlist_items.deleted = 1 THEN excluded.added_at
                                ELSE playlist_items.added_at END,
            updated_at   = MAX(excluded.updated_at, playlist_items.updated_at + 1),
            deleted      = 0",
        params![playlist_id, video_id, title, channel_id, channel_name, duration, now],
    )?;
    Ok(())
}

pub fn remove_from_playlist(
    conn: &Connection,
    playlist_id: &str,
    video_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE playlist_items SET deleted = 1, updated_at = MAX(?3, updated_at + 1)
         WHERE playlist_id = ?1 AND video_id = ?2",
        params![playlist_id, video_id, now_ms()],
    )?;
    Ok(())
}

pub fn is_in_playlist(conn: &Connection, playlist_id: &str, video_id: &str) -> bool {
    conn.query_row(
        "SELECT deleted FROM playlist_items WHERE playlist_id = ?1 AND video_id = ?2",
        params![playlist_id, video_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|deleted| deleted == 0)
    .unwrap_or(false)
}

pub fn list_playlist_items(conn: &Connection, playlist_id: &str) -> Vec<PlaylistItem> {
    let mut stmt = match conn.prepare(
        "SELECT playlist_id, video_id, title, channel_id, channel_name, duration, added_at, updated_at, deleted
         FROM playlist_items WHERE playlist_id = ?1 AND deleted = 0
         ORDER BY added_at ASC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![playlist_id], |row| {
        Ok(PlaylistItem {
            playlist_id: row.get(0)?,
            video_id: row.get(1)?,
            title: row.get(2)?,
            channel_id: row.get(3)?,
            channel_name: row.get(4)?,
            duration: row.get(5)?,
            added_at: row.get(6)?,
            updated_at: row.get(7)?,
            deleted: row.get::<_, i64>(8)? != 0,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

// ----------------------------------------------------------------- statistics

pub fn stats_overview(conn: &Connection) -> StatsOverview {
    let total_seconds: i64 = conn
        .query_row("SELECT COALESCE(SUM(seconds), 0) FROM watch_stats", [], |r| r.get(0))
        .unwrap_or(0);
    let total_videos: i64 = conn
        .query_row("SELECT COALESCE(SUM(videos), 0) FROM watch_stats", [], |r| r.get(0))
        .unwrap_or(0);
    let devices: i64 = conn
        .query_row("SELECT COUNT(DISTINCT device_id) FROM watch_stats", [], |r| r.get(0))
        .unwrap_or(0);

    let mut daily = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT day, COALESCE(SUM(seconds), 0), COALESCE(SUM(videos), 0)
             FROM watch_stats GROUP BY day",
        )
        .ok();
    let mut per_day: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();
    if let Some(stmt) = stmt.as_mut() {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        }) {
            for row in rows.flatten() {
                per_day.insert(row.0, (row.1, row.2));
            }
        }
    }

    let today = chrono::Local::now().date_naive();
    for offset in (0..30).rev() {
        let day = (today - chrono::Duration::days(offset)).format("%Y-%m-%d").to_string();
        let (seconds, videos) = per_day.get(&day).copied().unwrap_or((0, 0));
        daily.push(DayStat { day, seconds, videos });
    }

    let today_key = today.format("%Y-%m-%d").to_string();
    let today_seconds = per_day.get(&today_key).map(|v| v.0).unwrap_or(0);
    let week_seconds: i64 = daily.iter().rev().take(7).map(|d| d.seconds).sum();
    let month_seconds: i64 = daily.iter().map(|d| d.seconds).sum();

    // Watching streak: consecutive days with any watch time, anchored at today
    // or yesterday (so a streak is not lost before you watch anything today).
    let mut streak_days = 0;
    let mut probe = today;
    let mut allow_gap = true;
    loop {
        let key = probe.format("%Y-%m-%d").to_string();
        let seconds = per_day.get(&key).map(|v| v.0).unwrap_or(0);
        if seconds > 0 {
            streak_days += 1;
            probe -= chrono::Duration::days(1);
            allow_gap = false;
        } else if allow_gap && streak_days == 0 {
            probe -= chrono::Duration::days(1);
            allow_gap = false;
        } else {
            break;
        }
        if streak_days > 3650 {
            break;
        }
    }

    let mut top_channels = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT channel_id, MAX(channel_name), SUM(seconds), SUM(videos)
         FROM watch_stats WHERE channel_id <> ''
         GROUP BY channel_id ORDER BY SUM(seconds) DESC LIMIT 8",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok(ChannelStat {
                channel_id: row.get(0)?,
                channel_name: row.get(1)?,
                seconds: row.get(2)?,
                videos: row.get(3)?,
            })
        }) {
            top_channels = rows.flatten().collect();
        }
    }

    StatsOverview {
        total_seconds,
        today_seconds,
        week_seconds,
        month_seconds,
        total_videos,
        streak_days,
        devices,
        daily,
        top_channels,
    }
}

// ---------------------------------------------------------------------- sync

pub fn export_payload(conn: &Connection, generated_by: &str) -> SyncPayload {
    let subscriptions = match conn
        .prepare(
            "SELECT channel_id, channel_name, channel_avatar, subscribed_at, updated_at, deleted FROM subscriptions",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok(Subscription {
                    channel_id: row.get(0)?,
                    channel_name: row.get(1)?,
                    channel_avatar: row.get(2)?,
                    subscribed_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    deleted: row.get::<_, i64>(5)? != 0,
                })
            })?;
            Ok(rows.flatten().collect::<Vec<_>>())
        }) {
        Ok(list) => list,
        Err(_) => Vec::new(),
    };

    let history = match conn
        .prepare(
            "SELECT video_id, title, channel_id, channel_name, duration, watched_at, position, updated_at, deleted FROM history",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok(HistoryEntry {
                    video_id: row.get(0)?,
                    title: row.get(1)?,
                    channel_id: row.get(2)?,
                    channel_name: row.get(3)?,
                    duration: row.get(4)?,
                    watched_at: row.get(5)?,
                    position: row.get(6)?,
                    updated_at: row.get(7)?,
                    deleted: row.get::<_, i64>(8)? != 0,
                })
            })?;
            Ok(rows.flatten().collect::<Vec<_>>())
        }) {
        Ok(list) => list,
        Err(_) => Vec::new(),
    };

    let watch_stats = match conn
        .prepare("SELECT day, device_id, channel_id, channel_name, seconds, videos FROM watch_stats")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok(WatchStat {
                    day: row.get(0)?,
                    device_id: row.get(1)?,
                    channel_id: row.get(2)?,
                    channel_name: row.get(3)?,
                    seconds: row.get(4)?,
                    videos: row.get(5)?,
                })
            })?;
            Ok(rows.flatten().collect::<Vec<_>>())
        }) {
        Ok(list) => list,
        Err(_) => Vec::new(),
    };

    let saved = match conn
        .prepare(
            "SELECT video_id, title, channel_id, channel_name, duration, saved_at, updated_at, deleted FROM saved",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok(SavedEntry {
                    video_id: row.get(0)?,
                    title: row.get(1)?,
                    channel_id: row.get(2)?,
                    channel_name: row.get(3)?,
                    duration: row.get(4)?,
                    saved_at: row.get(5)?,
                    updated_at: row.get(6)?,
                    deleted: row.get::<_, i64>(7)? != 0,
                })
            })?;
            Ok(rows.flatten().collect::<Vec<_>>())
        }) {
        Ok(list) => list,
        Err(_) => Vec::new(),
    };

    let playlists = match conn
        .prepare("SELECT playlist_id, name, created_at, updated_at, deleted FROM playlists")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok(Playlist {
                    playlist_id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                    deleted: row.get::<_, i64>(4)? != 0,
                    item_count: 0,
                })
            })?;
            Ok(rows.flatten().collect::<Vec<_>>())
        }) {
        Ok(list) => list,
        Err(_) => Vec::new(),
    };

    let playlist_items = match conn
        .prepare(
            "SELECT playlist_id, video_id, title, channel_id, channel_name, duration, added_at, updated_at, deleted FROM playlist_items",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok(PlaylistItem {
                    playlist_id: row.get(0)?,
                    video_id: row.get(1)?,
                    title: row.get(2)?,
                    channel_id: row.get(3)?,
                    channel_name: row.get(4)?,
                    duration: row.get(5)?,
                    added_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    deleted: row.get::<_, i64>(8)? != 0,
                })
            })?;
            Ok(rows.flatten().collect::<Vec<_>>())
        }) {
        Ok(list) => list,
        Err(_) => Vec::new(),
    };

    let mut settings = Vec::new();
    for key in SYNCED_SETTINGS {
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at FROM settings_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0);
        if updated_at > 0 {
            if let Some(value) = get_setting(conn, key) {
                settings.push(SyncedSetting {
                    key: (*key).to_string(),
                    value,
                    updated_at,
                });
            }
        }
    }

    SyncPayload {
        version: SYNC_VERSION,
        generated_at: now_ms(),
        generated_by: generated_by.to_string(),
        subscriptions,
        history,
        watch_stats,
        saved,
        playlists,
        playlist_items,
        settings,
    }
}

pub fn merge_payload(conn: &mut Connection, payload: &SyncPayload) -> Result<MergeReport, String> {
    if payload.version > SYNC_VERSION {
        return Err(format!(
            "The sync file was written by a newer version of RustTube (format {}). Update this app instead of overwriting it.",
            payload.version
        ));
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut report = MergeReport::default();

    for sub in &payload.subscriptions {
        let changed = tx
            .execute(
                "INSERT INTO subscriptions(channel_id, channel_name, channel_avatar, subscribed_at, updated_at, deleted)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(channel_id) DO UPDATE SET
                    channel_name   = excluded.channel_name,
                    channel_avatar = excluded.channel_avatar,
                    subscribed_at  = excluded.subscribed_at,
                    updated_at     = excluded.updated_at,
                    deleted        = excluded.deleted
                 WHERE excluded.updated_at > subscriptions.updated_at",
                params![
                    sub.channel_id,
                    sub.channel_name,
                    sub.channel_avatar,
                    sub.subscribed_at,
                    sub.updated_at,
                    sub.deleted as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        report.subscriptions += changed;
    }

    for item in &payload.history {
        let changed = tx
            .execute(
                "INSERT INTO history(video_id, title, channel_id, channel_name, duration, watched_at, position, updated_at, deleted)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(video_id) DO UPDATE SET
                    title        = excluded.title,
                    channel_id   = excluded.channel_id,
                    channel_name = excluded.channel_name,
                    duration     = excluded.duration,
                    watched_at   = excluded.watched_at,
                    position     = excluded.position,
                    updated_at   = excluded.updated_at,
                    deleted      = excluded.deleted
                 WHERE excluded.updated_at > history.updated_at",
                params![
                    item.video_id,
                    item.title,
                    item.channel_id,
                    item.channel_name,
                    item.duration,
                    item.watched_at,
                    item.position,
                    item.updated_at,
                    item.deleted as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        report.history += changed;
    }

    for stat in &payload.watch_stats {
        // Per-device counters only ever grow, so max() is a safe idempotent merge.
        let changed = tx
            .execute(
                "INSERT INTO watch_stats(day, device_id, channel_id, channel_name, seconds, videos)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(day, device_id, channel_id) DO UPDATE SET
                    seconds      = MAX(watch_stats.seconds, excluded.seconds),
                    videos       = MAX(watch_stats.videos, excluded.videos),
                    channel_name = CASE WHEN excluded.channel_name <> '' THEN excluded.channel_name
                                        ELSE watch_stats.channel_name END
                 WHERE excluded.seconds > watch_stats.seconds
                    OR excluded.videos > watch_stats.videos
                    OR (watch_stats.channel_name = '' AND excluded.channel_name <> '')",
                params![
                    stat.day,
                    stat.device_id,
                    stat.channel_id,
                    stat.channel_name,
                    stat.seconds,
                    stat.videos
                ],
            )
            .map_err(|e| e.to_string())?;
        report.watch_stats += changed;
    }

    for item in &payload.saved {
        let changed = tx
            .execute(
                "INSERT INTO saved(video_id, title, channel_id, channel_name, duration, saved_at, updated_at, deleted)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(video_id) DO UPDATE SET
                    title        = excluded.title,
                    channel_id   = excluded.channel_id,
                    channel_name = excluded.channel_name,
                    duration     = excluded.duration,
                    saved_at     = excluded.saved_at,
                    updated_at   = excluded.updated_at,
                    deleted      = excluded.deleted
                 WHERE excluded.updated_at > saved.updated_at",
                params![
                    item.video_id,
                    item.title,
                    item.channel_id,
                    item.channel_name,
                    item.duration,
                    item.saved_at,
                    item.updated_at,
                    item.deleted as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        report.saved += changed;
    }

    for playlist in &payload.playlists {
        let changed = tx
            .execute(
                "INSERT INTO playlists(playlist_id, name, created_at, updated_at, deleted)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(playlist_id) DO UPDATE SET
                    name       = excluded.name,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    deleted    = excluded.deleted
                 WHERE excluded.updated_at > playlists.updated_at",
                params![
                    playlist.playlist_id,
                    playlist.name,
                    playlist.created_at,
                    playlist.updated_at,
                    playlist.deleted as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        report.playlists += changed;
    }

    for item in &payload.playlist_items {
        let changed = tx
            .execute(
                "INSERT INTO playlist_items(playlist_id, video_id, title, channel_id, channel_name, duration, added_at, updated_at, deleted)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(playlist_id, video_id) DO UPDATE SET
                    title        = excluded.title,
                    channel_id   = excluded.channel_id,
                    channel_name = excluded.channel_name,
                    duration     = excluded.duration,
                    added_at     = excluded.added_at,
                    updated_at   = excluded.updated_at,
                    deleted      = excluded.deleted
                 WHERE excluded.updated_at > playlist_items.updated_at",
                params![
                    item.playlist_id,
                    item.video_id,
                    item.title,
                    item.channel_id,
                    item.channel_name,
                    item.duration,
                    item.added_at,
                    item.updated_at,
                    item.deleted as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        report.playlist_items += changed;
    }

    for setting in &payload.settings {
        if !SYNCED_SETTINGS.contains(&setting.key.as_str()) {
            continue;
        }
        let local = tx
            .query_row(
                "SELECT updated_at FROM settings_meta WHERE key = ?1",
                params![setting.key],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        if setting.updated_at > local {
            tx.execute(
                "INSERT INTO settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![setting.key, setting.value],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO settings_meta(key, updated_at) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at",
                params![setting.key, setting.updated_at],
            )
            .map_err(|e| e.to_string())?;
            report.settings += 1;
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    report.records_applied = report.subscriptions
        + report.history
        + report.watch_stats
        + report.saved
        + report.playlists
        + report.playlist_items
        + report.settings;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        init(&conn).expect("init");
        conn
    }

    #[test]
    fn subscription_roundtrip_and_tombstone() {
        let mut a = memory_db();
        assert!(!is_subscribed(&a, "UC1"));
        set_subscription(&a, "UC1", "Channel One", "avatar", true).unwrap();
        assert!(is_subscribed(&a, "UC1"));

        let payload = export_payload(&a, "dev-a");

        let mut b = memory_db();
        merge_payload(&mut b, &payload).unwrap();
        assert!(is_subscribed(&b, "UC1"));
        assert_eq!(list_subscriptions(&b).len(), 1);

        // Unsubscribe on B, then merge back into A: deletion must win.
        set_subscription(&b, "UC1", "Channel One", "avatar", false).unwrap();
        let payload_b = export_payload(&b, "dev-b");
        merge_payload(&mut a, &payload_b).unwrap();
        assert!(!is_subscribed(&a, "UC1"), "tombstone should propagate");
    }

    #[test]
    fn older_records_do_not_overwrite_newer() {
        let mut a = memory_db();
        set_subscription(&a, "UC1", "New Name", "avatar", true).unwrap();
        let mut payload = export_payload(&a, "dev-a");
        payload.subscriptions[0].updated_at -= 60_000;
        payload.subscriptions[0].channel_name = "Stale Name".into();
        merge_payload(&mut a, &payload).unwrap();
        assert_eq!(list_subscriptions(&a)[0].channel_name, "New Name");
    }

    #[test]
    fn watch_stats_merge_without_double_counting() {
        let a = memory_db();
        let mut b = memory_db();
        record_watch(
            &a,
            &WatchUpdate {
                video_id: "v1",
                title: "One",
                channel_id: "UC1",
                channel_name: "Chan",
                duration: 100,
                position: 30,
                seconds: 30,
                new_view: true,
            },
        )
        .unwrap();
        record_watch(
            &b,
            &WatchUpdate {
                video_id: "v2",
                title: "Two",
                channel_id: "UC1",
                channel_name: "Chan",
                duration: 100,
                position: 20,
                seconds: 20,
                new_view: true,
            },
        )
        .unwrap();

        let payload_a = export_payload(&a, "dev-a");
        merge_payload(&mut b, &payload_a).unwrap();
        // Merging the same payload twice must not inflate totals.
        merge_payload(&mut b, &payload_a).unwrap();

        let overview = stats_overview(&b);
        assert_eq!(overview.total_seconds, 50);
        assert_eq!(overview.total_videos, 2);
        assert_eq!(overview.devices, 2);
        assert_eq!(overview.today_seconds, 50);
    }

    #[test]
    fn reset_watch_stats_clears_totals() {
        let conn = memory_db();
        record_watch(
            &conn,
            &WatchUpdate {
                video_id: "v1",
                title: "One",
                channel_id: "UC1",
                channel_name: "Chan",
                duration: 100,
                position: 30,
                seconds: 30,
                new_view: true,
            },
        )
        .unwrap();
        assert_eq!(stats_overview(&conn).total_seconds, 30);
        reset_watch_stats(&conn).unwrap();
        assert_eq!(stats_overview(&conn).total_seconds, 0);
        assert_eq!(stats_overview(&conn).total_videos, 0);
    }

    #[test]
    fn history_position_and_clear() {
        let conn = memory_db();
        record_watch(
            &conn,
            &WatchUpdate {
                video_id: "v1",
                title: "One",
                channel_id: "UC1",
                channel_name: "Chan",
                duration: 100,
                position: 45,
                seconds: 45,
                new_view: true,
            },
        )
        .unwrap();
        assert_eq!(history_position(&conn, "v1"), 45);
        assert_eq!(list_history(&conn, 10).len(), 1);
        clear_history(&conn).unwrap();
        assert_eq!(list_history(&conn, 10).len(), 0);
        assert_eq!(history_position(&conn, "v1"), 0);
    }

    #[test]
    fn rejects_future_payload_versions() {
        let mut conn = memory_db();
        let payload = SyncPayload { version: SYNC_VERSION + 1, ..Default::default() };
        assert!(merge_payload(&mut conn, &payload).is_err());
    }

    #[test]
    fn saved_and_playlists_roundtrip_through_sync() {
        let a = memory_db();
        set_saved(&a, "v1", "One", "UC1", "Chan", 100, true).unwrap();
        assert!(is_saved(&a, "v1"));
        let list = create_playlist(&a, "Watch later").unwrap();
        add_to_playlist(&a, &list, "v1", "One", "UC1", "Chan", 100).unwrap();

        let mut b = memory_db();
        merge_payload(&mut b, &export_payload(&a, "dev-a")).unwrap();
        assert!(is_saved(&b, "v1"));
        assert_eq!(list_playlists(&b)[0].item_count, 1);
        assert_eq!(list_playlist_items(&b, &list).len(), 1);

        // Removals propagate as tombstones instead of being resurrected.
        set_saved(&a, "v1", "", "", "", 0, false).unwrap();
        remove_from_playlist(&a, &list, "v1").unwrap();
        delete_playlist(&a, &list).unwrap();
        merge_payload(&mut b, &export_payload(&a, "dev-a")).unwrap();
        assert!(!is_saved(&b, "v1"));
        assert!(list_playlists(&b).is_empty());
        assert!(list_playlist_items(&b, &list).is_empty());
    }

    #[test]
    fn deleting_history_removes_its_watch_time() {
        let conn = memory_db();
        for (id, title, seconds) in [("v1", "One", 30), ("v2", "Two", 20)] {
            record_watch(
                &conn,
                &WatchUpdate {
                    video_id: id,
                    title,
                    channel_id: "UC1",
                    channel_name: "Chan",
                    duration: 100,
                    position: seconds,
                    seconds,
                    new_view: true,
                },
            )
            .unwrap();
        }
        assert_eq!(stats_overview(&conn).total_seconds, 50);
        assert_eq!(stats_overview(&conn).total_videos, 2);

        delete_history_item(&conn, "v1").unwrap();
        let overview = stats_overview(&conn);
        assert_eq!(overview.total_seconds, 20);
        assert_eq!(overview.total_videos, 1);
    }

    #[test]
    fn only_changed_settings_travel_to_other_devices() {
        let a = memory_db();
        set_setting(&a, "sidebar_hover", "1").unwrap();
        let payload = export_payload(&a, "dev-a");
        assert!(payload
            .settings
            .iter()
            .any(|s| s.key == "sidebar_hover" && s.value == "1"));

        let mut b = memory_db();
        merge_payload(&mut b, &payload).unwrap();
        assert_eq!(get_setting(&b, "sidebar_hover").as_deref(), Some("1"));

        // A newer local edit is not rolled back by an older snapshot.
        set_setting(&b, "sidebar_hover", "0").unwrap();
        merge_payload(&mut b, &payload).unwrap();
        assert_eq!(get_setting(&b, "sidebar_hover").as_deref(), Some("0"));
    }
}
