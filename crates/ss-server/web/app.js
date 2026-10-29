/* surveillance-rs web UI — vanilla JS SPA with hash routing (SPEC §11).
 * Auth is the ss_session cookie set by POST /auth/login, so fetch and
 * EventSource/<img> requests authenticate without any token handling here. */
(function () {
  'use strict';

  const API = '/api/v1';
  const ROLE_RANK = { viewer: 0, manager: 1, admin: 2 };
  const EVENT_KINDS = ['motion', 'connection_lost', 'connection_restored', 'recording_started', 'recording_stopped', 'storage_full'];
  const KIND_LABEL = {
    motion: 'Motion', connection_lost: 'Connection lost', connection_restored: 'Connection restored',
    recording_started: 'Recording started', recording_stopped: 'Recording stopped', storage_full: 'Storage full',
  };
  const MODES = [
    { key: 'none', label: 'Off', cls: 'm-none' },
    { key: 'continuous', label: 'Continuous', cls: 'm-continuous' },
    { key: 'motion', label: 'Motion', cls: 'm-motion' },
    { key: 'continuous_and_motion', label: 'Continuous + motion', cls: 'm-both' },
  ];
  const DAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
  const SPEEDS = [0.25, 0.5, 1, 2, 4, 8, 16];
  const DAY_MS = 86400000;

  // ------------------------------------------------------------ state
  const state = {
    me: null,          // current user (null = logged out)
    cameras: [],       // last GET /cameras result (includes status)
    es: null,          // EventSource for /events/stream
    seq: 0,            // bumped on every route change; stale async renders check it
    timers: [],
    eventHook: null,   // view-specific SSE listener
    play: null,        // playback state
    prevMotion: {},    // camera id -> last motion_active (for tile flashes)
    lastMotionToast: {},
  };

  const store = {
    get(key, dflt) {
      try { const v = localStorage.getItem('ss.' + key); return v === null ? dflt : JSON.parse(v); } catch (_) { return dflt; }
    },
    set(key, value) {
      try { localStorage.setItem('ss.' + key, JSON.stringify(value)); } catch (_) { /* private mode */ }
    },
  };

  // ------------------------------------------------------------ helpers
  const $ = (sel, root = document) => root.querySelector(sel);
  const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

  function esc(value) {
    return String(value === undefined || value === null ? '' : value).replace(/[&<>"']/g, (c) => (
      { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
    ));
  }

  class ApiError extends Error {
    constructor(status, code, message) { super(message); this.status = status; this.code = code; }
  }

  async function api(method, path, body) {
    // X-Requested-With is required by the server for cookie-authenticated unsafe requests (CSRF guard).
    const opts = { method, credentials: 'same-origin', headers: { 'X-Requested-With': 'ss' } };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    let res;
    try {
      res = await fetch(API + path, opts);
    } catch (_) {
      throw new ApiError(0, 'network', 'Network error: the server is unreachable.');
    }
    const text = await res.text();
    let data = null;
    if (text) { try { data = JSON.parse(text); } catch (_) { data = text; } }
    if (!res.ok) {
      if (res.status === 401 && path !== '/auth/login') sessionExpired();
      const message = data && typeof data === 'object' && data.message ? data.message : (typeof data === 'string' && data) || res.statusText;
      throw new ApiError(res.status, data && data.error, message);
    }
    return data;
  }

  function canAs(role) {
    return !!state.me && ROLE_RANK[state.me.role] >= ROLE_RANK[role];
  }

  function toast(message, kind = 'info') {
    const box = $('#toasts');
    const t = document.createElement('div');
    t.className = 'toast ' + kind;
    t.textContent = message;
    t.title = 'Click to dismiss';
    t.addEventListener('click', () => t.remove());
    box.appendChild(t);
    while (box.children.length > 5) box.firstChild.remove();
    setTimeout(() => t.remove(), 6000);
  }

  function errorText(err) {
    if (err && err.status === 429) return 'Too many failed logins. Try again in a few minutes.';
    return (err && err.message) || 'Unexpected error';
  }

  function fmtDateTime(iso) {
    return iso ? new Date(iso).toLocaleString() : '–';
  }

  function fmtTime(iso) {
    return iso ? new Date(iso).toLocaleTimeString() : '–';
  }

  function fmtDuration(ms) {
    if (!isFinite(ms) || ms < 0) return '–';
    const s = Math.round(ms / 1000);
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
    return h ? `${h}h ${m}m` : m ? `${m}m ${sec}s` : `${sec}s`;
  }

  function fmtBytes(n) {
    if (n === null || n === undefined) return '–';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let v = Number(n), i = 0;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return `${v.toFixed(i > 1 ? 1 : 0)} ${units[i]}`;
  }

  function localDateStr(d = new Date()) {
    const p = (x) => String(x).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  }

  /** Start and end (Date) of a local calendar day given as YYYY-MM-DD. */
  function dayBounds(dateStr) {
    const [y, m, d] = dateStr.split('-').map(Number);
    const start = new Date(y, m - 1, d, 0, 0, 0, 0);
    const end = new Date(y, m - 1, d + 1, 0, 0, 0, 0);
    return { start, end, ms: end - start };
  }

  function stateClass(s) {
    return { online: 'ok', connecting: 'warn', offline: 'err', disabled: 'off' }[s] || 'off';
  }

  function stateLabel(s) {
    return { online: 'Online', connecting: 'Connecting', offline: 'Offline', disabled: 'Disabled' }[s] || s;
  }

  function every(ms, fn) {
    const seq = state.seq;
    const id = setInterval(() => { if (seq === state.seq) fn(); }, ms);
    state.timers.push(id);
    return id;
  }

  function resetView() {
    state.timers.forEach(clearInterval);
    state.timers = [];
    state.eventHook = null;
    state.play = null;
  }

  /** Render into #view only if this render is still the current route. */
  function put(ctx, html) {
    if (!ctx.alive()) return false;
    $('#view').innerHTML = html;
    return true;
  }

  async function loadCameras() {
    state.cameras = await api('GET', '/cameras');
    return state.cameras;
  }

  function cameraName(id) {
    const c = state.cameras.find((x) => x.id === id);
    return c ? c.name : (id ? `camera ${id}` : 'system');
  }

  // ------------------------------------------------------------ chrome & routing
  function setChrome(active) {
    const bar = $('#topbar');
    if (!state.me) { bar.classList.add('hidden'); return; }
    bar.classList.remove('hidden');
    const items = [
      ['live', 'Live', 'viewer'], ['playback', 'Playback', 'viewer'], ['events', 'Events', 'viewer'],
      ['cameras', 'Cameras', 'manager'], ['users', 'Users', 'admin'], ['settings', 'Settings', 'admin'],
      ['system', 'System', 'viewer'],
    ];
    $('#nav').innerHTML = items
      .filter(([, , role]) => canAs(role))
      .map(([key, label]) => `<a href="#/${key}" class="${key === active ? 'active' : ''}">${label}</a>`)
      .join('');
    $('#whoami').innerHTML = `${esc(state.me.username)} <span class="role">${esc(state.me.role)}</span>`;
  }

  function parseHash() {
    const raw = location.hash.replace(/^#/, '');
    const q = raw.indexOf('?');
    const pathPart = q >= 0 ? raw.slice(0, q) : raw;
    const params = Object.fromEntries(new URLSearchParams(q >= 0 ? raw.slice(q + 1) : ''));
    const parts = pathPart.split('/').filter(Boolean);
    return { parts, params };
  }

  const ROUTES = {
    live: { render: renderLive, role: 'viewer' },
    playback: { render: renderPlayback, role: 'viewer' },
    events: { render: renderEvents, role: 'viewer' },
    cameras: { render: renderCameras, role: 'manager' },
    users: { render: renderUsers, role: 'admin' },
    settings: { render: renderSettings, role: 'admin' },
    system: { render: renderSystem, role: 'viewer' },
  };

  async function route() {
    const seq = ++state.seq;
    resetView();
    if (!state.me) { renderLogin(); return; }
    const { parts, params } = parseHash();
    const name = Object.prototype.hasOwnProperty.call(ROUTES, parts[0]) ? parts[0] : 'live';
    const route = ROUTES[name];
    setChrome(name);
    if (!canAs(route.role)) {
      $('#view').innerHTML = `<div class="card"><h2>Not available</h2><p class="muted">This page needs the ${esc(route.role)} role.</p></div>`;
      return;
    }
    const ctx = { args: parts.slice(1), params, alive: () => seq === state.seq };
    try {
      await route.render(ctx);
    } catch (err) {
      if (!ctx.alive() || !state.me) return;
      $('#view').innerHTML = `<div class="card"><h2>Something went wrong</h2><p class="error">${esc(errorText(err))}</p></div>`;
    }
  }

  function sessionExpired() {
    if (!state.me) return;
    state.me = null;
    stopEvents();
    resetView();
    setChrome(null);
    renderLogin('Your session has expired. Please sign in again.');
  }

  async function logout() {
    state.me = null;
    try { await api('POST', '/auth/logout'); } catch (_) { /* already gone */ }
    stopEvents();
    resetView();
    setChrome(null);
    renderLogin();
  }

  // ------------------------------------------------------------ login
  function renderLogin(message = '') {
    setChrome(null);
    $('#view').innerHTML = `
      <section class="card login">
        <h1>surveillance-rs</h1>
        <p class="muted">Sign in to continue</p>
        <form id="login-form" autocomplete="on">
          <label>Username <input name="username" autocomplete="username" required autofocus></label>
          <label>Password <input name="password" type="password" autocomplete="current-password" required></label>
          <div id="login-err" class="error">${esc(message)}</div>
          <button class="btn primary" type="submit">Sign in</button>
        </form>
      </section>`;
    $('#login-form').addEventListener('submit', async (e) => {
      e.preventDefault();
      const f = e.target;
      const btn = f.querySelector('button');
      btn.disabled = true;
      try {
        const res = await api('POST', '/auth/login', { username: f.username.value, password: f.password.value });
        state.me = res.user;
        startEvents();
        if (location.hash === '#/live') route(); else location.hash = '#/live';
      } catch (err) {
        $('#login-err').textContent = errorText(err);
        btn.disabled = false;
      }
    });
  }

  // ------------------------------------------------------------ live
  function tileHtml(c) {
    return `
      <div class="tile" data-cam="${c.id}" title="Open ${esc(c.name)}">
        <img alt="${esc(c.name)}" src="${API}/cameras/${c.id}/live">
        <div class="tile-top">
          <span class="dot ${stateClass(c.status.state)}"></span>
          <span class="tname">${esc(c.name)}</span>
          <span class="badge rec">REC</span>
          <span class="badge motion">MOTION</span>
        </div>
        <div class="tile-msg hidden"></div>
      </div>`;
  }

  function updateTiles() {
    for (const t of $$('.tile[data-cam]')) {
      const id = Number(t.dataset.cam);
      const c = state.cameras.find((x) => x.id === id);
      if (!c) continue;
      const st = c.status;
      t.querySelector('.dot').className = 'dot ' + stateClass(st.state);
      t.querySelector('.rec').classList.toggle('on', !!st.recording);
      t.querySelector('.motion').classList.toggle('on', !!st.motion_active);
      const msg = t.querySelector('.tile-msg');
      const text = st.state === 'online' ? '' : (st.state === 'disabled' ? 'Camera disabled' : (st.last_error || stateLabel(st.state)));
      msg.textContent = text;
      msg.classList.toggle('hidden', !text);
      if (st.motion_active && !state.prevMotion[id]) flashTile(id);
      state.prevMotion[id] = !!st.motion_active;
    }
  }

  function flashTile(id) {
    for (const t of $$(`.tile[data-cam="${id}"]`)) {
      t.classList.remove('flash');
      void t.offsetWidth; // restart the animation
      t.classList.add('flash');
    }
  }

  async function renderLive(ctx) {
    if (ctx.args[0]) return renderSingle(ctx, Number(ctx.args[0]));
    const cams = await loadCameras();
    if (!ctx.alive()) return;
    const layout = state.layout || store.get('layout', 4);
    const shown = cams.slice(0, layout);
    const buttons = [1, 4, 9, 16].map((n) =>
      `<button class="btn ${n === layout ? 'on' : ''}" type="button" data-layout="${n}">${n}</button>`).join('');
    const body = shown.length
      ? `<div class="grid" id="grid" style="--cols:${Math.ceil(Math.sqrt(layout))}">${shown.map(tileHtml).join('')}</div>`
      : `<div class="card"><p class="muted">No cameras yet.${canAs('manager') ? ' Add one under <a href="#/cameras">Cameras</a>.' : ''}</p></div>`;
    if (!put(ctx, `
      <div class="toolbar">
        <div class="seg">${buttons}</div>
        <span class="muted">${shown.length} of ${cams.length} cameras</span>
      </div>${body}`)) return;
    $$('[data-layout]').forEach((b) => b.addEventListener('click', () => {
      state.layout = Number(b.dataset.layout);
      store.set('layout', state.layout);
      route();
    }));
    const grid = $('#grid');
    if (grid) grid.addEventListener('click', (e) => {
      const tile = e.target.closest('.tile');
      if (tile) location.hash = `#/live/${tile.dataset.cam}`;
    });
    updateTiles();
    every(3000, async () => {
      try { await loadCameras(); updateTiles(); } catch (_) { /* transient */ }
    });
  }

  async function renderSingle(ctx, id) {
    const cam = await api('GET', `/cameras/${id}`);
    if (!ctx.alive()) return;
    const st = cam.status;
    const recControls = canAs('manager')
      ? `<button class="btn primary" id="rec-start" type="button">Start recording</button>
         <button class="btn" id="rec-stop" type="button">Stop recording</button>`
      : '';
    if (!put(ctx, `
      <div class="toolbar">
        <a class="btn" href="#/live">&larr; Grid</a>
        <h2 id="cam-title">${esc(cam.name)}</h2>
        <span class="dot ${stateClass(st.state)}" id="cam-dot"></span>
        <span class="badge rec" id="cam-rec">REC</span>
        <span class="badge motion" id="cam-motion">MOTION</span>
      </div>
      <div class="single"><img alt="${esc(cam.name)}" src="${API}/cameras/${id}/live"></div>
      <div class="row" style="margin-bottom:12px">
        <a class="btn" target="_blank" rel="noopener" href="${API}/cameras/${id}/snapshot">Snapshot</a>
        <a class="btn" href="#/playback?camera=${id}">Playback</a>
        ${recControls}
      </div>
      <div class="card" id="cam-info"></div>`)) return;

    const paint = (s) => {
      $('#cam-dot').className = 'dot ' + stateClass(s.state);
      $('#cam-rec').classList.toggle('on', !!s.recording);
      $('#cam-motion').classList.toggle('on', !!s.motion_active);
      $('#cam-info').innerHTML = `
        <dl class="kv">
          <dt>State</dt><dd>${esc(stateLabel(s.state))}</dd>
          <dt>Frame rate</dt><dd>${s.fps ? s.fps.toFixed(1) + ' fps' : '–'}</dd>
          <dt>Last frame</dt><dd>${esc(fmtDateTime(s.last_frame_at))}</dd>
          <dt>Recording</dt><dd>${s.recording ? esc(s.recording_trigger || 'yes') : 'no'}</dd>
          <dt>Last error</dt><dd class="${s.last_error ? 'error' : ''}">${esc(s.last_error || '–')}</dd>
        </dl>`;
    };
    paint(st);
    every(3000, async () => {
      try { const c = await api('GET', `/cameras/${id}`); paint(c.status); } catch (_) { /* transient */ }
    });
    const recAction = async (on) => {
      try {
        await api('POST', `/cameras/${id}/record/${on ? 'start' : 'stop'}`);
        toast(on ? 'Manual recording started' : 'Manual recording stopped', 'ok');
      } catch (err) { toast(errorText(err), 'err'); }
    };
    const startBtn = $('#rec-start');
    if (startBtn) startBtn.addEventListener('click', () => recAction(true));
    const stopBtn = $('#rec-stop');
    if (stopBtn) stopBtn.addEventListener('click', () => recAction(false));
  }

  // ------------------------------------------------------------ playback
  function timelinePct(bounds, iso) {
    const t = typeof iso === 'number' ? iso : Date.parse(iso);
    return Math.max(0, Math.min(100, ((t - bounds.start) / bounds.ms) * 100));
  }

  async function renderPlayback(ctx) {
    const cams = await loadCameras();
    if (!ctx.alive()) return;
    if (!cams.length) {
      put(ctx, '<div class="card"><p class="muted">No cameras yet.</p></div>');
      return;
    }
    const camId = Number(ctx.params.camera) || cams[0].id;
    const date = /^\d{4}-\d{2}-\d{2}$/.test(ctx.params.date || '') ? ctx.params.date : localDateStr();
    const speed = Number(store.get('speed', 1));
    const hours = Array.from({ length: 9 }, (_, i) => i * 3).map((h) =>
      `<span style="left:${(h / 24) * 100}%">${String(h).padStart(2, '0')}:00</span>`).join('');
    if (!put(ctx, `
      <div class="toolbar">
        <label>Camera <select id="pb-cam">${cams.map((c) =>
          `<option value="${c.id}" ${c.id === camId ? 'selected' : ''}>${esc(c.name)}</option>`).join('')}</select></label>
        <label>Date <input type="date" id="pb-date" value="${esc(date)}"></label>
        <label>Speed <select id="pb-speed">${SPEEDS.map((s) =>
          `<option value="${s}" ${s === speed ? 'selected' : ''}>${s}&times;</option>`).join('')}</select></label>
        <button class="btn" id="pb-today" type="button">Today</button>
      </div>
      <div class="card">
        <div class="tl-wrap">
          <div class="tl" id="tl" title="Click to play from this time"></div>
          <div class="hours" id="hours">${hours}</div>
        </div>
        <div class="legend">
          <span><i style="background:var(--cont)"></i>Continuous</span>
          <span><i style="background:var(--motion)"></i>Motion</span>
          <span><i style="background:var(--both)"></i>Manual</span>
          <span><i style="background:var(--err)"></i>Event</span>
        </div>
      </div>
      <div class="card">
        <div class="player-frame"><img id="player" alt="Playback"></div>
        <div class="row" style="justify-content:space-between">
          <span id="np" class="muted">Click the timeline or a recording to play.</span>
          <button class="btn ghost small" id="pb-stop" type="button">Stop</button>
        </div>
      </div>
      <div class="card"><div class="table-wrap" id="rec-table"></div></div>`)) return;

    const bounds = () => dayBounds($('#pb-date').value);
    const pickCamera = () => { location.hash = `#/playback?camera=${$('#pb-cam').value}&date=${$('#pb-date').value}`; };
    $('#pb-cam').addEventListener('change', pickCamera);
    $('#pb-date').addEventListener('change', pickCamera);
    $('#pb-today').addEventListener('click', () => { location.hash = `#/playback?camera=${$('#pb-cam').value}&date=${localDateStr()}`; });
    $('#pb-speed').addEventListener('change', () => {
      store.set('speed', Number($('#pb-speed').value));
      if (state.play) play(state.play.rec, currentOffset(), true);
    });
    $('#pb-stop').addEventListener('click', stopPlayback);

    let recs = [];
    const load = async () => {
      const b = bounds();
      const qs = `camera_id=${camId}&date=${$('#pb-date').value}`;
      const day = await api('GET', `/timeline?${qs}`);
      if (!ctx.alive()) return;
      recs = day.recordings;
      drawTimeline(b, day.recordings, day.events);
      drawRecordings(day.recordings);
    };

    $('#tl').addEventListener('click', (e) => {
      const bar = e.currentTarget.getBoundingClientRect();
      const b = bounds();
      const frac = Math.max(0, Math.min(1, (e.clientX - bar.left) / bar.width));
      const t = b.start.getTime() + frac * b.ms;
      // Blocks are drawn at a minimum width, so accept clicks within a few pixels of a recording.
      const tol = (4 / bar.width) * b.ms;
      let hit = null, best = Infinity;
      for (const r of recs) {
        const s = Date.parse(r.start), en = r.end ? Date.parse(r.end) : Date.now();
        const dist = t < s ? s - t : t > en ? t - en : 0;
        if (dist <= tol && dist < best) { best = dist; hit = { r, s, en }; }
      }
      if (hit) { play(hit.r, Math.min(Math.max(t - hit.s, 0), hit.en - hit.s)); return; }
      const next = recs.filter((r) => Date.parse(r.start) > t).sort((x, y) => Date.parse(x.start) - Date.parse(y.start))[0];
      if (next) { toast('No recording at that time; playing the next one.', 'info'); play(next, 0); }
      else toast('No recording at that time.', 'info');
    });

    function drawTimeline(b, rs, evs) {
      const blocks = rs.map((r) => {
        const s = timelinePct(b, r.start);
        const e = timelinePct(b, r.end ? r.end : Date.now());
        const w = Math.max(e - s, 0.2);
        return `<div class="blk ${esc(r.trigger)}" style="left:${s}%;width:${w}%" title="${esc(fmtTime(r.start))} · ${esc(r.trigger)}"></div>`;
      }).join('');
      const ticks = evs.map((ev) =>
        `<div class="tick" style="left:${timelinePct(b, ev.at)}%" title="${esc(fmtTime(ev.at))} · ${esc(KIND_LABEL[ev.kind] || ev.kind)}"></div>`).join('');
      $('#tl').innerHTML = blocks + ticks + '<div class="cursor hidden" id="cursor"></div>';
    }

    function drawRecordings(rs) {
      if (!rs.length) {
        $('#rec-table').innerHTML = '<p class="muted">No recordings on this day.</p>';
        return;
      }
      const rows = rs.map((r) => {
        const dur = r.end ? fmtDuration(Date.parse(r.end) - Date.parse(r.start)) : 'in progress';
        return `<tr>
          <td>${esc(fmtDateTime(r.start))}</td>
          <td>${esc(r.end ? fmtTime(r.end) : '–')}</td>
          <td>${esc(dur)}</td>
          <td><span class="badge ${esc(r.trigger)}">${esc(r.trigger)}</span></td>
          <td>${esc(fmtBytes(r.size_bytes))}</td>
          <td>${r.locked ? '🔒' : ''}</td>
          <td><div class="row">
            <button class="btn small" data-play="${r.id}" type="button">Play</button>
            ${canAs('manager') ? `<button class="btn small" data-lock="${r.id}" data-locked="${r.locked}" type="button">${r.locked ? 'Unlock' : 'Lock'}</button>` : ''}
            <a class="btn small" href="${API}/recordings/${r.id}/download">Download</a>
            ${canAs('manager') ? `<button class="btn small danger" data-del="${r.id}" type="button" ${r.locked || !r.end ? 'disabled title="Locked or in progress"' : ''}>Delete</button>` : ''}
          </div></td>
        </tr>`;
      }).join('');
      $('#rec-table').innerHTML = `<table><thead><tr>
        <th>Start</th><th>End</th><th>Length</th><th>Trigger</th><th>Size</th><th></th><th>Actions</th>
      </tr></thead><tbody>${rows}</tbody></table>`;
      $$('[data-play]', $('#rec-table')).forEach((b) => b.addEventListener('click', () => {
        const r = recs.find((x) => String(x.id) === b.dataset.play);
        if (r) play(r, 0);
      }));
      $$('[data-lock]', $('#rec-table')).forEach((b) => b.addEventListener('click', async () => {
        const locking = b.dataset.locked !== 'true';
        try {
          await api('POST', `/recordings/${b.dataset.lock}/${locking ? 'lock' : 'unlock'}`);
          await load();
        } catch (err) { toast(errorText(err), 'err'); }
      }));
      $$('[data-del]', $('#rec-table')).forEach((b) => b.addEventListener('click', async () => {
        if (!confirm('Delete this recording and its files? This cannot be undone.')) return;
        try {
          await api('DELETE', `/recordings/${b.dataset.del}`);
          toast('Recording deleted', 'ok');
          if (state.play && String(state.play.rec.id) === b.dataset.del) stopPlayback();
          await load();
        } catch (err) { toast(errorText(err), 'err'); }
      }));
    }

    // Playback position is derived from the wall clock and speed, clamped to the recording.
    function currentOffset() {
      const p = state.play;
      if (!p) return 0;
      const elapsed = (Date.now() - p.t0) * p.speed;
      const end = p.rec.end ? Date.parse(p.rec.end) - Date.parse(p.rec.start) : Infinity;
      return Math.min(p.offsetMs + elapsed, end);
    }

    function play(rec, offsetMs, restart = false) {
      const sp = Number($('#pb-speed').value) || 1;
      const offset = Math.max(0, Math.round(offsetMs));
      state.play = { rec, offsetMs: offset, t0: Date.now(), speed: sp };
      $('#player').src = `${API}/recordings/${rec.id}/stream?offset_ms=${offset}&speed=${sp}`;
      const at = new Date(Date.parse(rec.start) + offset);
      $('#np').textContent = `Recording #${rec.id} · ${at.toLocaleString()} · ${rec.trigger} · ${sp}×`;
      if (!restart) store.set('speed', sp);
    }

    function stopPlayback() {
      state.play = null;
      const img = $('#player');
      if (img) img.removeAttribute('src');
      const np = $('#np');
      if (np) np.textContent = 'Stopped.';
      const cursor = $('#cursor');
      if (cursor) cursor.classList.add('hidden');
    }

    every(500, () => {
      const cursor = $('#cursor');
      if (!cursor || !state.play) return;
      const b = bounds();
      const pos = Date.parse(state.play.rec.start) + currentOffset();
      cursor.classList.remove('hidden');
      cursor.style.left = timelinePct(b, pos) + '%';
    });

    await load();
    // Deep links such as #/playback?camera=1&date=...&rec=ID start playback at the recording.
    const deep = recs.find((r) => String(r.id) === ctx.params.rec);
    if (deep) play(deep, 0);
  }

  // ------------------------------------------------------------ events
  async function renderEvents(ctx) {
    const cams = await loadCameras();
    if (!ctx.alive()) return;
    const p = ctx.params;
    const kinds = Object.keys(KIND_LABEL);
    const camOpts = cams.map((c) => `<option value="${c.id}" ${String(c.id) === p.camera ? 'selected' : ''}>${esc(c.name)}</option>`).join('');
    const kindOpts = kinds.map((k) => `<option value="${k}" ${k === p.kind ? 'selected' : ''}>${esc(KIND_LABEL[k])}</option>`).join('');
    if (!put(ctx, `
      <div class="toolbar">
        <h2>Events</h2>
        <form class="inline" id="ev-filter">
          <label>Camera <select name="camera"><option value="">All</option>${camOpts}</select></label>
          <label>Kind <select name="kind"><option value="">All</option>${kindOpts}</select></label>
          <label>Status <select name="ack">
            <option value="">All</option>
            <option value="no" ${p.ack === 'no' ? 'selected' : ''}>Unacknowledged</option>
            <option value="yes" ${p.ack === 'yes' ? 'selected' : ''}>Acknowledged</option>
          </select></label>
          <button class="btn primary" type="submit">Apply</button>
        </form>
      </div>
      <div class="card"><div class="table-wrap" id="ev-table"><p class="muted">Loading…</p></div></div>`)) return;

    const load = async () => {
      const qs = new URLSearchParams({ limit: '200' });
      if (p.camera) qs.set('camera_id', p.camera);
      if (p.kind) qs.set('kind', p.kind);
      if (p.ack === 'no') qs.set('acknowledged', 'false');
      if (p.ack === 'yes') qs.set('acknowledged', 'true');
      const events = await api('GET', `/events?${qs}`);
      if (!ctx.alive()) return;
      if (!events.length) { $('#ev-table').innerHTML = '<p class="muted">No events match.</p>'; return; }
      const rows = events.map((ev) => {
        const day = localDateStr(new Date(ev.at));
        const link = ev.camera_id ? `#/playback?camera=${ev.camera_id}&date=${day}${ev.recording_id ? '&rec=' + ev.recording_id : ''}` : '';
        return `<tr class="${ev.acknowledged ? '' : 'unack'}">
          <td>${esc(fmtDateTime(ev.at))}</td>
          <td>${esc(cameraName(ev.camera_id))}</td>
          <td><span class="badge kind-${esc(ev.kind)}">${esc(KIND_LABEL[ev.kind] || ev.kind)}</span></td>
          <td>${ev.kind === 'motion' ? Math.round(ev.score * 100) + '%' : ''}</td>
          <td class="wrap">${esc(ev.message)}</td>
          <td><div class="row">
            ${link ? `<a class="btn small" href="${link}">Playback</a>` : ''}
            ${ev.acknowledged ? '<span class="muted">Acknowledged</span>' : `<button class="btn small" data-ack="${ev.id}" type="button">Acknowledge</button>`}
          </div></td>
        </tr>`;
      }).join('');
      $('#ev-table').innerHTML = `<table><thead><tr>
        <th>Time</th><th>Camera</th><th>Kind</th><th>Score</th><th>Message</th><th></th>
      </tr></thead><tbody>${rows}</tbody></table>`;
      $$('[data-ack]', $('#ev-table')).forEach((b) => b.addEventListener('click', async () => {
        try { await api('POST', `/events/${b.dataset.ack}/ack`); await load(); } catch (err) { toast(errorText(err), 'err'); }
      }));
    };

    $('#ev-filter').addEventListener('submit', (e) => {
      e.preventDefault();
      const f = e.target;
      const qs = new URLSearchParams();
      if (f.camera.value) qs.set('camera', f.camera.value);
      if (f.kind.value) qs.set('kind', f.kind.value);
      if (f.ack.value) qs.set('ack', f.ack.value);
      const q = qs.toString();
      location.hash = '#/events' + (q ? '?' + q : '');
    });

    let pending = null;
    state.eventHook = () => {
      clearTimeout(pending);
      pending = setTimeout(() => { if (ctx.alive()) load().catch(() => {}); }, 400);
    };
    await load();
  }

  // ------------------------------------------------------------ cameras (manager)
  async function renderCameras(ctx) {
    if (ctx.args[0] === 'new') return renderCameraForm(ctx, null);
    if (ctx.args[0]) return renderCameraForm(ctx, Number(ctx.args[0]));
    const cams = await loadCameras();
    if (!ctx.alive()) return;
    const rows = cams.map((c) => `<tr data-row="${c.id}">
      <td><a href="#/live/${c.id}">${esc(c.name)}</a></td>
      <td>${esc(c.source.type)}</td>
      <td><span class="row"><span class="dot ${stateClass(c.status.state)}" data-dot></span> <span data-state class="wrap">${esc(stateLabel(c.status.state))}${c.status.last_error ? ' — ' + esc(c.status.last_error) : ''}</span></span></td>
      <td>${c.enabled ? 'Yes' : 'No'}</td>
      <td><div class="row">
        <a class="btn small" href="#/cameras/${c.id}">Edit</a>
        <button class="btn small" data-toggle="${c.id}" data-enabled="${c.enabled}" type="button">${c.enabled ? 'Disable' : 'Enable'}</button>
        <button class="btn small danger" data-del="${c.id}" data-name="${esc(c.name)}" type="button">Delete</button>
      </div></td>
    </tr>`).join('');
    if (!put(ctx, `
      <div class="toolbar">
        <h2>Cameras</h2>
        <a class="btn primary" href="#/cameras/new">Add camera</a>
      </div>
      <div class="card"><div class="table-wrap">${cams.length ? `<table>
        <thead><tr><th>Name</th><th>Source</th><th>Status</th><th>Enabled</th><th></th></tr></thead>
        <tbody>${rows}</tbody></table>` : '<p class="muted">No cameras yet.</p>'}
      </div></div>`)) return;

    $$('[data-toggle]').forEach((b) => b.addEventListener('click', async () => {
      const enable = b.dataset.enabled !== 'true';
      try {
        await api('POST', `/cameras/${b.dataset.toggle}/${enable ? 'enable' : 'disable'}`);
        route();
      } catch (err) { toast(errorText(err), 'err'); }
    }));
    $$('[data-del]').forEach((b) => b.addEventListener('click', async () => {
      if (!confirm(`Delete camera "${b.dataset.name}" and all of its recordings? This cannot be undone.`)) return;
      try {
        await api('DELETE', `/cameras/${b.dataset.del}`);
        toast('Camera deleted', 'ok');
        route();
      } catch (err) { toast(errorText(err), 'err'); }
    }));
    every(3000, async () => {
      try {
        const fresh = await loadCameras();
        for (const c of fresh) {
          const row = $(`[data-row="${c.id}"]`);
          if (!row) continue;
          row.querySelector('[data-dot]').className = 'dot ' + stateClass(c.status.state);
          row.querySelector('[data-state]').textContent = stateLabel(c.status.state) + (c.status.last_error ? ' — ' + c.status.last_error : '');
        }
      } catch (_) { /* transient */ }
    });
  }

  let schedPainting = false; // true while the mouse is dragged over the schedule grid

  const SOURCE_FIELDS = { rtsp: ['url', 'auth'], mjpeg: ['url', 'auth'], snapshot: ['url', 'auth', 'interval'], synthetic: ['size'] };
  const MASK = '********';

  function blankSchedule(mode) {
    return Array.from({ length: 336 }, () => mode);
  }

  async function renderCameraForm(ctx, id) {
    const cam = id ? await api('GET', `/cameras/${id}`) : null;
    if (!ctx.alive()) return;
    const src = cam ? cam.source : { type: 'rtsp' };
    const rec = cam ? cam.recording : { segment_seconds: 300, pre_record_seconds: 5, post_record_seconds: 10, max_fps: 10, retention: { max_days: 30, max_bytes: null } };
    const mot = cam ? cam.motion : { enabled: true, sensitivity: 50, threshold: 2, cooldown_seconds: 10, regions: [] };
    const sched = (rec.schedule && rec.schedule.length === 336) ? rec.schedule.slice() : blankSchedule('continuous');
    const gb = rec.retention && rec.retention.max_bytes ? (rec.retention.max_bytes / 1024 ** 3).toString() : '';
    const regions = (mot.regions || []).map((r) => [r.x, r.y, r.w, r.h].join(' ')).join('\n');
    const typeOpts = Object.keys(SOURCE_FIELDS).map((t) =>
      `<option value="${t}" ${t === src.type ? 'selected' : ''}>${t.toUpperCase()}</option>`).join('');
    const brushOpts = MODES.map((m, i) => `<label class="chk">
      <input type="radio" name="brush" value="${m.key}" ${i === 1 ? 'checked' : ''}>
      <span class="swatch ${m.cls}"></span> ${esc(m.label)}</label>`).join('');
    const storedPw = src.password ? MASK : '';

    if (!put(ctx, `
      <div class="toolbar">
        <h2>${cam ? 'Edit camera' : 'Add camera'}</h2>
        <button class="btn" id="onvif-btn" type="button">Search ONVIF…</button>
      </div>
      <div id="onvif-result"></div>
      <form id="cam-form" class="card">
        <fieldset><legend>General</legend>
          <div class="grid-2">
            <label>Name <input name="name" maxlength="64" required value="${esc(cam ? cam.name : '')}"></label>
            <label class="chk" style="align-self:end"><input type="checkbox" name="enabled" ${!cam || cam.enabled ? 'checked' : ''}> Enabled</label>
          </div>
        </fieldset>
        <fieldset><legend>Source</legend>
          <div class="grid-2">
            <label>Type <select name="type">${typeOpts}</select></label>
            <label data-for="rtsp mjpeg snapshot">URL <input name="url" placeholder="rtsp://… or http://…" value="${esc(src.url || '')}"></label>
            <label data-for="rtsp mjpeg snapshot">Username <input name="username" autocomplete="off" value="${esc(src.username || '')}"></label>
            <label data-for="rtsp mjpeg snapshot">Password <input name="password" type="password" autocomplete="new-password" value="${esc(storedPw)}" placeholder="${storedPw ? 'unchanged' : ''}"></label>
            <label data-for="snapshot">Poll interval (ms) <input name="interval_ms" type="number" min="50" value="${esc(src.interval_ms || 1000)}"></label>
            <label data-for="synthetic">Width <input name="width" type="number" min="1" max="4096" value="${esc(src.width || 640)}"></label>
            <label data-for="synthetic">Height <input name="height" type="number" min="1" max="4096" value="${esc(src.height || 480)}"></label>
            <label data-for="synthetic">FPS <input name="fps" type="number" min="1" max="60" value="${esc(src.fps || 5)}"></label>
          </div>
        </fieldset>
        <fieldset><legend>Recording</legend>
          <div class="grid-2">
            <label>Segment length (s) <input name="segment_seconds" type="number" min="10" max="3600" value="${esc(rec.segment_seconds)}"></label>
            <label>Pre-record (s) <input name="pre_record_seconds" type="number" min="0" max="30" value="${esc(rec.pre_record_seconds)}"></label>
            <label>Post-record (s) <input name="post_record_seconds" type="number" min="0" max="600" value="${esc(rec.post_record_seconds)}"></label>
            <label>Max FPS written <input name="max_fps" type="number" min="1" max="60" value="${esc(rec.max_fps)}"></label>
            <label>Keep days (empty = forever) <input name="max_days" type="number" min="1" value="${esc(rec.retention && rec.retention.max_days ? rec.retention.max_days : '')}"></label>
            <label>Max size GB (empty = no limit) <input name="max_gb" type="number" min="0.1" step="0.1" value="${esc(gb)}"></label>
          </div>
          <h3 style="margin-top:14px">Weekly schedule</h3>
          <div class="brush">Brush: ${brushOpts}</div>
          <div class="row" style="margin-bottom:8px">
            <button class="btn small" id="sched-fill" type="button">Fill whole week with brush</button>
            <span class="muted">Click or drag over the grid (Monday to Sunday, 30-minute slots).</span>
          </div>
          <div class="sched-wrap"><div class="sched" id="sched"></div></div>
        </fieldset>
        <fieldset><legend>Motion detection</legend>
          <div class="grid-2">
            <label class="chk"><input type="checkbox" name="motion_enabled" ${mot.enabled ? 'checked' : ''}> Enabled</label>
            <label>Sensitivity (1–99) <input name="sensitivity" type="number" min="1" max="99" value="${esc(mot.sensitivity)}"></label>
            <label>Threshold % (1–99) <input name="threshold" type="number" min="1" max="99" value="${esc(mot.threshold)}"></label>
            <label>Cooldown (s) <input name="cooldown_seconds" type="number" min="0" value="${esc(mot.cooldown_seconds)}"></label>
          </div>
          <label style="margin-top:10px"><span>Detection regions: one per line as <code>x y w h</code> (0–1). Empty = whole frame.</span>
            <textarea name="regions" rows="3">${esc(regions)}</textarea></label>
        </fieldset>
        <div class="error" id="cam-err"></div>
        <div class="row">
          <button class="btn primary" type="submit">${cam ? 'Save changes' : 'Create camera'}</button>
          <a class="btn" href="#/cameras">Cancel</a>
        </div>
      </form>`)) return;

    const form = $('#cam-form');
    const typeSel = form.elements.type;
    const showFields = () => {
      $$('[data-for]').forEach((el) => {
        el.classList.toggle('hidden', !el.dataset.for.split(' ').includes(typeSel.value));
      });
    };
    typeSel.addEventListener('change', showFields);
    showFields();

    // Schedule grid: 336 cells, index = weekday * 48 + slot.
    const grid = $('#sched');
    const hourLabels = Array.from({ length: 24 }, (_, h) => `<div class="hl">${h}</div>`).join('');
    grid.innerHTML = `<div></div>${hourLabels}` + DAYS.map((d, di) => `
      <div class="dl">${d}</div>` + Array.from({ length: 48 }, (_, s) => {
        const i = di * 48 + s;
        const m = MODES.find((x) => x.key === sched[i]) || MODES[1];
        return `<div class="cell ${m.cls}" data-i="${i}"></div>`;
      }).join('')).join('');
    const brush = () => (form.elements.brush.value || 'continuous');
    const paint = (cell) => {
      const i = Number(cell.dataset.i);
      const key = brush();
      sched[i] = key;
      cell.className = 'cell ' + (MODES.find((m) => m.key === key) || MODES[1]).cls;
    };
    grid.addEventListener('pointerdown', (e) => {
      const cell = e.target.closest('.cell');
      if (!cell) return;
      schedPainting = true;
      paint(cell);
      e.preventDefault();
    });
    grid.addEventListener('pointerover', (e) => {
      if (!schedPainting) return;
      const cell = e.target.closest('.cell');
      if (cell) paint(cell);
    });
    $('#sched-fill').addEventListener('click', () => {
      for (let i = 0; i < 336; i++) sched[i] = brush();
      $$('.cell', grid).forEach((c) => {
        c.className = 'cell ' + (MODES.find((m) => m.key === brush()) || MODES[1]).cls;
      });
    });

    $('#onvif-btn').addEventListener('click', async () => {
      const out = $('#onvif-result');
      out.innerHTML = '<div class="card muted">Searching the network for ONVIF devices (about 3 seconds)…</div>';
      try {
        const devices = await api('GET', '/discovery/onvif?timeout_ms=3000');
        if (!devices.length) { out.innerHTML = '<div class="card muted">No ONVIF devices answered.</div>'; return; }
        out.innerHTML = `<div class="card"><h3>Found ${devices.length} device(s)</h3><table><thead><tr>
          <th>Name</th><th>Device service</th><th></th></tr></thead><tbody>${devices.map((d, i) => `
          <tr><td>${esc(d.name || d.endpoint)}</td><td class="wrap">${esc((d.xaddrs || []).join(' '))}</td>
          <td><button class="btn small" type="button" data-use="${i}">Use name</button></td></tr>`).join('')}
          </tbody></table><p class="muted">The RTSP stream URL is camera-specific; copy it from the camera's documentation or ONVIF tool.</p></div>`;
        $$('[data-use]', out).forEach((b) => b.addEventListener('click', () => {
          const d = devices[Number(b.dataset.use)];
          if (!form.name.value && d.name) form.name.value = d.name;
          toast('Name copied. Enter the stream URL for this device.', 'info');
        }));
      } catch (err) {
        out.innerHTML = `<div class="card error">${esc(errorText(err))}</div>`;
      }
    });

    const parseRegions = (text) => text.split('\n').map((line) => line.trim()).filter(Boolean).map((line, idx) => {
      const nums = line.split(/[\s,]+/).map(Number);
      if (nums.length !== 4 || nums.some((n) => !isFinite(n))) throw new Error(`Region line ${idx + 1}: expected four numbers "x y w h".`);
      const [x, y, w, h] = nums;
      return { x, y, w, h };
    });

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const err = $('#cam-err');
      err.textContent = '';
      let body;
      try {
        const type = typeSel.value;
        const source = { type };
        if (type === 'synthetic') {
          Object.assign(source, { width: Number(form.width.value), height: Number(form.height.value), fps: Number(form.fps.value) });
        } else {
          Object.assign(source, { url: form.url.value.trim(), username: form.username.value.trim() || null, password: form.password.value || null });
          if (type === 'snapshot') source.interval_ms = Number(form.interval_ms.value);
        }
        const optNum = (v) => (v === '' ? null : Number(v));
        body = {
          name: form.name.value.trim(),
          enabled: form.enabled.checked,
          source,
          recording: {
            schedule: sched.slice(),
            segment_seconds: Number(form.segment_seconds.value),
            pre_record_seconds: Number(form.pre_record_seconds.value),
            post_record_seconds: Number(form.post_record_seconds.value),
            max_fps: Number(form.max_fps.value),
            retention: {
              max_days: optNum(form.max_days.value),
              max_bytes: form.max_gb.value === '' ? null : Math.round(Number(form.max_gb.value) * 1024 ** 3),
            },
          },
          motion: {
            enabled: form.motion_enabled.checked,
            sensitivity: Number(form.sensitivity.value),
            threshold: Number(form.threshold.value),
            cooldown_seconds: Number(form.cooldown_seconds.value),
            regions: parseRegions(form.regions.value),
          },
        };
      } catch (ex) {
        err.textContent = ex.message;
        return;
      }
      try {
        if (id) await api('PUT', `/cameras/${id}`, body);
        else await api('POST', '/cameras', body);
        toast(id ? 'Camera saved' : 'Camera created', 'ok');
        location.hash = '#/cameras';
      } catch (ex) {
        err.textContent = errorText(ex);
      }
    });
  }

  // ------------------------------------------------------------ users (admin)
  async function renderUsers(ctx) {
    const users = await api('GET', '/users');
    if (!ctx.alive()) return;
    const rows = users.map((u) => `<tr>
      <td>${esc(u.username)}${u.id === state.me.id ? ' <span class="muted">(you)</span>' : ''}</td>
      <td><select data-role="${u.id}" ${u.id === state.me.id ? 'disabled' : ''}>
        ${['viewer', 'manager', 'admin'].map((r) => `<option value="${r}" ${r === u.role ? 'selected' : ''}>${r}</option>`).join('')}
      </select></td>
      <td>${esc(fmtDateTime(u.created_at))}</td>
      <td><div class="row">
        <button class="btn small" data-pw="${u.id}" data-name="${esc(u.username)}" type="button">Set password</button>
        ${u.id === state.me.id ? '' : `<button class="btn small danger" data-del="${u.id}" data-name="${esc(u.username)}" type="button">Delete</button>`}
      </div></td>
    </tr>`).join('');
    if (!put(ctx, `
      <div class="toolbar"><h2>Users</h2></div>
      <div class="card">
        <form class="inline" id="user-form">
          <label>Username <input name="username" required maxlength="64"></label>
          <label>Password (min 8) <input name="password" type="password" required minlength="8" autocomplete="new-password"></label>
          <label>Role <select name="role"><option value="viewer">viewer</option><option value="manager">manager</option><option value="admin">admin</option></select></label>
          <button class="btn primary" type="submit">Add user</button>
          <span class="error" id="user-err"></span>
        </form>
        <div class="table-wrap"><table><thead><tr><th>User</th><th>Role</th><th>Created</th><th></th></tr></thead>
        <tbody>${rows}</tbody></table></div>
      </div>`)) return;

    $('#user-form').addEventListener('submit', async (e) => {
      e.preventDefault();
      const f = e.target;
      try {
        await api('POST', '/users', { username: f.username.value.trim(), password: f.password.value, role: f.role.value });
        toast('User created', 'ok');
        route();
      } catch (err) { $('#user-err').textContent = errorText(err); }
    });
    $$('[data-role]').forEach((s) => s.addEventListener('change', async () => {
      try {
        await api('PUT', `/users/${s.dataset.role}`, { role: s.value });
        toast('Role updated', 'ok');
      } catch (err) { toast(errorText(err), 'err'); route(); }
    }));
    $$('[data-pw]').forEach((b) => b.addEventListener('click', async () => {
      const pw = prompt(`New password for ${b.dataset.name} (at least 8 characters):`);
      if (!pw) return;
      try {
        await api('PUT', `/users/${b.dataset.pw}`, { password: pw });
        toast('Password updated', 'ok');
      } catch (err) { toast(errorText(err), 'err'); }
    }));
    $$('[data-del]').forEach((b) => b.addEventListener('click', async () => {
      if (!confirm(`Delete user ${b.dataset.name}?`)) return;
      try {
        await api('DELETE', `/users/${b.dataset.del}`);
        toast('User deleted', 'ok');
        route();
      } catch (err) { toast(errorText(err), 'err'); }
    }));
  }

  // ------------------------------------------------------------ settings (admin)
  async function renderSettings(ctx) {
    const s = await api('GET', '/settings');
    if (!ctx.alive()) return;
    if (!put(ctx, `
      <div class="toolbar"><h2>Settings</h2></div>
      <form class="card" id="settings-form" style="max-width:640px">
        <div class="grid-2">
          <label>Stop recording below free space (GB)
            <input name="min_gb" type="number" min="0" step="0.1" value="${(s.min_free_bytes / 1024 ** 3).toFixed(1)}"></label>
          <label>Session lifetime (hours)
            <input name="ttl" type="number" min="1" max="720" value="${s.session_ttl_hours}"></label>
          <label>Retention sweep interval (seconds)
            <input name="retention" type="number" min="10" value="${s.retention_interval_seconds}"></label>
        </div>
        <div class="row" style="margin-top:12px"><button class="btn primary" type="submit">Save settings</button><span id="set-msg" class="muted"></span></div>
      </form>`)) return;
    $('#settings-form').addEventListener('submit', async (e) => {
      e.preventDefault();
      const f = e.target;
      try {
        await api('PUT', '/settings', {
          min_free_bytes: Math.round(Number(f.min_gb.value) * 1024 ** 3),
          session_ttl_hours: Number(f.ttl.value),
          retention_interval_seconds: Number(f.retention.value),
        });
        toast('Settings saved', 'ok');
        $('#set-msg').textContent = 'Saved.';
      } catch (err) { toast(errorText(err), 'err'); }
    });
  }

  // ------------------------------------------------------------ system
  async function renderSystem(ctx) {
    const paint = (info) => {
      const st = info.storage;
      const used = st.total_bytes ? Math.min(100, ((st.total_bytes - st.free_bytes) / st.total_bytes) * 100) : 0;
      return `
        <div class="cards">
          <div class="card"><h3>Cameras</h3><div class="stat">${info.cameras.online} / ${info.cameras.total} online</div>
            <div class="muted">${info.cameras.recording} recording now</div></div>
          <div class="card"><h3>Storage</h3><div class="stat">${fmtBytes(st.free_bytes)} free</div>
            <div class="bar"><div style="width:${used.toFixed(1)}%"></div></div>
            <div class="muted">${fmtBytes(st.used_bytes)} recordings · ${fmtBytes(st.total_bytes)} total</div></div>
          <div class="card"><h3>Server</h3>
            <dl class="kv">
              <dt>Version</dt><dd>${esc(info.version)}</dd>
              <dt>Uptime</dt><dd>${esc(fmtDuration(info.uptime_seconds * 1000))}</dd>
              <dt>ffmpeg</dt><dd>${info.ffmpeg ? 'available' : 'not found (RTSP cameras unavailable)'}</dd>
              <dt>Storage root</dt><dd>${esc(st.root)}</dd>
            </dl></div>
        </div>`;
    };
    const first = await api('GET', '/system/info');
    if (!put(ctx, `
      <div class="toolbar"><h2>System</h2></div>
      <div id="sys">${paint(first)}</div>
      <form class="card" id="pw-form" style="max-width:480px">
        <h3>Change my password</h3>
        <div class="grid-2">
          <label>Current password <input name="old" type="password" required autocomplete="current-password"></label>
          <label>New password (min 8) <input name="new" type="password" required minlength="8" autocomplete="new-password"></label>
        </div>
        <div class="row" style="margin-top:12px"><button class="btn primary" type="submit">Change password</button><span id="pw-msg" class="error"></span></div>
      </form>`)) return;
    every(10000, async () => {
      try { const info = await api('GET', '/system/info'); const el = $('#sys'); if (el) el.innerHTML = paint(info); } catch (_) { /* transient */ }
    });
    $('#pw-form').addEventListener('submit', async (e) => {
      e.preventDefault();
      const f = e.target;
      const msg = $('#pw-msg');
      try {
        await api('PUT', '/auth/password', { old_password: f.old.value, new_password: f.new.value });
        f.reset();
        msg.className = 'muted';
        msg.textContent = 'Password changed.';
      } catch (err) {
        msg.className = 'error';
        msg.textContent = errorText(err);
      }
    });
  }

  // ------------------------------------------------------------ server-sent events
  function startEvents() {
    stopEvents();
    if (!window.EventSource) return;
    const es = new EventSource(API + '/events/stream');
    for (const kind of EVENT_KINDS) {
      es.addEventListener(kind, (e) => {
        let ev;
        try { ev = JSON.parse(e.data); } catch (_) { return; }
        onServerEvent(kind, ev);
      });
    }
    es.onerror = () => { if (!state.me) stopEvents(); };
    state.es = es;
  }

  function stopEvents() {
    if (state.es) { state.es.close(); state.es = null; }
  }

  function onServerEvent(kind, ev) {
    if (kind === 'motion') {
      flashTile(ev.camera_id);
      // Motion can fire often; show at most one toast per camera every 10 s.
      const now = Date.now();
      if (now - (state.lastMotionToast[ev.camera_id] || 0) < 10000) return;
      state.lastMotionToast[ev.camera_id] = now;
    }
    // Recording start/stop is routine; keep it out of the toast stream.
    if (!kind.startsWith('recording_')) {
      const tone = kind === 'connection_lost' || kind === 'storage_full' ? 'warn' : kind === 'connection_restored' ? 'ok' : 'info';
      toast(`${KIND_LABEL[kind] || kind}: ${cameraName(ev.camera_id)}${ev.message ? ' — ' + ev.message : ''}`, tone);
    }
    if (state.eventHook) state.eventHook(ev);
  }

  // ------------------------------------------------------------ boot
  async function boot() {
    $('#logout').addEventListener('click', logout);
    window.addEventListener('hashchange', route);
    window.addEventListener('pointerup', () => { schedPainting = false; });
    try {
      state.me = await api('GET', '/auth/me');
      startEvents();
    } catch (_) {
      state.me = null;
    }
    route();
  }

  boot();
})();
