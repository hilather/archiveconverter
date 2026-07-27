//! Path normalization for stable regex matching across platforms.

use std::path::{Component, Path};

/// Normalize archive member paths to use `/` separators and strip leading `./`.
pub fn normalize_member_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./");
    // Collapse repeated slashes except we don't preserve UNC etc. for archive paths
    let mut out = String::with_capacity(path.len());
    let mut prev_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    out.trim_start_matches('/').to_string()
}

/// Basename of a normalized member path.
pub fn member_basename(path: &str) -> String {
    let norm = normalize_member_path(path);
    norm.rsplit('/').next().unwrap_or(&norm).to_string()
}

/// Join path components with `/` for archive membership.
pub fn join_member(parent: &str, child: &str) -> String {
    let parent = normalize_member_path(parent);
    let child = normalize_member_path(child);
    if parent.is_empty() {
        child
    } else if child.is_empty() {
        parent
    } else {
        format!("{parent}/{child}")
    }
}

/// Ensure a relative path does not escape the extraction root.
pub fn is_safe_member_path(path: &str) -> bool {
    let raw = path.replace('\\', "/");
    // Reject absolute paths before normalization strips the root.
    if raw.starts_with('/') || raw.chars().nth(1) == Some(':') {
        return false;
    }
    let norm = normalize_member_path(path);
    if norm.is_empty() {
        return false;
    }
    let p = Path::new(&norm);
    for c in p.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_and_dots() {
        assert_eq!(normalize_member_path(r".\foo\bar.7z"), "foo/bar.7z");
        assert_eq!(normalize_member_path("foo//bar"), "foo/bar");
        assert_eq!(normalize_member_path("/foo/bar"), "foo/bar");
    }

    #[test]
    fn basename_works() {
        assert_eq!(member_basename("a/b/c.7z"), "c.7z");
        assert_eq!(member_basename("c.7z"), "c.7z");
    }

    #[test]
    fn rejects_path_escape() {
        assert!(!is_safe_member_path("../evil"));
        assert!(!is_safe_member_path("/abs"));
        assert!(is_safe_member_path("ok/file.txt"));
    }
}
