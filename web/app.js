// ESC web app. Plain JavaScript, no libraries, no build step.
// One WebSocket to the relay carries every action; uploads and downloads use plain HTTP.

// ============================================================================================
// Little helpers
// ============================================================================================

const $ = (s, r = document) => r.querySelector(s);
const PROPS = new Set(['value', 'checked', 'disabled', 'hidden', 'selected', 'indeterminate', 'multiple', 'open']);

/** h('div', {class: 'x', onclick: fn}, child, 'text', [more]) */
function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const k in attrs) {
      const v = attrs[k];
      if (v == null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else if (PROPS.has(k)) el[k] = v;
      else el.setAttribute(k, v === true ? '' : v);
    }
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : String(kid));
  }
  return el;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const KB = 1024, MB = KB * 1024, GB = MB * 1024;
function bytes(n) {
  if (n < KB) return `${n} B`;
  if (n < MB) return `${(n / KB).toFixed(n < 10 * KB ? 1 : 0)} KB`;
  if (n < GB) return `${(n / MB).toFixed(n < 10 * MB ? 1 : 0)} MB`;
  return `${(n / GB).toFixed(2)} GB`;
}
const fClock = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const fWeek = new Intl.DateTimeFormat(undefined, { weekday: 'short' });
const fDate = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short' });
const fDay = new Intl.DateTimeFormat(undefined, { weekday: 'long', day: 'numeric', month: 'long' });
const fFull = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
const sameDay = (a, b) => new Date(a).toDateString() === new Date(b).toDateString();
function when(t) {
  if (!t) return '';
  const d = new Date(t);
  if (sameDay(t, Date.now())) return fClock.format(d);
  if (Date.now() - t < 6 * 864e5) return fWeek.format(d);
  return fDate.format(d);
}
function until(t) {
  const ms = Math.max(0, t - Date.now() - (S.skew || 0));
  const hh = Math.floor(ms / 36e5), mm = Math.floor((ms % 36e5) / 6e4);
  return hh ? `${hh} h ${mm} min` : `${mm} min`;
}
const b64u = (u8) => btoa(String.fromCharCode(...u8)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const unb64u = (s) => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
const ext = (name) => (/\.([a-z0-9]{1,5})$/i.exec(name || '')?.[1] || 'file').toLowerCase();
const kindOf = (mime, name) =>
  /^image\//.test(mime) || /\.(heic|heif|jxl)$/i.test(name) ? 'image'
  : /^video\//.test(mime) ? 'video'
  : /^audio\//.test(mime) ? 'audio'
  : /pdf|word|document|text|sheet|excel|presentation|powerpoint|rtf|opendocument/.test(mime) ? 'doc'
  : 'other';

let toastTimer;
function toast(msg) {
  document.querySelector('.toast')?.remove();
  const t = h('div', { class: 'toast', role: 'status' }, msg);
  document.body.append(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.remove(), 3500);
}

// Icons: 24px strokes, drawn here so the app ships no image files.
const ICONS = {
  back: 'M15 18l-6-6 6-6',
  close: 'M18 6 6 18M6 6l12 12',
  settings: 'M4 7h9m4 0h3M4 17h3m4 0h9M15 5v4M9 15v4',
  clip: 'M20.5 11.5l-8.2 8.2a5.2 5.2 0 0 1-7.4-7.4l8.6-8.6a3.5 3.5 0 0 1 5 5l-8.6 8.6a1.7 1.7 0 0 1-2.5-2.5l7.9-7.9',
  send: 'M4 12h15M13 6l6 6-6 6',
  download: 'M12 4v11m-5-5 5 5 5-5M5 20h14',
  anchor: 'M12 8.5a2.25 2.25 0 1 0 0-4.5 2.25 2.25 0 0 0 0 4.5ZM12 8.5V21M4.5 13.5a7.5 7.5 0 0 0 15 0M8.5 11.5h7',
  stash: 'M3 8.5 12 4l9 4.5v7L12 20l-9-4.5zM3 8.5l9 4.5 9-4.5M12 13v7',
  room: 'M17 3l-6.5 6.5M8.5 9.8l5.7 5.7M8.5 9.8c-3 .6-5 3.6-5.5 10.2 6.6-.5 9.6-2.5 10.2-5.5',
  tower: 'M9 21l1-9h4l1 9M7.5 12h9L15 7H9zM12 3v4M10 3h4',
  info: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 11v5M12 7.6v.1',
  plus: 'M12 5v14M5 12h14',
  door: 'M6 21V3h12v18M14.5 12v.1M3 21h18',
  crew: 'M16 20v-1.5a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4V20M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7ZM22 20v-1.5a4 4 0 0 0-3-3.8M15.5 4.2a3.5 3.5 0 0 1 0 6.6',
  copy: 'M9 9h11v11H9zM5 15H4V4h11v1',
  share: 'M4 13v7h16v-7M12 3v12M7.5 7.5 12 3l4.5 4.5',
  trash: 'M4 6h16M9 6V4h6v2M6 6l1 14h10l1-14',
  play: 'M8 5.5v13l11-6.5z',
  bot: 'M6.5 9.5h11v9h-11zM10 13.5v.1M14 13.5v.1M10 16.5h4M12 6v3.5M12 6a1.2 1.2 0 1 0 0-2.4A1.2 1.2 0 0 0 12 6Z',
  zoom: 'M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14ZM20 20l-4-4M11 8v6M8 11h6',
};
function icon(name, cls = '') {
  const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('aria-hidden', 'true');
  if (cls) s.setAttribute('class', cls);
  const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  p.setAttribute('d', ICONS[name]);
  if (name === 'play') p.setAttribute('fill', 'currentColor');
  else {
    p.setAttribute('fill', 'none');
    p.setAttribute('stroke', 'currentColor');
    p.setAttribute('stroke-width', '1.8');
    p.setAttribute('stroke-linecap', 'round');
    p.setAttribute('stroke-linejoin', 'round');
  }
  s.append(p);
  return s;
}
const iconBtn = (name, label, onclick, cls = '') => h('button', { class: `icon-btn ${cls}`, type: 'button', title: label, 'aria-label': label, onclick }, icon(name));

// ============================================================================================
// Signal flags: every callsign is spelled in International Code of Signals flags.
// ============================================================================================

const FR = '#D0202E', FU = '#1747A6', FY = '#F7C600', FK = '#161A1D', FW = '#FFFFFF';
const r = (x, y, w, hh, c) => `<rect x="${x}" y="${y}" width="${w}" height="${hh}" fill="${c}"/>`;
const saltire = (c) => `<path d="M0 0L30 20M30 0L0 20" stroke="${c}" stroke-width="4.2"/>`;
const FLAG = {
  B: `<path d="M0 0H30L23 10L30 20H0Z" fill="${FR}"/>`,
  C: r(0, 0, 30, 4, FU) + r(0, 4, 30, 4, FW) + r(0, 8, 30, 4, FR) + r(0, 12, 30, 4, FW) + r(0, 16, 30, 4, FU),
  D: r(0, 0, 30, 20, FY) + r(0, 5, 30, 10, FU),
  E: r(0, 0, 30, 10, FU) + r(0, 10, 30, 10, FR),
  F: r(0, 0, 30, 20, FW) + `<path d="M15 1.5L28.5 10L15 18.5L1.5 10Z" fill="${FR}"/>`,
  G: [0, 1, 2, 3, 4, 5].map((i) => r(i * 5, 0, 5, 20, i % 2 ? FU : FY)).join(''),
  H: r(0, 0, 15, 20, FW) + r(15, 0, 15, 20, FR),
  J: r(0, 0, 30, 20, FU) + r(0, 6.67, 30, 6.66, FW),
  K: r(0, 0, 15, 20, FY) + r(15, 0, 15, 20, FU),
  M: r(0, 0, 30, 20, FU) + saltire(FW),
  N: [0, 1, 2, 3].flatMap((y) => [0, 1, 2, 3].map((x) => r(x * 7.5, y * 5, 7.5, 5, (x + y) % 2 ? FW : FU))).join(''),
  P: r(0, 0, 30, 20, FU) + r(10, 6.5, 10, 7, FW),
  Q: r(0, 0, 30, 20, FY),
  R: r(0, 0, 30, 20, FR) + r(12.5, 0, 5, 20, FY) + r(0, 7.5, 30, 5, FY),
  S: r(0, 0, 30, 20, FW) + r(10, 6.5, 10, 7, FU),
  T: r(0, 0, 10, 20, FR) + r(10, 0, 10, 20, FW) + r(20, 0, 10, 20, FU),
  V: r(0, 0, 30, 20, FW) + saltire(FR),
  W: r(0, 0, 30, 20, FU) + r(3.5, 3.5, 23, 13, FW) + r(8, 7, 14, 6, FR),
  X: r(0, 0, 30, 20, FW) + r(12.5, 0, 5, 20, FU) + r(0, 7.5, 30, 5, FU),
  Z: `<path d="M0 0H30L15 10Z" fill="${FY}"/><path d="M30 0V20L15 10Z" fill="${FU}"/><path d="M0 20H30L15 10Z" fill="${FR}"/><path d="M0 0V20L15 10Z" fill="${FK}"/>`,
  2: r(0, 0, 30, 20, FU) + `<circle cx="8.5" cy="10" r="4.6" fill="${FW}"/>`,
  3: r(0, 0, 10, 20, FR) + r(10, 0, 10, 20, FW) + r(20, 0, 10, 20, FU),
  4: r(0, 0, 30, 20, FR) + r(6.5, 0, 4, 20, FW) + r(0, 8, 30, 4, FW),
  5: r(0, 0, 15, 20, FY) + r(15, 0, 15, 20, FU),
  6: r(0, 0, 30, 10, FK) + r(0, 10, 30, 10, FW),
  7: r(0, 0, 30, 10, FY) + r(0, 10, 30, 10, FR),
  8: r(0, 0, 30, 20, FW) + r(6.5, 0, 4, 20, FR) + r(0, 8, 30, 4, FR),
  9: r(0, 0, 15, 10, FW) + r(15, 0, 15, 10, FK) + r(0, 10, 15, 10, FR) + r(15, 10, 15, 10, FY),
};
/** What each flag says on its own (International Code of Signals). */
const MEANING = {
  K: 'I wish to communicate with you',
  C: 'Affirmative',
  N: 'Negative',
  V: 'I require assistance',
};
const PENNANT = 'M0 0L30 7L30 13L0 20Z';
function flagSVG(ch, square = false) {
  const body = FLAG[ch];
  if (!body) return '';
  const pen = ch >= '0' && ch <= '9';
  const edge = pen
    ? `<path d="${PENNANT}" fill="none" stroke="rgba(0,0,0,.28)" stroke-width=".9"/>`
    : `<rect x=".45" y=".45" width="29.1" height="19.1" fill="none" stroke="rgba(0,0,0,.2)" stroke-width=".9"/>`;
  return `<svg class="flag" viewBox="0 0 30 20"${square ? ' preserveAspectRatio="none"' : ''} aria-hidden="true">${pen ? `<g clip-path="url(#esc-pennant)">${body}</g>` : body}${edge}</svg>`;
}
function flags(cs, cls = '') {
  const s = h('span', { class: `flags ${cls}`, 'aria-hidden': 'true' });
  s.innerHTML = [...(cs || '')].map((c) => flagSVG(c)).join('');
  return s;
}
function avatar(cs, small = false) {
  const a = h('span', { class: `avatar${small ? ' small' : ''}`, 'aria-hidden': 'true', title: cs });
  a.innerHTML = [...(cs || '')].slice(0, 4).map((c) => flagSVG(c, true)).join('');
  return a;
}
function crewAvatar(conv) {
  const a = h('span', { class: 'avatar crew', 'aria-hidden': 'true' });
  const others = (conv.m || []).filter((m) => m.s === 'j' && m.a !== S.me?.id).map((m) => m.a);
  const pick = [...others, conv.by, S.me?.id].filter(Boolean).slice(0, 4);
  while (pick.length < 4) pick.push(pick[pick.length - 1] || 'Q');
  a.innerHTML = pick.map((cs) => flagSVG(cs[0], true)).join('');
  return a;
}
const CS_RE = /^[23456789BCDFGHJKMNPQRSTVWXZ]{4,5}$/;
const cleanCS = (s) => (s || '').toUpperCase().replace(/[^0-9A-Z]/g, '');

// ============================================================================================
// State, connection, RPC
// ============================================================================================

const S = {
  me: null,
  convs: new Map(),
  msgs: new Map(),
  rin: [],
  rout: [],
  relays: [],
  signup: 'invite',
  mi: 0,
  gmax: 128,
  tmax: 16000,
  files: null,
  queue: [],
  online: false,
  skew: 0,
  info: null,
};
const WS = { s: null, seq: 0, wait: new Map(), q: [], backoff: 500, started: false };

function connect() {
  const s = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
  WS.s = s;
  s.onopen = () => {
    WS.backoff = 500;
    S.online = true;
    for (const m of WS.q.splice(0)) s.send(m);
    hello().catch(() => {});
  };
  s.onmessage = (e) => {
    let m;
    try {
      m = JSON.parse(e.data);
    } catch {
      return;
    }
    if (m.i) {
      const w = WS.wait.get(m.i);
      if (!w) return;
      WS.wait.delete(m.i);
      if ('r' in m) w.ok(m.r);
      else w.no(Object.assign(new Error(m.e || 'Something went wrong.'), { code: m.c }));
    } else if (m.p) onPush(m);
  };
  s.onclose = () => {
    if (WS.s === s) WS.s = null;
    S.online = false;
    for (const w of WS.wait.values()) w.no(Object.assign(new Error('Connection lost. Trying again…'), { code: 0 }));
    WS.wait.clear();
    paint();
    setTimeout(async () => {
      const r = await fetch('/api/me', { cache: 'no-store' }).catch(() => null);
      if (r && r.status === 401) return signedOut();
      connect();
    }, WS.backoff);
    WS.backoff = Math.min(WS.backoff * 2, 15000);
  };
}

function rpc(m, args = {}) {
  return new Promise((ok, no) => {
    const i = ++WS.seq;
    WS.wait.set(i, { ok, no });
    const text = JSON.stringify({ ...args, i, m });
    if (WS.s && WS.s.readyState === 1) WS.s.send(text);
    else WS.q.push(text);
  });
}

async function api(path, body, method = 'POST') {
  const r = await fetch(path, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
    cache: 'no-store',
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw Object.assign(new Error(j.e || `Request failed (${r.status})`), { code: r.status, data: j });
  return j;
}

async function hello() {
  const r = await rpc('hello');
  S.me = r.me;
  S.rin = r.rin;
  S.rout = r.rout;
  S.relays = r.relays;
  S.signup = r.signup;
  S.mi = r.mi;
  S.gmax = r.gmax;
  S.tmax = r.tmax;
  S.skew = r.now - Date.now();
  S.convs = new Map(r.convs.map((c) => [c.c, c]));
  if (!document.querySelector('.shell')) mountShell();
  const route = parseRoute();
  if (route.view === 'line' && S.msgs.has(route.c)) {
    S.msgs.delete(route.c);
    if (S.view?.c === route.c) S.view = null;
  }
  paint();
  routeChanged();
  const want = sessionStorage.getItem('esc-knock');
  if (want) {
    sessionStorage.removeItem('esc-knock');
    knockDialog(want);
  }
}

function onPush(m) {
  switch (m.p) {
    case 'msg':
      addMessage(m.m);
      break;
    case 'conv': {
      const old = S.convs.get(m.c.c);
      S.convs.set(m.c.c, m.c);
      if (!old && m.c.me === 'i') notify('Crew invite', `You're invited to ${m.c.n}`, m.c.c);
      if (S.view?.c === m.c.c) S.view.header?.();
      break;
    }
    case 'cgone':
      S.convs.delete(m.c);
      S.msgs.delete(m.c);
      if (parseRoute().c === m.c) go('');
      break;
    case 'rin':
      if (!S.rin.some((x) => x.r === m.r.r)) S.rin.push(m.r);
      notify('Knock knock', `${m.r.n || m.r.f} wishes to communicate with you`, 'knock');
      break;
    case 'rout':
      if (!S.rout.some((x) => x.r === m.r.r)) S.rout.push(m.r);
      break;
    case 'rgone':
      S.rin = S.rin.filter((x) => x.r !== m.r);
      S.rout = S.rout.filter((x) => x.r !== m.r);
      break;
    case 'me':
      S.me = m.me;
      break;
    case 'files':
      S.files = null;
      refreshFileStates();
      break;
    case 'read': {
      const c = S.convs.get(m.c);
      if (c && m.t > (c.rd || 0)) {
        c.rd = m.t;
        c.u = 0;
      }
      break;
    }
    case 'sync':
      hello().catch(() => {});
      return;
  }
  paint();
  S.view?.update?.(m.p);
}

// ============================================================================================
// Painting: rail + banners are cheap to rebuild; the pane keeps its own DOM.
// ============================================================================================

let painting = false;
function paint() {
  if (painting) return;
  painting = true;
  requestAnimationFrame(() => {
    painting = false;
    if (!S.me) return;
    paintAlerts();
    paintRail();
    let total = 0;
    for (const c of S.convs.values()) total += c.u || 0;
    total += S.rin.length + [...S.convs.values()].filter((c) => c.me === 'i').length;
    document.title = total ? `(${total}) ESC` : 'ESC';
  });
}

function displayName(cs, conv) {
  return S.me?.nick?.[cs] || conv?.m?.find((m) => m.a === cs)?.n || cs;
}
function convTitle(conv) {
  if (conv.g) return conv.n || 'Crew';
  const other = conv.m?.find((m) => m.a !== S.me.id);
  return other ? displayName(other.a, conv) : 'Line';
}
function convOther(conv) {
  return conv.m?.find((m) => m.a !== S.me.id)?.a;
}

function tankState() {
  const m = S.me;
  const pct = m.q ? m.u / m.q : 0;
  return { pct, warn: !m.ar && pct >= 0.8, alert: !m.ar && pct >= 0.95 };
}

function paintAlerts() {
  const box = $('.alerts');
  if (!box) return;
  box.replaceChildren();
  const m = S.me;
  const t = tankState();
  if (t.alert) {
    const fresh = !sessionStorage.getItem('esc-red');
    box.append(
      h(
        'div',
        { class: `banner red${fresh ? ' fresh' : ''}`, role: 'alert' },
        flags('NC'),
        h('p', null, h('b', null, 'Red Alert. '), `Your Tank is ${Math.floor(t.pct * 100)}% full. New messages with files will probably NOT be received.`),
        h('button', { class: 'btn small solid', type: 'button', onclick: () => go('/room') }, 'Open the Cleaning Room'),
        h('button', { class: 'btn small', type: 'button', onclick: () => setAutoRecycle(true) }, 'Turn on auto-recycling'),
      ),
    );
    if (fresh) {
      sessionStorage.setItem('esc-red', '1');
      redAlertDialog(t.pct);
    }
  } else if (m.cl) {
    box.append(
      h(
        'div',
        { class: 'banner amber' },
        icon('bot', 'i'),
        h('p', null, `Dustin comes by in ${until(m.cl)} to bring your Tank back under ${bytes(m.q)}. Make Keepers of anything you want to keep.`),
        h('button', { class: 'btn small', type: 'button', onclick: () => go('/room') }, 'See what goes'),
      ),
    );
  }
  if (!S.online) box.append(h('div', { class: 'banner quiet' }, 'Reconnecting…'));
}

function redAlertDialog(pct) {
  const d = h(
    'dialog',
    { class: 'alarm' },
    h(
      'div',
      { class: 'body' },
      flags('NC', 'big'),
      h('h2', null, 'Red Alert'),
      h('p', null, `Your Tank is ${Math.floor(pct * 100)}% full and auto-recycling is off.`),
      h('p', null, h('b', null, 'New messages with files will probably NOT be received.'), ' Text messages always arrive.'),
      h(
        'div',
        { class: 'foot' },
        h('button', { class: 'btn ghost', type: 'button', onclick: () => d.close() }, 'I understand'),
        h('button', { class: 'btn', type: 'button', onclick: () => (d.close(), setAutoRecycle(true)) }, 'Turn on auto-recycling'),
        h('button', { class: 'btn', type: 'button', onclick: () => (d.close(), go('/room')) }, 'Open the Cleaning Room'),
      ),
    ),
  );
  d.addEventListener('close', () => d.remove());
  document.body.append(d);
  d.showModal();
}

async function setAutoRecycle(on) {
  try {
    await rpc('me_set', { ar: on });
    toast(on ? 'Auto-recycling is on. Dustin keeps your Tank under its size.' : 'Auto-recycling is off.');
  } catch (e) {
    toast(e.message);
  }
}

function tankEl() {
  const m = S.me;
  const t = tankState();
  const g = h('span', { class: `gauge${t.alert ? ' alert' : t.warn ? ' warn' : ''}` });
  const fill = h('i');
  fill.style.width = `${Math.min(100, t.pct * 100)}%`;
  const m80 = h('em');
  m80.style.left = '80%';
  const m95 = h('em');
  m95.style.left = '95%';
  g.append(fill, m80, m95);
  const note = m.ar
    ? m.cl
      ? `Dustin comes by in ${until(m.cl)}`
      : `Auto-recycling on: Dustin keeps it under ${bytes(m.q)}`
    : t.alert
      ? 'Red Alert: new files will probably not arrive'
      : t.warn
        ? 'Getting full. Free some space soon.'
        : 'Auto-recycling is off';
  return h(
    'button',
    { class: 'tank', type: 'button', onclick: () => go('/room'), title: 'Open the Cleaning Room' },
    h('span', { class: 'top' }, h('span', null, 'Tank'), h('span', null, h('b', null, bytes(m.u)), ` of ${bytes(m.q)}`)),
    g,
    h('span', { class: 'note' }, note),
  );
}

function paintRail() {
  const rail = $('.rail');
  if (!rail) return;
  const me = S.me;
  const route = parseRoute();
  const head = h(
    'header',
    { class: 'me' },
    h(
      'button',
      { class: 'who', type: 'button', onclick: () => callsignDialog(), title: 'Your callsign' },
      avatar(me.id),
      h('span', { class: 'grow' }, h('span', { class: 'callsign' }, me.id), h('span', { class: 'name' }, me.n || 'Add your name in Settings')),
    ),
    iconBtn('settings', 'Settings', () => go('/settings')),
  );
  const actions = h(
    'div',
    { class: 'actions' },
    h('button', { class: 'btn primary', type: 'button', onclick: () => knockDialog() }, icon('door', 'i'), 'Knock'),
    h('button', { class: 'btn', type: 'button', onclick: () => crewDialog() }, icon('crew', 'i'), 'New crew'),
  );
  const list = h('div', { class: 'list' });
  const invites = [...S.convs.values()].filter((c) => c.me === 'i');
  if (S.rin.length || invites.length) {
    list.append(h('h2', null, 'At your door'));
    for (const k of S.rin) list.append(knockCard(k));
    for (const c of invites) list.append(inviteCard(c));
  }
  const convs = [...S.convs.values()].filter((c) => c.me === 'j').sort((a, b) => (b.l?.t || b.t || 0) - (a.l?.t || a.t || 0));
  if (convs.length) {
    if (S.rin.length || invites.length) list.append(h('h2', null, 'Lines and crews'));
    for (const c of convs) list.append(convItem(c, route.c === c.c));
  } else if (!S.rin.length && !invites.length) {
    list.append(
      h(
        'div',
        { class: 'empty' },
        h('p', null, 'Nobody here yet. Knock on someone’s callsign to open a Line, or share yours so they can knock on you.'),
        h('div', { class: 'row' }, flags(me.id), h('span', { class: 'callsign' }, me.id)),
        h('button', { class: 'btn', type: 'button', onclick: () => callsignDialog() }, icon('share', 'i'), 'Share my callsign'),
      ),
    );
  }
  if (S.rout.length) {
    list.append(h('h2', null, 'Waiting at their door'));
    for (const k of S.rout) {
      list.append(
        h(
          'div',
          { class: 'waiting' },
          avatar(k.to, true),
          h('span', { class: 'grow' }, h('span', { class: 'callsign' }, k.to), ` knocked at ${when(k.t)}`),
          h('button', { class: 'btn small ghost', type: 'button', onclick: () => rpc('knock_cancel', { r: k.r }).catch((e) => toast(e.message)) }, 'Cancel'),
        ),
      );
    }
  }
  const place = (path, ic, label) =>
    h('a', { href: `#${path}`, 'aria-current': route.path === path ? 'true' : null }, icon(ic), label);
  const nav = h(
    'nav',
    { class: 'places', 'aria-label': 'Places' },
    place('/stash', 'stash', 'Stash'),
    place('/room', 'room', 'Cleaning Room'),
    me.adm ? place('/tower', 'tower', 'Control Tower') : null,
  );
  const keep = rail.querySelector('.list')?.scrollTop || 0;
  rail.replaceChildren(head, tankEl(), actions, list, nav);
  list.scrollTop = keep;
}

function convItem(c, current) {
  const last = c.l;
  let line = '';
  if (last) {
    const who = last.f === S.me.id ? 'You: ' : c.g ? `${displayName(last.f, c)}: ` : '';
    line = who + (last.x || (last.k ? `${last.k} file${last.k > 1 ? 's' : ''}` : ''));
  } else line = c.g ? `${c.m?.filter((m) => m.s === 'j').length || 1} aboard` : 'Line open. Say hello.';
  return h(
    'a',
    { class: 'item', href: `#/l/${c.c}`, 'aria-current': current ? 'true' : null },
    c.g ? crewAvatar(c) : avatar(convOther(c) || ''),
    h(
      'span',
      { class: 'txt' },
      h('span', { class: 't1' }, h('b', null, convTitle(c)), h('time', null, when(last?.t || c.t))),
      h('span', { class: 't2' }, h('span', { class: 'snip' }, line), c.u ? h('span', { class: 'badge' }, c.u > 99 ? '99+' : c.u) : null),
    ),
  );
}

function knockCard(k) {
  const answer = (a) => async () => {
    try {
      const r = await rpc('answer', { r: k.r, a });
      if (r?.c) go(`/l/${r.c}`);
    } catch (e) {
      toast(e.message);
    }
  };
  return h(
    'div',
    { class: 'knock-card' },
    h('div', { class: 'row' }, avatar(k.f), h('div', { class: 'grow' }, h('b', null, k.n || 'Someone'), h('div', { class: 'callsign' }, k.f))),
    k.x ? h('p', { class: 'note' }, k.x) : null,
    h('span', { class: 'meaning' }, flags('K'), `“${MEANING.K}”`),
    h(
      'div',
      { class: 'row' },
      h('button', { class: 'btn primary small', type: 'button', onclick: answer('accept') }, flags('C'), 'Open the door'),
      h('button', { class: 'btn small', type: 'button', onclick: answer('decline'), title: 'They are not told' }, flags('N'), 'Ignore'),
      h('button', { class: 'btn small ghost danger', type: 'button', onclick: answer('block'), title: 'They can never knock again, and are not told' }, 'Bolt'),
    ),
  );
}

function inviteCard(c) {
  const answer = (ok) => async () => {
    try {
      await rpc('crew_answer', { c: c.c, ok });
      if (ok) go(`/l/${c.c}`);
    } catch (e) {
      toast(e.message);
    }
  };
  return h(
    'div',
    { class: 'knock-card' },
    h('div', { class: 'row' }, avatar(c.by), h('div', { class: 'grow' }, h('b', null, c.n), h('div', { class: 'hint' }, `Crew invite from ${c.byn || c.by} · ${c.cnt} aboard`))),
    h(
      'div',
      { class: 'row' },
      h('button', { class: 'btn primary small', type: 'button', onclick: answer(true) }, flags('C'), 'Join the crew'),
      h('button', { class: 'btn small', type: 'button', onclick: answer(false) }, flags('N'), 'Ignore'),
    ),
  );
}

// ============================================================================================
// Routing
// ============================================================================================

function parseRoute() {
  const raw = decodeURIComponent(location.hash.slice(1));
  if (raw.startsWith('/l/')) return { view: 'line', c: raw.slice(3), path: raw };
  if (['/stash', '/room', '/settings', '/tower'].includes(raw)) return { view: raw.slice(1), path: raw };
  return { view: 'home', path: '' };
}
function go(path) {
  const target = path ? `#${path}` : '#';
  if (location.hash !== target) location.hash = target;
  else routeChanged();
}

function routeChanged() {
  if (!S.me) return;
  const route = parseRoute();
  const shell = $('.shell');
  shell.classList.toggle('open', route.view !== 'home');
  const pane = $('.pane');
  S.view?.leave?.();
  S.view = null;
  pane.replaceChildren();
  if (route.view === 'line') {
    const c = S.convs.get(route.c);
    if (!c || c.me !== 'j') return go('');
    S.view = lineView(pane, c);
  } else if (route.view === 'stash') S.view = stashView(pane);
  else if (route.view === 'room') S.view = roomView(pane);
  else if (route.view === 'settings') S.view = settingsView(pane);
  else if (route.view === 'tower' && S.me.adm) S.view = towerView(pane);
  else pane.append(homePane());
  paint();
}

function homePane() {
  return h(
    'div',
    { class: 'scroll home' },
    h(
      'div',
      { class: 'home-inner' },
      h('div', { class: 'halyard' }, ...['E', 'S', 'C'].map((c) => flagNode(c))),
      h('p', null, 'Pick a Line or a Crew on the left. Or knock on a callsign to start something new.'),
    ),
  );
}
function flagNode(ch) {
  const t = document.createElement('template');
  t.innerHTML = flagSVG(ch);
  return t.content.firstChild;
}
function back() {
  return h('button', { class: 'icon-btn back', type: 'button', 'aria-label': 'Back', onclick: () => go('') }, icon('back'));
}
function bar(...kids) {
  return h('header', { class: 'bar' }, back(), ...kids);
}

// ============================================================================================
// A Line or a Crew
// ============================================================================================

function addMessage(m) {
  const box = S.msgs.get(m.c);
  if (box && !box.list.some((x) => x.id === m.id)) {
    const tmp = m.k ? box.list.findIndex((x) => x.k === m.k) : -1;
    if (tmp >= 0) box.list.splice(tmp, 1, m);
    else {
      box.list.push(m);
      box.list.sort((a, b) => a.t - b.t);
    }
    if (S.view?.c === m.c) S.view.render();
  }
  const c = S.convs.get(m.c);
  if (!c) return;
  c.l = { t: m.t, f: m.f, x: (m.x || '').slice(0, 120), k: m.a?.length || 0 };
  c.t = m.t;
  if (m.f !== S.me.id) {
    const looking = S.view?.c === m.c && document.visibilityState === 'visible';
    if (looking) markRead(c, m.t);
    else {
      c.u = (c.u || 0) + 1;
      notify(convTitle(c), m.x || `${m.a?.length || 0} file(s)`, m.c);
    }
  }
}

let readTimer;
function markRead(c, t) {
  if (t <= (c.rd || 0)) return;
  c.rd = t;
  c.u = 0;
  clearTimeout(readTimer);
  readTimer = setTimeout(() => rpc('read', { c: c.c, t }).catch(() => {}), 600);
}

function notify(title, body, tag) {
  if (document.visibilityState === 'visible' || !('Notification' in window) || Notification.permission !== 'granted') return;
  try {
    const n = new Notification(title, { body: body.slice(0, 140), tag, icon: '/icon.svg' });
    n.onclick = () => {
      window.focus();
      if (tag && tag !== 'knock') go(`/l/${tag}`);
      n.close();
    };
  } catch {
    /* some platforms only allow notifications from a service worker */
  }
}

async function loadHistory(c, older) {
  let box = S.msgs.get(c);
  if (!box) S.msgs.set(c, (box = { list: [], more: true, loading: false }));
  if (box.loading || (older && !box.more)) return;
  box.loading = true;
  try {
    const before = older && box.list.length ? box.list[0].t : undefined;
    const r = await rpc('hist', { c, b: before, n: 60 });
    const known = new Set(box.list.map((m) => m.id));
    box.list = [...r.m.filter((m) => !known.has(m.id)), ...box.list].sort((a, b) => a.t - b.t);
    box.more = r.more;
  } finally {
    box.loading = false;
  }
}

function linkify(text) {
  const out = [];
  const re = /\bhttps?:\/\/[^\s<>"]+[^\s<>".,;:!?)\]'’]/g;
  let last = 0;
  for (const m of text.matchAll(re)) {
    if (m.index > last) out.push(text.slice(last, m.index));
    out.push(h('a', { href: m[0], target: '_blank', rel: 'noopener noreferrer' }, m[0]));
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

const fileUrl = (a, which = 'o') => `/f/${a.b}/${which}/${encodeURIComponent(a.n)}`;

function attachmentsEl(m, conv) {
  const wrap = h('div', { class: `atts${m.a.length === 1 ? ' one' : ''}` });
  const visuals = m.a.filter((a) => a.s === 'ok' && a.th && ['image', 'video'].includes(kindOf(a.m, a.n)));
  for (const a of m.a) {
    const kind = kindOf(a.m, a.n);
    if (m.pending && a.local) {
      wrap.append(h('div', { class: 'thumb' }, a.local ? h('img', { src: a.local, alt: '' }) : null));
      continue;
    }
    if (a.s !== 'ok') {
      wrap.append(goneCard(a));
      continue;
    }
    if (a.th && (kind === 'image' || kind === 'video')) {
      const img = h('img', { src: fileUrl(a, 't'), alt: a.n, loading: 'lazy', decoding: 'async' });
      if (a.w && a.h) {
        img.width = a.w;
        img.height = a.h;
      }
      wrap.append(
        h(
          'button',
          {
            class: 'thumb',
            type: 'button',
            title: a.n,
            onclick: () => (kind === 'video' ? window.open(fileUrl(a), '_blank', 'noopener') : peek(visuals.filter((v) => kindOf(v.m, v.n) === 'image'), a.b, conv)),
          },
          img,
          kind === 'video' ? h('span', { class: 'play' }, icon('play')) : null,
        ),
      );
    } else wrap.append(fileCard(a));
  }
  return wrap;
}

function fileCard(a) {
  const kind = kindOf(a.m, a.n);
  const opens = kind === 'video' || kind === 'audio' || a.m === 'application/pdf' || a.m === 'text/plain' || kind === 'image';
  return h(
    'a',
    { class: 'filecard', href: opens ? fileUrl(a) : `${fileUrl(a)}?dl=1`, target: opens ? '_blank' : null, rel: 'noopener' },
    h('span', { class: 'ext' }, ext(a.n)),
    h('span', { class: 'info' }, h('b', null, a.n), h('small', null, bytes(a.cz || a.z))),
  );
}

function goneCard(a) {
  const label = { ref: 'Bounced: your Tank was full', rec: 'Recycled by Dustin', del: 'Deleted by you', gone: 'Not yours to open' }[a.s] || 'Unavailable';
  const action = a.s === 'ref' ? 'Catch it' : a.s === 'rec' || a.s === 'del' ? 'Rescue' : null;
  return h(
    'div',
    { class: 'filecard gone' },
    h('span', { class: 'ext' }, ext(a.n)),
    h('span', { class: 'info' }, h('b', null, a.n), h('small', null, `${label} · ${bytes(a.cz || a.z)}`)),
    action
      ? h(
          'button',
          {
            class: 'btn small',
            type: 'button',
            onclick: async () => {
              const was = a.s;
              try {
                await rpc('file_op', { o: 'get', b: [a.b] });
                setAttState(a.b, 'ok');
                toast(was === 'ref' ? 'Caught it.' : 'Rescued.');
              } catch (e) {
                toast(e.code === 410 ? 'Nobody holds that file any more.' : e.message);
              }
            },
          },
          action,
        )
      : null,
  );
}

function setAttState(b, s) {
  for (const box of S.msgs.values()) for (const m of box.list) for (const a of m.a || []) if (a.b === b) a.s = s;
  S.view?.render?.();
}

async function refreshFileStates() {
  if (!S.msgs.size) return;
  try {
    const r = await rpc('files');
    S.files = r;
    const st = new Map(r.f.map((f) => [f.b, f.s]));
    let changed = false;
    for (const box of S.msgs.values())
      for (const m of box.list)
        for (const a of m.a || []) {
          const s = st.get(a.b) || 'gone';
          if (a.s !== s && !m.pending) {
            a.s = s;
            changed = true;
          }
        }
    if (changed) S.view?.render?.();
  } catch {
    /* offline: next hello refreshes */
  }
}

function lineView(pane, conv) {
  const c = conv.c;
  const head = h('header', { class: 'bar' });
  const list = h('div', { class: 'msgs', role: 'log', 'aria-live': 'polite' });
  const tray = h('div', { class: 'tray', hidden: true });
  const traySum = h('div', { class: 'tray-sum', hidden: true });
  const ta = h('textarea', { rows: 1, placeholder: 'Message', 'aria-label': 'Message', maxlength: S.tmax });
  const picker = h('input', { type: 'file', multiple: true, hidden: true });
  const items = [];
  let atBottom = true;

  function header() {
    const cv = S.convs.get(c) || conv;
    const other = convOther(cv);
    const joined = cv.m?.filter((m) => m.s === 'j').length || 0;
    const invited = cv.m?.filter((m) => m.s === 'i').length || 0;
    head.replaceChildren(
      back(),
      cv.g ? crewAvatar(cv) : avatar(other || ''),
      h(
        'div',
        { class: 'title' },
        h('b', null, convTitle(cv)),
        h('small', null, cv.g ? `Crew · ${joined} aboard${invited ? `, ${invited} invited` : ''}` : h('span', { class: 'callsign' }, other || '')),
      ),
      iconBtn('info', cv.g ? 'Crew details' : 'Line details', () => (cv.g ? crewInfo(cv) : lineInfo(cv))),
    );
  }

  function render() {
    const box = S.msgs.get(c);
    if (!box) return;
    const keepFromBottom = list.scrollHeight - list.scrollTop;
    const cv = S.convs.get(c) || conv;
    list.replaceChildren();
    if (box.more) list.append(h('button', { class: 'btn small ghost more', type: 'button', onclick: () => older() }, 'Show earlier messages'));
    if (!box.list.length) list.append(h('div', { class: 'day' }, cv.g ? 'This crew is quiet. Say something.' : 'The door is open. Say hello.'));
    let prev = null;
    for (const m of box.list) {
      if (!prev || !sameDay(prev.t, m.t)) list.append(h('div', { class: 'day' }, fDay.format(new Date(m.t))));
      list.append(msgEl(m, prev && sameDay(prev.t, m.t) ? prev : null, cv));
      prev = m;
    }
    if (atBottom) list.scrollTop = list.scrollHeight;
    else list.scrollTop = list.scrollHeight - keepFromBottom;
    const last = box.list[box.list.length - 1];
    if (last && document.visibilityState === 'visible') markRead(cv, last.t);
  }

  function msgEl(m, prev, cv) {
    const mine = m.f === S.me.id;
    const cont = prev && prev.f === m.f && m.t - prev.t < 5 * 6e4;
    const el = h('div', { class: `msg${mine ? ' mine' : ''}${cont ? ' cont' : ' group-start'}` });
    if (!mine && cv.g) el.append(cont ? h('span', { class: 'avatar small ghost' }) : avatar(m.f, true));
    const b = h('div', { class: 'bubble' });
    if (!mine && cv.g && !cont) b.append(h('span', { class: 'from' }, displayName(m.f, cv)));
    if (m.a?.length) b.append(attachmentsEl(m, cv));
    if (m.x) b.append(h('div', { class: 'text' }, linkify(m.x)));
    let meta;
    if (m.pending) {
      meta = m.failed
        ? h('button', { class: 'btn small danger', type: 'button', onclick: () => retry(m) }, 'Not sent. Tap to retry')
        : h('span', { class: 'meta sending' }, m.progress != null ? `Sending ${Math.round(m.progress * 100)}%` : 'Sending…');
    } else meta = h('time', { class: 'meta', datetime: new Date(m.t).toISOString(), title: fFull.format(new Date(m.t)) }, fClock.format(new Date(m.t)));
    b.append(meta);
    el.append(b);
    return el;
  }

  async function older() {
    const before = list.scrollHeight;
    await loadHistory(c, true);
    atBottom = false;
    render();
    list.scrollTop = list.scrollHeight - before;
  }

  list.addEventListener('scroll', () => {
    atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 60;
    if (list.scrollTop < 40 && S.msgs.get(c)?.more && !S.msgs.get(c)?.loading) older();
  });

  // ---- composer & tray ----
  function addFiles(files) {
    for (const f of files) {
      const kind = kindOf(f.type, f.name);
      items.push({ file: f, url: kind === 'image' || kind === 'video' ? URL.createObjectURL(f) : null, kind });
    }
    paintTray();
  }
  function paintTray() {
    tray.hidden = traySum.hidden = !items.length;
    tray.replaceChildren(
      ...items.map((it, i) =>
        h(
          'div',
          { class: 'chip' },
          h('div', { class: 'pv' }, it.kind === 'image' && it.url ? h('img', { src: it.url, alt: '' }) : ext(it.file.name)),
          h('span', null, it.file.name),
          h('span', null, bytes(it.file.size)),
          h(
            'button',
            {
              class: 'x',
              type: 'button',
              'aria-label': `Remove ${it.file.name}`,
              onclick: () => {
                items.splice(i, 1);
                if (it.url) URL.revokeObjectURL(it.url);
                paintTray();
              },
            },
            '×',
          ),
        ),
      ),
    );
    const total = items.reduce((s, it) => s + it.file.size, 0);
    const m = S.me;
    const room = m.ar ? m.q - m.pin : m.q - m.u;
    traySum.textContent = `${items.length} file${items.length > 1 ? 's' : ''}, ${bytes(total)}.` + (total > room ? ' This is more than your Tank has room for.' : m.ar && m.u + total > m.q ? ' Dustin will recycle your oldest files to make room.' : '');
  }
  picker.addEventListener('change', () => {
    addFiles(picker.files);
    picker.value = '';
  });
  ta.addEventListener('input', () => {
    ta.style.height = 'auto';
    ta.style.height = `${Math.min(ta.scrollHeight, innerHeight * 0.4)}px`;
  });
  const coarse = matchMedia('(pointer: coarse)').matches;
  ta.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey && !coarse && !e.isComposing) {
      e.preventDefault();
      sendNow();
    }
  });
  ta.addEventListener('paste', (e) => {
    const fs = [...(e.clipboardData?.files || [])];
    if (fs.length) {
      e.preventDefault();
      addFiles(fs);
    }
  });
  let dragDepth = 0;
  const dropHint = h('div', { class: 'drop', hidden: true }, 'Drop to add to the tray');
  pane.addEventListener('dragenter', (e) => {
    if (![...(e.dataTransfer?.types || [])].includes('Files')) return;
    dragDepth++;
    dropHint.hidden = false;
  });
  pane.addEventListener('dragleave', () => {
    if (--dragDepth <= 0) {
      dragDepth = 0;
      dropHint.hidden = true;
    }
  });
  pane.addEventListener('dragover', (e) => e.preventDefault());
  pane.addEventListener('drop', (e) => {
    e.preventDefault();
    dragDepth = 0;
    dropHint.hidden = true;
    if (e.dataTransfer?.files?.length) addFiles(e.dataTransfer.files);
  });

  async function sendNow() {
    const text = ta.value;
    if (!text.trim() && !items.length) return;
    const sending = items.splice(0);
    ta.value = '';
    ta.style.height = 'auto';
    paintTray();
    const k = Math.floor(Math.random() * 2 ** 52);
    const box = S.msgs.get(c);
    const tmp = {
      id: `tmp-${k}`,
      k,
      c,
      f: S.me.id,
      t: Date.now() + S.skew,
      x: text,
      a: sending.map((it) => ({ n: it.file.name, m: it.file.type, z: it.file.size, s: 'ok', local: it.kind === 'image' ? it.url : null })),
      pending: true,
      items: sending,
    };
    box.list.push(tmp);
    atBottom = true;
    render();
    await deliver(tmp);
  }

  async function deliver(tmp) {
    tmp.failed = false;
    render();
    try {
      const metas = [];
      const total = tmp.items.reduce((s, it) => s + it.file.size, 0) || 1;
      let done = 0;
      for (const it of tmp.items) {
        if (!it.meta) {
          it.meta = await uploadFile(it.file, (p) => {
            tmp.progress = (done + p * it.file.size) / total;
            render();
          });
        }
        done += it.file.size;
        metas.push(it.meta);
      }
      tmp.progress = null;
      const r = await rpc('send', { c, x: tmp.x, a: metas, k: tmp.k });
      const box = S.msgs.get(c);
      const i = box.list.indexOf(tmp);
      if (box.list.some((m) => m.id === r.id)) {
        if (i >= 0) box.list.splice(i, 1);
      } else if (i >= 0) Object.assign(tmp, { id: r.id, t: r.t, pending: false });
      for (const it of tmp.items) if (it.url) setTimeout(() => URL.revokeObjectURL(it.url), 30000);
      render();
      if (metas.length) refreshFileStates();
    } catch (e) {
      tmp.failed = true;
      render();
      toast(e.message);
    }
  }
  function retry(m) {
    deliver(m);
  }

  const send = h('button', { class: 'btn primary', type: 'button', 'aria-label': 'Send', onclick: sendNow }, icon('send', 'i'));
  const composer = h('form', { class: 'composer', onsubmit: (e) => (e.preventDefault(), sendNow()) }, iconBtn('clip', 'Attach files', () => picker.click()), ta, send, picker);
  header();
  pane.append(head, list, tray, traySum, composer, dropHint);

  const view = {
    c,
    header,
    render,
    update(kind) {
      if (kind === 'conv') header();
      if (kind === 'me' && items.length) paintTray();
    },
    leave() {
      for (const it of items) if (it.url) URL.revokeObjectURL(it.url);
    },
  };
  if (S.msgs.has(c)) render();
  else {
    list.append(h('div', { class: 'day' }, 'Loading…'));
    loadHistory(c, false).then(render, (e) => toast(e.message));
  }
  if (!coarse) ta.focus();
  return view;
}

