//! Runs in any app that is a `dash_core::Host`.
//! Packaged-app (MSIX / Microsoft Store / Appx) enumeration for the launcher.
//!
//! WHY THIS EXISTS. The launch-target cache is built by walking Start Menu and
//! Desktop for `.lnk`/`.exe`/`.url`/`.msc` files. Packaged apps have **none of
//! those** — they are installed under `C:\Program Files\WindowsApps` (an ACL'd
//! directory) and are launched by **`AppUserModelID` (AUMID)** through the shell,
//! not by a filesystem path. So they were structurally invisible to the
//! launcher no matter how the scan was tuned.
//!
//! Measured on a real machine (2026-07-27): 10 of 221 launchable apps were
//! missing, and they were not obscure — **Settings, Terminal, `ChatGPT`, Claude,
//! Sticky Notes, `DuckDuckGo`, NVIDIA Control Panel**. "Settings" being
//! unreachable is a serious hole for a launcher whose whole pitch is that if it
//! exists on your machine, you can get to it.
//!
//! HOW. We enumerate the shell's `shell:AppsFolder` virtual namespace — the
//! exact source the Start menu and `Get-StartApps` use. Each child item yields
//! a display name plus a parsing name; for a packaged app the parsing name IS
//! the AUMID (it contains `!`, separating the package family from the app id).
//!
//! WHY ONLY PACKAGED ENTRIES. `AppsFolder` also lists every classic Win32 app,
//! which the `.lnk` walk already covers. Taking everything would double-index
//! the whole catalog and fight the existing dedup, so we keep only entries
//! whose parsing name is an AUMID and leave the Win32 path untouched.
//!
//! Enumeration is read-only shell COM: no package is activated, nothing is
//! spawned, nothing touches the network.

/// One launchable packaged app.
#[derive(Debug, Clone)]
pub struct PackagedApp {
    /// Friendly display name as the shell reports it (e.g. "Sticky Notes").
    pub name: String,
    /// `AppUserModelID` — `<PackageFamilyName>!<ApplicationId>`. This is the only
    /// launch handle a packaged app has; there is no exe path to run.
    pub aumid: String,
}

/// The synthetic launch path we store for a packaged app.
///
/// Packaged apps have no filesystem path, but the launcher cache is keyed by
/// `path`. We store the shell's own URI form so the value is self-describing,
/// stays unique, and can be handed straight to the shell at launch time. The
/// `shell:AppsFolder\` prefix is also what `launch_cached_target` branches on to
/// skip its `Path::exists()` check (an AUMID is not a file).
pub fn packaged_launch_path(aumid: &str) -> String {
    format!("shell:AppsFolder\\{aumid}")
}

/// Whether a launch-target path refers to a packaged app rather than a file.
pub fn is_packaged_launch_path(path: &str) -> bool {
    path.starts_with("shell:AppsFolder\\")
}

