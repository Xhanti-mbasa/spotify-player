# Playback reliability patch — 0.25.1

Base: `Xhanti-mbasa/spotify-player`, commit `d7978f60ae6bb15c4eaf4cc47e309b00e0d5883c`.
The source manifest says 0.25.1. This does not establish that the fork is byte-for-byte identical to CachyOS package 0.25.1-1.1.

## Findings and changes

These are defects found by source inspection, not claims that the laptop's audio failures have been reproduced. Spotify authentication, device registration, audio output and the visualizer still require the manual tests below.

| Symptom | Cause found in source | File/function | Patch |
| --- | --- | --- | --- |
| Enter does nothing without an active session | Start sends no explicit device when buffered playback is absent; device initialization runs separately | `client/mod.rs`: `handle_player_request`, `resolve_playback_device`, `initialize_playback` | Discover the active device; otherwise check the integrated connection and device list with four attempts, 200 ms between checks, then explicitly activate it. Report failures. Preserve an existing active phone. Stop delayed startup initialization overriding user commands. |
| Pause/resume unreliable | Explicit resume and pause are skipped based on cached `is_playing`; concurrently spawned requests read the same snapshot | `client/handlers.rs`: `start_client_handler`; `client/mod.rs`: playback handlers; `cli/client.rs` | Dedicated ordered TUI/media command worker; shared async command lock for socket controls; always send explicit play/pause; consult fresh state when the snapshot is old. Return CLI errors. |
| First local track immediately pauses | Startup pause can remain armed until the first user-started track | `client/mod.rs`: `pause_streaming_on_startup`; `streaming.rs`: event receiver | Explicit playback intent disarms startup suppression. Failed pauses no longer report success. |
| Reconnection interferes with playback | Old connection is shut down after a new one is created | `client/mod.rs`: `new_streaming_connection`; `streaming.rs`: `new_connection` | Request old connection shutdown before replacement startup. Abort event receiver if Spirc initialization fails or its task ends; log task failures. |
| Phone track changes remain stale | Periodic polling defaults to zero; local events cannot observe all phone changes | `config/mod.rs`, `client/handlers.rs`, `examples/app.toml` | Default polling to 1000 ms, with 1000 ms minimum for positive values. Explicit zero remains supported. UI redraw interval remains independent. |
| Refreshes fight newer commands or amplify API traffic | Every event spawns delayed GETs; concurrent responses can overwrite newer state; middleware caches the player endpoint for another second | `client/mod.rs`: `retrieve_current_playback`, `update_playback_non_blocking`; `client/middleware.rs` | Coalesce event bursts with a trailing refresh; allow only one background player GET at a time, at least 750 ms between starts. Discard responses overtaken by commands/events. Player GET bypasses the recent-response cache while retaining rate-limit handling. Network GETs do not hold the command lock. |
| Metadata/artwork miss changes | New playback detected by title, not URI; genre lookup precedes image retrieval; old render state survives until new image arrives | `client/mod.rs`: `retrieve_current_playback`, `handle_new_playback_event`; `ui/playback.rs` | Compare playable URIs; update snapshot atomically; retrieve artwork before optional genre data; clear old artwork state when URL changes. Handle missing artist/album metadata without the affected unwraps. |
| Progress keeps moving while paused or jumps | Progress uses raw state even when optimistic pause state differs, and assumes progress/timestamp always exist | `state/player.rs`; `client/mod.rs`: `record_local_playback_event`; `streaming.rs` | Use buffered play/pause state, tolerate absent progress, and apply local position events only when both device and track match. Rebase progress after controls. |
| Volume wraps, loses presses, or oscillates | Signed subtraction casts below zero to u8; mouse addition can overflow; repeated keys reuse stale volume; requests complete out of order | `state/player.rs`, `event/mod.rs`, `client/handlers.rs`, `client/mod.rs`, `cli/client.rs` | Wide signed arithmetic and 0–100 clamp; immediate accumulation in the UI; skip unchanged values; collapse consecutive queued volume requests without crossing another control; temporarily preserve latest volume intent and then reconcile. Do not carry pending intent across devices. |
| Event handling stalls or failures disappear | Hook commands block the event receiver; reconciliation discards errors | `streaming.rs`; `client/mod.rs` | Run hooks on the blocking pool; log reconciliation and lifecycle failures. |
| Visualizer appears inactive | Local playback is prerequisite; play/pause events govern visualization activity | `streaming.rs` and existing `ui/streaming.rs` | Preserve the audio sink/FFT path; update activity on Playing/Paused/EndOfTrack. No visualizer redesign. Live audio testing remains required. |

## Execution paths

- Enter: `config/keymap.rs` → `event/mod.rs` → page/window handler (`handle_command_for_track_table_window`, track/episode list equivalents) → `PlayerRequest::StartPlayback` → ordered worker → `handle_state_player_request` → `handle_player_request` → `start_playback` → Spotify Web API → selected Connect device.
- Play, pause, toggle, next, previous, seek, repeat, shuffle and volume: global event handler or media controls → same worker/controller → endpoint with device ID. Socket controls use the shared controller; independent socket senders are ordered by lock acquisition.
- Device popup: `event/popup.rs` → `TransferPlayback` → same controller → Spotify transfer endpoint.
- Local startup: `new_session` → `new_streaming_connection` → `streaming::new_connection` → audio backend/visualization sink → Spirc task. Readiness checks inspect connection presence, session validity and device discovery; they do not prove audible output.
- Local events: librespot event receiver → matching local position/play state → coalesced refresh.
- Remote state: `start_player_event_watcher` → `GetCurrentPlayback` → serialized background snapshot → shared player state → `ui::run` → playback metadata/artwork.
- Queue refresh remains separately throttled to five seconds when the current item differs. It is not a one-second queue SLA.

