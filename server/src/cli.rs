//! The pipeline and the account, from the command line, as subcommands of
//! the server binary:
//!
//!   two-khz-server login              sign in through the browser
//!   two-khz-server crawl --max-tracks 5000
//!   two-khz-server analyse
//!   two-khz-server build-space
//!   two-khz-server layout
//!
//! The same code the pipeline view drives, so a stage started here and one
//! started from a client do the same thing.

use crate::pipeline::{analyse, assemble, demo, layout, models, Job, Paths};
use crate::qobuz::{Credentials, QobuzClient, DEFAULT_RATE_PER_SEC};
use crate::{crawl, db, login, stages};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::time::Duration;

pub const USAGE: &str = "\
account
  login [--timeout S] [--no-browser]   sign in through the browser, token into .env
  refresh-credentials [--write]        re-scrape the app id and secrets
  whoami                               check the credentials
  favorites [--kind tracks|albums|artists] [--limit N]

pipeline
  crawl [--max-tracks N] [--max-distance N] [--rate R] [--no-seed]
  crawl --artist ID | --album ID       one discography, or one tracklist
  analyse [--limit N] [--retry-failed] [--workers N] [--fetchers N] [--cache-gb G]
  build-space [--weights JSON]         e.g. --weights '{\"tempo\": 2.0}'
  layout [--neighbours N] [--min-dist D]
  models                               fetch the CLAP weights now, not on first use
  status                               row counts, and what is due
  evaluate                             do tracks from one album land together?
  demo DIR                             a synthetic corpus, no Qobuz needed

hiding
  block ARTIST [--reason R] [--all] [--purge]    by id, or part of a name
  unblock ARTIST
  blocked";

/// Arguments after the subcommand: `--flag value`, bare `--switch`, and
/// positionals.
struct Args<'a>(&'a [String]);

impl Args<'_> {
    fn value(&self, name: &str) -> Option<&str> {
        let at = self.0.iter().position(|a| a == name)?;
        self.0.get(at + 1).map(String::as_str)
    }

    fn parse<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        match self.value(name) {
            Some(v) => v.parse().map_err(|e| anyhow::anyhow!("{name} {v}: {e}")),
            None => Ok(default),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }

    /// The first argument that is neither a flag nor a flag's value.
    fn positional(&self, switches: &[&str]) -> Option<&str> {
        let mut skip = false;
        for arg in self.0 {
            if skip {
                skip = false;
                continue;
            }
            if arg.starts_with("--") {
                skip = !switches.contains(&arg.as_str());
                continue;
            }
            return Some(arg);
        }
        None
    }
}

/// Run `command` if it is one of these. `None` means not ours.
pub fn run(command: &str, args: &[String], paths: &Paths) -> Option<Result<()>> {
    let args = Args(args);
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting a runtime")
    };
    let block = |future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + '_>>| {
        runtime()?.block_on(future)
    };

    Some(match command {
        "login" => block(Box::pin(login_command(&args, paths))),
        "refresh-credentials" => block(Box::pin(refresh_credentials(&args, paths))),
        "whoami" => block(Box::pin(whoami(paths))),
        "favorites" | "favourites" => block(Box::pin(favourites(&args, paths))),
        "crawl" => block(Box::pin(crawl_command(&args, paths))),
        "analyse" | "analyze" => block(Box::pin(analyse_command(&args, paths))),
        "build-space" => block(Box::pin(build_space(&args, paths))),
        "layout" => layout_command(&args, paths),
        "models" => block(Box::pin(async {
            models::ensure_all(&paths.model_dir, &Job::stderr()).await?;
            println!("models ready in {}", paths.model_dir.display());
            Ok(())
        })),
        "status" => status(paths),
        "evaluate" => evaluate(paths),
        "demo" => block(Box::pin(demo_command(&args))),
        "block" => block_command(&args, paths),
        "unblock" => unblock_command(&args, paths),
        "blocked" => blocked(paths),
        _ => return None,
    })
}

// ------------------------------------------------------------------ account

fn client(paths: &Paths, rate: f64) -> Result<QobuzClient> {
    Ok(QobuzClient::new(Credentials::from_env(&paths.env_dir)?).with_rate(rate))
}

