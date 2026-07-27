//! Ordered regex rewrite rules for archive member names.

use crate::error::{Error, Result};
use crate::util::pathnorm::normalize_member_path;
use regex::Regex;
use std::collections::HashMap;

/// One rename rule: apply `replacement` when `pattern` matches.
///
/// Replacement supports Rust regex capture expansion (`$1`, `$name`, `${name}`).
#[derive(Debug, Clone)]
pub struct RenameRule {
    pub pattern: Regex,
    pub replacement: String,
}

impl RenameRule {
    pub fn new(pattern: &str, replacement: &str) -> Result<Self> {
        let pattern = Regex::new(pattern).map_err(|source| Error::InvalidRegex {
            pattern: pattern.to_string(),
            source,
        })?;
        Ok(Self {
            pattern,
            replacement: replacement.to_string(),
        })
    }

    /// Parse `PATTERN=REPLACEMENT` (first `=` separates pattern from replacement).
    pub fn parse_pair(spec: &str) -> Result<Self> {
        let (pat, rep) = spec
            .split_once('=')
            .ok_or_else(|| Error::Other(format!("rename rule must be PATTERN=REPL, got: {spec}")))?;
        if pat.is_empty() {
            return Err(Error::Other(
                "rename pattern must not be empty".to_string(),
            ));
        }
        Self::new(pat, rep)
    }

    pub fn apply(&self, name: &str) -> String {
        self.pattern
            .replace_all(name, self.replacement.as_str())
            .into_owned()
    }
}

/// Ordered list of rename rules applied left-to-right.
#[derive(Debug, Clone, Default)]
pub struct NameTransformer {
    rules: Vec<RenameRule>,
}

impl NameTransformer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, rule: RenameRule) {
        self.rules.push(rule);
    }

    pub fn add(&mut self, pattern: &str, replacement: &str) -> Result<()> {
        self.push(RenameRule::new(pattern, replacement)?);
        Ok(())
    }

    pub fn from_pairs<I, S>(specs: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut t = Self::new();
        for s in specs {
            t.push(RenameRule::parse_pair(s.as_ref())?);
        }
        Ok(t)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Apply all rules in order to a member path (normalized).
    pub fn transform(&self, path: &str) -> String {
        let mut name = normalize_member_path(path);
        for rule in &self.rules {
            name = rule.apply(&name);
        }
        normalize_member_path(&name)
    }

    /// Transform many names; error if two distinct inputs map to the same output.
    pub fn transform_all<'a, I>(&self, names: I) -> Result<HashMap<String, String>>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut map = HashMap::new();
        let mut inverse: HashMap<String, String> = HashMap::new();
        for original in names {
            let orig_norm = normalize_member_path(original);
            let dest = self.transform(&orig_norm);
            if dest.is_empty() {
                return Err(Error::Other(format!(
                    "rename of '{orig_norm}' produced empty path"
                )));
            }
            if let Some(prev) = inverse.get(&dest) {
                if prev != &orig_norm {
                    return Err(Error::NameCollision(dest));
                }
            }
            inverse.insert(dest.clone(), orig_norm.clone());
            map.insert(orig_norm, dest);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_old_suffix() {
        let t = NameTransformer::from_pairs([r"^(?P<base>.+)_old\.7z$=${base}.7z"]).unwrap();
        assert_eq!(t.transform("data_old.7z"), "data.7z");
        assert_eq!(t.transform("keep.7z"), "keep.7z");
    }

    #[test]
    fn whitespace_to_underscore() {
        let mut t = NameTransformer::new();
        t.add(r"\s+", "_").unwrap();
        assert_eq!(t.transform("my archive.7z"), "my_archive.7z");
    }

    #[test]
    fn ordered_rules() {
        let t = NameTransformer::from_pairs([r"_old\.7z$=.7z", r"\s+=_"]).unwrap();
        assert_eq!(t.transform("x_old.7z"), "x.7z");
    }

    #[test]
    fn collision_detected() {
        let mut t = NameTransformer::new();
        t.add(r".*", "same.7z").unwrap();
        let err = t.transform_all(["a.7z", "b.7z"]).unwrap_err();
        assert!(matches!(err, Error::NameCollision(_)));
    }

    #[test]
    fn parse_pair_requires_eq() {
        assert!(RenameRule::parse_pair("noreplace").is_err());
    }
}
