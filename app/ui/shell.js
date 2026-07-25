/* Casual RAS — contacts-first desktop shell (Phase 1).
 *
 * Wired to the LIVE backend for the parts that exist today — contacts, presence, and out-of-session
 * messaging — via the real Tauri commands (`list_contacts` / `list_online` / `send_message` /
 * `add_contact` / `set_contact_blocked` / `call_contact`) and the `presence` / `message` / `call-request`
 * events. The call and remote-session flows are the DESIGNED surfaces running as interactive previews;
 * they get wired to their real backends as ADR-103/104/105 land and the existing session machinery is
 * folded in. Secret hygiene (Inv 8): message bodies are rendered with `textContent`, never innerHTML.
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
    if (e.target.closest("#actCall")) startIncoming(false);
    else if (e.target.closest("#actVideo")) startIncoming(true);
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

  // ---- call flow (designed preview; real voice/video calling is ADR-103/104) ----
  const callwin = el("#callwin"), scrim = el("#scrim"), incall = el("#incall"), pip = el("#pip");
  let timerInt, secs = 0;
  function startIncoming(video) {
    callWith = byId(active); if (!callWith) return;
    el("#cwname").textContent = callWith.name; el("#cwav").textContent = callWith.init;
    callwin.querySelector(".csub b").textContent = video ? "Incoming video call" : "Incoming voice call";
    callwin.classList.add("show"); scrim.classList.add("show");
  }
  function connectCall() {
    callwin.classList.remove("show"); scrim.classList.remove("show"); pip.classList.remove("show");
    el("#rmtname").textContent = callWith.name; incall.classList.add("show"); inCall = true;
    secs = 0; tick(); clearInterval(timerInt); timerInt = setInterval(tick, 1000); toast("Connected · " + callWith.name);
  }
  function tick() { el("#ctimer").textContent = String(Math.floor(secs / 60)).padStart(2, "0") + ":" + String(secs % 60).padStart(2, "0"); secs++; }
  function endCall() { incall.classList.remove("show"); pip.classList.remove("show"); inCall = false; clearInterval(timerInt); toast("Call ended", "var(--ink-faint)"); }
  function minCall() { incall.classList.remove("show"); pip.classList.add("show"); el("#pipname").textContent = callWith.name; toast("Call minimized — still connected"); }
  el("#btnAcceptA").onclick = connectCall; el("#btnAcceptV").onclick = connectCall;
  el("#btnDecline").onclick = () => { callwin.classList.remove("show"); scrim.classList.remove("show"); toast("Declined", "var(--ink-faint)"); };
  el("#btnHang").onclick = endCall; el("#btnPipEnd").onclick = endCall;
  el("#btnMinCall").onclick = minCall; el("#btnExpand").onclick = () => { pip.classList.remove("show"); incall.classList.add("show"); };
  el("#btnMute").onclick = (e) => e.currentTarget.classList.toggle("on");
  el("#btnCam").onclick = (e) => e.currentTarget.classList.toggle("on");

  // ---- consent -> session flow ----
  const consent = el("#consent"), session = el("#session"), ownerbar = el("#ownerbar");
  const deskdemo = el("#deskdemo"), viewCanvas = el("#viewCanvas"), sessHud = el("#sessHud"), sessFatal = el("#sessFatal");

  // The real WebCodecs viewer, folded in from the proven app path (viewer.js). Lazily created and
  // bound to the session canvas. Content-free status only (Inv 8). ON-DEVICE VERIFICATION PENDING:
  // the connect + decode path is a faithful port of working code but needs a two-machine run.
  let viewer = null, viewerLive = false, controller = null, sessContactName = "";
  const takeBtn = () => el("#btnTakeControl");
  const sessBadge = () => session.querySelector(".ctrl-badge");

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
    viewer = window.RASViewer.createViewer({
      canvas: viewCanvas,
      invoke: T.core.invoke,
      Channel: T.core.Channel,
      onStatus: (s) => { sessHud.hidden = false; sessHud.textContent = s; },
      onFatal: (m) => { sessFatal.hidden = false; sessFatal.querySelector(".fmsg").textContent = m; },
      onLive: (up) => { viewerLive = up; if (!up && controller) controller.reset(); },
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
    if (viewer && viewerLive) { try { await viewer.disconnect(); } catch (_) {} }
    viewerLive = false;
    session.classList.remove("show"); ownerbar.classList.remove("show"); inSession = false;
    viewCanvas.hidden = true; deskdemo.hidden = false; sessHud.hidden = true; sessFatal.hidden = true;
    toast("Session ended — control revoked", "var(--session)");
  }
  el("#btnSessStop").onclick = endSession; el("#btnOwnerStop").onclick = endSession;

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
    // Host denied a control request, or revoked an active lease mid-session (Inv 4). Content-free.
    listen("control-consent-denied", () => { if (controller) controller.notifyConsentDenied(); });
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
