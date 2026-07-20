// Casual RAS — compact sharing control strip (issue #5).
//
// Shown (always-on-top) while the main window is minimized OUT of the shared screen during an active
// share. Carries the always-visible active indicator + Stop (Invariant 7 — the stop control is never
// hidden), plus a "Controls" button to bring the full window back if the host needs chat/files.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const label = document.getElementById("label");
listen("share-control", (e) => {
  label.textContent = e.payload ? "REMOTE CONTROL ACTIVE" : "REMOTE VIEWING ACTIVE";
});
listen("share-viewer", (e) => {
  if (e.payload) label.textContent = "REMOTE VIEWING ACTIVE";
});

document.getElementById("stop").addEventListener("click", () => {
  invoke("stop_sharing").catch(() => {});
});
document.getElementById("show").addEventListener("click", () => {
  invoke("show_main_window").catch(() => {});
});
