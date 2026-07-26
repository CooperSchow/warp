# Eqho fork — performance notes and pitfalls

Why this file exists: the fork's custom features made WarpOss noticeably slower
than stock Warp — long startup, beachballs, an unusable Settings page — while
stock Warp on the same machine was always smooth. Every cause was something
*we* introduced, and each was avoidable. Read this before adding a feature that
touches the filesystem, the keychain, or the settings UI.

---

## Pitfall 1 — We *ship* dev builds; upstream only debugs with them

`script/dev-install.sh` installs `target/debug/warp-oss` into
`~/Applications/WarpOss.app`. That means the binary you use all day is built
with the `dev` profile, whereas stock Warp is a release build. At the upstream
default (`opt-level = 0` for everything except a few packages), the UI crates
are slow enough to beachball on ordinary interactions.

**Mitigation, already applied** — `Cargo.toml`, `[profile.dev.package]`:

```toml
warp.opt-level = 1
warpui_core.opt-level = 1
warp_core.opt-level = 1
warp_editor.opt-level = 1
ui_components.opt-level = 1
```

`opt-level = 1` removes the pathological slowness at a modest compile-time
cost. Going higher mostly buys compile time, not responsiveness.

**Rule of thumb:** if you ever see "it's slow but only in our build", check the
profile before suspecting your feature. And if a build ever needs to be
genuinely fast (profiling, a demo), build `--profile rlto` instead and install
that binary — don't chase micro-optimizations in an unoptimized build.

---

## Pitfall 2 — Command-palette `SyncDataSource`s run on the main thread

This one cost us months of intermittent beachballs.

`SearchMixer::run_query_internal` (`crates/warp_search_core/src/mixer.rs`)
calls `SyncDataSource::run_query` **inline**, on the main thread. Only
`AsyncDataSource` gets `ctx.spawn`. Our "Claude Conversations" provider was a
sync source that, whenever its 2-second cache expired, walked
`~/.claude/projects` and **fully JSON-parsed every transcript** — 234 files /
313 MB / one 92 MB file on a real machine. Opening the palette froze the app.

**Rules for any sync data source:**

- Work must be bounded by something *you* control, never by "however much data
  the user happens to have". A per-item cost that grows with usage is a
  time bomb.
- Never read a whole file when a slice will do. Transcripts, logs, and
  histories grow without limit.
- Cache by content identity (mtime/size), not just by elapsed time. A TTL alone
  still pays full cost every TTL; an mtime check makes the steady state free.
- If the work genuinely can't be bounded, use an `AsyncDataSource` (or
  `ctx.spawn` the load and serve the previous snapshot meanwhile).

**How it was fixed** (`app/src/terminal/cli_agent_sessions/history/mod.rs`,
`app/src/search/command_palette/claude_conversations/data_source.rs`):

- `read_session_summary` reads a 64 KB head (cwd, first prompt, start time) and
  a 32 KB tail (`ai-title`, latest timestamp) — never the middle. Worst case
  per file: 96 KB instead of unbounded.
- The palette caches summaries per path and reuses any whose mtime is unchanged,
  so a refresh is a directory walk plus one `stat` per file.
- Consequence to remember: `message_count` and `models` on `ClaudeSession` now
  reflect only the scanned slices. Don't treat them as exact totals — the
  "is this a real session" filter uses `first_prompt.is_some()` instead.

---

## Pitfall 3 — A nearly-full disk looks exactly like an app bug

`target/debug/incremental` reached **72 GB** and pushed the volume to 99% full.
macOS beachballs system-wide at that point, which is easy to misread as "the
app I just changed is broken".

**Check first, before profiling anything:**

```bash
df -h /System/Volumes/Data          # want well clear of 95% used
du -sh ~/Desktop/Eqho_Code_Projects/warp/target
rm -rf ~/Desktop/Eqho_Code_Projects/warp/target/debug/incremental   # safe; regenerates
```

A full `target/` for this repo runs 50–110 GB. Clean the incremental cache
periodically; it costs one slower build and nothing else.

---

## Pitfall 4 — Blocking calls that *look* cheap

- **Keychain reads block.** `SecKeychainFindGenericPassword` can hang for
  minutes if an ACL prompt can't be displayed (seen while running an isolated
  test instance). Anything reading secure storage on the main thread can freeze
  the app. `app/src/settings/local_control.rs` reads `LocalControlMode` from the
  keychain during settings init — a known-safe path only because it normally
  returns instantly.
- **Subprocesses.** The usage pill shells out to `/usr/bin/security` and
  `/usr/bin/curl` every 60s, but does so via `command::r#async::Command` inside
  `ctx.spawn`, i.e. on the background executor. Keep it that way; a synchronous
  `std::process::Command` there would be a periodic freeze.
- **Session restore is inherently heavy.** Restoring N Claude tabs launches N
  `claude --resume` processes at once. That's the feature working as designed,
  but it makes cold start expensive — don't add more startup work on top of it.

---

## Diagnosing a stall quickly

The fastest ground truth is a sample of the running process — no rebuild, no
guessing:

```bash
PID=$(pgrep -f "Applications/WarpOss.app/Contents/MacOS/warp-oss" | head -1)
sample "$PID" 5 -file /tmp/warposs-sample.txt
awk '/Thread_.*main-thread/,/^\s*[0-9]+ Thread/' /tmp/warposs-sample.txt | tail -40
```

Read the **main thread** stack: anything of ours doing file or keychain work
there is the bug. Grep the sample for your feature's symbols to confirm or
exonerate it (`grep -c claude_auto_color /tmp/warposs-sample.txt`).

For a stall you can reproduce on demand, run an isolated instance against a
throwaway `HOME` so you can experiment without touching the real app or its
session state:

```bash
env -u XDG_RUNTIME_DIR HOME=/tmp/warposs-test PATH=/usr/bin:/bin \
  ~/Desktop/Eqho_Code_Projects/warp/target/debug/warp-oss "warposs://settings/appearance"
```

Note the URL scheme is `warposs://` (not `warp://`) and the config directory is
`~/.warp-oss` (not `~/.warp`); `WARP_DATA_PROFILE=<name>` isolates a debug
instance's data further.

---

## Pre-deploy checklist

1. `df -h /System/Volumes/Data` — clear of 95%? If not, drop
   `target/debug/incremental`.
2. Building the binary you intend to ship (`[profile.dev.package]` opt-levels
   still in place)?
3. Any new filesystem/keychain/subprocess work off the main thread, or bounded?
4. `cargo test -p warp --lib --features gui` for the touched areas.
5. Quit WarpOss, then `~/Desktop/Eqho_Code_Projects/reinstall-warposs.sh`
   (`dev-install.sh` refuses to run while the app is open, and swapping the
   binary under a live process crashes it).
