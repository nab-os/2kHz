# 18. Testing

By the end of this chapter you will know what the test suite covers, how to
run the parts that need nothing and the parts that need model weights or
sample files, how the demo corpus doubles as an end-to-end test, and the
testing style the codebase favours.

## Running them

```sh
cd server && cargo test                                   # unit tests, no network
cd server && cargo test --release -- --ignored smoke      # end to end, fetches CLAP once
cd app    && cargo test                                   # client unit tests
```

No test needs Qobuz credentials. The app's tests compile the default
(desktop) feature, so they need the same GTK/webkit headers as a desktop
build.

## What is covered

46 tests in all; 5 are `#[ignore]`d because they need something from
outside.

### Server

| file | tests | what they pin down |
|---|---|---|
| `db.rs` | `catalogue_round_trip` | the whole data layer on a scratch DB: sparse-vs-full upserts, shortest `seed_distance`, lowest frontier priority, `pending_count`, `resolve_artist`, blocking (and its frontier cleanup), `stages::corpus`, `purge`, `catalog::build` (BPM-only descriptors, no `qobuz_json`), and `Catalog::load` reading it back in space order with a `<missing>` placeholder |
| `login.rs` | `decodes_base64_with_and_without_padding`, `pulls_credentials_out_of_a_bundle` | the hand-written decoder, and secret extraction from a real (Sept 2026) bundle excerpt |
| `pipeline/audio.rs` | `id3_size_counts_header_and_synchsafe_body`, `frame_sync_skips_a_false_header`, `resampling_keeps_a_tone_and_its_length`; ignored: `excerpt_from_the_middle_of_a_served_file` | the byte-level excerpt machinery |
| `pipeline/clap.rs` | `slaney_scale_round_trips`, `repeat_pad_tiles_then_zero_fills`, `windows_drop_a_short_stub`; ignored: `front_end_matches_transformers` | the mel front end and windowing |
| `pipeline/descriptors.rs` | `loudness_of_a_quiet_sine`, `tempo_of_a_click_track`, `key_of_a_triad` | the DSP on synthetic signals with known answers |
| `pipeline/assemble.rs` | `bpm_folds_into_one_octave`, `keys_a_fifth_apart_are_neighbours`, `pca_finds_the_long_axis`, `normalised_rows_are_unit_length_and_columns_centred`, `missing_values_take_the_median` | every primitive of build-space |
| `pipeline/layout.rs` | `curve_matches_umap_learn_for_the_defaults`, `clusters_stay_apart_on_the_map` | the UMAP port against umap-learn's constants and a clustering property |
| `pipeline/analyse.rs` | ignored: `cost_of_one_excerpt` | a benchmark, not a check |
| `pipeline/demo.rs` | ignored: `smoke` | the whole pipeline, end to end |

### Client

| file | tests | what they pin down |
|---|---|---|
| `paths.rs` | five `shortest_path_order` tests | anchor handling, dropping unknown ids, keeping duplicates, pass-through below three |
| `qobuz.rs` | six identity tests, `discography_drops_albums_by_other_artists`, `cover_url_follows_the_two_by_two_tail_convention` | ISRC identity rules, `own_releases`, cover guessing |
| `app.rs` | `per_field_matches_the_joined_string`, `a_term_spanning_two_fields_cannot_be_produced_by_splitting` | the search optimisation is equivalent to the naive version |
| `ui/generate.rs` | three `dedup_by_recording` tests | ISRC dedupe of generated sequences |
| `ui/menu.rs` | `every_target_survives_the_round_trip`, `rubbish_parses_to_nothing_rather_than_a_wrong_row` | `MenuTarget::tag`/`parse` |
| `ui/player.rs` | four `index_after_move` tests | queue reorder bookkeeping |

## The ignored tests

- **`smoke`** (`pipeline/demo.rs`): builds the demo corpus in a temp dir
  (below) and checks: the 174 BPM album folds to ~87; **same-album top-1 >
  75%** (design.md reports 30 of 32); every track gets layout coordinates.
  Needs the CLAP audio and text weights (fetched on first run, ~620MB).
- **`front_end_matches_transformers`** (`clap.rs`): compares the mel front
  end with reference files written by transformers
  (`TWO_KHZ_CLAP_REFERENCE=<dir>`), to within 0.01 dB; with
  `TWO_KHZ_MODEL_DIR` set, also checks the embedding's cosine to torch's is
  above 0.9999.
