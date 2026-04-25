use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ignore::WalkBuilder;
use walkdir::WalkDir;

use crate::config::ResolvedPair;
use crate::gitignore::GitignoreCache;

pub struct SyncGuard {
    recent: Mutex<HashMap<PathBuf, Instant>>,
    ttl: Duration,
}

impl SyncGuard {
    pub fn new(ttl: Duration) -> Self {
        Self {
            recent: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Resolve a path to its canonical form for consistent echo detection.
    /// Falls back to the original path if canonicalization fails (e.g., file doesn't exist yet).
    fn normalize(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    pub fn mark(&self, path: &Path) {
        let canonical = Self::normalize(path);
        self.recent
            .lock()
            .unwrap()
            .insert(canonical, Instant::now());
    }

    pub fn is_echo(&self, path: &Path) -> bool {
        let canonical = Self::normalize(path);
        let map = self.recent.lock().unwrap();
        if let Some(instant) = map.get(&canonical) {
            instant.elapsed() < self.ttl
        } else {
            false
        }
    }

    pub fn prune(&self) {
        let mut map = self.recent.lock().unwrap();
        map.retain(|_, instant| instant.elapsed() < self.ttl);
    }
}

/// Outcome of a successful sync_file call. Distinguishes a real copy/transform
/// from a no-op skip caused by empty-source/empty-output protection, so callers
/// can flag silently-misconfigured pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Synced,
    SkippedEmpty,
}

#[derive(Debug)]
pub enum SyncError {
    Io(std::io::Error),
    Command {
        cmd: String,
        exit: Option<i32>,
        stderr: String,
    },
    Timeout {
        cmd: String,
        after: Duration,
    },
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "Sync IO error: {e}"),
            SyncError::Command { cmd, exit, stderr } => {
                let exit_str = match exit {
                    Some(c) => format!("exit code {c}"),
                    None => "terminated by signal".to_string(),
                };
                if stderr.is_empty() {
                    write!(f, "Command '{cmd}' failed ({exit_str})")
                } else {
                    write!(f, "Command '{cmd}' failed ({exit_str}): {stderr}")
                }
            }
            SyncError::Timeout { cmd, after } => {
                write!(
                    f,
                    "Command '{cmd}' timed out after {}s and was killed",
                    after.as_secs()
                )
            }
        }
    }
}

impl std::error::Error for SyncError {}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Io(e)
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path_for(dest: &Path) -> PathBuf {
    let file_name = dest
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"));
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let temp_name = format!(".{}.bsync.{pid}.{n}.tmp", file_name.to_string_lossy());
    match dest.parent() {
        Some(parent) if parent != Path::new("") => parent.join(temp_name),
        _ => PathBuf::from(temp_name),
    }
}

