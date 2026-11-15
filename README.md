# RustTube

A native desktop YouTube client written entirely in Rust, in the spirit of
[FreeTube](https://github.com/freetubeapp/freetube) but built the way
[Spotifast](https://github.com/crmne/spotifast) is: **egui for the interface, no
browser engine anywhere**. Subscriptions, watch history and watch-time
statistics live in local SQLite and sync between machines through cloud storage
you already own.

No Google account, no RustTube account, no telemetry, no webview.

## Interface

Red-on-black, native widgets, drawn by egui itself — no HTML, no CSS, no icon
font. Layout follows the same shape as Spotifast: a sidebar with navigation and
your subscriptions, a top bar with search and sync state, a scrolling content
area, and a player bar pinned to the bottom once something is playing.

| Area | What it does |
| --- | --- |
| Trending | Region-based card grid with thumbnails, duration badges and view counts |
| Search | Channels and videos, with live channel suggestions (icon + name) as you type |
| Subscriptions | Latest uploads from the channels you follow |
| Channel | Banner, avatar, subscriber count, subscribe toggle, full upload list |
| Watch | Poster with play/pause, title, likes, description, related rail |
| History | Resume points, per-item progress, remove and clear |
| Statistics | Total/weekly watch time, videos opened, streak, 30-day chart, top channels |
| Settings | Instance, region, history, cloud sync, local data |

## Playback

egui cannot decode video, so RustTube links **libmpv** and renders its frames
inside the app window through an OpenGL paint callback. There is no separate
player window and no browser engine: mpv decodes, egui draws the result in the
watch page, with the same 8px corners as the rest of the interface.

Starting a video is kept fast deliberately:

- **One long-lived libmpv instance.** It stays alive for the whole session, so
  loading a video is a single `loadfile` command — no process spawn, no socket
  handshake. Stopping returns it to idle instead of tearing it down.
- **One HLS variant, cached.** `src/stream.rs` asks `yt-dlp` for the HLS master
  playlist, then picks the best variant at or below 1080p and hands *that* to
  mpv. Opening one variant instead of the master skips subtitle playlists and
  ffmpeg's variant probing: measured **0.5 s to first frame at 1080p**, against
  ~7 s when mpv resolves YouTube itself.
- **Prefetch on hover.** Resting the pointer on a card for 140 ms starts the
  resolution, so a click usually finds the URL already waiting.
- **Immediate feedback.** The player bar appears the moment you click and shows
  "Starting…" until mpv reports a real position, so the app never looks stuck.

Why HLS rather than the direct DASH URLs `yt-dlp -g` prints: YouTube now answers
403 to those unless the client carries a PO token, which mpv does not. ffmpeg
fetching HLS segments is accepted, so playback goes through that path. If a
video has no HLS manifest at all, the resolver falls back to the Android
client's muxed stream (360p, but still fetched successfully), and then to mpv's
own yt-dlp hook.

Playback state arrives through libmpv's event loop, which observes `time-pos`,
`duration`, `pause` and `eof-reached`. That drives the UI *and* the statistics
honestly — watch time only accrues while mpv reports playing. The player bar
still controls pause, seeking and volume, and the video surface itself is
clickable.

## Run it

Requirements:

- Rust 1.85+
- **libmpv** development files (the in-process player is linked at build time)
  - macOS: `brew install mpv`
  - Windows: a prebuilt libmpv, with `LIBMPV_DIR` pointing at the folder that
    contains `libmpv.dll.a`
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) on `PATH`, or shipped next to the
  binary (`brew install yt-dlp` / `winget install yt-dlp` for development)

```bash
cargo run            # debug
cargo test           # unit + two-device sync tests
cargo build --release
```

macOS and Windows are both supported: the GL renderer has a Cocoa/OpenGL and a
WGL implementation, and the app looks for a bundled `yt-dlp` before falling
back to `PATH`.

## Releases

Pushing a `v*` tag (or running the Release workflow manually) builds both
platforms on GitHub Actions and attaches:

- `RustTube-<version>-macos-arm64.zip` — `RustTube.app` with libmpv and yt-dlp
  bundled into `Contents/Frameworks` and `Contents/MacOS`
- `RustTube-<version>-windows-x64.zip` — `RustTube.exe` with `libmpv-2.dll` and
  `yt-dlp.exe` alongside it

Both are unsigned. macOS needs right-click → Open the first time (or
`xattr -dr com.apple.quarantine RustTube.app`); Windows may show SmartScreen,
where More info → Run anyway works. App data stays local
(`%APPDATA%\RustTube` on Windows, `~/Library/Application Support/RustTube` on
macOS).

## Data and sync

| Platform | Location |
| --- | --- |
| macOS | `~/Library/Application Support/RustTube/rusttube.db` |
| Windows | `%APPDATA%\RustTube\rusttube.db` |

Settings → Cross-device sync writes **one** `rusttube-sync.json` snapshot into a
cloud-synced folder (iCloud Drive, OneDrive, Dropbox, a network share) or any
WebDAV collection. Your provider's own client moves the file; RustTube only
merges it. There is no server in between.

### Merge rules

- **Subscriptions and history**: per-record last-write-wins on `updated_at`
  (unix milliseconds). Deleting writes a tombstone, so a delete on your laptop
  cannot be resurrected by an older record from your desktop.
- **Watch time**: stored per `(day, device, channel)` and each device only ever
  raises its own counters, so merging takes `max()` per row instead of summing.
  Syncing twice, or from three machines, cannot inflate your watch time.
- Every sync is *pull → merge → push merged snapshot*, so devices converge on
  the union of both. Unknown newer snapshot versions are refused, not
  overwritten.

### Credentials

The WebDAV password is stored locally in the app database, never in the synced
snapshot. Use provider app passwords; a future version should move this to the
OS keychain.

## Layout

```
src/main.rs    window bootstrap
src/app.rs     the whole egui interface: routing, views, async message pump
src/theme.rs   palette, painted icons, cards, progress bar, formatters
src/embed.rs   libmpv instance, OpenGL FBO renderer, egui paint callback
src/stream.rs  yt-dlp stream resolution, memory cache, deduplicated prefetch
src/api.rs     Invidious + Piped client with per-request instance fallback
src/db.rs      SQLite schema, watch writes, merge logic
src/sync.rs    folder + WebDAV backends, sync orchestration
```

Metadata comes from a chain of public instances (`invidious.materialio.us`,
`api.piped.private.coffee`, …) and fails over per request; the instance that
answered last is tried first next time. Any instance can be pinned in Settings.

## Known limitations

- Metadata availability depends on public instances; YouTube rate-limits them
  and disables individual endpoints. Playback still works when detail calls
  fail.
- No account features (likes, comments, playlists), no downloads, no sponsor
  skipping.
- Quality is capped at 1080p by default (`MAX_HEIGHT` in `src/stream.rs`). The
  master playlist carries 1440p and 2160p too, so a quality preference is a
  small change here.
- Sync is a single snapshot file. Two devices writing at the same instant can
  lose the loser's newest record until the next sync; per-record merges make
  that window small and non-destructive.
- Tombstones are kept forever; they are tiny.
- Pinned to egui/eframe 0.32 because eframe 0.36 changed the `App` trait to
  `ui(&mut Ui)`; upgrading is a mechanical port of `update` → `ui`.

## Verification

`cargo test` covers payload parsing, HTML/entity cleanup, path expansion, URL
building, merge semantics (tombstones, stale records, idempotent stats) and a
two-device folder sync round trip.

Two opt-in tests need the outside world:

```bash
cargo test mpv_plays_a_video_quickly -- --ignored --nocapture # real playback
cargo test live_instance_smoke_test -- --ignored --nocapture # real metadata
```