// ============================================================================================
// Uploads: chunked, resumable, previews made by this browser's own codecs
// ============================================================================================

const CHUNK = 8 * MB;

function xhrPut(url, body, onp) {
  return new Promise((ok, no) => {
    const x = new XMLHttpRequest();
    x.open('PUT', url);
    x.responseType = 'json';
    x.upload.onprogress = (e) => onp(e.loaded);
    x.onload = () =>
      x.status === 200 ? ok(x.response) : no(Object.assign(new Error(x.response?.e || `Upload failed (${x.status})`), { status: x.status, data: x.response }));
    x.onerror = () => no(Object.assign(new Error('Network error while uploading'), { status: 0 }));
    x.send(body);
  });
}

async function uploadBlob(blob, onp) {
  const { u } = await api('/api/up', { z: blob.size });
  let o = 0;
  let tries = 0;
  for (;;) {
    const end = Math.min(blob.size, o + CHUNK);
    try {
      const r = await xhrPut(`/api/up/${u}?o=${o}`, blob.slice(o, end), (n) => onp((o + n) / Math.max(1, blob.size)));
      if (r.b) {
        onp(1);
        return r;
      }
      o = r.o;
      tries = 0;
    } catch (e) {
      if (e.status === 409 && e.data?.o != null) {
        o = e.data.o;
        continue;
      }
      if (e.status === 404 || e.status === 507 || ++tries > 6) throw e;
      await sleep(800 * tries);
      try {
        o = (await api(`/api/up/${u}`, null, 'GET')).o;
      } catch {
        /* keep the old offset */
      }
    }
  }
}

