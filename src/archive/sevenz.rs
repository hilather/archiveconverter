//! 7-Zip CLI backend (`7zz` / `7z` / `7za`).

use super::detect::format_from_path;
use super::{ArchiveBackend, ArchiveFormat, EntryMeta, PackOptions};
use crate::error::{Error, Result};
use crate::util::pathnorm::normalize_member_path;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Resolve 7z executable from PATH (prefers 7zz, then 7z, then 7za).
pub fn find_7z_binary() -> Result<PathBuf> {
    for name in ["7zz", "7z", "7za"] {
        if let Ok(path) = which::which(name) {
            return Ok(path);
        }
    }
    // Common user-local install from our setup scripts
    let home_local = dirs_fallback_local_7z();
    for p in home_local {
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(Error::BackendMissing(
        "no 7zz/7z/7za found on PATH; install p7zip or 7-Zip".into(),
    ))
}

fn dirs_fallback_local_7z() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        v.push(PathBuf::from(&home).join(".local/bin/7zz"));
        v.push(PathBuf::from(&home).join(".local/bin/7z"));
    }
    v
}

#[derive(Debug, Clone)]
pub struct SevenZCli {
    binary: PathBuf,
}

impl SevenZCli {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }

    pub fn discover() -> Result<Self> {
        Ok(Self::new(find_7z_binary()?))
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn version_line(&self) -> Result<String> {
        let output = self.run_raw(&[])?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .lines()
            .next()
            .unwrap_or("7z (unknown version)")
            .to_string())
    }

    fn run_raw(&self, args: &[&str]) -> Result<Output> {
        let output = Command::new(&self.binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::BackendFailed(format!("failed to spawn {:?}: {e}", self.binary)))?;
        Ok(output)
    }

    fn run_checked(&self, args: &[&str]) -> Result<Output> {
        let output = self.run_raw(args)?;
        // 7z exit codes: 0 OK, 1 warning, 2 fatal, ...
        if output.status.code().unwrap_or(2) > 1 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(Error::BackendFailed(format!(
                "7z {:?} failed (code {:?}): {stderr}{stdout}",
                args,
                output.status.code()
            )));
        }
        Ok(output)
    }

    /// Parse `7z l -slt` technical listing (richer, heavier).
    pub fn list_technical(&self, archive: &Path) -> Result<Vec<EntryMeta>> {
        let archive_s = archive.to_string_lossy();
        let output = self.run_checked(&["l", "-slt", "-ba", archive_s.as_ref()])?;
        let text = String::from_utf8_lossy(&output.stdout);
        parse_slt_listing(&text)
    }

    /// Cheaper listing via `7z l -ba` (name + size; good enough for filters/stats).
    pub fn list_basic(&self, archive: &Path) -> Result<Vec<EntryMeta>> {
        let archive_s = archive.to_string_lossy();
        let output = self.run_checked(&["l", "-ba", archive_s.as_ref()])?;
        let text = String::from_utf8_lossy(&output.stdout);
        parse_ba_listing(&text)
    }

    /// Archive-level properties (`7z l -slt` without `-ba` so the archive header is present).
    pub fn archive_is_solid(&self, archive: &Path) -> Result<bool> {
        let archive_s = archive.to_string_lossy();
        let output = self.run_checked(&["l", "-slt", archive_s.as_ref()])?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_archive_solid(&text))
    }
}

