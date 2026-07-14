// Casual RAS host — control panel.
//
// Shows the connection ticket to share, a live session indicator (Invariant 7 — always visible while
// sharing, not suppressible by the UI), and a Stop control. The Rust side emits `ticket` / `status` /
// `connected` events; Stop invokes `stop_sharing`.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ticket = document.getElementById("ticket");
const copyBtn = document.getElementById("copy");
const statusEl = document.getElementById("status");
const indicator = document.getElementById("indicator");
const stopBtn = document.getElementById("stop");

listen("ticket", (e) => {
  ticket.value = e.payload;
});

listen("status", (e) => {
  statusEl.textContent = e.payload;
});

listen("connected", (e) => {
  const live = !!e.payload;
  indicator.textContent = live ? "● REMOTE VIEWING ACTIVE" : "● IDLE";
  indicator.className = live ? "live" : "idle";
});

copyBtn.addEventListener("click", async () => {
  ticket.select();
  try {
    await navigator.clipboard.writeText(ticket.value);
    copyBtn.textContent = "Copied";
    setTimeout(() => (copyBtn.textContent = "Copy"), 1200);
  } catch (_) {
    // Fallback: the text is already selected for a manual Cmd-C.
  }
});

stopBtn.addEventListener("click", () => invoke("stop_sharing"));
