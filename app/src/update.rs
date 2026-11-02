//! Updates from GitHub Releases. Once a day Dash looks for a newer release;
//! the overlay offers it, and on "Update" this system's package is
//! downloaded and its SHA-256 checked against the `.sha256` file published
//! beside it. Windows: the installer runs silently (it closes Dash,
//! installs, and starts Dash again). macOS: the `.app` inside the `.dmg`
//! replaces the running one. Linux: the new `AppImage` replaces the running
//! one. Then Dash starts again.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};

const LATEST: &str = "https://api.github.com/repos/Topurrra/Dash/releases/latest";
const CHECK_EVERY: Duration = Duration::from_hours(24);
const TIMEOUT: Duration = Duration::from_secs(30);

/// A newer release, as GitHub lists it.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Its page (release notes).
    pub page: String,
    installer: String,
    checksum: String,
    installer_name: String,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Whether `candidate` is a later version than `current` (`v` prefixes and
/// anything after `-` ignored; parts compared as numbers).
fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v').split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn last_check_path() -> PathBuf {
    dash_core::shared::shared_data_dir().join("preferences").join("dash-update.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Whether a day has passed since the last look.
pub fn due() -> bool {
    let last = std::fs::read(last_check_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("lastCheckMs").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    now_ms().saturating_sub(last) >= u64::try_from(CHECK_EVERY.as_millis()).unwrap_or(u64::MAX)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into()
}

/// The latest release when it's newer than this build (and has an installer
/// with a checksum). Notes the time of the look.
pub fn check() -> Result<Option<Release>, String> {
    let response = agent()
        .get(LATEST)
        .header("User-Agent", &format!("Dash/{}", current_version()))
        .header("Accept", "application/vnd.github+json")
        .call();
    let mut response = match response {
        // No release published yet.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        other => other.map_err(|e| format!("Couldn't reach GitHub: {e}"))?,
    };
    let text = response.body_mut().read_to_string().map_err(|e| format!("Couldn't read GitHub's answer: {e}"))?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Unexpected answer from GitHub: {e}"))?;
    let path = last_check_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, serde_json::json!({ "lastCheckMs": now_ms() }).to_string());
    Ok(release_from(&body).filter(|r| newer(&r.version, current_version())))
}

/// How this system's package name ends (`installer/build.ps1`,
/// `packaging/macos/make_dmg.sh`, `packaging/linux/make_appimage.sh`).
fn package_suffix() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(windows) {
        "_x64-setup.exe".to_string()
    } else if cfg!(target_os = "macos") {
        format!("_{arch}.dmg")
    } else {
        format!("_{arch}.AppImage")
    }
}

/// The parts of GitHub's release JSON an update needs.
fn release_from(body: &serde_json::Value) -> Option<Release> {
    let version = body.get("tag_name")?.as_str()?.trim_start_matches('v').to_string();
    let page = body.get("html_url").and_then(|u| u.as_str()).unwrap_or_default().to_string();
    let assets = body.get("assets")?.as_array()?;
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|a| a.get("browser_download_url")?.as_str().map(str::to_string))
    };
    let suffix = package_suffix();
    let installer_name =
        assets.iter().filter_map(|a| a.get("name")?.as_str()).find(|n| n.ends_with(&suffix))?.to_string();
    Some(Release {
        version,
        page,
        installer: url_of(&installer_name)?,
        checksum: url_of(&format!("{installer_name}.sha256"))?,
        installer_name,
    })
}