async function decodeImage(file) {
  try {
    const b = await createImageBitmap(file);
    return { src: b, w: b.width, h: b.height, done: () => b.close() };
  } catch {
    const url = URL.createObjectURL(file);
    try {
      const img = new Image();
      img.src = url;
      await img.decode();
      return { src: img, w: img.naturalWidth, h: img.naturalHeight, done: () => URL.revokeObjectURL(url) };
    } catch {
      URL.revokeObjectURL(url);
      return null;
    }
  }
}

async function encodeScaled(src, w, hh, max, q) {
  const s = Math.min(1, max / Math.max(w, hh));
  const W = Math.max(1, Math.round(w * s)), H = Math.max(1, Math.round(hh * s));
  const canvas = typeof OffscreenCanvas !== 'undefined' ? new OffscreenCanvas(W, H) : Object.assign(document.createElement('canvas'), { width: W, height: H });
  const g = canvas.getContext('2d');
  g.imageSmoothingQuality = 'high';
  g.drawImage(src, 0, 0, W, H);
  const out = (type) => (canvas.convertToBlob ? canvas.convertToBlob({ type, quality: q }) : new Promise((res) => canvas.toBlob(res, type, q)));
  let blob = await out('image/webp');
  if (!blob || blob.type !== 'image/webp') blob = await out('image/jpeg');
  return blob;
}

