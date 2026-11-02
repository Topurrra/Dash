//! What the Files table does to files: rename, move, delete (to the Recycle
//! Bin or for good), show several in their folders, and drag them out to
//! Explorer or another app (Windows: the shell's own data object and drag
//! loop, so no COM objects of ours).

use std::path::{Path, PathBuf};

/// Renames a file or folder within its folder. Returns the new path.
pub fn rename_in_place(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name == "." || new_name == ".." {
        return Err("Type a name".into());
    }
    if new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err("A name can't contain \\ / : * ? \" < > |".into());
    }
    let parent = path.parent().ok_or("This can't be renamed")?;
    let target = parent.join(new_name);
    if target == path {
        return Ok(target);
    }
    // A change of case only is the same name to Windows' file system.
    let case_only = target.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    let taken = || format!("“{new_name}” already exists here");
    if !case_only && std::fs::symlink_metadata(&target).is_ok() {
        return Err(taken());
    }
    // The check above is for a clear message; this refuses a clash anyway.
    rename_no_replace(path, &target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            taken()
        } else {
            e.to_string()
        }
    })?;
    Ok(target)
}

/// Renames `from` to `to`, failing (`AlreadyExists`) rather than replacing
/// anything at `to`, even something that appeared after a check.
#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
    // No MOVEFILE_REPLACE_EXISTING (an existing target fails) and no
    // MOVEFILE_COPY_ALLOWED (other drives fail with ERROR_NOT_SAME_DEVICE,
    // which `move_into` handles by copying).
    // SAFETY: both paths are NUL-terminated wide strings that outlive the call.
    unsafe { MoveFileExW(&HSTRING::from(from), &HSTRING::from(to), MOVE_FILE_FLAGS(0)) }.map_err(|e| win_io(&e))
}

/// Elsewhere: no portable no-replace rename in std, so check, then rename.
#[cfg(not(windows))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let case_only = to.to_string_lossy().to_lowercase() == from.to_string_lossy().to_lowercase();
    if !case_only && std::fs::symlink_metadata(to).is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

/// A Win32 error as an `io::Error` with its real error code (so `kind()`
/// works; the `windows` crate's own conversion keeps the HRESULT).
#[cfg(windows)]
fn win_io(e: &windows::core::Error) -> std::io::Error {
    let code = e.code().0.cast_unsigned();
    if code & 0xFFFF_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((code & 0xFFFF).cast_signed())
    } else {
        std::io::Error::other(e.message().clone())
    }
}

/// Moves a file or folder into `folder` (keeping its name). Across drives
/// it copies, then deletes the original. Returns the new path.
pub fn move_into(path: &Path, folder: &Path) -> Result<PathBuf, String> {
    let name = path.file_name().ok_or("This can't be moved")?;
    let target = folder.join(name);
    if target == path {
        return Err("It's already in that folder".into());
    }
    if folder.starts_with(path) {
        return Err("A folder can't go inside itself".into());
    }
    let taken = || format!("“{}” already exists in {}", name.to_string_lossy(), folder.display());
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(taken());
    }
    match rename_no_replace(path, &target) {
        Ok(()) => Ok(target),
        Err(e) if crosses_devices(&e) => move_by_copy(path, &target).map(|()| target),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(taken()),
        Err(e) => Err(e.to_string()),
    }
}

fn crosses_devices(e: &std::io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE on Windows, EXDEV elsewhere.
    e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(if cfg!(windows) { 17 } else { 18 })
}

/// Copies `from` to `to` (recursively), then deletes `from`. A failed copy
/// removes what it made (never something already at `to`) and leaves the
/// original alone.
fn move_by_copy(from: &Path, to: &Path) -> Result<(), String> {
    let mut made = false;
    if let Err(e) = copy_recursive(from, to, &mut made) {
        if made {
            let _ = delete_permanently(to);
        }
        return Err(e);
    }
    delete_permanently(from)
}

/// Copies without ever writing over an existing file or folder. `made`
/// turns true once something at `to` may be this call's.
fn copy_recursive(from: &Path, to: &Path, made: &mut bool) -> Result<(), String> {
    let at = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let meta = std::fs::symlink_metadata(from).map_err(|e| at(from, e))?;
    if meta.is_dir() {
        std::fs::create_dir(to).map_err(|e| at(to, e))?;
        *made = true;
        for entry in std::fs::read_dir(from).map_err(|e| at(from, e))? {
            let entry = entry.map_err(|e| at(from, e))?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()), &mut true)?;
        }
        Ok(())
    } else {
        copy_file_new(from, to).map_err(|e| {
            // AlreadyExists: what's there isn't ours to clean up.
            *made = e.kind() != std::io::ErrorKind::AlreadyExists;
            at(from, e)
        })?;
        *made = true;
        Ok(())
    }
}

