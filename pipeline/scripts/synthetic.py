"""Synthetic corpus fixture.

Builds fake "albums" with deliberately distinct musical character, analyses them
with the real extractors, and assembles the real space. Used by the smoke test
and the Rust/Python parity test, so neither needs Qobuz credentials.

Callers must set TWO_KHZ_DATA_DIR before importing two_khz.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import soundfile as sf

SR = 44100
DURATION = 25
TRACKS_PER_ALBUM = 4

# name, bpm (0 = beatless), base frequency, noise level, harmonic richness
ALBUMS = [
    ("Ambient Drift", 0, 110, 0.01, 2),
    ("Techno Nights", 140, 55, 0.12, 3),
    ("Rock Garage", 110, 98, 0.30, 6),
    ("Jazz Corner", 92, 147, 0.04, 8),
    ("Drum and Bass", 174, 45, 0.18, 4),
    ("Downtempo Haze", 75, 82, 0.05, 3),
    ("Bright Pop", 128, 220, 0.08, 5),
    ("Dark Drone", 0, 38, 0.02, 2),
]


def synth(bpm: int, base_freq: float, noise: float, harmonics: int, seed: int) -> np.ndarray:
    rng = np.random.default_rng(seed)
    t = np.arange(int(SR * DURATION)) / SR
    audio = np.zeros_like(t)

    # Harmonic stack, slightly detuned per track so albums vary internally.
    detune = 1.0 + rng.uniform(-0.03, 0.03)
    for h in range(1, harmonics + 1):
        audio += (0.6 / h) * np.sin(2 * np.pi * base_freq * detune * h * t + rng.uniform(0, 6.28))

    audio *= 0.5 + 0.5 * np.sin(2 * np.pi * rng.uniform(0.05, 0.2) * t)
    audio += noise * rng.standard_normal(len(t))

    if bpm:
        period = 60.0 / bpm
        click = int(0.04 * SR)
        env = np.exp(-np.linspace(0, 14, click))
        kick = env * np.sin(2 * np.pi * 55 * np.arange(click) / SR)
        for k in range(int(DURATION / period)):
            i = int(k * period * SR)
            audio[i : i + click] += 1.4 * kick

    peak = float(np.abs(audio).max())
    return (audio / peak * 0.9).astype(np.float32) if peak > 0 else audio.astype(np.float32)


def write_audio(audio_dir: Path) -> list[tuple[int, str, int, str]]:
    """Write audio files. Returns (track_id, title, artist_id, album_id)."""
    audio_dir.mkdir(parents=True, exist_ok=True)
    entries = []
    track_id = 1000
    for album_index, (album_name, bpm, freq, noise, harmonics) in enumerate(ALBUMS):
        for n in range(TRACKS_PER_ALBUM):
            track_id += 1
            sf.write(
                audio_dir / f"{track_id}.flac",
                synth(bpm, freq, noise, harmonics, seed=track_id),
                SR,
            )
            entries.append(
                (track_id, f"{album_name} {n + 1}", 100 + album_index, f"album-{album_index}")
            )
    return entries


def populate_db(conn, entries) -> None:
    from two_khz import crawl

    for album_index, (album_name, *_rest) in enumerate(ALBUMS):
        artist = {"id": 100 + album_index, "name": f"Artist {album_index}"}
        crawl.upsert_artist(conn, artist)
        crawl.upsert_album(
            conn,
            {
                "id": f"album-{album_index}",
                "title": album_name,
                "artist": artist,
                "release_date_original": f"{1990 + album_index * 4}-01-01",
                "genre": {"name": album_name.split()[0]},
            },
        )
    for track_id, title, artist_id, album_id in entries:
        crawl.upsert_track(
            conn,
            {"id": track_id, "title": title, "duration": DURATION},
            album_id=album_id,
            artist_id=artist_id,
            seed_distance=0,
        )
    conn.commit()


def analyse(conn, entries, audio_dir: Path, verbose: bool = True) -> None:
    from two_khz import analyse as analyse_mod
    from two_khz.features import clap_ext, essentia_ext

    essentia = essentia_ext.EssentiaExtractor()
    clap = clap_ext.ClapExtractor()
    for n, (track_id, *_r) in enumerate(entries, start=1):
        path = audio_dir / f"{track_id}.flac"
        descriptors, effnet, genre400 = essentia.extract(path)
        analyse_mod.store(conn, track_id, descriptors, clap.embed_audio(path), effnet, genre400)
        if verbose and n % 8 == 0:
            print(f"  {n}/{len(entries)}")
    conn.commit()


def prepare(workdir: Path, verbose: bool = True):
    """Build the whole fixture. Returns (conn, entries, manifest)."""
    from two_khz import db, space

    audio_dir = workdir / "audio"
    if verbose:
        print(f"synthesising {len(ALBUMS) * TRACKS_PER_ALBUM} tracks ...")
    entries = write_audio(audio_dir)

    conn = db.connect(workdir / "data" / "two_khz.db")
    populate_db(conn, entries)

    if verbose:
        print("extracting features ...")
    analyse(conn, entries, audio_dir, verbose=verbose)

    if verbose:
        print("building space ...")
    manifest = space.build(conn, verbose=False)
    return conn, entries, manifest
