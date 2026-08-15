//! Rsync-compatible filter rules and filter-file parsing.
//!
//! Semantics follow `rsync(1)` include/exclude rules as applied to a flat
//! archive member list (we simulate the directory walk):
//!
//! - Rules are checked **in order**; the first match wins.
//! - If no rule matches, the path is **included** (rsync default).
//! - A pattern with no `/` (except a trailing `/`) matches the **basename**.
//! - A pattern containing `/` (not counting a trailing `/`) or `**` is matched
//!   against the full normalized path. A leading `/` anchors to the archive root.
//! - A trailing `/` matches **directories only**.
//! - Excluding a directory (first-match on that directory) excludes its children
//!   — rsync would not recurse.
//! - `*` matches any run of non-`/` characters; `**` also matches `/`;
//!   `?` matches one non-`/` character.
//! - A trailing `***` matches the directory and everything under it.
//!
//! Filter files accept `#` / `;` comments, `+`/`-`/`include`/`exclude` rules,
//! `merge` / `.` (and `dir-merge` / `:`, treated as merge), and `clear` / `!`.

use crate::error::{Error, Result};
use crate::util::pathnorm::{member_basename, normalize_member_path};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One compiled rsync include/exclude pattern.
#[derive(Debug, Clone)]
pub struct RsyncPattern {
    /// Original pattern text (without `+`/`-` prefix).
    pub original: String,
    /// `true` = include (`+`), `false` = exclude (`-`).
    pub include: bool,
    tokens: Vec<Tok>,
    /// Match against basename only (pattern has no internal `/` and no `**`).
    basename_only: bool,
    /// Pattern ended with `/` — directories only.
    dir_only: bool,
    /// Trailing `***` — directory and all descendants.
    triple_star: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Lit(char),
    Ques,
    Star,
    GlobStar,
    Class {
        negated: bool,
        chars: Vec<char>,
        ranges: Vec<(char, char)>,
    },
}

impl RsyncPattern {
    /// Parse a pattern body (no `+`/`-` prefix).
    pub fn parse(pattern: &str, include: bool) -> Result<Self> {
        let original = pattern.to_string();
        let mut pat = pattern.trim();
        if pat.is_empty() {
            return Err(Error::InvalidFilter {
                rule: original,
                message: "empty rsync pattern".into(),
            });
        }

        let triple_star = pat.ends_with("***");
        if triple_star {
            pat = pat.trim_end_matches("***");
            pat = pat.trim_end_matches('/');
        }

        let dir_only = !triple_star && pat.ends_with('/');
        if dir_only {
            pat = pat.trim_end_matches('/');
        }

        let anchored = pat.starts_with('/');
        if anchored {
            pat = &pat[1..];
        }

        let has_internal_slash = pat.contains('/');
        let has_globstar = pat.contains("**");
        let basename_only = !anchored && !has_internal_slash && !has_globstar && !triple_star;

        let tokens = tokenize(pat).map_err(|message| Error::InvalidFilter {
            rule: original.clone(),
            message,
        })?;

        Ok(Self {
            original,
            include,
            tokens,
            basename_only,
            dir_only,
            triple_star,
        })
    }

    /// Whether this pattern matches `path` (normalized) as a file or directory.
    pub fn matches(&self, path: &str, is_dir: bool) -> bool {
        let path = normalize_member_path(path);
        if path.is_empty() {
            return false;
        }
        if self.dir_only && !is_dir {
            return false;
        }
        if self.triple_star {
            let prefix = self.triple_prefix();
            if prefix.is_empty() {
                return true;
            }
            return path == prefix || path.starts_with(&format!("{prefix}/"));
        }
        let target = if self.basename_only {
            member_basename(&path)
        } else {
            path
        };
        match_tokens(&self.tokens, &target.chars().collect::<Vec<_>>())
    }

    fn triple_prefix(&self) -> String {
        // Reconstruct the directory prefix from tokens (literals + slashes only).
        let mut s = String::new();
        for t in &self.tokens {
            match t {
                Tok::Lit(c) => s.push(*c),
                _ => {}
            }
        }
        s
    }
}