/// Enumerate every launchable packaged app. Returns an empty vec on any COM
/// failure — a launcher missing Store apps is worse than today, but a launcher
/// that fails to build its cache at all is far worse, so this never propagates.
#[cfg(windows)]
pub fn enumerate_packaged_apps() -> Vec<PackagedApp> {
    use windows::core::{w, PWSTR};
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN_DESKTOPABSOLUTEPARSING,
        SIGDN_NORMALDISPLAY,
    };

    /// Copy a shell-allocated wide string into an owned String and free it.
    /// `raw` must be null or a NUL-terminated string from `CoTaskMemAlloc`
    /// that the caller owns.
    unsafe fn take_pwstr(raw: PWSTR) -> Option<String> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: per the contract above, `raw` is a NUL-terminated string
        // we own (from GetDisplayName); it is read once here, then freed.
        let text = unsafe { raw.to_string() }.ok();
        // SAFETY: freed with the allocator it came from and never touched again.
        unsafe { CoTaskMemFree(Some(raw.0.cast_const().cast())) };
        text
    }

    let mut out = Vec::new();

    // All calls below are standard read-only shell COM. Every raw pointer we
    // receive (PWSTR from GetDisplayName) is copied into an owned String and
    // then freed with CoTaskMemFree, per the shell's contract.

    // Apartment-threaded, matching `launcher_icons`. We never
    // CoUninitialize for the same reason it doesn't: this may run on a
    // pooled thread that other COM users share.
    // SAFETY: CoInitializeEx is always safe to call; a repeat or a different
    // apartment just returns an error code, which we ignore.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // SAFETY: `w!` gives a static NUL-terminated string.
    let Ok(folder) = (unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(w!("shell:AppsFolder"), None) }) else {
        return out;
    };
    // SAFETY: `folder` is a live shell item; BHID_EnumItems is a static GUID.
    let Ok(items) = (unsafe { folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) }) else {
        return out;
    };

    loop {
        let mut fetched = [None; 1];
        let mut count = 0u32;
        // Next() returns S_FALSE (not an Err) when the enumeration is done,
        // so drive the loop off the fetched count rather than the HRESULT.
        // SAFETY: `fetched` and `count` are valid out slots for one item.
        if unsafe { items.Next(&mut fetched, Some(&raw mut count)) }.is_err() || count == 0 {
            break;
        }
        let Some(item) = fetched[0].take() else { break };

        // The parsing name is the AUMID for packaged apps and a filesystem
        // path for classic ones. The `!` is what separates the two cases.
        // SAFETY: `item` is a live shell item; the string it returns is ours.
        let Ok(parsing_raw) = (unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) }) else {
            continue;
        };
        // SAFETY: a string GetDisplayName just handed us, used nowhere else.
        let Some(aumid) = (unsafe { take_pwstr(parsing_raw) }) else { continue };
        if !aumid.contains('!') {
            continue; // classic Win32 app — already covered by the .lnk walk
        }

        // SAFETY: as above.
        let Ok(name_raw) = (unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }) else {
            continue;
        };
        // SAFETY: as above.
        let Some(name) = (unsafe { take_pwstr(name_raw) }) else { continue };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }

        out.push(PackagedApp { name, aumid });
    }

    out
}

#[cfg(not(windows))]
pub fn enumerate_packaged_apps() -> Vec<PackagedApp> {
    Vec::new()
}

