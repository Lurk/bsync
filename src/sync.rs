use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

#[derive(Debug)]
pub enum SyncError {
    Io(std::io::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "Sync IO error: {e}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Io(e)
    }
}

fn temp_path_for(dest: &Path) -> PathBuf {
    let file_name = dest
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"));
    let temp_name = format!(".{}.bsync.tmp", file_name.to_string_lossy());
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
) -> Result<(), SyncError> {
    if !allow_empty_sync {
        let source_size = fs::metadata(source)?.len();
        if source_size == 0
            && let Ok(dest_meta) = fs::metadata(dest)
            && dest_meta.len() > 0
        {
            tracing::warn!(
                "Skipping sync of empty file {} over non-empty {} (allow_empty_sync=false)",
                source.display(),
                dest.display()
            );
            return Ok(());
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp = temp_path_for(dest);

    let copy_result = fs::copy(source, &temp);
    if let Err(e) = copy_result {
        let _ = fs::remove_file(&temp);
        return Err(SyncError::Io(e));
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
            return Ok(());
        }
    }

    if let Err(e) = fs::rename(&temp, dest) {
        let _ = fs::remove_file(&temp);
        return Err(SyncError::Io(e));
    }

    guard.mark(dest);
    tracing::info!("Synced {} -> {}", source.display(), dest.display());
    Ok(())
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

    let mut all_keys: std::collections::HashSet<&PathBuf> = a_files.keys().collect();
    all_keys.extend(b_files.keys());

    for relative in all_keys {
        let a_entry = a_files.get(relative);
        let b_entry = b_files.get(relative);

        match (a_entry, b_entry) {
            (Some(a_path), None) => {
                let b_dest = pair.b_base.join(relative);
                sync_file(a_path, &b_dest, guard, pair.allow_empty_sync)?;
            }
            (None, Some(b_path)) => {
                let a_dest = pair.a_base.join(relative);
                sync_file(b_path, &a_dest, guard, pair.allow_empty_sync)?;
            }
            (Some(a_path), Some(b_path)) => {
                let a_mtime = fs::metadata(a_path)?.modified()?;
                let b_mtime = fs::metadata(b_path)?.modified()?;
                if a_mtime > b_mtime {
                    let b_dest = pair.b_base.join(relative);
                    sync_file(a_path, &b_dest, guard, pair.allow_empty_sync)?;
                } else if b_mtime > a_mtime {
                    let a_dest = pair.a_base.join(relative);
                    sync_file(b_path, &a_dest, guard, pair.allow_empty_sync)?;
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
        sync_file(&src, &dest, &guard, false).unwrap();

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
        sync_file(&src, &dest, &guard, false).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "nested");
    }

    #[test]
    fn test_sync_file_nonexistent_source_errors() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("missing.txt");
        let dest = dir.path().join("dest.txt");

        let guard = SyncGuard::new(Duration::from_secs(2));
        let result = sync_file(&src, &dest, &guard, false);
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

    #[test]
    fn test_sync_file_skips_empty_over_nonempty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("empty.txt");
        let dest = dir.path().join("has_content.txt");
        fs::write(&src, "").unwrap();
        fs::write(&dest, "important content").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false).unwrap();

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
        sync_file(&src, &dest, &guard, true).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "");
    }

    #[test]
    fn test_sync_file_allows_empty_to_nonexistent_dest() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("empty.txt");
        let dest = dir.path().join("new_file.txt");
        fs::write(&src, "").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false).unwrap();

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
        sync_file(&src, &dest, &guard, false).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "new content");
    }

    #[test]
    fn test_temp_path_for_dest() {
        let dest = Path::new("/some/dir/notes.md");
        let temp = temp_path_for(dest);
        assert_eq!(temp, PathBuf::from("/some/dir/.notes.md.bsync.tmp"));
    }

    #[test]
    fn test_temp_path_for_dest_no_parent() {
        let dest = Path::new("notes.md");
        let temp = temp_path_for(dest);
        assert_eq!(temp, PathBuf::from(".notes.md.bsync.tmp"));
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
        let result = sync_file(&src, &dest, &guard, false);

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
        let _ = sync_file(&src, &dest, &guard, false);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let temp = temp_path_for(&dest);
        assert!(!temp.exists(), "temp file should be cleaned up");
    }

    #[test]
    fn test_sync_file_post_copy_protects_dest_when_temp_is_empty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");

        fs::write(&src, "").unwrap();
        fs::write(&dest, "precious content").unwrap();

        let guard = SyncGuard::new(Duration::from_secs(2));
        sync_file(&src, &dest, &guard, false).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "precious content");

        let temp = temp_path_for(&dest);
        assert!(!temp.exists());
    }
}
