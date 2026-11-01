//! Dash as the host of its engines: their files live in the shared data
//! folder (`%LOCALAPPDATA%\Dash`). Other apps built on the same engines can
//! use it too: whichever starts first runs the engines, the others follow
//! it (see `dash_core::link`).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub struct DashHost {
    dir: PathBuf,
}

impl dash_core::Host for DashHost {
    fn data_dir(&self) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        Ok(self.dir.clone())
    }

    /// The overlay reads state when it opens, so events only go to a follower.
    fn emit(&self, event: &str, payload: serde_json::Value) {
        dash_core::link::broadcast(event, &payload);
    }
}

/// The one host of this process.
pub fn host() -> Arc<dyn dash_core::Host> {
    static HOST: OnceLock<Arc<dyn dash_core::Host>> = OnceLock::new();
    HOST.get_or_init(|| Arc::new(DashHost { dir: dash_core::shared::shared_data_dir() })).clone()
}

/// The async runtime the engines' async APIs run on.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("dash-engine")
            .build()
            .expect("tokio runtime")
    })
}

/// Runs the engines, or follows another app that already does.
pub fn claim_engines() {
    dash_core::link::register(dash_clipboard::link_handler());
    dash_core::link::register(dash_search::link::link_handler(host(), runtime().handle().clone()));
    dash_core::link::claim(
        &dash_core::shared::shared_data_dir(),
        "Dash",
        || {},
        Arc::new(|event: &str, _: serde_json::Value| {
            if event == dash_core::link::LOST_OWNERSHIP_EVENT {
                restart_as_follower();
            }
        }),
    );
}

/// Another process owns the engines while this one ran them too: stop this
/// one's workers and start Dash again, which then follows that owner. (Its
/// clipboard capture, watchers and schedules can't be stopped piecemeal.)
fn restart_as_follower() {
    dash_search::search::stop_all_index_workers();
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(["--background", RESTARTED]).spawn();
    }
    std::process::exit(0);
}

/// Given to the restarted Dash: wait for this one to let go of the hotkeys.
pub const RESTARTED: &str = "--restarted";

/// First run (no saved index settings): index every drive's names and the
/// home folder's contents. Later runs: the
/// scheduler keeps them fresh. Only the owner does this.
pub fn start_search_engines() {
    use dash_search::search as s;
    dash_core::link::when_owner(|| {
        let host = host();
        if matches!(s::get_file_search_index_options(host.clone()), Ok(None)) {
            seed_first_index(host);
        }
    });
    s::start_file_search_scheduler(host());
    s::prewarm_search_engines(host());
}

fn seed_first_index(host: Arc<dyn dash_core::Host>) {
    use dash_search::search as s;
    let drives = s::list_logical_drives();
    let home = std::env::var("USERPROFILE").ok().into_iter().collect::<Vec<_>>();
    let options: Result<s::FileSearchIndexOptions, _> = serde_json::from_value(serde_json::json!({
        "roots": home,
        "filenameRoots": drives,
        "includeHidden": false,
        "indexContent": true,
        "maxContentKb": null,
        "commitEvery": null,
        "performanceMode": "balanced",
        "watcherEnabled": true,
    }));
    // Built with the saved mode (Balanced: below-normal priority with short
    // pauses). Not Quiet: idle priority, 2 threads and 18 ms per file made a
    // first index of a big drive take hours. Names first: they're what
    // search needs; the contents take longer.
    if let Ok(options) = options {
        let _ = s::save_file_search_index_options(host.clone(), options.clone());
        runtime().spawn(async move {
            let _ = s::start_filename_search_index(host.clone(), options.clone()).await;
            let _ = s::start_content_search_index(host, options).await;
        });
    }
}
