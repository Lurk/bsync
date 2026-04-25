use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuffixToken {
    Literal(String),
    Star,
    DoubleStarSlash,
}

#[derive(Debug, Clone)]
pub enum FilenameMap {
    Identity,
    Template {
        a_tokens: Vec<SuffixToken>,
        b_tokens: Vec<SuffixToken>,
        a_regex: regex::Regex,
        b_regex: regex::Regex,
    },
}

impl FilenameMap {
    /// Construct a Template mapping. Caller must have verified the two token vecs
    /// share the same wildcard shape via `wildcard_shapes_compatible`.
    pub fn build(a_tokens: &[SuffixToken], b_tokens: &[SuffixToken]) -> Self {
        let a_regex = build_regex_for_tokens(a_tokens);
        let b_regex = build_regex_for_tokens(b_tokens);
        FilenameMap::Template {
            a_tokens: a_tokens.to_vec(),
            b_tokens: b_tokens.to_vec(),
            a_regex,
            b_regex,
        }
    }

    pub fn map_a_to_b(&self, rel: &Path) -> Option<PathBuf> {
        self.translate(rel, /* a_to_b = */ true)
    }

    pub fn map_b_to_a(&self, rel: &Path) -> Option<PathBuf> {
        self.translate(rel, /* a_to_b = */ false)
    }

    fn translate(&self, rel: &Path, a_to_b: bool) -> Option<PathBuf> {
        match self {
            FilenameMap::Identity => Some(rel.to_path_buf()),
            FilenameMap::Template {
                a_tokens,
                b_tokens,
                a_regex,
                b_regex,
            } => {
                let (regex, to) = if a_to_b {
                    (a_regex, b_tokens)
                } else {
                    (b_regex, a_tokens)
                };
                let rel_str = rel.to_str()?;
                let caps = regex.captures(rel_str)?;
                let mut wildcard_idx = 0;
                let mut out = String::new();
                for tok in to {
                    match tok {
                        SuffixToken::Literal(s) => out.push_str(s),
                        SuffixToken::Star | SuffixToken::DoubleStarSlash => {
                            wildcard_idx += 1;
                            let m = caps.get(wildcard_idx)?;
                            out.push_str(m.as_str());
                        }
                    }
                }
                Some(PathBuf::from(out))
            }
        }
    }
}

fn build_regex_for_tokens(tokens: &[SuffixToken]) -> regex::Regex {
    let mut pattern = String::from("^");
    for tok in tokens {
        match tok {
            SuffixToken::Literal(s) => pattern.push_str(&regex::escape(s)),
            SuffixToken::Star => pattern.push_str("([^/]*)"),
            SuffixToken::DoubleStarSlash => pattern.push_str("((?:[^/]*/)*)"),
        }
    }
    pattern.push('$');
    regex::Regex::new(&pattern).expect("token-derived regex is always valid")
}

pub fn tokenize_suffix(suffix: &str) -> Result<Vec<SuffixToken>, &'static str> {
    // `*`, `/` and the rejected metacharacters are all single-byte ASCII, so
    // byte-index lookahead is correct as long as the literal-building path
    // pushes whole `chars`. Iterating char_indices() gives us both.
    let bytes = suffix.as_bytes();
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut doublestar_count = 0;
    let mut chars = suffix.char_indices();

    while let Some((i, c)) = chars.next() {
        match c {
            '?' | '[' | '{' => {
                return Err("characters '?', '[', '{' are not supported");
            }
            '*' => {
                if !literal.is_empty() {
                    tokens.push(SuffixToken::Literal(std::mem::take(&mut literal)));
                }

                let is_double = i + 1 < bytes.len() && bytes[i + 1] == b'*';
                if is_double {
                    let prev_is_boundary = i == 0 || bytes[i - 1] == b'/';
                    let next_is_slash = i + 2 < bytes.len() && bytes[i + 2] == b'/';
                    if !prev_is_boundary || !next_is_slash {
                        return Err(
                            "'**' must appear as a full path component followed by '/' (e.g. '**/x', 'foo/**/x')",
                        );
                    }
                    doublestar_count += 1;
                    if doublestar_count > 1 {
                        return Err("more than one '**' is not supported");
                    }
                    tokens.push(SuffixToken::DoubleStarSlash);
                    chars.next(); // consume second '*'
                    chars.next(); // consume trailing '/'
                } else {
                    tokens.push(SuffixToken::Star);
                }
            }
            _ => {
                literal.push(c);
            }
        }
    }

    if !literal.is_empty() {
        tokens.push(SuffixToken::Literal(literal));
    }

    Ok(tokens)
}

