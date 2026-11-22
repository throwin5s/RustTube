# RustTube

A native desktop YouTube client written entirely in Rust, in the spirit of
[FreeTube](https://github.com/freetubeapp/freetube) but built the way
[Spotifast](https://github.com/crmne/spotifast) is: **egui for the interface, no
browser engine anywhere**. Subscriptions, watch history and watch-time
statistics live in local SQLite and sync between machines through cloud storage
you already own.

No Google account, no RustTube account, no telemetry, no webview, no server of
its own. Privacy is structural: the only outbound requests are the metadata and
stream lookups against public instances and YouTube, plus the updater's public
GitHub release check — no identifiers, no analytics, and the update check can be
turned off in Settings. Two optional features leave the machine only when you
enable them: SponsorBlock (the video ID and category names) and AI summaries
(the caption text, title and channel to the endpoint you configure).

## Interface

Red-on-black, native widgets, drawn by egui itself — no HTML, no CSS, no icon
font. Player glyphs use bundled [Lucide 0.468.0](https://github.com/lucide-icons/lucide/tree/f12b0de177fbc2a6795e99be065887e72b237123) icons ([license](assets/lucide/LICENSE)). Layout follows the same shape as Spotifast: a sidebar with navigation and
your subscriptions, a top bar with search and sync state, a scrolling content
area, and a player bar pinned to the bottom once something is playing.

### Icon skins

Player and status icons can be replaced without rebuilding. A skin is a folder
under the app data directory:

- macOS: `~/Library/Application Support/RustTube/skins/`
- Windows: `%APPDATA%\RustTube\skins\`
- Linux: `~/.local/share/rusttube/skins/`

Each folder contains `skin.json` and white-on-transparent PNG masks. Pick it in
Settings → Interface → Icon skin, then use **Rescan skins** after adding or
editing one. `RUSTTUBE_SKINS_DIR` points at a portable skins folder and
`RUSTTUBE_ICON_SKIN=<name>` forces a skin for one launch. Missing entries fall
back to the bundled Lucide glyphs.

On macOS the dropdown also offers **SF Symbols (macOS)**. That mode asks AppKit
for Apple's system symbols at runtime, so no SF Symbols files are stored in the
app or the skin pack. It is intentionally not available on Windows, where Apple
does not license the symbols; use Reel there.

| Area | What it does |
| --- | --- |
| Trending | Region-based card grid with thumbnails, duration badges and view counts |
| Search | Channels and videos, live suggestions as you type, and recommended searches from the channels you watch most |
| Subscriptions | Latest uploads plus a Continue watching shelf with resume points |
| Channel | Banner, avatar, subscriber count, subscribe toggle, full upload list |
| Watch | Player overlay, chapters, next-part button, notes, AI summary, and Up next |
| Playlists | Local playlists with watched progress, YouTube playlist search, and a queue that survives restarts |
| History | Resume points, per-item progress, remove and clear |
| Statistics | Total/weekly watch time, videos opened, streak, 30-day chart, top channels |
| Settings | Playback defaults, interface and gamepad, instance, region, history, SponsorBlock, AI, cloud sync, local data |

Settings > Interface has two optional transitions: the sidebar can reveal itself
when the pointer touches the left edge (the top-bar button then pins it open),
and opening a video can grow the clicked thumbnail into the player instead of
cutting to the new page. Animation speed (Slow / Normal / Fast) scales both the
sidebar slide and the video open/close transitions; Normal is the default and
Reset returns to it. Channel pages load their next page as you approach the end
of the grid, so there is no button to press.

## Playback

egui cannot decode video, so by default RustTube links **libmpv** and renders its
frames inside the app window through an OpenGL paint callback. Settings >
Playback can instead open the stream in a normal **mpv** window. Both modes use
mpv for decoding and yt-dlp for resolution; the external mode talks to mpv over
its IPC socket so history, resume points and watch time keep working.

Starting a video is kept fast deliberately:

- **One long-lived player.** The embedded libmpv instance stays alive for the
  whole session, so loading a video is a single `loadfile` command — no process
  spawn, no socket handshake. External mode likewise keeps one mpv process warm;
  stopping returns it to idle instead of tearing it down.
- **One HLS variant, cached.** `src/stream.rs` first asks YouTube's visionOS
  InnerTube player response for the HLS master playlist (typically ~0.1–0.2 s),
  then picks the best variant at or below the selected quality and hands *that*
  to mpv. If YouTube refuses that response, the existing `yt-dlp` path resolves
  the same master instead. Auto uses 1080p; 2160p, 1440p, 1080p, 720p, 480p and
  360p can be chosen from the player or Settings. Opening one variant instead
  of the master skips subtitle playlists and ffmpeg's variant probing: measured
  **~1.0 s click-to-first-frame**, against ~7 s when mpv resolves YouTube
  itself. Streams are cached per video and quality, and changing quality while
  playing reloads the same position and pause state.
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
honestly — watch time only accrues while mpv reports playing. The video surface
itself is clickable, and its overlay has a slim seek bar with a hover timestamp,
play/pause, 10-second skips, mute, fullscreen and a **Playback settings** menu
for volume, quality and speed. The bookmark menu saves videos or adds them to a
playlist. The wide-player toggle lives in Settings. Speed, mute and quality
persist in the settings table. When the instance APIs fail or return no
description, RustTube asks the bundled yt-dlp for public metadata without
interrupting playback.

Hover previews use a separate muted, looping libmpv instance. After 140 ms the
stream is prefetched; after 700 ms the thumbnail plays from 10% of its duration
(clamped to 5–30 seconds) with a small **Preview** badge. They work in the card
grid, the related rail and History, stop when the pointer leaves the thumbnail,
and stay on the poster for live items. Settings > Playback can disable them.

### Chapters, queue and sleep timer

A description with leading timestamps becomes a clickable chapter list under the
video; **N** jumps to the next chapter. When the related list has the following
part or episode from the same channel, the title row gets a **Next** button. The
queue is saved locally and survives restarts. The player settings menu has a
sleep timer: 15, 30 or 60 minutes, or stop at the end of the video (**T** cycles
it).

### SponsorBlock (opt-in)

Off by default. When enabled in Settings, RustTube asks the public SponsorBlock
API which ranges in the current video are skippable and jumps over the
categories you choose. Only the video ID and category names are sent — no
account, history or watch position. The player menu lists the segments it found.
Mute and highlight submissions are ignored; only `skip` actions are used.

### Gamepads

With a standard gamepad connected, **A** plays/pauses, **X** mutes, **Y**
fullscreen, the D-pad seeks and changes volume, and the triggers change speed.
Gamepad input only applies while a video is open and can be disabled in
Settings → Interface.

### Keyboard shortcuts

Shortcuts are ignored while a text field has the keyboard.

| Key | Action |
| --- | --- |
| Space, K | Play/pause |
| J, L | Seek 10 seconds |
| Left, Right | Seek 5 seconds |
| Up, Down | Volume ±5 |
| M | Mute/unmute |
| O, P | Speed down/up |
| F | Fullscreen (the player bar has the same button) |
| N | Next chapter |
| T | Cycle the sleep timer |
| Esc | Leave fullscreen |

## Run it

Requirements:

- Rust 1.85+
- **libmpv** development files (the embedded player is linked at build time)
  - macOS: `brew install mpv`
  - Windows: a prebuilt libmpv, with `LIBMPV_DIR` pointing at the folder that
    contains `libmpv.dll.a`
- **mpv** on PATH for Settings > Playback > Separate MPV window (a path can be
  set there instead; macOS auto-detects Homebrew locations)
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) on `PATH`, or shipped next to the
  binary (`brew install yt-dlp` / `winget install yt-dlp` for development)

```bash
cargo run            # debug
cargo test           # unit + two-device sync tests
cargo build --release
```

### Cold-start benchmark

The cold-start proof is a repo script rather than a set of shell incantations.
It builds the debug binary, launches a **fresh process for every run** (so the
stream cache and hover prefetch are cold), autoplays the fixed
`dQw4w9WgXcQ` search result, reads the app's
`click → first frame` timing, and prints the mean/median/min/max:

```bash
./scripts/cold-start.sh              # current direct-InnerTube path, 5 runs
./scripts/cold-start.sh --legacy     # old yt-dlp client path, for the ~3.2 s before-style number
./scripts/cold-start.sh --fallback   # current yt-dlp fallback, with InnerTube disabled
./scripts/cold-start.sh --runs 10    # increase the sample size
```

It needs a desktop session, network access, and `yt-dlp` on `PATH` (or next to
the binary). The app is killed as soon as the first-frame line appears; raw
logs are kept in a temporary directory printed at the end. `--no-build` reuses
`target/debug/rusttube` and skips the build step.

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

### Updates

Released builds update themselves. Once a day RustTube asks GitHub for the
newest release and, when it is newer, shows an **Update** button in the top bar
and in Settings > Local data.

- **macOS:** `Update & restart` replaces the whole `RustTube.app` bundle —
  Frameworks, libmpv and the bundled `yt-dlp` included — checks the download
  against the SHA-256 digest GitHub publishes for that asset, and relaunches
  into the new build. It needs to be running from an `.app` bundle; a loose
  `cargo run` binary only ever reports that a release exists.
- **Windows:** `Update & restart` downloads and unpacks the release for you.
  After RustTube closes, an update helper replaces the program files in the
  existing folder and reopens `RustTube.exe`. Run the packaged `RustTube.exe`
  from a folder you can write to; no manual download or replacement is needed.


On both platforms, your library and settings stay in the separate data folder
listed below.

Settings > Local data has **Check for updates automatically** (on by default)
and a manual check. Publishing a release is just tagging: the workflow attaches
both platform zips and the updater matches them by name. Set
`RUSTTUBE_NO_UPDATE=1` to skip the check entirely.

## Data and sync

| Platform | Location |
| --- | --- |
| macOS | `~/Library/Application Support/RustTube/rusttube.db` |
| Windows | `%APPDATA%\RustTube\rusttube.db` |

### Cloud sync

Settings → Cross-device sync writes **one** `rusttube-sync.json` snapshot into a
cloud folder you already have, or any WebDAV collection. Your provider's own
client moves the file; RustTube only merges it. There is no RustTube account and
no server in between.

Pick **Cloud account** and RustTube lists the clients it finds signed in on this
machine: **Google Drive, OneDrive, iCloud Drive, Dropbox, Box, pCloud, Proton
Drive, Nextcloud, MEGA, Tresorit, Yandex Disk, Jottacloud and Koofr**. It
never asks for a password — signing in to the provider's desktop app is the whole
setup — and it stores the *provider*, not a path, so the same choice means
`RustTube/rusttube-sync.json` inside that account on macOS and Windows alike.

Because no path is stored, a second machine set to the same provider picks up the
same file wherever that client mounts it (`~/Library/CloudStorage/…` on macOS, a
drive letter on Windows). On a fresh install with exactly one cloud client
signed in, RustTube adopts it automatically on first launch and syncs; otherwise
it is one click in Settings. **Sync automatically** runs a merge on launch and
every five minutes after that, always pull → merge → push.

Folder mode still works for a network share or anything else that syncs, and
WebDAV covers Nextcloud/ownCloud-style servers.

**MEGA without a desktop client** uses rclone: pick **rclone** and run the
bundled wizard once per machine (`rclone config` → new remote → choose MEGA →
email, password and 2FA code). RustTube lists the remotes rclone already has as
clickable chips and stores the path (e.g. `MegaRustTube:RustTube`) itself, so
there is nothing to retype or install, and it never stores your MEGA password.

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
OS keychain. rclone keeps its own credentials in `rclone.conf`, outside
RustTube — the app only stores the remote path.

## Layout

```
src/main.rs    window bootstrap
src/app.rs     the whole egui interface: routing, views, async message pump
src/theme.rs   palette, painted icons, cards, progress bar, formatters
src/embed.rs   libmpv instance, OpenGL FBO renderer, egui paint callback
src/player.rs  separate mpv process, IPC commands and playback snapshots
src/stream.rs  InnerTube/yt-dlp stream resolution, memory cache, deduplicated prefetch
src/api.rs     Invidious + Piped client, chapters and next-part matching
src/db.rs      SQLite schema, watch writes, continue-watching, merge logic
src/summary.rs AI summaries through an OpenAI-compatible endpoint
src/sponsor.rs opt-in SponsorBlock segment lookup and skipping
src/sync.rs    folder + WebDAV backends, sync orchestration
```

Metadata comes from a chain of public instances (`invidious.materialio.us`,
`api.piped.private.coffee`, …) and fails over per request; the instance that
answered last is tried first next time. Any instance can be pinned in Settings.

## Known limitations

- Metadata availability depends on public instances; YouTube rate-limits them
  and disables individual endpoints. Playback still works when detail calls
  fail.
- No Google account features (likes, comments, subscribing on YouTube), no
  downloads. Local playlists and the saved queue cover watching order; YouTube
  playlists can be searched and played but not edited.
- SponsorBlock only works when enabled and when the community has segments for
  that video. AI summaries need your own OpenAI-compatible endpoint and key.
- Quality is capped at 1080p by default (`DEFAULT_MAX_HEIGHT` in
  `src/stream.rs`). 1440p and 2160p are available when selected explicitly.
- Sync is a single snapshot file. Two devices writing at the same instant can
  lose the loser's newest record until the next sync; per-record merges make
  that window small and non-destructive.
- Tombstones are kept forever; they are tiny.
- Pinned to egui/eframe 0.32 because eframe 0.36 changed the `App` trait to
  `ui(&mut Ui)`; upgrading is a mechanical port of `update` → `ui`.

## Verification

`cargo test` covers payload parsing, HTML/entity cleanup, path expansion, URL
building, InnerTube → HLS stream parsing against captured fixtures, chapter
parsing, next-part matching, SponsorBlock segment filtering, merge semantics
(tombstones, stale records, idempotent stats) and a two-device folder sync round
trip.

Three opt-in tests need the outside world:

```bash
cargo test mpv_plays_a_video_quickly -- --ignored --nocapture # real playback
cargo test forced_innertube_failure_plays_through_the_ytdlp_fallback -- --ignored --nocapture
cargo test live_instance_smoke_test -- --ignored --nocapture # real metadata
```
