//! Member include/exclude filtering via regular expressions.

mod rename;

pub use rename::{NameTransformer, RenameRule};

use crate::error::{Error, Result};
use crate::util::pathnorm::normalize_member_path;
use regex::Regex;

/// A set of exclusion (and optional inclusion) regex patterns for archive paths.
#[derive(Debug, Clone, Default)]
pub struct MemberFilter {
    /// Original exclude pattern strings (for 7z `-x!` mapping).
    exclude_patterns: Vec<String>,
    excludes: Vec<Regex>,
    include_patterns: Vec<String>,
    includes: Vec<Regex>,
    /// When true, match against basename only instead of full normalized path.
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
        self.excludes.push(compile(pattern)?);
        self.exclude_patterns.push(pattern.to_string());
        Ok(())
    }

    pub fn add_include(&mut self, pattern: &str) -> Result<()> {
        self.includes.push(compile(pattern)?);
        self.include_patterns.push(pattern.to_string());
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

    fn match_target(&self, path: &str) -> String {
        let norm = normalize_member_path(path);
        if self.basename_only {
            norm.rsplit('/').next().unwrap_or(&norm).to_string()
        } else {
            norm
        }
    }

    /// Returns true if the member should be kept (not excluded).
    pub fn should_keep(&self, path: &str) -> bool {
        let target = self.match_target(path);
        if self.excludes.iter().any(|re| re.is_match(&target)) {
            return false;
        }
        if !self.includes.is_empty() && !self.includes.iter().any(|re| re.is_match(&target)) {
            return false;
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.excludes.is_empty() && self.includes.is_empty()
    }

    /// Try to map exclude regexes to 7z `-x!` wildcards.
    ///
    /// Returns `None` if any pattern cannot be mapped safely (caller should
    /// fall back to extract-all + post-filter). Includes are not supported via
    /// 7z switches here.
    pub fn try_7z_exclude_globs(&self) -> Option<Vec<String>> {
        if !self.includes.is_empty() {
            return None;
        }
        if self.exclude_patterns.is_empty() {
            return Some(Vec::new());
        }
        let mut globs = Vec::new();
        for p in &self.exclude_patterns {
            globs.extend(regex_to_7z_globs(p, self.basename_only)?);
        }
        Some(globs)
    }
}

/// Map a common exclude regex to one or more 7z wildcard patterns.
fn regex_to_7z_globs(pattern: &str, basename_only: bool) -> Option<Vec<String>> {
    let p = pattern.trim();
    // (?i)\.ext$  →  *.ext  (and * .EXT if we care; 7z on Linux is case-sensitive)
    if let Some(ext) = strip_suffix_ext(p) {
        if basename_only {
            return Some(vec![format!("*.{ext}")]);
        }
        return Some(vec![format!("*.{ext}"), format!("*/*.{ext}")]);
    }
    // ^prefix/ or ^prefix/
    if let Some(rest) = p.strip_prefix('^') {
        let rest = rest.strip_suffix('$').unwrap_or(rest);
        // Simple path prefix: __MACOSX/ or foo/bar/
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
    // (?i)\.tmp$ or \.tmp$
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
    // only escaped dots and alnum/_-
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
    fn include_only_txt() {
        let mut f = MemberFilter::new();
        f.add_include(r"\.txt$").unwrap();
        assert!(f.should_keep("a.txt"));
        assert!(!f.should_keep("a.bin"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let mut f = MemberFilter::new();
        f.add_include(r".*").unwrap();
        f.add_exclude(r"secret").unwrap();
        assert!(!f.should_keep("secret.txt"));
        assert!(f.should_keep("public.txt"));
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
}
