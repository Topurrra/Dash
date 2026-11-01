//! Dash search: the filename and content indexes, natural-language and
//! advanced queries, ranking and frecency, text extraction and OCR for
//! indexing, meaning (semantic) search, live text search, and the app
//! launcher cache. The app it runs in is a
//! [`dash_core::Host`].

// Paths the moved modules use (`super::sensitive_scan`, `super::local_db`,
// `super::ort_environment`).
pub use dash_core::local_db;
pub use dash_core::sensitive as sensitive_scan;
#[cfg(feature = "onnx")]
pub use embedding::ort_environment;

pub mod embedding;
pub mod entry;
pub mod frecency;
pub mod launcher_icons;
pub mod link;
pub mod live_grep;
pub mod names;
pub mod ocr;
pub mod ocr_cache;
pub mod packaged_apps;
pub mod query;
pub mod rank;
pub mod search;
pub mod text_extract;
pub mod vector_cache;
