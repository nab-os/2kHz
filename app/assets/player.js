// Playback transport.
//
// Owns the <audio> element, so seeking and progress never round-trip through
// Rust. Only track-boundary events come back.

(async () => {
  // Scheduled from the render that mounts the element, so poll briefly rather
  // than assuming the DOM has flushed.
  let audio = document.getElementById("player");
  for (let attempt = 0; attempt < 60 && !audio; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 50));
    audio = document.getElementById("player");
  }
  if (!audio) return;

  window.qsuggestPlayUrl = (url) => {
    audio.src = url;
    // A previous track's position survives a src swap in some webviews.
    audio.currentTime = 0;
    audio.play().catch(() => {});
  };
  window.qsuggestResume = () => audio.play().catch(() => {});
  window.qsuggestPause = () => audio.pause();
  window.qsuggestStop = () => {
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  };
  window.qsuggestSeek = (fraction) => {
    if (Number.isFinite(audio.duration)) audio.currentTime = fraction * audio.duration;
  };
  window.qsuggestVolume = (value) => {
    audio.volume = Math.max(0, Math.min(1, value));
  };

  audio.addEventListener("ended", () => dioxus.send({ type: "ended" }));
  audio.addEventListener("play", () => dioxus.send({ type: "playing", playing: true }));
  audio.addEventListener("pause", () => dioxus.send({ type: "playing", playing: false }));

  audio.addEventListener("error", () => {
    // Signed URLs expire, so a late click on a stale queue entry lands here
    // rather than on the Qobuz error path.
    dioxus.send({ type: "failed" });
  });

  // timeupdate fires about four times a second; twice is plenty for a progress
  // bar and leaves room on the channel for the map's selection messages.
  let lastSent = 0;
  audio.addEventListener("timeupdate", () => {
    const now = performance.now();
    if (now - lastSent < 500) return;
    lastSent = now;
    dioxus.send({
      type: "time",
      position: audio.currentTime || 0,
      duration: Number.isFinite(audio.duration) ? audio.duration : 0,
    });
  });

  // Keep the channel open so Rust keeps receiving transport events.
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
})();