/// Downloads the installer (`progress`: 0–100), checks its SHA-256, and runs
/// it silently. The installer closes Dash itself.
pub fn install(release: &Release, progress: impl Fn(u8)) -> Result<(), String> {
    let agent = agent();
    let expected = agent
        .get(&release.checksum)
        .call()
        .map_err(|e| format!("Couldn't download the checksum: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let expected = expected.split_whitespace().next().unwrap_or_default().to_lowercase();
    let mut response =
        agent.get(&release.installer).call().map_err(|e| format!("Couldn't download the update: {e}"))?;
    let total: Option<u64> =
        response.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let target = std::env::temp_dir().join(&release.installer_name);
    let mut file = std::fs::File::create(&target).map_err(|e| e.to_string())?;
    let mut reader = response.body_mut().with_config().limit(1024 * 1024 * 1024).reader();
    let mut hasher = Sha256::new();
    let (mut done, mut buf) = (0u64, vec![0u8; 256 * 1024]);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("The download broke off: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += u64::try_from(n).unwrap_or(0);
        if let Some(total) = total.filter(|t| *t > 0) {
            progress(u8::try_from((done * 100 / total).min(100)).unwrap_or(100));
        }
    }
    drop(file);
    if hex::encode(hasher.finalize()) != expected {
        let _ = std::fs::remove_file(&target);
        return Err("The download doesn't match its checksum; nothing was installed.".into());
    }
    run_installer(&target)
}

#[cfg(windows)]
fn run_installer(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new(path)
        .arg("/S")
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Couldn't start the installer: {e}"))
}

/// Puts the `.app` from the downloaded `.dmg` in place of the running one,
/// then starts it.
#[cfg(target_os = "macos")]
fn run_installer(dmg: &std::path::Path) -> Result<(), String> {
    use std::process::Command;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // …/Dash.app/Contents/MacOS/dash
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|b| b.extension().is_some_and(|e| e == "app"))
        .ok_or("This copy of Dash isn't an installed app; get the update from the release page.")?
        .to_path_buf();
    let mount = std::env::temp_dir().join("dash-update-mount");
    let _ = std::fs::create_dir_all(&mount);
    let attached = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(dmg)
        .status()
        .map_err(|e| e.to_string())?;
    if !attached.success() {
        return Err("Couldn't open the downloaded .dmg".into());
    }
    let staged = bundle.with_extension("app.new");
    let _ = std::fs::remove_dir_all(&staged);
    let copied = Command::new("ditto").arg(mount.join("Dash.app")).arg(&staged).status();
    let _ = Command::new("hdiutil").arg("detach").arg(&mount).status();
    if !copied.is_ok_and(|s| s.success()) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err("Couldn't copy the new Dash out of the .dmg".into());
    }
    let old = bundle.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&bundle, &old).map_err(|e| format!("Couldn't replace Dash: {e}"))?;
    if let Err(error) = std::fs::rename(&staged, &bundle) {
        let _ = std::fs::rename(&old, &bundle);
        return Err(format!("Couldn't replace Dash: {error}"));
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_file(dmg);
    let mut start = Command::new("open");
    start.arg("-n").arg(&bundle).arg("--args");
    restart(start)
}

/// Puts the downloaded `AppImage` in place of the running one, then starts it.
#[cfg(target_os = "linux")]
fn run_installer(image: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    // Set by the AppImage runtime to the file it was started from.
    let running = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .ok_or("This copy of Dash isn't the AppImage; get the update from the release page.")?;
    // Next to the running one, so the rename below stays on one disk.
    let staged = running.with_extension("AppImage.new");
    std::fs::copy(image, &staged).map_err(|e| format!("Couldn't place the update: {e}"))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&staged, &running).map_err(|e| format!("Couldn't replace Dash: {e}"))?;
    let _ = std::fs::remove_file(image);
    restart(std::process::Command::new(running))
}

/// Starts the new Dash (it waits for this one to let go of the hotkeys)
/// and closes this one.
#[cfg(not(windows))]
fn restart(mut start: std::process::Command) -> Result<(), String> {
    start
        .args(["--background", crate::host::RESTARTED])
        .spawn()
        .map_err(|e| format!("Couldn't start the new Dash: {e}"))?;
    dash_search::search::stop_all_index_workers();
    std::process::exit(0);
}

// ── In the app ───────────────────────────────────────────────────────────

use gpui::{div, prelude::*, px, App, Global, SharedString};

use crate::theme;

/// What the update bar shows (nothing when `None`).
pub enum Offer {
    Available(Release),
    Downloading(Release, u8),
    /// Downloaded and checked; the installer is closing Dash.
    Installing,
    Failed(Release, String),
}

#[derive(Default)]
struct Updates(Option<Offer>);

impl Global for Updates {}

/// Looks for a newer release now if a day has passed, then once an hour
/// checks whether another day has.
pub fn start_checks(cx: &mut App) {
    cx.set_global(Updates::default());
    cx.spawn(async |cx| loop {
        if due() {
            if let Ok(Ok(Some(release))) = crate::host::runtime().spawn_blocking(check).await {
                let _ = cx.update(|cx| offer(Some(Offer::Available(release)), cx));
            }
        }
        cx.background_executor().timer(Duration::from_hours(1)).await;
    })
    .detach();
}

fn offer(offer: Option<Offer>, cx: &mut App) {
    if cx.try_global::<Updates>().is_some() {
        cx.global_mut::<Updates>().0 = offer;
        cx.refresh_windows();
    }
}

/// Settings › Check now: `done` gets what to say.
pub fn check_now(cx: &mut App, done: impl FnOnce(String, &mut App) + 'static) {
    cx.spawn(async move |cx| {
        let result = crate::host::runtime().spawn_blocking(check).await.map_err(|e| e.to_string()).and_then(|r| r);
        let _ = cx.update(|cx| match result {
            Ok(Some(release)) => {
                let message = format!("Dash {} is available — see the bar at the bottom.", release.version);
                offer(Some(Offer::Available(release)), cx);
                done(message, cx);
            }
            Ok(None) => done(format!("Dash {} is the latest version.", current_version()), cx),
            Err(e) => done(e, cx),
        });
    })
    .detach();
}