## Configuration

In the existing `~/.config/spotify-player/app.toml`, change the existing polling setting rather than adding a duplicate:

```toml
playback_refresh_duration_in_ms = 1000
enable_streaming = "Always"
enable_audio_visualization = true
enable_media_control = true
```

Preserve the existing `[device]` section. All four settings above are top-level fields, so place them before `[device]`.

An existing explicit `playback_refresh_duration_in_ms = 0` disables periodic polling even with this patch. One-second polling is a cadence, not a guarantee of one-second end-to-end latency: Spotify propagation, network latency, rate limits and artwork downloads can take longer.

## Local build and checks (CachyOS/Arch)

From the patched repository root:

```bash
sudo pacman -S --needed rust base-devel pkgconf openssl alsa-lib dbus
features=rodio-backend,media-control,image,notify,fzf,daemon
cargo fmt --all -- --check
cargo check --locked --no-default-features --features "$features"
cargo test --locked --no-default-features --features "$features"
cargo clippy --locked --no-default-features --features "$features" -- -D warnings
cargo clippy --locked --no-default-features -- -D warnings
cargo build --locked --release --target-dir target --no-default-features --features "$features"
./target/release/spotify_player --version
```

This uses the project's default rodio audio backend, plus artwork and the CI features. `daemon` is included because streaming lifecycle code changed. Alternative audio backends need their corresponding native libraries; do not enable all audio backends indiscriminately.

Stop the running client before testing the new binary. Two integrated clients can interfere with device selection and socket routing. Launch `./target/release/spotify_player` directly before installing an override.

## Manual verification

Record the selected device and logs for each test. The log level can be raised with `RUST_LOG=spotify_player=debug` when launching the binary; use the application's configured log/cache location. Do not share access tokens.

| Test | Exact action | Pass condition |
| --- | --- | --- |
| A — cold local start | Close other laptop player instances. Pause/close phone playback. Start the patched binary; highlight a known playable song; press Enter. | Integrated device becomes active and the selected song is audible. Failure produces a useful logged error, not a silent no-op. |
| B — resume | While playing locally, pause using the normal binding; wait two seconds; resume. | Audio stops and the same track resumes near the paused position. |
| C — rapid toggles | Start playing; toggle six times; finish with explicit play through the CLI/media command. | Final audio/UI state is playing; earlier responses do not reverse it. Repeat starting paused. |
| D — phone updates | Select the phone, start Song A, then change to Song B on the phone. Repeat five times. | Laptop metadata/device/play state follow. Measure latency; expect roughly polling interval plus API latency, not a guaranteed maximum. |
| E — device transfer | While the phone plays, select the integrated laptop device from the device popup. Repeat transfer back and forth. | Correct device plays; no unexpected takeover by a delayed startup task. |
| F — volume | Use ten rapid increases, then ten decreases. Test at 0 and 100, using keyboard and mouse, on both devices. | Changes accumulate, never wrap, and settle on the actual device volume. Unsupported remote volume returns an error. |
| G — track identity | Next/previous repeatedly; include two different tracks with the same title if available. | Correct metadata and progress; old cover is not presented as the new track's cover. A new cover may be blank while loading. |
| H — failure/recovery | Temporarily disconnect the network, send pause/play, reconnect, then send an explicit control. | TUI remains usable, errors are logged, and playback/refresh recover. No uncontrolled repeated mutations. |
| I — visualization | Play locally with visualization enabled; pause; resume; then transfer to phone. | Bars receive local audio, stop while paused, and do not pretend to visualize phone audio. |
| J — lifecycle | Restart the integrated client during playback, then select a track. | No old event task continues to change the new session's state; inspect lifecycle logs. |

## Reversible local override

After the direct binary passes the manual tests, run from the repository root:

```bash
mkdir -p "$HOME/.local/bin" "$HOME/.local/state/spotify-player-patch"
stamp=$(date +%Y%m%d-%H%M%S)
pacman -Q spotify-player > "$HOME/.local/state/spotify-player-patch/package-$stamp.txt"
command -v spotify_player
type -a spotify_player
if [ -e "$HOME/.local/bin/spotify_player" ]; then
    cp -p "$HOME/.local/bin/spotify_player" "$HOME/.local/state/spotify-player-patch/spotify_player-$stamp"
fi
install -m755 target/release/spotify_player "$HOME/.local/bin/spotify_player"
export PATH="$HOME/.local/bin:$PATH"
hash -r
command -v spotify_player
type -a spotify_player
spotify_player --version
```

`command -v` must resolve to your home directory. Add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zshrc` if needed, then open a new terminal. The pacman binary in `/usr/bin` is preserved.

Rollback by moving the override aside and refreshing the shell command cache:

```bash
mv "$HOME/.local/bin/spotify_player" "$HOME/.local/bin/spotify_player.patched-disabled-$(date +%Y%m%d-%H%M%S)"
hash -r
command -v spotify_player
/usr/bin/spotify_player --version
```

If you had a previous local override, restore its timestamped backup instead.

## Automated verification results

See `validation-results.txt` in the delivered bundle for the actual commands and outcomes. Mock tests do not validate Spotify authentication, real device registration, PipeWire/ALSA routing, audible playback or visualization.