function videoPoster(file) {
  return new Promise((resolve) => {
    const url = URL.createObjectURL(file);
    const v = document.createElement('video');
    let settled = false;
    const done = (x) => {
      if (settled) return;
      settled = true;
      URL.revokeObjectURL(url);
      resolve(x);
    };
    v.muted = true;
    v.playsInline = true;
    v.preload = 'auto';
    v.onerror = () => done(null);
    v.onloadeddata = () => {
      v.currentTime = Math.min(1, (v.duration || 3) / 3);
    };
    v.onseeked = async () => {
      try {
        const thumb = await encodeScaled(v, v.videoWidth, v.videoHeight, 400, 0.78);
        done({ w: v.videoWidth, h: v.videoHeight, thumb, preview: null });
      } catch {
        done(null);
      }
    };
    v.src = url;
    setTimeout(() => done(null), 8000);
  });
}

async function previews(file) {
  const kind = kindOf(file.type, file.name);
  if (kind === 'video') return videoPoster(file);
  if (kind !== 'image') return null;
  const d = await decodeImage(file);
  if (!d) return null; // this browser can't decode it: the file goes as-is
  try {
    const thumb = await encodeScaled(d.src, d.w, d.h, 400, 0.78);
    const webSafe = /^image\/(png|jpeg|gif|webp|avif)$/.test(file.type);
    const preview = !webSafe || file.size > 4 * MB || Math.max(d.w, d.h) > 4096 ? await encodeScaled(d.src, d.w, d.h, 2048, 0.86) : null;
    return { w: d.w, h: d.h, thumb, preview };
  } finally {
    d.done();
  }
}

/** Upload a file plus its previews; returns the attachment description the relay expects. */
async function uploadFile(file, onp) {
  const pv = await previews(file).catch(() => null);
  const sizes = [file.size, pv?.thumb?.size || 0, pv?.preview?.size || 0];
  const total = sizes.reduce((a, b) => a + b, 0) || 1;
  let base = 0;
  const step = (n) => (p) => onp(Math.min(1, (base + p * n) / total));
  const meta = { n: file.name || 'file', m: file.type || 'application/octet-stream', z: file.size };
  const o = await uploadBlob(file, step(file.size));
  meta.b = o.b;
  base += file.size;
  if (pv?.thumb) {
    const t = await uploadBlob(pv.thumb, step(pv.thumb.size));
    meta.t = t.b;
    meta.tz = t.z;
    base += pv.thumb.size;
  }
  if (pv?.preview) {
    const p = await uploadBlob(pv.preview, step(pv.preview.size));
    meta.p = p.b;
    meta.pz = p.z;
  }
  if (pv) {
    meta.w = pv.w;
    meta.h = pv.h;
  }
  return meta;
}

// ============================================================================================
// Peek: the image viewer
// ============================================================================================

function peek(list, startB, conv) {
  if (!list.length) return;
  let i = Math.max(0, list.findIndex((a) => a.b === startB));
  let zoom = 1, tx = 0, ty = 0;
  const img = h('img', { alt: '' });
  const note = h('div', { class: 'peek-note', hidden: true });
  const title = h('div', { class: 'title' });
  const keepBtn = iconBtn('anchor', 'Make it a Keeper', async () => {
    const a = list[i];
    const f = S.files?.f?.find((x) => x.b === a.b);
    const on = !f?.pin;
    try {
      await rpc('file_op', { o: on ? 'pin' : 'unpin', b: [a.b] });
      toast(on ? 'Kept. Dustin will never recycle it.' : 'No longer a Keeper.');
    } catch (e) {
      toast(e.message);
    }
  });
  const dl = h('a', { class: 'icon-btn', title: 'Download', 'aria-label': 'Download' }, icon('download'));
  const stage = h('div', { class: 'peek-stage' }, img, note);
  const prev = h('button', { class: 'peek-nav prev', type: 'button', 'aria-label': 'Previous' }, icon('back'));
  const next = h('button', { class: 'peek-nav next', type: 'button', 'aria-label': 'Next' }, h('span', { class: 'flip' }, icon('send')));
  const d = h(
    'dialog',
    { class: 'peek', 'aria-label': 'Picture' },
    h('div', { class: 'peek-wrap' }, h('div', { class: 'peek-bar' }, iconBtn('close', 'Close', () => d.close()), title, keepBtn, dl), stage),
  );
  if (list.length > 1) stage.append(prev, next);
  function apply() {
    img.style.transform = `translate(${tx}px, ${ty}px) scale(${zoom})`;
    img.style.cursor = zoom > 1 ? 'grab' : 'zoom-in';
  }
  function show() {
    const a = list[i];
    zoom = 1;
    tx = ty = 0;
    apply();
    note.hidden = true;
    title.replaceChildren(h('b', null, a.n), h('small', null, `${bytes(a.z)}${a.w ? ` · ${a.w} × ${a.h}` : ''}`));
    dl.href = `${fileUrl(a)}?dl=1`;
    let triedPreview = false;
    img.onerror = () => {
      if (a.pv && !triedPreview) {
        triedPreview = true;
        img.src = fileUrl(a, 'p');
        note.textContent = 'Showing a preview: this browser can’t open the original format. Download keeps the original.';
        note.hidden = false;
      } else {
        note.textContent = 'This browser can’t show this picture. Download it instead.';
        note.hidden = false;
      }
    };
    img.src = fileUrl(a);
  }
  const step = (n) => {
    i = (i + n + list.length) % list.length;
    show();
  };
  prev.onclick = () => step(-1);
  next.onclick = () => step(1);
  d.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowLeft') step(-1);
    if (e.key === 'ArrowRight') step(1);
  });
  img.addEventListener('click', () => {
    zoom = zoom > 1 ? 1 : 2.5;
    tx = ty = 0;
    apply();
  });
  let drag = null;
  stage.addEventListener('pointerdown', (e) => {
    drag = { x: e.clientX, y: e.clientY, tx, ty, moved: false };
    stage.setPointerCapture(e.pointerId);
  });
  stage.addEventListener('pointermove', (e) => {
    if (!drag) return;
    const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
    if (Math.abs(dx) + Math.abs(dy) > 6) drag.moved = true;
    if (zoom > 1) {
      tx = drag.tx + dx;
      ty = drag.ty + dy;
      apply();
    }
  });
  stage.addEventListener('pointerup', (e) => {
    if (drag && zoom === 1 && list.length > 1) {
      const dx = e.clientX - drag.x;
      if (Math.abs(dx) > 60) step(dx < 0 ? 1 : -1);
    }
    drag = null;
  });
  stage.addEventListener(
    'wheel',
    (e) => {
      e.preventDefault();
      zoom = Math.min(8, Math.max(1, zoom * (e.deltaY < 0 ? 1.15 : 1 / 1.15)));
      if (zoom === 1) tx = ty = 0;
      apply();
    },
    { passive: false },
  );
  d.addEventListener('close', () => d.remove());
  document.body.append(d);
  d.showModal();
  show();
}

// ============================================================================================
// Dialogs: callsign, Knock, Crew, details
// ============================================================================================

function dialog(build) {
  const d = h('dialog');
  d.addEventListener('close', () => d.remove());
  d.addEventListener('click', (e) => {
    if (e.target === d) d.close();
  });
  document.body.append(d);
  build(d);
  d.showModal();
  return d;
}

function callsignDialog() {
  const me = S.me;
  const link = `${location.origin}/#${me.id}`;
  dialog((d) =>
    d.append(
      h(
        'div',
        { class: 'body' },
        h('h2', null, 'Your callsign'),
        flags(me.id, 'big'),
        h('div', { class: 'callsign', style: null }, h('span', { class: 'callsign big-cs' }, me.id)),
        h('p', { class: 'hint' }, 'Your callsign is the only way to reach you on ESC. Share it with people you want to talk to: they knock, you decide whether to open the door.'),
        h(
          'div',
          { class: 'foot' },
          h('button', { class: 'btn', type: 'button', onclick: () => navigator.clipboard?.writeText(me.id).then(() => toast('Callsign copied.')) }, icon('copy', 'i'), 'Copy callsign'),
          h(
            'button',
            {
              class: 'btn primary',
              type: 'button',
              onclick: async () => {
                if (navigator.share) {
                  try {
                    await navigator.share({ title: 'My ESC callsign', text: `Knock on me on ESC: ${me.id}`, url: link });
                  } catch {
                    /* closed */
                  }
                } else {
                  await navigator.clipboard?.writeText(link);
                  toast('Link copied.');
                }
              },
            },
            icon('share', 'i'),
            'Share a link',
          ),
        ),
      ),
    ),
  );
}

