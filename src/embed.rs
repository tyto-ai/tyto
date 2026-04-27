use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, ModelTrait, TextEmbedding};

pub const DIMS: usize = 384;

const MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;

/// The HuggingFace model code for the active embedding model.
/// If DIMS is updated, write a schema migration to resize F32_BLOB accordingly.
pub fn model_id() -> String {
    EmbeddingModel::get_model_info(&MODEL)
        .map(|info| info.model_code.clone())
        .unwrap_or_else(|| MODEL.to_string())
}

pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    pub fn load() -> Result<Self> {
        let cache_dir = if let Ok(dir) = std::env::var("COREE_MODEL_DIR") {
            std::path::PathBuf::from(dir)
        } else {
            let dir = dirs::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from(".cache"))
                .join("coree")
                .join("models");
            if !dir.exists() {
                eprintln!("[coree] Downloading embedding model on first run. This may take a moment...");
            }
            dir
        };

        // COREE_FORCE_MODEL_REFRESH=1: delete the model cache before loading so
        // fastembed re-downloads a fresh copy. Useful for troubleshooting a
        // corrupted model or testing the cold-start download path locally.
        if std::env::var("COREE_FORCE_MODEL_REFRESH").as_deref() == Ok("1") && cache_dir.exists() {
            std::fs::remove_dir_all(&cache_dir)
                .context("COREE_FORCE_MODEL_REFRESH: failed to remove model cache")?;
        }

        let model = TextEmbedding::try_new(
            InitOptions::new(MODEL)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )
        .context("Failed to load embedding model")?;

        Ok(Self { model })
    }

    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let t = std::time::Instant::now();
        let results = self
            .model
            .embed(vec![text], None)
            .context("Embedding failed")?;
        tracing::debug!(
            elapsed_ms = t.elapsed().as_millis(),
            chars = text.len(),
            "embed"
        );
        results
            .into_iter()
            .next()
            .context("Embedding model returned no results")
    }
}

/// Encode a float slice as a little-endian byte blob for libsql vector storage.
/// Shared by store and retrieve to avoid duplication.
pub fn floats_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode a little-endian byte blob back to a float slice. Inverse of floats_to_blob.
pub fn blob_to_floats(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_to_blob_roundtrip() {
        let floats = vec![1.0f32, 2.0f32, -3.5f32];
        let blob = floats_to_blob(&floats);
        assert_eq!(blob.len(), 12); // 3 floats * 4 bytes each
        let decoded: Vec<f32> = blob
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, floats);
    }
}