pub fn sync_file(
    source: &Path,
    dest: &Path,
    guard: &SyncGuard,
    allow_empty_sync: bool,
    pipeline: Option<(&str, Duration)>,
) -> Result<SyncOutcome, SyncError> {
    let source_meta = fs::metadata(source)?;
    let source_mtime = source_meta.modified()?;

    if pipeline.is_none() && !allow_empty_sync {
        let source_size = source_meta.len();
        if source_size == 0
            && let Ok(dest_meta) = fs::metadata(dest)
            && dest_meta.len() > 0
        {
            tracing::warn!(
                "Skipping sync of empty file {} over non-empty {} (allow_empty_sync=false)",
                source.display(),
                dest.display()
            );
            return Ok(SyncOutcome::SkippedEmpty);
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp = temp_path_for(dest);

    // The pre-empty source check above is skipped when a pipeline command is
    // configured (a 0-byte input may legitimately produce a non-empty output —
    // e.g. gzip's header). The post-temp empty check below catches the
    // misconfigured-pipeline case after the command actually runs.
    let result = match pipeline {
        None => fs::copy(source, &temp).map(|_| ()).map_err(SyncError::Io),
        Some((cmd, timeout)) => crate::pipeline::run_command_to_temp(source, &temp, cmd, timeout),
    };
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }

    if !allow_empty_sync {
        let temp_size = fs::metadata(&temp).map(|m| m.len()).unwrap_or(0);
        if temp_size == 0
            && let Ok(dest_meta) = fs::metadata(dest)
            && dest_meta.len() > 0
        {
            let _ = fs::remove_file(&temp);
            tracing::warn!(
                "Skipping sync of empty file {} over non-empty {} (allow_empty_sync=false)",
                source.display(),
                dest.display()
            );
            return Ok(SyncOutcome::SkippedEmpty);
        }
    }

    if let Err(e) = fs::rename(&temp, dest) {
        let _ = fs::remove_file(&temp);
        return Err(SyncError::Io(e));
    }

    // Propagate source mtime to dest so initial_sync's mtime comparison
    // converges across daemon restarts. Without this, dest's mtime is "now"
    // after a sync, the next startup sees dest newer than source, and a
    // non-deterministic pipeline (bare `gzip` embeds a timestamp) ping-pongs
    // forever. Failures here are non-fatal: the data is already on disk.
    if let Err(e) = preserve_mtime(dest, source_mtime) {
        tracing::warn!("Failed to preserve mtime on {}: {e}", dest.display());
    }

    guard.mark(dest);
    tracing::info!("Synced {} -> {}", source.display(), dest.display());
    Ok(SyncOutcome::Synced)
}

fn preserve_mtime(path: &Path, mtime: SystemTime) -> std::io::Result<()> {
    let times = fs::FileTimes::new().set_modified(mtime);
    let f = fs::OpenOptions::new().write(true).open(path)?;
    f.set_times(times)
}

pub fn sync_delete(target: &Path, guard: &SyncGuard) -> Result<(), SyncError> {
    if target.exists() {
        guard.mark(target);
        fs::remove_file(target)?;
        tracing::info!("Deleted {}", target.display());
    }
    Ok(())
}

pub fn initial_sync(
    pair: &ResolvedPair,
    guard: &SyncGuard,
    gi_cache: Option<&Arc<Mutex<GitignoreCache>>>,
) -> Result<(), SyncError> {
    tracing::info!("Initial sync: {} <-> {}", pair.a_pattern, pair.b_pattern);

    let a_files = collect_matching_files(&pair.a_base, &pair.a_glob, gi_cache);
    let b_files = collect_matching_files(&pair.b_base, &pair.b_glob, gi_cache);

    // Re-key both sides by their B-side equivalent path so that A and B share keys.
    let mut a_by_b_key: HashMap<PathBuf, PathBuf> = HashMap::new();
    for (a_rel, a_path) in &a_files {
        match pair.filename_map.map_a_to_b(a_rel) {
            Some(b_rel) => {
                a_by_b_key.insert(b_rel, a_path.clone());
            }
            None => {
                tracing::warn!(
                    "Initial sync: skipping A-side {} (no forward mapping to B)",
                    a_rel.display()
                );
            }
        }
    }

    let mut all_keys: std::collections::HashSet<PathBuf> = a_by_b_key.keys().cloned().collect();
    all_keys.extend(b_files.keys().cloned());

    type Pipeline<'a> = Option<(&'a str, Duration)>;
    let (pipe_a_to_b, pipe_b_to_a): (Pipeline, Pipeline) = match &pair.content {
        crate::config::ContentTransform::Identity => (None, None),
        crate::config::ContentTransform::Command {
            a_to_b,
            b_to_a,
            timeout,
        } => (
            Some((a_to_b.as_str(), *timeout)),
            Some((b_to_a.as_str(), *timeout)),
        ),
    };

    // Per-file errors (sync_file failure, mtime read failure) are logged
    // and skipped so a single bad file doesn't abort the rest of the pair.
    let try_sync = |source: &Path, dest: &Path, pipeline: Option<(&str, Duration)>| match sync_file(
        source,
        dest,
        guard,
        pair.allow_empty_sync,
        pipeline,
    ) {
        Ok(SyncOutcome::Synced) => {}
        Ok(SyncOutcome::SkippedEmpty) => {
            if let Some((c, _)) = pipeline {
                tracing::warn!(
                    "Initial sync: pipeline command '{c}' produced empty output for {} -> {}; destination preserved",
                    source.display(),
                    dest.display()
                );
            }
        }
        Err(e) => {
            tracing::error!(
                "Initial sync failed {} -> {}: {e}",
                source.display(),
                dest.display()
            );
        }
    };

    let map_b_to_a = |b_rel: &Path| -> Option<PathBuf> {
        let mapped = pair.filename_map.map_b_to_a(b_rel);
        if mapped.is_none() {
            tracing::warn!(
                "Initial sync: skipping B-side {} (no inverse mapping to A)",
                b_rel.display()
            );
        }
        mapped
    };

    for b_rel in all_keys {
        let a_entry = a_by_b_key.get(&b_rel);
        let b_entry = b_files.get(&b_rel);

        match (a_entry, b_entry) {
            (Some(a_path), None) => {
                let b_dest = pair.b_base.join(&b_rel);
                try_sync(a_path, &b_dest, pipe_a_to_b);
            }
            (None, Some(b_path)) => {
                let Some(a_rel) = map_b_to_a(&b_rel) else {
                    continue;
                };
                let a_dest = pair.a_base.join(&a_rel);
                try_sync(b_path, &a_dest, pipe_b_to_a);
            }
            (Some(a_path), Some(b_path)) => {
                let a_mtime = match fs::metadata(a_path).and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            "Initial sync mtime read failed for {}: {e}",
                            a_path.display()
                        );
                        continue;
                    }
                };
                let b_mtime = match fs::metadata(b_path).and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            "Initial sync mtime read failed for {}: {e}",
                            b_path.display()
                        );
                        continue;
                    }
                };
                if a_mtime > b_mtime {
                    let b_dest = pair.b_base.join(&b_rel);
                    try_sync(a_path, &b_dest, pipe_a_to_b);
                } else if b_mtime > a_mtime {
                    let Some(a_rel) = map_b_to_a(&b_rel) else {
                        continue;
                    };
                    let a_dest = pair.a_base.join(&a_rel);
                    try_sync(b_path, &a_dest, pipe_b_to_a);
                }
            }
            (None, None) => unreachable!(),
        }
    }

    tracing::info!("Initial sync complete");
    Ok(())
}

