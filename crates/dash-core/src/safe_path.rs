//! Path validation for backend commands that accept paths from the
//! frontend (or wherever).
//!
//! The threat model: a malicious or buggy frontend caller can hand any
//! string to a Tauri command. If the command does `fs::read(input_path)`
//! directly, the caller can read arbitrary system files —
//! `C:\Windows\System32\config\SAM`, another user's profile, anything the
//! process has access to. The Windows ACL is the last line of defense
//! and the app runs as the user, so the OS won't save us.
//!
//! `validate_user_path` canonicalizes the input, then rejects it if the
//! resolved path lands on an OS-critical location (`forbid_system_path`).
//! Canonicalization first defeats:
//!   - `..` traversal: `appdata\..\..\..\Windows\...` resolves to its real
//!     location before the forbidden-location check runs.
//!   - Symlink escapes: `canonicalize` follows symlinks, so a malicious
//!     symlink is checked at its real target.
//!   - Mixed slashes / case differences: the forbidden-location check is
//!     case-insensitive over the canonical path.
//!
//! When validation FAILS we return a generic error message — leaking
//! "C:\Windows\System32 is a protected location" tells an attacker more
//! about the system than they need to know.

use std::path::{Path, PathBuf};

/// System-level path patterns that are always off-limits to the app
/// regardless of how the path arrived. Keep this list conservative —
/// false positives mean a user can't process a perfectly legitimate
/// file in their own Program Files install, but false negatives mean
/// the app could be weaponized to plant malware in a Startup folder.
///
/// All comparisons are case-insensitive (Windows file systems are
/// case-insensitive, and an attacker using `C:\WINDOWS\...` would
/// otherwise bypass a case-sensitive check). Stored lowercase.
///
/// Patterns:
///   - `C:\Windows\*` — OS binaries + DLLs. Tampering = malware.
///   - `C:\Program Files\*` / `C:\Program Files (x86)\*` — installed apps.
///   - `C:\ProgramData\*` — per-machine app data; writes there persist
///     across users + survive reinstall.
///   - `*\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\*`
///     — classic Windows persistence vector. Writes here = auto-execute on
///     next login.
///   - `*\AppData\Local\Microsoft\Windows\Start Menu\Programs\Startup\*`
///     — same threat, local variant.
///   - `C:\$Recycle.Bin\*` / `C:\System Volume Information\*` — OS-managed,
///     no legitimate user reason to write.
///   - `C:\boot*`, `C:\pagefile.sys`, `C:\hiberfil.sys`, `C:\swapfile.sys`
///     — boot/swap files, modifying = brick the OS.
const FORBIDDEN_PATH_SUBSTRINGS: &[&str] = &[
    r"\windows\",
    r"\program files\",
    r"\program files (x86)\",
    r"\programdata\",
    r"\$recycle.bin\",
    r"\system volume information\",
    r"\appdata\roaming\microsoft\windows\start menu\programs\startup\",
    r"\appdata\local\microsoft\windows\start menu\programs\startup\",
];

/// Also forbid these as exact filenames (relative to a drive root), since
/// they're single-file targets, not directory prefixes.
const FORBIDDEN_DRIVE_ROOT_FILES: &[&str] = &["pagefile.sys", "hiberfil.sys", "swapfile.sys", "bootmgr"];

