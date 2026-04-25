use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

use crate::filename_map::{FilenameMap, SuffixToken, tokenize_suffix, wildcard_shapes_compatible};
use crate::gitignore::GitignoreCache;

pub const DEFAULT_PIPELINE_TIMEOUT_SECS: u64 = 300;

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
    #[serde(default)]
    pub allow_empty_sync: bool,
    #[serde(default)]
    pub a_to_b: Option<String>,
    #[serde(default)]
    pub b_to_a: Option<String>,
    #[serde(default)]
    pub pipeline_timeout_secs: Option<u64>,
}

#[derive(Debug)]
pub struct ResolvedPair {
    pub a_base: PathBuf,
    pub b_base: PathBuf,
    pub a_glob: GlobMatcher,
    pub b_glob: GlobMatcher,
    pub a_pattern: String,
    pub b_pattern: String,
    pub sync_deletions: bool,
    pub allow_empty_sync: bool,
    pub has_glob: bool,
    pub filename_map: FilenameMap,
    pub content: ContentTransform,
}

impl ResolvedPair {
    pub fn a_to_b_path(&self, rel: &Path) -> Option<PathBuf> {
        self.filename_map
            .map_a_to_b(rel)
            .map(|other| self.b_base.join(other))
    }

    pub fn b_to_a_path(&self, rel: &Path) -> Option<PathBuf> {
        self.filename_map
            .map_b_to_a(rel)
            .map(|other| self.a_base.join(other))
    }
}

#[derive(Debug, Clone)]
pub enum ContentTransform {
    Identity,
    Command {
        a_to_b: String,
        b_to_a: String,
        timeout: Duration,
    },
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    InvalidGlob(String, globset::Error),
    MismatchedGlobSuffix {
        a: String,
        b: String,
    },
    WatchRootNotFound(PathBuf),
    NoPairs,
    InvalidPairNumber {
        given: usize,
        max: usize,
    },
    UnsupportedGlobFeature {
        pattern: String,
        reason: &'static str,
    },
    PipelineRequiresBothDirections {
        a: String,
        b: String,
    },
    InvalidPipelineTimeout {
        a: String,
        b: String,
    },
    PipelineTimeoutWithoutCommand {
        a: String,
        b: String,
    },
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
            ConfigError::InvalidPairNumber { given, max } => {
                write!(f, "Invalid pair number {given}: config has {max} pair(s)")
            }
            ConfigError::UnsupportedGlobFeature { pattern, reason } => {
                write!(f, "Unsupported glob feature in '{pattern}': {reason}")
            }
            ConfigError::PipelineRequiresBothDirections { a, b } => {
                write!(
                    f,
                    "Both 'a_to_b' and 'b_to_a' must be set together (or neither) for pair '{a}' <-> '{b}'"
                )
            }
            ConfigError::InvalidPipelineTimeout { a, b } => {
                write!(
                    f,
                    "'pipeline_timeout_secs' must be > 0 for pair '{a}' <-> '{b}'"
                )
            }
            ConfigError::PipelineTimeoutWithoutCommand { a, b } => {
                write!(
                    f,
                    "'pipeline_timeout_secs' is set but no pipeline command is configured for pair '{a}' <-> '{b}'"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

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

        // Tokenize both suffixes unconditionally so the documented restrictions
        // (no '?', '[…]', '{…}'; well-formed '**') apply uniformly — even when
        // the suffixes are identical and we'd otherwise short-circuit to Identity.
        let a_tokens =
            tokenize_suffix(&a_suffix).map_err(|reason| ConfigError::UnsupportedGlobFeature {
                pattern: pair.a.clone(),
                reason,
            })?;
        let b_tokens =
            tokenize_suffix(&b_suffix).map_err(|reason| ConfigError::UnsupportedGlobFeature {
                pattern: pair.b.clone(),
                reason,
            })?;

        let filename_map = if a_suffix == b_suffix {
            FilenameMap::Identity
        } else {
            if !wildcard_shapes_compatible(&a_tokens, &b_tokens) {
                return Err(ConfigError::MismatchedGlobSuffix {
                    a: pair.a.clone(),
                    b: pair.b.clone(),
                });
            }
            FilenameMap::build(&a_tokens, &b_tokens)
        };

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

        let content = match (pair.a_to_b.as_ref(), pair.b_to_a.as_ref()) {
            (None, None) => {
                if pair.pipeline_timeout_secs.is_some() {
                    return Err(ConfigError::PipelineTimeoutWithoutCommand {
                        a: pair.a.clone(),
                        b: pair.b.clone(),
                    });
                }
                ContentTransform::Identity
            }
            (Some(a_to_b), Some(b_to_a)) => {
                let secs = pair
                    .pipeline_timeout_secs
                    .unwrap_or(DEFAULT_PIPELINE_TIMEOUT_SECS);
                if secs == 0 {
                    return Err(ConfigError::InvalidPipelineTimeout {
                        a: pair.a.clone(),
                        b: pair.b.clone(),
                    });
                }
                ContentTransform::Command {
                    a_to_b: a_to_b.clone(),
                    b_to_a: b_to_a.clone(),
                    timeout: Duration::from_secs(secs),
                }
            }
            _ => {
                return Err(ConfigError::PipelineRequiresBothDirections {
                    a: pair.a.clone(),
                    b: pair.b.clone(),
                });
            }
        };

        resolved.push(ResolvedPair {
            a_base,
            b_base,
            a_glob,
            b_glob,
            a_pattern: a_expanded,
            b_pattern: b_expanded,
            sync_deletions: pair.sync_deletions,
            allow_empty_sync: pair.allow_empty_sync,
            has_glob,
            filename_map,
            content,
        });
    }

    Ok(resolved)
}

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
        println!("  allow_empty_sync: {}", pair.allow_empty_sync);
        println!(
            "  gitignore filtering: {}",
            if pair.has_glob { "enabled" } else { "disabled" }
        );

