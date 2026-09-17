//! The root component, and the startup it needs.
//!
//! In the library rather than `main.rs` because an Android build has no
//! `main`: the APK loads the .so and calls `start_app`.

use crate::backend::{self, backend};
use crate::qobuz::FORMAT_MP3_320;
use crate::ui::{
    Blocklist, Crawler, GeneratePanel, Generator, Library, LibraryPanel, LocalIds, Pipeline,
    PipelineView, Player, PlayerBar, Selection,
};
use crate::{engine, map, Wiring};
use crate::platform::wry::http::Response;
use dioxus::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

/// Where the engine loads the space from. Fixed by `bootstrap`: the repo's
/// `data/` locally, a synced copy when paired.
static DATA_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
static DB_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

pub(crate) fn data_dir() -> &'static std::path::Path {
    DATA_DIR.get().expect("set by bootstrap")
}

pub(crate) fn db_path() -> &'static std::path::Path {
    DB_PATH.get().expect("set by bootstrap")
}

/// Wire up the backend, sync if remote, and load the space. Fallible but not
/// fatal: a desktop says what to run, a phone shows the setup screen.
pub fn bootstrap() -> anyhow::Result<()> {
    let wiring = Wiring::from_env()?;

    let data_dir = wiring.data_dir().to_path_buf();
    let db_path = wiring.db_path();
    let remote = wiring.is_remote();
    let _ = DATA_DIR.set(data_dir.clone());
    let _ = DB_PATH.set(db_path.clone());

    backend::init(wiring.into_backend());

    // Sync before the engine loads: it memory-maps what it finds and will not
    // look again until told to.
    if remote {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(backend().sync_space())?;
    }

    crate::init_engine(&data_dir, &db_path)
}

/// Re-run the parts of `bootstrap` a freshly paired device needs. Split out
/// because `backend::init` is a `OnceLock` the setup screen has already filled.
pub async fn sync_and_load() -> anyhow::Result<()> {
    backend().sync_space().await?;
    let data_dir = crate::client_data_dir();
    let db_path = data_dir.join("catalog.db");
    let _ = DATA_DIR.set(data_dir.clone());
    let _ = DB_PATH.set(db_path.clone());
    crate::init_engine(&data_dir, &db_path)
}

/// The root: either the app, or the screen that gets you to the app.
///
/// Two components rather than an early return, Dioxus counts hooks per
/// scope, and changing that count between renders panics.
#[component]
pub fn App() -> Element {
    let ready = use_signal(crate::engine_ready);

    rsx! {
        style { {include_str!("../assets/style.css")} }
        if ready() {
            Shell {}
        } else {
            Setup { ready }
        }
    }
}

