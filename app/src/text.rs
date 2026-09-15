//! CLAP text embeddings at query time, via ONNX.
//!
//! Only the text tower is shipped, the audio tower runs during analysis.
//! Requires `data/clap_text.onnx` and `data/clap_tokenizer.json`, from
//! `uv run python -m qsuggest.features.onnx_export`.

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::TensorRef;
use std::path::Path;
use tokenizers::Tokenizer;

pub const ONNX_NAME: &str = "clap_text.onnx";
pub const TOKENIZER_NAME: &str = "clap_tokenizer.json";

pub struct TextEncoder {
    session: Session,
    tokenizer: Tokenizer,
}

impl TextEncoder {
    /// Returns Ok(None) when the model has not been exported, so the app can
    /// simply hide text steering rather than refusing to start.
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let onnx_path = data_dir.join(ONNX_NAME);
        let tokenizer_path = data_dir.join(TOKENIZER_NAME);
        if !onnx_path.is_file() || !tokenizer_path.is_file() {
            return Ok(None);
        }

        let session = Session::builder()
            .context("creating ONNX session builder")?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("loading {}", onnx_path.display()))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", tokenizer_path.display()))?;

        Ok(Some(Self { session, tokenizer }))
    }

    /// L2-normalised 512-d embedding, in the same space as the stored audio
    /// embeddings.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenising {text:?}: {e}"))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&v| v as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&v| v as i64)
            .collect();
        let shape = [1_i64, ids.len() as i64];

        let outputs = self.session.run(ort::inputs![
            "input_ids" => TensorRef::from_array_view((shape, ids.as_slice()))?,
            "attention_mask" => TensorRef::from_array_view((shape, mask.as_slice()))?,
        ])?;

        let (_shape, data) = outputs["text_embedding"].try_extract_tensor::<f32>()?;
        let mut vector = data.to_vec();

        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        for v in vector.iter_mut() {
            *v /= norm;
        }
        Ok(vector)
    }
}