function callsignInput(onChange) {
  const preview = flags('');
  const input = h('input', {
    type: 'text',
    class: 'callsign-in',
    maxlength: 7,
    autocomplete: 'off',
    autocapitalize: 'characters',
    spellcheck: 'false',
    placeholder: 'K7Q2',
    'aria-label': 'Callsign',
  });
  input.addEventListener('input', () => {
    const v = cleanCS(input.value);
    preview.innerHTML = [...v].map((c) => flagSVG(c)).join('');
    onChange?.(v);
  });
  return { input, preview };
}

function knockDialog(prefill = '') {
  dialog((d) => {
    const err = h('p', { class: 'err' });
    const { input, preview } = callsignInput(() => (err.textContent = ''));
    const note = h('textarea', { rows: 3, maxlength: 280, placeholder: 'Optional: a few words so they know it’s you' });
    const form = h(
      'form',
      {
        onsubmit: async (e) => {
          e.preventDefault();
          const to = cleanCS(input.value);
          if (!CS_RE.test(to)) {
            err.textContent = 'Callsigns are 4 characters from 2-9 and B C D F G H J K M N P Q R S T V W X Z.';
            return;
          }
          if (to === S.me.id) {
            err.textContent = 'That’s your own callsign.';
            return;
          }
          try {
            const r = await rpc('knock', { to, x: note.value });
            d.close();
            if (r.c) go(`/l/${r.c}`);
            else toast(`Knocked on ${to}. If they open the door, your Line appears here.`);
          } catch (ex) {
            err.textContent = ex.message;
          }
        },
      },
      h('h2', null, 'Knock on a callsign'),
      h('div', { class: 'row' }, flags('K'), h('span', { class: 'hint' }, `Flag K: “${MEANING.K}.”`)),
      h('label', { class: 'field' }, h('span', null, 'Callsign'), input),
      preview,
      h('label', { class: 'field' }, h('span', null, 'Note'), note),
      h('p', { class: 'hint' }, 'They see your callsign, your name and this note. You only hear back if they open the door.'),
      err,
      h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, 'Cancel'), h('button', { class: 'btn primary', type: 'submit' }, 'Knock')),
    );
    d.append(form);
    if (prefill) {
      input.value = prefill;
      input.dispatchEvent(new Event('input'));
    }
    setTimeout(() => input.focus(), 30);
  });
}

function chipInput(max) {
  const chips = h('div', { class: 'chips' });
  const list = [];
  const { input, preview } = callsignInput();
  const err = h('p', { class: 'err' });
  function paintChips() {
    chips.replaceChildren(
      ...list.map((cs, i) =>
        h(
          'span',
          { class: 'c' },
          flags(cs),
          cs,
          h('button', { type: 'button', 'aria-label': `Remove ${cs}`, onclick: () => (list.splice(i, 1), paintChips()) }, '×'),
        ),
      ),
    );
  }
  function take() {
    const v = cleanCS(input.value);
    if (!v) return true;
    if (!CS_RE.test(v)) {
      err.textContent = `${v} is not a callsign.`;
      return false;
    }
    if (v === S.me.id) {
      err.textContent = 'You are already aboard.';
      return false;
    }
    if (!list.includes(v)) {
      if (list.length >= max) {
        err.textContent = `A crew holds at most ${S.gmax} people.`;
        return false;
      }
      list.push(v);
    }
    input.value = '';
    preview.replaceChildren();
    err.textContent = '';
    paintChips();
    return true;
  }
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ',' || e.key === ' ') {
      e.preventDefault();
      take();
    }
  });
  return { el: h('div', { class: 'field' }, chips, input, preview, err), list, take };
}

function crewDialog() {
  dialog((d) => {
    const name = h('input', { type: 'text', maxlength: 80, placeholder: 'Family, Weekend trip, Night shift…', required: true });
    const people = chipInput(S.gmax - 1);
    const err = h('p', { class: 'err' });
    d.append(
      h(
        'form',
        {
          onsubmit: async (e) => {
            e.preventDefault();
            if (!people.take()) return;
            try {
              const r = await rpc('crew_new', { n: name.value, ids: people.list });
              d.close();
              go(`/l/${r.c}`);
              if (people.list.length) toast('Crew created. People join when they accept.');
            } catch (ex) {
              err.textContent = ex.message;
            }
          },
        },
        h('h2', null, 'New crew'),
        h('label', { class: 'field' }, h('span', null, 'Crew name'), name),
        h('label', { class: 'field' }, h('span', null, `Invite callsigns (up to ${S.gmax - 1})`)),
        people.el,
        h('p', { class: 'hint' }, 'Nobody joins until they accept your invite.'),
        err,
        h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, 'Cancel'), h('button', { class: 'btn primary', type: 'submit' }, 'Create crew')),
      ),
    );
    setTimeout(() => name.focus(), 30);
  });
}

function crewInfo(conv) {
  dialog((d) => {
    const me = conv.m.find((m) => m.a === S.me.id);
    const boss = me?.r === 'o' || me?.r === 'a';
    const rename = h('input', { type: 'text', value: conv.n, maxlength: 80 });
    const people = chipInput(S.gmax - conv.m.length);
    const err = h('p', { class: 'err' });
    const roleName = { o: 'Owner', a: 'Admin', m: '' };
    const members = conv.m.map((m) =>
      h(
        'div',
        { class: 'member' },
        avatar(m.a, true),
        h('div', { class: 'txt' }, h('b', null, m.a === S.me.id ? 'You' : displayName(m.a, conv)), h('small', null, h('span', { class: 'callsign' }, m.a), m.s === 'i' ? ' · invited' : roleName[m.r] ? ` · ${roleName[m.r]}` : '')),
        boss && m.a !== S.me.id && m.r !== 'o'
          ? h(
              'button',
              {
                class: 'btn small ghost danger',
                type: 'button',
                onclick: async () => {
                  try {
                    await rpc('crew_remove', { c: conv.c, a: m.a });
                    d.close();
                  } catch (ex) {
                    err.textContent = ex.message;
                  }
                },
              },
              m.s === 'i' ? 'Cancel invite' : 'Remove',
            )
          : null,
        me?.r === 'o' && m.a !== S.me.id && m.s === 'j'
          ? h('button', { class: 'btn small ghost', type: 'button', onclick: () => rpc('crew_set', { c: conv.c, a: m.a, admin: m.r !== 'a' }).then(() => d.close(), (ex) => (err.textContent = ex.message)) }, m.r === 'a' ? 'Remove admin' : 'Make admin')
          : null,
      ),
    );
    d.append(
      h(
        'div',
        { class: 'body' },
        h('h2', null, conv.n),
        boss
          ? h(
              'form',
              { class: 'row', onsubmit: (e) => (e.preventDefault(), rpc('crew_set', { c: conv.c, n: rename.value }).then(() => toast('Renamed.'), (ex) => (err.textContent = ex.message))) },
              h('span', { class: 'grow' }, rename),
              h('button', { class: 'btn', type: 'submit' }, 'Rename'),
            )
          : null,
        h('div', null, ...members),
        h('label', { class: 'field' }, h('span', null, 'Invite more callsigns')),
        people.el,
        err,
        h(
          'div',
          { class: 'foot' },
          h(
            'button',
            {
              class: 'btn danger',
              type: 'button',
              onclick: async () => {
                if (!confirm(`Leave ${conv.n}? You stop getting its messages.`)) return;
                try {
                  await rpc('crew_leave', { c: conv.c });
                  d.close();
                  go('');
                } catch (ex) {
                  err.textContent = ex.message;
                }
              },
            },
            'Leave crew',
          ),
          h(
            'button',
            {
              class: 'btn primary',
              type: 'button',
              onclick: async () => {
                if (!people.take() || !people.list.length) return;
                try {
                  await rpc('crew_invite', { c: conv.c, ids: people.list });
                  toast('Invited. They join when they accept.');
                  d.close();
                } catch (ex) {
                  err.textContent = ex.message;
                }
              },
            },
            'Send invites',
          ),
        ),
      ),
    );
  });
}

function lineInfo(conv) {
  const other = convOther(conv);
  dialog((d) => {
    const nick = h('input', { type: 'text', maxlength: 40, value: S.me.nick?.[other] || '', placeholder: conv.m.find((m) => m.a === other)?.n || 'Nickname' });
    d.append(
      h(
        'div',
        { class: 'body' },
        h('h2', null, convTitle(conv)),
        flags(other, 'big'),
        h('span', { class: 'callsign' }, other),
        h(
          'form',
          { class: 'field', onsubmit: (e) => (e.preventDefault(), rpc('nick', { a: other, n: nick.value }).then(() => (toast('Nickname saved.'), d.close()), (ex) => toast(ex.message))) },
          h('span', null, 'Your private nickname for them'),
          h('div', { class: 'row' }, h('span', { class: 'grow' }, nick), h('button', { class: 'btn', type: 'submit' }, 'Save')),
        ),
        h('p', { class: 'hint' }, 'Only you see nicknames.'),
        h(
          'div',
          { class: 'foot' },
          h(
            'button',
            {
              class: 'btn danger',
              type: 'button',
              onclick: async () => {
                if (!confirm(`Bolt ${other}? They can never knock on you again. They are not told.`)) return;
                await rpc('block', { a: other, on: true }).catch((ex) => toast(ex.message));
                toast(`${other} is bolted out.`);
                d.close();
              },
            },
            'Bolt this callsign',
          ),
          h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, 'Close'),
        ),
      ),
    );
  });
}

// ============================================================================================
// Stash and the Cleaning Room
// ============================================================================================

async function getFiles(force) {
  if (!S.files || force) S.files = await rpc('files');
  return S.files;
}
const placeName = (f) => {
  if (f.d === 'drive') return 'Stash';
  const c = f.c?.length ? S.convs.get(f.c[0]) : null;
  return c ? convTitle(c) : f.d === 'sent' ? 'Sent' : 'Received';
};

function stashView(pane) {
  const grid = h('div', { class: 'grid' });
  const filter = h('select', { 'aria-label': 'Show' }, h('option', { value: 'all' }, 'All my files'), h('option', { value: 'drive' }, 'My uploads'), h('option', { value: 'sent' }, 'Sent'), h('option', { value: 'recv' }, 'Received'));
  const sort = sortSelect();
  const search = h('input', { type: 'search', placeholder: 'Search names', 'aria-label': 'Search names' });
  const picker = h('input', { type: 'file', multiple: true, hidden: true });
  const progress = h('span', { class: 'hint' });
  picker.addEventListener('change', async () => {
    const files = [...picker.files];
    picker.value = '';
    for (const [n, f] of files.entries()) {
      try {
        const meta = await uploadFile(f, (p) => (progress.textContent = `Uploading ${n + 1} of ${files.length}: ${Math.round(p * 100)}%`));
        await rpc('stash_add', { f: meta });
      } catch (e) {
        toast(`${f.name}: ${e.message}`);
      }
    }
    progress.textContent = '';
    await getFiles(true);
    paintGrid();
  });
  function paintGrid() {
    const fs = filterSort(S.files?.f || [], { show: filter.value, sort: sort.value, q: search.value, onlyActive: true });
    grid.replaceChildren(
      ...(fs.length
        ? fs.map((f) => tile(f))
        : [h('div', { class: 'empty' }, h('p', null, 'Nothing here yet. Upload to your Stash, or send files in a Line or Crew.'))]),
    );
  }
  function tile(f) {
    const kind = kindOf(f.m, f.n);
    const face = h('button', {
      class: 'face',
      type: 'button',
      title: f.n,
      onclick: () => (kind === 'image' ? peek(fs2peek(), f.b) : window.open(kind === 'video' || kind === 'audio' || f.m === 'application/pdf' ? fileUrl(f) : `${fileUrl(f)}?dl=1`, '_blank', 'noopener')),
    });
    face.append(f.th ? h('img', { src: fileUrl(f, 't'), alt: '', loading: 'lazy' }) : ext(f.n));
    if (f.pin) face.append(h('span', { class: 'keep keep-mark' }, 'Keeper'));
    return h(
      'div',
      { class: 'tile' },
      face,
      h(
        'div',
        { class: 'cap' },
        h('div', null, h('b', null, f.n), h('small', null, `${bytes(f.cz)} · ${when(f.t)} · ${placeName(f)}`)),
        iconBtn('anchor', f.pin ? 'Remove Keeper' : 'Make it a Keeper', async () => {
          await rpc('file_op', { o: f.pin ? 'unpin' : 'pin', b: [f.b] }).catch((e) => toast(e.message));
          await getFiles(true);
          paintGrid();
        }),
        iconBtn('send', 'Send to a Line or Crew', () => sendToDialog(f)),
      ),
    );
  }
  const fs2peek = () => filterSort(S.files?.f || [], { show: filter.value, sort: sort.value, q: search.value, onlyActive: true }).filter((f) => kindOf(f.m, f.n) === 'image').map((f) => ({ ...f, z: f.z }));
  for (const el of [filter, sort, search]) el.addEventListener('input', paintGrid);
  pane.append(
    bar(h('div', { class: 'title' }, h('b', null, 'Stash'), h('small', null, 'Every file you hold: your uploads and everything sent or received'))),
    h('div', { class: 'tools' }, h('button', { class: 'btn primary', type: 'button', onclick: () => picker.click() }, icon('plus', 'i'), 'Upload to Stash'), filter, sort, search, progress, picker),
    h('div', { class: 'scroll' }, grid),
  );
  grid.append(h('div', { class: 'empty' }, 'Loading…'));
  getFiles(true).then(paintGrid, (e) => toast(e.message));
  return {
    update(kind) {
      if (kind === 'files') getFiles(true).then(paintGrid);
    },
  };
}

function sortSelect() {
  return h(
    'select',
    { 'aria-label': 'Sort' },
    h('option', { value: 'new' }, 'Newest first'),
    h('option', { value: 'old' }, 'Oldest first'),
    h('option', { value: 'big' }, 'Biggest first'),
    h('option', { value: 'small' }, 'Smallest first'),
    h('option', { value: 'name' }, 'Name A to Z'),
  );
}

