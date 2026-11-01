//! Runs in any app that is a `dash_core::Host`.
//! Text embeddings for semantic search.
//!
//! Wraps a bundled EmbeddingGemma 2 ONNX model (text-only, q4, 768-d) via
//! `ort` + `tokenizers` directly. The model is an asymmetric encoder —
//! queries get a "task: search result | query: " prefix and documents get
//! "title: none | text: ". Leaving out the prefixes silently lowers quality.
//!
//! Text-only: the model accepts optional image, video and audio features;
//! we pass empty tensors so those encoders are never exercised. The output
//! vectors are identical to the full multimodal pipeline's text vectors.
//!
//! RAM: ~310 MB peak, ~300 MB idle. The native tokenizer keeps ~70 MB
//! (vs ~300 MB for a JS tokenizer), and with ONNX Runtime's memory arena
//! disabled the activation buffers are freed after each run instead of kept
//! (~700 MB savings).
//!
//! Gating mirrors the other native-runtime features (`vosk`, `ocr-leptess`,
//! `screenrec`): the real implementation is behind the `semantic` cargo
//! feature, which pulls `ort` + `tokenizers`. Builds without the feature
//! get inert stubs (`embed` → `None`, `is_available` → `false`), so
//! semantic search is simply absent, never a build break.
//!
//! Everything here is offline: the model + tokenizer are read from the
//! bundled resource dir (`embedding-runtime/`, set via `set_model_dir` at
//! startup). No HuggingFace download, no network.
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

/// EmbeddingGemma 2 output dimensionality.
pub const EMBED_DIM: usize = 768;

/// Absolute path to the bundled model dir (`embedding-runtime/`), resolved
/// from the Tauri resource dir once at startup. `embed` reads the ONNX +
/// tokenizer files from here.
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
    MODEL_DIR
        .get()
        .cloned()
        .or_else(|| Some(std::env::current_exe().ok()?.parent()?.join("embedding-runtime")))
}