async fn whoami(paths: &Paths) -> Result<()> {
    let user = client(paths, DEFAULT_RATE_PER_SEC)?.user().await?;
    let field = |key: &str| user.get(key).and_then(|v| v.as_str()).unwrap_or("?").to_string();
    let credential = user.get("credential").cloned().unwrap_or_default();
    let subscription = credential
        .get("label")
        .or_else(|| credential.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("user id     : {}", user.get("id").map(|v| v.to_string()).unwrap_or_default());
    println!("email       : {}", field("email"));
    println!("display name: {}", field("display_name"));
    println!("subscription: {subscription}");
    Ok(())
}

async fn login_command(args: &Args<'_>, paths: &Paths) -> Result<()> {
    eprintln!("fetching the Qobuz web player bundle…");
    let bundle = login::fetch_bundle().await?;
    let timeout = Duration::from_secs(args.parse("--timeout", 300u64)?);
    let token = login::browser_login(&bundle, timeout, !args.has("--no-browser")).await?;
    login::update_env(
        &paths.env_dir,
        &[
            ("QOBUZ_APP_ID", bundle.app_id.clone()),
            ("QOBUZ_APP_SECRETS", bundle.secrets.join(",")),
            ("QOBUZ_USER_AUTH_TOKEN", token),
        ],
    )?;
    println!("logged in; token saved to {}", paths.env_dir.join(".env").display());
    whoami(paths).await
}

async fn refresh_credentials(args: &Args<'_>, paths: &Paths) -> Result<()> {
    eprintln!("fetching the Qobuz web player bundle…");
    let bundle = login::fetch_bundle().await?;
    let joined = bundle.secrets.join(",");
    if !args.has("--write") {
        println!("\nAdd these to your .env:\n");
        println!("QOBUZ_APP_ID={}", bundle.app_id);
        println!("QOBUZ_APP_SECRETS={joined}");
        println!("\n(re-run with --write to update the file in place)");
        return Ok(());
    }
    login::update_env(
        &paths.env_dir,
        &[("QOBUZ_APP_ID", bundle.app_id.clone()), ("QOBUZ_APP_SECRETS", joined)],
    )?;
    println!("updated .env with the app id and {} candidate secrets", bundle.secrets.len());
    Ok(())
}

async fn favourites(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let kind = args.value("--kind").unwrap_or("tracks");
    if !["tracks", "albums", "artists"].contains(&kind) {
        bail!("--kind is tracks, albums or artists");
    }
    let limit = args.parse("--limit", 0usize)?;
    let cap = if limit == 0 { usize::MAX } else { limit };
    let items = client(paths, DEFAULT_RATE_PER_SEC)?.favorites_raw(kind, cap).await?;

    let name = |v: &serde_json::Value, key: &str, field: &str| -> String {
        v.get(key)
            .and_then(|o| o.get(field))
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let text = |v: &serde_json::Value, key: &str| v.get(key).and_then(|s| s.as_str()).unwrap_or("?").to_string();
    for item in &items {
        let id = item.get("id").map(|v| v.to_string()).unwrap_or_default();
        match kind {
            "tracks" => println!(
                "{id:>12}  {} - {}  [{}]",
                name(item, "performer", "name"),
                text(item, "title"),
                name(item, "album", "title")
            ),
            "albums" => println!("{id:>12}  {} - {}", name(item, "artist", "name"), text(item, "title")),
            _ => println!("{id:>12}  {}", text(item, "name")),
        }
    }
    eprintln!("\n{} {kind} shown", items.len());
    Ok(())
}

// ----------------------------------------------------------------- pipeline

async fn crawl_command(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let conn = db::open_for_write(&paths.db_path)?;
    let mut client = client(paths, args.parse("--rate", DEFAULT_RATE_PER_SEC)?)?;
    client.login().await.context("signing in to Qobuz")?;

    if let Some(artist) = args.value("--artist") {
        let queued = crawl::discover_artist(&conn, &mut client, artist.parse()?).await?;
        println!("artist {artist}: {queued} albums queued");
        return Ok(());
    }
    if let Some(album) = args.value("--album") {
        let written = crawl::crawl_one_album(&conn, &mut client, album).await?;
        println!("album {album}: {written} tracks");
        return Ok(());
    }

    if !args.has("--no-seed") {
        eprintln!("seeding from favourites…");
        let seeded = crawl::seed(&conn, &mut client, 5000).await?;
        eprintln!(
            "  seeded {} tracks, {} albums, {} artists",
            seeded.tracks_added, seeded.albums_expanded, seeded.artists_expanded
        );
    }

    let max_tracks = args.parse("--max-tracks", 5000i64)?;
    let max_distance = args.parse("--max-distance", two_khz::api::DEFAULT_MAX_DISTANCE)?;
    eprintln!("crawling to {max_tracks} tracks, max {max_distance} hops");
    let report = |stats: &crawl::Stats, tracks: i64| {
        eprintln!(
            "  artists={} albums={} tracks={tracks} errors={}",
            stats.artists_expanded, stats.albums_expanded, stats.errors
        );
    };
    let stats = crawl::crawl(&conn, &mut client, max_tracks, max_distance, Some(&report)).await?;
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

async fn analyse_command(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let options = analyse::Options {
        limit: args.parse("--limit", 0)?,
        retry_failed: args.has("--retry-failed"),
        cache_gb: args.parse("--cache-gb", 20.0)?,
        workers: args.parse("--workers", 0)?,
        fetchers: args.parse("--fetchers", 0)?,
    };
    let limiter = client(paths, DEFAULT_RATE_PER_SEC)?.rate_limit();
    analyse::run(paths, limiter, &options, &Job::stderr()).await?;
    Ok(())
}

async fn build_space(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let weights: Option<HashMap<String, f32>> = args
        .value("--weights")
        .map(serde_json::from_str)
        .transpose()
        .context("--weights is a JSON object of block name to weight")?;
    if let Some(unknown) = weights
        .iter()
        .flat_map(|w| w.keys())
        .find(|k| !assemble::BLOCKS.contains(&k.as_str()))
    {
        bail!("no block called {unknown}; the blocks are {}", assemble::BLOCKS.join(", "));
    }
    assemble::run(paths, weights, &Job::stderr()).await?;
    Ok(())
}

fn layout_command(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let options = layout::Options {
        neighbours: args.parse("--neighbours", 15)?,
        min_dist: args.parse("--min-dist", 0.1)?,
        epochs: args.parse("--epochs", 0)?,
    };
    layout::run(paths, &options, &Job::stderr())?;
    Ok(())
}

fn status(paths: &Paths) -> Result<()> {
    let corpus = stages::corpus(&paths.db_path)?;
    let conn = db::open_for_write(&paths.db_path)?;
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(0)
    };
    for (label, n) in [
        ("artists", count("artists")),
        ("albums", count("albums")),
        ("tracks", corpus.tracks),
        ("features", corpus.analysed),
        ("layout", count("layout")),
        ("failures", corpus.failed),
        ("frontier_pending", corpus.pending),
        ("to_analyse", corpus.to_analyse),
    ] {
        println!("{label:<18} {n:>8}");
    }
    if let Ok(space) = two_khz::space::Space::load(&paths.data_dir) {
        let m = &space.manifest;
        println!("{:<18} {} x {} (built {})", "space", m.n_tracks, m.n_dims, m.built_at);
    }
    Ok(())
}

/// Space sanity: do tracks from one album land near each other? If not, the
/// space is noise and no path logic will rescue it. Worth watching whenever
/// the weights or labels change.
fn evaluate(paths: &Paths) -> Result<()> {
    let space = two_khz::space::Space::load(&paths.data_dir)?;
    let weighted = space.weighted(&space.default_weights());
    let conn = db::open_for_write(&paths.db_path)?;
    let albums: HashMap<i64, String> = conn
        .prepare("SELECT id, album_id FROM tracks WHERE album_id IS NOT NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut by_album: HashMap<&str, Vec<usize>> = HashMap::new();
    for (row, id) in space.manifest.track_ids.iter().enumerate() {
        if let Some(album) = albums.get(id) {
            by_album.entry(album).or_default().push(row);
        }
    }
    by_album.retain(|_, rows| rows.len() > 1);
    if by_album.is_empty() {
        bail!("not enough multi-track albums to evaluate");
    }

    let mut ranks = Vec::new();
    for rows in by_album.values() {
        for &row in rows {
            let mut scores = weighted.similarities(weighted.row(row));
            scores[row] = f32::NEG_INFINITY;
            let order = two_khz::space::argsort_desc(&scores);
            let rank = order
                .iter()
                .position(|j| *j != row && rows.contains(j))
                .map_or(order.len(), |p| p + 1);
            ranks.push(rank);
        }
    }
    ranks.sort_unstable();
    let top1 = ranks.iter().filter(|&&r| r == 1).count();
    let median = ranks[ranks.len() / 2];
    let mean = ranks.iter().sum::<usize>() as f64 / ranks.len() as f64;
    println!("albums evaluated      : {}", by_album.len());
    println!("tracks evaluated      : {}", ranks.len());
    println!(
        "same-album is top-1   : {top1}/{} ({:.1}%)",
        ranks.len(),
        100.0 * top1 as f64 / ranks.len() as f64
    );
    println!("median rank           : {median}");
    println!("mean rank             : {mean:.1}  (of {} tracks)", weighted.n_tracks);
    Ok(())
}

async fn demo_command(args: &Args<'_>) -> Result<()> {
    let Some(dir) = args.positional(&[]) else {
        bail!("demo needs a directory: two-khz-server demo /tmp/two-khz-demo");
    };
    let root = std::path::absolute(dir)?;
    let paths = demo::build(&root, &Job::stderr()).await?;
    println!(
        "\ndemo corpus ready in {}\nserve it with:\n  TWO_KHZ_DATA_DIR={} two-khz-server serve",
        paths.data_dir.display(),
        paths.data_dir.display()
    );
    Ok(())
}

// ------------------------------------------------------------------- hiding

fn block_command(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let switches = ["--all", "--purge"];
    let Some(needle) = args.positional(&switches) else {
        bail!("block needs an artist id or part of a name");
    };
    let conn = db::open_for_write(&paths.db_path)?;
    let matches = db::resolve_artist(&conn, needle)?;
    if matches.is_empty() {
        bail!("no artist matching {needle:?} in the catalogue; block by numeric id if they have not been crawled yet");
    }
    // Blocking the wrong person is worse than an extra keystroke.
    if matches.len() > 1 && !args.has("--all") {
        eprintln!("{} artists match {needle:?}:", matches.len());
        for m in &matches {
            eprintln!("  {:>10}  {}  ({} tracks)", m.id, m.name, m.tracks);
        }
        bail!("re-run with an id, or --all to block every match");
    }

    let reason = args.value("--reason");
    for m in &matches {
        db::block_artist(&paths.db_path, m.id, &m.name, reason)?;
        println!("blocked {} ({})", m.name, m.id);
        if args.has("--purge") {
            let purged = db::purge(&conn, m.id)?;
            println!(
                "  purged {} tracks, {} features, {} albums",
                purged.tracks, purged.features, purged.albums
            );
        }
    }
    if args.has("--purge") {
        println!("\nRe-run build-space and layout to drop them from the map.");
    } else {
        println!("\nHidden everywhere from now on; no rebuild needed.");
    }
    Ok(())
}

fn unblock_command(args: &Args<'_>, paths: &Paths) -> Result<()> {
    let Some(needle) = args.positional(&[]) else {
        bail!("unblock needs an artist id or part of a name");
    };
    let blocked = db::blocked_artists(&paths.db_path)?;
    let lifted: Vec<_> = blocked
        .iter()
        .filter(|b| {
            b.artist_id.to_string() == needle
                || b.name.to_lowercase().contains(&needle.to_lowercase())
        })
        .collect();
    if lifted.is_empty() {
        bail!("no block found for {needle:?}");
    }
    for b in lifted {
        db::unblock_artist(&paths.db_path, b.artist_id)?;
        println!("unblocked {} ({})", b.name, b.artist_id);
    }
    println!("Purged data does not come back; re-crawl to restore it.");
    Ok(())
}

fn blocked(paths: &Paths) -> Result<()> {
    let rows = db::blocked_artists(&paths.db_path)?;
    if rows.is_empty() {
        println!("no artists blocked");
    }
    for b in rows {
        let reason = b.reason.map(|r| format!("  - {r}")).unwrap_or_default();
        println!("{:>10}  {}{reason}", b.artist_id, b.name);
    }
    Ok(())
}
