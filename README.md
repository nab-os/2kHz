# Qobuz suggestion system

Music as a navigable space: every track a point in ~80 dimensions where distance
approximates perceptual similarity, so recommendation becomes geometry.

Qobuz's API returns metadata only (no BPM, no key, no energy), so the acoustic
half of every vector is computed from the audio itself.

Python does the modelling (Essentia, CLAP, UMAP); Rust does I/O and interaction.