        match &pair.filename_map {
            FilenameMap::Identity => println!("  filename mapping: identity"),
            FilenameMap::Template { a_tokens, .. } => {
                let n_wildcards = a_tokens
                    .iter()
                    .filter(|t| !matches!(t, SuffixToken::Literal(_)))
                    .count();
                println!("  filename mapping: template ({n_wildcards} wildcard(s))");
            }
        }

        match &pair.content {
            ContentTransform::Identity => println!("  content transform: identity"),
            ContentTransform::Command {
                a_to_b,
                b_to_a,
                timeout,
            } => {
                println!(
                    "  content transform: command (a→b: \"{a_to_b}\", b→a: \"{b_to_a}\", timeout: {}s)",
                    timeout.as_secs()
                );
            }
        }

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

fn canonicalize_pattern(pattern: &str) -> Result<String, ConfigError> {
    let expanded = expand_tilde(pattern);
    let root = extract_watch_root(&expanded);
    let suffix = extract_glob_suffix(&expanded);
    let canonical_root = std::fs::canonicalize(&root).map_err(ConfigError::Io)?;
    if suffix.is_empty() {
        Ok(canonical_root.display().to_string())
    } else {
        Ok(format!("{}/{}", canonical_root.display(), suffix))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn add_pair(
    path: &Path,
    a: &str,
    b: &str,
    sync_deletions: bool,
    allow_empty_sync: bool,
    a_to_b: Option<String>,
    b_to_a: Option<String>,
    pipeline_timeout_secs: Option<u64>,
) -> Result<(String, String), ConfigError> {
    let a_canonical = canonicalize_pattern(a)?;
    let b_canonical = canonicalize_pattern(b)?;

    let check = PairConfig {
        a: a_canonical.clone(),
        b: b_canonical.clone(),
        sync_deletions,
        allow_empty_sync,
        a_to_b: a_to_b.clone(),
        b_to_a: b_to_a.clone(),
        pipeline_timeout_secs,
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

    Ok((a_canonical, b_canonical))
}

pub fn remove_pair(path: &Path, number: usize) -> Result<PairConfig, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    let mut config: Config = toml::from_str(&content).map_err(ConfigError::Parse)?;

    if number == 0 || number > config.pair.len() {
        return Err(ConfigError::InvalidPairNumber {
            given: number,
            max: config.pair.len(),
        });
    }

    let removed = config.pair.remove(number - 1);

    let content = toml::to_string_pretty(&config).expect("Failed to serialize config");
    std::fs::write(path, content).map_err(ConfigError::Io)?;

    Ok(removed)
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
            "[[pair]]\na = \"{}/**/*.md\"\nb = \"{}/*.txt\"\n",
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

        add_pair(
            &config_path,
            &a_pattern,
            &b_pattern,
            false,
            false,
            None,
            None,
            None,
        )
        .unwrap();

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
        add_pair(&config_path, &a1, &b1, false, false, None, None, None).unwrap();

        let a2 = format!("{}/**/*.txt", dir_c.display());
        let b2 = format!("{}/**/*.txt", dir_d.display());
        add_pair(&config_path, &a2, &b2, true, false, None, None, None).unwrap();

        let pairs = load(&config_path).unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs[1].sync_deletions);
    }

    #[test]
    fn test_remove_pair() {
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
        add_pair(&config_path, &a1, &b1, false, false, None, None, None).unwrap();

        let a2 = format!("{}/**/*.txt", dir_c.display());
        let b2 = format!("{}/**/*.txt", dir_d.display());
        add_pair(&config_path, &a2, &b2, true, false, None, None, None).unwrap();

        let removed = remove_pair(&config_path, 1).unwrap();
        assert!(removed.a.contains("a/"));

        let content = fs::read_to_string(&config_path).unwrap();
        let config: Config = toml::from_str(&content).unwrap();
        assert_eq!(config.pair.len(), 1);
        assert!(config.pair[0].sync_deletions);
    }

    #[test]
    fn test_remove_pair_invalid_number() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let a = format!("{}/**/*.md", dir_a.display());
        let b = format!("{}/**/*.md", dir_b.display());
        add_pair(&config_path, &a, &b, false, false, None, None, None).unwrap();

        assert!(matches!(
            remove_pair(&config_path, 0),
            Err(ConfigError::InvalidPairNumber { given: 0, max: 1 })
        ));
        assert!(matches!(
            remove_pair(&config_path, 2),
            Err(ConfigError::InvalidPairNumber { given: 2, max: 1 })
        ));
    }