function filterSort(files, o) {
  let fs = files.filter((f) => (o.onlyActive ? f.s === 'ok' : true));
  if (o.show && o.show !== 'all') {
    if (o.show.startsWith('c:')) fs = fs.filter((f) => f.c?.includes(o.show.slice(2)));
    else fs = fs.filter((f) => f.d === o.show);
  }
  if (o.kind && o.kind !== 'all') fs = fs.filter((f) => kindOf(f.m, f.n) === o.kind);
  if (o.keep === 'only') fs = fs.filter((f) => f.pin);
  if (o.keep === 'hide') fs = fs.filter((f) => !f.pin);
  if (o.q) {
    const q = o.q.toLowerCase();
    fs = fs.filter((f) => f.n.toLowerCase().includes(q));
  }
  const by = {
    new: (a, b) => b.t - a.t,
    old: (a, b) => a.t - b.t,
    big: (a, b) => b.cz - a.cz,
    small: (a, b) => a.cz - b.cz,
    name: (a, b) => a.n.localeCompare(b.n),
  }[o.sort || 'new'];
  return fs.sort(by);
}

function sendToDialog(f) {
  dialog((d) => {
    const convs = [...S.convs.values()].filter((c) => c.me === 'j');
    d.append(
      h(
        'div',
        { class: 'body' },
        h('h2', null, `Send ${f.n}`),
        h('p', { class: 'hint' }, 'It is sent from your Stash: nothing is uploaded again.'),
        h(
          'div',
          null,
          ...convs.map((c) =>
            h(
              'button',
              {
                class: 'item',
                type: 'button',
                onclick: async () => {
                  try {
                    await rpc('send', { c: c.c, x: '', a: [{ b: f.b, n: f.n, m: f.m, z: f.z }], k: Math.floor(Math.random() * 2 ** 52) });
                    d.close();
                    toast(`Sent to ${convTitle(c)}.`);
                  } catch (e) {
                    toast(e.message);
                  }
                },
              },
              c.g ? crewAvatar(c) : avatar(convOther(c) || ''),
              h('span', { class: 'txt' }, h('span', { class: 't1' }, h('b', null, convTitle(c)))),
            ),
          ),
        ),
        h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, 'Close')),
      ),
    );
  });
}

function roomView(pane) {
  const selected = new Set();
  const show = h('select', { 'aria-label': 'Where' }, h('option', { value: 'all' }, 'Everywhere'), h('option', { value: 'drive' }, 'Stash uploads'), h('option', { value: 'sent' }, 'Sent by me'), h('option', { value: 'recv' }, 'Received'));
  for (const c of [...S.convs.values()].filter((c) => c.me === 'j')) show.append(h('option', { value: `c:${c.c}` }, `In ${convTitle(c)}`));
  const kind = h('select', { 'aria-label': 'Type' }, h('option', { value: 'all' }, 'All types'), h('option', { value: 'image' }, 'Pictures'), h('option', { value: 'video' }, 'Videos'), h('option', { value: 'audio' }, 'Audio'), h('option', { value: 'doc' }, 'Documents'), h('option', { value: 'other' }, 'Other'));
  const keep = h('select', { 'aria-label': 'Keepers' }, h('option', { value: 'all' }, 'Keepers and others'), h('option', { value: 'hide' }, 'Hide Keepers'), h('option', { value: 'only' }, 'Only Keepers'));
  const sort = sortSelect();
  sort.value = 'big';
  const search = h('input', { type: 'search', placeholder: 'Search names', 'aria-label': 'Search names' });
  const body = h('div', { class: 'page' });
  const tbody = h('tbody');
  const selInfo = h('p');
  const selbar = h(
    'div',
    { class: 'selbar' },
    selInfo,
    h('button', { class: 'btn small', type: 'button', onclick: () => pickBiggest(10) }, 'Select the 10 biggest'),
    h('button', { class: 'btn small', type: 'button', onclick: () => op('pin') }, 'Make Keepers'),
    h('button', { class: 'btn small', type: 'button', onclick: () => op('unpin') }, 'Remove Keeper'),
    h('button', { class: 'btn small danger solid', type: 'button', onclick: () => op('delete') }, icon('trash', 'i'), 'Delete'),
  );
  let shown = [];
  function paint() {
    const files = S.files?.f || [];
    shown = filterSort(files, { show: show.value, kind: kind.value, keep: keep.value, sort: sort.value, q: search.value, onlyActive: true });
    for (const b of [...selected]) if (!shown.some((f) => f.b === b)) selected.delete(b);
    const all = h('input', { type: 'checkbox', 'aria-label': 'Select all shown', checked: shown.length > 0 && shown.every((f) => selected.has(f.b)) });
    all.addEventListener('change', () => {
      for (const f of shown) all.checked ? selected.add(f.b) : selected.delete(f.b);
      paint();
    });
    tbody.replaceChildren(
      ...shown.map((f) => {
        const cb = h('input', { type: 'checkbox', checked: selected.has(f.b), 'aria-label': `Select ${f.n}` });
        cb.addEventListener('change', () => {
          cb.checked ? selected.add(f.b) : selected.delete(f.b);
          paintSel();
          tr.classList.toggle('sel', cb.checked);
        });
        const tr = h(
          'tr',
          { class: selected.has(f.b) ? 'sel' : null },
          h('td', null, cb),
          h('td', null, f.th ? h('img', { class: 'mini', src: fileUrl(f, 't'), alt: '', loading: 'lazy' }) : h('span', { class: 'mini' }, ext(f.n))),
          h('td', { class: 'name' }, h('div', null, f.n), h('small', { class: 'hint' }, placeName(f))),
          h('td', { class: 'num' }, bytes(f.cz)),
          h('td', { class: 'num' }, when(f.t)),
          h('td', null, f.pin ? h('span', { class: 'keep-mark' }, 'Keeper') : (S.files?.queue || []).includes(f.b) ? h('span', { class: 'state-mark bad' }, 'Next for Dustin') : ''),
        );
        return tr;
      }),
    );
    table.tHead.rows[0].cells[0].replaceChildren(all);
    paintSel();
    paintDustin();
    paintLost();
  }
  function paintSel() {
    const files = (S.files?.f || []).filter((f) => selected.has(f.b));
    const sum = files.reduce((s, f) => s + (f.pin ? 0 : f.cz), 0);
    selInfo.textContent = files.length ? `${files.length} selected. Deleting frees ${bytes(files.reduce((s, f) => s + f.cz, 0))}.` : `${shown.length} files shown, ${bytes(shown.reduce((s, f) => s + f.cz, 0))}.`;
    void sum;
  }
  function pickBiggest(n) {
    [...shown].sort((a, b) => b.cz - a.cz).slice(0, n).forEach((f) => selected.add(f.b));
    paint();
  }
  async function op(o) {
    if (!selected.size) return toast('Select some files first.');
    if (o === 'delete' && !confirm(`Delete ${selected.size} file(s) from your account? Others keep their copies.`)) return;
    try {
      await rpc('file_op', { o, b: [...selected] });
      if (o === 'delete') selected.clear();
      await getFiles(true);
      paint();
      toast(o === 'delete' ? 'Deleted.' : o === 'pin' ? 'They are Keepers now.' : 'Keeper removed.');
    } catch (e) {
      toast(e.message);
    }
  }
  const dustin = h('div', { class: 'dustin' });
  function paintDustin() {
    const m = S.me;
    const q = S.files?.queue || [];
    const qBytes = (S.files?.f || []).filter((f) => q.includes(f.b)).reduce((s, f) => s + f.cz, 0);
    const bot = icon('bot', 'bot');
    let text;
    if (!m.ar) text = [h('p', null, h('b', null, 'Auto-recycling is off.'), ` Your Tank holds ${bytes(m.q)}; at 95% you get a Red Alert and new files will probably not arrive.`), h('button', { class: 'btn small', type: 'button', onclick: () => setAutoRecycle(true) }, 'Turn on auto-recycling')];
    else if (m.cl)
      text = [
        h('p', null, h('b', null, `Dustin comes by in ${until(m.cl)}.`), ` He will recycle the ${q.length} oldest files that aren’t Keepers (${bytes(qBytes)}) to bring your Tank under ${bytes(m.q)}. Make Keepers of anything you want to keep.`),
        h('div', { class: 'row' }, h('button', { class: 'btn small', type: 'button', onclick: () => ((show.value = 'all'), (keep.value = 'hide'), (sort.value = 'old'), paint()) }, 'Show his list'), h('button', { class: 'btn small danger', type: 'button', onclick: async () => { try { const r = await rpc('file_op', { o: 'cleanup' }); toast(`Dustin recycled ${r.n} file(s).`); await getFiles(true); paint(); } catch (e) { toast(e.message); } } }, 'Recycle now')),
      ];
    else text = [h('p', null, h('b', null, 'Dustin is on duty.'), ` When your Tank passes ${bytes(m.q)}, he recycles your oldest files first. Keepers are never touched.${q.length ? ` Next up: ${q.length} file(s).` : ''}`)];
    dustin.replaceChildren(bot, h('div', { class: 'grow' }, ...text));
  }
  const lost = h('section');
  function paintLost() {
    const fs = (S.files?.f || []).filter((f) => f.s !== 'ok' && f.live);
    lost.hidden = !fs.length;
    lost.replaceChildren(
      h('h2', null, 'Bounced, recycled or deleted'),
      h('p', { class: 'hint' }, 'These are still held by someone else, so you can get them back while they last.'),
      ...fs.map((f) =>
        h(
          'div',
          { class: 'member' },
          h('span', { class: 'mini' }, ext(f.n)),
          h('div', { class: 'txt' }, h('b', null, f.n), h('small', null, `${{ ref: 'Bounced', rec: 'Recycled by Dustin', del: 'Deleted by you' }[f.s]} · ${bytes(f.cz)} · ${placeName(f)}`)),
          h('button', { class: 'btn small', type: 'button', onclick: async () => { try { await rpc('file_op', { o: 'get', b: [f.b] }); setAttState(f.b, 'ok'); await getFiles(true); paint(); } catch (e) { toast(e.message); } } }, f.s === 'ref' ? 'Catch it' : 'Rescue'),
        ),
      ),
    );
  }
  const table = h('table', { class: 'rows' }, h('thead', null, h('tr', null, h('th', null, ''), h('th', null, ''), h('th', null, 'Name'), h('th', null, 'Size'), h('th', null, 'Added'), h('th', null, ''))), tbody);
  for (const el of [show, kind, keep, sort, search]) el.addEventListener('input', paint);
  body.append(dustin, h('section', null, table), lost);
  pane.append(
    bar(h('div', { class: 'title' }, h('b', null, 'Cleaning Room'), h('small', null, () => ''))),
    h('div', { class: 'tools' }, show, kind, keep, sort, search),
    h('div', { class: 'scroll' }, body),
    selbar,
  );
  const sub = pane.querySelector('.bar small');
  const paintSub = () => (sub.textContent = `Tank: ${bytes(S.me.u)} of ${bytes(S.me.q)}${S.me.pin ? `, ${bytes(S.me.pin)} in Keepers` : ''}`);
  paintSub();
  getFiles(true).then(paint, (e) => toast(e.message));
  return {
    update(k) {
      paintSub();
      if (k === 'files') getFiles(true).then(paint);
      if (k === 'me') paintDustin();
    },
  };
}

// ============================================================================================
// Settings
// ============================================================================================

async function deriveKey(password, salt) {
  if (!crypto.subtle) throw new Error('ESC needs a secure (https) connection for passwords.');
  const base = await crypto.subtle.importKey('raw', new TextEncoder().encode(password.normalize('NFKC')), 'PBKDF2', false, ['deriveBits']);
  const bits = await crypto.subtle.deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt, iterations: 600000 }, base, 256);
  return new Uint8Array(bits);
}

function section(title, ...kids) {
  return h('section', null, h('h2', null, title), ...kids);
}

