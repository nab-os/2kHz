"""Navigation: neighbours, paths, drift, radio.

Reference implementation. The Rust app reimplements these for the UI; the two
are kept in sync by a parity test over a fixed seed, so this file is the oracle.

Brute-force cosine is the right call at this scale: 50k x 81 float32 is ~16MB
and a query is a single matrix-vector product.
"""

from __future__ import annotations

import heapq
import json
import sqlite3
from dataclasses import dataclass, field
from typing import Any

import numpy as np

from . import space


def _order(scores: np.ndarray) -> np.ndarray:
    """Indices by descending score, ties broken by index.

    The stable sort is not cosmetic: it is what makes the Rust port comparable
    for exact equality. numpy's default argsort is unstable, so two equally
    similar tracks could come back in either order between runs.
    """
    return np.argsort(-scores, kind="stable")


@dataclass
class Constraints:
    """Rules applied while walking, to stop paths degenerating into one artist."""

    artist_cooldown: int = 3          # no repeat artist within this many steps
    max_bpm_delta: float | None = None  # max |ΔBPM| between consecutive picks
    allow_same_album_consecutive: bool = False
    exclude: set[int] = field(default_factory=set)


class Navigator:
    def __init__(self, conn: sqlite3.Connection, weights: dict[str, float] | None = None):
        self.vectors, self.manifest = space.load(weights)
        self.track_ids = np.asarray(self.manifest["track_ids"], dtype=np.int64)
        self.index_of = {int(t): i for i, t in enumerate(self.track_ids)}

        norms = np.linalg.norm(self.vectors, axis=1, keepdims=True)
        self.unit = self.vectors / np.clip(norms, 1e-9, None)

        self._load_metadata(conn)
        self._load_clap(conn)
        self._knn_cache: tuple[np.ndarray, np.ndarray] | None = None

    # ---------------------------------------------------------------- setup

    def _load_metadata(self, conn: sqlite3.Connection) -> None:
        rows = {
            r["id"]: r
            for r in conn.execute(
                """
                SELECT t.id, t.title, t.artist_id, t.album_id, t.seed_distance,
                       ar.name AS artist_name, al.title AS album_title,
                       al.genre AS genre, al.release_date,
                       f.essentia_json
                FROM tracks t
                LEFT JOIN artists ar ON ar.id = t.artist_id
                LEFT JOIN albums  al ON al.id = t.album_id
                LEFT JOIN features f ON f.track_id = t.id
                """
            )
        }
        n = len(self.track_ids)
        self.title = [""] * n
        self.artist_name = [""] * n
        self.album_title = [""] * n
        self.genre = [""] * n
        self.artist_id = np.full(n, -1, dtype=np.int64)
        self.album_id = [""] * n
        self.seed_distance = np.zeros(n, dtype=np.int32)
        self.bpm = np.full(n, np.nan, dtype=np.float64)

        for track_id, i in self.index_of.items():
            row = rows.get(track_id)
            if row is None:
                continue
            self.title[i] = row["title"] or ""
            self.artist_name[i] = row["artist_name"] or ""
            self.album_title[i] = row["album_title"] or ""
            self.genre[i] = row["genre"] or ""
            self.artist_id[i] = row["artist_id"] if row["artist_id"] is not None else -1
            self.album_id[i] = row["album_id"] or ""
            self.seed_distance[i] = row["seed_distance"] or 0
            descriptors = json.loads(row["essentia_json"] or "{}")
            if descriptors.get("bpm"):
                self.bpm[i] = float(descriptors["bpm"])

    def _load_clap(self, conn: sqlite3.Connection) -> None:
        """Raw CLAP audio embeddings, kept for text matching.

        Small enough to hold outright: 448 x 512 float32 is under a megabyte.
        """
        stored = {
            r["track_id"]: np.frombuffer(r["clap_f32"], dtype=np.float32)
            for r in conn.execute("SELECT track_id, clap_f32 FROM features WHERE clap_f32 IS NOT NULL")
        }
        if not stored:
            self.clap = None
            return
        width = len(next(iter(stored.values())))
        matrix = np.zeros((len(self.track_ids), width), dtype=np.float32)
        for track_id, i in self.index_of.items():
            vector = stored.get(track_id)
            if vector is not None and len(vector) == width:
                matrix[i] = vector
        self.clap = matrix

    def describe(self, i: int) -> dict[str, Any]:
        return {
            "track_id": int(self.track_ids[i]),
            "title": self.title[i],
            "artist": self.artist_name[i],
            "album": self.album_title[i],
            "genre": self.genre[i],
            "bpm": None if np.isnan(self.bpm[i]) else round(float(self.bpm[i]), 1),
            "seed_distance": int(self.seed_distance[i]),
        }

    # ------------------------------------------------------------ primitives

    def similarity(self, vector: np.ndarray) -> np.ndarray:
        unit = vector / max(float(np.linalg.norm(vector)), 1e-9)
        return self.unit @ unit

    def neighbours(
        self, track_id: int, k: int = 10, exclude_same_artist: bool = False
    ) -> list[dict[str, Any]]:
        i = self.index_of[track_id]
        sims = self.similarity(self.vectors[i])
        sims[i] = -np.inf
        if exclude_same_artist:
            sims[self.artist_id == self.artist_id[i]] = -np.inf
        top = _order(sims)[:k]
        return [{**self.describe(j), "similarity": round(float(sims[j]), 4)} for j in top]

    def _allowed(self, candidate: int, chosen: list[int], constraints: Constraints) -> bool:
        if int(self.track_ids[candidate]) in constraints.exclude:
            return False
        if not chosen:
            return True

        recent = chosen[-constraints.artist_cooldown :] if constraints.artist_cooldown else []
        if self.artist_id[candidate] != -1 and self.artist_id[candidate] in {
            self.artist_id[c] for c in recent
        }:
            return False

        previous = chosen[-1]
        if not constraints.allow_same_album_consecutive:
            if self.album_id[candidate] and self.album_id[candidate] == self.album_id[previous]:
                return False

        if constraints.max_bpm_delta is not None:
            a, b = self.bpm[candidate], self.bpm[previous]
            if np.isfinite(a) and np.isfinite(b) and abs(a - b) > constraints.max_bpm_delta:
                return False

        return True

    def _snap(
        self, waypoint: np.ndarray, chosen: list[int], used: set[int], constraints: Constraints
    ) -> int | None:
        """Nearest track to a waypoint that satisfies the constraints."""
        sims = self.similarity(waypoint)
        for candidate in _order(sims):
            if candidate in used:
                continue
            if self._allowed(int(candidate), chosen, constraints):
                return int(candidate)
        return None

    # ----------------------------------------------------------------- modes

    def interpolate(
        self, from_id: int, to_id: int, steps: int = 12, constraints: Constraints | None = None
    ) -> list[dict[str, Any]]:
        """Walk the straight line from A to B, snapping to real tracks."""
        constraints = constraints or Constraints()
        a, b = self.index_of[from_id], self.index_of[to_id]
        # Waypoints are built from unit vectors: the metric is cosine, so a
        # track with a large norm should not drag the line towards itself.
        start, end = self.unit[a], self.unit[b]

        chosen = [a]
        used = {a, b}
        for step in range(1, steps - 1):
            t = step / (steps - 1)
            pick = self._snap((1 - t) * start + t * end, chosen, used, constraints)
            if pick is None:
                break
            chosen.append(pick)
            used.add(pick)
        chosen.append(b)
        return [self.describe(i) for i in chosen]

    def _knn_graph(self, k: int = 16) -> tuple[np.ndarray, np.ndarray]:
        """Symmetric-ish kNN graph over the whole space, cached per Navigator."""
        if self._knn_cache is not None and self._knn_cache[0].shape[1] == k:
            return self._knn_cache
        sims = self.unit @ self.unit.T
        np.fill_diagonal(sims, -np.inf)
        # Stable sort per row, so the graph is identical to the Rust one.
        ordered = np.argsort(-sims, axis=1, kind="stable")[:, :k]
        rows = np.arange(sims.shape[0])[:, None]
        dist = 1.0 - sims[rows, ordered]
        self._knn_cache = (ordered, dist)
        return self._knn_cache

    def graph_path(
        self, from_id: int, to_id: int, k: int = 16, constraints: Constraints | None = None
    ) -> list[dict[str, Any]]:
        """Dijkstra over the kNN graph.

        Usually better than interpolate: a straight line through a high
        dimensional space crosses empty regions where the nearest track is
        arbitrary, whereas this stays on the data manifold.
        """
        constraints = constraints or Constraints()
        neighbours, distances = self._knn_graph(k)
        start, goal = self.index_of[from_id], self.index_of[to_id]

        best = {start: 0.0}
        previous: dict[int, int] = {}
        queue = [(0.0, start)]
        visited: set[int] = set()

        while queue:
            cost, node = heapq.heappop(queue)
            if node in visited:
                continue
            visited.add(node)
            if node == goal:
                break
            for neighbour, edge in zip(neighbours[node], distances[node]):
                neighbour = int(neighbour)
                if neighbour in visited:
                    continue
                penalty = 0.0
                if self.artist_id[neighbour] == self.artist_id[node]:
                    penalty += 0.05  # nudge away from same-artist chains
                new_cost = cost + float(edge) + penalty
                if new_cost < best.get(neighbour, np.inf):
                    best[neighbour] = new_cost
                    previous[neighbour] = node
                    heapq.heappush(queue, (new_cost, neighbour))

        if goal not in previous and goal != start:
            return []

        path = [goal]
        while path[-1] != start:
            path.append(previous[path[-1]])
        return [self.describe(i) for i in reversed(path)]

    def semantic_block(self) -> dict[str, Any]:
        return next(b for b in self.manifest["blocks"] if b["name"] == "semantic")

    def text_anchors(self, clap_vector: np.ndarray, k: int = 5) -> list[int]:
        """Indices of the tracks whose audio best matches a phrase."""
        if self.clap is None:
            raise RuntimeError("CLAP embeddings were not loaded")
        sims = self.clap @ clap_vector.astype(np.float32)
        return [int(i) for i in np.argsort(-sims, kind="stable")[:k]]

    def text_target(self, clap_vector: np.ndarray, k: int = 5) -> np.ndarray:
        """A destination in the space for a described sound.

        Deliberately *not* the text embedding pushed through the audio PCA.
        CLAP has a modality gap: text and audio embeddings rank correctly
        against each other under cosine, but sit in offset regions of the 512-d
        space. Projecting text through a PCA fitted on audio therefore yields
        coordinates that encode the gap rather than the content, and a walk
        towards them never arrives. Averaging the best-matching tracks keeps the
        target inside the audio distribution, where the rest of the space lives.
        """
        anchors = self.text_anchors(clap_vector, k=k)
        return self.unit[anchors].mean(axis=0)

    def drift_to_text(
        self,
        from_id: int,
        clap_vector: np.ndarray,
        steps: int = 12,
        k: int = 5,
        constraints: Constraints | None = None,
    ) -> list[dict[str, Any]]:
        """Walk from a track towards a described sound."""
        constraints = constraints or Constraints()
        start_index = self.index_of[from_id]
        start = self.unit[start_index]
        target = self.text_target(clap_vector, k=k)

        chosen = [start_index]
        used = {start_index}
        for step in range(1, steps):
            t = step / (steps - 1)
            pick = self._snap((1 - t) * start + t * target, chosen, used, constraints)
            if pick is None:
                break
            chosen.append(pick)
            used.add(pick)
        return [self.describe(i) for i in chosen]

    def drift(
        self,
        from_id: int,
        direction: np.ndarray,
        steps: int = 12,
        reach: float = 1.5,
        constraints: Constraints | None = None,
    ) -> list[dict[str, Any]]:
        """Walk away from a track along a direction vector."""
        constraints = constraints or Constraints()
        start_index = self.index_of[from_id]
        # Unit vector, so `reach` means the same thing regardless of where the
        # starting track sits. See interpolate() for the same reasoning.
        start = self.unit[start_index]

        chosen = [start_index]
        used = {start_index}
        for step in range(1, steps):
            waypoint = start + direction * (reach * step / (steps - 1))
            pick = self._snap(waypoint, chosen, used, constraints)
            if pick is None:
                break
            chosen.append(pick)
            used.add(pick)
        return [self.describe(i) for i in chosen]

    def radio_nearest(
        self,
        from_id: int,
        steps: int = 20,
        artist_penalty: float = 0.0,
        constraints: Constraints | None = None,
    ) -> list[dict[str, Any]]:
        """Greedy walk: each jump goes to the most similar track not yet played.

        Deterministic, unlike `radio`, which samples. The difference matters in
        practice because a prolific artist occupies a tight cluster, so pure
        greedy tends to sit inside one discography. `artist_penalty` is
        subtracted from the similarity of any candidate whose artist appears in
        the last ARTIST_WINDOW picks, which bends the walk outward without
        forbidding a return later.
        """
        constraints = constraints or Constraints()
        current = self.index_of[from_id]
        chosen = [current]
        used = {current}

        for _ in range(steps - 1):
            sims = self.similarity(self.vectors[current]).copy()
            sims[list(used)] = -np.inf

            if artist_penalty > 0:
                seen: dict[int, int] = {}
                for c in chosen:
                    artist = int(self.artist_id[c])
                    if artist != -1:
                        seen[artist] = seen.get(artist, 0) + 1
                for artist, times in seen.items():
                    sims[self.artist_id == artist] -= artist_penalty * times

            order = np.argsort(-sims, kind="stable")
            pick = next(
                (
                    int(c)
                    for c in order
                    if np.isfinite(sims[c]) and self._allowed(int(c), chosen, constraints)
                ),
                None,
            )
            if pick is None:
                break
            chosen.append(pick)
            used.add(pick)
            # The walk has to move: similarities for the next jump come from
            # where we just landed, not from where we started.
            current = pick

        return [self.describe(i) for i in chosen]

    def radio(
        self,
        from_id: int,
        steps: int = 20,
        temperature: float = 0.3,
        k: int = 20,
        constraints: Constraints | None = None,
        seed: int | None = None,
    ) -> list[dict[str, Any]]:
        """Stochastic nearest-neighbour walk: similar, but not a straight line."""
        constraints = constraints or Constraints()
        rng = np.random.default_rng(seed)
        current = self.index_of[from_id]
        chosen = [current]
        used = {current}

        for _ in range(steps - 1):
            sims = self.similarity(self.vectors[current])
            candidates = [
                int(c)
                for c in np.argsort(-sims)[: k * 4]
                if c not in used and self._allowed(int(c), chosen, constraints)
            ][:k]
            if not candidates:
                break
            scores = np.array([sims[c] for c in candidates])
            weights = np.exp((scores - scores.max()) / max(temperature, 1e-6))
            current = int(rng.choice(candidates, p=weights / weights.sum()))
            chosen.append(current)
            used.add(current)

        return [self.describe(i) for i in chosen]
