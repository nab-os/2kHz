# Glossary

Terms used in the course, with the chapter that explains each.

**Anchor**: in queue sorting, the playing track, used as the fixed start
of the path and not returned. (13)

**Artist penalty / artist pull**: similarity subtracted from a radio
candidate once per earlier appearance of its artist in the walk. Default
0.10. (13)

**Autocorrelation**: the correlation of a signal with a shifted copy of
itself; peaks at the beat period for a rhythmic onset envelope. (7)

**Bit reservoir**: MP3's mechanism for letting a frame use bits stored in
earlier frames; why the first frame of a byte range fails to decode. (5)

**Block**: one named group of dimensions in the space (tempo, key,
dynamics, timbre, mood, style, era, semantic), each normalised separately
and weighted as a unit. (8)

**Buildable**: an analysed, non-blocked track with a CLAP blob; what
`build-space` would include. (10)

**Catalogue / `catalog.db`**: the slim SQLite projection of `two_khz.db`
that clients sync. (2)

**Chroma**: a 12-bin profile of how much of each pitch class is present. (7)

**Circle of fifths**: the ordering C, G, D, A… in which adjacent keys share
six of seven notes; used to place keys in the key block. (8)

**CLAP**: Contrastive Language-Audio Pretraining; paired audio and text
networks mapping into one embedding space. Checkpoint:
`laion/clap-htsat-unfused`. (6)

**Contrast (mood)**: a mood score computed as similarity to one phrase minus
similarity to its opposite. (8)

**Cosine similarity**: the dot product of two unit vectors; 1 for
identical direction. Everything in the space is compared this way. (8, 12)

**Cursor (log)**: a count of lines ever pushed, used to read "what is new"
from a `LogBuffer`. (10)

**Descriptors**: the non-neural features: BPM, onset rate, key, loudness,
spectral centroid/rolloff/flatness, zero-crossing rate. (7)

**Device token**: a 256-bit random secret identifying one paired client;
stored only as its SHA-256. (11)

**Dijkstra**: shortest-path algorithm used by `graph_path` over the kNN
graph. (13)

**Drift**: a walk from a track towards the region matching a phrase. (13)

**EBU R128**: the broadcast loudness standard; integrated loudness (LUFS)
and loudness range (LU). (7)

**Engine**: the client's global holding the memory-mapped space and the
navigator. (12)

**Epoch (library)**: a counter that lets a late fetch notice it has been
superseded. (15)

**Excerpt**: the middle 90 seconds of a track, fetched by byte range. (5)

**Extractor version**: `analyse::VERSION`; rows with another version are
re-analysed. (2, 5)

**Folding (BPM)**: doubling or halving a tempo into [70, 140) so octave
errors do not matter. (8)

**Frame sync**: finding a real MP3 frame boundary: three consecutive valid
headers. (5)

**Frontier**: the crawl's queue of artists and albums to expand, in SQLite. (4)

**Fuzzy union**: `a + b - ab`; how UMAP symmetrises directed neighbour
weights. (9)

**Generation**: a server counter bumped when build-space or layout
finishes; clients resync when it changes. (10)

**Haystack**: the lowercased artist/title/album of a track, precomputed for
search. (14)

**ISRC**: International Standard Recording Code; identifies a recording
across releases. Used for deduplication. (2, 3)

**Job**: a stage's log sink plus cancel flag. (10)

**kNN graph**: each track linked to its *k* most similar tracks. (9, 13)

**Krumhansl-Kessler profiles**: perceptual weights of scale degrees, used
to match chroma to a key. (7)

**Log-mel spectrogram**: energy per mel band per time frame, in dB; CLAP's
input. (6)

**Manifest**: `space.json`: track order, block layout, default weights. (8)

**Mel scale (Slaney)**: a perceptual frequency scale, linear below 1kHz and
logarithmic above. (6)

**Modality gap**: text and audio embeddings occupying separate regions of
CLAP's space; why drift anchors on tracks rather than projecting text. (13)

**Negative sampling**: UMAP's trick of repelling a few random points per
attraction step instead of all pairs. (9)

**Off-thread**: running a `!Send` future on a blocking-pool thread with its
own runtime. (1, 10)

**Onset envelope / spectral flux**: the summed positive change in band
energies; spikes at note onsets. (7)

**PCA**: principal component analysis; keeps the directions of greatest
variance. Used for style (60→20), semantic (512→40) and the map's initial
positions. (8, 9)

**Recipe**: the record of which mode and inputs produced a generated
result. (15)

**Row normalisation**: scaling each track's block vector to unit length. (8)

**Scope**: `play` or `pipeline`; what a device token may do. (11)

**Seed distance**: similar-artist hops from a favourite; 0 is your own
library. (2, 4)

**Selection / Detail**: the space track "in hand" vs. what the detail sheet
shows; separate signals. (14)

**Signed URL**: a time-limited Qobuz CDN URL from `track/getFileUrl`. (3)

**Space**: the per-track vectors (`space.bin`) plus the manifest. (8, 12)

**SSE**: Server-Sent Events; how the pipeline log streams. (11)

**Stale (result)**: a generated result whose weights have since changed. (15)

**Token bucket**: the rate limiter: burst of 4, refilling at 2/s, shared
by every client of one account. (3)

**2-opt**: a local improvement for path problems: reverse a segment if it
shortens the route. (13)

**UMAP**: the projection that draws the map. (9)

**Weighted space**: the space with block weights applied and rows
re-normalised, ready for one-dot-product cosine queries. (12)

**Z-score**: subtract the mean, divide by the standard deviation. (8)

**Zero-shot**: classifying against text labels with no training for those
labels. (6, 8)