function settingsView(pane) {
  const me = S.me;
  const page = h('div', { class: 'page' });
  const name = h('input', { type: 'text', maxlength: 40, value: me.n, placeholder: 'How people see you once they’re in' });
  const door = h('input', { type: 'checkbox', class: 'switch', checked: me.acc, 'aria-label': 'Door open' });
  door.addEventListener('change', () => rpc('me_set', { acc: door.checked }).then(() => toast(door.checked ? 'Door open: people can knock.' : 'Door closed: nobody new can knock.'), (e) => toast(e.message)));
  const ar = h('input', { type: 'checkbox', class: 'switch', checked: me.ar, 'aria-label': 'Auto-recycling' });
  ar.addEventListener('change', () => setAutoRecycle(ar.checked));
  const lifeline = h('div', { class: 'stack' });
  function paintLifeline() {
    const cur = S.me.rec;
    const email = h('input', { type: 'email', placeholder: 'you@example.com' });
    lifeline.replaceChildren(
      h('p', { class: 'hint' }, cur === 'email' ? `Email Lifeline: a reset link goes to ${S.me.em}.` : cur === 'code' ? 'Code Lifeline: you saved a recovery code.' : 'No Lifeline: if you forget your password, this account is lost for good.'),
      h('div', { class: 'row' }, h('span', { class: 'grow' }, email), h('button', { class: 'btn', type: 'button', onclick: () => rpc('lifeline', { k: 'email', e: email.value }).then(() => toast('Email Lifeline saved.'), (e) => toast(e.message)) }, 'Use this email')),
      h(
        'div',
        { class: 'row' },
        h(
          'button',
          {
            class: 'btn',
            type: 'button',
            onclick: async () => {
              try {
                const r = await rpc('lifeline', { k: 'code' });
                dialog((d) =>
                  d.append(
                    h(
                      'div',
                      { class: 'body' },
                      h('h2', null, 'Your Lifeline code'),
                      h('p', null, 'Write it down or save it in a password manager. It is shown only now, and it works once.'),
                      h('div', { class: 'code-show' }, r.code),
                      h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => navigator.clipboard?.writeText(r.code).then(() => toast('Copied.')) }, 'Copy'), h('button', { class: 'btn primary', type: 'button', onclick: () => d.close() }, 'I saved it')),
                    ),
                  ),
                );
              } catch (e) {
                toast(e.message);
              }
            },
          },
          'Make a Lifeline code',
        ),
        cur !== 'none'
          ? h('button', { class: 'btn ghost danger', type: 'button', onclick: () => confirm('Remove your Lifeline? A forgotten password would then lose this account.') && rpc('lifeline', { k: 'none' }).then(() => toast('Lifeline removed.'), (e) => toast(e.message)) }, 'Remove Lifeline')
          : null,
      ),
    );
  }
  const devices = h('div');
  async function paintDevices() {
    const r = await rpc('devices').catch(() => ({ s: [] }));
    devices.replaceChildren(
      ...r.s
        .sort((a, b) => b.t - a.t)
        .map((s) =>
          h(
            'div',
            { class: 'member' },
            h('div', { class: 'txt' }, h('b', null, s.l || 'A device'), h('small', null, s.me ? 'This device' : `Signed in ${when(s.t)}`)),
            s.me ? null : h('button', { class: 'btn small ghost danger', type: 'button', onclick: () => rpc('device_end', { h: s.h }).then(paintDevices, (e) => toast(e.message)) }, 'Sign out'),
          ),
        ),
    );
  }
  const bolted = h('div');
  function paintBolted() {
    const list = S.me.blk || [];
    bolted.replaceChildren(
      ...(list.length
        ? list.map((cs) => h('div', { class: 'member' }, avatar(cs, true), h('div', { class: 'txt' }, h('span', { class: 'callsign' }, cs)), h('button', { class: 'btn small', type: 'button', onclick: () => rpc('block', { a: cs, on: false }).catch((e) => toast(e.message)) }, 'Unbolt')))
        : [h('p', { class: 'hint' }, 'Nobody is bolted out.')]),
    );
  }
  const pw = { cur: h('input', { type: 'password', autocomplete: 'current-password' }), n1: h('input', { type: 'password', autocomplete: 'new-password', minlength: 8 }), n2: h('input', { type: 'password', autocomplete: 'new-password' }) };
  const pwErr = h('p', { class: 'err' });
  const tickets = h('div', { class: 'stack' });
  async function paintTickets() {
    if (!(S.mi > 0 || S.me.adm)) return (tickets.hidden = true);
    const r = await rpc('tickets').catch(() => ({ i: [], left: 0 }));
    tickets.replaceChildren(
      h('p', { class: 'hint' }, S.me.adm ? 'Tickets let new people create an account here.' : `You can hand out ${r.left} more ticket(s).`),
      ...r.i.map((t) => h('div', { class: 'member' }, h('div', { class: 'txt' }, h('b', { class: 'callsign' }, t.c), h('small', null, `${t.u} use(s) left · until ${fDate.format(new Date(t.x))}`)), h('button', { class: 'btn small', type: 'button', onclick: () => navigator.clipboard?.writeText(`${location.origin}/#ticket=${t.c}`).then(() => toast('Ticket link copied.')) }, 'Copy link'))),
      h('button', { class: 'btn', type: 'button', onclick: () => rpc('ticket_new', { u: 1, d: 7 }).then(paintTickets, (e) => toast(e.message)) }, 'Create a ticket'),
    );
  }
  page.append(
    section('You', h('div', { class: 'row' }, flags(me.id, 'big'), h('span', { class: 'callsign big-cs' }, me.id)), h('form', { class: 'row', onsubmit: (e) => (e.preventDefault(), rpc('me_set', { n: name.value }).then(() => toast('Name saved.'), (ex) => toast(ex.message))) }, h('span', { class: 'grow' }, name), h('button', { class: 'btn', type: 'submit' }, 'Save name')), h('p', { class: 'hint' }, 'Your name is only shown to people you share a Line or Crew with, and to people you knock on.')),
    section('Door', h('div', { class: 'kv' }, h('div', null, h('b', null, 'Accept knocks from new people'), h('span', { class: 'hint' }, 'When closed, nobody new can knock. People you already talk to can still invite you to crews.')), door)),
    section('Auto-recycling', h('div', { class: 'kv' }, h('div', null, h('b', null, 'Let Dustin recycle old files'), h('span', { class: 'hint' }, `When your Tank (${bytes(me.q)}) is full, the oldest files that aren’t Keepers are recycled, so new files always arrive. Text never counts. Turning this on while over gives you ${S.me.cl ? 'a' : 'a 24 h'} warning first.`)), ar)),
    section('Lifeline', h('p', { class: 'hint' }, 'How you get back in if you forget your password.'), lifeline),
    section('Password', h('form', { class: 'stack', onsubmit: changePassword }, h('label', { class: 'field' }, h('span', null, 'Current password'), pw.cur), h('label', { class: 'field' }, h('span', null, 'New password (at least 8 characters)'), pw.n1), h('label', { class: 'field' }, h('span', null, 'Repeat new password'), pw.n2), pwErr, h('div', null, h('button', { class: 'btn', type: 'submit' }, 'Change password')))),
    section('Devices', h('p', { class: 'hint' }, 'Where you are signed in.'), devices),
    section('Bolted callsigns', bolted),
    section('Tickets', tickets),
    section(
      'This device',
      'Notification' in window && Notification.permission === 'default'
        ? h('button', { class: 'btn', type: 'button', onclick: () => Notification.requestPermission().then((p) => toast(p === 'granted' ? 'You’ll be notified while ESC runs in the background.' : 'Notifications stay off.')) }, 'Notify me while ESC is in the background')
        : h('p', { class: 'hint' }, 'Notification' in window ? `Notifications are ${Notification.permission === 'granted' ? 'on' : 'blocked in this browser'}.` : 'This browser has no notifications.'),
      h('div', null, h('button', { class: 'btn danger', type: 'button', onclick: signOut }, 'Sign out')),
    ),
  );
  async function changePassword(e) {
    e.preventDefault();
    pwErr.textContent = '';
    if (pw.n1.value.length < 8) return (pwErr.textContent = 'Use at least 8 characters.');
    if (pw.n1.value !== pw.n2.value) return (pwErr.textContent = 'The new passwords don’t match.');
    try {
      const { s } = await api('/api/pre', { id: S.me.id });
      const old = await deriveKey(pw.cur.value, unb64u(s));
      const salt = crypto.getRandomValues(new Uint8Array(16));
      const key = await deriveKey(pw.n1.value, salt);
      await rpc('password', { old: b64u(old), s: b64u(salt), k: b64u(key) });
      pw.cur.value = pw.n1.value = pw.n2.value = '';
      toast('Password changed. Your other devices were signed out.');
      paintDevices();
    } catch (ex) {
      pwErr.textContent = ex.message;
    }
  }
  pane.append(bar(h('div', { class: 'title' }, h('b', null, 'Settings'))), h('div', { class: 'scroll' }, page));
  paintLifeline();
  paintDevices();
  paintBolted();
  paintTickets();
  return {
    update(k) {
      if (k === 'me') {
        door.checked = S.me.acc;
        ar.checked = S.me.ar;
        paintLifeline();
        paintBolted();
      }
    },
  };
}

async function signOut() {
  await api('/api/logout', {}).catch(() => {});
  location.hash = '';
  location.reload();
}

// ============================================================================================
// Control Tower (admins)
// ============================================================================================

function towerView(pane) {
  const page = h('div', { class: 'page' });
  pane.append(bar(h('div', { class: 'title' }, h('b', null, 'Control Tower'), h('small', null, 'Run this relay and its Ring'))), h('div', { class: 'scroll' }, page));
  async function paint() {
    let t;
    try {
      t = await rpc('tower');
    } catch (e) {
      page.replaceChildren(h('p', { class: 'err' }, e.message));
      return;
    }
    const s = t.set;
    const num = (v, attrs = {}) => h('input', { type: 'number', value: v, min: 0, step: 'any', ...attrs });
    const f = {
      default_quota: num((s.default_quota / GB).toFixed(2)),
      flex_pct: num(s.flex_pct, { max: 100 }),
      cleanup_h: num(s.cleanup_h),
      reserve_pct: num(s.reserve_pct, { max: 90 }),
      reserve_min: num((s.reserve_min / GB).toFixed(1)),
      member_invites: num(s.member_invites),
      req_pending_max: num(s.req_pending_max, { min: 1 }),
      req_daily_max: num(s.req_daily_max, { min: 1 }),
      signup: h('select', null, ...['invite', 'open', 'closed'].map((v) => h('option', { value: v, selected: s.signup === v }, { invite: 'With a Ticket only', open: 'Open to anyone', closed: 'Closed' }[v]))),
    };
    const fieldRow = (label, el, hint) => h('label', { class: 'field' }, h('span', null, label), el, hint ? h('span', { class: 'hint' }, hint) : null);
    const setErr = h('p', { class: 'err' });
    const smtp = {
      host: h('input', { type: 'text', value: s['smtp.host'], placeholder: 'smtp.gmail.com' }),
      port: num(s['smtp.port'] || 465),
      user: h('input', { type: 'text', value: s['smtp.user'] }),
      pass: h('input', { type: 'password', placeholder: s['smtp.pass_set'] ? 'Saved. Type to replace.' : 'App password' }),
      from: h('input', { type: 'email', value: s['smtp.from'], placeholder: 'esc@yourdomain' }),
      tls: h('select', null, h('option', { value: 'ssl', selected: s['smtp.tls'] !== 'starttls' }, 'SSL (usually port 465)'), h('option', { value: 'starttls', selected: s['smtp.tls'] === 'starttls' }, 'STARTTLS (usually port 587)')),
    };
    const testTo = h('input', { type: 'email', placeholder: 'Send a test to…' });
    const room = t.disk.room;
    const over = t.committed > room + t.stored;
    page.replaceChildren(
      section(
        'Overview',
        h(
          'div',
          { class: 'stats' },
          ...[
            ['Free disk', bytes(t.disk.free)],
            ['Room for files', bytes(room)],
            ['Files stored', bytes(t.stored)],
            ['Tanks promised', bytes(t.committed)],
            ['People', String(t.accounts.length)],
            ['Online now', String(t.online)],
          ].map(([k, v]) => h('div', { class: 'stat' }, h('small', null, k), h('b', null, v))),
        ),
        over ? h('p', { class: 'hint' }, 'The Tanks you promised add up to more than the free disk. That’s fine while people don’t fill them; files stop arriving for everyone once the disk reserve is reached.') : null,
      ),
      section(
        'Ring rules',
        h(
          'form',
          {
            class: 'stack',
            onsubmit: async (e) => {
              e.preventDefault();
              setErr.textContent = '';
              const set = {
                default_quota: Math.round(parseFloat(f.default_quota.value) * GB),
                flex_pct: +f.flex_pct.value,
                cleanup_h: +f.cleanup_h.value,
                reserve_pct: +f.reserve_pct.value,
                reserve_min: Math.round(parseFloat(f.reserve_min.value) * GB),
                member_invites: +f.member_invites.value,
                req_pending_max: +f.req_pending_max.value,
                req_daily_max: +f.req_daily_max.value,
                signup: f.signup.value,
              };
              try {
                await rpc('tower_set', { set });
                toast('Saved.');
                paint();
              } catch (ex) {
                setErr.textContent = ex.message;
              }
            },
          },
          fieldRow('Default Tank size (GB)', f.default_quota, 'Text never counts. Each person can get their own size below.'),
          fieldRow('Spillway (% over the Tank)', f.flex_pct, 'With auto-recycling on, a full Tank still takes new files up to this much over, so they always arrive.'),
          fieldRow('Dustin’s grace period (hours)', f.cleanup_h, 'When someone turns auto-recycling back on while over, this is how long they get to pick Keepers.'),
          fieldRow('Disk reserve (%)', f.reserve_pct),
          fieldRow('Disk reserve, at least (GB)', f.reserve_min, 'Uploads stop before the disk gets this full.'),
          fieldRow('Sign-up', f.signup),
          fieldRow('Tickets each member may hand out', f.member_invites, '0 means only admins create Tickets.'),
          fieldRow('Knocks waiting at once (per person)', f.req_pending_max),
          fieldRow('Knocks per day (per person)', f.req_daily_max),
          setErr,
          h('div', null, h('button', { class: 'btn primary', type: 'submit' }, 'Save ring rules')),
        ),
      ),
      section(
        'People',
        h(
          'table',
          { class: 'rows' },
          h('thead', null, h('tr', null, h('th', null, 'Callsign'), h('th', null, 'Tank used'), h('th', null, 'Own Tank (GB)'), h('th', null, 'Admin'), h('th', null, 'Disabled'), h('th', null, ''))),
          h(
            'tbody',
            null,
            ...t.accounts.map((a) => {
              const q = h('input', { type: 'number', step: 'any', min: 0, value: a.qo ? (a.q / GB).toFixed(2) : '', placeholder: 'default' });
              q.style.width = '7em';
              const adm = h('input', { type: 'checkbox', checked: a.adm, disabled: a.id === S.me.id });
              const dis = h('input', { type: 'checkbox', checked: a.dis, disabled: a.id === S.me.id });
              return h(
                'tr',
                null,
                h('td', null, h('div', { class: 'row' }, avatar(a.id, true), h('div', null, h('span', { class: 'callsign' }, a.id), h('div', { class: 'hint' }, a.n || '')))),
                h('td', { class: 'num' }, `${bytes(a.u)} of ${bytes(a.q)}`, h('div', { class: 'hint' }, `${a.files} files${a.ar ? '' : ', recycling off'}`)),
                h('td', null, q),
                h('td', null, adm),
                h('td', null, dis),
                h(
                  'td',
                  null,
                  h(
                    'button',
                    {
                      class: 'btn small',
                      type: 'button',
                      onclick: async () => {
                        try {
                          await rpc('tower_account', { id: a.id, q: q.value === '' ? null : Math.round(parseFloat(q.value) * GB), adm: adm.checked, dis: dis.checked });
                          toast(`${a.id} updated.`);
                          paint();
                        } catch (e) {
                          toast(e.message);
                        }
                      },
                    },
                    'Save',
                  ),
                ),
              );
            }),
          ),
        ),
      ),
      section(
        'Tickets',
        h('p', { class: 'hint' }, 'A Ticket lets one new person create an account. Send them the link.'),
        ...t.invites.map((i) =>
          h(
            'div',
            { class: 'member' },
            h('div', { class: 'txt' }, h('b', { class: 'callsign' }, i.c), h('small', null, `${i.u} use(s) left${i.adm ? ', makes an admin' : ''} · until ${i.x > 9e15 ? 'used' : fDate.format(new Date(i.x))}${i.by ? ` · by ${i.by}` : ''}`)),
            h('button', { class: 'btn small', type: 'button', onclick: () => navigator.clipboard?.writeText(`${location.origin}/#ticket=${i.c}`).then(() => toast('Ticket link copied.')) }, 'Copy link'),
            h('button', { class: 'btn small ghost danger', type: 'button', onclick: () => rpc('ticket_del', { c: i.c }).then(paint, (e) => toast(e.message)) }, 'Tear up'),
          ),
        ),
        h('div', { class: 'row' }, h('button', { class: 'btn', type: 'button', onclick: () => rpc('ticket_new', { u: 1, d: 7 }).then(paint, (e) => toast(e.message)) }, 'Create a Ticket'), h('button', { class: 'btn ghost', type: 'button', onclick: () => rpc('ticket_new', { u: 1, d: 7, adm: true }).then(paint, (e) => toast(e.message)) }, 'Create an admin Ticket')),
      ),
      section(
        'Email Lifelines',
        h('p', { class: 'hint' }, 'To email password-reset links, the relay needs an email account to send from (for Gmail, use an app password).'),
        fieldRow('SMTP server', smtp.host),
        fieldRow('Port', smtp.port),
        fieldRow('Security', smtp.tls),
        fieldRow('User', smtp.user),
        fieldRow('Password', smtp.pass),
        fieldRow('Send as', smtp.from),
        h(
          'div',
          { class: 'row' },
          h(
            'button',
            {
              class: 'btn primary',
              type: 'button',
              onclick: async () => {
                const set = { 'smtp.host': smtp.host.value.trim(), 'smtp.port': +smtp.port.value, 'smtp.user': smtp.user.value.trim(), 'smtp.from': smtp.from.value.trim(), 'smtp.tls': smtp.tls.value };
                if (smtp.pass.value) set['smtp.pass'] = smtp.pass.value;
                try {
                  await rpc('tower_set', { set });
                  toast('Mail settings saved.');
                  paint();
                } catch (e) {
                  toast(e.message);
                }
              },
            },
            'Save mail settings',
          ),
          h('span', { class: 'grow' }, testTo),
          h('button', { class: 'btn', type: 'button', onclick: () => rpc('tower_mail_test', { to: testTo.value }).then(() => toast('Test email sent.'), (e) => toast(e.message)) }, 'Send test'),
        ),
      ),
      section(
        'The Ring',
        h('p', { class: 'hint' }, 'Every Star (relay) keeps a full copy of everything. Add another computer to the Ring for redundancy.'),
        ...t.relays.map((r) => h('div', { class: 'member' }, h('span', { class: 'flags' }, '★'), h('div', { class: 'txt' }, h('b', null, `${r.n}${r.me ? ' (this Star)' : ''}`), h('small', null, r.u || 'no public address yet')))),
        h(
          'button',
          {
            class: 'btn',
            type: 'button',
            onclick: async () => {
              try {
                const sp = await rpc('tower_spark');
                const url = sp.url || location.origin;
                dialog((d) =>
                  d.append(
                    h(
                      'div',
                      { class: 'body' },
                      h('h2', null, 'Light a new Star'),
                      h('p', null, 'On the new computer, copy the esc program, then run (one line):'),
                      h('div', { class: 'code-show' }, `esc join ${url} ${sp.spark} --url <its public address>`),
                      h('p', { class: 'hint' }, 'The Spark works once, within 24 hours. Then run "esc serve" there: it copies the whole Ring and joins in.'),
                      h('div', { class: 'foot' }, h('button', { class: 'btn primary', type: 'button', onclick: () => d.close() }, 'Done')),
                    ),
                  ),
                );
              } catch (e) {
                toast(e.message);
              }
            },
          },
          'Light a new Star',
        ),
      ),
    );
  }
  paint();
  return {};
}