    #[test]
    fn test_resolve_pairs_builds_filename_map() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        // Identity mapping
        let pair_id = PairConfig {
            a: format!("{}/**/*.md", dir_a.display()),
            b: format!("{}/**/*.md", dir_b.display()),
            sync_deletions: false,
            allow_empty_sync: false,
            a_to_b: None,
            b_to_a: None,
            pipeline_timeout_secs: None,
        };
        let pairs = resolve_pairs(std::slice::from_ref(&pair_id)).unwrap();
        assert!(matches!(pairs[0].filename_map, FilenameMap::Identity));

        // Template mapping (gzip-style)
        let pair_tpl = PairConfig {
            a: format!("{}/**/*.md", dir_a.display()),
            b: format!("{}/**/*.md.gz", dir_b.display()),
            sync_deletions: false,
            allow_empty_sync: false,
            a_to_b: None,
            b_to_a: None,
            pipeline_timeout_secs: None,
        };
        let pairs = resolve_pairs(std::slice::from_ref(&pair_tpl)).unwrap();
        assert!(matches!(
            pairs[0].filename_map,
            FilenameMap::Template { .. }
        ));
    }

    #[test]
    fn test_pipeline_parses_both_directions() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            r#"
[[pair]]
a = "{}/**/*.md"
b = "{}/**/*.md.gz"
a_to_b = "gzip -c"
b_to_a = "gunzip -c"
"#,
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let pairs = load(&config_path).unwrap();
        match &pairs[0].content {
            ContentTransform::Command {
                a_to_b,
                b_to_a,
                timeout,
            } => {
                assert_eq!(a_to_b, "gzip -c");
                assert_eq!(b_to_a, "gunzip -c");
                assert_eq!(*timeout, Duration::from_secs(DEFAULT_PIPELINE_TIMEOUT_SECS));
            }
            ContentTransform::Identity => panic!("expected Command, got Identity"),
        }
    }

    #[test]
    fn test_pipeline_defaults_to_identity_when_absent() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            "[[pair]]\na = \"{}/**/*.md\"\nb = \"{}/**/*.md\"\n",
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let pairs = load(&config_path).unwrap();
        assert!(matches!(pairs[0].content, ContentTransform::Identity));
    }

    #[test]
    fn test_pipeline_requires_both_directions() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            r#"
