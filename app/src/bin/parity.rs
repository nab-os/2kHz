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
            engine
                .drift_by_text(from, phrase, steps, 5)?
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        }
        // Diagnostic: dump the raw text embedding so it can be compared with
        // the Python side value by value.
        "embed" => {
            let phrase = &args[1];
            let encoder = engine
                .text_encoder
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no text encoder"))?;
            let vector = encoder.embed(phrase)?;
            println!("{}", serde_json::to_string(&vector)?);
            return Ok(());
        }
        other => bail!("unknown mode: {other}"),
    };

    println!("{}", serde_json::to_string(&ids)?);
    Ok(())
}