fn start_install(release: Release, cx: &mut App) {
    offer(Some(Offer::Downloading(release.clone(), 0)), cx);
    let percent = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
    let seen = std::sync::Arc::clone(&percent);
    let job = release.clone();
    let task = crate::host::runtime()
        .spawn_blocking(move || install(&job, |p| percent.store(p, std::sync::atomic::Ordering::Relaxed)));
    cx.spawn(async move |cx| {
        // Progress until the download is done.
        while !task.is_finished() {
            let p = seen.load(std::sync::atomic::Ordering::Relaxed);
            let _ = cx.update(|cx| offer(Some(Offer::Downloading(release.clone(), p)), cx));
            cx.background_executor().timer(Duration::from_millis(250)).await;
        }
        let result = task.await.map_err(|e| e.to_string()).and_then(|r| r);
        let _ = cx.update(|cx| match result {
            Ok(()) => offer(Some(Offer::Installing), cx),
            Err(e) => offer(Some(Offer::Failed(release, e)), cx),
        });
    })
    .detach();
}

/// The bar above the overlay's footer while an update is on offer.
pub fn render_bar(cx: &App) -> Option<gpui::AnyElement> {
    let offer = cx.try_global::<Updates>()?.0.as_ref()?;
    let button = |id: &'static str, label: &'static str, primary: bool| {
        div()
            .id(id)
            .flex_none()
            .px_2p5()
            .py_1()
            .rounded_md()
            .text_xs()
            .font_weight(gpui::FontWeight::MEDIUM)
            .cursor_pointer()
            .when(primary, |d| d.bg(theme::accent()).text_color(gpui::white()).hover(|s| s.opacity(0.88)))
            .when(!primary, |d| d.text_color(theme::text_2()).hover(|s| s.bg(theme::hover())))
            .child(label)
    };
    let (text, actions): (SharedString, Vec<gpui::AnyElement>) = match offer {
        Offer::Available(release) => {
            let (page, update) = (release.page.clone(), release.clone());
            (
                format!("Dash {} is available (you have {}).", release.version, current_version()).into(),
                vec![
                    button("update-notes", "What's new", false)
                        .on_click(move |_, _, _| {
                            let _ = open_page(&page);
                        })
                        .into_any_element(),
                    button("update-later", "Later", false).on_click(|_, _, cx| offer_none(cx)).into_any_element(),
                    button("update-now", "Update", true)
                        .on_click(move |_, _, cx| start_install(update.clone(), cx))
                        .into_any_element(),
                ],
            )
        }
        Offer::Downloading(release, p) => (format!("Downloading Dash {}… {p}%", release.version).into(), Vec::new()),
        Offer::Installing => ("Installing — Dash closes and starts again in a moment.".into(), Vec::new()),
        Offer::Failed(release, error) => {
            let retry = release.clone();
            (
                format!("The update didn't install: {error}").into(),
                vec![
                    button("update-later", "Dismiss", false).on_click(|_, _, cx| offer_none(cx)).into_any_element(),
                    button("update-retry", "Try again", true)
                        .on_click(move |_, _, cx| start_install(retry.clone(), cx))
                        .into_any_element(),
                ],
            )
        }
    };
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(40.))
            .pl_4()
            .pr_3()
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::accent_soft())
            .child(crate::icons::icon(crate::icons::Icon::Down, 15., theme::accent()))
            .child(div().flex_1().min_w_0().text_sm().text_color(theme::text()).child(text))
            .children(actions)
            .into_any_element(),
    )
}

/// `dash --shots`: the bar with a made-up release (`None`: no bar).
pub fn show_sample(version: Option<&str>, cx: &mut App) {
    if cx.try_global::<Updates>().is_none() {
        cx.set_global(Updates::default());
    }
    offer(
        version.map(|v| {
            Offer::Available(Release {
                version: v.to_string(),
                page: String::new(),
                installer: String::new(),
                checksum: String::new(),
                installer_name: String::new(),
            })
        }),
        cx,
    );
}

fn offer_none(cx: &mut App) {
    offer(None, cx);
}

fn open_page(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer").arg(url).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        open::that_detached(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("v0.1.10", "0.1.9"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-beta", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn release_needs_an_installer_and_its_checksum() {
        let name = format!("Dash_0.2.0{}", package_suffix());
        let body = serde_json::json!({
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/Topurrra/Dash/releases/tag/v0.2.0",
            "assets": [
                { "name": "Dash_0.2.0_other.zip", "browser_download_url": "https://x/other.zip" },
                { "name": name, "browser_download_url": "https://x/package" },
                { "name": format!("{name}.sha256"), "browser_download_url": "https://x/package.sha256" },
            ]
        });
        let release = release_from(&body).expect("release");
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.installer, "https://x/package");
        assert_eq!(release.checksum, "https://x/package.sha256");
        let mut no_checksum = body.clone();
        no_checksum["assets"].as_array_mut().unwrap().pop();
        assert!(release_from(&no_checksum).is_none(), "no checksum, no update");
    }
}
