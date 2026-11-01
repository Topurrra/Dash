//! What Dash's engines (search, clipboard history) need from the app they
//! run in, and helpers both share.
//!
//! The Dash app implements [`Host`]; so can any other app that embeds
//! the engines. The engines never talk to a UI framework
//! directly: they ask the host where their files live and tell it when
//! something changed.

use std::path::PathBuf;

#[cfg(windows)]
pub mod dpapi;
pub mod link;
pub mod local_db;
pub mod safe_path;
pub mod sensitive;
pub mod shared;

/// The app an engine runs in.
pub trait Host: Send + Sync + 'static {
    /// The folder the engine keeps its files in (created by the engine as
    /// needed).
    fn data_dir(&self) -> Result<PathBuf, String>;

    /// Something changed that a UI may want to show (`event` is a stable
    /// name such as `clipboard-history-updated`).
    fn emit(&self, event: &str, payload: serde_json::Value);
}

/// A shared host is a host.
impl<T: Host + ?Sized> Host for std::sync::Arc<T> {
    fn data_dir(&self) -> Result<PathBuf, String> {
        (**self).data_dir()
    }

    fn emit(&self, event: &str, payload: serde_json::Value) {
        (**self).emit(event, payload)
    }
}