fn tokenize(pat: &str) -> std::result::Result<Vec<Tok>, String> {
    let chars: Vec<char> = pat.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    return Err("trailing backslash in pattern".into());
                }
                out.push(Tok::Lit(chars[i]));
                i += 1;
            }
            '?' => {
                out.push(Tok::Ques);
                i += 1;
            }
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push(Tok::GlobStar);
                    i += 2;
                    // Collapse further * in a `**` run.
                    while i < chars.len() && chars[i] == '*' {
                        i += 1;
                    }
                } else {
                    out.push(Tok::Star);
                    i += 1;
                }
            }
            '[' => {
                let (tok, next) = parse_class(&chars, i)?;
                out.push(tok);
                i = next;
            }
            c => {
                out.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    Ok(out)
}

fn parse_class(
    chars: &[char],
    start: usize,
) -> std::result::Result<(Tok, usize), String> {
    // start points at '['
    let mut i = start + 1;
    if i >= chars.len() {
        return Err("unclosed character class".into());
    }
    let mut negated = false;
    if chars[i] == '!' || chars[i] == '^' {
        negated = true;
        i += 1;
    }
    let mut class_chars = Vec::new();
    let mut ranges = Vec::new();
    let mut first = true;
    while i < chars.len() {
        if chars[i] == ']' && !first {
            return Ok((
                Tok::Class {
                    negated,
                    chars: class_chars,
                    ranges,
                },
                i + 1,
            ));
        }
        first = false;
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
            let a = chars[i];
            let b = chars[i + 2];
            if a <= b {
                ranges.push((a, b));
            } else {
                ranges.push((b, a));
            }
            i += 3;
        } else {
            class_chars.push(chars[i]);
            i += 1;
        }
    }
    Err("unclosed character class".into())
}

fn class_matches(c: char, chars: &[char], ranges: &[(char, char)]) -> bool {
    chars.contains(&c) || ranges.iter().any(|(a, b)| c >= *a && c <= *b)
}

fn match_tokens(pat: &[Tok], text: &[char]) -> bool {
    match_from(pat, 0, text, 0)
}

fn match_from(pat: &[Tok], pi: usize, text: &[char], ti: usize) -> bool {
    if pi == pat.len() {
        return ti == text.len();
    }
    match &pat[pi] {
        Tok::GlobStar => {
            // `**` matches any string (including empty and slashes).
            // `**/` also matches an empty prefix (so `**/foo` matches `foo`).
            let rest = &pat[pi + 1..];
            if match_from(pat, pi + 1, text, ti) {
                return true;
            }
            if matches!(rest.first(), Some(Tok::Lit('/')))
                && match_from(pat, pi + 2, text, ti)
            {
                return true;
            }
            for k in ti..text.len() {
                if match_from(pat, pi + 1, text, k + 1) {
                    return true;
                }
                if matches!(rest.first(), Some(Tok::Lit('/')))
                    && text[k] == '/'
                    && match_from(pat, pi + 2, text, k + 1)
                {
                    return true;
                }
            }
            false
        }
        Tok::Star => {
            if match_from(pat, pi + 1, text, ti) {
                return true;
            }
            let mut k = ti;
            while k < text.len() && text[k] != '/' {
                k += 1;
                if match_from(pat, pi + 1, text, k) {
                    return true;
                }
            }
            false
        }
        Tok::Ques => {
            if ti >= text.len() || text[ti] == '/' {
                return false;
            }
            match_from(pat, pi + 1, text, ti + 1)
        }
        Tok::Lit(c) => {
            if ti >= text.len() || text[ti] != *c {
                return false;
            }
            match_from(pat, pi + 1, text, ti + 1)
        }
        Tok::Class {
            negated,
            chars,
            ranges,
        } => {
            if ti >= text.len() || text[ti] == '/' {
                return false;
            }
            let hit = class_matches(text[ti], chars, ranges);
            if *negated == hit {
                return false;
            }
            match_from(pat, pi + 1, text, ti + 1)
        }
    }
}

/// Kind of a parsed filter-file / `--filter` line.
#[derive(Debug, Clone)]
pub enum ParsedFilterLine {
    Rule(RsyncPattern),
    Merge(PathBuf),
    Clear,
}

