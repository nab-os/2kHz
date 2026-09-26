# 2kHz, from the inside: a course

This is a guided tour of every part of 2kHz: what each piece does, why it is
shaped the way it is, and how the pieces talk to each other. It is written
against the tree as of **v0.7.0** (`eaa1322`), and it quotes the code by
file and function name so you can read along.

The [README](../../README.md) tells you how to *run* 2kHz, and
[design.md](../design.md) records the measurements behind the big decisions.
This course is the missing middle: how the code actually works.

## Who this is for

Someone who wants to be able to change any part of the app with confidence.
You should be comfortable reading Rust. You do not need to know Dioxus, Axum,
Diesel, ONNX, digital signal processing or UMAP beforehand, each chapter
introduces what it uses.

## How to take it

Read the chapters in order the first time. Each one starts with what you
will understand by the end, walks through the code, and finishes with
**check yourself** questions and **exercises**. The questions can be
answered from the chapter; the exercises need the code open.

Keep two terminals handy:

```sh
cd server && cargo test          # the server's unit tests, no network
cd app    && cargo test          # the client's, no window needed
```

and, if you have ~620MB of disk to spare, a demo corpus you can poke at
without a Qobuz account (chapter 18 explains it):

```sh
cd server && cargo run --release -- demo /tmp/two-khz-demo
```

## Contents

**Part I: The shape of the thing**

0. [Orientation](00-orientation.md): what 2kHz is, the two programs, the
   life of one track from favourite to map point
1. [The toolkit](01-toolkit.md): the crates, the Cargo layout and feature
   flags, and the handful of patterns that recur everywhere
2. [The data model](02-data-model.md): `schema.sql`, the Diesel mirror,
   upserts, the slim catalogue

**Part II: The pipeline (server side)**

3. [Talking to Qobuz](03-qobuz.md): credentials, signing, the shared rate
   limit, and the shapes the app sees
4. [The crawl](04-crawl.md): seeding, the frontier, and how the corpus grows
5. [Getting audio](05-analyse.md): the `analyse` stage: byte-range
   excerpts, MP3 frame sync, decoding, resampling, workers
6. [CLAP](06-clap.md): the neural half: the mel front end, the audio and
   text towers, model weights
7. [Descriptors](07-descriptors.md): the signal-processing half: loudness,
   spectrum, tempo, key
8. [Building the space](08-space.md): eight blocks, normalisation, PCA,
   and why weights are applied later
9. [Layout](09-layout.md): the UMAP port that draws the map

**Part III: The server as a service**

10. [The server process](10-server.md): `main`, the `Hub`, jobs, threads
    and cancellation
11. [The HTTP API](11-api.md): routes, device tokens and scopes, sync, SSE

**Part IV: The client**

12. [The client core](12-client-core.md): the engine, the memory-mapped
    space, the catalogue, the remote backend
13. [Navigating the space](13-navigation.md): neighbours, radio, paths,
    drift, queue sorting
14. [The UI architecture](14-ui-architecture.md): Dioxus in five minutes,
    the shell, contexts, and the reactive plumbing
15. [The UI surfaces](15-ui-surfaces.md): library, detail sheet, generator,
    player, queue, menus, pipeline view
16. [The webview bridge](16-webview.md): `eval` channels, the canvas map,
    the audio element, and the CSS layout

**Part V: Around the code**

17. [Building and shipping](17-build-and-ship.md): features, Docker,
    compose, CI, Debian packages, Android
18. [Testing](18-testing.md): what is tested, how, and the demo corpus
19. [Patterns, wrinkles and exercises](19-patterns-and-wrinkles.md): the
    recurring ideas, the known rough edges, and bigger projects to try

[Glossary](glossary.md): every term of art used in the course, in one place.

## A map of the repository

```
2khz/
├── README.md            how to run it
├── docs/
│   ├── design.md        why it is the way it is, with measurements
│   ├── ux-rethink.md    the phone-first redesign, and what landed
│   └── course/          this course
├── schema.sql           the database contract, shared by both programs
├── Dockerfile           the server image
├── compose.yaml         an example deployment
├── .github/             CI: .debs, AppImages, APK, Docker images
├── server/              two-khz-server: API + pipeline, one binary
│   └── src/
│       ├── main.rs      CLI entry, `serve`, pairing, catalogue rebuilds
│       ├── cli.rs       the pipeline/account subcommands
│       ├── routes.rs    the HTTP API
│       ├── auth.rs      device tokens and scopes
│       ├── hub.rs       everything the routes call into
│       ├── stages.rs    running a stage; corpus counts
│       ├── db.rs        writing the database; blocking; purge
│       ├── catalog.rs   the slim catalog.db for clients
│       ├── crawl.rs     the crawler
│       ├── qobuz.rs     the Qobuz client (credentials live here only)
│       ├── login.rs     browser sign-in; scraping the web player
│       ├── text.rs      the CLAP text tower
│       └── pipeline/    analyse, audio, clap, descriptors, labels,
│                        assemble (build-space), layout, models, demo
└── app/                 two-khz-app: the shared library + the GUI
    ├── src/
    │   ├── lib.rs       the Engine, client paths, server wiring
    │   ├── api.rs       wire types shared with the server
    │   ├── schema.rs    Diesel's view of schema.sql (shared)
    │   ├── qobuz.rs     Qobuz item shapes and parsers (shared)
    │   ├── space.rs     loading space.bin; weighting; top-k
    │   ├── db.rs        reading catalog.db
    │   ├── paths.rs     the Navigator: every walk through the space
    │   ├── map.rs       the binary payload for the canvas
    │   ├── logbuffer.rs a bounded, cursor-read log
    │   ├── backend/     the HTTP client for the server
    │   ├── app.rs       root component, setup, the Shell
    │   ├── main.rs      window creation
    │   └── ui/          every panel
    └── assets/          style.css and the JS that runs in the webview
```

(`pipeline/` at the repo root holds only `__pycache__` from the Python
pipeline this Rust code replaced. It is not tracked and can be ignored.)
