# bsync

A bidirectional file sync daemon. Watches paired directories and syncs file changes both ways, driven by a TOML config with glob patterns.

Works on macOS (launchd) and Linux (systemd).

## Install

```
cargo install --path .
```

## Usage

### Add a sync pair

```
bsync add -a '~/projects/docs/**/*.md' -b '~/backup/docs/**/*.md'
```

The `--delete` flag enables deletion syncing:

```
bsync add -a '/path/a/**/*.txt' -b '/path/b/**/*.txt' --delete
```

To compress files on the way to the backup side and decompress them on the way back, declare a content transform:

```
bsync add -a '~/docs/**/*.md' -b '~/backup/docs/**/*.md.gz' \
    --a-to-b 'gzip -c' \
    --b-to-a 'gunzip -c'
```

### Remove a sync pair

```
bsync remove 2
```

The number corresponds to the pair numbering shown by `bsync validate`.

### Validate config

```
bsync validate
```

### Run in foreground

```
bsync run
```

Use `--log-to-file` to write logs to a rotating file instead of stderr (useful when running as a service):

```
bsync run --log-to-file
```

When `--log-to-file` is enabled, logs are written to daily-rotating files:

- **macOS:** `~/Library/Logs/bsync/`
- **Linux:** `~/.local/state/bsync/`

### Install as a system service

```
bsync install
```

This generates a launchd plist on macOS or a systemd unit on Linux.

### Reload config without restarting

```
bsync reload
```

### Uninstall the service

```
bsync uninstall
```

## Configuration

Default config path: `~/.config/bsync/config.toml`

```toml
[[pair]]
a = "~/projects/docs/**/*.md"
b = "~/backup/docs/**/*.md"
sync_deletions = false

[[pair]]
a = "/srv/app/config/**/*.yaml"
b = "/backup/config/**/*.yaml"
sync_deletions = true
allow_empty_sync = true

[[pair]]
a = "~/projects/docs/**/*.md"
b = "~/backup/docs/**/*.md.gz"
a_to_b = "gzip -c"
b_to_a = "gunzip -c"
```

Each pair defines two glob patterns. The static prefix (everything before the first glob metacharacter) is the watched root directory. The glob suffix filters which files get synced.

## How it works

A filesystem watcher (via [notify](https://docs.rs/notify)) is created for each side of every pair. File events are pushed into a channel, filtered by glob match, and then copied to the other side.

To prevent infinite sync loops, bsync skips a sync when source and destination already have matching mtimes (within 1-second slack). After every sync, the source's mtime is propagated to the destination, so steady-state mtime equality is the loop-breaking invariant: any echo event — from FSEvents racing our own write, from cloud-storage providers re-touching files, or from any other source — finds matching mtimes and short-circuits. A real user edit changes source mtime, breaking the equality and triggering a real sync.

Sending `SIGHUP` triggers a config reload without restarting the process.

## `.gitignore` support

When a sync pair uses glob patterns, bsync automatically respects `.gitignore` rules. Files that would be ignored by git are skipped during sync — both for initial sync and ongoing file watching.

- **Union semantics:** if a file is gitignored on *either* side of a pair, it is skipped in *both* directions. This prevents syncing build artifacts, caches, and other generated files regardless of which side defines the rule.
- **Plain path pairs** (no glob metacharacters) do not check `.gitignore`.
- Parent and nested `.gitignore` files are all respected.
- Rules are refreshed automatically (1-second cache) — no restart needed after `.gitignore` edits.

## Cloud storage compatibility

Cloud storage providers like iCloud can evict inactive files, replacing them with 0-byte placeholders on disk. Without protection, bsync would copy these empty placeholders over the good copies on the other side.

By default, bsync will **not** overwrite a non-empty file with a 0-byte source. This protection is on for all pairs. If a sync is skipped for this reason, a warning is logged.

To disable this protection for a specific pair (e.g., if you need to sync intentionally emptied files), set `allow_empty_sync = true`:

```toml
[[pair]]
a = "~/data/**/*.csv"
b = "~/backup/**/*.csv"
allow_empty_sync = true
```

## Pipelines

bsync supports two independent extensions per pair: **filename mapping** (the two glob patterns can differ in literal portions, e.g. extension or directory name) and **content transforms** (per-direction shell commands). They can be used together or separately.

### Filename mapping

By default the two patterns of a pair must have identical glob suffixes — file `notes/foo.md` on side A is mirrored to `notes/foo.md` on side B. bsync also accepts pairs whose suffixes share the same **wildcard shape** but differ in their literal portions.

Accepted:

- `~/docs/**/*.md` ↔ `~/backup/**/*.yamd` (extension rename)
- `~/docs/**/*.md` ↔ `~/backup/**/*.md.gz` (added extension)
- `~/parent/**/foo/*.md` ↔ `~/parent/**/bar/*.md.gz` (literal dirs after a wildcard can differ)

Top-level differing dirs like `~/foo/**/*.md` ↔ `~/bar/**/*.md.gz` also work, but the leading `foo`/`bar` are absorbed into each pair's watch root — bsync sees both suffixes as `**/*.md` ↔ `**/*.md.gz` (an extension-difference Template).

Rejected (config error):

- Patterns whose wildcards differ in count or kind (e.g. `*.md` vs `**/*.md`).
- Suffixes containing more than one `**`, a trailing `**`, or `**` embedded inside a path segment.
- Suffixes containing `?`, `[…]`, or `{…}` glob features.

### Content transforms

Set `a_to_b` and `b_to_a` (both required together) to per-direction shell commands. Each command is invoked once per file as `sh -c <cmd>`, with the source file connected to stdin and a temp file connected to stdout. The temp file is renamed over the destination after the command exits successfully.

```toml
[[pair]]
a = "~/docs/**/*.md"
b = "~/backup/**/*.md.gz"
a_to_b = "gzip -c"
b_to_a = "gunzip -c"
pipeline_timeout_secs = 60
```

Notes:

- The command runs once per synced file, with the daemon's environment and current working directory.
- A non-zero exit leaves the destination untouched (atomic temp-rename) and logs the captured stderr.
- Each command is killed with `SIGKILL` after `pipeline_timeout_secs` seconds (default `300`). The shell runs in its own process group, so any subprocesses it forked are killed too. Pipelines that intentionally detach (`setsid`, `nohup`, daemonization) escape this and are *not* killed.
- All sync events for all pairs run on a single thread today, so a pipeline that hangs near its timeout will block other pairs until the timeout fires. Tune `pipeline_timeout_secs` accordingly.
- Empty-output protection still applies: if the command produces 0 bytes and the destination is non-empty, the destination is preserved unless `allow_empty_sync = true`. A pipeline command that silently produces 0 bytes is logged as a warning.
- bsync propagates the source file's mtime to the destination after every sync (identity copy or pipeline). This keeps initial-sync's mtime comparison stable across restarts: without it, a non-deterministic pipeline like bare `gzip` (which embeds a timestamp in its output) would see the just-synced destination as "newer" on the next start and pipeline it back through `b_to_a`, ping-ponging forever.

## License

MIT
