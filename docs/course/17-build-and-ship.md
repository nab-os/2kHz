# 17. Building and shipping

By the end of this chapter you will know how to build each artefact locally,
how the Docker image is put together, what the CI pipeline produces, and the
reasoning behind the packaging choices.

Files: `app/Cargo.toml`, `app/Dioxus.toml`, `app/android-env.sh`,
`Dockerfile`, `.dockerignore`, `compose.yaml`,
`.github/workflows/build.yml`, `.github/scripts/finish-deb.sh`.

## Building locally

```sh
# the server: no system libraries needed beyond a C compiler
cd server && cargo build --release

# the desktop app: needs GTK/webkit headers on Linux (see README)
cd app && cargo build --release          # or: dx serve / dx bundle

# the Android app (arm64 only)
cd app && . ./android-env.sh
dx build --release --platform android --target aarch64-linux-android \
   --no-default-features --features mobile
```

`dx` is the Dioxus CLI (pinned to 0.7.10 in CI). For Android, use `dx`, not
`cargo android build`: the two generate conflicting JNI trampolines. The
`gen/` directory, `mobile.toml` and the `[package.metadata.cargo-android]`
block are leftovers from `cargo mobile init` and are unused.

### Android specifics

- **arm64 only.** `manganis` (Dioxus's asset crate) refuses to build for
  32-bit Android. Without `--target aarch64-linux-android`, `dx` builds
  x86_64 for an emulator, which will not install on a phone.
- `android-env.sh` points `cc-rs` at the NDK's per-API-level clang wrappers
  (API 26, the floor for the WebView features wry uses), since the NDK ships
  `aarch64-linux-android26-clang`, not a bare triple.
- There is no separate JNI entry point in the library: `dioxus-desktop`
  already exports `start_app` for Android, and its trampoline `dlsym`s the
  binary's `main`. Defining another would be a duplicate `#[no_mangle]`.
- The application id must differ from `dev.dioxus.main` (design.md explains
  a Kotlin `typealias` that would otherwise refer to itself); it is
  `fr.glargh.twokhz`.

### Desktop specifics

`main.rs` opens a 1500×950 window titled "2kHz" (`TWO_KHZ_WINDOW=WxH`
overrides it, handy for checking the phone layout on a desktop).
`Dioxus.toml` supplies icons, category, descriptions and Debian metadata to
`dx bundle`.

## The Docker image

The server only. Two stages:

**Builder** (`rust:1.96.0-trixie`):

- installs `build-essential pkg-config` (bundled SQLite and `tokenizers`'
  `onig` need a C compiler);
- copies `app/`, `server/` and `schema.sql` (a build input, because
  `db.rs` embeds it with `include_str!`);
- builds with **cache mounts** for the cargo registry and the target dir;
- copies the binary and any `libonnxruntime*.so*` that `ort` downloaded
  into `/out` *within the same step*, the target dir is a cache mount and
  will not exist in the layer afterwards. (This is why the build needs
  network access: `ort`'s `download-binaries` fetches ONNX Runtime.)

**Runtime** (`debian:trixie-slim`):

- `ca-certificates curl libstdc++6` (TLS roots, the healthcheck, and
  ONNX Runtime's C++ runtime);
- a `twokhz` user with **fixed uid 10001**, not `--system`, which expects a
  low uid, so a bind-mounted `/data` can be chowned predictably;
- the binary into `/usr/local/bin`, the ONNX Runtime library into
  `/usr/local/lib` + `ldconfig`;
- `TWO_KHZ_DATA_DIR=/data`, `TWO_KHZ_ENV_DIR=/data`,
  `TWO_KHZ_MODEL_DIR=/data/models`, `TWO_KHZ_CACHE_DIR=/cache/audio`;
- `HEALTHCHECK` curls `/api/health`;
- `ENTRYPOINT ["two-khz-server"]`, `CMD ["serve", "--bind", "0.0.0.0:7700"]`,
  so `docker run … analyse` or `docker exec … pair --name phone` pass
  straight through.

`.dockerignore` keeps out `.git`, docs, `.env` (credentials come in as
environment variables), any host `target/` (which would also defeat the
cache mount), `data/`, `cache/` and the Android leftovers.

### `compose.yaml`

An example deployment, heavily commented:

- a **pinned image tag** rather than `latest`, "so that `compose pull` on a
  bad day cannot swap the running server for a release nobody here has
  tried" (at `eaa1322` it still pins `v0.6.1`, one release behind the
  crates' 0.7.0, bumping it is part of cutting a release);
- `ports: "127.0.0.1:7700:7700"`: the container binds `0.0.0.0` (it must, to
  be reachable across the container boundary), and the *published* port is
  what is restricted to loopback;
- `env_file: .env` (optional);
- two volumes: `data` (the only thing worth backing up) and `cache`;
- `init: true`: PID 1 has no default signal handlers, so without an init
  process `docker stop` would wait out the grace period;
- the same healthcheck.

The header lists the first-run sequence: copy `.env`, `pull`, `run --rm
server pair …`, `run --rm server build-catalog`, `up -d`.

## CI: `.github/workflows/build.yml`

Runs on pushes to `main`, `v*` tags, pull requests and manual dispatch.

### `linux`: a matrix of Ubuntu 24.04 and 26.04

Each release is built on **its own runner**, because each desktop package
links that release's webkit and glibc, a 24.04 build is not safe on 26.04.
`ubuntu-latest` is avoided on purpose: it migrates from 24.04 to 26.04 in
October to November 2026, which would silently turn the matrix into two
identical legs.

Per leg:

1. `dx bundle` → desktop `.deb` + `.AppImage`.
2. `cargo build` and **`cargo test`** the server.
3. `finish-deb.sh` on the desktop `.deb`: `dx` leaves `Depends` and
   `Maintainer` blank (and stamps the Cargo version), so the script unpacks,
   runs **`dpkg-shlibdeps`** over the ELF binaries to compute `Depends` for
   *this* release, fills in the maintainer and version, and repacks.
   Computed rather than hardcoded because 24.04's 64-bit `time_t`
   transition renamed libraries (`libgtk-3-0` → `libgtk-3-0t64`,
   `libssl3` → `libssl3t64`).
4. A server `.deb` built by hand (binary, `schema.sql`, README, a control
   file), also through `finish-deb.sh`. It depends on three libraries; the
   desktop one on twelve including webkit, "a server box should not be made
   to install a browser engine".
5. Tarballs of each, and the AppImage, into `dist/`.

### `android`: one arm64 APK

Signs when the keystore secrets are readable; otherwise builds an
**unsigned** APK rather than skipping, a fork's pull request structurally
cannot read secrets, and this still proves arm64 compiles. A **tag fails**
instead of falling back, so a release can never carry an APK nobody can
install. Half a keystore (some secrets set, not others) is reported as a
misconfiguration.

### `docker`: the server image

Built on every push, so a broken `Dockerfile` fails next to the `.deb`s;
**pushed only on a tag**, to Docker Hub (`4gjr3z1t/2khz`) and GHCR, with a
version tag and `latest`. On non-tag builds a smoke test runs
`docker run --rm <image> devices` against the freshly built image.

### `release`: on `v*` tags

Needs `linux` and `android`, downloads their artefacts by name, and opens a
GitHub release with everything attached.

## A path to be careful with

`pipeline::Paths::from_env` falls back to `qobuz::repo_root()` when the
`TWO_KHZ_*` variables are unset, and `repo_root` is
`env!("CARGO_MANIFEST_DIR")`'s parent, a **compile-time** path. In a
checkout, that is exactly right. In the Docker image it never matters,
because all four variables are set. But a server installed from the `.deb`
or a tarball and run without them will look for `data/`, `data/models/`,
`cache/` and `.env` under the directory the *CI runner* built it in. When
running a packaged server, set `TWO_KHZ_DATA_DIR`, `TWO_KHZ_MODEL_DIR`,
`TWO_KHZ_CACHE_DIR` and `TWO_KHZ_ENV_DIR` (chapter 19 suggests a fix).

## Check yourself

1. Why are there two Ubuntu legs, and why not `ubuntu-latest`?
2. Why is `Depends` computed by `dpkg-shlibdeps` instead of written down?
3. What does the Docker builder copy out of the target directory, and why in
   the same `RUN`?
4. Why is the container's bind `0.0.0.0` safe in the compose example?
5. What does the Android job do on a pull request from a fork? On a tag
   without secrets?

## Exercises

1. `docker build -t two-khz:dev .` and run
   `docker run --rm two-khz:dev --help`.
2. Run `.github/scripts/finish-deb.sh` by hand on a `dx bundle` output and
   compare `dpkg-deb -I` before and after.
3. Make `Paths::from_env` fall back to an XDG data directory (and
   `/var/lib/two-khz` when running as a system user) instead of the compile
   time checkout, keeping the checkout default only for debug builds.