/// Resolve a packaged app's AUMID to the real `.exe` on disk.
///
/// WHY. The shell's tile image (`IShellItemImageFactory`) is a Start-menu
/// *tile*: the logo sits inside a safe area with padding, often on its own
/// plate, so at a 32px row it renders as a tiny mark. The app's **exe** carries
/// a normal Win32 icon resource that fills its canvas — which is exactly why the
/// Commands section (which lists running processes by exe path) already showed
/// crisp ChatGPT/Claude icons while the launcher showed tiny ones. Resolving to
/// the exe lets packaged apps reuse the same extractor as every classic app, so
/// they look identical rather than merely "present".
///
/// `C:\Program Files\WindowsApps` denies *listing* to nobody in practice — it is
/// ACL'd against writes and some reads, but directory enumeration and manifest
/// reads succeed at normal user privilege (verified 2026-07-27), so this needs
/// no elevation and keeps the no-UAC law intact.
#[cfg(windows)]
pub fn resolve_packaged_exe(aumid: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    // `OpenAI.Codex_2p2nqsd0c76g0!App` → family + application id.
    let (family, app_id) = aumid.split_once('!')?;
    // `OpenAI.Codex_2p2nqsd0c76g0` → package name + publisher hash.
    let (pkg_name, pub_hash) = family.rsplit_once('_')?;
    if pkg_name.is_empty() || pub_hash.is_empty() {
        return None;
    }

    // Package folders are `<Name>_<Version>_<Arch>__<PublisherHash>`. Match on
    // both ends so a same-named package from another publisher can't be picked.
    let root = PathBuf::from(r"C:\Program Files\WindowsApps");
    let name_prefix = format!("{}_", pkg_name.to_ascii_lowercase());
    let hash_suffix = format!("__{}", pub_hash.to_ascii_lowercase());

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let dir = e.file_name().to_string_lossy().to_ascii_lowercase();
            dir.starts_with(&name_prefix) && dir.ends_with(&hash_suffix)
        })
        .map(|e| e.path())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // Several versions can coexist during an update; the lexically greatest
    // folder name is the newest version (the version field is zero-padded-ish
    // and ordered), which is the one the shell would launch.
    candidates.sort();
    candidates.reverse();

    for dir in candidates {
        let Some(exe_rel) = executable_for_app_id(&dir.join("AppxManifest.xml"), app_id) else {
            continue;
        };
        // Manifests use either separator (`app/ChatGPT.exe`, `app\Claude.exe`).
        let exe = dir.join(exe_rel.replace('/', "\\"));
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

/// Pull `Executable` off the `<Application>` whose `Id` matches, from an
/// `AppxManifest`. Returns the manifest-relative path.
#[cfg(windows)]
fn executable_for_app_id(manifest: &std::path::Path, app_id: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let xml = std::fs::read_to_string(manifest).ok()?;
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return None,
            // `<Application …/>` is usually a Start element (it has children like
            // VisualElements) but can be Empty; handle both.
            Ok(Event::Start(e) | Event::Empty(e)) => {
                // Element names are namespace-prefixed in some manifests
                // (`uap:Application` never occurs, but be tolerant anyway).
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if !name.rsplit(':').next().unwrap_or(&name).eq_ignore_ascii_case("Application") {
                    continue;
                }
                let (mut id, mut exe) = (None, None);
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                    let key = key.rsplit(':').next().unwrap_or(&key).to_ascii_lowercase();
                    let val = String::from_utf8_lossy(&attr.value).to_string();
                    match key.as_str() {
                        "id" => id = Some(val),
                        "executable" => exe = Some(val),
                        _ => {}
                    }
                }
                if id.as_deref() == Some(app_id) {
                    return exe;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_paths_round_trip() {
        let path = packaged_launch_path("Microsoft.WindowsTerminal_8wekyb3d8bbwe!App");
        assert_eq!(path, "shell:AppsFolder\\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App");
        assert!(is_packaged_launch_path(&path));
    }

    #[test]
    fn real_file_paths_are_not_packaged() {
        assert!(!is_packaged_launch_path(r"C:\Windows\System32\notepad.exe"));
        assert!(!is_packaged_launch_path(
            r"C:\Users\x\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Firefox.lnk"
        ));
        assert!(!is_packaged_launch_path(""));
    }

    /// Exercises the real shell COM enumeration on this machine. Ignored by
    /// default (it depends on what's installed); run with:
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture enumerates_real_packaged_apps
    /// ```
    ///
    /// Asserts only what must hold on any Windows box rather than a specific
    /// app list: the call succeeds, every entry has a name, and every parsing
    /// name is a real AUMID (contains `!`). Settings ships with Windows, so its
    /// absence would mean the enumeration silently returned classic apps only.
    #[cfg(windows)]
    #[test]
    #[ignore = "enumerates the real shell AppsFolder; run with --ignored"]
    fn enumerates_real_packaged_apps() {
        let apps = enumerate_packaged_apps();
        println!("packaged apps found: {}", apps.len());
        for app in apps.iter().take(25) {
            println!("  {}  ->  {}", app.name, app.aumid);
        }
        assert!(!apps.is_empty(), "no packaged apps found — shell enumeration failed");
        for app in &apps {
            assert!(!app.name.trim().is_empty(), "empty name: {app:?}");
            assert!(app.aumid.contains('!'), "not an AUMID: {app:?}");
        }
    }
}
