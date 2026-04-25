use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;

pub struct GitignoreCache {
    a_base: PathBuf,
    b_base: PathBuf,
    a_matcher: Gitignore,
    b_matcher: Gitignore,
    built_at: Instant,
    ttl: Duration,
}

impl GitignoreCache {
    pub fn new(a_base: PathBuf, b_base: PathBuf) -> Self {
        let a_matcher = build_gitignore(&a_base);
        let b_matcher = build_gitignore(&b_base);
        Self {
            a_base,
            b_base,
            a_matcher,
            b_matcher,
            built_at: Instant::now(),
            ttl: Duration::from_secs(1),
        }
    }

    pub fn is_ignored(&mut self, relative: &Path) -> bool {
        if self.built_at.elapsed() > self.ttl {
            self.rebuild();
        }
        self.a_matcher.matched_path_or_any_parents(relative, false).is_ignore()
            || self.b_matcher.matched_path_or_any_parents(relative, false).is_ignore()
    }

    fn rebuild(&mut self) {
        self.a_matcher = build_gitignore(&self.a_base);
        self.b_matcher = build_gitignore(&self.b_base);
        self.built_at = Instant::now();
    }
}

fn build_gitignore(base: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(base);

    let mut ancestor = base.parent();
    while let Some(dir) = ancestor {
        let gi = dir.join(".gitignore");
        if gi.is_file() {
            builder.add(gi);
        }
        ancestor = dir.parent();
    }

    for entry in WalkDir::new(base).into_iter().filter_map(|e| e.ok()) {
        if entry.file_name() == ".gitignore" {
            builder.add(entry.path());
        }
    }

    builder.build().unwrap_or_else(|_| {
        // If building fails, return an empty matcher that ignores nothing
        GitignoreBuilder::new(base).build().unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_ignored_on_a_side_blocks_both() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join(".gitignore"), "*.log\n").unwrap();
        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(cache.is_ignored(Path::new("debug.log")));
        assert!(!cache.is_ignored(Path::new("readme.md")));
    }

    #[test]
    fn test_ignored_on_b_side_blocks_both() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_b.path().join(".gitignore"), "*.bin\n").unwrap();

        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(cache.is_ignored(Path::new("output.bin")));
        assert!(!cache.is_ignored(Path::new("src/main.rs")));
    }

    #[test]
    fn test_neither_side_ignores() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(!cache.is_ignored(Path::new("anything.txt")));
    }

    #[test]
    fn test_nested_gitignore() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join(".gitignore"), "*.log\n").unwrap();
        let sub = dir_a.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".gitignore"), "*.tmp\n").unwrap();

        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(cache.is_ignored(Path::new("sub/data.tmp")));
        assert!(cache.is_ignored(Path::new("app.log")));
        assert!(!cache.is_ignored(Path::new("readme.md")));
    }

    #[test]
    fn test_ttl_triggers_rebuild() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());
        // Override TTL to 0 so it always rebuilds
        cache.ttl = Duration::from_millis(0);

        assert!(!cache.is_ignored(Path::new("test.log")));

        fs::write(dir_a.path().join(".gitignore"), "*.log\n").unwrap();

        // Next call should rebuild and pick up the new rule
        std::thread::sleep(Duration::from_millis(1));
        assert!(cache.is_ignored(Path::new("test.log")));
    }

    #[test]
    fn test_directory_prefix_rule_matches_files_inside() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join(".gitignore"), "/target\n").unwrap();
        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(cache.is_ignored(Path::new("target/foo.md")));
        assert!(cache.is_ignored(Path::new("target/sub/bar.md")));
        assert!(!cache.is_ignored(Path::new("notes/foo.md")));
    }

    #[test]
    fn test_bare_name_rule_matches_nested_files() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_a.path().join(".gitignore"), "node_modules\n").unwrap();
        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(cache.is_ignored(Path::new("frontend/node_modules/x/readme.md")));
        assert!(cache.is_ignored(Path::new("node_modules/x/readme.md")));
        assert!(!cache.is_ignored(Path::new("src/main.rs")));
    }

    #[test]
    fn test_b_side_directory_prefix_blocks_both() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        fs::write(dir_b.path().join(".gitignore"), "/target\n").unwrap();
        let mut cache = GitignoreCache::new(dir_a.path().to_path_buf(), dir_b.path().to_path_buf());

        assert!(cache.is_ignored(Path::new("target/foo.md")));
    }
}