[[pair]]
a = "{}/**/*.md"
b = "{}/**/*.md.gz"
a_to_b = "gzip -c"
"#,
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let err = load(&config_path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::PipelineRequiresBothDirections { .. }
        ));
    }

    #[test]
    fn test_resolved_pair_path_methods_identity() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let pair = PairConfig {
            a: format!("{}/**/*.md", dir_a.display()),
            b: format!("{}/**/*.md", dir_b.display()),
            sync_deletions: false,
            allow_empty_sync: false,
            a_to_b: None,
            b_to_a: None,
            pipeline_timeout_secs: None,
        };
        let pairs = resolve_pairs(std::slice::from_ref(&pair)).unwrap();
        let resolved = &pairs[0];

        let rel = Path::new("notes/foo.md");
        assert_eq!(
            resolved.a_to_b_path(rel).unwrap(),
            dir_b.join("notes/foo.md")
        );
        assert_eq!(
            resolved.b_to_a_path(rel).unwrap(),
            dir_a.join("notes/foo.md")
        );
    }

    #[test]
    fn test_add_pair_persists_pipeline_fields() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let a_pattern = format!("{}/**/*.md", dir_a.display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.display());
        add_pair(
            &config_path,
            &a_pattern,
            &b_pattern,
            false,
            false,
            Some("gzip -c".into()),
            Some("gunzip -c".into()),
            None,
        )
        .unwrap();

        let pairs = load(&config_path).unwrap();
        match &pairs[0].content {
            ContentTransform::Command {
                a_to_b,
                b_to_a,
                timeout,
            } => {
                assert_eq!(a_to_b, "gzip -c");
                assert_eq!(b_to_a, "gunzip -c");
                assert_eq!(*timeout, Duration::from_secs(DEFAULT_PIPELINE_TIMEOUT_SECS));
            }
            _ => panic!("expected Command transform"),
        }
    }

    #[test]
    fn test_resolved_pair_path_methods_template_gzip() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let pair = PairConfig {
            a: format!("{}/**/*.md", dir_a.display()),
            b: format!("{}/**/*.md.gz", dir_b.display()),
            sync_deletions: false,
            allow_empty_sync: false,
            a_to_b: None,
            b_to_a: None,
            pipeline_timeout_secs: None,
        };
        let pairs = resolve_pairs(std::slice::from_ref(&pair)).unwrap();
        let resolved = &pairs[0];

        assert_eq!(
            resolved.a_to_b_path(Path::new("foo.md")).unwrap(),
            dir_b.join("foo.md.gz")
        );
        assert_eq!(
            resolved.b_to_a_path(Path::new("sub/x.md.gz")).unwrap(),
            dir_a.join("sub/x.md")
        );
    }

    #[test]
    fn test_pipeline_timeout_secs_overrides_default() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            r#"
[[pair]]
a = "{}/**/*.md"
b = "{}/**/*.md.gz"
a_to_b = "gzip -c"
b_to_a = "gunzip -c"
pipeline_timeout_secs = 17
"#,
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let pairs = load(&config_path).unwrap();
        match &pairs[0].content {
            ContentTransform::Command { timeout, .. } => {
                assert_eq!(*timeout, Duration::from_secs(17));
            }
            ContentTransform::Identity => panic!("expected Command, got Identity"),
        }
    }

    #[test]
    fn test_pipeline_timeout_secs_zero_rejected() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            r#"
[[pair]]
a = "{}/**/*.md"
b = "{}/**/*.md.gz"
a_to_b = "gzip -c"
b_to_a = "gunzip -c"
pipeline_timeout_secs = 0
"#,
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let err = load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidPipelineTimeout { .. }));
    }

    // pipeline_timeout_secs without a_to_b/b_to_a is meaningless and almost
    // always indicates a config typo — surface it instead of silently ignoring.
    #[test]
    fn test_pipeline_timeout_without_command_rejected() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            r#"
[[pair]]
a = "{}/**/*.md"
b = "{}/**/*.md"
pipeline_timeout_secs = 30
"#,
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let err = load(&config_path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::PipelineTimeoutWithoutCommand { .. }
        ));
    }

    // Identity mode (suffixes equal) used to skip tokenizing entirely, so
    // configs with documented-as-rejected glob features (?, [...], {...}) were
    // silently accepted. Validation must apply uniformly to both sides.
    #[test]
    fn test_load_rejects_unsupported_glob_feature_in_identity_mode() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let content = format!(
            "[[pair]]\na = \"{}/**/a?.md\"\nb = \"{}/**/a?.md\"\n",
            dir_a.display(),
            dir_b.display()
        );
        fs::write(&config_path, content).unwrap();

        let err = load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::UnsupportedGlobFeature { .. }));
    }

    #[test]
    fn test_add_pair_persists_pipeline_timeout() {
        let dir = TempDir::new().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let config_path = dir.path().join("config.toml");
        let a_pattern = format!("{}/**/*.md", dir_a.display());
        let b_pattern = format!("{}/**/*.md.gz", dir_b.display());
        add_pair(
            &config_path,
            &a_pattern,
            &b_pattern,
            false,
            false,
            Some("gzip -c".into()),
            Some("gunzip -c".into()),
            Some(42),
        )
        .unwrap();

        let pairs = load(&config_path).unwrap();
        match &pairs[0].content {
            ContentTransform::Command { timeout, .. } => {
                assert_eq!(*timeout, Duration::from_secs(42));
            }
            _ => panic!("expected Command transform"),
        }
    }
}