impl ArchiveBackend for SevenZCli {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::SevenZ
    }

    fn list(&self, archive: &Path) -> Result<Vec<EntryMeta>> {
        // Prefer compact listing; fall back to technical if parse yields nothing.
        match self.list_basic(archive) {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => self.list_technical(archive),
            Err(_) => self.list_technical(archive),
        }
    }

    fn is_solid(&self, archive: &Path) -> Result<bool> {
        self.archive_is_solid(archive)
    }

    fn extract_members(&self, archive: &Path, members: &[&str], dest_dir: &Path) -> Result<()> {
        if members.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(dest_dir)?;
        let archive_s = archive.to_string_lossy().into_owned();
        let out_s = format!("-o{}", dest_dir.display());
        // One 7z invocation: solid archives decode the solid stream once.
        let mut args: Vec<String> = vec![
            "x".into(),
            "-y".into(),
            out_s,
            archive_s,
        ];
        for m in members {
            args.push((*m).to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run_checked(&arg_refs)?;
        Ok(())
    }

    fn extract_member(&self, archive: &Path, member: &str, dest_file: &Path) -> Result<()> {
        if let Some(parent) = dest_file.parent() {
            fs::create_dir_all(parent)?;
        }
        // Stream a single member to the destination via stdout (-so). Avoids a
        // second temp tree + full-file copy (important for multi‑GB nested .7z).
        let archive_s = archive.to_string_lossy();
        let status = Command::new(&self.binary)
            .args(["e", "-so", "-y", archive_s.as_ref(), member])
            .stdout(Stdio::from(
                fs::File::create(dest_file).map_err(Error::Io)?,
            ))
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| Error::BackendFailed(format!("failed to spawn 7z extract -so: {e}")))?;

        if status.code().unwrap_or(2) <= 1 {
            // Success or warning; ensure we got some bytes (empty members are valid).
            if dest_file.is_file() {
                return Ok(());
            }
        }

        // Fallback: extract to temp dir then copy (handles odd paths / older 7z).
        let _ = fs::remove_file(dest_file);
        let tmp = tempfile::tempdir().map_err(Error::Io)?;
        let out = tmp.path();
        let out_s = format!("-o{}", out.display());
        self.run_checked(&["x", "-y", out_s.as_str(), archive_s.as_ref(), member])?;

        let extracted = out.join(member);
        let extracted = if extracted.exists() {
            extracted
        } else {
            let alt = out.join(normalize_member_path(member));
            if alt.exists() {
                alt
            } else {
                find_extracted_file(out, member)?
            }
        };
        fs::copy(&extracted, dest_file)?;
        Ok(())
    }

    fn extract_all(&self, archive: &Path, dest_dir: &Path) -> Result<()> {
        fs::create_dir_all(dest_dir)?;
        let archive_s = archive.to_string_lossy();
        let out_s = format!("-o{}", dest_dir.display());
        self.run_checked(&["x", "-y", out_s.as_str(), archive_s.as_ref()])?;
        Ok(())
    }

    fn extract_all_with_excludes(
        &self,
        archive: &Path,
        dest_dir: &Path,
        exclude_globs: &[String],
    ) -> Result<()> {
        if exclude_globs.is_empty() {
            return self.extract_all(archive, dest_dir);
        }
        fs::create_dir_all(dest_dir)?;
        let archive_s = archive.to_string_lossy().into_owned();
        let out_s = format!("-o{}", dest_dir.display());
        let mut args: Vec<String> = vec!["x".into(), "-y".into(), out_s, archive_s];
        for g in exclude_globs {
            args.push(format!("-x!{g}"));
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run_checked(&arg_refs)?;
        Ok(())
    }

    fn pack_dir(&self, src_dir: &Path, dest_archive: &Path, opts: &PackOptions) -> Result<()> {
        if let Some(parent) = dest_archive.parent() {
            fs::create_dir_all(parent)?;
        }
        if dest_archive.exists() {
            fs::remove_file(dest_archive)?;
        }

        let dest_s = dest_archive.to_string_lossy();
        let mut args: Vec<String> = vec![
            "a".into(),
            "-t7z".into(),
            format!("-mx={}", opts.level),
            "-y".into(),
        ];
        if opts.non_solid {
            args.push("-ms=off".into());
        }
        if let Some(t) = opts.threads {
            args.push(format!("-mmt={t}"));
        } else {
            args.push("-mmt=on".into());
        }
        args.push(dest_s.into_owned());
        // Pack contents of src_dir (not the directory node itself as sole root if we cd)
        // Using `.` relative: run with current_dir = src_dir
        args.push(".".into());

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = Command::new(&self.binary)
            .args(&arg_refs)
            .current_dir(src_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::BackendFailed(format!("failed to spawn 7z pack: {e}")))?;

        if output.status.code().unwrap_or(2) > 1 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(Error::BackendFailed(format!(
                "7z pack failed: {stderr}{stdout}"
            )));
        }
        if !dest_archive.is_file() {
            return Err(Error::BackendFailed(format!(
                "pack did not create {}",
                dest_archive.display()
            )));
        }
        Ok(())
    }

    fn test(&self, archive: &Path) -> Result<()> {
        let archive_s = archive.to_string_lossy();
        self.run_checked(&["t", archive_s.as_ref()])?;
        Ok(())
    }
}

fn find_extracted_file(root: &Path, member: &str) -> Result<PathBuf> {
    let want = normalize_member_path(member);
    let want_base = want.rsplit('/').next().unwrap_or(&want);
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if normalize_member_path(&rel) == want
            || entry.file_name().to_string_lossy() == want_base
        {
            return Ok(entry.path().to_path_buf());
        }
    }
    Err(Error::EntryNotFound(member.to_string()))
}

/// Parse archive-level `Solid = +` from full `7z l -slt` output (not `-ba`).
pub fn parse_archive_solid(text: &str) -> bool {
    // Prefer the archive header block (before the `----------` member separator).
    let header = text.split("----------").next().unwrap_or(text);
    for line in header.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Solid = ") {
            let v = rest.trim();
            return v == "+" || v.eq_ignore_ascii_case("true") || v == "1";
        }
    }
    // Fallback: any Solid = + in the file (some 7z builds place it differently).
    text.lines().any(|line| {
        let line = line.trim();
        line == "Solid = +" || line.eq_ignore_ascii_case("Solid = true")
    })
}

