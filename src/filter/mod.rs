//! Member include/exclude filtering: regex and rsync-style rules.

mod rename;
mod rsync;

pub use rename::{NameTransformer, RenameRule};
pub use rsync::{
    load_filter_file, load_side_file, parse_filter_line, rsync_exclude_to_7z_globs,
    ParsedFilterLine, RsyncPattern,
};

use crate::error::{Error, Result};
use crate::util::pathnorm::normalize_member_path;
use regex::Regex;
use std::path::Path;

/// One ordered include/exclude rule (first match wins).
#[derive(Debug, Clone)]
enum FilterRule {
    Regex {
        include: bool,
        original: String,
        re: Regex,
    },
    Rsync(RsyncPattern),
}

impl FilterRule {
    fn include(&self) -> bool {
        match self {
            FilterRule::Regex { include, .. } => *include,
            FilterRule::Rsync(p) => p.include,
        }
    }

    fn is_rsync(&self) -> bool {
        matches!(self, FilterRule::Rsync(_))
    }

    fn matches(&self, path: &str, is_dir: bool, basename_only: bool) -> bool {
        match self {
            FilterRule::Regex { re, .. } => {
                let target = regex_target(path, basename_only);
                re.is_match(&target)
            }
            FilterRule::Rsync(p) => p.matches(path, is_dir),
        }
    }
}

fn regex_target(path: &str, basename_only: bool) -> String {
    let norm = normalize_member_path(path);
    if basename_only {
        norm.rsplit('/').next().unwrap_or(&norm).to_string()
    } else {
        norm
    }
}

/// Ordered include/exclude rules for archive member paths.
///
/// Evaluation follows **rsync filter semantics**:
///
/// 1. Rules are checked in insertion order; the **first match wins**.
/// 2. If no rule matches, the path is **kept** (rsync default include).
/// 3. Rsync directory excludes prune children (a `- dir/` rule drops `dir/a`).
/// 4. Regex `--exclude-*` / `--include-*` participate in the same ordered list.
///
/// An include-only list does **not** drop non-matching files (same as rsync).
/// To keep only `*.txt`, use `+ *.txt` then `- *` (or the equivalent regex pair).
#[derive(Debug, Clone, Default)]
pub struct MemberFilter {
    rules: Vec<FilterRule>,
    /// When true, regex rules match the basename only. Rsync rules keep their
    /// own path-vs-basename rules (a `/` in the pattern selects full-path match).
    basename_only: bool,
}

