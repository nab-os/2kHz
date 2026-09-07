"""Excerpt fetching and the disposable audio cache.

We analyse the middle ~90s of each track: intros, outros and fades skew tempo
and timbre statistics badly. Excerpts are cached as mono 44.1kHz FLAC under a
size cap, and the cache is entirely disposable, deleting it costs only the
time to re-download.
"""

from __future__ import annotations

import os
import subprocess
import sys
import threading
from pathlib import Path

from . import config
from .qobuz import FORMAT_MP3_320, QobuzClient

EXCERPT_SECONDS = 90
MIN_TRACK_SECONDS = 45
# Below this an excerpt is not worth analysing; Essentia's descriptors over a
# couple of seconds are noise.
MIN_EXCERPT_SECONDS = 20
DEFAULT_CACHE_BYTES = 20 * 1024**3  # 20 GB
# How much new audio to accept before re-measuring the cache. See maybe_evict.
RESCAN_BYTES = 512 * 1024**2
# Deliberately not ".part.flac": the cache is swept with rglob("*.flac"), which
# put in-flight downloads on the eviction list. ffmpeg is told the format
# explicitly since it can no longer infer it.
TEMP_SUFFIX = ".part"


class AudioError(RuntimeError):
    pass


def _size(path: Path) -> int:
    """Size in bytes, or 0 if the file is gone. Never raises.

    Worth the indirection: the cache is swept concurrently with downloads, so
    any file listed a moment ago may be unlinked by the time it is statted.
    """
    try:
        return path.stat().st_size
    except OSError:
        return 0


def _mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except OSError:
        return 0.0


class AudioCache:
    """Size-capped LRU of decoded excerpts, keyed by track id.

    Safe to share across downloader threads.
    """

    def __init__(self, root: Path | None = None, max_bytes: int = DEFAULT_CACHE_BYTES):
        self.root = root or config.CACHE_DIR
        self.max_bytes = max_bytes
        self.root.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        # Bytes written since the last full scan; see maybe_evict.
        self._unscanned = 0

    def path_for(self, track_id: int) -> Path:
        # Shard by the last two digits to keep directory sizes sane.
        shard = f"{track_id % 100:02d}"
        d = self.root / shard
        d.mkdir(exist_ok=True)
        return d / f"{track_id}.flac"

    def get(self, track_id: int) -> Path | None:
        path = self.path_for(track_id)
        if _size(path) > 0:
            os.utime(path, None)  # mark as recently used
            return path
        return None

    def entries(self) -> list[Path]:
        """Finished excerpts. Never the in-flight temporaries, see TEMP_SUFFIX."""
        return list(self.root.rglob("*.flac"))

    def total_bytes(self) -> int:
        return sum(_size(f) for f in self.entries())

    def evict(self) -> int:
        """Delete least-recently-used excerpts until under the cap. Returns count freed.

        Every stat here is tolerant of the file having gone: eviction runs from
        whichever worker happened to cross the threshold, while others are busy
        writing and a second `analyse` process may be sweeping the same cache.
        """
        files = sorted(self.entries(), key=_mtime)
        total = sum(_size(f) for f in files)
        removed = 0
        for f in files:
            if total <= self.max_bytes:
                break
            total -= _size(f)
            f.unlink(missing_ok=True)
            removed += 1
        return removed

    def maybe_evict(self, written: int) -> int:
        """Evict, but only once enough new audio has landed to be worth checking.

        `total_bytes` stats every file in the cache, which was being paid after
        every single download: O(cache size) per track, and at a hundred
        thousand excerpts that alone outweighs the analysis. Amortise it over
        RESCAN_BYTES instead, and let the cache overshoot the cap by that much
        in between.
        """
        with self._lock:
            self._unscanned += written
            if self._unscanned < RESCAN_BYTES:
                return 0
            self._unscanned = 0

        if self.total_bytes() <= self.max_bytes:
            return 0
        return self.evict()


def _audio_duration(path: Path) -> float:
    """Seconds of decodable audio in a file, or 0.0 if it cannot be read."""
    proc = subprocess.run(
        [
            "ffprobe", "-v", "error",
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
            str(path),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    try:
        return float(proc.stdout.strip())
    except ValueError:
        return 0.0


def _excerpt_start(duration: int | None) -> float:
    """Centre the excerpt window in the track."""
    if not duration or duration <= EXCERPT_SECONDS:
        return 0.0
    return max(0.0, (duration - EXCERPT_SECONDS) / 2)


def download_excerpt(url: str, dest: Path, duration: int | None) -> Path:
    """Decode the middle window straight from the remote URL into mono FLAC.

    ffmpeg's HTTP protocol issues range requests for the seek, so this pulls only
    the bytes it needs rather than the whole file.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    # Unique per caller: several threads may race on the same track id after a
    # retry, and a shared temp file would have them clobbering each other.
    tmp = dest.with_name(
        f"{dest.stem}.{os.getpid()}.{threading.get_ident():x}{TEMP_SUFFIX}"
    )
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel", "error",
        "-nostdin",
        "-ss", f"{_excerpt_start(duration):.2f}",
        "-i", url,
        "-t", str(EXCERPT_SECONDS),
        "-ac", "1",            # mono
        "-ar", "44100",        # Essentia resamples to 48k for CLAP from here
        "-sample_fmt", "s16",
        "-f", "flac",          # the temp name has no extension to infer from
        "-y", str(tmp),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    if proc.returncode != 0 or _size(tmp) == 0:
        tmp.unlink(missing_ok=True)
        raise AudioError(f"ffmpeg failed: {proc.stderr.strip()[:300]}")

    # ffmpeg exits 0 with a header and no samples when the seek lands past the
    # end. Catch it here: a junk excerpt that reaches the cache is served from
    # then on without revalidation.
    seconds = _audio_duration(tmp)
    if seconds < MIN_EXCERPT_SECONDS:
        tmp.unlink(missing_ok=True)
        raise AudioError(
            f"excerpt decoded to only {seconds:.1f}s of audio "
            "(empty stream, or the track duration Qobuz reported was wrong)"
        )

    try:
        tmp.replace(dest)
    except OSError as exc:
        tmp.unlink(missing_ok=True)
        raise AudioError(f"could not move the excerpt into place: {exc}") from exc

    return dest


def ensure_excerpt(
    client: QobuzClient,
    track_id: int,
    duration: int | None,
    cache: AudioCache | None = None,
) -> Path:
    """Return a local excerpt for a track, downloading it if not cached."""
    cache = cache or AudioCache()

    cached = cache.get(track_id)
    if cached is not None:
        return cached

    if duration is not None and duration < MIN_TRACK_SECONDS:
        raise AudioError(f"track too short to analyse ({duration}s)")

    info = client.file_url(track_id, format_id=FORMAT_MP3_320)
    url = info.get("url")
    if not url:
        raise AudioError(f"no stream url returned (restricted or unavailable): {info}")

    dest = cache.path_for(track_id)
    path = download_excerpt(url, dest, duration)

    freed = cache.maybe_evict(_size(path))
    if freed:
        print(f"  cache evicted {freed} excerpts", file=sys.stderr)

    return path
