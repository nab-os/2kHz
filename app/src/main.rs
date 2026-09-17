//! Entry point for every platform with a screen.
//!
//! Deliberately thin, everything else lives in the library, because Android
//! has no `main` of its own: dioxus-desktop's JNI trampoline dlsym's this one.

fn main() {
    // Local unless QSUGGEST_SERVER (or a stored pairing) says otherwise.
    // Not fatal on mobile: no stderr anyone will read and no local corpus, so
    // a phone launches into the setup screen instead.
    let started = qsuggest::app::bootstrap();

    #[cfg(not(feature = "mobile"))]
    if let Err(err) = started {
        eprintln!("could not start: {err:#}");
        eprintln!(
            "\nEither run the pipeline first:\n  \
             cd app && cargo run --release --bin crawl -- --max-tracks 500\n  \
             cd ../pipeline && uv run qsuggest analyse\n  \
             uv run qsuggest build-space\n  \
             uv run qsuggest layout\n\n\
             or point this at a server:\n  \
             QSUGGEST_SERVER=http://host:7700 QSUGGEST_TOKEN=... cargo run"
        );
        std::process::exit(1);
    }

    #[cfg(feature = "mobile")]
    let _ = started;

    // A locally started stage must not outlive the window. (A stage started on
    // a *server* deliberately does, see ui::pipeline.) No-op without `local`.
    #[cfg(feature = "local")]
    qsuggest::stages::install_exit_guard();

    #[cfg(feature = "mobile")]
    dioxus::launch(qsuggest::app::App);

    // Three columns plus a map need room; the default window is too small to
    // show them without the panels collapsing.
    #[cfg(not(feature = "mobile"))]
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            dioxus::desktop::Config::new().with_window(
                dioxus::desktop::WindowBuilder::new()
                    .with_title("Qobuz suggestion space")
                    .with_inner_size(dioxus::desktop::LogicalSize::new(1500.0, 950.0)),
            ),
        )
        .launch(qsuggest::app::App);
}