impl MemberFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn basename_only(mut self, yes: bool) -> Self {
        self.basename_only = yes;
        self
    }

    pub fn add_exclude(&mut self, pattern: &str) -> Result<()> {
        self.rules.push(FilterRule::Regex {
            include: false,
            original: pattern.to_string(),
            re: compile(pattern)?,
        });
        Ok(())
    }

    pub fn add_include(&mut self, pattern: &str) -> Result<()> {
        self.rules.push(FilterRule::Regex {
            include: true,
            original: pattern.to_string(),
            re: compile(pattern)?,
        });
        Ok(())
    }

    pub fn add_rsync(&mut self, pattern: RsyncPattern) {
        self.rules.push(FilterRule::Rsync(pattern));
    }

    /// Parse one `--filter` line (`+ pat`, `- pat`, bare pat = exclude).
    pub fn add_filter_line(&mut self, line: &str) -> Result<()> {
        match parse_filter_line(line)? {
            None => Ok(()),
            Some(ParsedFilterLine::Clear) => {
                self.rules.clear();
                Ok(())
            }
            Some(ParsedFilterLine::Merge(_)) => Err(Error::InvalidFilter {
                rule: line.to_string(),
                message: "merge is only valid inside a filter file (use --filter-from)".into(),
            }),
            Some(ParsedFilterLine::Rule(p)) => {
                self.add_rsync(p);
                Ok(())
            }
        }
    }

    /// Append rules from an rsync filter file (`--filter-from`).
    pub fn add_filter_from(&mut self, path: &Path) -> Result<()> {
        for line in load_filter_file(path)? {
            match line {
                ParsedFilterLine::Clear => self.rules.clear(),
                ParsedFilterLine::Merge(_) => {
                    // load_filter_file already inlines merges.
                }
                ParsedFilterLine::Rule(p) => self.add_rsync(p),
            }
        }
        Ok(())
    }

    /// `--exclude-from` (one exclude pattern per line).
    pub fn add_exclude_from(&mut self, path: &Path) -> Result<()> {
        for p in load_side_file(path, false)? {
            self.add_rsync(p);
        }
        Ok(())
    }

    /// `--include-from` (one include pattern per line).
    pub fn add_include_from(&mut self, path: &Path) -> Result<()> {
        for p in load_side_file(path, true)? {
            self.add_rsync(p);
        }
        Ok(())
    }

    pub fn with_excludes<I, S>(patterns: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut f = Self::new();
        for p in patterns {
            f.add_exclude(p.as_ref())?;
        }
        Ok(f)
    }

    pub fn with_includes<I, S>(patterns: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut f = Self::new();
        for p in patterns {
            f.add_include(p.as_ref())?;
        }
        Ok(f)
    }

    /// Build from CLI pieces. Order: filter-from, `--filter` lines, include-from,
    /// exclude-from, then regex excludes (and optional regex includes).
    pub fn from_cli(
        filter_from: &[std::path::PathBuf],
        filter_lines: &[String],
        include_from: &[std::path::PathBuf],
        exclude_from: &[std::path::PathBuf],
        regex_includes: &[String],
        regex_excludes: &[String],
        basename_only: bool,
    ) -> Result<Self> {
        let mut f = Self::new();
        for p in filter_from {
            f.add_filter_from(p)?;
        }
        for line in filter_lines {
            f.add_filter_line(line)?;
        }
        for p in include_from {
            f.add_include_from(p)?;
        }
        for p in exclude_from {
            f.add_exclude_from(p)?;
        }
        for p in regex_includes {
            f.add_include(p)?;
        }
        for p in regex_excludes {
            f.add_exclude(p)?;
        }
        if basename_only {
            f = f.basename_only(true);
        }
        Ok(f)
    }

    /// First matching rule for this exact path (no ancestor walk).
    fn first_match(&self, path: &str, is_dir: bool) -> Option<bool> {
        for rule in &self.rules {
            if rule.matches(path, is_dir, self.basename_only) {
                return Some(rule.include());
            }
        }
        None
    }

    /// Whether a **file** member should be kept.
    pub fn should_keep(&self, path: &str) -> bool {
        self.should_keep_path(path, false)
    }

    /// Rsync-style keep decision for a file or directory.
    ///
    /// Parent directories are checked as directories first: if a parent would
    /// be excluded, the child is dropped (rsync does not recurse).
    pub fn should_keep_path(&self, path: &str, is_dir: bool) -> bool {
        let norm = normalize_member_path(path);
        if norm.is_empty() {
            return false;
        }
        if self.has_rsync() {
            let parts: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
            let end = if is_dir {
                parts.len()
            } else {
                parts.len().saturating_sub(1)
            };
            let mut prefix = String::new();
            for (i, part) in parts.iter().enumerate() {
                if i >= end {
                    break;
                }
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if let Some(false) = self.first_rsync_match(&prefix, true) {
                    return false;
                }
            }
        }
        self.first_match(&norm, is_dir).unwrap_or(true)
    }

    fn has_rsync(&self) -> bool {
        self.rules.iter().any(|r| r.is_rsync())
    }

    fn first_rsync_match(&self, path: &str, is_dir: bool) -> Option<bool> {
        for rule in &self.rules {
            if let FilterRule::Rsync(p) = rule {
                if p.matches(path, is_dir) {
                    return Some(p.include);
                }
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Try to map exclude rules to 7z `-x!` wildcards.
    ///
    /// Returns `None` if any include is present or a pattern cannot be mapped
    /// safely (caller should extract-all + post-filter).
    pub fn try_7z_exclude_globs(&self) -> Option<Vec<String>> {
        if self.rules.iter().any(|r| r.include()) {
            return None;
        }
        if self.rules.is_empty() {
            return Some(Vec::new());
        }
        let mut globs = Vec::new();
        for rule in &self.rules {
            match rule {
                FilterRule::Regex { original, .. } => {
                    globs.extend(regex_to_7z_globs(original, self.basename_only)?);
                }
                FilterRule::Rsync(p) => {
                    globs.extend(rsync_exclude_to_7z_globs(&p.original)?);
                }
            }
        }
        Some(globs)
    }
}

/// Map a common exclude regex to one or more 7z wildcard patterns.
fn regex_to_7z_globs(pattern: &str, basename_only: bool) -> Option<Vec<String>> {
    let p = pattern.trim();
    // (?i)\.ext$  →  *.ext
    if let Some(ext) = strip_suffix_ext(p) {
        if basename_only {
            return Some(vec![format!("*.{ext}")]);
        }
        return Some(vec![format!("*.{ext}"), format!("*/*.{ext}")]);
    }
    // ^prefix/
    if let Some(rest) = p.strip_prefix('^') {
        let rest = rest.strip_suffix('$').unwrap_or(rest);
        if is_simple_path_prefix(rest) {
            let prefix = rest.trim_end_matches('/');
            return Some(vec![
                format!("{prefix}"),
                format!("{prefix}/*"),
                format!("{prefix}/*/*"),
            ]);
        }
    }
    // Exact basename: ^name\.7z$
    if let Some(name) = strip_exact_name(p) {
        if basename_only {
            return Some(vec![name]);
        }
        return Some(vec![name.clone(), format!("*/{name}")]);
    }
    None
}

fn strip_suffix_ext(p: &str) -> Option<String> {
    let p = p.strip_prefix("(?i)").unwrap_or(p);
    let p = p.strip_prefix("(?-i)").unwrap_or(p);
    if let Some(rest) = p.strip_prefix(r"\.") {
        let rest = rest.strip_suffix('$')?;
        if rest.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Some(rest.to_string());
        }
    }
    None
}

fn strip_exact_name(p: &str) -> Option<String> {
    let p = p.strip_prefix('^')?;
    let p = p.strip_suffix('$')?;
    let mut out = String::new();
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let n = chars.next()?;
            if n == '.' {
                out.push('.');
            } else {
                return None;
            }
        } else if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            return None;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn is_simple_path_prefix(p: &str) -> bool {
    !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.')
        && !p.contains("..")
}

fn compile(pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|source| Error::InvalidRegex {
        pattern: pattern.to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn exclude_tmp_files() {
        let f = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
        assert!(!f.should_keep("dir/foo.TMP"));
        assert!(f.should_keep("dir/foo.txt"));
    }

    #[test]
    fn exclude_macosx() {
        let f = MemberFilter::with_excludes([r"^__MACOSX/"]).unwrap();
        assert!(!f.should_keep("__MACOSX/._x"));
        assert!(f.should_keep("data/x"));
    }

    #[test]
    fn include_only_does_not_exclude_others() {
        // Rsync default: unmatched paths are kept. Include-only is not a whitelist.
        let mut f = MemberFilter::new();
        f.add_include(r"\.txt$").unwrap();
        assert!(f.should_keep("a.txt"));
        assert!(f.should_keep("a.bin"));
    }

    #[test]
    fn first_match_wins_exclude_then_include() {
        let mut f = MemberFilter::new();
        f.add_exclude(r"secret").unwrap();
        f.add_include(r".*").unwrap();
        assert!(!f.should_keep("secret.txt"));
        assert!(f.should_keep("public.txt"));
    }

    #[test]
    fn first_match_wins_include_then_exclude() {
        let mut f = MemberFilter::new();
        f.add_include(r"secret").unwrap();
        f.add_exclude(r".*").unwrap();
        assert!(f.should_keep("secret.txt"));
        assert!(!f.should_keep("public.txt"));
    }

    #[test]
    fn basename_mode() {
        let f = MemberFilter::with_excludes([r"^skip\.7z$"])
            .unwrap()
            .basename_only(true);
        assert!(!f.should_keep("nested/skip.7z"));
        assert!(f.should_keep("nested/keep.7z"));
    }

    #[test]
    fn invalid_regex_errors() {
        let err = MemberFilter::with_excludes([r"("]).unwrap_err();
        assert!(matches!(err, Error::InvalidRegex { .. }));
    }

    #[test]
    fn maps_tmp_and_macosx_to_7z_globs() {
        let f = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
        let g = f.try_7z_exclude_globs().unwrap();
        assert!(g.iter().any(|x| x.contains(".tmp")));
        assert!(g.iter().any(|x| x.contains("__MACOSX")));
    }

    #[test]
    fn complex_regex_not_mapped() {
        let f = MemberFilter::with_excludes([r"foo.*bar\d+"]).unwrap();
        assert!(f.try_7z_exclude_globs().is_none());
    }

    #[test]
    fn rsync_include_txt_exclude_rest() {
        let mut f = MemberFilter::new();
        f.add_filter_line("+ *.txt").unwrap();
        f.add_filter_line("+ */").unwrap();
        f.add_filter_line("- *").unwrap();
        assert!(f.should_keep("a.txt"));
        assert!(f.should_keep("dir/a.txt"));
        assert!(!f.should_keep("a.bin"));
        assert!(!f.should_keep("dir/a.bin"));
    }

    #[test]
    fn rsync_dir_exclude_prunes_children() {
        let mut f = MemberFilter::new();
        f.add_filter_line("- secret/").unwrap();
        assert!(!f.should_keep("secret/x.txt"));
        assert!(f.should_keep("public/x.txt"));
        // A later include cannot resurrect children of an excluded dir
        // unless it matches the directory itself first (rsync walk).
        let mut f2 = MemberFilter::new();
        f2.add_filter_line("- secret/").unwrap();
        f2.add_filter_line("+ secret/keep.txt").unwrap();
        assert!(
            !f2.should_keep("secret/keep.txt"),
            "excluded parent dir prunes children"
        );
        let mut f3 = MemberFilter::new();
        f3.add_filter_line("+ secret/keep.txt").unwrap();
        f3.add_filter_line("+ secret/").unwrap();
        f3.add_filter_line("- *").unwrap();
        assert!(f3.should_keep("secret/keep.txt"));
        assert!(!f3.should_keep("secret/drop.bin"));
    }

    #[test]
    fn rsync_filter_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        fs::write(&rules, "+ *.7z\n+ */\n- *\n").unwrap();
        let mut f = MemberFilter::new();
        f.add_filter_from(&rules).unwrap();
        assert!(f.should_keep("alpha.7z"));
        assert!(f.should_keep("nested/beta.7z"));
        assert!(!f.should_keep("readme.txt"));
    }

    #[test]
    fn exclude_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("ex");
        fs::write(&rules, "skip_me.7z\n*.tmp\n").unwrap();
        let mut f = MemberFilter::new();
        f.add_exclude_from(&rules).unwrap();
        assert!(!f.should_keep("skip_me.7z"));
        assert!(!f.should_keep("dir/foo.tmp"));
        assert!(f.should_keep("keep.7z"));
    }

    #[test]
    fn includes_block_7z_glob_fast_path() {
        let mut f = MemberFilter::new();
        f.add_filter_line("+ *.txt").unwrap();
        f.add_filter_line("- *").unwrap();
        assert!(f.try_7z_exclude_globs().is_none());
    }
}
