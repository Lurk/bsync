use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::gitignore::GitignoreCache;

#[derive(Deserialize, Serialize)]
pub struct Config {
    pub pair: Vec<PairConfig>,
}

#[derive(Deserialize, Serialize)]
pub struct PairConfig {
    pub a: String,
    pub b: String,
    #[serde(default)]
    pub sync_deletions: bool,
}

pub struct ResolvedPair {
    pub a_base: PathBuf,
    pub b_base: PathBuf,
    pub a_glob: GlobMatcher,
    pub b_glob: GlobMatcher,
    pub a_pattern: String,
    pub b_pattern: String,
    pub sync_deletions: bool,
    pub has_glob: bool,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    InvalidGlob(String, globset::Error),
    MismatchedGlobSuffix { a: String, b: String },
    WatchRootNotFound(PathBuf),
    NoPairs,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "IO error: {e}"),
            ConfigError::Parse(e) => write!(f, "TOML parse error: {e}"),
            ConfigError::InvalidGlob(pat, e) => write!(f, "Invalid glob '{pat}': {e}"),
            ConfigError::MismatchedGlobSuffix { a, b } => {
                write!(f, "Glob suffixes must match: '{a}' vs '{b}'")
            }
            ConfigError::WatchRootNotFound(p) => {
                write!(f, "Watch root directory not found: {}", p.display())
            }
            ConfigError::NoPairs => write!(f, "Config must contain at least one [[pair]]"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Extract the static directory prefix from a glob pattern.
/// Walks path components until hitting the first glob metacharacter.
pub fn extract_watch_root(pattern: &str) -> PathBuf {
    let path = Path::new(pattern);
    let mut root = PathBuf::new();
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{') {
            break;
        }
        root.push(component);
    }
    if root.as_os_str().is_empty() {
        root.push(".");
    }
    root
}

/// Extract the glob suffix (the part after the static root).
fn extract_glob_suffix(pattern: &str) -> String {
    let path = Path::new(pattern);
    let mut found_glob = false;
    let mut suffix_parts = Vec::new();
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if !found_glob {
            if s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{') {
                found_glob = true;
                suffix_parts.push(s.into_owned());
            }
        } else {
            suffix_parts.push(s.into_owned());
        }
    }
    suffix_parts.join("/")
}

/// Expand leading `~` or `~/` to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{}{}", home, &path[1..]);
    }
    path.to_string()
}

pub fn load(path: &Path) -> Result<Vec<ResolvedPair>, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    let config: Config = toml::from_str(&content).map_err(ConfigError::Parse)?;

    if config.pair.is_empty() {
        return Err(ConfigError::NoPairs);
    }

    resolve_pairs(&config.pair)
}

fn resolve_pairs(pairs: &[PairConfig]) -> Result<Vec<ResolvedPair>, ConfigError> {
    let mut resolved = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let a_expanded = expand_tilde(&pair.a);
        let b_expanded = expand_tilde(&pair.b);

        let a_suffix = extract_glob_suffix(&a_expanded);
        let b_suffix = extract_glob_suffix(&b_expanded);
        if a_suffix != b_suffix {
            return Err(ConfigError::MismatchedGlobSuffix {
                a: pair.a.clone(),
                b: pair.b.clone(),
            });
        }

        let a_base = extract_watch_root(&a_expanded);
        let b_base = extract_watch_root(&b_expanded);

        if !a_base.is_dir() {
            return Err(ConfigError::WatchRootNotFound(a_base));
        }
        if !b_base.is_dir() {
            return Err(ConfigError::WatchRootNotFound(b_base));
        }

        let a_glob = Glob::new(&a_expanded)
            .map_err(|e| ConfigError::InvalidGlob(a_expanded.clone(), e))?
            .compile_matcher();
        let b_glob = Glob::new(&b_expanded)
            .map_err(|e| ConfigError::InvalidGlob(b_expanded.clone(), e))?
            .compile_matcher();

        let has_glob = !a_suffix.is_empty();

        resolved.push(ResolvedPair {
            a_base,
            b_base,
            a_glob,
            b_glob,
            a_pattern: a_expanded,
            b_pattern: b_expanded,
            sync_deletions: pair.sync_deletions,
            has_glob,
        });
    }

    Ok(resolved)
}

/// Print validation summary for a config file.
pub fn validate_and_print(path: &Path) -> Result<(), ConfigError> {
    let pairs = load(path)?;

    for (i, pair) in pairs.iter().enumerate() {
        println!("Pair #{}:", i + 1);

        println!("  A: {}", pair.a_pattern);
        println!("    Watch root: {}", pair.a_base.display());
        println!("    Exists: {}", pair.a_base.is_dir());

        println!("  B: {}", pair.b_pattern);
        println!("    Watch root: {}", pair.b_base.display());
        println!("    Exists: {}", pair.b_base.is_dir());

        println!("  sync_deletions: {}", pair.sync_deletions);
        println!(
            "  gitignore filtering: {}",
            if pair.has_glob { "enabled" } else { "disabled" }
        );

        let mut gi_cache = if pair.has_glob {
            Some(GitignoreCache::new(
                pair.a_base.clone(),
                pair.b_base.clone(),
            ))
        } else {
            None
        };

        if pair.a_base.is_dir() {
            let count = count_matching_files(&pair.a_base, &pair.a_glob, gi_cache.as_mut());
            println!("  Files matching A: {count}");
        }

        if pair.b_base.is_dir() {
            let count = count_matching_files(&pair.b_base, &pair.b_glob, gi_cache.as_mut());
            println!("  Files matching B: {count}");
        }

        println!();
    }

    Ok(())
}

pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME environment variable not set");
    PathBuf::from(home)
        .join(".config")
        .join("bsync")
        .join("config.toml")
}

pub fn add_pair(path: &Path, a: &str, b: &str, sync_deletions: bool) -> Result<(), ConfigError> {
    // Validate the pair before persisting
    let check = PairConfig {
        a: a.to_string(),
        b: b.to_string(),
        sync_deletions,
    };
    resolve_pairs(std::slice::from_ref(&check))?;

    let mut config = if path.exists() {
        let content = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        toml::from_str::<Config>(&content).map_err(ConfigError::Parse)?
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        }
        Config { pair: Vec::new() }
    };

    config.pair.push(check);

    let content = toml::to_string_pretty(&config).expect("Failed to serialize config");
    std::fs::write(path, content).map_err(ConfigError::Io)?;

    Ok(())
}

fn count_matching_files(
    base: &Path,
    glob: &GlobMatcher,
    gi_cache: Option<&mut GitignoreCache>,
) -> usize {
    if let Some(cache) = gi_cache {
        WalkBuilder::new(base)
            .hidden(false)
            .build()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && glob.is_match(e.path())
                    && e.path()
                        .strip_prefix(base)
                        .map(|rel| !cache.is_ignored(rel))
                        .unwrap_or(false)
            })
            .count()
    } else {
        WalkDir::new(base)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && glob.is_match(e.path()))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_extract_watch_root() {
        assert_eq!(
            extract_watch_root("/home/user/docs/**/*.md"),
            PathBuf::from("/home/user/docs")
        );
        assert_eq!(
            extract_watch_root("/home/user/docs/*.txt"),
            PathBuf::from("/home/user/docs")
        );
        assert_eq!(extract_watch_root("**/*.rs"), PathBuf::from("."));
        assert_eq!(extract_watch_root("/a/b/c/d"), PathBuf::from("/a/b/c/d"));
    }

    #[test]
    fn test_extract_glob_suffix() {
        assert_eq!(extract_glob_suffix("/home/user/docs/**/*.md"), "**/*.md");
        assert_eq!(extract_glob_suffix("/other/path/**/*.md"), "**/*.md");
        assert_eq!(extract_glob_suffix("**/*.rs"), "**/*.rs");
    }

    #[test]
    fn test_expand_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/foo"), format!("{home}/foo"));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_load_valid_config() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let config_content = format!(
            "[[pair]]\na = \"{}/**/*.md\"\nb = \"{}/**/*.md\"\n",
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, config_content).unwrap();

        let pairs = load(&config_path).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_base, dir_a);
        assert_eq!(pairs[0].b_base, dir_b);
        assert!(!pairs[0].sync_deletions);
    }

    #[test]
    fn test_load_empty_pairs_returns_error() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "pair = []\n").unwrap();

        let result = load(&config_path);
        assert!(matches!(result, Err(ConfigError::NoPairs)));
    }

    #[test]
    fn test_load_mismatched_glob_suffix() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let config_content = format!(
            "[[pair]]\na = \"{}/**/*.md\"\nb = \"{}/**/*.txt\"\n",
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, config_content).unwrap();

        let result = load(&config_path);
        assert!(matches!(
            result,
            Err(ConfigError::MismatchedGlobSuffix { .. })
        ));
    }

    #[test]
    fn test_load_nonexistent_watch_root() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        let config_content = format!(
            "[[pair]]\na = \"{}/nonexistent_a/**/*.md\"\nb = \"{}/nonexistent_b/**/*.md\"\n",
            dir.path().display(),
            dir.path().display()
        );
        fs::write(&config_path, config_content).unwrap();

        let result = load(&config_path);
        assert!(matches!(result, Err(ConfigError::WatchRootNotFound(_))));
    }

    #[test]
    fn test_add_pair_creates_config() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("sub/config.toml");
        let a_pattern = format!("{}/**/*.md", dir_a.display());
        let b_pattern = format!("{}/**/*.md", dir_b.display());

        add_pair(&config_path, &a_pattern, &b_pattern, false).unwrap();

        assert!(config_path.exists());
        let pairs = load(&config_path).unwrap();
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn test_add_pair_appends() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        let dir_c = dir.path().join("c");
        let dir_d = dir.path().join("d");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::create_dir_all(&dir_c).unwrap();
        fs::create_dir_all(&dir_d).unwrap();

        let config_path = dir.path().join("config.toml");
        let a1 = format!("{}/**/*.md", dir_a.display());
        let b1 = format!("{}/**/*.md", dir_b.display());
        add_pair(&config_path, &a1, &b1, false).unwrap();

        let a2 = format!("{}/**/*.txt", dir_c.display());
        let b2 = format!("{}/**/*.txt", dir_d.display());
        add_pair(&config_path, &a2, &b2, true).unwrap();

        let pairs = load(&config_path).unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs[1].sync_deletions);
    }
}