/// Whether the model files are there (cheap; doesn't load the model).
pub fn model_files_present() -> bool {
    model_dir().is_some_and(|dir| dir.join("tokenizer.json").is_file() && dir.join("onnx").join("model_q4.onnx").is_file())
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
    if starts.len() >= 2 {
        let last = *starts.last().unwrap();
        if words.len().saturating_sub(last) <= overlap {
            starts.pop();
        }
    }
    if starts.len() > max_chunks {
        let n = starts.len();
        let picked: Vec<usize> = (0..max_chunks)
            .map(|i| starts[i * (n - 1) / (max_chunks - 1).max(1)])
            .collect();
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
    use super::{model_dir, EMBED_DIM};
    use ort::session::Session;
    use ort::value::Tensor;
    use std::sync::{LazyLock, Mutex};
    use tokenizers::Tokenizer;

    const QUERY_PREFIX: &str = "task: search result | query: ";
    const DOC_PREFIX: &str = "title: none | text: ";

    struct EmbedModel {
        session: Session,
        tokenizer: Tokenizer,
    }

    static MODEL: LazyLock<Mutex<Option<EmbedModel>>> =
        LazyLock::new(|| Mutex::new(load_model()));

    fn load_model() -> Option<EmbedModel> {
        let dir = model_dir()?;
        let tokenizer_path = dir.join("tokenizer.json");
        let model_path = dir.join("onnx").join("model_q4.onnx");
        if !tokenizer_path.exists() || !model_path.exists() {
            eprintln!("embedding: model files not found in {}; semantic search disabled.", dir.display());
            return None;
        }

        let mut tokenizer = match Tokenizer::from_file(&tokenizer_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("embedding: failed to load tokenizer ({e}); semantic search disabled.");
                return None;
            }
        };
        tokenizer
            .with_padding(Some(tokenizers::PaddingParams::default()));
        if let Err(e) = tokenizer.with_truncation(Some(tokenizers::TruncationParams {
            max_length: 2048,
            ..Default::default()
        })) {
            eprintln!("embedding: failed to set truncation ({e}); continuing anyway.");
        }

        let Some(env) = super::ort_environment() else {
            return None;
        };
        let session = match Session::builder(env).and_then(|mut b| b.commit_from_file(&model_path)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("embedding: failed to load ONNX model ({e}); semantic search disabled.");
                return None;
            }
        };

        Some(EmbedModel { session, tokenizer })
    }

    pub fn is_available() -> bool {
        MODEL.lock().map(|m| m.is_some()).unwrap_or(false)
    }

    fn make_empty_features() -> ort::Result<Tensor<f32>> {
        Tensor::<f32>::from_array(([0usize, 512usize], Vec::<f32>::new().into_boxed_slice()))
    }

    fn raw_embed(model: &mut EmbedModel, texts: &[&str]) -> Option<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Some(Vec::new());
        }
        let mut all_vecs = Vec::new();

        for chunk_start in (0..texts.len()).step_by(16) {
            let batch = &texts[chunk_start..(chunk_start + 16).min(texts.len())];
            let encodings = model.tokenizer.encode_batch(batch.to_vec(), true).ok()?;
            let batch_size = encodings.len();
            let seq_len = encodings[0].get_ids().len();

            let mut ids_flat = Vec::with_capacity(batch_size * seq_len);
            let mut mask_flat = Vec::with_capacity(batch_size * seq_len);
            for enc in &encodings {
                ids_flat.extend(enc.get_ids().iter().map(|&id| id as i64));
                mask_flat.extend(enc.get_attention_mask().iter().map(|&m| m as i64));
            }

            let input_ids = Tensor::<i64>::from_array((
                [batch_size, seq_len],
                ids_flat.into_boxed_slice(),
            )).ok()?;
            let attention_mask = Tensor::<i64>::from_array((
                [batch_size, seq_len],
                mask_flat.into_boxed_slice(),
            )).ok()?;

            let outputs = model.session.run(
                ort::inputs![
                    "input_ids" => input_ids,
                    "attention_mask" => attention_mask,
                    "image_features" => make_empty_features().ok()?,
                    "video_features" => make_empty_features().ok()?,
                    "audio_features" => make_empty_features().ok()?
                ]
            ).ok()?;

            let (shape, data) = outputs["sentence_embedding"]
                .try_extract_tensor::<f32>()
                .ok()?;

            let out_dim = if shape.len() == 2 { shape[1] as usize } else { 0 };
            if out_dim != EMBED_DIM {
                eprintln!("embedding: unexpected output dim {out_dim}, expected {EMBED_DIM}");
                return None;
            }

            for j in 0..batch_size {
                let start = j * EMBED_DIM;
                let end = start + EMBED_DIM;
                if end > data.len() {
                    return None;
                }
                let v: Vec<f32> = data[start..end].to_vec();
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm <= 0.0 {
                    all_vecs.push(v);
                } else {
                    all_vecs.push(v.iter().map(|x| x / norm).collect());
                }
            }
        }

        Some(all_vecs)
    }

    pub fn embed_query(text: &str) -> Option<Vec<f32>> {
        let prefixed = format!("{}{}", QUERY_PREFIX, text);
        let mut guard = MODEL.lock().ok()?;
        let model = guard.as_mut()?;
        let mut vecs = raw_embed(model, &[&prefixed])?;
        vecs.pop()
    }

    pub fn embed_docs(texts: &[&str]) -> Option<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Some(Vec::new());
        }
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{}{}", DOC_PREFIX, t)).collect();
        let refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
        let mut guard = MODEL.lock().ok()?;
        let model = guard.as_mut()?;
        raw_embed(model, &refs)
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

/// Embed one piece of text (query side) with the query prefix, or `None` if
/// semantic search is unavailable or the text is empty. Never panics.
pub fn embed(text: &str) -> Option<Vec<f32>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    imp::embed_query(trimmed)
}

/// Embed many passages at once (index/document side) with the document prefix.
/// Returns one vector per input in order, or `None` if semantic search is
/// unavailable. Empty-after-trim inputs are dropped, so the output length may
/// be shorter than the input — callers pair vectors with chunks by filtering
/// the same way.
pub fn embed_batch(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    let trimmed: Vec<&str> = texts
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
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
    #[ignore = "needs the ~200 MB model; set DASH_TEST_MODEL_DIR"]
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
    #[ignore = "loads the real bundled ONNX model; run with --ignored"]
    fn loads_the_bundled_model_and_embeds_sensibly() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("embedding-runtime");
        assert!(dir.join("onnx").join("model_q4.onnx").exists(), "no model at {}", dir.display());
        set_model_dir(dir);

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
        assert!(
            related > unrelated + 0.10,
            "embedding quality too low: related={related:.3} unrelated={unrelated:.3}"
        );
    }
}