/// Reject paths that target OS-critical locations. Returns Ok(()) for
/// safe paths; returns Err with a generic message for forbidden ones.
///
/// Designed for the Class B case (user-picked file via OS dialog) where
/// we don't want to constrain the user to a specific root, but we DO
/// want to refuse the small set of paths that no legitimate workflow
/// would target. Even if the frontend gets compromised and tries to
/// route a malicious path through a tool command, this kicks it back.
///
/// The input is canonicalized internally so case + slash + symlink
/// normalization happen before the substring check.
///
/// # Errors
/// Returns `Err` if the path (or, for a new file, its parent) can't be
/// resolved, or if it's a protected system location.
pub fn forbid_system_path<P: AsRef<Path>>(input: P) -> Result<PathBuf, String> {
    let input = input.as_ref();
    // If the path doesn't exist yet (write target), canonicalize the
    // parent and re-append. This still catches Startup folder writes,
    // which is the main threat for not-yet-existing files.
    let candidate = if input.exists() {
        input.canonicalize().map_err(|_| "Invalid or inaccessible path".to_string())?
    } else if let Some(parent) = input.parent() {
        let canon_parent = parent.canonicalize().map_err(|_| "Invalid or inaccessible parent directory".to_string())?;
        match input.file_name() {
            Some(name) => canon_parent.join(name),
            None => canon_parent,
        }
    } else {
        return Err("Invalid path".to_string());
    };

    let lower = candidate.to_string_lossy().to_lowercase();
    for forbidden in FORBIDDEN_PATH_SUBSTRINGS {
        if lower.contains(forbidden) {
            return Err("Path targets a protected system location".to_string());
        }
    }

    // Drive-root file check — `C:\pagefile.sys` is a file at the drive
    // root, not inside a forbidden directory, so the substring scan
    // above misses it. Compare just the path's filename against the
    // explicit list.
    if let Some(file_name) = candidate.file_name() {
        let lower_name = file_name.to_string_lossy().to_lowercase();
        // Only treat it as a drive-root file if its parent IS a drive
        // root (e.g. `C:\`). Otherwise we'd block any file *named*
        // pagefile.sys anywhere on disk, which is too aggressive.
        if let Some(parent) = candidate.parent() {
            let parent_str = parent.to_string_lossy();
            // Drive root looks like `C:\` or `D:\` — exactly 3 chars,
            // letter + colon + separator.
            let is_drive_root = parent_str.len() == 3
                && parent_str.chars().nth(1) == Some(':')
                && (parent_str.ends_with('\\') || parent_str.ends_with('/'));
            if is_drive_root {
                for forbidden in FORBIDDEN_DRIVE_ROOT_FILES {
                    if lower_name == *forbidden {
                        return Err("Path targets a protected system file".to_string());
                    }
                }
            }
        }
    }

    Ok(candidate)
}

/// `path` without Windows' verbatim prefix (`\\?\D:\x` → `D:\x`,
/// `\\?\UNC\server\share` → `\\server\share`), which canonicalizing adds and
/// many apps don't understand when handed a file to open.
pub fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        // Only drive paths; other verbatim forms (`\\?\Volume{…}`) need it.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// Convenience for the most common Class B call site: a Tauri command
/// receives a path string from the frontend, expects it to be a real
/// file (not a write target), and wants both:
///   1. canonicalization (no `..` traversal, real path resolution), and
///   2. rejection if it lands on a forbidden system location.
///
/// Returns the canonical `PathBuf` on success — callers can pass it
/// directly to `fs::read`, `image::open`, etc. without re-canonicalizing.
///
/// # Errors
/// Returns `Err` if the path doesn't exist or can't be read, or if it's a
/// protected system location.
pub fn validate_user_path<P: AsRef<Path>>(input: P) -> Result<PathBuf, String> {
    let canonical = input.as_ref().canonicalize().map_err(|_| "Invalid or inaccessible path".to_string())?;
    forbid_system_path(&canonical)
}

