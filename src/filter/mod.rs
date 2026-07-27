//! Member include/exclude filtering via regular expressions.

mod rename;

pub use rename::{NameTransformer, RenameRule};

use crate::error::{Error, Result};
use crate::util::pathnorm::normalize_member_path;
use regex::Regex;

/// A set of exclusion (and optional inclusion) regex patterns for archive paths.
#[derive(Debug, Clone, Default)]
pub struct MemberFilter {
    excludes: Vec<Regex>,
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
        Ok(())
    }

    pub fn add_include(&mut self, pattern: &str) -> Result<()> {
        self.includes.push(compile(pattern)?);
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

    fn match_target<'a>(&self, path: &'a str) -> String {
        let norm = normalize_member_path(path);
        if self.basename_only {
            norm.rsplit('/').next().unwrap_or(&norm).to_string()
        } else {
            norm
        }
    }

    /// Returns true if the member should be kept (not excluded).
    ///
    /// Rules:
    /// 1. If any exclude matches → drop.
    /// 2. If includes is non-empty and none match → drop.
    /// 3. Otherwise keep.
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
}
