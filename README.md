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
```

Each pair defines two glob patterns. The static prefix (everything before the first glob metacharacter) is the watched root directory. The glob suffix filters which files get synced.

## How it works

A filesystem watcher (via [notify](https://docs.rs/notify)) is created for each side of every pair. File events are pushed into a channel, filtered by glob match, and then copied to the other side.

To prevent infinite sync loops, a `SyncGuard` marks destination paths before writing. When the watcher fires for our own write, the event is recognized as an echo and skipped (2-second TTL).

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

## Development

```
cargo build       # build
cargo test        # run tests
cargo clippy      # lint
cargo fmt         # format
```

## License

MIT