/// Parse one `--filter` / filter-file line (comments already stripped).
pub fn parse_filter_line(line: &str) -> Result<Option<ParsedFilterLine>> {
    let line = strip_comment(line).trim();
    if line.is_empty() {
        return Ok(None);
    }
    if line == "!" || line.eq_ignore_ascii_case("clear") {
        return Ok(Some(ParsedFilterLine::Clear));
    }

    let (kind, rest) = split_rule_prefix(line);
    match kind {
        RuleKind::Merge => {
            let p = rest.trim();
            if p.is_empty() {
                return Err(Error::InvalidFilter {
                    rule: line.to_string(),
                    message: "merge path is empty".into(),
                });
            }
            Ok(Some(ParsedFilterLine::Merge(PathBuf::from(p))))
        }
        RuleKind::Include | RuleKind::Exclude => {
            let pat = rest.trim();
            if pat.is_empty() {
                return Err(Error::InvalidFilter {
                    rule: line.to_string(),
                    message: "pattern is empty".into(),
                });
            }
            Ok(Some(ParsedFilterLine::Rule(RsyncPattern::parse(
                pat,
                matches!(kind, RuleKind::Include),
            )?)))
        }
        RuleKind::Ignore => {
            tracing::debug!(line, "ignoring rsync protect/risk filter (not applicable)");
            Ok(None)
        }
        RuleKind::Bare => {
            // Lenient: a bare pattern in a filter list is an exclude (rsync
            // `--exclude` / `--exclude-from` form).
            Ok(Some(ParsedFilterLine::Rule(RsyncPattern::parse(
                line, false,
            )?)))
        }
    }
}

#[derive(Clone, Copy)]
enum RuleKind {
    Include,
    Exclude,
    Merge,
    Ignore,
    Bare,
}

fn split_rule_prefix(line: &str) -> (RuleKind, &str) {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return (RuleKind::Bare, line);
    }
    // Single-character prefixes: + - . : H S P R C
    let first = bytes[0] as char;
    let rest = if bytes.len() > 1 && (bytes[1] as char).is_whitespace() {
        line[1..].trim_start()
    } else {
        &line[1..]
    };
    match first {
        '+' => return (RuleKind::Include, rest),
        '-' => return (RuleKind::Exclude, rest),
        '.' | ':' => return (RuleKind::Merge, rest),
        'H' | 'h' if is_single_char_rule(line) => return (RuleKind::Exclude, rest),
        'S' | 's' if is_single_char_rule(line) => return (RuleKind::Include, rest),
        'P' | 'p' | 'R' | 'r' | 'C' | 'c' if is_single_char_rule(line) => {
            return (RuleKind::Ignore, rest);
        }
        _ => {}
    }

    let lower = line.to_ascii_lowercase();
    for (kw, kind) in [
        ("include ", RuleKind::Include),
        ("exclude ", RuleKind::Exclude),
        ("merge ", RuleKind::Merge),
        ("dir-merge ", RuleKind::Merge),
        ("hide ", RuleKind::Exclude),
        ("show ", RuleKind::Include),
        ("protect ", RuleKind::Ignore),
        ("risk ", RuleKind::Ignore),
    ] {
        if lower.starts_with(kw) {
            return (kind, &line[kw.len()..]);
        }
    }
    (RuleKind::Bare, line)
}

/// `H pattern` is a hide rule; `Hello` is a bare pattern.
fn is_single_char_rule(line: &str) -> bool {
    line.len() >= 2 && line.as_bytes()[1].is_ascii_whitespace()
}

fn strip_comment(line: &str) -> &str {
    let line = line.trim();
    if line.starts_with('#') || line.starts_with(';') {
        return "";
    }
    line
}

/// Load an rsync **filter** file (`--filter-from` / `merge`).
pub fn load_filter_file(path: &Path) -> Result<Vec<ParsedFilterLine>> {
    load_filter_file_inner(path, &mut HashSet::new())
}

