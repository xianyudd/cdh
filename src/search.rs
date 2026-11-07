//! Shared query intent and ranking, independent of recommendation limits and UI state.
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Matcher, Utf32Str,
};
use std::path::{Component, Path};

#[derive(Clone, Default)]
pub(crate) struct SearchContext {
    pub home: Option<String>,
    pub pwd: Option<String>,
}

impl SearchContext {
    pub fn from_process(pwd: Option<String>) -> Self {
        Self {
            home: std::env::var("HOME").ok(),
            pwd,
        }
    }

    fn path_intent(&self, token: &str) -> Option<String> {
        let expanded = if token == "~" || token.starts_with("~/") {
            let Some(home) = self.home.as_deref().filter(|home| !home.is_empty()) else {
                return Some(token.to_lowercase()); // unresolved literal intent, never fuzzy
            };
            format!("{}{}", home, &token[1..])
        } else if token.starts_with('/') {
            token.to_string()
        } else if token.starts_with("./") || token.starts_with("../") {
            let Some(pwd) = self.pwd.as_deref().filter(|pwd| !pwd.is_empty()) else {
                return Some(token.to_lowercase());
            };
            format!("{}/{}", pwd, token)
        } else {
            return None;
        };
        let mut parts = Vec::new();
        for component in Path::new(&expanded).components() {
            match component {
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                _ => {}
            }
        }
        let mut path = format!("/{}", parts.join("/"));
        if token.ends_with('/') && path != "/" {
            path.push('/');
        }
        Some(path.to_lowercase())
    }
}

/// Higher is stronger. Coverage is below class, never above basename intent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct Strength {
    pub class: u8,
    pub coverage: usize,
    pub score: u32,
}

