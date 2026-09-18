//! Headless crawler, the Rust replacement for the Python one.
//!
//!   cargo run --release --bin crawl -- --max-tracks 5000
//!   cargo run --release --bin crawl -- --no-seed --max-distance 1
//!
//! Writes the same tables, so either can resume what the other started.

use anyhow::{Context, Result};
use two_khz::crawl;
use two_khz::qobuz::{QobuzClient, DEFAULT_RATE_PER_SEC};

struct Options {
    max_tracks: i64,
    max_distance: i64,
    rate: f64,
    seed: bool,
    /// Fetch just this artist's discography, or this album's tracklist, and stop.
    only_artist: Option<i64>,
    only_album: Option<String>,
}

fn parse_args() -> Result<Options> {
    let mut options = Options {
        max_tracks: 5000,
        max_distance: crawl::DEFAULT_MAX_DISTANCE,
        rate: DEFAULT_RATE_PER_SEC,
        seed: true,
        only_artist: None,
        only_album: None,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| -> Result<String> {
            args.get(i + 1)
                .cloned()
                .with_context(|| format!("{} needs a value", args[i]))
        };
        match args[i].as_str() {
            "--max-tracks" => {
                options.max_tracks = value(i)?.parse()?;
                i += 2;
            }
            "--max-distance" => {
                options.max_distance = value(i)?.parse()?;
                i += 2;
            }
            "--rate" => {
                options.rate = value(i)?.parse()?;
                i += 2;
            }
            "--no-seed" => {
                options.seed = false;
                i += 1;
            }
            "--artist" => {
                options.only_artist = Some(value(i)?.parse()?);
                i += 2;
            }
            "--album" => {
                options.only_album = Some(value(i)?);
                i += 2;
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: crawl [--max-tracks N] [--max-distance N] [--rate R] [--no-seed]\n\
                     \x20      crawl --artist ID      fetch one discography and queue it\n\
                     \x20      crawl --album ID       fetch one tracklist"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(options)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let options = parse_args()?;
    let db_path = two_khz::default_db_path();

    // Creates the tables if this is a fresh checkout: the crawler writes them,
    // so it must not depend on a Python command having run first.
    let conn = two_khz::db::open_for_write(&db_path)?;

    let mut client = QobuzClient::from_repo(&two_khz::qobuz::repo_root())?.with_rate(options.rate);
    client.login().await.context("signing in to Qobuz")?;

    // Targeted fetches: what the app's "analyse" button runs.
    if let Some(artist_id) = options.only_artist {
        let queued = crawl::discover_artist(&conn, &mut client, artist_id).await?;
        println!("artist {artist_id}: {queued} albums queued");
        return Ok(());
    }
    if let Some(album_id) = options.only_album {
        let written = crawl::crawl_one_album(&conn, &mut client, &album_id).await?;
        println!("album {album_id}: {written} tracks");
        return Ok(());
    }

    if options.seed {
        eprintln!("seeding from favourites…");
        let seeded = crawl::seed(&conn, &mut client, 5000).await?;
        eprintln!(
            "  seeded {} tracks, {} albums, {} artists",
            seeded.tracks_added, seeded.albums_expanded, seeded.artists_expanded
        );
    }

    eprintln!(
        "crawling to {} tracks, max {} hops, {:.1} req/s",
        options.max_tracks, options.max_distance, options.rate
    );

    let report = |stats: &crawl::Stats, tracks: i64| {
        eprintln!(
            "  artists={} albums={} tracks={} errors={}",
            stats.artists_expanded, stats.albums_expanded, tracks, stats.errors
        );
    };

    let stats = crawl::crawl(
        &conn,
        &mut client,
        options.max_tracks,
        options.max_distance,
        Some(&report),
    )
    .await?;

    println!(
        "expanded {} artists and {} albums, {} tracks added, {} errors, {} blocked",
        stats.artists_expanded,
        stats.albums_expanded,
        stats.tracks_added,
        stats.errors,
        stats.blocked_skipped
    );
    Ok(())
}
