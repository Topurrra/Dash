//! Runs in any app that is a `dash_core::Host`.
//! Text embeddings for semantic search.
//!
//! A static embedding model, `Model2Vec` `potion-retrieval-32M` (MIT, 512-d),
//! through `model2vec-rs`: a text's vector is the weighted mean of its
//! tokens' vectors — a table lookup, no neural network at run time. On
//! 3,467 of the owner's documents it embedded ~270,000 passages a second
//! with ~0.4 GB of RAM, where `EmbeddingGemma` 2 (the model before) managed
//! ~2 a second with ~10 GB: days for a large home folder instead of
//! seconds. Queries and documents are embedded the same way (no prefixes).
//!
//! The real implementation is behind the `semantic` cargo feature. Builds
//! without it get inert stubs (`embed` → `None`, `is_available` →
//! `false`), so semantic search is simply absent, never a build break.
//!
//! Everything here is offline: `tokenizer.json`, `model.safetensors` and
//! `config.json` are read from the bundled resource dir
//! (`embedding-runtime/`, set via `set_model_dir` at startup). No
//! `HuggingFace` download, no network.
//!
//! `chunk_text` (below) splits a document into overlapping passages so each
//! is embedded separately — a long document's specific matching passage is
//! not diluted into one averaged doc vector. It's pure logic (no model), so
//! it's available and tested even in builds without the `semantic` feature.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The one ONNX Runtime environment for the app (ort 2.0.0-rc.10+ needs it
/// for every session). Shared by embeddings and background removal; `None`
/// if the runtime can't start, which callers treat as "feature off".
#[cfg(feature = "onnx")]
pub fn ort_environment() -> Option<&'static ort::environment::Environment> {
    static ENV: OnceLock<Option<ort::environment::Environment>> = OnceLock::new();
    ENV.get_or_init(|| match ort::init().with_name("dash").build() {
        Ok(env) => Some(env),
        Err(e) => {
            eprintln!("onnx runtime: failed to start ({e})");
            None
        }
    })
    .as_ref()
}

/// `potion-retrieval-32M` output dimensionality.
pub const EMBED_DIM: usize = 512;

/// The model files, inside the model dir.
const MODEL_FILES: [&str; 3] = ["tokenizer.json", "model.safetensors", "config.json"];

/// Absolute path to the bundled model dir (`embedding-runtime/`), resolved
/// from the Tauri resource dir once at startup. `embed` reads the model files
/// from here.
static MODEL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Record where the bundled model lives. Called once from app setup with
/// `app.path().resource_dir().join("embedding-runtime")`. Idempotent — a
/// second call is ignored.
pub fn set_model_dir(dir: PathBuf) {
    let _ = MODEL_DIR.set(dir);
}

/// The model dir: the one set at startup, else `embedding-runtime/` next to
/// the program. The fallback matters for the index workers — separate
/// processes that never run the app's setup, yet embed every document.
fn model_dir() -> Option<PathBuf> {
    MODEL_DIR.get().cloned().or_else(|| Some(std::env::current_exe().ok()?.parent()?.join("embedding-runtime")))
}

/// Whether the model files are there (cheap; doesn't load the model).
pub fn model_files_present() -> bool {
    model_dir().is_some_and(|dir| MODEL_FILES.iter().all(|file| dir.join(file).is_file()))
}

/// Split a document into overlapping word-window passages for per-chunk
/// embedding. `target_words` sizes each window near the model's sequence
/// limit; `overlap` re-includes trailing words so a match spanning a chunk
/// boundary isn't lost. If the document yields more than `max_chunks`
/// windows, the windows are evenly subsampled across the whole document
/// (not just the head) so long documents stay represented end-to-end while
/// the embedding cost stays bounded.
pub fn chunk_text(text: &str, target_words: usize, overlap: usize, max_chunks: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || target_words == 0 || max_chunks == 0 {
        return Vec::new();
    }
    if words.len() <= target_words {
        return vec![words.join(" ")];
    }
    let step = target_words.saturating_sub(overlap).max(1);
    let mut starts: Vec<usize> = (0..words.len()).step_by(step).collect();
    if let [_, .., last] = starts[..] {
        if words.len().saturating_sub(last) <= overlap {
            starts.pop();
        }
    }
    if starts.len() > max_chunks {
        let n = starts.len();
        let picked: Vec<usize> = (0..max_chunks).map(|i| starts[i * (n - 1) / (max_chunks - 1).max(1)]).collect();
        starts = picked;
        starts.dedup();
    }
    starts
        .into_iter()
        .map(|start| {
            let end = (start + target_words).min(words.len());
            words[start..end].join(" ")
        })
        .collect()
}

#[cfg(feature = "semantic")]
mod imp {
    use super::{model_dir, EMBED_DIM, MODEL_FILES};
    use model2vec_rs::model::StaticModel;
    use std::sync::LazyLock;

    /// Tokens read per text; longer passages are cut (Dash's are ~180 words).
    const MAX_TOKENS: usize = 512;

    static MODEL: LazyLock<Option<StaticModel>> = LazyLock::new(load_model);

