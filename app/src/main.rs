//! Dioxus desktop app: navigate the suggestion space, and browse and play the
//! Qobuz catalogue it was built from.
//!
//! Two halves side by side. Left is live Qobuz; right is the analysed space.
//! They meet on track ids, see the `ui` module.

mod ui;

use dioxus::desktop::{use_asset_handler, wry::http::Response};
use dioxus::prelude::*;
use qsuggest::qobuz::FORMAT_MP3_320;
use qsuggest::{default_data_dir, default_db_path, engine, map};
use std::collections::HashMap;
use std::rc::Rc;
use ui::{
    Blocklist, Crawler, GeneratePanel, Generator, Library, LibraryPanel, LocalIds, Pipeline,
    PipelineView, Player, PlayerBar, Selection,
};

fn main() {
    let data_dir = default_data_dir();
    let db_path = default_db_path();

    match qsuggest::init_engine(&data_dir, &db_path) {
        Ok(()) => {
            // Same database the space was loaded from, so queued crawl work
            // lands where the pipeline will look for it.
            ui::set_db_path(db_path.clone());
        }
        Err(err) => {
            eprintln!(
                "could not load the space from {}:\n  {err:#}",
                data_dir.display()
            );
            eprintln!(
                "\nRun the pipeline first:\n  \
                 cd pipeline\n  \
                 uv run qsuggest crawl --max-tracks 500\n  \
                 uv run qsuggest analyse\n  \
                 uv run qsuggest build-space\n  \
                 uv run qsuggest layout"
            );
            std::process::exit(1);
        }
    }

    // A pipeline stage must not outlive the window it was started from.
    ui::pipeline::install_exit_guard();

    // Three columns plus a map need room; the default window is too small to
    // show them without the panels collapsing.
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            dioxus::desktop::Config::new().with_window(
                dioxus::desktop::WindowBuilder::new()
                    .with_title("Qobuz suggestion space")
                    .with_inner_size(dioxus::desktop::LogicalSize::new(1500.0, 950.0)),
            ),
        )
        .launch(App);
}

#[component]
fn App() -> Element {
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
    use_context_provider(Crawler::new);
    let pipeline = use_context_provider(Pipeline::new);

    // Explore is the map and space tools; Pipeline builds the corpus. One
    // window because they share the player and the database.
    let mut explore = use_signal(|| true);

    // A rebuilt space on disk means the loaded one is stale. Reload it and
    // redraw rather than making the user restart the app.
    use_effect(move || {
        let generation = *pipeline.generation.read();

        if generation > 0 {
            match qsuggest::reload_engine(&default_data_dir(), &default_db_path()) {
                Ok(()) => {
                    selected.set(None);
                    generator.clear();
                    document::eval("window.qsuggestReloadPoints && window.qsuggestReloadPoints();");
                }
                Err(err) => eprintln!("could not reload the rebuilt space: {err:#}"),
            }
        }

        // Only the shell can see the engine, and "how many points are drawn"
        // is a question about the loaded space, not about the database.
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
        pipeline.refresh();
    });

    // Blocking writes to the database, refreshes the engine's filter, and
    // redraws the map, all three, or the halves disagree about what exists.
    let blocked =
        use_signal(|| qsuggest::db::blocked_artists(&default_db_path()).unwrap_or_default());

    let apply_block = use_callback(move |(artist_id, name): (i64, String)| {
        let mut blocked = blocked;
        let path = default_db_path();
        if let Err(err) = qsuggest::db::block_artist(&path, artist_id, &name, None) {
            eprintln!("could not hide artist {artist_id}: {err:#}");
            return;
        }
        let _ = engine().lock().unwrap().refresh_blocked(&path);
        blocked.set(qsuggest::db::blocked_artists(&path).unwrap_or_default());
        document::eval("window.qsuggestReloadPoints && window.qsuggestReloadPoints();");
    });

    let lift_block = use_callback(move |artist_id: i64| {
        let mut blocked = blocked;
        let path = default_db_path();
        if let Err(err) = qsuggest::db::unblock_artist(&path, artist_id) {
            eprintln!("could not unhide artist {artist_id}: {err:#}");
            return;
        }
        let _ = engine().lock().unwrap().refresh_blocked(&path);
        blocked.set(qsuggest::db::blocked_artists(&path).unwrap_or_default());
        document::eval("window.qsuggestReloadPoints && window.qsuggestReloadPoints();");
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

    ui::use_transport(player);

    // Open on the user's own library, which is also what the crawl seeds from.
    use_future(move || async move {
        ui::open_initial(library);
    });

    // Bulk point data crosses as binary here, never through eval.
    use_asset_handler("points", move |request, responder| {
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
        style { {include_str!("../assets/style.css")} }
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