pub fn wildcard_shapes_compatible(a: &[SuffixToken], b: &[SuffixToken]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(at, bt)| {
        matches!(
            (at, bt),
            (SuffixToken::Star, SuffixToken::Star)
                | (SuffixToken::DoubleStarSlash, SuffixToken::DoubleStarSlash)
                | (SuffixToken::Literal(_), SuffixToken::Literal(_))
        )
    })
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    pub fn tokenize_for_test(suffix: &str) -> Vec<SuffixToken> {
        tokenize_suffix(suffix).expect("test suffix tokenizes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_suffix_simple_star() {
        let tokens = tokenize_suffix("*.md").unwrap();
        assert_eq!(
            tokens,
            vec![SuffixToken::Star, SuffixToken::Literal(".md".into())]
        );
    }

    #[test]
    fn test_tokenize_suffix_doublestar_slash_star() {
        let tokens = tokenize_suffix("**/*.md").unwrap();
        assert_eq!(
            tokens,
            vec![
                SuffixToken::DoubleStarSlash,
                SuffixToken::Star,
                SuffixToken::Literal(".md".into()),
            ]
        );
    }

    #[test]
    fn test_tokenize_suffix_with_leading_literal() {
        let tokens = tokenize_suffix("foo/**/*.md.gz").unwrap();
        assert_eq!(
            tokens,
            vec![
                SuffixToken::Literal("foo/".into()),
                SuffixToken::DoubleStarSlash,
                SuffixToken::Star,
                SuffixToken::Literal(".md.gz".into()),
            ]
        );
    }

    // The original tokenizer iterated bytes and cast each `bytes[i] as char`,
    // which split multi-byte UTF-8 codepoints into Latin-1 surrogates and
    // silently corrupted any non-ASCII directory name in the suffix.
    #[test]
    fn test_tokenize_suffix_preserves_non_ascii_literal() {
        let tokens = tokenize_suffix("документы/*.md").unwrap();
        assert_eq!(
            tokens,
            vec![
                SuffixToken::Literal("документы/".into()),
                SuffixToken::Star,
                SuffixToken::Literal(".md".into()),
            ]
        );
    }

    #[test]
    fn test_tokenize_suffix_rejects_question_mark() {
        assert!(tokenize_suffix("a?.md").is_err());
    }

    #[test]
    fn test_tokenize_suffix_rejects_charclass() {
        assert!(tokenize_suffix("[ab]*.md").is_err());
    }

    #[test]
    fn test_tokenize_suffix_rejects_braces() {
        assert!(tokenize_suffix("{a,b}.md").is_err());
    }

    #[test]
    fn test_tokenize_suffix_rejects_trailing_doublestar() {
        assert!(tokenize_suffix("foo/**").is_err());
    }

    #[test]
    fn test_tokenize_suffix_rejects_doublestar_inside_segment() {
        assert!(tokenize_suffix("foo**bar.md").is_err());
    }

    #[test]
    fn test_tokenize_suffix_rejects_multiple_doublestar() {
        assert!(tokenize_suffix("**/x/**/*.md").is_err());
    }

    #[test]
    fn test_wildcard_shapes_compatible_identity() {
        let a = tokenize_suffix("**/*.md").unwrap();
        let b = tokenize_suffix("**/*.md").unwrap();
        assert!(wildcard_shapes_compatible(&a, &b));
    }

    #[test]
    fn test_wildcard_shapes_compatible_ext_rename() {
        let a = tokenize_suffix("**/*.md").unwrap();
        let b = tokenize_suffix("**/*.yamd").unwrap();
        assert!(wildcard_shapes_compatible(&a, &b));
    }

    #[test]
    fn test_wildcard_shapes_compatible_gzip_pattern() {
        let a = tokenize_suffix("**/*.md").unwrap();
        let b = tokenize_suffix("**/*.md.gz").unwrap();
        assert!(wildcard_shapes_compatible(&a, &b));
    }

    #[test]
    fn test_wildcard_shapes_compatible_differing_literal_dirs() {
        let a = tokenize_suffix("foo/**/*.md").unwrap();
        let b = tokenize_suffix("bar/**/*.md.gz").unwrap();
        assert!(wildcard_shapes_compatible(&a, &b));
    }

    #[test]
    fn test_wildcard_shapes_incompatible_different_count() {
        let a = tokenize_suffix("*.md").unwrap();
        let b = tokenize_suffix("**/*.md").unwrap();
        assert!(!wildcard_shapes_compatible(&a, &b));
    }

    #[test]
    fn test_wildcard_shapes_incompatible_different_kind() {
        // [Star, Literal(".md")]   vs   [DoubleStarSlash, Literal(".md")]
        // Same count of wildcards (1) but different kind.
        let a = vec![SuffixToken::Star, SuffixToken::Literal(".md".into())];
        let b = vec![
            SuffixToken::DoubleStarSlash,
            SuffixToken::Literal(".md".into()),
        ];
        assert!(!wildcard_shapes_compatible(&a, &b));
    }

    // A has trailing Literal(".md"), B has no trailing literal. Without the
    // structural-position check, these are accepted (both have wildcard set
    // [DoubleStarSlash, Star]) and B-side files like "notes" map to A as
    // "notes.md", which then matches A's glob and ping-pongs.
    #[test]
    fn test_wildcard_shapes_incompatible_missing_trailing_literal() {
        let a = tokenize_suffix("**/*.md").unwrap();
        let b = tokenize_suffix("**/*").unwrap();
        assert!(!wildcard_shapes_compatible(&a, &b));
    }

    // A has an interior Literal between wildcards, B doesn't. Accepting these
    // would let B-side files map "x.md" to "foo/x.md" with no inverse on B,
    // turning the bidirectional contract into a one-way fan-out.
    #[test]
    fn test_wildcard_shapes_incompatible_extra_interior_literal() {
        let a = tokenize_suffix("**/*.md").unwrap();
        let b = tokenize_suffix("**/foo/*.md").unwrap();
        assert!(!wildcard_shapes_compatible(&a, &b));
    }

    #[test]
    fn test_filename_map_identity_passes_path_through() {
        let map = FilenameMap::Identity;
        assert_eq!(
            map.map_a_to_b(Path::new("notes/foo.md")).unwrap(),
            PathBuf::from("notes/foo.md")
        );
        assert_eq!(
            map.map_b_to_a(Path::new("notes/foo.md")).unwrap(),
            PathBuf::from("notes/foo.md")
        );
    }

    #[test]
    fn test_filename_map_template_ext_rename() {
        let map = FilenameMap::build(
            &tokenize_suffix("**/*.md").unwrap(),
            &tokenize_suffix("**/*.yamd").unwrap(),
        );
        assert_eq!(
            map.map_a_to_b(Path::new("notes/sub/foo.md")).unwrap(),
            PathBuf::from("notes/sub/foo.yamd")
        );
        assert_eq!(
            map.map_b_to_a(Path::new("notes/sub/foo.yamd")).unwrap(),
            PathBuf::from("notes/sub/foo.md")
        );
    }

    #[test]
    fn test_filename_map_template_gzip() {
        let map = FilenameMap::build(
            &tokenize_suffix("**/*.md").unwrap(),
            &tokenize_suffix("**/*.md.gz").unwrap(),
        );
        assert_eq!(
            map.map_a_to_b(Path::new("foo.md")).unwrap(),
            PathBuf::from("foo.md.gz")
        );
        assert_eq!(
            map.map_a_to_b(Path::new("a/b/foo.md")).unwrap(),
            PathBuf::from("a/b/foo.md.gz")
        );
        assert_eq!(
            map.map_b_to_a(Path::new("a/b/foo.md.gz")).unwrap(),
            PathBuf::from("a/b/foo.md")
        );
    }

    // Exercises FilenameMap with token sequences that begin with a Literal.
    // resolve_pairs cannot produce such tokens itself (extract_watch_root
    // strips leading literal path components into the watch root), so this
    // is a defensive check on FilenameMap's substitution logic, not a
    // user-reachable config shape.
    #[test]
    fn test_filename_map_template_with_leading_literal_tokens() {
        let map = FilenameMap::build(
            &tokenize_suffix("foo/**/*.md").unwrap(),
            &tokenize_suffix("bar/**/*.md.gz").unwrap(),
        );
        assert_eq!(
            map.map_a_to_b(Path::new("foo/sub/x.md")).unwrap(),
            PathBuf::from("bar/sub/x.md.gz")
        );
        assert_eq!(
            map.map_b_to_a(Path::new("bar/sub/x.md.gz")).unwrap(),
            PathBuf::from("foo/sub/x.md")
        );
    }

    #[test]
    fn test_filename_map_template_returns_none_for_nonmatching() {
        let map = FilenameMap::build(
            &tokenize_suffix("**/*.md").unwrap(),
            &tokenize_suffix("**/*.md.gz").unwrap(),
        );
        assert!(map.map_a_to_b(Path::new("notes/foo.txt")).is_none());
    }
}