fn load_filter_file_inner(
    path: &Path,
    visiting: &mut HashSet<PathBuf>,
) -> Result<Vec<ParsedFilterLine>> {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visiting.insert(canon.clone()) {
        return Err(Error::InvalidFilter {
            rule: path.display().to_string(),
            message: "recursive merge in filter file".into(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::FilterFileNotFound(path.to_path_buf())
        } else {
            Error::Other(format!("read filter file {}: {e}", path.display()))
        }
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let parsed = parse_filter_line(raw).map_err(|e| match e {
            Error::InvalidFilter { rule, message } => Error::InvalidFilter {
                rule,
                message: format!("{message} ({}:{})", path.display(), lineno + 1),
            },
            other => other,
        })?;
        match parsed {
            None => {}
            Some(ParsedFilterLine::Merge(rel)) => {
                let merged = if rel.is_absolute() {
                    rel
                } else {
                    base.join(rel)
                };
                out.extend(load_filter_file_inner(&merged, visiting)?);
            }
            Some(other) => out.push(other),
        }
    }
    visiting.remove(&canon);
    Ok(out)
}

/// Load `--exclude-from` / `--include-from` (one pattern per line, implicit side).
pub fn load_side_file(path: &Path, include: bool) -> Result<Vec<RsyncPattern>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::FilterFileNotFound(path.to_path_buf())
        } else {
            Error::Other(format!("read filter file {}: {e}", path.display()))
        }
    })?;
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        // Allow an explicit +/− prefix; otherwise the file's side wins.
        let (side, pat) = if let Some(rest) = line.strip_prefix("+ ") {
            (true, rest.trim())
        } else if let Some(rest) = line.strip_prefix("- ") {
            (false, rest.trim())
        } else if let Some(rest) = line.strip_prefix('+') {
            (true, rest.trim())
        } else if let Some(rest) = line.strip_prefix('-') {
            (false, rest.trim())
        } else {
            (include, line)
        };
        if pat.is_empty() {
            return Err(Error::InvalidFilter {
                rule: line.to_string(),
                message: format!("empty pattern in {} line {}", path.display(), lineno + 1),
            });
        }
        out.push(RsyncPattern::parse(pat, side)?);
    }
    Ok(out)
}