- **`excerpt_from_the_middle_of_a_served_file`** (`audio.rs`): spins up a
  30-line HTTP server on a random port that honours `Range` headers like the
  CDN, serves a real MP3 (`TWO_KHZ_MP3_SAMPLE=track.mp3`), and checks the
  excerpt is under half the file, decodes to the same rate, and is 90s long.
- **`cost_of_one_excerpt`** (`analyse.rs`): times resampling, descriptors
  and CLAP at 1, 4 and all threads. Run with `--nocapture`.

## The demo corpus

`two-khz-server demo <dir>` (`pipeline/demo.rs`) builds a complete corpus
with no network except model weights:

- **eight fake albums** with deliberately distinct character, four 25s
  tracks each: Ambient Drift (beatless, 110Hz), Techno Nights (140 BPM, 55Hz),
  Rock Garage (110 BPM, noisy, 6 harmonics), Jazz Corner (92 BPM, 8
  harmonics), Drum and Bass (174 BPM, 45Hz), Downtempo Haze (75 BPM), Bright
  Pop (128 BPM, 220Hz), Dark Drone (beatless, 38Hz);
- `synth(bpm, base, noise, harmonics, seed)` makes each track: a harmonic
  stack slightly detuned per track, a slow amplitude wobble, Gaussian-ish
  noise, and a decaying 55Hz click on every beat, deterministic from the
  seed;
- `populate` writes albums (release years 1990, 1994, …), artists and tracks
  through the real `crawl::upsert_*` functions;
- then the **real** `analyse::analyse_pcm` and `store`, the **real**
  `assemble::run`, and the **real** `layout::run`.

```sh
cd server
cargo run --release -- demo /tmp/two-khz-demo
TWO_KHZ_DATA_DIR=/tmp/two-khz-demo/data cargo run --release -- pair --name demo --scope pipeline
TWO_KHZ_DATA_DIR=/tmp/two-khz-demo/data cargo run --release -- serve
```

Then point the app at it. It is the best sandbox for the exercises in this
course: small, fast to rebuild, and every stage is the production code.

## The testing style

A few habits recur and are worth copying:

- **Oracles over restated arithmetic.** `index_after_move` is checked
  against actually reordering a `Vec` for every (index, from, to) in a
  7-element queue, rather than re-deriving the formula in the test.
- **Fixtures whose answer you can state.** The `paths.rs` tests put tracks
  on a unit circle, where the shortest route is just angle order. The DSP
  tests use sines, click tracks and triads with known tempo, loudness and
  key.
- **Equivalence tests for optimisations.** The search haystack tests prove
  the fast per-field check equals the slow joined-string check, and document
  the one case where they could differ.
- **Totality for encode/decode pairs.** The menu round-trip test lists
  every variant, because the failure mode is silent.
- **One scratch database, end to end.** `catalogue_round_trip` walks the
  data layer as the stages use it, instead of mocking SQLite.
- **Comments state why a case matters**, often naming the bug it came from.

## What is not tested

Worth knowing before you change them:

- the HTTP routes and auth extractors (no request-level tests);
- `Hub`'s job orchestration (start/stop/queue/generation);
- the Qobuz client beyond credential parsing (signing, rate limiting,
  retries);
- the navigation modes other than queue sorting (`neighbours`,
  `radio_nearest`, `interpolate`, `graph_path`, `drift_to_text`);
- anything in the webview, `docs/ux-rethink.md` is candid that the redesign
  was verified at the compiler level, not by looking at it on a phone.

## Check yourself

1. Which tests need network access, and for what?
2. What does the smoke test check about the space's quality?
3. Why is `index_after_move` tested against a real `Vec`?
4. What makes the demo corpus a good end-to-end test rather than a mock?

## Exercises

1. Write a `radio_nearest` test on the circle fixture (chapter 13, exercise 1).
2. Write an Axum route test for auth: build the router with a scratch
   `AuthStore`, and check a request without a token gets 401, a `play`
   token on `/api/pipeline/start` gets 403, and `/api/health` gets 200.
   (`tower::ServiceExt::oneshot` is the usual tool.)
3. Write a `tokio::time::pause()` test for `QobuzClient::acquire` (chapter 3,
   exercise 2).