// ============================================================================================
// Welcome: sign in, create an account with a Ticket, Lifelines
// ============================================================================================

function signedOut() {
  S.me = null;
  WS.s?.close();
  welcome();
}

async function welcome() {
  document.title = 'ESC';
  const app = $('#app');
  const info = (S.info = await api('/api/info', null, 'GET').catch(() => ({ signup: 'invite', star: '', mail: false })));
  const params = new URLSearchParams(location.hash.slice(1));
  const ticket = params.get('ticket') || '';
  const lifeline = params.get('lifeline') || '';
  const share = cleanCS(location.hash.slice(1));
  if (CS_RE.test(share)) sessionStorage.setItem('esc-knock', share);
  let mode = lifeline ? 'reset' : ticket ? 'create' : 'signin';
  const box = h('div', { class: 'panel' });
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'What do you want to do' });
  function paintSeg() {
    seg.replaceChildren(
      ...[
        ['signin', 'Sign in'],
        ['create', 'New account'],
        ['life', 'Lost password'],
      ].map(([m, label]) => h('button', { type: 'button', 'aria-pressed': String(mode === m || (m === 'life' && mode === 'reset')), onclick: () => ((mode = m), paint()) }, label)),
    );
  }
  function paint() {
    paintSeg();
    box.replaceChildren(seg, mode === 'signin' ? signIn() : mode === 'create' ? create() : mode === 'reset' ? resetByLink() : lostPassword());
  }
  function busy(btn, label) {
    btn.disabled = true;
    const old = btn.textContent;
    btn.textContent = label;
    return () => {
      btn.disabled = false;
      btn.textContent = old;
    };
  }
  function signIn() {
    const { input: cs, preview } = callsignInput();
    const pw = h('input', { type: 'password', autocomplete: 'current-password', required: true });
    const err = h('p', { class: 'err' });
    const btn = h('button', { class: 'btn primary', type: 'submit' }, 'Sign in');
    setTimeout(() => cs.focus(), 30);
    return h(
      'form',
      {
        class: 'stack',
        onsubmit: async (e) => {
          e.preventDefault();
          err.textContent = '';
          const id = cleanCS(cs.value);
          if (!CS_RE.test(id)) return (err.textContent = 'Type your 4-character callsign.');
          const done = busy(btn, 'Checking…');
          try {
            const { s } = await api('/api/pre', { id });
            const k = await deriveKey(pw.value, unb64u(s));
            await api('/api/login', { id, k: b64u(k) });
            start();
          } catch (ex) {
            err.textContent = ex.message;
            done();
          }
        },
      },
      h('label', { class: 'field' }, h('span', null, 'Callsign'), cs),
      preview,
      h('label', { class: 'field' }, h('span', null, 'Password'), pw),
      err,
      btn,
    );
  }
  function create() {
    const t = h('input', { type: 'text', value: ticket, placeholder: 'Ticket code', autocapitalize: 'characters' });
    const name = h('input', { type: 'text', maxlength: 40, placeholder: 'Optional' });
    const p1 = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8, required: true });
    const p2 = h('input', { type: 'password', autocomplete: 'new-password', required: true });
    const err = h('p', { class: 'err' });
    const btn = h('button', { class: 'btn primary', type: 'submit' }, 'Create my account');
    const needTicket = info.signup !== 'open';
    return h(
      'form',
      {
        class: 'stack',
        onsubmit: async (e) => {
          e.preventDefault();
          err.textContent = '';
          if (p1.value.length < 8) return (err.textContent = 'Use at least 8 characters for the password.');
          if (p1.value !== p2.value) return (err.textContent = 'The passwords don’t match.');
          const done = busy(btn, 'Creating…');
          try {
            const salt = crypto.getRandomValues(new Uint8Array(16));
            const k = await deriveKey(p1.value, salt);
            const r = await api('/api/signup', { t: t.value.trim().toUpperCase(), s: b64u(salt), k: b64u(k), n: name.value });
            history.replaceState(null, '', location.pathname);
            revealCallsign(r.id);
          } catch (ex) {
            err.textContent = ex.message;
            done();
          }
        },
      },
      info.signup === 'closed' ? h('p', { class: 'hint' }, 'Sign-up is closed here. Only admin Tickets work.') : null,
      needTicket || ticket ? h('label', { class: 'field' }, h('span', null, needTicket ? 'Ticket' : 'Ticket (optional)'), t) : null,
      h('label', { class: 'field' }, h('span', null, 'Your name'), name, h('span', { class: 'hint' }, 'Only people you talk to see it.')),
      h('label', { class: 'field' }, h('span', null, 'Password (at least 8 characters)'), p1),
      h('label', { class: 'field' }, h('span', null, 'Repeat password'), p2),
      h('p', { class: 'hint' }, 'You get a 4-character callsign. It is the only way people can reach you.'),
      err,
      btn,
    );
  }
  function lostPassword() {
    const { input: cs, preview } = callsignInput();
    const code = h('input', { type: 'text', placeholder: 'XXXXX-XXXXX-XXXXX-XXXXX', autocapitalize: 'characters', spellcheck: 'false' });
    const p1 = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8 });
    const err = h('p', { class: 'err' });
    const ok = h('p', { class: 'hint' });
    return h(
      'div',
      { class: 'stack' },
      h('label', { class: 'field' }, h('span', null, 'Callsign'), cs),
      preview,
      h('p', { class: 'hint' }, 'If you set an Email Lifeline, we send a reset link. If you saved a Lifeline code, type it with a new password.'),
      info.mail
        ? h(
            'button',
            {
              class: 'btn',
              type: 'button',
              onclick: async () => {
                err.textContent = '';
                try {
                  await api('/api/lifeline/email', { id: cleanCS(cs.value) });
                  ok.textContent = 'If that callsign has an Email Lifeline, a link is on its way. It works for 30 minutes.';
                } catch (ex) {
                  err.textContent = ex.message;
                }
              },
            },
            'Email me a reset link',
          )
        : null,
      h('label', { class: 'field' }, h('span', null, 'Lifeline code'), code),
      h('label', { class: 'field' }, h('span', null, 'New password'), p1),
      err,
      ok,
      h(
        'button',
        {
          class: 'btn primary',
          type: 'button',
          onclick: async () => {
            err.textContent = '';
            if (p1.value.length < 8) return (err.textContent = 'Use at least 8 characters.');
            try {
              const salt = crypto.getRandomValues(new Uint8Array(16));
              const k = await deriveKey(p1.value, salt);
              await api('/api/lifeline/code', { id: cleanCS(cs.value), code: code.value, s: b64u(salt), k: b64u(k) });
              toast('Password reset. Your Lifeline code is used up: make a new one in Settings.');
              start();
            } catch (ex) {
              err.textContent = ex.message;
            }
          },
        },
        'Reset with the code',
      ),
    );
  }
  function resetByLink() {
    const p1 = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8 });
    const p2 = h('input', { type: 'password', autocomplete: 'new-password' });
    const err = h('p', { class: 'err' });
    return h(
      'form',
      {
        class: 'stack',
        onsubmit: async (e) => {
          e.preventDefault();
          if (p1.value.length < 8) return (err.textContent = 'Use at least 8 characters.');
          if (p1.value !== p2.value) return (err.textContent = 'The passwords don’t match.');
          try {
            const salt = crypto.getRandomValues(new Uint8Array(16));
            const k = await deriveKey(p1.value, salt);
            await api('/api/lifeline/token', { t: lifeline, s: b64u(salt), k: b64u(k) });
            history.replaceState(null, '', location.pathname);
            start();
          } catch (ex) {
            err.textContent = ex.message;
          }
        },
      },
      h('h2', null, 'Choose a new password'),
      h('label', { class: 'field' }, h('span', null, 'New password'), p1),
      h('label', { class: 'field' }, h('span', null, 'Repeat'), p2),
      err,
      h('button', { class: 'btn primary', type: 'submit' }, 'Save the new password'),
    );
  }
  function revealCallsign(id) {
    box.replaceChildren(
      h(
        'div',
        { class: 'reveal' },
        h('h2', null, 'You are'),
        flags(id, 'big'),
        h('span', { class: 'callsign' }, id),
        h('p', null, 'That’s your callsign. Share it with the people you want to talk to: they knock, and you decide whether to open the door.'),
        h('p', { class: 'hint' }, 'Tip: in Settings, set up a Lifeline so a forgotten password doesn’t lose your account.'),
        h('div', { class: 'row' }, h('button', { class: 'btn', type: 'button', onclick: () => navigator.clipboard?.writeText(id).then(() => toast('Callsign copied.')) }, 'Copy callsign'), h('button', { class: 'btn primary', type: 'button', onclick: () => start() }, 'Come aboard')),
      ),
    );
  }
  const halyard = h('div', { class: 'halyard', 'aria-hidden': 'true' }, ...['E', 'S', 'C'].map((c) => flagNode(c)));
  app.replaceChildren(
    h(
      'main',
      { class: 'welcome' },
      h(
        'div',
        { class: 'welcome-inner' },
        h('div', { class: 'wordmark' }, halyard, h('h1', null, 'Extremely Simple Communication'), h('p', null, info.star ? `Star ${info.star}` : '')),
        box,
      ),
    ),
  );
  paint();
}

// ============================================================================================
// Boot
// ============================================================================================

function mountShell() {
  $('#app').replaceChildren(h('div', { class: 'alerts' }), h('div', { class: 'shell' }, h('aside', { class: 'rail', 'aria-label': 'Lines and crews' }), h('main', { class: 'pane' })));
}

async function start() {
  const r = await fetch('/api/me', { cache: 'no-store' }).catch(() => null);
  if (!r || r.status === 401) return welcome();
  const share = cleanCS(location.hash.slice(1));
  if (CS_RE.test(share)) {
    sessionStorage.setItem('esc-knock', share);
    history.replaceState(null, '', location.pathname);
  }
  $('#app').replaceChildren(h('div', { class: 'empty' }, 'Raising the flags…'));
  if (!WS.started) {
    WS.started = true;
    connect();
  } else hello().catch(() => {});
}

function defs() {
  const t = document.createElement('template');
  t.innerHTML = `<svg class="sr" aria-hidden="true"><defs><clipPath id="esc-pennant"><path d="${PENNANT}"/></clipPath></defs></svg>`;
  document.body.prepend(t.content.firstChild);
}

addEventListener('hashchange', () => {
  if (!S.me) return;
  const share = cleanCS(location.hash.slice(1));
  if (CS_RE.test(share) && !location.hash.startsWith('#/')) {
    history.replaceState(null, '', location.pathname);
    knockDialog(share);
    return;
  }
  routeChanged();
});
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && S.view?.c) {
    const box = S.msgs.get(S.view.c);
    const last = box?.list[box.list.length - 1];
    const c = S.convs.get(S.view.c);
    if (last && c) markRead(c, last.t);
    paint();
  }
});
if ('serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js').catch(() => {});

defs();
start();
