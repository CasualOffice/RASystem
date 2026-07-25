/* Casual RAS — contacts-first desktop shell (Phase 1).
 *
 * Wired to the LIVE backend for the parts that exist today — contacts, presence, and out-of-session
 * messaging — via the real Tauri commands (`list_contacts` / `list_online` / `send_message` /
 * `add_contact` / `set_contact_blocked` / `call_contact`) and the `presence` / `message` / `call-request`
 * events. The remote-session and 1:1-call flows are wired to their real backends too: the session
 * viewer/control/audio/chat/clipboard/file path, and the call commands (`call_place`/`accept`/`decline`/
 * `hangup`/`set_mute`) + the content-free `call-lifecycle` event stream (ADR-104). Where a live command
 * isn't present in the current build the attempt is caught and the demo preview / an honest toast shows
 * instead. Secret hygiene (Inv 8): message bodies are rendered with `textContent`, never innerHTML.
 *
 * Runs in two modes: inside the Tauri app (`window.__TAURI__` present) → real data; opened as a plain
 * file / artifact → a demo dataset so the shell always renders. */
(function () {
  "use strict";
  const el = (s) => document.querySelector(s);
  const T = window.__TAURI__;
  const invoke = T && T.core ? T.core.invoke : null;
  const listen = T && T.event ? T.event.listen : null;
  const LIVE = !!invoke;

  // ---- demo dataset (used only when not running inside the app) ----
  const demo = [
    { id: "d-priya", name: "Priya Nair", init: "PN", color: "#3f5b8a", pres: "online", time: "2:41 PM", prev: "Sounds good — calling you now.", unread: 0, msgs: [
      { who: "them", time: "2:38 PM", text: "Did the reconnect fix land? I want to test the WiFi-blip resume." },
      { who: "me", time: "2:39 PM", text: "Yep, merged this morning. Sharing now — you should see the reconnect fire in about ten seconds.", img: true },
      { who: "them", time: "2:41 PM", text: "Perfect. Sounds good — calling you now.", react: true } ] },
    { id: "d-alex", name: "Alex Rivera", init: "AR", color: "#7a5c3a", pres: "insession", time: "2:33 PM", prev: "Screen shared with you", session: true, unread: 0, msgs: [
      { who: "them", time: "2:30 PM", text: "Can you take a look at my build? Something's off with the linker on Windows." },
      { who: "me", time: "2:31 PM", text: "Sure — send a screen-access request and I'll drive." },
      { who: "them", time: "2:33 PM", text: "Requesting now.", file: true } ] },
    { id: "d-devon", name: "Devon Cole", init: "DC", color: "#4a6b57", pres: "away", time: "1:12 PM", prev: "Devon: let's sync after lunch", unread: 2, msgs: [
      { who: "them", time: "1:10 PM", text: "The audio pump refactor is ready for review whenever." },
      { who: "them", time: "1:12 PM", text: "Let's sync after lunch?" } ] },
    { id: "d-marcus", name: "Marcus Bell", init: "MB", color: "#6b4a63", pres: "dnd", time: "11:48 AM", prev: "You: shipped the token pass", unread: 0, msgs: [
      { who: "me", time: "11:48 AM", text: "Shipped the token pass, take a look when you're free." } ] },
    { id: "d-sofia", name: "Sofia Lindqvist", init: "SL", color: "#3a5f6b", pres: "offline", time: "Yesterday", prev: "Sofia: thanks for the help earlier", muted: true, unread: 0, msgs: [
      { who: "them", time: "Yesterday", text: "Thanks for the help earlier — the portal consent finally worked." } ] },
  ];

  let contacts = [];
  let active = null, inCall = false, callWith = null, inSession = false;

  const PALETTE = ["#3f5b8a", "#7a5c3a", "#4a6b57", "#6b4a63", "#3a5f6b", "#5a5478", "#4a5578", "#6b5a3a"];
  const initials = (name) => (name || "?").trim().split(/\s+/).slice(0, 2).map((w) => w[0] || "").join("").toUpperCase() || "?";
  const colorFor = (id) => PALETTE[[...(id || "")].reduce((a, c) => a + c.charCodeAt(0), 0) % PALETTE.length];
  const shortId = (id) => (id || "").slice(0, 6);

  // ---- data layer ----
  async function loadContacts() {
    if (!LIVE) { contacts = demo.map((c) => ({ ...c })); active = contacts[0].id; return; }
    let dtos = [];
    try { dtos = await invoke("list_contacts"); } catch (_) { dtos = []; }
    let online = [];
    try { online = await invoke("list_online"); } catch (_) { online = []; }
    const onset = new Set(online);
    contacts = dtos.map((d) => ({
      id: d.id,
      name: d.label || shortId(d.id),
      init: initials(d.label || shortId(d.id)),
      color: colorFor(d.id),
      code: d.code,
      blocked: d.blocked,
      pres: d.blocked ? "offline" : onset.has(d.id) ? "online" : "offline",
      time: "",
      prev: d.blocked ? "Blocked" : "No messages yet",
      unread: 0,
      msgs: [],
    }));
    if (!active && contacts[0]) active = contacts[0].id;
  }

  const byId = (id) => contacts.find((c) => c.id === id);
  const now = () => new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });

  // ---- list render ----
  const presIcon = (p) => `<span class="presence ${p}"><i></i></span>`;
  const convs = el("#convs");
  function renderList() {
    if (!contacts.length) {
      convs.innerHTML = `<div style="padding:24px 12px;color:var(--ink-faint);font-size:13px;text-align:center">No contacts yet.<br/>Add one to start.</div>`;
      return;
    }
    convs.innerHTML = contacts.map((c) => `
      <div class="conv ${c.id === active ? "active" : ""} ${c.muted ? "muted" : ""}" data-id="${c.id}" tabindex="0" role="button">
        <span class="avatar s46" style="background:${c.color}">${c.init}${presIcon(c.pres)}</span>
        <span class="cname"></span>
        <span class="ctime">${c.time}</span>
        <span class="cprev ${c.session ? "session" : ""}"></span>
        <span class="cmeta">${c.unread ? `<span class="badge ${c.muted ? "muted" : ""}">${c.unread}</span>` : ""}</span>
      </div>`).join("");
    // set text nodes safely (Inv 8 — never innerHTML for names/previews)
    convs.querySelectorAll(".conv").forEach((row) => {
      const c = byId(row.dataset.id);
      row.querySelector(".cname").textContent = c.name;
      row.querySelector(".cprev").textContent = c.prev;
    });
  }

  // ---- conversation render ----
  function renderMain() {
    const c = byId(active);
    if (!c) { el("#topbar").innerHTML = ""; el("#thread").innerHTML = `<div style="margin:auto;color:var(--ink-faint)">Select a contact.</div>`; return; }
    el("#topbar").innerHTML = `
      <div class="who">
        <span class="avatar s32" style="background:${c.color}">${c.init}${presIcon(c.pres)}</span>
        <div><div class="nm"></div><div class="st">${statusText(c)}</div></div>
      </div>
      <div class="actions">
        <button class="iconbtn" id="actCall" title="Voice call"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M5 4h4l2 5-3 2a12 12 0 0 0 5 5l2-3 5 2v4a2 2 0 0 1-2 2A16 16 0 0 1 3 6a2 2 0 0 1 2-2z"/></svg></button>
        <button class="iconbtn" id="actVideo" title="Video call"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="m23 7-7 5 7 5V7z"/><rect x="1" y="5" width="15" height="14" rx="2"/></svg></button>
        <button class="iconbtn" id="actScreen" title="Request / share screen"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><rect x="2" y="4" width="20" height="13" rx="2"/><path d="M8 21h8M12 17v4"/></svg></button>
        <button class="iconbtn" id="actInfo" title="Details"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/></svg></button>
      </div>`;
    el("#topbar .nm").textContent = c.name;
    const th = el("#thread");
    th.innerHTML = "";
    if (!c.msgs.length) {
      const empty = document.createElement("div");
      empty.style.cssText = "margin:auto;text-align:center;color:var(--ink-faint);font-size:13px";
      empty.textContent = LIVE ? "No messages yet — say hello. Messages deliver when they're online." : "Start of your conversation.";
      th.appendChild(empty);
    } else {
      const day = document.createElement("div"); day.className = "daydiv"; day.textContent = "Today"; th.appendChild(day);
      let lastWho = null;
      c.msgs.forEach((m) => {
        const grouped = m.who === lastWho; lastWho = m.who;
        const who = m.who === "me" ? { init: "YM", color: "#4a5578", name: "You" } : { init: c.init, color: c.color, name: c.name };
        const row = document.createElement("div");
        row.className = "msg" + (grouped ? " grouped" : "");
        if (grouped) { const ht = document.createElement("div"); ht.className = "hovertime"; ht.textContent = m.time; row.appendChild(ht); }
        else { const g = document.createElement("div"); g.className = "gut"; g.innerHTML = `<span class="avatar s40" style="background:${who.color}"></span>`; g.querySelector(".avatar").textContent = who.init; row.appendChild(g); }
        const body = document.createElement("div"); body.className = "body";
        if (!grouped) { const hd = document.createElement("div"); hd.className = "hd"; hd.innerHTML = `<span class="snm"></span><span class="stm">${m.time}</span>`; hd.querySelector(".snm").textContent = who.name; body.appendChild(hd); }
        const txt = document.createElement("div"); txt.className = "txt"; txt.textContent = m.text; body.appendChild(txt); // Inv 8: textContent, never innerHTML
        if (m.img) { const im = document.createElement("div"); im.className = "msg-img"; im.innerHTML = `<span class="imeta">reconnect-demo.png · 240 KB</span>`; body.appendChild(im); }
        if (m.file) { const fc = document.createElement("div"); fc.className = "filecard"; fc.innerHTML = `<span class="fi"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/></svg></span><div style="flex:1"><div class="fn">build-log-windows.txt</div><div class="fs">18.4 KB · Downloads</div></div>`; body.appendChild(fc); }
        if (m.react) { const r = document.createElement("div"); r.className = "reacts"; r.innerHTML = `<span class="chip mine">👍 3</span><span class="chip react-add" title="Add reaction"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 5v14M5 12h14"/></svg></span>`; body.appendChild(r); }
        row.appendChild(body); th.appendChild(row);
      });
    }
    th.scrollTop = th.scrollHeight;
    el("#cinput").placeholder = "Message " + c.name.split(" ")[0] + "…";
  }
  function statusText(c) {
    return c.pres === "online" ? "Active now" : c.pres === "insession" ? "In a session with you" : c.pres === "away" ? "Away" : c.pres === "dnd" ? "Do not disturb" : c.blocked ? "Blocked" : "Offline";
  }

  // ---- toast ----
  let toastT;
  function toast(msg, color) {
    const t = el("#toast"); el("#toasttx").textContent = msg;
    t.querySelector(".td").style.background = color || "var(--online)";
    t.classList.add("show"); clearTimeout(toastT); toastT = setTimeout(() => t.classList.remove("show"), 2600);
  }

  // ---- list / composer interactions ----
  convs.addEventListener("click", (e) => { const r = e.target.closest(".conv"); if (r) { active = r.dataset.id; const c = byId(active); if (c) c.unread = 0; renderList(); renderMain(); } });
  convs.addEventListener("keydown", (e) => { if ((e.key === "Enter" || e.key === " ") && e.target.classList.contains("conv")) { e.preventDefault(); active = e.target.dataset.id; renderList(); renderMain(); } });

  const cin = el("#cinput"), send = el("#send");
  cin.addEventListener("input", () => { send.disabled = !cin.value.trim(); });
  el("#composer").addEventListener("submit", async (e) => {
    e.preventDefault();
    const v = cin.value.trim(); if (!v || !active) return;
    const c = byId(active);
    c.msgs.push({ who: "me", time: now(), text: v });
    c.prev = "You: " + v; c.time = now();
    cin.value = ""; send.disabled = true; renderMain(); renderList();
    if (LIVE) { try { await invoke("send_message", { contactId: active, text: v }); } catch (_) { toast("Couldn't deliver — they may be offline", "var(--amber)"); } }
  });

  // ---- top-bar actions ----
  el("#topbar").addEventListener("click", (e) => {
    if (e.target.closest("#actCall")) initiateCall(false);
    else if (e.target.closest("#actVideo")) initiateCall(true);
    else if (e.target.closest("#actScreen")) screenAction();
  });
  // Clicking "screen" means: I want to VIEW this contact's screen. Live → real viewer session
  // (their host runs its own local Allow/Deny — Inv 1 — before any pixels cross). Demo → the
  // owner-side consent preview, so the design still demonstrates without a backend.
  function screenAction() {
    const c = byId(active); if (!c) return;
    if (LIVE) startViewerSession(c);
    else openConsent();
  }

  // ---- call flow (ADR-103/104) ----
  // LIVE: drives the real Tauri call commands (call_place/accept/decline/hangup/set_mute) and lets the
  // `call-lifecycle` event stream (content-free — Inv 8) drive UI transitions. If those commands aren't
  // present in this build yet (the on-device driver is a follow-up), the attempt is caught and the user
  // gets an honest toast rather than a fake call. Demo mode (browser/artifact) keeps the design preview.
  const callwin = el("#callwin"), scrim = el("#scrim"), incall = el("#incall"), pip = el("#pip");
  let timerInt, secs = 0, callVideo = false;
  const callInvoke = (cmd, args) => (LIVE ? invoke(cmd, args) : Promise.reject(new Error("demo")));

  function showIncoming(name, init, video) {
    el("#cwname").textContent = name; el("#cwav").textContent = init;
    callwin.querySelector(".csub b").textContent = video ? "Incoming video call" : "Incoming voice call";
    callwin.classList.add("show"); scrim.classList.add("show");
  }
  function showInCall(name, label) {
    callwin.classList.remove("show"); scrim.classList.remove("show"); pip.classList.remove("show");
    el("#rmtname").textContent = name || ""; incall.classList.add("show"); inCall = true;
    clearInterval(timerInt);
    if (label) { el("#ctimer").textContent = label; } // e.g. "Calling…" / "Connecting…"
    else { secs = 0; tick(); timerInt = setInterval(tick, 1000); }
  }
  function tick() { el("#ctimer").textContent = String(Math.floor(secs / 60)).padStart(2, "0") + ":" + String(secs % 60).padStart(2, "0"); secs++; }
  function endCall(msg) {
    incall.classList.remove("show"); pip.classList.remove("show"); callwin.classList.remove("show");
    inCall = false; clearInterval(timerInt);
    if (msg) toast(msg, "var(--ink-faint)");
  }
  function minCall() { incall.classList.remove("show"); pip.classList.add("show"); el("#pipname").textContent = callWith ? callWith.name : ""; toast("Call minimized — still connected"); }

  // Outbound: place a call to the active contact.
  function initiateCall(video) {
    callWith = byId(active); if (!callWith) return;
    callVideo = video;
    if (!LIVE) { showIncoming(callWith.name, callWith.init, video); return; } // demo: preview the ring
    showInCall(callWith.name, "Calling…");
    callInvoke("call_place", { contactId: active, video }).catch(() => {
      endCall();
      toast("Voice/video calling isn't available in this build yet", "var(--amber)");
    });
  }
  function acceptCall(video) {
    if (!LIVE) { showInCall(callWith ? callWith.name : "", null); toast("Connected · " + (callWith ? callWith.name : "")); return; }
    callVideo = video;
    callInvoke("call_accept", { video }).catch(() => {}); // UI advances on the `active` lifecycle event
  }
  // Mute buttons carry the `on` class while the source is ACTIVE (unmuted); muted = not on.
  function pushMute() {
    const audioMuted = !el("#btnMute").classList.contains("on");
    const videoMuted = !el("#btnCam").classList.contains("on");
    callInvoke("call_set_mute", { audioMuted, videoMuted }).catch(() => {});
  }

  el("#btnAcceptA").onclick = () => acceptCall(false);
  el("#btnAcceptV").onclick = () => acceptCall(true);
  el("#btnDecline").onclick = () => { callInvoke("call_decline").catch(() => {}); callwin.classList.remove("show"); scrim.classList.remove("show"); toast("Declined", "var(--ink-faint)"); };
  el("#btnHang").onclick = () => { callInvoke("call_hangup").catch(() => {}); if (!LIVE) endCall("Call ended"); };
  el("#btnPipEnd").onclick = () => { callInvoke("call_hangup").catch(() => {}); if (!LIVE) endCall("Call ended"); };
  el("#btnMinCall").onclick = minCall; el("#btnExpand").onclick = () => { pip.classList.remove("show"); incall.classList.add("show"); };
  el("#btnMute").onclick = (e) => { e.currentTarget.classList.toggle("on"); pushMute(); };
  el("#btnCam").onclick = (e) => { e.currentTarget.classList.toggle("on"); pushMute(); };

  // ---- consent -> session flow ----
  const consent = el("#consent"), session = el("#session"), ownerbar = el("#ownerbar");
  const deskdemo = el("#deskdemo"), viewCanvas = el("#viewCanvas"), sessHud = el("#sessHud"), sessFatal = el("#sessFatal");

  // The real WebCodecs viewer, folded in from the proven app path (viewer.js). Lazily created and
  // bound to the session canvas. Content-free status only (Inv 8). ON-DEVICE VERIFICATION PENDING:
  // the connect + decode path is a faithful port of working code but needs a two-machine run.
  let viewer = null, viewerLive = false, controller = null, audioPlayer = null, sessContactName = "";
  const takeBtn = () => el("#btnTakeControl");
  const audioBtn = () => el("#btnAudio");
  const sessBadge = () => session.querySelector(".ctrl-badge");

  // Reflect the shared-audio player state (audio.js) onto the toolbar button. Output-only, live-only
  // (Inv 12); an "AUDIO SHARED" affordance is honest disclosure (Inv 7). Hidden until a packet arrives.
  function reflectAudio(s) {
    const b = audioBtn();
    if (!b) return;
    b.hidden = !s.started;
    b.classList.toggle("playing", s.started && !s.muted && !s.needsGesture);
    b.classList.toggle("muted", s.muted);
    b.classList.toggle("needs-gesture", s.needsGesture);
    const lbl = b.querySelector(".audio-label");
    if (lbl) lbl.textContent = s.needsGesture ? "Enable audio" : s.muted ? "Muted" : "Audio";
    b.setAttribute("aria-pressed", s.muted ? "false" : "true");
  }

  // Reflect the take-control FSM (control.js) into the toolbar button + session badge (Inv 7: the
  // controlling state is always legible; --session chrome marks live OS control).
  function reflectControl(state, controlling) {
    const b = takeBtn();
    if (b) {
      b.classList.remove("armed", "pending", "denied");
      b.disabled = false;
      if (state === "granted") { b.textContent = "Controlling — click to stop"; b.classList.add("armed"); }
      else if (state === "requesting") { b.textContent = "Requesting…"; b.classList.add("pending"); b.disabled = true; }
      else if (state === "denied") { b.textContent = "Request denied"; b.classList.add("denied"); b.disabled = true; }
      else if (state === "timeout") { b.textContent = "No response — timed out"; b.classList.add("pending"); b.disabled = true; }
      else b.textContent = "Take control";
    }
    const badge = sessBadge();
    if (badge && badge.lastChild) badge.lastChild.textContent = (controlling ? "Controlling " : "Viewing ") + (sessContactName || "");
    if (state === "granted") toast("Control granted — you're driving " + sessContactName, "var(--session)");
    else if (state === "denied") toast("Control request denied", "var(--danger)");
    else if (state === "timeout") toast("No response to control request", "var(--amber)");
  }

  function ensureViewer() {
    if (viewer) return viewer;
    if (!LIVE || !window.RASViewer || !T.core || !T.core.Channel) return null;
    // one-time fatal banner scaffold (Inv 8: message is engine capability text, never stream content)
    sessFatal.innerHTML = "";
    const title = document.createElement("b"); title.textContent = "Can't show this screen";
    const msg = document.createElement("span"); msg.className = "fmsg";
    const close = document.createElement("button"); close.className = "btn btn-ghost"; close.textContent = "Close"; close.onclick = endSession;
    sessFatal.append(title, msg, close);
    // shared-audio player (host→controller Opus). Fed by the viewer's audio Channel below.
    if (window.RASAudio) {
      audioPlayer = window.RASAudio.createAudioPlayer({
        onState: reflectAudio,
        onUnsupported: (m) => toast(m, "var(--amber)"),
      });
      const ab = audioBtn();
      if (ab) ab.onclick = () => { if (audioPlayer) audioPlayer.toggle(); };
    }
    viewer = window.RASViewer.createViewer({
      canvas: viewCanvas,
      invoke: T.core.invoke,
      Channel: T.core.Channel,
      onStatus: (s) => { sessHud.hidden = false; sessHud.textContent = s; },
      onFatal: (m) => { sessFatal.hidden = false; sessFatal.querySelector(".fmsg").textContent = m; },
      onLive: (up) => { viewerLive = up; if (!up && controller) controller.reset(); },
      onAudio: (msg) => { if (audioPlayer) audioPlayer.handle(msg); },
    });
    if (window.RASControl) {
      controller = window.RASControl.createController({
        canvas: viewCanvas,
        invoke: T.core.invoke,
        isLive: () => viewerLive,
        onState: reflectControl,
      });
      const b = takeBtn();
      if (b) b.onclick = () => { if (controller) controller.requestOrToggle(); };
    }
    return viewer;
  }

  // LIVE: connect to a contact and render their screen into the session canvas.
  async function startViewerSession(c) {
    const v = ensureViewer();
    if (!v) { toast("Viewer unavailable in this build", "var(--danger)"); return; }
    sessContactName = c.name;
    if (controller) controller.reset(); // fresh view-only start; reflectControl sets the badge to "Viewing X"
    else if (sessBadge() && sessBadge().lastChild) sessBadge().lastChild.textContent = "Viewing " + c.name;
    sessFatal.hidden = true; sessHud.hidden = false; sessHud.textContent = "requesting " + c.name + "'s screen…";
    deskdemo.hidden = true; viewCanvas.hidden = false;
    session.classList.add("show"); inSession = true;
    toast("Requesting " + c.name + "'s screen — waiting for their consent", "var(--session)");
    await v.connectContact(c.id);   // false ⇒ onStatus already carries the reason; overlay stays so it's visible
  }

  // DEMO owner-side preview (no backend): the consent card → a faux session + un-hideable owner bar.
  function openConsent() { const c = byId(active); if (c) el("#reqname").textContent = c.name; consent.classList.add("show"); scrim.classList.add("show"); }
  el("#btnDeny").onclick = () => { consent.classList.remove("show"); scrim.classList.remove("show"); toast("Access denied", "var(--danger)"); };
  el("#btnAllow").onclick = () => { consent.classList.remove("show"); scrim.classList.remove("show"); startDemoSession(); };
  function startDemoSession() { deskdemo.hidden = false; viewCanvas.hidden = true; sessHud.hidden = true; sessFatal.hidden = true; session.classList.add("show"); ownerbar.classList.add("show"); inSession = true; toast("Session live · fresh grant issued", "var(--session)"); }

  async function endSession() {
    if (controller) controller.reset(); // release any held input + lease-follow before we drop (Inv 4)
    if (audioPlayer) audioPlayer.reset(); // tear down the AudioContext — no audio state lingers (Inv 12)
    if (viewer && viewerLive) { try { await viewer.disconnect(); } catch (_) {} }
    viewerLive = false;
    session.classList.remove("show"); ownerbar.classList.remove("show"); inSession = false;
    viewCanvas.hidden = true; deskdemo.hidden = false; sessHud.hidden = true; sessFatal.hidden = true;
    const ab = audioBtn(); if (ab) ab.hidden = true;
    resetSessionChat(); // wipe chat content + collapse (Inv 8 hygiene — no stale chat lingers)
    resetFileTransfer(); // cancel any in-flight send + close a pending offer
    toast("Session ended — control revoked", "var(--session)");
  }
  el("#btnSessStop").onclick = endSession; el("#btnOwnerStop").onclick = endSession;

  // ---- in-session chat + clipboard (send_chat / send_clipboard; chat-message / clipboard-received) ----
  // Usable only while a viewer session is live. Content is placed via textContent, never innerHTML and
  // never logged (Inv 8). Chat is unencrypted-to-the-log-but-Redacted-on-the-wire host-side already.
  const sessChat = el("#sessChat"), sessChatLog = el("#sessChatLog"), sessChatInput = el("#sessChatInput");
  const sessChatUnread = el("#sessChatUnread"), sessChatNotice = el("#sessChatNotice");
  let chatOpen = false, chatUnread = 0, chatNoticeT = null;
  const chatTime = () => new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

  function chatEmpty() { sessChatLog.innerHTML = ""; const p = document.createElement("div"); p.className = "sc-empty"; p.textContent = "No messages yet — say hi 👋"; sessChatLog.appendChild(p); }
  function appendChat(text, mine) {
    const e = sessChatLog.querySelector(".sc-empty"); if (e) e.remove();
    const stick = sessChatLog.scrollHeight - sessChatLog.scrollTop - sessChatLog.clientHeight < 24;
    const b = document.createElement("div"); b.className = "sc-msg " + (mine ? "me" : "them");
    const t = document.createElement("span"); t.className = "sc-text"; t.textContent = text; b.appendChild(t); // Inv 8: textContent
    const tm = document.createElement("span"); tm.className = "sc-time"; tm.textContent = chatTime(); b.appendChild(tm);
    sessChatLog.appendChild(b);
    if (mine || stick) sessChatLog.scrollTop = sessChatLog.scrollHeight;
    if (!mine && !chatOpen) { chatUnread++; sessChatUnread.textContent = chatUnread > 99 ? "99+" : String(chatUnread); sessChatUnread.hidden = false; }
  }
  function openChat() { chatOpen = true; sessChat.hidden = false; chatUnread = 0; sessChatUnread.hidden = true; setTimeout(() => sessChatInput.focus(), 40); sessChatLog.scrollTop = sessChatLog.scrollHeight; }
  function closeChat() { chatOpen = false; sessChat.hidden = true; }
  function chatNotice(msg, ok) {
    sessChatNotice.textContent = msg; sessChatNotice.classList.toggle("ok", !!ok); sessChatNotice.hidden = false;
    clearTimeout(chatNoticeT); chatNoticeT = setTimeout(() => { sessChatNotice.hidden = true; }, 3000);
  }
  function resetSessionChat() { closeChat(); chatUnread = 0; sessChatUnread.hidden = true; sessChatInput.value = ""; sessChatNotice.hidden = true; chatEmpty(); }
  async function sendSessChat() {
    if (!viewerLive) return;
    const text = sessChatInput.value.trim(); if (!text) return;
    sessChatInput.value = ""; appendChat(text, true);
    if (LIVE) { try { await invoke("send_chat", { text }); } catch (_) { chatNotice("Couldn't send — no active session."); } }
  }
  el("#btnSessChat").onclick = () => { if (chatOpen) closeChat(); else openChat(); };
  el("#btnSessChatClose").onclick = closeChat;
  el("#sessChatForm").addEventListener("submit", (e) => { e.preventDefault(); sendSessChat(); });
  sessChatInput.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); sendSessChat(); } });

  // Push local clipboard text to the peer (host-side re-gates on the clipboard capability, Inv 15).
  async function sendClipboard() {
    if (!viewerLive) { toast("Start a session first", "var(--amber)"); return; }
    let text = "";
    try { text = await navigator.clipboard.readText(); } catch (_) { toast("Clipboard access denied by the OS", "var(--amber)"); return; }
    if (!text) { toast("Clipboard is empty"); return; }
    if (!LIVE) return;
    try { await invoke("send_clipboard", { text }); toast("Clipboard sent · " + text.length + " chars", "var(--online)"); }
    catch (_) { toast("Couldn't send clipboard", "var(--danger)"); }
  }
  el("#btnSendClip").onclick = sendClipboard;
  chatEmpty();

  // ---- file transfer (sender: file_begin→file_chunk→file_end; receiver: file-offer→respond_file_offer) ----
  // The host resolves the destination from a leaf filename into its sandbox (Downloads); the controller
  // never sends a path (Inv 6). Content is never logged (Inv 8). Sender is gated on a live viewer session.
  const CHUNK = 256 * 1024;               // 256 KiB per file_chunk
  const ACCEPT_TIMEOUT_MS = 95000;         // mirrors the host's file-offer consent window (+slack)
  const OFFER_TIMEOUT_MS = 60000;          // receiver auto-deny
  // Host's stable rejection codes (ErrorCode Debug strings) → honest text. Content-free (enum tags only).
  const REJECT_REASONS = {
    ConsentDenied: "The other side declined.",
    CapabilityDenied: "File transfer is not authorized.",
    InvalidMessage: "The file couldn't be accepted (unsafe name, wrong target, or too large).",
    SessionRevoked: "The session was stopped.", LeaseInvalid: "The session was stopped.",
    Internal: "The other side couldn't receive the file.",
  };
  const rejectReasonText = (code) => (code && REJECT_REASONS[code]) || "The transfer was rejected.";
  const fmtSize = (n) => n < 1024 ? n + " B" : n < 1048576 ? (n / 1024).toFixed(1) + " KB" : (n / 1048576).toFixed(1) + " MB";

  const filePicker = el("#filePicker"), fileSend = el("#fileSend");
  const fsName = el("#fileSendName"), fsPct = el("#fileSendPct"), fsFill = el("#fileSendFill"), fsState = el("#fileSendState"), fsCancel = el("#fileSendCancel");
  let sending = false, cancelled = false, acceptResolve = null, acceptReject = null;

  function fsSet(state, cls) { fsState.textContent = state; fsState.className = "fs-state" + (cls ? " " + cls : ""); }
  function fsProgress(done, total) { const p = total ? Math.floor((done / total) * 100) : 0; fsFill.style.width = p + "%"; fsPct.textContent = p + "%"; }
  function showCard(name) { fsName.textContent = name; fsName.title = name; fsFill.className = "fs-fill"; fsFill.style.width = "0%"; fsPct.textContent = ""; fsCancel.disabled = false; fileSend.hidden = false; }
  function hideCardLater(ms) { setTimeout(() => { if (!sending) fileSend.hidden = true; }, ms); }
  function waitForAccept() {
    return new Promise((resolve, reject) => {
      acceptResolve = resolve; acceptReject = reject;
      setTimeout(() => { if (acceptReject) settleAccept(false, "timeout"); }, ACCEPT_TIMEOUT_MS);
    });
  }
  function settleAccept(ok, reason) { const res = acceptResolve, rej = acceptReject; acceptResolve = acceptReject = null; if (ok && res) res(); else if (!ok && rej) rej(reason || "declined"); }

  async function sendFile(file) {
    if (sending || !viewerLive || !LIVE) return;
    sending = true; cancelled = false;
    fsCancel.disabled = false; showCard(file.name); fsSet("Waiting for the other side to accept…", "receiving");
    try { await invoke("file_begin", { filename: file.name, size: file.size }); }
    catch (_) { return failTransfer("Couldn't start the transfer."); }
    try { await waitForAccept(); }
    catch (reason) { if (reason === "timeout") return failTransfer("No response — the transfer timed out."); if (reason === "cancelled") return finishCancel(); return declineTransfer(reason); }
    if (cancelled) return finishCancel();
    fsSet("Sending…", "receiving"); fsProgress(0, file.size);
    let off = 0;
    try {
      while (off < file.size) {
        if (cancelled) return finishCancel();
        const end = Math.min(off + CHUNK, file.size);
        const buf = await file.slice(off, end).arrayBuffer();
        if (cancelled) return finishCancel();
        await invoke("file_chunk", { bytes: Array.from(new Uint8Array(buf)) });
        off = end; fsProgress(off, file.size);
      }
      await invoke("file_end");
    } catch (_) { return failTransfer("Transfer failed — the connection may have dropped."); }
    fsFill.className = "fs-fill ok"; fsSet("Sent ✓", "ok"); fsPct.textContent = "100%"; fsCancel.disabled = true;
    sending = false; hideCardLater(2600);
  }
  function failTransfer(msg) { try { invoke("file_end"); } catch (_) {} fsFill.className = "fs-fill err"; fsSet(msg, "err"); fsCancel.disabled = true; sending = false; hideCardLater(4000); }
  function declineTransfer(code) { fsFill.className = "fs-fill err"; fsSet(rejectReasonText(code), "err"); fsCancel.disabled = true; sending = false; hideCardLater(3200); }
  function finishCancel() { try { invoke("file_end"); } catch (_) {} fsFill.className = "fs-fill err"; fsSet("Canceled.", "err"); fsCancel.disabled = true; sending = false; hideCardLater(2200); }
  function resetFileTransfer() {
    if (sending) { cancelled = true; settleAccept(false, "cancelled"); }
    sending = false; fileSend.hidden = true;
    clearOfferTimers(); const fo = el("#fileOffer"); if (fo) fo.hidden = true;
  }
  function pickFile() { if (!viewerLive) { toast("Start a session first", "var(--amber)"); return; } if (sending) return; filePicker.value = ""; filePicker.click(); }
  el("#btnSendFile").onclick = pickFile;
  filePicker.addEventListener("change", () => { const f = filePicker.files && filePicker.files[0]; if (f) sendFile(f); });
  fsCancel.onclick = () => { if (!sending) { fileSend.hidden = true; return; } cancelled = true; settleAccept(false, "cancelled"); };

  // Receiver: an incoming offer → local Allow/Deny (Inv 1). Deny is the safe default (Esc / timeout).
  const fileOffer = el("#fileOffer"), foName = el("#fileOfferName"), foSize = el("#fileOfferSize"), foTimeout = el("#fileOfferTimeout");
  let offerTimer = null, offerCountdown = null;
  function clearOfferTimers() { if (offerTimer) { clearTimeout(offerTimer); offerTimer = null; } if (offerCountdown) { clearInterval(offerCountdown); offerCountdown = null; } }
  function respondOffer(accept) { clearOfferTimers(); fileOffer.hidden = true; if (LIVE) { try { invoke("respond_file_offer", { accept }); } catch (_) {} } }
  function openOffer(filename, size) {
    clearOfferTimers();
    foName.textContent = filename; foName.title = filename; foSize.textContent = "· " + fmtSize(size);
    fileOffer.hidden = false; setTimeout(() => el("#fileOfferDeny").focus(), 60);
    let left = Math.round(OFFER_TIMEOUT_MS / 1000);
    foTimeout.textContent = "Auto-declines in " + left + "s if no response.";
    offerCountdown = setInterval(() => { left -= 1; foTimeout.textContent = left > 0 ? "Auto-declines in " + left + "s if no response." : "Declining…"; }, 1000);
    offerTimer = setTimeout(() => respondOffer(false), OFFER_TIMEOUT_MS);
  }
  el("#fileOfferAccept").onclick = () => respondOffer(true);
  el("#fileOfferDeny").onclick = () => respondOffer(false);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && !fileOffer.hidden) respondOffer(false); });

  function closeTransient() { callwin.classList.remove("show"); consent.classList.remove("show"); scrim.classList.remove("show"); }
  scrim.onclick = closeTransient;
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { closeTransient(); if (e.shiftKey) { if (inCall) endCall(); if (inSession) endSession(); } }
  });

  // ---- live events (real mode only) ----
  if (LIVE && listen) {
    listen("presence", (e) => {
      const p = e.payload || {}; const c = byId(p.contactId);
      if (c && !c.blocked) { c.pres = p.online ? "online" : "offline"; renderList(); if (active === c.id) renderMain(); }
    });
    listen("message", (e) => {
      const m = e.payload || {}; const c = byId(m.contact_id); if (!c) return;
      c.msgs.push({ who: "them", time: now(), text: m.text }); // text is already .reveal()'d host-side; render via textContent
      c.prev = c.name.split(" ")[0] + ": " + m.text; c.time = now();
      if (active !== c.id) c.unread = (c.unread || 0) + 1;
      renderList(); if (active === c.id) renderMain();
    });
    listen("call-request", (e) => { const m = e.payload || {}; const c = byId(m.contact_id); toast((c ? c.name : "A contact") + " wants to view your screen", "var(--session)"); });
    // Content-free 1:1-call lifecycle (ADR-104) — drives the ring / in-call / end UI. Never carries a
    // media byte (Inv 8); an incoming ring is surfaced but never auto-answered (Inv 1 — the local user
    // taps Accept). `kind` ∈ outgoing_ringing/incoming_ringing/connecting/active/ended/declined/missed/
    // failed/remote_mute_changed; optional `contactId`, `media` ("voice"|"video").
    listen("call-lifecycle", (e) => {
      const ev = e.payload || {}, kind = ev.kind;
      const c = ev.contactId ? byId(ev.contactId) : callWith;
      if (c) callWith = c;
      const name = c ? c.name : (callWith ? callWith.name : "Contact");
      const init = c ? c.init : (callWith ? callWith.init : "?");
      if (kind === "incoming_ringing") { callVideo = ev.media === "video"; showIncoming(name, init, callVideo); }
      else if (kind === "connecting") { showInCall(name, "Connecting…"); }
      else if (kind === "active") { showInCall(name, null); toast("Connected · " + name, "var(--online)"); }
      else if (kind === "ended") { endCall("Call ended"); }
      else if (kind === "declined") { endCall("Call declined"); }
      else if (kind === "missed") { endCall("No answer"); }
      else if (kind === "failed") { endCall("Call failed"); }
      // outgoing_ringing keeps the "Calling…" state already shown; remote_mute_changed is presentation-only.
    });
    // Received call audio: each event carries one self-describing RAU1 Opus blob; decode + play via the
    // shared WebCodecs audio player (audio.js). Content-free to the log (Inv 8 — only bytes are handled).
    let callAudio = null, callVid = null;
    listen("call-audio", (e) => {
      if (!callAudio && window.RASAudio) callAudio = window.RASAudio.createAudioPlayer({ onState: () => {}, onUnsupported: () => {} });
      if (callAudio) callAudio.handle(e.payload);
    });
    listen("call-audio-inactive", () => { if (callAudio) { callAudio.reset(); callAudio = null; } });
    // Received call video: each event carries one RCFG/RAS1 blob; decode + render into the in-call
    // canvas via the shared WebCodecs viewer (no connect — blobs are pushed from Rust). Inv 8: bytes only.
    const callCanvas = el("#callVideo");
    listen("call-video", (e) => {
      if (!callVid && window.RASViewer && callCanvas) {
        callCanvas.hidden = false;
        callVid = window.RASViewer.createViewer({
          canvas: callCanvas,
          invoke: () => Promise.resolve(), // no request_keyframe in a call — no-op
          Channel: T.core && T.core.Channel,
          onStatus: () => {}, onFatal: () => {}, onLive: () => {},
        });
      }
      if (callVid) callVid.feed(e.payload);
    });
    // Local self-view: our own camera frames looped back from Rust (same encoded VP9 as we send the
    // peer — the camera is opened once). Decode + render into the small self-view canvas. Inv 8: bytes only.
    const selfCanvas = el("#selfView");
    let selfVid = null;
    listen("call-selfvideo", (e) => {
      if (!selfVid && window.RASViewer && selfCanvas) {
        selfCanvas.hidden = false;
        selfVid = window.RASViewer.createViewer({
          canvas: selfCanvas,
          invoke: () => Promise.resolve(),
          Channel: T.core && T.core.Channel,
          onStatus: () => {}, onFatal: () => {}, onLive: () => {},
        });
      }
      if (selfVid) selfVid.feed(e.payload);
    });
    const stopCallVideo = () => {
      if (callVid) { callVid.resetSink(); callVid = null; } if (callCanvas) callCanvas.hidden = true;
      if (selfVid) { selfVid.resetSink(); selfVid = null; } if (selfCanvas) selfCanvas.hidden = true;
    };
    listen("call-audio-inactive", stopCallVideo);
    // Host denied a control request, or revoked an active lease mid-session (Inv 4). Content-free.
    listen("control-consent-denied", () => { if (controller) controller.notifyConsentDenied(); });
    // In-session chat from the remote peer (text is .reveal()'d host-side; render via textContent, Inv 8).
    listen("chat-message", (e) => { const text = typeof e.payload === "string" ? e.payload : String(e.payload ?? ""); if (text) appendChat(text, false); });
    // Peer pushed us clipboard text — content-free byte count only (Inv 8).
    listen("clipboard-received", (e) => { const n = Number(e.payload) || 0; chatNotice("Received clipboard · " + n + " bytes", true); if (!chatOpen) openChat(); });
    // File transfer (sender side): the host consented / refused our offer.
    listen("file-accepted", () => settleAccept(true));
    listen("file-rejected", (e) => settleAccept(false, e.payload || "Rejected"));
    // File transfer (receiver side): an incoming offer, and a completed drop.
    listen("file-offer", (e) => { const p = e.payload || {}; openOffer(typeof p.filename === "string" ? p.filename : "file", Number(p.size) || 0); });
    listen("file-offer-closed", () => { clearOfferTimers(); fileOffer.hidden = true; });
    listen("file-received", (e) => { const p = e.payload || {}; const fn = typeof p.filename === "string" ? p.filename : "file"; toast("Received " + fn + " · " + fmtSize(Number(p.size) || 0) + " → Downloads", "var(--online)"); });
  }

  // ---- add-contact modal (real: my_identity + add_contact) ----
  const addModal = el("#addModal"), addInput = el("#addInput"), addLabel = el("#addLabel"), addErr = el("#addErr");
  function openAdd() {
    addInput.value = ""; addLabel.value = ""; addErr.hidden = true;
    addModal.classList.add("show"); scrim.classList.add("show");
    addInput.focus();
    if (LIVE) {
      invoke("my_identity").then((me) => { const code = el("#myCode"); code.textContent = me.code; code.title = me.code; el("#btnCopyCode").dataset.code = me.ticket || me.code; })
        .catch(() => { el("#myCode").textContent = "unavailable"; });
    } else { el("#myCode").textContent = "CRAS-K7QP-4M2X-9RJT-8W6D (demo)"; }
  }
  function closeAdd() { addModal.classList.remove("show"); scrim.classList.remove("show"); }
  el("#btnAddContact").onclick = openAdd;
  el("#btnAddClose").onclick = closeAdd; el("#btnAddCancel").onclick = closeAdd;
  el("#btnCopyCode").onclick = (e) => {
    const v = e.currentTarget.dataset.code || el("#myCode").textContent;
    if (navigator.clipboard) navigator.clipboard.writeText(v).then(() => toast("Code copied"));
  };
  el("#btnAddSave").onclick = async () => {
    const input = addInput.value.trim(); if (!input) { addErr.textContent = "Paste their invite code first."; addErr.hidden = false; return; }
    if (!LIVE) { toast("Add works in the app (demo mode here)", "var(--amber)"); closeAdd(); return; }
    try {
      await invoke("add_contact", { input, label: addLabel.value.trim() });
      closeAdd(); await loadContacts(); renderList(); renderMain(); toast("Contact added");
    } catch (err) { addErr.textContent = String(err).replace(/^Error:\s*/, ""); addErr.hidden = false; }
  };

  // ---- contact details drawer (real: block / remove) ----
  const drawer = el("#drawer");
  function openDrawer() {
    const c = byId(active); if (!c) return;
    const body = el("#drawerBody"); body.innerHTML = "";
    const av = document.createElement("div"); av.style.cssText = "display:flex;justify-content:center;margin-bottom:12px";
    av.innerHTML = `<span class="avatar s72" style="background:${c.color}">${c.init}${presIcon(c.pres)}</span>`;
    body.appendChild(av);
    const nm = document.createElement("div"); nm.className = "dname"; nm.textContent = c.name; body.appendChild(nm);
    const pr = document.createElement("div"); pr.className = "dpres"; pr.textContent = statusText(c); body.appendChild(pr);
    if (c.code) { const cd = document.createElement("div"); cd.className = "dcode"; cd.textContent = c.code; body.appendChild(cd); }
    const acts = document.createElement("div"); acts.className = "drawer-act";
    const blockBtn = document.createElement("button"); blockBtn.className = "btn btn-ghost";
    blockBtn.textContent = c.blocked ? "Unblock" : "Block";
    blockBtn.onclick = async () => {
      if (!LIVE) { toast("Works in the app", "var(--amber)"); return; }
      try { await invoke("set_contact_blocked", { id: c.id, blocked: !c.blocked }); await loadContacts(); renderList(); renderMain(); closeDrawer(); toast(c.blocked ? "Unblocked" : "Blocked " + c.name); }
      catch (err) { toast(String(err), "var(--danger)"); }
    };
    const rmBtn = document.createElement("button"); rmBtn.className = "btn btn-danger"; rmBtn.textContent = "Remove contact";
    rmBtn.onclick = async () => {
      if (!LIVE) { toast("Works in the app", "var(--amber)"); return; }
      try { await invoke("remove_contact", { id: c.id }); const wasActive = active === c.id; await loadContacts(); if (wasActive) active = contacts[0] ? contacts[0].id : null; renderList(); renderMain(); closeDrawer(); toast("Contact removed"); }
      catch (err) { toast(String(err), "var(--danger)"); }
    };
    acts.appendChild(blockBtn); acts.appendChild(rmBtn); body.appendChild(acts);
    drawer.classList.add("show");
  }
  function closeDrawer() { drawer.classList.remove("show"); }
  el("#btnDrawerClose").onclick = closeDrawer;

  // hook the info action (delegated on topbar) to open the drawer
  el("#topbar").addEventListener("click", (e) => { if (e.target.closest("#actInfo")) openDrawer(); });

  // close the add modal on scrim / Esc too (additive — never touches the un-hideable owner bar)
  scrim.addEventListener("click", closeAdd);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") { closeAdd(); closeDrawer(); } });
  addInput && addInput.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); el("#btnAddSave").click(); } });

  // ---- boot ----
  (async function boot() {
    await loadContacts();
    renderList(); renderMain();
    if (LIVE) {
      // light presence refresh so dots stay fresh even if an event is missed
      setInterval(async () => {
        try {
          const online = new Set(await invoke("list_online"));
          contacts.forEach((c) => { if (!c.blocked) c.pres = online.has(c.id) ? "online" : "offline"; });
          renderList(); if (active) renderMain();
        } catch (_) {}
      }, 15000);
    }
  })();
})();