/// Parse `7z l -ba` lines into entries.
///
/// Columns: date time attr size [compressed] name…
/// For solid archives, packed size is often blank so only one number appears before the name.
pub fn parse_ba_listing(text: &str) -> Result<Vec<EntryMeta>> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("----") || line.starts_with("Date ") {
            continue;
        }
        if line.contains(" files") && (line.contains("folder") || line.contains("folders")) {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Need at least: date time attr size name
        if parts.len() < 5 {
            continue;
        }
        let attr = parts[2];
        // Attr looks like "....A" or "D...."
        if attr.len() < 4 {
            continue;
        }
        let is_dir = attr.starts_with('D');
        let size: u64 = match parts[3].parse() {
            Ok(s) => s,
            Err(_) => continue,
        };
        // parts[4] may be packed size (digits) or the start of the name
        let name = if parts.len() >= 6 && parts[4].chars().all(|c| c.is_ascii_digit()) {
            parts[5..].join(" ")
        } else {
            parts[4..].join(" ")
        };
        if name.is_empty() {
            continue;
        }
        let path = normalize_member_path(&name);
        let format_hint = if is_dir {
            ArchiveFormat::Unknown
        } else {
            format_from_path(&path)
        };
        entries.push(EntryMeta {
            path,
            size,
            is_dir,
            format_hint,
        });
    }
    Ok(entries)
}

/// Parse `-slt` listing output into entries (files only; dirs optional).
pub fn parse_slt_listing(text: &str) -> Result<Vec<EntryMeta>> {
    let mut entries = Vec::new();
    let mut path: Option<String> = None;
    let mut size: u64 = 0;
    let mut is_dir = false;
    let mut in_entry = false;

    let flush = |entries: &mut Vec<EntryMeta>,
                 path: &mut Option<String>,
                 size: &mut u64,
                 is_dir: &mut bool,
                 in_entry: &mut bool| {
        if *in_entry {
            if let Some(p) = path.take() {
                let p = normalize_member_path(&p);
                if !p.is_empty() {
                    let format_hint = if *is_dir {
                        ArchiveFormat::Unknown
                    } else {
                        format_from_path(&p)
                    };
                    entries.push(EntryMeta {
                        path: p,
                        size: *size,
                        is_dir: *is_dir,
                        format_hint,
                    });
                }
            }
        }
        *size = 0;
        *is_dir = false;
        *in_entry = false;
    };

    for line in text.lines() {
        if line.starts_with("Path = ") {
            flush(
                &mut entries,
                &mut path,
                &mut size,
                &mut is_dir,
                &mut in_entry,
            );
            let p = line.trim_start_matches("Path = ").to_string();
            // First Path is often the archive itself when not using -ba; with -ba each is a member
            path = Some(p);
            in_entry = true;
        } else if line.starts_with("Size = ") {
            size = line
                .trim_start_matches("Size = ")
                .trim()
                .parse()
                .unwrap_or(0);
        } else if line.starts_with("Folder = ") {
            let v = line.trim_start_matches("Folder = ").trim();
            is_dir = v == "+" || v.eq_ignore_ascii_case("true");
        } else if line.starts_with("Attributes = ") {
            let v = line.trim_start_matches("Attributes = ");
            if v.starts_with('D') {
                is_dir = true;
            }
        }
    }
    flush(
        &mut entries,
        &mut path,
        &mut size,
        &mut is_dir,
        &mut in_entry,
    );

    // Drop the archive path itself if it slipped in (no extension path equal to full file)
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slt_sample() {
        let sample = r#"
Path = file1.txt
Size = 12
Attributes = A
Folder = -

Path = nested/data.7z
Size = 999
Folder = -
Attributes = A

Path = emptydir
Size = 0
Folder = +
"#;
        let entries = parse_slt_listing(sample).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "file1.txt");
        assert_eq!(entries[0].size, 12);
        assert!(!entries[0].is_dir);
        assert_eq!(entries[1].path, "nested/data.7z");
        assert_eq!(entries[1].format_hint, ArchiveFormat::SevenZ);
        assert!(entries[2].is_dir);
    }

    #[test]
    fn parse_ba_sample() {
        let sample = "\
2024-01-01 12:00:00 ....A          12           10  file1.txt
2024-01-01 12:00:00 ....A         999               nested/data.7z
2024-01-01 12:00:00 D....           0            0  emptydir
";
        let entries = parse_ba_listing(sample).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "file1.txt");
        assert_eq!(entries[0].size, 12);
        assert!(!entries[0].is_dir);
        assert_eq!(entries[1].path, "nested/data.7z");
        assert_eq!(entries[1].format_hint, ArchiveFormat::SevenZ);
        assert!(entries[2].is_dir);
    }

    #[test]
    fn parse_solid_flag() {
        let solid = r#"
Path = outer.7z
Type = 7z
Solid = +
Blocks = 1
----------
Path = a.7z
Size = 1
"#;
        assert!(parse_archive_solid(solid));
        let nonsolid = r#"
Path = outer.7z
Type = 7z
Solid = -
Blocks = 3
----------
Path = a.7z
Size = 1
"#;
        assert!(!parse_archive_solid(nonsolid));
    }

    #[test]
    fn discover_binary_or_skip() {
        // In CI/dev we install 7zz; this documents the contract
        match find_7z_binary() {
            Ok(p) => assert!(p.exists(), "{}", p.display()),
            Err(e) => eprintln!("7z not installed: {e}"),
        }
    }
}