/// Copies one file; fails (`AlreadyExists`) if `to` exists.
#[cfg(windows)]
fn copy_file_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::CopyFileExW;
    const COPY_FILE_FAIL_IF_EXISTS: u32 = 1;
    // SAFETY: both paths are NUL-terminated wide strings that outlive the
    // call; no progress callback, callback data or cancel flag is passed.
    unsafe { CopyFileExW(&HSTRING::from(from), &HSTRING::from(to), None, None, None, COPY_FILE_FAIL_IF_EXISTS) }
        .map_err(|e| win_io(&e))
}

#[cfg(not(windows))]
fn copy_file_new(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.set_permissions(src.metadata()?.permissions())
}

/// Deletes for good (no Recycle Bin): a folder with everything in it.
pub fn delete_permanently(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        // A link to a folder on Windows is removed as a folder.
        std::fs::remove_file(path).or_else(|e| std::fs::remove_dir(path).map_err(|_| e))
    };
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// Paths as one UTF-16 buffer, each ending in a NUL and the list in another
/// (what `SHFILEOPSTRUCTW.pFrom` wants).
#[cfg(any(windows, test))]
pub fn double_null_list(paths: &[String]) -> Vec<u16> {
    let mut out = Vec::new();
    for p in paths {
        out.extend(p.encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

/// Paths grouped by folder, folders in first-seen order (showing several
/// items opens each folder once with its items selected).
pub fn group_by_folder(paths: &[String]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for p in paths {
        let folder = crate::files_table::folder_of(p).to_string();
        match groups.iter_mut().find(|(f, _)| f.eq_ignore_ascii_case(&folder)) {
            Some((_, items)) => items.push(p.clone()),
            None => groups.push((folder, vec![p.clone()])),
        }
    }
    groups
}

/// Sends files and folders to the Recycle Bin without asking again (the
/// overlay asked first) — except that the shell asks before deleting for
/// good anything the bin can't take (another drive, a share, too big), and
/// "No" keeps it. Blocks while that question is up: the caller sets
/// `settings::PICKING` so the overlay stays.
#[cfg(windows)]
pub fn recycle(paths: &[String]) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE,
        SHFILEOPSTRUCTW,
    };
    // "No" to the shell's question shows as aborted (and sometimes as
    // ERROR_CANCELLED); the caller counts what actually went.
    const ERROR_CANCELLED: i32 = 1223;
    if paths.is_empty() {
        return Ok(());
    }
    let from = double_null_list(paths);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        // FOF_WANTNUKEWARNING overrides FOF_NOCONFIRMATION for items that
        // would be destroyed instead of recycled.
        fFlags: u16::try_from(
            (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT | FOF_WANTNUKEWARNING).0,
        )
        .expect("the FOF_* flags all sit in the low 16 bits"),
        ..Default::default()
    };
    // Called off the UI thread: the shell wants COM there.
    // SAFETY: no reserved pointer is passed; balanced by CoUninitialize below
    // only when it succeeded.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    // SAFETY: `op` is fully initialized and `from` (the double-NUL list
    // `pFrom` points into) lives until after the call.
    let code = unsafe { SHFileOperationW(&raw mut op) };
    if com {
        // SAFETY: pairs the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
    if op.fAnyOperationsAborted.as_bool() || code == ERROR_CANCELLED {
        return Err("Some items weren't deleted".into());
    }
    if code != 0 {
        return Err(format!("Couldn't move to the Recycle Bin (error {code:#x})"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn recycle(_paths: &[String]) -> Result<(), String> {
    Err("There's no Recycle Bin here yet; Shift+Delete deletes for good".into())
}

/// Shell item ids for paths (freed on drop).
#[cfg(windows)]
struct Pidls(Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST>);

#[cfg(windows)]
impl Pidls {
    fn parse(paths: &[String]) -> Result<Self, String> {
        use windows::core::HSTRING;
        use windows::Win32::UI::Shell::SHParseDisplayName;
        let mut out = Pidls(Vec::with_capacity(paths.len()));
        for p in paths {
            let mut pidl = std::ptr::null_mut();
            // SAFETY: the HSTRING outlives the call; `pidl` receives an
            // allocation this guard frees on drop.
            unsafe { SHParseDisplayName(&HSTRING::from(p.as_str()), None, &raw mut pidl, 0, None) }
                .map_err(|e| format!("{p}: {}", e.message()))?;
            out.0.push(pidl);
        }
        Ok(out)
    }

    fn as_const(&self) -> Vec<*const windows::Win32::UI::Shell::Common::ITEMIDLIST> {
        self.0.iter().map(|p| (*p).cast_const()).collect()
    }
}

#[cfg(windows)]
impl Drop for Pidls {
    fn drop(&mut self) {
        for p in &self.0 {
            // SAFETY: each pointer came from a successful SHParseDisplayName,
            // is owned only by this guard and is freed exactly once here.
            unsafe { windows::Win32::UI::Shell::ILFree(Some((*p).cast_const())) };
        }
    }
}

/// Opens each item's folder in Explorer with the items selected (at most
/// ten folders). Runs on the UI thread, where GPUI set up OLE.
#[cfg(windows)]
pub fn reveal_many(paths: &[String]) -> Result<(), String> {
    use windows::Win32::UI::Shell::{ILFindLastID, SHOpenFolderAndSelectItems};
    for (folder, items) in group_by_folder(paths).into_iter().take(10) {
        let folder = Pidls::parse(&[folder])?;
        let items = Pidls::parse(&items)?;
        // SAFETY: each pidl is a valid id list owned by `items`; the result
        // points inside it, and `items` outlives `children`.
        let children: Vec<_> =
            items.0.iter().map(|p| unsafe { ILFindLastID((*p).cast_const()) }.cast_const()).collect();
        // SAFETY: `folder` holds exactly one valid pidl (parse of one path)
        // and `children` point into `items`, both alive for the call.
        unsafe { SHOpenFolderAndSelectItems(folder.0[0], Some(&children), 0) }.map_err(|e| e.message().clone())?;
    }
    Ok(())
}

/// Drags files out of Dash: the shell builds the data object (`CF_HDROP` and
/// the rest Explorer wants) and runs the drag loop with its own drag image.
/// Blocks until the drop; call it outside any GPUI update (it pumps
/// messages, and GPUI repaints meanwhile).
#[cfg(windows)]
pub fn drag_out(paths: &[String]) -> Result<(), String> {
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows::Win32::UI::Shell::{BHID_DataObject, SHCreateShellItemArrayFromIDLists, SHDoDragDrop};
    if paths.is_empty() {
        return Ok(());
    }
    let pidls = Pidls::parse(paths)?;
    // SAFETY: the pidls are valid and alive until the end of the function.
    let items = unsafe { SHCreateShellItemArrayFromIDLists(&pidls.as_const()) }.map_err(|e| e.message().clone())?;
    // SAFETY: `items` is a live COM object we own; no bind context is passed.
    let data: IDataObject = unsafe { items.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.message().clone())?;
    // SAFETY: no arguments; it only reads this thread's active window.
    let window = unsafe { GetActiveWindow() };
    // No drop source of ours: the shell supplies the default one.
    // SAFETY: `data` is a live data object we own, and this runs on the UI
    // thread where OLE is initialized (see doc comment).
    let _ = unsafe {
        SHDoDragDrop(
            window,
            &data,
            None::<&windows::Win32::System::Ole::IDropSource>,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )
    };
    Ok(())
}

#[cfg(not(windows))]
pub fn drag_out(_paths: &[String]) -> Result<(), String> {
    Err("Dragging files out isn't supported here yet".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh folder under the system temp folder, removed on drop.
    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "dash-file-ops-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }

        fn file(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, body).unwrap();
            p
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn renames_within_the_folder() {
        let t = Temp::new();
        let a = t.file("a.txt", "x");
        let b = rename_in_place(&a, " b.txt ").unwrap();
        assert_eq!(b, t.0.join("b.txt"));
        assert!(!a.exists());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "x");
        // Same name: nothing to do.
        assert_eq!(rename_in_place(&b, "b.txt").unwrap(), b);
    }

    #[test]
    fn rename_refuses_bad_names_and_clashes() {
        let t = Temp::new();
        let a = t.file("a.txt", "a");
        t.file("taken.txt", "t");
        assert!(rename_in_place(&a, "").is_err());
        assert!(rename_in_place(&a, "..").is_err());
        assert!(rename_in_place(&a, "x/y").is_err());
        assert!(rename_in_place(&a, "what?").is_err());
        assert!(rename_in_place(&a, "taken.txt").unwrap_err().contains("already exists"));
        assert_eq!(std::fs::read_to_string(t.0.join("taken.txt")).unwrap(), "t");
    }

    #[test]
    fn rename_can_change_case_only() {
        let t = Temp::new();
        let a = t.file("case.txt", "c");
        let b = rename_in_place(&a, "CASE.txt").unwrap();
        let names: Vec<String> =
            std::fs::read_dir(&t.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["CASE.txt".to_string()]);
        assert_eq!(b.file_name().unwrap(), "CASE.txt");
    }

    #[test]
    fn moves_into_a_folder() {
        let t = Temp::new();
        let a = t.file("a.txt", "a");
        let dest = t.0.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let moved = move_into(&a, &dest).unwrap();
        assert_eq!(moved, dest.join("a.txt"));
        assert!(!a.exists() && moved.exists());
        // Already there.
        assert!(move_into(&moved, &dest).is_err());
    }

    #[test]
    fn move_refuses_clashes_and_folder_into_itself() {
        let t = Temp::new();
        let a = t.file("a.txt", "new");
        let dest = t.0.join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("a.txt"), "old").unwrap();
        assert!(move_into(&a, &dest).unwrap_err().contains("already exists"));
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "old");
        let inner = dest.join("inner");
        std::fs::create_dir(&inner).unwrap();
        assert!(move_into(&dest, &inner).is_err());
    }

    #[test]
    fn copy_then_delete_moves_a_tree() {
        let t = Temp::new();
        let src = t.0.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub").join("f.txt"), "deep").unwrap();
        std::fs::write(src.join("g.txt"), "top").unwrap();
        let to = t.0.join("to");
        move_by_copy(&src, &to).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(to.join("sub").join("f.txt")).unwrap(), "deep");
        assert_eq!(std::fs::read_to_string(to.join("g.txt")).unwrap(), "top");
    }

    /// A target that appears after the "already exists" check (here: the
    /// low-level steps called directly) is refused, never replaced, and the
    /// original stays.
    #[test]
    fn target_appearing_late_is_an_error_and_nothing_is_lost() {
        let t = Temp::new();
        let a = t.file("a.txt", "mine");
        let b = t.file("b.txt", "theirs");
        let e = rename_no_replace(&a, &b).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "theirs");

        assert_eq!(copy_file_new(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "theirs");

        // Moving by copy onto something already there: refused, and the
        // cleanup leaves what was there alone.
        let err = move_by_copy(&a, &b).unwrap_err();
        assert!(err.contains("a.txt"), "names the path: {err}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "theirs");
        let dir = t.0.join("dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep"), "k").unwrap();
        let src = t.0.join("src");
        std::fs::create_dir(&src).unwrap();
        assert!(move_by_copy(&src, &dir).is_err());
        assert!(src.exists());
        assert_eq!(std::fs::read_to_string(dir.join("keep")).unwrap(), "k");
    }

    #[test]
    fn deletes_files_and_folders_for_good() {
        let t = Temp::new();
        let f = t.file("f.txt", "f");
        delete_permanently(&f).unwrap();
        assert!(!f.exists());
        let d = t.0.join("d");
        std::fs::create_dir_all(d.join("e")).unwrap();
        std::fs::write(d.join("e").join("x"), "x").unwrap();
        delete_permanently(&d).unwrap();
        assert!(!d.exists());
        assert!(delete_permanently(&t.0.join("missing")).is_err());
    }

    #[test]
    fn double_null_list_layout() {
        let list = double_null_list(&["a".into(), "bc".into()]);
        assert_eq!(list, vec![u16::from(b'a'), 0, u16::from(b'b'), u16::from(b'c'), 0, 0]);
        assert_eq!(double_null_list(&[]), vec![0]);
    }

    #[test]
    fn groups_paths_by_folder() {
        let groups = group_by_folder(&["C:\\a\\1".into(), "C:\\b\\2".into(), "c:\\A\\3".into()]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "C:\\a");
        assert_eq!(groups[0].1, vec!["C:\\a\\1".to_string(), "c:\\A\\3".to_string()]);
        assert_eq!(groups[1].1, vec!["C:\\b\\2".to_string()]);
    }
}