pub(crate) fn path_depth(path: &str) -> usize {
    Path::new(path)
        .components()
        .filter(|part| matches!(part, Component::Normal(_)))
        .count()
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

fn literal_class(path: &str, token: &str) -> u8 {
    let name = basename(path);
    if name == token {
        5
    } else if name.starts_with(token) {
        4
    } else if name.contains(token) {
        3
    } else {
        1
    }
}

fn path_class(path: &str, intent: &str) -> Option<u8> {
    let exact = intent.trim_end_matches('/');
    if path.trim_end_matches('/') == exact || (intent == "/" && path == "/") {
        return Some(8);
    }
    if !path.starts_with(intent) {
        return None;
    }
    if intent.ends_with('/') || path[exact.len()..].contains('/') {
        Some(6)
    } else {
        Some(7)
    }
}

enum Branch {
    Literal(String),
    Path(String),
}

enum QueryKind {
    Empty,
    Or(Vec<Branch>),
    Path(String),
    Fuzzy(Pattern, String),
}

pub(crate) struct Query {
    kind: QueryKind,
}

impl Query {
    pub fn cli(tokens: &[String], context: &SearchContext) -> Self {
        let branches: Vec<_> = tokens
            .iter()
            .filter(|t| !t.is_empty())
            .map(|token| {
                context
                    .path_intent(token)
                    .map(Branch::Path)
                    .unwrap_or_else(|| Branch::Literal(token.to_lowercase()))
            })
            .collect();
        Self {
            kind: if branches.is_empty() {
                QueryKind::Empty
            } else {
                QueryKind::Or(branches)
            },
        }
    }

    pub fn interactive(text: &str, context: &SearchContext) -> Self {
        if text.trim().is_empty() {
            return Self {
                kind: QueryKind::Empty,
            };
        }
        // Only bare path prefixes are special. Explicit nucleo operators stay fuzzy.
        if let Some(path) = context.path_intent(text) {
            return Self {
                kind: QueryKind::Path(path),
            };
        }
        Self {
            kind: QueryKind::Fuzzy(
                Pattern::parse(text.trim(), CaseMatching::Ignore, Normalization::Smart),
                text.trim().to_lowercase(),
            ),
        }
    }

    pub fn empty(&self) -> bool {
        matches!(self.kind, QueryKind::Empty)
    }

    pub fn strength(
        &self,
        raw: &str,
        matcher: &mut Matcher,
        buffer: &mut Vec<char>,
    ) -> Option<Strength> {
        let path = raw.to_lowercase();
        match &self.kind {
            QueryKind::Empty => Some(Strength::default()),
            QueryKind::Path(intent) => path_class(&path, intent).map(|class| Strength {
                class,
                coverage: 1,
                score: 0,
            }),
            QueryKind::Or(branches) => {
                let mut best = Strength::default();
                let mut coverage = 0;
                for branch in branches {
                    let class = match branch {
                        Branch::Literal(token) if path.contains(token) => {
                            Some(literal_class(&path, token))
                        }
                        Branch::Literal(_) => None,
                        Branch::Path(intent) => path_class(&path, intent),
                    };
                    if let Some(class) = class {
                        coverage += 1;
                        best.class = best.class.max(class);
                    }
                }
                (coverage > 0).then_some(Strength { coverage, ..best })
            }
            QueryKind::Fuzzy(pattern, text) => {
                // Full-path eligibility comes first, including negation and AND atoms.
                let full_score = pattern.score(Utf32Str::new(raw, buffer), matcher)?;
                let leaf_score = pattern.score(Utf32Str::new(basename(raw), buffer), matcher);
                let (class, score) = match leaf_score {
                    Some(score) => {
                        let literal = literal_class(&path, text);
                        // Literal classes should not depend on full path depth/length.
                        if literal > 1 {
                            (literal, 0)
                        } else {
                            (2, score)
                        }
                    }
                    None => (1, full_score),
                };
                Some(Strength {
                    class,
                    coverage: 1,
                    score,
                })
            }
        }
    }

    pub fn highlights(&self, raw: &str, matcher: &mut Matcher) -> Vec<u32> {
        let mut out = Vec::new();
        match &self.kind {
            QueryKind::Fuzzy(pattern, _) => {
                let mut buffer = Vec::new();
                let _ = pattern.indices(Utf32Str::new(raw, &mut buffer), matcher, &mut out);
            }
            QueryKind::Or(branches) => {
                for branch in branches {
                    match branch {
                        Branch::Literal(token) => literal_highlights(raw, token, &mut out),
                        Branch::Path(intent)
                            if path_class(&raw.to_lowercase(), intent).is_some() =>
                        {
                            literal_highlights(raw, intent.trim_end_matches('/'), &mut out)
                        }
                        _ => {}
                    }
                }
            }
            QueryKind::Path(intent) if path_class(&raw.to_lowercase(), intent).is_some() => {
                literal_highlights(raw, intent.trim_end_matches('/'), &mut out)
            }
            _ => {}
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn literal_highlights(raw: &str, token: &str, out: &mut Vec<u32>) {
    // Map lowercase byte offsets back to raw scalar indices, including expanding lowercase.
    let mut lower = String::new();
    let mut indices = Vec::new();
    for (index, ch) in raw.chars().enumerate() {
        for ch in ch.to_lowercase() {
            lower.push(ch);
            indices.resize(indices.len() + ch.len_utf8(), index as u32);
        }
    }
    for (start, _) in lower.match_indices(token) {
        out.extend_from_slice(&indices[start..start + token.len()]);
    }
}

/// Reversible argv display. Plain tokens remain uncluttered; quotes protect spaces.
pub(crate) fn encode_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| {
            if !token.is_empty()
                && !token
                    .chars()
                    .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'))
            {
                token.clone()
            } else {
                format!("\"{}\"", token.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tolerates incomplete quotes during editing; no shell expansion or execution.
pub(crate) fn decode_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo_matcher::Config;
    fn strength(query: &Query, path: &str) -> Option<Strength> {
        query.strength(
            path,
            &mut Matcher::new(Config::DEFAULT.match_paths()),
            &mut Vec::new(),
        )
    }
    #[test]
    fn unresolved_home_or_pwd_never_falls_back_to_unrelated_fuzzy_paths() {
        let context = SearchContext::default();
        assert!(strength(
            &Query::interactive("~/project", &context),
            "/archive/~/old/project"
        )
        .is_none());
        assert!(strength(
            &Query::interactive("../project", &context),
            "/archive/../old/project"
        )
        .is_none());
    }

    #[test]
    fn argv_roundtrips_spaces_quotes_backslashes_and_unicode() {
        let tokens = vec![
            "alpha beta".into(),
            "quote\"here".into(),
            "back\\slash".into(),
            "目录".into(),
        ];
        assert_eq!(decode_tokens(&encode_tokens(&tokens)), tokens);
        assert_eq!(
            decode_tokens("alpha \"beta gamma"),
            vec!["alpha", "beta gamma"]
        );
    }
    #[test]
    fn cli_or_preserves_quoted_token_and_class_beats_coverage() {
        let context = SearchContext::default();
        let query = Query::cli(&["alpha".into(), "beta".into()], &context);
        assert!(strength(&query, "/x/alpha").is_some());
        assert!(strength(&query, "/x/beta").is_some());
        assert!(strength(&query, "/alpha/beta/child") < strength(&query, "/x/alpha"));
        let query = Query::cli(&["alpha beta".into()], &context);
        assert!(strength(&query, "/x/alpha").is_none());
        assert!(strength(&query, "/x/alpha beta").is_some());
    }
    #[test]
    fn paths_expand_lexically_with_component_completion_and_trailing_boundary() {
        let context = SearchContext {
            home: Some("/home/u".into()),
            pwd: Some("/work/base".into()),
        };
        for (text, exact) in [
            ("~/目录 space", "/home/u/目录 space"),
            ("../project", "/work/project"),
            ("./a/../project", "/work/base/project"),
        ] {
            let query = Query::interactive(text, &context);
            assert_eq!(strength(&query, exact).unwrap().class, 8);
            assert!(strength(&query, &format!("/unrelated{exact}")).is_none());
            assert!(!query
                .highlights(exact, &mut Matcher::new(Config::DEFAULT))
                .is_empty());
        }
        let query = Query::interactive("/a/pro", &context);
        assert_eq!(strength(&query, "/a/project").unwrap().class, 7);
        assert_eq!(strength(&query, "/a/project/child").unwrap().class, 6);
        let query = Query::interactive("/a/pro/", &context);
        assert!(strength(&query, "/a/project").is_none());
        assert!(strength(&query, "/a/pro/child").is_some());
    }
    #[test]
    fn smart_unicode_normalization_matches_and_highlights_accented_basename() {
        let query = Query::interactive("cafe", &SearchContext::default());
        let matched =
            strength(&query, "/x/café").expect("ASCII query must match accented basename");
        assert_eq!(matched.class, 2);
        assert_eq!(
            query.highlights("/x/café", &mut Matcher::new(Config::DEFAULT.match_paths())),
            vec![3, 4, 5, 6]
        );
        // Smart normalization must not erase an explicitly accented query's intent.
        let accented = Query::interactive("café", &SearchContext::default());
        assert!(strength(&accented, "/x/café").is_some());
        assert!(strength(&accented, "/x/cafe").is_none());
    }

    #[test]
    fn explicit_patterns_keep_negation_and_anchoring() {
        let query = Query::interactive("project !old", &SearchContext::default());
        assert!(strength(&query, "/x/project-old").is_none());
        assert!(strength(&query, "/x/project").is_some());
        let query = Query::interactive("^/a", &SearchContext::default());
        assert!(strength(&query, "/a/b").is_some());
        assert!(strength(&query, "/b/a").is_none());
    }
}