    fn load_model() -> Option<StaticModel> {
        let dir = model_dir()?;
        if !MODEL_FILES.iter().all(|file| dir.join(file).is_file()) {
            eprintln!("embedding: model files not found in {}; semantic search disabled.", dir.display());
            return None;
        }
        match StaticModel::from_pretrained(&dir, None, Some(true), None) {
            Ok(model) if model.dim() == EMBED_DIM => Some(model),
            Ok(model) => {
                eprintln!(
                    "embedding: model has {} dimensions, expected {EMBED_DIM}; semantic search disabled.",
                    model.dim()
                );
                None
            }
            Err(error) => {
                eprintln!("embedding: failed to load the model ({error}); semantic search disabled.");
                None
            }
        }
    }

    pub fn is_available() -> bool {
        MODEL.is_some()
    }

    pub fn embed_query(text: &str) -> Option<Vec<f32>> {
        embed_docs(&[text])?.pop()
    }

    pub fn embed_docs(texts: &[&str]) -> Option<Vec<Vec<f32>>> {
        let model = MODEL.as_ref()?;
        let texts: Vec<String> = texts.iter().map(|text| (*text).to_string()).collect();
        Some(model.encode_with_args(&texts, Some(MAX_TOKENS), 256))
    }
}

#[cfg(not(feature = "semantic"))]
mod imp {
    pub fn is_available() -> bool {
        false
    }
    pub fn embed_query(_text: &str) -> Option<Vec<f32>> {
        None
    }
    pub fn embed_docs(_texts: &[&str]) -> Option<Vec<Vec<f32>>> {
        None
    }
}

/// Whether semantic embedding is compiled in AND the model loaded.
pub fn is_available() -> bool {
    imp::is_available()
}

/// Embed one piece of text (a query), or `None` if semantic search is
/// unavailable or the text is empty. Never panics.
pub fn embed(text: &str) -> Option<Vec<f32>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    imp::embed_query(trimmed)
}

/// Embed many passages at once (index/document side).
/// Returns one vector per input in order, or `None` if semantic search is
/// unavailable. Empty-after-trim inputs are dropped, so the output length may
/// be shorter than the input — callers pair vectors with chunks by filtering
/// the same way.
pub fn embed_batch(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    let trimmed: Vec<&str> = texts.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    imp::embed_docs(&trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads the real model: `DASH_TEST_MODEL_DIR=<embedding-runtime dir>
    /// cargo test -p dash-search --features semantic -- --ignored model`.
    #[cfg(feature = "semantic")]
    #[test]
    #[ignore = "needs the ~125 MB model; set DASH_TEST_MODEL_DIR"]
    fn model_ranks_by_meaning() {
        let dir = std::env::var("DASH_TEST_MODEL_DIR").expect("DASH_TEST_MODEL_DIR");
        set_model_dir(PathBuf::from(dir));
        assert!(model_files_present());
        let query = embed("how much did we spend on travel").expect("query vector");
        assert_eq!(query.len(), EMBED_DIM);
        let docs = embed_batch(&[
            "Flight and hotel expenses for the March business trip".to_string(),
            "Recipe for chocolate cake with vanilla frosting".to_string(),
        ])
        .expect("document vectors");
        let cosine = |a: &[f32], b: &[f32]| {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            dot / (a.iter().map(|x| x * x).sum::<f32>().sqrt() * b.iter().map(|x| x * x).sum::<f32>().sqrt())
        };
        let (travel, cake) = (cosine(&query, &docs[0]), cosine(&query, &docs[1]));
        assert!(travel > cake, "travel {travel} should beat cake {cake}");
    }

    #[test]
    fn short_text_is_one_chunk() {
        let chunks = chunk_text("just a few words here", 180, 30, 16);
        assert_eq!(chunks, vec!["just a few words here".to_string()]);
        assert!(chunk_text("", 180, 30, 16).is_empty());
    }

    #[test]
    fn long_text_windows_with_overlap_and_cap() {
        let text = (0..1000).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let chunks = chunk_text(&text, 100, 20, 8);
        assert!(!chunks.is_empty() && chunks.len() <= 8, "cap respected: {}", chunks.len());
        assert!(chunks[0].starts_with("w0 "));
        assert!(chunks.last().unwrap().contains("w999"));
    }

    #[cfg(feature = "semantic")]
    #[test]
    #[ignore = "loads the real bundled model; set DASH_TEST_MODEL_DIR and run with --ignored"]
    fn loads_the_bundled_model_and_embeds_sensibly() {
        let dir = PathBuf::from(std::env::var("DASH_TEST_MODEL_DIR").expect("DASH_TEST_MODEL_DIR"));
        set_model_dir(dir.clone());
        assert!(model_files_present(), "no model at {}", dir.display());

        assert!(is_available(), "model failed to load (semantic search would be silently OFF)");

        let vecs = embed_batch(&[
            "a small cat sat on the mat".to_string(),
            "a kitten rested on the rug".to_string(),
            "quarterly submarine propulsion economics".to_string(),
        ])
        .expect("embed_batch returned None");
        assert_eq!(vecs.len(), 3);
        for v in &vecs {
            assert_eq!(v.len(), EMBED_DIM, "wrong dimensionality");
            assert!(v.iter().all(|x| x.is_finite()), "non-finite values in embedding");
            assert!(v.iter().any(|&x| x != 0.0), "all-zero embedding");
        }

        let cos = |a: &Vec<f32>, b: &Vec<f32>| -> f32 {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            dot / (na * nb)
        };
        let related = cos(&vecs[0], &vecs[1]);
        let unrelated = cos(&vecs[0], &vecs[2]);
        assert!(related > unrelated + 0.10, "embedding quality too low: related={related:.3} unrelated={unrelated:.3}");
    }
}