/// Map a simple rsync exclude to 7z `-x!` globs, if safe.
pub fn rsync_exclude_to_7z_globs(pattern: &str) -> Option<Vec<String>> {
    let p = pattern.trim();
    if p.is_empty() || p.contains('?') || p.contains('[') || p.contains('\\') {
        return None;
    }
    if p.ends_with("***") {
        let prefix = p.trim_end_matches('*').trim_end_matches('/');
        if prefix.is_empty() || prefix.contains('*') || prefix.contains("..") {
            return None;
        }
        return Some(vec![prefix.to_string(), format!("{prefix}/*")]);
    }
    // `*.ext` (basename glob) — 7z `*` also matches `/`.
    if let Some(ext) = p.strip_prefix("*.") {
        if !ext.is_empty()
            && ext
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Some(vec![format!("*.{ext}")]);
        }
    }
    // Directory prefix: `dir/` or `/dir/`
    let dir = p.trim_start_matches('/').trim_end_matches('/');
    if p.ends_with('/')
        && !dir.is_empty()
        && !dir.contains('*')
        && !dir.contains("..")
        && dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.')
    {
        return Some(vec![dir.to_string(), format!("{dir}/*")]);
    }
    // Exact basename `name` or `name.7z`
    if !p.contains('/')
        && !p.contains('*')
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Some(vec![p.to_string(), format!("*/{p}")]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exc(p: &str) -> RsyncPattern {
        RsyncPattern::parse(p, false).unwrap()
    }

    #[test]
    fn basename_star_ext() {
        let p = exc("*.tmp");
        assert!(p.matches("dir/foo.tmp", false));
        assert!(p.matches("foo.tmp", false));
        assert!(!p.matches("dir/foo.txt", false));
        assert!(!p.matches("tmp", false));
    }

    #[test]
    fn star_does_not_cross_slash() {
        let p = exc("foo*bar");
        assert!(p.matches("foobar", false));
        assert!(p.matches("fooXXXbar", false));
        assert!(!p.matches("foo/xx/bar", false));
    }

    #[test]
    fn globstar_any_depth() {
        let p = exc("**/foo.txt");
        assert!(p.matches("foo.txt", false));
        assert!(p.matches("a/foo.txt", false));
        assert!(p.matches("a/b/foo.txt", false));
        assert!(!p.matches("foo.bin", false));
    }

    #[test]
    fn globstar_under_dir() {
        let p = exc("secret/**");
        assert!(p.matches("secret/a", false));
        assert!(p.matches("secret/a/b.txt", false));
        assert!(!p.matches("secret", false));
        assert!(!p.matches("other/a", false));
    }

    #[test]
    fn triple_star_dir_and_contents() {
        let p = exc("secret/***");
        assert!(p.matches("secret", true));
        assert!(p.matches("secret/a", false));
        assert!(p.matches("secret/a/b", false));
        assert!(!p.matches("secret2", false));
        assert!(!p.matches("oseecret", false));
    }

    #[test]
    fn trailing_slash_dirs_only() {
        let p = exc("build/");
        assert!(p.matches("build", true));
        assert!(p.matches("sub/build", true)); // basename `build`
        assert!(!p.matches("build", false));
        assert!(!p.matches("build/out", false));
    }

    #[test]
    fn anchored_root() {
        let p = exc("/skip.7z");
        assert!(p.matches("skip.7z", false));
        assert!(!p.matches("nested/skip.7z", false));
    }

    #[test]
    fn full_path_pattern() {
        let p = exc("nested/skip.7z");
        assert!(p.matches("nested/skip.7z", false));
        assert!(!p.matches("skip.7z", false));
        assert!(!p.matches("other/skip.7z", false));
    }

    #[test]
    fn question_and_class() {
        let p = exc("file.t?t");
        assert!(p.matches("file.txt", false));
        assert!(p.matches("file.tat", false));
        assert!(!p.matches("file.tt", false));
        let c = exc("file.[abc]");
        assert!(c.matches("file.a", false));
        assert!(!c.matches("file.d", false));
        let n = exc("file.[!abc]");
        assert!(n.matches("file.d", false));
        assert!(!n.matches("file.a", false));
    }

    #[test]
    fn parse_filter_prefixes() {
        match parse_filter_line("+ *.txt").unwrap() {
            Some(ParsedFilterLine::Rule(r)) => assert!(r.include),
            _ => panic!("expected include"),
        }
        match parse_filter_line("- secret/").unwrap() {
            Some(ParsedFilterLine::Rule(r)) => assert!(!r.include),
            _ => panic!("expected exclude"),
        }
        match parse_filter_line("include *.7z").unwrap() {
            Some(ParsedFilterLine::Rule(r)) => assert!(r.include),
            _ => panic!("expected include"),
        }
        assert!(matches!(
            parse_filter_line("!").unwrap(),
            Some(ParsedFilterLine::Clear)
        ));
        assert!(parse_filter_line("# comment").unwrap().is_none());
        assert!(parse_filter_line("").unwrap().is_none());
    }

    #[test]
    fn filter_file_merge_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let merged = dir.path().join("extra.rules");
        std::fs::write(&merged, "- *.bak\n").unwrap();
        let main = dir.path().join("main.rules");
        std::fs::write(
            &main,
            "# header\n; also\n+ *.txt\nmerge extra.rules\n- *\n",
        )
        .unwrap();
        let lines = load_filter_file(&main).unwrap();
        assert_eq!(lines.len(), 3);
        match &lines[0] {
            ParsedFilterLine::Rule(r) => assert!(r.include && r.original == "*.txt"),
            _ => panic!(),
        }
        match &lines[1] {
            ParsedFilterLine::Rule(r) => assert!(!r.include && r.original == "*.bak"),
            _ => panic!(),
        }
        match &lines[2] {
            ParsedFilterLine::Rule(r) => assert!(!r.include && r.original == "*"),
            _ => panic!(),
        }
    }

    #[test]
    fn missing_filter_file_errors() {
        let err = load_filter_file(Path::new("/no/such/filter.rules")).unwrap_err();
        assert!(matches!(err, Error::FilterFileNotFound(_)));
    }

    #[test]
    fn maps_simple_excludes_to_7z() {
        assert!(rsync_exclude_to_7z_globs("*.tmp")
            .unwrap()
            .iter()
            .any(|g| g == "*.tmp"));
        assert!(rsync_exclude_to_7z_globs("__MACOSX/")
            .unwrap()
            .iter()
            .any(|g| g.contains("__MACOSX")));
        assert!(rsync_exclude_to_7z_globs("foo.*bar").is_none());
    }
}
