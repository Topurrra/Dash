//! Answers a follower's search calls on the owner (see `dash_core::link`).

use std::sync::Arc;

use dash_core::link::Handler;
use dash_core::Host;
use serde_json::{to_value, Value};

use crate::{frecency, search as s};

fn arg<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    serde_json::from_value(args.get(key).cloned().unwrap_or(Value::Null)).map_err(|e| format!("{key}: {e}"))
}

fn done<T: serde::Serialize>(result: Result<T, String>) -> Result<Value, String> {
    result.and_then(|v| to_value(v).map_err(|e| e.to_string()))
}

/// `runtime` runs the async calls (searches, index builds).
pub fn link_handler(host: Arc<dyn Host>, runtime: tokio::runtime::Handle) -> Handler {
    Arc::new(move |op: &str, a: Value| {
        let h = host.clone();
        let result = match op {
            "cancel_file_search_index_build" => done(s::cancel_file_search_index_build()),
            "stop_file_search_index_watcher" => done(s::stop_file_search_index_watcher(h)),
            "get_file_search_status" => done(s::get_file_search_status(h)),
            "get_file_search_index_options" => done(s::get_file_search_index_options(h)),
            "save_file_search_index_options" => done(arg(&a, "options").and_then(|o| s::save_file_search_index_options(h, o))),
            "save_file_search_rebuild_schedule" => {
                done(arg(&a, "schedule").and_then(|o| s::save_file_search_rebuild_schedule(h, o)))
            }
            "start_file_search_index" => match arg(&a, "options") {
                Ok(o) => done(runtime.block_on(s::start_file_search_index(h, o))),
                Err(e) => Err(e),
            },
            "start_content_search_index" => match arg(&a, "options") {
                Ok(o) => done(runtime.block_on(s::start_content_search_index(h, o))),
                Err(e) => Err(e),
            },
            "start_filename_search_index" => match arg(&a, "options") {
                Ok(o) => done(runtime.block_on(s::start_filename_search_index(h, o))),
                Err(e) => Err(e),
            },
            "search_local_files" => match arg(&a, "options") {
                Ok(o) => done(runtime.block_on(s::search_local_files(h, o))),
                Err(e) => Err(e),
            },
            "search_names" => match arg(&a, "options") {
                Ok(o) => done(runtime.block_on(crate::names::search_names(h, o))),
                Err(e) => Err(e),
            },
            "search_file_contents" => match arg(&a, "options") {
                Ok(o) => done(runtime.block_on(s::search_file_contents(h, o))),
                Err(e) => Err(e),
            },
            "update_notes_index" => done((|| s::update_notes_index(h, arg(&a, "upserts")?, arg(&a, "deletes")?))()),
            "record_frecency_launch" => done((|| frecency::record_frecency_launch(h, arg(&a, "kind")?, arg(&a, "path")?))()),
            "get_recent_items" => done(arg(&a, "limit_per_kind").map(|l| frecency::get_recent_items(h, l))),
            "get_frecency_boosts" => done(arg(&a, "kind").map(|k| frecency::get_frecency_boosts(h, k))),
            _ => return None,
        };
        Some(result)
    })
}
