//! The one folder the engines keep their search index and clipboard
//! history in (shared by every app built on them), and a one-time move of
//! an app's older, private copy into it.

use std::fs;
use std::path::{Path, PathBuf};

/// `%LOCALAPPDATA%\Dash` (macOS: `~/Library/Application Support/Dash`;
/// Linux: `$XDG_DATA_HOME/dash` or `~/.local/share/dash`).
pub fn shared_data_dir() -> PathBuf {
    let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return env("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir).join("Dash");
    }
    let home = env("HOME").unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/Dash");
    }
    env("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")).join("dash")
}

/// What the engines keep, relative to their folder.
const ENGINE_ITEMS: &[&str] = &[
    "file-search-index",
    "clipboard-images",
    "launcher_icons_v3",
    "preferences/clipboard_history.json",
    "preferences/clipboard_exclusions.json",
    "preferences/frecency.json",
];

const MIGRATED_MARKER: &str = "moved-in";

/// Moves an app's own copy of the engine files from `old` (its private data
/// folder, from before the folder was shared) into `shared`, once, replacing
/// what's there. Only call while holding the owner lock, before any engine
/// starts. Returns whether anything moved.
///
/// # Errors
/// Returns `Err` if a folder can't be created or an item can't be moved.
pub fn move_engine_data(old: &Path, shared: &Path) -> Result<bool, String> {
    let marker = shared.join(MIGRATED_MARKER);
    if marker.exists() {
        return Ok(false);
    }
    fs::create_dir_all(shared).map_err(|e| e.to_string())?;
    let mut moved = false;
    for item in ENGINE_ITEMS {
        let from = old.join(item);
        if !from.exists() {
            continue;
        }
        let to = shared.join(item);
        remove(&to)?;
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if fs::rename(&from, &to).is_err() {
            // Another drive: copy, then remove the original.
            copy(&from, &to)?;
            remove(&from)?;
        }
        moved = true;
    }
    fs::write(&marker, b"1").map_err(|e| e.to_string())?;
    Ok(moved)
}

fn remove(path: &Path) -> Result<(), String> {
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    };
    result.map_err(|e| format!("Cannot remove {}: {e}", path.display()))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if from.is_dir() {
        fs::create_dir_all(to).map_err(|e| e.to_string())?;
        for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ()).map_err(|e| format!("Cannot copy {}: {e}", from.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_an_apps_data_in_once_replacing_whats_there() {
        let root = std::env::temp_dir().join(format!("dash-move-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (old, shared) = (root.join("app"), root.join("shared"));
        fs::create_dir_all(old.join("file-search-index")).unwrap();
        fs::write(old.join("file-search-index/a"), b"app").unwrap();
        fs::create_dir_all(old.join("preferences")).unwrap();
        fs::write(old.join("preferences/clipboard_history.json"), b"app-history").unwrap();
        fs::write(old.join("preferences/other.json"), b"stays").unwrap();
        fs::create_dir_all(shared.join("file-search-index")).unwrap();
        fs::write(shared.join("file-search-index/b"), b"dash").unwrap();

        assert!(move_engine_data(&old, &shared).unwrap());
        assert_eq!(fs::read(shared.join("file-search-index/a")).unwrap(), b"app");
        assert!(!shared.join("file-search-index/b").exists(), "what was there is replaced");
        assert_eq!(fs::read(shared.join("preferences/clipboard_history.json")).unwrap(), b"app-history");
        assert!(!old.join("file-search-index").exists());
        assert!(old.join("preferences/other.json").exists(), "the app's other files stay");

        // Second time: nothing.
        fs::create_dir_all(old.join("clipboard-images")).unwrap();
        assert!(!move_engine_data(&old, &shared).unwrap());
        let _ = fs::remove_dir_all(&root);
    }
}
