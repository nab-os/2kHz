//! Parity harness: same query as the Python CLI, track ids as JSON, so the two
//! can be diffed exactly.
//!
//!   cargo run --bin parity -- neighbours 1001 10
//!   cargo run --bin parity -- path 1001 1017
//!   cargo run --bin parity -- interpolate 1001 1017 8

use anyhow::{bail, Result};
use qsuggest::paths::Constraints;
use qsuggest::{default_data_dir, default_db_path, Engine};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        bail!("usage: parity <neighbours|path|interpolate> <args...>");
    }

    let mut engine = Engine::load(&default_data_dir(), &default_db_path())?;
    let constraints = Constraints::default();

    let ids: Vec<i64> = match args[0].as_str() {
        "neighbours" => {
            let track: i64 = args[1].parse()?;
            let k: usize = args.get(2).map_or(Ok(10), |v| v.parse())?;
            engine
                .navigator
                .neighbours(track, k, false)
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        }
        "radio" => {
            let track: i64 = args[1].parse()?;
            let steps: usize = args.get(2).map_or(Ok(10), |v| v.parse())?;
            let penalty: f32 = args.get(3).map_or(Ok(0.0), |v| v.parse())?;
            engine
                .navigator
                .radio_nearest(track, steps, penalty, &constraints)
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        }
        "path" => {
            let from: i64 = args[1].parse()?;
            let to: i64 = args[2].parse()?;
            engine
                .navigator
                .graph_path(from, to, 16)
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        }
        "interpolate" => {
            let from: i64 = args[1].parse()?;
            let to: i64 = args[2].parse()?;
            let steps: usize = args.get(3).map_or(Ok(12), |v| v.parse())?;
            engine
                .navigator
                .interpolate(from, to, steps, &constraints)
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        }
        "drift" => {
            let from: i64 = args[1].parse()?;
            let phrase = &args[2];
            let steps: usize = args.get(3).map_or(Ok(8), |v| v.parse())?;
            // Loaded here rather than through a backend: this harness
            // compares Rust against the Python oracle, so it should not depend
            // on how the app is wired.
            let embedding = text_encoder()?.embed(phrase)?;
            engine
                .navigator
                .drift_to_text(from, &embedding, steps, 5, &constraints)
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        }
        // Diagnostic: dump the raw text embedding so it can be compared with
        // the Python side value by value.
        "embed" => {
            let phrase = &args[1];
            let vector = text_encoder()?.embed(phrase)?;
            println!("{}", serde_json::to_string(&vector)?);
            return Ok(());
        }
        other => bail!("unknown mode: {other}"),
    };

    println!("{}", serde_json::to_string(&ids)?);
    Ok(())
}

/// The CLAP text tower, straight off disk.
fn text_encoder() -> anyhow::Result<qsuggest::text::TextEncoder> {
    let dir = qsuggest::default_model_dir();
    qsuggest::text::TextEncoder::load(&dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no text encoder in {}; run: uv run python -m qsuggest.features.onnx_export",
            dir.display()
        )
    })
}