fn collect_matching_files(
    base: &Path,
    glob: &globset::GlobMatcher,
    gi_cache: Option<&Arc<Mutex<GitignoreCache>>>,
) -> HashMap<PathBuf, PathBuf> {
    let mut files = HashMap::new();

    if let Some(cache) = gi_cache {
        for entry in WalkBuilder::new(base)
            .hidden(false)
            .build()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file()
                && glob.is_match(path)
                && let Ok(relative) = path.strip_prefix(base)
                && !cache.lock().unwrap().is_ignored(relative)
            {
                files.insert(relative.to_path_buf(), path.to_path_buf());
            }
        }
    } else {
        for entry in WalkDir::new(base).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file()
                && glob.is_match(entry.path())
                && let Ok(relative) = entry.path().strip_prefix(base)
            {
                files.insert(relative.to_path_buf(), entry.path().to_path_buf());
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedPair;
    use globset::Glob;
    use std::thread;
    use tempfile::TempDir;

    const TEST_PIPELINE_TIMEOUT: Duration = Duration::from_secs(30);

    fn make_pair(
        a_base: &Path,
        b_base: &Path,
        pattern: &str,
        sync_deletions: bool,
    ) -> ResolvedPair {
        let a_pattern = format!("{}/**/{}", a_base.display(), pattern);
        let b_pattern = format!("{}/**/{}", b_base.display(), pattern);
        ResolvedPair {
            a_base: a_base.to_path_buf(),
            b_base: b_base.to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: crate::filename_map::FilenameMap::Identity,
            content: crate::config::ContentTransform::Identity,
        }
    }

    #[test]
    fn test_sync_guard_mark_and_echo() {
        let guard = SyncGuard::new(Duration::from_secs(2));
        let path = Path::new("/tmp/test_file.txt");

        assert!(!guard.is_echo(path));
        guard.mark(path);
        assert!(guard.is_echo(path));
    }

    #[test]
    fn test_sync_guard_ttl_expiry() {
        let guard = SyncGuard::new(Duration::from_millis(50));
        let path = Path::new("/tmp/test_file.txt");

        guard.mark(path);
        assert!(guard.is_echo(path));

        thread::sleep(Duration::from_millis(100));
        assert!(!guard.is_echo(path));
    }

    #[test]
    fn test_sync_guard_prune() {
        let guard = SyncGuard::new(Duration::from_millis(50));
        guard.mark(Path::new("/tmp/a.txt"));
        guard.mark(Path::new("/tmp/b.txt"));

        thread::sleep(Duration::from_millis(100));
        guard.prune();

        let map = guard.recent.lock().unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_sync_file_copies_content() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "hello world").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello world");
        assert!(guard.is_echo(&dest));
    }

    #[test]
    fn test_sync_file_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("a/b/c/dest.txt");
        fs::write(&src, "nested").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "nested");
    }

    #[test]
    fn test_sync_file_nonexistent_source_errors() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("missing.txt");
        let dest = dir.path().join("dest.txt");

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = sync_file(&src, &dest, &guard, false, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_sync_delete_removes_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("to_delete.txt");
        fs::write(&file, "bye").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_delete(&file, &guard).unwrap();

        assert!(!file.exists());
    }

    #[test]
    fn test_sync_delete_nonexistent_is_ok() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("missing.txt");

        let guard = SyncGuard::new(Duration::from_secs(2));
        assert!(sync_delete(&file, &guard).is_ok());
    }

    #[test]
    fn test_collect_matching_files() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        fs::write(base.join("a.md"), "").unwrap();
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("sub/b.md"), "").unwrap();
        fs::write(base.join("c.txt"), "").unwrap();

        let pattern = format!("{}/**/*.md", base.display());
        let glob = Glob::new(&pattern).unwrap().compile_matcher();
        let files = collect_matching_files(base, &glob, None);

        assert_eq!(files.len(), 2);
        assert!(files.contains_key(&PathBuf::from("a.md")));
        assert!(files.contains_key(&PathBuf::from("sub/b.md")));
    }

    #[test]
    fn test_initial_sync_one_side() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        fs::write(dir_a.path().join("hello.md"), "from A").unwrap();

        let pair = make_pair(dir_a.path(), dir_b.path(), "*.md", false);
        let guard = SyncGuard::new(Duration::from_secs(2));
        initial_sync(&pair, &guard, None).unwrap();

        assert_eq!(
            fs::read_to_string(dir_b.path().join("hello.md")).unwrap(),
            "from A"
        );
    }

    #[test]
    fn test_initial_sync_bidirectional() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        fs::write(dir_a.path().join("a_only.md"), "A").unwrap();
        fs::write(dir_b.path().join("b_only.md"), "B").unwrap();

        let pair = make_pair(dir_a.path(), dir_b.path(), "*.md", false);
        let guard = SyncGuard::new(Duration::from_secs(2));
        initial_sync(&pair, &guard, None).unwrap();

        assert!(dir_b.path().join("a_only.md").exists());
        assert!(dir_a.path().join("b_only.md").exists());
    }

    #[test]
    fn test_initial_sync_newer_wins() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join("file.md"), "old from A").unwrap();
        // Small sleep to ensure different mtime
        thread::sleep(Duration::from_millis(50));
        fs::write(dir_b.path().join("file.md"), "new from B").unwrap();

        let pair = make_pair(dir_a.path(), dir_b.path(), "*.md", false);
        let guard = SyncGuard::new(Duration::from_secs(2));
        initial_sync(&pair, &guard, None).unwrap();

        // B is newer, so A should get B's content
        assert_eq!(
            fs::read_to_string(dir_a.path().join("file.md")).unwrap(),
            "new from B"
        );
    }

    // Callers (initial_sync, main loop) need to tell a real copy from a
    // protective skip — particularly so a misconfigured pipeline that silently
    // produces 0 bytes is observable, not indistinguishable from success.
    #[test]
    fn test_sync_file_outcome_synced_on_normal_copy() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "hello").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        let outcome = sync_file(&src, &dest, &guard, false, None).unwrap();
        assert!(matches!(outcome, SyncOutcome::Synced));
    }

    #[test]
    fn test_sync_file_outcome_skipped_when_source_empty_and_dest_nonempty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("empty.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "").unwrap();
        fs::write(&dest, "precious").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        let outcome = sync_file(&src, &dest, &guard, false, None).unwrap();
        assert!(matches!(outcome, SyncOutcome::SkippedEmpty));
    }

    #[test]
    fn test_sync_file_outcome_skipped_when_command_output_empty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "non-empty").unwrap();
        fs::write(&dest, "precious").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        let outcome = sync_file(
            &src,
            &dest,
            &guard,
            false,
            Some(("cat /dev/null", TEST_PIPELINE_TIMEOUT)),
        )
        .unwrap();
        assert!(matches!(outcome, SyncOutcome::SkippedEmpty));
    }

    #[test]
    fn test_sync_file_skips_empty_over_nonempty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("empty.txt");
        let dest = dir.path().join("has_content.txt");
        fs::write(&src, "").unwrap();
        fs::write(&dest, "important content").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "important content");
    }

    #[test]
    fn test_sync_file_allows_empty_over_nonempty_when_flag_set() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("empty.txt");
        let dest = dir.path().join("has_content.txt");
        fs::write(&src, "").unwrap();
        fs::write(&dest, "will be overwritten").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, true, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "");
    }

    #[test]
    fn test_sync_file_allows_empty_to_nonexistent_dest() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("empty.txt");
        let dest = dir.path().join("new_file.txt");
        fs::write(&src, "").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "");
    }

    #[test]
    fn test_sync_file_normal_copy_with_protection_on() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "new content").unwrap();
        fs::write(&dest, "old content").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "new content");
    }

    fn count_bsync_temps(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.starts_with('.') && s.contains("bsync") && s.ends_with(".tmp")
            })
            .count()
    }

    // Two concurrent sync_file calls targeting the same dest used to compute
    // the same temp filename, so the second File::create truncated the first
    // call's in-flight write. Temp paths must be unique per invocation.
    #[test]
    fn test_temp_path_for_dest_is_unique_per_call() {
        let dest = Path::new("/some/dir/notes.md");
        let t1 = temp_path_for(dest);
        let t2 = temp_path_for(dest);
        assert_ne!(t1, t2);
        assert_eq!(t1.parent(), Some(Path::new("/some/dir")));
        assert_eq!(t2.parent(), Some(Path::new("/some/dir")));
        let n1 = t1.file_name().unwrap().to_string_lossy().into_owned();
        assert!(n1.starts_with('.'), "temp must be hidden");
        assert!(n1.contains("bsync"), "temp must be identifiable");
        assert!(n1.ends_with(".tmp"), "temp must end with .tmp");
    }

    #[test]
    fn test_temp_path_for_dest_no_parent_is_unique_per_call() {
        let dest = Path::new("notes.md");
        let t1 = temp_path_for(dest);
        let t2 = temp_path_for(dest);
        assert_ne!(t1, t2);
        let n1 = t1.file_name().unwrap().to_string_lossy().into_owned();
        assert!(n1.starts_with('.'));
        assert!(n1.contains("bsync"));
    }

    #[test]
    fn test_sync_file_does_not_corrupt_dest_on_copy_failure() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "good content").unwrap();
        fs::write(&dest, "original content").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = sync_file(&src, &dest, &guard, false, None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "original content");
    }

    #[test]
    fn test_sync_file_cleans_up_temp_on_failure() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "content").unwrap();
        fs::write(&dest, "original").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let guard = SyncGuard::new(Duration::from_secs(2));
        let _ = sync_file(&src, &dest, &guard, false, None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert_eq!(
            count_bsync_temps(dir.path()),
            0,
            "temp files should be cleaned up"
        );
    }

    #[test]
    fn test_sync_file_post_copy_protects_dest_when_temp_is_empty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");

        fs::write(&src, "").unwrap();
        fs::write(&dest, "precious content").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "precious content");

        assert_eq!(count_bsync_temps(dir.path()), 0);
    }

    #[test]
    fn test_sync_file_with_command_writes_transformed_content() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "abc").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(
            &src,
            &dest,
            &guard,
            false,
            Some(("tr a-z A-Z", TEST_PIPELINE_TIMEOUT)),
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "ABC");
        assert!(guard.is_echo(&dest));
    }

    #[test]
    fn test_sync_file_with_command_preserves_dest_on_failure() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "input").unwrap();
        fs::write(&dest, "original").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = sync_file(
            &src,
            &dest,
            &guard,
            false,
            Some(("exit 1", TEST_PIPELINE_TIMEOUT)),
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "original");
        assert_eq!(
            count_bsync_temps(dir.path()),
            0,
            "temp files should be cleaned up"
        );
    }

    #[test]
    fn test_sync_file_with_command_post_empty_check() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "non-empty").unwrap();
        fs::write(&dest, "precious").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        // `cat /dev/null` produces 0 bytes regardless of input.
        sync_file(
            &src,
            &dest,
            &guard,
            false,
            Some(("cat /dev/null", TEST_PIPELINE_TIMEOUT)),
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "precious");
    }

    #[test]
    fn test_initial_sync_with_template_filename_map() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join("only_a.md"), "from A").unwrap();
        fs::write(dir_b.path().join("only_b.md.gz"), "from B").unwrap();

        let a_pattern = format!("{}/**/*.md", dir_a.path().display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: false,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Command {
                a_to_b: "cat".into(),
                b_to_a: "cat".into(),
                timeout: TEST_PIPELINE_TIMEOUT,
            },
        };

        let guard = SyncGuard::new(Duration::from_secs(2));
        initial_sync(&pair, &guard, None).unwrap();

        // A's only_a.md propagated to B as only_a.md.gz (cat is identity).
        assert_eq!(
            fs::read_to_string(dir_b.path().join("only_a.md.gz")).unwrap(),
            "from A"
        );
        // B's only_b.md.gz propagated to A as only_b.md.
        assert_eq!(
            fs::read_to_string(dir_a.path().join("only_b.md")).unwrap(),
            "from B"
        );
    }

    // The B-side glob can match files that the (stricter) FilenameMap template
    // cannot reverse — for example, after a config change or a moved file.
    // initial_sync used to panic via .expect("B path tokenized into A template")
    // in that case; it must instead log and continue.
    #[test]
    fn test_initial_sync_skips_unmappable_b_file_without_panic() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        // B has a file that the permissive glob will pick up.
        fs::write(dir_b.path().join("orphan.txt"), "txt content").unwrap();

        // Permissive B glob: matches anything. Restrictive template: expects .md.gz.
        let a_pattern = format!("{}/**/*.md", dir_a.path().display());
        let b_pattern = format!("{}/**/*", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: false,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Identity,
        };

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = initial_sync(&pair, &guard, None);

        assert!(
            result.is_ok(),
            "initial_sync must not panic on unmappable B file"
        );
        // The unmappable B file was skipped — no A-side companion was created.
        assert!(!dir_a.path().join("orphan.txt").exists());
        assert!(!dir_a.path().join("orphan").exists());
    }

    // Symmetric to test_initial_sync_skips_unmappable_b_file_without_panic:
    // the A-side glob can also match files the (stricter) FilenameMap template
    // cannot forward-map. initial_sync must log and skip rather than silently
    // drop them — same observability contract as the B-side path.
    #[test]
    fn test_initial_sync_skips_unmappable_a_file_without_panic() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        // A has a file the permissive glob picks up but the stricter template
        // can't forward-map (regex expects a `.md` suffix).
        fs::write(dir_a.path().join("orphan.txt"), "txt content").unwrap();

        let a_pattern = format!("{}/**/*", dir_a.path().display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: false,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Identity,
        };

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = initial_sync(&pair, &guard, None);

        assert!(
            result.is_ok(),
            "initial_sync must not panic on unmappable A file"
        );
        // The unmappable A file was skipped — no B-side companion was created.
        assert!(!dir_b.path().join("orphan.txt").exists());
        assert!(!dir_b.path().join("orphan.md.gz").exists());
    }

    // Mirrors what run_sync_loop does for SyncEventKind::Delete under a
    // template FilenameMap: side-A delete event for `notes.md` resolves via
    // a_to_b_path to `<b_base>/notes.md.gz` and sync_delete removes it.
    // Covers the top-level and nested-directory cases.
    #[test]
    fn test_template_delete_a_to_b_removes_mapped_file() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_b.path().join("notes.md.gz"), "stale").unwrap();
        fs::create_dir_all(dir_b.path().join("sub")).unwrap();
        fs::write(dir_b.path().join("sub/x.md.gz"), "stale").unwrap();

        let a_pattern = format!("{}/**/*.md", dir_a.path().display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: true,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Identity,
        };

        let top = pair.a_to_b_path(Path::new("notes.md")).unwrap();
        let nested = pair.a_to_b_path(Path::new("sub/x.md")).unwrap();
        assert_eq!(top, dir_b.path().join("notes.md.gz"));
        assert_eq!(nested, dir_b.path().join("sub/x.md.gz"));

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_delete(&top, &guard).unwrap();
        sync_delete(&nested, &guard).unwrap();

        assert!(!dir_b.path().join("notes.md.gz").exists());
        assert!(!dir_b.path().join("sub/x.md.gz").exists());
    }

    // The reverse direction: side-B delete event for `notes.md.gz` resolves
    // via b_to_a_path to `<a_base>/notes.md` and sync_delete removes it.
    #[test]
    fn test_template_delete_b_to_a_removes_mapped_file() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join("notes.md"), "stale").unwrap();
        fs::create_dir_all(dir_a.path().join("sub")).unwrap();
        fs::write(dir_a.path().join("sub/x.md"), "stale").unwrap();

        let a_pattern = format!("{}/**/*.md", dir_a.path().display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: true,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Identity,
        };

        let top = pair.b_to_a_path(Path::new("notes.md.gz")).unwrap();
        let nested = pair.b_to_a_path(Path::new("sub/x.md.gz")).unwrap();
        assert_eq!(top, dir_a.path().join("notes.md"));
        assert_eq!(nested, dir_a.path().join("sub/x.md"));

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_delete(&top, &guard).unwrap();
        sync_delete(&nested, &guard).unwrap();

        assert!(!dir_a.path().join("notes.md").exists());
        assert!(!dir_a.path().join("sub/x.md").exists());
    }

    #[test]
    fn test_initial_sync_continues_on_per_file_command_error() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join("first.md"), "one").unwrap();
        fs::write(dir_a.path().join("second.md"), "two").unwrap();

        let a_pattern = format!("{}/**/*.md", dir_a.path().display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: false,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Command {
                a_to_b: "exit 1".into(),
                b_to_a: "exit 1".into(),
                timeout: TEST_PIPELINE_TIMEOUT,
            },
        };

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = initial_sync(&pair, &guard, None);

        // Per-file failures must not propagate; initial_sync logs and continues.
        assert!(
            result.is_ok(),
            "initial_sync must not abort on per-file errors"
        );
        // Both files were attempted and both failed cleanly (no partial dest).
        assert!(!dir_b.path().join("first.md.gz").exists());
        assert!(!dir_b.path().join("second.md.gz").exists());
    }

    fn force_set_mtime(path: &Path, mtime: SystemTime) {
        let times = fs::FileTimes::new().set_modified(mtime);
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(times).unwrap();
    }

    fn assert_mtimes_match(a: SystemTime, b: SystemTime, label: &str) {
        let diff = if a > b {
            a.duration_since(b).unwrap()
        } else {
            b.duration_since(a).unwrap()
        };
        // Allow 1s slack for filesystems with second-resolution timestamps
        // (e.g. HFS+); APFS/ext4 round-trip exactly.
        assert!(
            diff < Duration::from_secs(1),
            "{label}: mtimes differ by {diff:?} (a={a:?}, b={b:?})"
        );
    }

    // sync_file must propagate the source mtime to the destination so
    // initial_sync's mtime-based reconciliation reaches a steady state on
    // restart. Without this, B's mtime is "now" after a sync, the next
    // startup sees B newer than A, and a non-deterministic pipeline (e.g.
    // bare `gzip`, which embeds a timestamp) ping-pongs forever.
    #[test]
    fn test_sync_file_preserves_source_mtime_identity() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "hello").unwrap();

        // Pin source mtime far in the past so any "set to now" behavior is
        // unmistakably wrong, and any precision rounding stays well under 1s.
        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        force_set_mtime(&src, target);

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false, None).unwrap();

        let dest_mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        assert_mtimes_match(dest_mtime, target, "identity copy mtime");
    }

    #[test]
    fn test_sync_file_preserves_source_mtime_with_pipeline() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "hello").unwrap();

        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        force_set_mtime(&src, target);

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(
            &src,
            &dest,
            &guard,
            false,
            Some(("cat", TEST_PIPELINE_TIMEOUT)),
        )
        .unwrap();

        let dest_mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        assert_mtimes_match(dest_mtime, target, "pipeline output mtime");
    }

    // The motivating case: a pipeline that produces different bytes on every
    // invocation (bare `gzip` embeds a timestamp; here we simulate it by
    // appending the shell's pid). With mtime preservation, two consecutive
    // initial_sync passes must converge — the second pass sees equal mtimes
    // on both sides, decides neither is newer, and is a no-op. Without
    // preservation, the second pass would see B newer (mtime was "now" after
    // the first run), pipeline B->A with different bytes, then A->B forever.
    #[test]
    fn test_initial_sync_idempotent_with_nondeterministic_pipeline() {
        use crate::config::ContentTransform;
        use crate::filename_map::FilenameMap;
        use globset::Glob;

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join("foo.md"), "hello").unwrap();

        let a_pattern = format!("{}/**/*.md", dir_a.path().display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.path().display());
        let a_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md");
        let b_tokens = crate::filename_map::test_helpers::tokenize_for_test("**/*.md.gz");
        let pair = ResolvedPair {
            a_base: dir_a.path().to_path_buf(),
            b_base: dir_b.path().to_path_buf(),
            a_glob: Glob::new(&a_pattern).unwrap().compile_matcher(),
            b_glob: Glob::new(&b_pattern).unwrap().compile_matcher(),
            a_pattern,
            b_pattern,
            sync_deletions: false,
            allow_empty_sync: false,
            has_glob: true,
            filename_map: FilenameMap::build(&a_tokens, &b_tokens),
            content: ContentTransform::Command {
                // Non-deterministic: appends the shell's pid, different per run.
                a_to_b: "cat; echo $$".into(),
                b_to_a: "cat; echo $$".into(),
                timeout: TEST_PIPELINE_TIMEOUT,
            },
        };

        let guard = SyncGuard::new(Duration::from_secs(2));

        initial_sync(&pair, &guard, None).unwrap();
        let dest = dir_b.path().join("foo.md.gz");
        let bytes_after_first = fs::read(&dest).unwrap();
        assert!(
            !bytes_after_first.is_empty(),
            "first initial_sync should write B"
        );

        initial_sync(&pair, &guard, None).unwrap();
        let bytes_after_second = fs::read(&dest).unwrap();
        assert_eq!(
            bytes_after_first, bytes_after_second,
            "second initial_sync must be a no-op when mtimes are preserved"
        );
        // A must be untouched too — no inverse pipeline ran.
        assert_eq!(fs::read(dir_a.path().join("foo.md")).unwrap(), b"hello");
    }
}