/// Pairing, for a device that cannot be configured with environment variables.
/// A phone has no shell to export a server address in, so it is typed once and
/// written beside the synced space.
#[component]
fn Setup(ready: Signal<bool>) -> Element {
    let mut address = use_signal(|| {
        crate::ServerConfig::load()
            .map(|c| c.base)
            .unwrap_or_else(|| "http://".into())
    });
    let mut token = use_signal(String::new);
    let mut status = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);

    let connect = move |_| {
        let base = address.peek().trim().trim_end_matches('/').to_string();
        let secret = token.peek().trim().to_string();
        if base.is_empty() || secret.is_empty() {
            status.set(Some("Both the address and the token are needed.".into()));
            return;
        }

        busy.set(true);
        status.set(Some("connecting…".into()));

        spawn(async move {
            let config = crate::ServerConfig {
                base: base.clone(),
                token: secret.clone(),
            };

            // Save first: `backend::init` is a OnceLock, so a second attempt
            // here would not replace a wrong address.
            if let Err(err) = config.save() {
                status.set(Some(format!("could not save the pairing: {err:#}")));
                busy.set(false);
                return;
            }

            backend::init(Wiring::remote(base, secret).into_backend());

            match crate::app::sync_and_load().await {
                Ok(()) => {
                    status.set(None);
                    ready.set(true);
                }
                Err(err) => {
                    status.set(Some(format!(
                        "{err:#}\n\nIf the address and token are right, the server may not have \
                         built a space yet, run build-space and layout on it."
                    )));
                    busy.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "setup",
            h1 { "Connect to your server" }
            p { class: "muted",
                "This device navigates the space on its own, but the catalogue, playback and \
                 the pipeline live on a machine that can host them. Pair one with "
                code { "qsuggest-server pair --name phone --scope play" }
                "."
            }

            label { "Server address" }
            input {
                class: "search",
                placeholder: "http://nas.tailnet.ts.net:7700",
                value: "{address}",
                oninput: move |event| address.set(event.value()),
            }

            label { "Device token" }
            input {
                class: "search",
                placeholder: "what `pair` printed",
                value: "{token}",
                oninput: move |event| token.set(event.value()),
            }

            div { class: "actions",
                button {
                    class: "primary",
                    disabled: busy(),
                    onclick: connect,
                    if busy() { "connecting…" } else { "connect" }
                }
            }

            if let Some(message) = status.read().clone() {
                pre { class: "log", "{message}" }
            }
        }
    }
}

#[component]
fn Shell() -> Element {
    let mut query = use_signal(String::new);
    let mut weights = use_signal(|| engine().lock().unwrap().space.default_weights());

    // Shared with the Qobuz panel, which can select a track it recognises.
    let selection = use_context_provider(|| Selection(Signal::new(None)));
    let mut selected = selection.0;

    let player = use_context_provider(|| Player {
        queue: Signal::new(Vec::new()),
        index: Signal::new(0),
        playing: Signal::new(false),
        position: Signal::new((0.0, 0.0)),
        quality: Signal::new(FORMAT_MP3_320),
        status: Signal::new(None),
    });

    let library = use_context_provider(Library::new);

    let generator = use_context_provider(Generator::new);
    let crawler = use_context_provider(Crawler::new);
    let pipeline = use_context_provider(Pipeline::new);

    // The crawl and pipeline publish status rather than writing these signals,
    // a server cannot reach into a Dioxus scope.
    crate::ui::crawler::use_crawl_status(crawler);
    crate::ui::pipeline::use_pipeline_watch(pipeline);

    // Explore is the map and space tools; Pipeline builds the corpus. One
    // window because they share the player and the database.
    let mut explore = use_signal(|| true);

    // A rebuilt space means the loaded one is stale. Remotely the bytes have
    // to come down first; `sync_space` is a no-op locally, so this stays one
    // path.
    use_effect(move || {
        let generation = *pipeline.generation.read();

        spawn(async move {
            if generation > 0 {
                match backend().sync_space().await {
                    Ok(_) => match crate::reload_engine(data_dir(), db_path()) {
                        Ok(()) => {
                            selected.set(None);
                            generator.clear();
                            document::eval(
                                "window.qsuggestReloadPoints && window.qsuggestReloadPoints();",
                            );
                        }
                        Err(err) => eprintln!("could not reload the rebuilt space: {err:#}"),
                    },
                    Err(err) => eprintln!("could not sync the rebuilt space: {err:#}"),
                }
            }

            // Only the shell sees the engine, and "how many points are drawn"
            // is about the loaded space, not the database.
            let (in_space, on_map) = {
                let guard = engine().lock().unwrap();
                let catalog = &guard.navigator.catalog;
                (
                    catalog.visible().count() as i64,
                    catalog
                        .visible()
                        .filter(|&i| catalog.get(i).x.is_some())
                        .count() as i64,
                )
            };
            let mut pipeline = pipeline;
            pipeline.space_counts.set((in_space, on_map));
        });
    });

    // Write, refresh the engine's filter, redraw, all three, or the halves
    // disagree. The engine is told the ids because remotely there is no local
    // database to re-read.
    let blocked = use_signal(Vec::new);

    use_future(move || async move {
        let mut blocked = blocked;
        if let Ok(found) = backend().blocked_artists().await {
            engine()
                .lock()
                .unwrap()
                .set_blocked(found.iter().map(|a| a.artist_id).collect());
            blocked.set(found);
        }
    });

    let apply_block = use_callback(move |(artist_id, name): (i64, String)| {
        spawn(async move {
            let mut blocked = blocked;
            if let Err(err) = backend().block_artist(artist_id, &name).await {
                eprintln!("could not hide artist {artist_id}: {err:#}");
                return;
            }
            if let Ok(found) = backend().blocked_artists().await {
                engine()
                    .lock()
                    .unwrap()
                    .set_blocked(found.iter().map(|a| a.artist_id).collect());
                blocked.set(found);
            }
            document::eval("window.qsuggestReloadPoints && window.qsuggestReloadPoints();");
        });
    });

    let lift_block = use_callback(move |artist_id: i64| {
        spawn(async move {
            let mut blocked = blocked;
            if let Err(err) = backend().unblock_artist(artist_id).await {
                eprintln!("could not unhide artist {artist_id}: {err:#}");
                return;
            }
            if let Ok(found) = backend().blocked_artists().await {
                engine()
                    .lock()
                    .unwrap()
                    .set_blocked(found.iter().map(|a| a.artist_id).collect());
                blocked.set(found);
            }
            document::eval("window.qsuggestReloadPoints && window.qsuggestReloadPoints();");
        });
    });

    use_context_provider(|| Blocklist {
        artists: blocked,
        block: apply_block,
        unblock: lift_block,
    });

    // Recomputed when the block list changes: a hidden track should stop
    // offering to locate itself on the map.
    let local_ids = use_memo(move || {
        blocked.read();
        pipeline.generation.read();
        Rc::new(engine().lock().unwrap().navigator.catalog.id_set())
    });
    use_context_provider(|| LocalIds(local_ids));

    crate::ui::use_transport(player);

    // Open on the user's own library, which is also what the crawl seeds from.
    use_future(move || async move {
        crate::ui::open_initial(library);
    });

    // Bulk point data crosses as binary here, never through eval.
    crate::platform::use_asset_handler("points", move |request, responder| {
        let guard = engine().lock().unwrap();
        let catalog = &guard.navigator.catalog;
        let path = request.uri().path().to_string();

        let (content_type, body) = if path.ends_with("/meta") {
            (
                "application/json",
                serde_json::to_vec(&map::meta(catalog)).unwrap_or_default(),
            )
        } else {
            ("application/octet-stream", map::payload(catalog))
        };

        responder.respond(
            Response::builder()
                .header("Content-Type", content_type)
                .body(body)
                .unwrap(),
        );
    });

    // Long-lived eval: runs the map, then waits for selection messages.
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../assets/map.js"));
        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            if let Some(id) = message.get("track_id").and_then(|v| v.as_f64()) {
                selected.set(Some(id as i64));
            }
        }
    });

    // Push the generated route to the map. Ids only, which is what eval is
    // sized for.
    use_effect(move || {
        let ids: Vec<i64> = generator
            .result
            .read()
            .iter()
            .map(|s| s.track.track_id)
            .collect();
        let script = format!(
            "window.qsuggestSetRoute && window.qsuggestSetRoute({});",
            serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())
        );
        document::eval(&script);
    });

    let block_names: Vec<String> = engine()
        .lock()
        .unwrap()
        .space
        .manifest
        .blocks
        .iter()
        .map(|b| b.name.clone())
        .collect();

    let (total_tracks, hidden_tracks) = {
        blocked.read();
        let guard = engine().lock().unwrap();
        let catalog = &guard.navigator.catalog;
        let visible = catalog.visible().count();
        (visible, catalog.len() - visible)
    };

    let matches: Vec<(i64, String, String)> = {
        // Subscribe, so hiding an artist empties them out of this list too.
        blocked.read();
        let guard = engine().lock().unwrap();
        let catalog = &guard.navigator.catalog;
        let needle = query().to_lowercase();
        catalog
            .visible()
            .map(|i| catalog.get(i))
            .filter(|t| {
                needle.is_empty()
                    || t.title.to_lowercase().contains(&needle)
                    || t.artist.to_lowercase().contains(&needle)
            })
            .take(80)
            .map(|t| (t.track_id, t.artist.clone(), t.title.clone()))
            .collect()
    };

    let selected_label = selected()
        .and_then(|id| {
            let guard = engine().lock().unwrap();
            guard
                .navigator
                .catalog
                .tracks
                .iter()
                .find(|t| t.track_id == id)
                .map(|t| format!("{} - {}", t.artist, t.title))
        })
        .unwrap_or_else(|| "nothing selected".into());

    rsx! {
        div { class: "app",
            header {
                h1 { "Qobuz suggestion space" }
                span { class: "muted", "{total_tracks} tracks" }
                if hidden_tracks > 0 {
                    span { class: "muted", "({hidden_tracks} hidden)" }
                }
                nav { class: "views",
                    button {
                        class: if explore() { "tab active" } else { "tab" },
                        onclick: move |_| explore.set(true),
                        "explore"
                    }
                    button {
                        class: if explore() { "tab" } else { "tab active" },
                        onclick: move |_| explore.set(false),
                        "pipeline"
                    }
                }
                span { class: "spacer" }
                span { class: "muted", "{selected_label}" }
            }

            // Hidden, not unmounted: map.js holds a reference to the canvas,
            // and remounting it would leave the map drawing into a dead node.
            div { class: if explore() { "body" } else { "body hidden" },
                LibraryPanel {}

                div { class: "map-wrap",
                    canvas { id: "map" }
                }

                aside { class: "side",
                    // ------------------------------------------------ browse
                    section { class: "panel",
                        h2 { "In the space" }
                        input {
                            class: "search",
                            placeholder: "search title or artist",
                            value: "{query}",
                            oninput: move |e| query.set(e.value()),
                        }
                        ul { class: "list",
                            for (id, artist, title) in matches {
                                li {
                                    key: "{id}",
                                    class: if selected() == Some(id) { "row selected" } else { "row" },
                                    onclick: move |_| selected.set(Some(id)),
                                    span { class: "artist", "{artist}" }
                                    span { class: "title", "{title}" }
                                }
                            }
                        }
                    }

                    // ---------------------------------------------- generate
                    GeneratePanel {}

                    // ----------------------------------------------- weights
                    section { class: "panel",
                        h2 { "Weights" }
                        p { class: "muted",
                            "Reshapes distances immediately. Map positions are fixed until the layout step is re-run."
                        }
                        for name in block_names {
                            div { class: "slider", key: "{name}",
                                label { "{name}" }
                                input {
                                    r#type: "range",
                                    min: "0",
                                    max: "3",
                                    step: "0.1",
                                    value: "{weights().get(&name).copied().unwrap_or(1.0)}",
                                    oninput: {
                                        let name = name.clone();
                                        move |e: FormEvent| {
                                            if let Ok(v) = e.value().parse::<f32>() {
                                                let mut w: HashMap<String, f32> = weights();
                                                w.insert(name.clone(), v);
                                                weights.set(w.clone());
                                                let _ = engine().lock().unwrap().set_weights(&w);
                                                generator.clear();
                                            }
                                        }
                                    },
                                }
                                span { class: "muted",
                                    {format!("{:.1}", weights().get(&name).copied().unwrap_or(1.0))}
                                }
                            }
                        }
                    }
                }
            }

            if !explore() {
                PipelineView {}
            }

            PlayerBar {}
        }
    }
}