/// Same as `validate_user_path` but for write targets — the path itself
/// may not exist yet, but its parent must. Returns parent.join(filename)
/// after both `forbid_system_path` passes.
///
/// # Errors
/// Returns `Err` if the path has no parent or file name, the parent can't be
/// resolved, or the target is a protected system location.
pub fn validate_user_write_target<P: AsRef<Path>>(input: P) -> Result<PathBuf, String> {
    let input = input.as_ref();
    let parent = input.parent().ok_or_else(|| "Path has no parent directory".to_string())?;
    let file_name = input.file_name().ok_or_else(|| "Path has no file name".to_string())?;
    let canon_parent = parent.canonicalize().map_err(|_| "Invalid or inaccessible parent directory".to_string())?;
    let candidate = canon_parent.join(file_name);
    // Run the canonical candidate through the substring check directly,
    // skipping the .canonicalize() inside forbid_system_path which would
    // fail on a non-existent file.
    forbid_system_path(&candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn verbatim_prefix_is_dropped_for_opening() {
        let plain = |p: &str| without_verbatim_prefix(Path::new(p)).display().to_string();
        assert_eq!(plain(r"\\?\D:\My Files\a b.txt"), r"D:\My Files\a b.txt");
        assert_eq!(plain(r"\\?\UNC\server\share\x.pdf"), r"\\server\share\x.pdf");
        assert_eq!(plain(r"D:\x"), r"D:\x");
        assert_eq!(plain(r"\\?\Volume{1234}\x"), r"\\?\Volume{1234}\x");
    }

    // ─── forbid_system_path / validate_user_path tests ──────────────────

    #[test]
    fn allows_normal_user_files() {
        // A file inside the temp dir (which is part of the user profile,
        // not a forbidden location) should pass.
        let temp = std::env::temp_dir();
        let file = temp.join("dash-forbid-test-1.txt");
        fs::write(&file, b"x").unwrap();
        assert!(validate_user_path(&file).is_ok());
        fs::remove_file(&file).ok();
    }

    #[test]
    fn rejects_windows_directory_paths() {
        // Construct a path string containing the forbidden substring.
        // We deliberately don't canonicalize it for this test — we want
        // to verify the substring check works on the lowercased input.
        let p = PathBuf::from(r"C:\Windows\System32\cmd.exe");
        let result = forbid_system_path(&p);
        assert!(result.is_err(), "Windows path should be rejected: {result:?}");
    }

    #[test]
    fn rejects_program_files_paths() {
        let p = PathBuf::from(r"C:\Program Files\Adobe\Reader\AcroRd32.exe");
        assert!(forbid_system_path(&p).is_err());
        let p2 = PathBuf::from(r"C:\Program Files (x86)\Steam\steam.exe");
        assert!(forbid_system_path(&p2).is_err());
    }

    #[test]
    fn rejects_startup_folder_writes() {
        // The exact path doesn't need to exist — forbid_system_path
        // tries to canonicalize but falls back to the parent's
        // canonicalization for not-yet-existing targets. If even that
        // fails (parent doesn't exist either), we still want the test
        // to verify the substring path; construct one that exists.
        let user_profile = std::env::var("USERPROFILE").unwrap_or_else(|_| String::from(r"C:\Users\Default"));
        let startup =
            format!(r"{user_profile}\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\malicious.bat");
        let result = forbid_system_path(PathBuf::from(&startup));
        // If the path doesn't exist and the parent doesn't either, we
        // get "Invalid or inaccessible parent" — also a rejection.
        // Either way, we should NOT get Ok back.
        assert!(result.is_err(), "Startup folder path should be rejected: {result:?}");
    }

    #[test]
    fn rejects_case_variant_windows_path() {
        // Lowercased internally, so `WINDOWS` in caps should still match.
        let p = PathBuf::from(r"c:\WINDOWS\System32\evil.dll");
        assert!(forbid_system_path(&p).is_err());
    }

    #[test]
    fn does_not_block_innocent_path_with_substring() {
        // A path that just happens to mention "windows" but isn't
        // ACTUALLY in `C:\Windows\` (e.g. `D:\my-windows-themes\bg.jpg`)
        // is fine — the patterns require the literal forbidden segment
        // surrounded by separators.
        let temp = std::env::temp_dir().join("dash-windows-themes-test");
        fs::create_dir_all(&temp).unwrap();
        let inside = temp.join("ok.txt");
        fs::write(&inside, b"x").unwrap();
        // "windows-themes" contains "windows" but not "\windows\" so it
        // passes the substring check.
        assert!(validate_user_path(&inside).is_ok());
        fs::remove_dir_all(&temp).ok();
    }
}
