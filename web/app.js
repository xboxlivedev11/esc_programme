// ESC web app. Plain JavaScript, no libraries, no build step.
// One WebSocket to the relay carries every action; uploads and downloads use plain HTTP.

import { PT } from './i18n.js';

// ============================================================================================
// Preferences (per device), language, theme
// ============================================================================================

const PREFS_KEY = 'esc-prefs';
const prefs = { theme: 'auto', lang: 'auto', sound: true, soundOpen: true, notify: true, askedNotify: false };
try {
  Object.assign(prefs, JSON.parse(localStorage.getItem(PREFS_KEY) || '{}'));
} catch {
  /* private mode: defaults */
}
function savePrefs() {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    /* not persisted */
  }
}

/** Brazilian Portuguese unless the browser prefers English (or the user picked a language). */
function pickLang() {
  if (prefs.lang === 'pt' || prefs.lang === 'en') return prefs.lang;
  for (const l of navigator.languages || [navigator.language || '']) {
    if (/^pt\b/i.test(l)) return 'pt';
    if (/^en\b/i.test(l)) return 'en';
  }
  return 'pt';
}
const LANG = pickLang();
const LOCALE = LANG === 'pt' ? 'pt-BR' : 'en';
document.documentElement.lang = LOCALE;

/** Translate an English UI string; {0}, {1}… are replaced by the arguments. */
function t(s, ...a) {
  const v = LANG === 'pt' ? (PT[s] ?? s) : s;
  return a.length ? v.replace(/\{(\d)\}/g, (_, i) => a[+i]) : v;
}
/** An error from the relay, the network or the browser, in the user's language. */
const err = (e) => t(e?.message || String(e));

const darkQuery = matchMedia('(prefers-color-scheme: dark)');
function applyTheme() {
  const root = document.documentElement;
  if (prefs.theme === 'light' || prefs.theme === 'dark') root.dataset.theme = prefs.theme;
  else root.removeAttribute('data-theme');
  const dark = prefs.theme === 'dark' || (prefs.theme !== 'light' && darkQuery.matches);
  const meta = document.querySelector('meta[name=theme-color]');
  if (meta) meta.content = dark ? '#0b1724' : '#f1f4f6';
}
applyTheme();
darkQuery.addEventListener('change', applyTheme);
const THEMES = { auto: 'Automatic theme', light: 'Light theme', dark: 'Dark theme' };
const themeIcon = () => ({ light: 'sun', dark: 'moon' })[prefs.theme] || 'auto';
function cycleTheme() {
  prefs.theme = { auto: 'light', light: 'dark', dark: 'auto' }[prefs.theme] || 'auto';
  savePrefs();
  applyTheme();
  paint();
  toast(t(THEMES[prefs.theme]));
}

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
const num = new Intl.NumberFormat(LOCALE, { maximumFractionDigits: 2 });
function bytes(n) {
  if (n < KB) return `${n} B`;
  if (n < MB) return `${num.format(+(n / KB).toFixed(n < 10 * KB ? 1 : 0))} KB`;
  if (n < GB) return `${num.format(+(n / MB).toFixed(n < 10 * MB ? 1 : 0))} MB`;
  return `${num.format(+(n / GB).toFixed(2))} GB`;
}
const fClock = new Intl.DateTimeFormat(LOCALE, { hour: '2-digit', minute: '2-digit' });
const fWeek = new Intl.DateTimeFormat(LOCALE, { weekday: 'short' });
const fDate = new Intl.DateTimeFormat(LOCALE, { day: 'numeric', month: 'short' });
const fDay = new Intl.DateTimeFormat(LOCALE, { weekday: 'long', day: 'numeric', month: 'long' });
const fFull = new Intl.DateTimeFormat(LOCALE, { dateStyle: 'medium', timeStyle: 'short' });
const sameDay = (a, b) => new Date(a).toDateString() === new Date(b).toDateString();
function when(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  if (sameDay(ts, Date.now())) return fClock.format(d);
  if (Date.now() - ts < 6 * 864e5) return fWeek.format(d);
  return fDate.format(d);
}
function until(ts) {
  const ms = Math.max(0, ts - Date.now() - (S.skew || 0));
  const hh = Math.floor(ms / 36e5), mm = Math.floor((ms % 36e5) / 6e4);
  return hh ? t('{0} h {1} min', hh, mm) : t('{0} min', mm);
}
function mmss(s) {
  if (!isFinite(s) || s < 0) return '0:00';
  s = Math.round(s);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}
const b64u = (u8) => btoa(String.fromCharCode(...u8)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const unb64u = (s) => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
const ext = (name) => (/\.([a-z0-9]{1,5})$/i.exec(name || '')?.[1] || 'file').toLowerCase();
const kindOf = (mime, name) =>
  /^image\//.test(mime) || /\.(heic|heif|jxl)$/i.test(name) ? 'image'
  : /^video\//.test(mime) ? 'video'
  : /^audio\//.test(mime) || /\.(opus|oga|ogg|m4a|mp3|wav|flac|weba)$/i.test(name) ? 'audio'
  : /pdf|word|document|text|sheet|excel|presentation|powerpoint|rtf|opendocument/.test(mime) ? 'doc'
  : 'other';
/** Voice messages are audio files recorded here, named voice-<time>. */
const isVoice = (a) => kindOf(a.m, a.n) === 'audio' && /^voice-/.test(a.n || '');
const filesWord = (n) => (n === 1 ? t('1 file') : t('{0} files', n));

let toastTimer;
function toast(msg) {
  document.querySelector('.toast')?.remove();
  const el = h('div', { class: 'toast', role: 'status' }, msg);
  document.body.append(el);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.remove(), 3500);
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
  pause: 'M7.5 5h3.2v14H7.5zM13.3 5h3.2v14h-3.2z',
  mic: 'M12 15a3.5 3.5 0 0 0 3.5-3.5v-5a3.5 3.5 0 1 0-7 0v5A3.5 3.5 0 0 0 12 15ZM5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21M8.5 21h7',
  bot: 'M6.5 9.5h11v9h-11zM10 13.5v.1M14 13.5v.1M10 16.5h4M12 6v3.5M12 6a1.2 1.2 0 1 0 0-2.4A1.2 1.2 0 0 0 12 6Z',
  sun: 'M12 16.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9ZM12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
  moon: 'M20 14.5A8.5 8.5 0 0 1 9.5 4 8.5 8.5 0 1 0 20 14.5Z',
  auto: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 3v18M12 7.5h4.5M12 12h6M12 16.5h4.5',
  bell: 'M6 16V11a6 6 0 1 1 12 0v5l2 2H4zM10 20.5a2 2 0 0 0 4 0',
  camera: 'M4 8h3l2-3h6l2 3h3v11H4zM12 16.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Z',
};
const FILLED = new Set(['play', 'pause']);
function icon(name, cls = '') {
  const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('aria-hidden', 'true');
  if (cls) s.setAttribute('class', cls);
  const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  p.setAttribute('d', ICONS[name]);
  if (FILLED.has(name)) p.setAttribute('fill', 'currentColor');
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
const iconBtn = (name, label, onclick, cls = '') =>
  h('button', { class: `icon-btn ${cls}`, type: 'button', title: label, 'aria-label': label, onclick }, icon(name));

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
const MEANING = { K: 'I wish to communicate with you', C: 'Affirmative', N: 'Negative' };
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
function flagNode(ch) {
  const tpl = document.createElement('template');
  tpl.innerHTML = flagSVG(ch);
  return tpl.content.firstChild;
}

/** Profile pictures this account may see: callsign -> picture version. */
const PICS = new Map();
function notePics() {
  PICS.clear();
  if (S.me?.pic) PICS.set(S.me.id, S.me.pic);
  for (const c of S.convs.values()) {
    for (const m of c.m || []) if (m.p) PICS.set(m.a, m.p);
    if (c.byp && c.by) PICS.set(c.by, c.byp);
  }
  for (const k of S.rin) if (k.p) PICS.set(k.f, k.p);
}
/** A person: their picture if they set one, else their callsign's flags. */
function avatar(cs, small = false) {
  const v = PICS.get(cs);
  if (v) {
    return h('span', { class: `avatar pic${small ? ' small' : ''}`, title: cs }, h('img', { src: `/pic/u/${cs}?v=${v}`, alt: '', loading: 'lazy', decoding: 'async' }));
  }
  const a = h('span', { class: `avatar${small ? ' small' : ''}`, 'aria-hidden': 'true', title: cs });
  a.innerHTML = [...(cs || '')].slice(0, 4).map((c) => flagSVG(c, true)).join('');
  return a;
}
/** A crew: its picture, else one flag from each of up to four members. */
function crewAvatar(conv, small = false) {
  if (conv.pic) {
    return h('span', { class: `avatar pic crew${small ? ' small' : ''}` }, h('img', { src: `/pic/c/${conv.c}?v=${conv.pic}`, alt: '', loading: 'lazy', decoding: 'async' }));
  }
  const a = h('span', { class: `avatar crew${small ? ' small' : ''}`, 'aria-hidden': 'true' });
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
      const res = await fetch('/api/me', { cache: 'no-store' }).catch(() => null);
      if (res && res.status === 401) return signedOut();
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
  let res;
  try {
    res = await fetch(path, {
      method,
      headers: body ? { 'Content-Type': 'application/json' } : undefined,
      body: body ? JSON.stringify(body) : undefined,
      cache: 'no-store',
    });
  } catch {
    const msg = location.hostname.endsWith('.trycloudflare.com')
      ? t('Cannot reach this Star. Quick-tunnel links (*.trycloudflare.com) change when the tunnel restarts — open the current link.')
      : t('Cannot reach the relay. Check your connection and address.');
    throw Object.assign(new Error(msg), { code: 0 });
  }
  const j = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(j.e || t('Request failed ({0})', res.status)), { code: res.status, data: j });
  return j;
}

// Display choices that follow the account to every device and Star: language, theme, the two sound switches.
// (Notification permission is per device, so it is not here.) `pf` keys on the wire are short: l, t, s, o.
const PF = { l: 'lang', t: 'theme', s: 'sound', o: 'soundOpen' };

/** Save changed choices with the account. Fire and forget: the local choice already took effect. */
function savePf(change) {
  const pf = {};
  for (const [k, key] of Object.entries(PF)) if (key in change) pf[k] = change[key];
  return S.me ? rpc('me_set', { pf }).catch(() => {}) : Promise.resolve();
}

/**
 * Take the account's choices as the truth (they win over this browser's), at login and whenever another device
 * changes them. Returns true when the language changed and the page must reload to show it. A new account
 * with nothing saved yet is seeded once from this browser's explicit choices, so its other devices follow.
 */
function applyPf(pf, live = false) {
  if (!pf) {
    if (live) return false; // a push never seeds; only the login does
    const seed = {};
    if (prefs.lang !== 'auto') seed.lang = prefs.lang;
    if (prefs.theme !== 'auto') seed.theme = prefs.theme;
    if (Object.keys(seed).length) savePf(seed);
    return false;
  }
  const before = pickLang();
  let themed = false;
  for (const [k, key] of Object.entries(PF)) {
    if (!(k in pf) || prefs[key] === pf[k]) continue;
    prefs[key] = pf[k];
    if (key === 'theme') themed = true;
  }
  savePrefs();
  if (themed) applyTheme();
  return pickLang() !== before;
}

async function hello() {
  const res = await rpc('hello');
  S.me = res.me;
  if (applyPf(S.me.pf)) return location.reload(); // another language: reload once; it matches next time
  S.rin = res.rin;
  S.rout = res.rout;
  S.relays = res.relays;
  S.signup = res.signup;
  S.mi = res.mi;
  S.gmax = res.gmax;
  S.tmax = res.tmax;
  S.skew = res.now - Date.now();
  S.convs = new Map(res.convs.map((c) => [c.c, c]));
  notePics();
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
      notePics();
      if (!old && m.c.me === 'i') {
        alertNew({ kind: 'knock', who: crewAvatar(m.c, true), title: t('Crew invite'), body: t('{0} invites you to {1}', m.c.byn || m.c.by, m.c.n), go: '' });
      }
      if (S.view?.c === m.c.c) S.view.header?.();
      break;
    }
    case 'cgone':
      S.convs.delete(m.c);
      S.msgs.delete(m.c);
      if (parseRoute().c === m.c) go('');
      break;
    case 'rin': {
      // A knock is pushed again when the knocker changes their picture or name: update, no alert.
      const i = S.rin.findIndex((x) => x.r === m.r.r);
      if (i >= 0) S.rin[i] = m.r;
      else {
        S.rin.push(m.r);
        alertNew({ kind: 'knock', who: null, title: t('Knock knock'), body: t('{0} wishes to communicate with you', m.r.n ? `${m.r.n} | ${m.r.f}` : m.r.f), go: '' });
      }
      notePics();
      break;
    }
    case 'rout':
      if (!S.rout.some((x) => x.r === m.r.r)) S.rout.push(m.r);
      break;
    case 'rgone':
      S.rin = S.rin.filter((x) => x.r !== m.r);
      S.rout = S.rout.filter((x) => x.r !== m.r);
      break;
    case 'me':
      S.me = m.me;
      applyPf(m.me.pf, true); // theme and sounds follow another device at once; a new language waits for the next load
      notePics();
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
// Alerts: sounds, browser notifications, cards in the page, the tab icon
// ============================================================================================

// Sounds are synthesized with Web Audio: no audio files. Browsers only allow sound after the
// first click or key press on the page, so the context is created (or resumed) then.
let actx = null;
function unlockAudio() {
  if (!actx) {
    const C = window.AudioContext || window.webkitAudioContext;
    if (C) actx = new C();
  }
  if (actx?.state === 'suspended') actx.resume().catch(() => {});
}
addEventListener('pointerdown', unlockAudio, { capture: true, passive: true });
addEventListener('keydown', unlockAudio, { capture: true });

function tone(f, at, dur, vol, type = 'sine') {
  const o = actx.createOscillator();
  const g = actx.createGain();
  o.type = type;
  o.frequency.setValueAtTime(f, at);
  g.gain.setValueAtTime(0.0001, at);
  g.gain.exponentialRampToValueAtTime(vol, at + 0.012);
  g.gain.exponentialRampToValueAtTime(0.0001, at + dur);
  o.connect(g).connect(actx.destination);
  o.start(at);
  o.stop(at + dur + 0.05);
}
/** 'msg': two-note chime. 'open': one short note for the conversation on screen. 'knock': knock-knock. */
function chime(kind) {
  if (!actx || actx.state !== 'running') return;
  const t0 = actx.currentTime + 0.01;
  if (kind === 'knock') {
    for (const at of [0, 0.17]) {
      tone(170, t0 + at, 0.11, 0.55, 'triangle');
      tone(90, t0 + at, 0.14, 0.45);
    }
  } else if (kind === 'open') {
    tone(880, t0, 0.2, 0.13);
    tone(1760, t0, 0.12, 0.03);
  } else {
    tone(784, t0, 0.34, 0.16);
    tone(1175, t0 + 0.11, 0.44, 0.12);
  }
}

/** A system notification (shown by the service worker, so it also works on Android). */
async function sysNotify(title, body, go) {
  if (!prefs.notify || !('Notification' in window) || Notification.permission !== 'granted') return;
  const opts = { body: (body || '').slice(0, 140), tag: go || 'esc', renotify: true, icon: '/icon.svg', badge: '/icon.svg', data: { go: go || '' } };
  try {
    const reg = await navigator.serviceWorker?.getRegistration();
    if (reg) return void (await reg.showNotification(title, opts));
    const n = new Notification(title, opts);
    n.onclick = () => {
      focus();
      location.hash = go || '';
      n.close();
    };
  } catch {
    /* notifications blocked on this platform */
  }
}
navigator.serviceWorker?.addEventListener('message', (e) => {
  if (e.data && typeof e.data.go === 'string') location.hash = e.data.go;
});

/** A card at the top of the page saying who wrote what; click it to open the conversation. */
function pageAlert({ kind, who, title, body, go }) {
  let box = $('.pings');
  if (!box) document.body.append((box = h('div', { class: 'pings', 'aria-live': 'polite' })));
  const card = h(
    'button',
    {
      class: `ping${kind === 'knock' ? ' knock' : ''}`,
      type: 'button',
      onclick: () => {
        card.remove();
        if (go != null) location.hash = go;
      },
    },
    who || h('span', { class: 'ping-ic' }, icon('door')),
    h('span', { class: 'txt' }, h('b', { translate: 'no' }, title), h('span', { translate: 'no' }, body)),
  );
  box.prepend(card);
  while (box.children.length > 3) box.lastChild.remove();
  setTimeout(() => card.classList.add('out'), 6000);
  setTimeout(() => card.remove(), 6400);
}

/** Something new arrived. Pick the sound, the card and the system notification by context. */
function alertNew({ kind, who, title, body, go, open }) {
  const hidden = document.visibilityState !== 'visible';
  const away = hidden || !document.hasFocus();
  if (open && !away) {
    if (prefs.soundOpen) chime('open');
    return;
  }
  if (prefs.sound) chime(kind === 'knock' ? 'knock' : 'msg');
  if (away) sysNotify(title, body, go);
  if (!hidden && !open) pageAlert({ kind, who, title, body, go });
}

// The tab icon gets a red dot while something is unread.
const ICON_SVG = (dot) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" rx="112" fill="#f1f4f6"/><rect x="100" y="92" width="16" height="328" rx="8" fill="#102131"/><rect x="116" y="112" width="150" height="200" fill="#f7c600"/><rect x="266" y="112" width="150" height="200" fill="#1747a6"/><rect x="116.5" y="112.5" width="299" height="199" fill="none" stroke="#000" stroke-opacity=".18" stroke-width="3"/>${dot ? '<circle cx="392" cy="392" r="104" fill="#d22630" stroke="#f1f4f6" stroke-width="28"/>' : ''}</svg>`;
let iconDot = null;
function setFavicon(dot) {
  if (dot === iconDot) return;
  iconDot = dot;
  const link = $('link[rel=icon]');
  if (link) link.href = `data:image/svg+xml,${encodeURIComponent(ICON_SVG(dot))}`;
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
    let total = S.rin.length;
    for (const c of S.convs.values()) total += c.me === 'i' ? 1 : c.u || 0;
    document.title = total ? `(${total}) ESC` : 'ESC';
    setFavicon(total > 0);
  });
}

function displayName(cs, conv) {
  return S.me?.nick?.[cs] || conv?.m?.find((m) => m.a === cs)?.n || cs;
}
function convTitle(conv) {
  if (conv.g) return conv.n || t('Crew');
  const other = conv.m?.find((m) => m.a !== S.me.id);
  return other ? displayName(other.a, conv) : t('Line');
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
  const ts = tankState();
  if (ts.alert) {
    const fresh = !sessionStorage.getItem('esc-red');
    box.append(
      h(
        'div',
        { class: `banner red${fresh ? ' fresh' : ''}`, role: 'alert' },
        flags('NC'),
        h('p', null, h('b', null, t('Red Alert.')), ' ', t('Your Tank is {0}% full. New messages with files will probably NOT be received.', Math.floor(ts.pct * 100))),
        h('button', { class: 'btn small solid', type: 'button', onclick: () => go('/room') }, t('Open the Cleaning Room')),
        h('button', { class: 'btn small', type: 'button', onclick: () => setAutoRecycle(true) }, t('Turn on auto-recycling')),
      ),
    );
    if (fresh) {
      sessionStorage.setItem('esc-red', '1');
      redAlertDialog(ts.pct);
    }
  } else if (m.cl) {
    box.append(
      h(
        'div',
        { class: 'banner amber' },
        icon('bot', 'i'),
        h('p', null, t('Dustin comes by in {0} to bring your Tank back under {1}. Make Keepers of anything you want to keep.', until(m.cl), bytes(m.q))),
        h('button', { class: 'btn small', type: 'button', onclick: () => go('/room') }, t('See what goes')),
      ),
    );
  }
  if (prefs.notify && 'Notification' in window && Notification.permission === 'default' && !prefs.askedNotify) {
    box.append(
      h(
        'div',
        { class: 'banner quiet' },
        icon('bell', 'i'),
        h('p', null, t('Get a notification when a message arrives while ESC is in the background?')),
        h('button', { class: 'btn small primary', type: 'button', onclick: () => askNotify(true) }, t('Turn on notifications')),
        h('button', { class: 'btn small ghost', type: 'button', onclick: () => askNotify(false) }, t('Not now')),
      ),
    );
  }
  if (!S.online) box.append(h('div', { class: 'banner quiet' }, t('Reconnecting…')));
}

async function askNotify(yes) {
  prefs.askedNotify = true;
  savePrefs();
  if (yes) {
    const p = await Notification.requestPermission().catch(() => 'denied');
    prefs.notify = p === 'granted';
    savePrefs();
    toast(p === 'granted' ? t('Notifications are on.') : t('Notifications stay off. You can allow them later in the browser’s site settings.'));
  }
  paint();
  S.view?.update?.('prefs');
}

function redAlertDialog(pct) {
  const d = h(
    'dialog',
    { class: 'alarm' },
    h(
      'div',
      { class: 'body' },
      flags('NC', 'big'),
      h('h2', null, t('Red Alert')),
      h('p', null, t('Your Tank is {0}% full and auto-recycling is off.', Math.floor(pct * 100))),
      h('p', null, h('b', null, t('New messages with files will probably NOT be received.')), ' ', t('Text messages always arrive.')),
      h(
        'div',
        { class: 'foot' },
        h('button', { class: 'btn ghost', type: 'button', onclick: () => d.close() }, t('I understand')),
        h('button', { class: 'btn', type: 'button', onclick: () => (d.close(), setAutoRecycle(true)) }, t('Turn on auto-recycling')),
        h('button', { class: 'btn', type: 'button', onclick: () => (d.close(), go('/room')) }, t('Open the Cleaning Room')),
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
    toast(on ? t('Auto-recycling is on. Dustin keeps your Tank under its size.') : t('Auto-recycling is off.'));
  } catch (e) {
    toast(err(e));
  }
}

function tankEl() {
  const m = S.me;
  const ts = tankState();
  const g = h('span', { class: `gauge${ts.alert ? ' alert' : ts.warn ? ' warn' : ''}` });
  const fill = h('i');
  fill.style.width = `${Math.min(100, ts.pct * 100)}%`;
  const m80 = h('em');
  m80.style.left = '80%';
  const m95 = h('em');
  m95.style.left = '95%';
  g.append(fill, m80, m95);
  const note = m.ar
    ? m.cl
      ? t('Dustin comes by in {0}', until(m.cl))
      : t('Auto-recycling on: Dustin keeps it under {0}', bytes(m.q))
    : ts.alert
      ? t('Red Alert: new files will probably not arrive')
      : ts.warn
        ? t('Getting full. Free some space soon.')
        : t('Auto-recycling is off');
  return h(
    'button',
    { class: 'tank', type: 'button', onclick: () => go('/room'), title: t('Open the Cleaning Room') },
    h('span', { class: 'top' }, h('span', null, t('Tank')), h('span', null, h('b', null, bytes(m.u)), ' ', t('of {0}', bytes(m.q)))),
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
      { class: 'who', type: 'button', onclick: () => callsignDialog(), title: t('Your callsign') },
      avatar(me.id),
      h('span', { class: 'grow' }, h('span', { class: 'callsign', translate: 'no' }, me.id), h('span', { class: 'name', translate: 'no' }, me.n || t('Add your name in Settings'))),
    ),
    iconBtn(themeIcon(), `${t(THEMES[prefs.theme])}. ${t('Change theme')}`, cycleTheme),
    iconBtn('settings', t('Settings'), () => go('/settings')),
  );
  const actions = h(
    'div',
    { class: 'actions' },
    h('button', { class: 'btn primary', type: 'button', onclick: () => knockDialog() }, icon('door', 'i'), t('Knock')),
    h('button', { class: 'btn', type: 'button', onclick: () => crewDialog() }, icon('crew', 'i'), t('New crew')),
  );
  const list = h('div', { class: 'list' });
  const invites = [...S.convs.values()].filter((c) => c.me === 'i');
  if (S.rin.length || invites.length) {
    list.append(h('h2', null, t('At your door')));
    for (const k of S.rin) list.append(knockCard(k));
    for (const c of invites) list.append(inviteCard(c));
  }
  const convs = [...S.convs.values()].filter((c) => c.me === 'j').sort((a, b) => (b.l?.t || b.t || 0) - (a.l?.t || a.t || 0));
  if (convs.length) {
    if (S.rin.length || invites.length) list.append(h('h2', null, t('Lines and crews')));
    for (const c of convs) list.append(convItem(c, route.c === c.c));
  } else if (!S.rin.length && !invites.length) {
    list.append(
      h(
        'div',
        { class: 'empty' },
        h('p', null, t('Nobody here yet. Knock on someone’s callsign to open a Line, or share yours so they can knock on you.')),
        h('div', { class: 'row' }, flags(me.id), h('span', { class: 'callsign', translate: 'no' }, me.id)),
        h('button', { class: 'btn', type: 'button', onclick: () => callsignDialog() }, icon('share', 'i'), t('Share my callsign')),
      ),
    );
  }
  if (S.rout.length) {
    list.append(h('h2', null, t('Waiting at their door')));
    for (const k of S.rout) {
      list.append(
        h(
          'div',
          { class: 'waiting' },
          avatar(k.to, true),
          h('span', { class: 'grow' }, h('span', { class: 'callsign', translate: 'no' }, k.to), ' ', t('knocked at {0}', when(k.t))),
          h('button', { class: 'btn small ghost', type: 'button', onclick: () => rpc('knock_cancel', { r: k.r }).catch((e) => toast(err(e))) }, t('Cancel')),
        ),
      );
    }
  }
  const place = (path, ic, label) => h('a', { href: `#${path}`, 'aria-current': route.path === path ? 'true' : null }, icon(ic), label);
  const nav = h(
    'nav',
    { class: 'places', 'aria-label': t('Places') },
    place('/stash', 'stash', t('Stash')),
    place('/room', 'room', t('Cleaning Room')),
    me.adm ? place('/tower', 'tower', t('Control Tower')) : null,
  );
  const keep = rail.querySelector('.list')?.scrollTop || 0;
  rail.replaceChildren(head, tankEl(), actions, list, nav);
  list.scrollTop = keep;
}

/** The list's second line: who said what last. */
function lastLine(c) {
  const last = c.l;
  if (!last) return c.g ? t('{0} aboard', c.m?.filter((m) => m.s === 'j').length || 1) : t('Line open. Say hello.');
  const who = last.f === S.me.id ? t('You: ') : c.g ? `${displayName(last.f, c)}: ` : '';
  const what = last.x || (last.v ? t('Voice message') : last.k ? filesWord(last.k) : '');
  return who + what;
}

function convItem(c, current) {
  const other = c.g ? null : convOther(c);
  const title = convTitle(c);
  return h(
    'a',
    { class: `item${c.flash ? ' flash' : ''}`, href: `#/l/${c.c}`, 'aria-current': current ? 'true' : null },
    c.g ? crewAvatar(c) : avatar(other || ''),
    h(
      'span',
      { class: 'txt' },
      h(
        'span',
        { class: 't1' },
        h('b', { translate: 'no' }, title, other && title !== other ? h('span', { class: 'cs' }, ` | ${other}`) : null),
        h('time', null, when(c.l?.t || c.t)),
      ),
      h('span', { class: 't2' }, h('span', { class: 'snip', translate: 'no' }, lastLine(c)), c.u ? h('span', { class: 'badge' }, c.u > 99 ? '99+' : c.u) : null),
    ),
  );
}

function knockCard(k) {
  const answer = (a) => async () => {
    try {
      const res = await rpc('answer', { r: k.r, a });
      if (res?.c) go(`/l/${res.c}`);
    } catch (e) {
      toast(err(e));
    }
  };
  return h(
    'div',
    { class: 'knock-card' },
    h('div', { class: 'row' }, avatar(k.f), h('div', { class: 'grow' }, h('b', { translate: 'no' }, k.n || t('Someone')), h('div', { class: 'callsign', translate: 'no' }, k.f))),
    k.x ? h('p', { class: 'note', translate: 'no' }, k.x) : null,
    h('span', { class: 'meaning' }, flags('K'), `“${t(MEANING.K)}”`),
    h(
      'div',
      { class: 'row' },
      h('button', { class: 'btn primary small', type: 'button', onclick: answer('accept') }, flags('C'), t('Open the door')),
      h('button', { class: 'btn small', type: 'button', onclick: answer('decline'), title: t('They are not told') }, flags('N'), t('Ignore')),
      h('button', { class: 'btn small ghost danger', type: 'button', onclick: answer('block'), title: t('They can never knock again, and are not told') }, t('Bolt')),
    ),
  );
}

function inviteCard(c) {
  const answer = (ok) => async () => {
    try {
      await rpc('crew_answer', { c: c.c, ok });
      if (ok) go(`/l/${c.c}`);
    } catch (e) {
      toast(err(e));
    }
  };
  return h(
    'div',
    { class: 'knock-card' },
    h(
      'div',
      { class: 'row' },
      avatar(c.by),
      h('div', { class: 'grow' }, h('b', { translate: 'no' }, c.n), h('div', { class: 'hint', translate: 'no' }, t('Crew invite from {0}', c.byn ? `${c.byn} | ${c.by}` : c.by), ' · ', t('{0} aboard', c.cnt))),
    ),
    h(
      'div',
      { class: 'row' },
      h('button', { class: 'btn primary small', type: 'button', onclick: answer(true) }, flags('C'), t('Join the crew')),
      h('button', { class: 'btn small', type: 'button', onclick: answer(false) }, flags('N'), t('Ignore')),
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
    h('div', { class: 'home-inner' }, h('div', { class: 'halyard' }, ...['E', 'S', 'C'].map((c) => flagNode(c))), h('p', null, t('Pick a Line or a Crew on the left. Or knock on a callsign to start something new.'))),
  );
}
function back() {
  return h('button', { class: 'icon-btn back', type: 'button', 'aria-label': t('Back'), onclick: () => go('') }, icon('back'));
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
  const voice = m.a?.length === 1 && isVoice(m.a[0]);
  c.l = { t: m.t, f: m.f, x: (m.x || '').slice(0, 120), k: m.a?.length || 0, v: voice };
  c.t = m.t;
  if (m.f === S.me.id) return;
  const open = S.view?.c === m.c;
  if (open && document.visibilityState === 'visible') markRead(c, m.t);
  else {
    c.u = (c.u || 0) + 1;
    c.flash = true;
    setTimeout(() => (c.flash = false), 1600);
  }
  const title = c.g ? `${displayName(m.f, c)} · ${convTitle(c)}` : `${convTitle(c)} | ${m.f}`;
  const body = m.x || (voice ? t('Voice message') : filesWord(m.a?.length || 0));
  alertNew({ kind: 'msg', who: c.g ? crewAvatar(c, true) : avatar(m.f, true), title, body, go: `/l/${m.c}`, open });
}

let readTimer;
function markRead(c, ts) {
  if (ts <= (c.rd || 0)) return;
  c.rd = ts;
  c.u = 0;
  clearTimeout(readTimer);
  readTimer = setTimeout(() => rpc('read', { c: c.c, t: ts }).catch(() => {}), 600);
}

async function loadHistory(c, older) {
  let box = S.msgs.get(c);
  if (!box) S.msgs.set(c, (box = { list: [], more: true, loading: false }));
  if (box.loading || (older && !box.more)) return;
  box.loading = true;
  try {
    const before = older && box.list.length ? box.list[0].t : undefined;
    const res = await rpc('hist', { c, b: before, n: 60 });
    const known = new Set(box.list.map((m) => m.id));
    box.list = [...res.m.filter((m) => !known.has(m.id)), ...box.list].sort((a, b) => a.t - b.t);
    box.more = res.more;
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

/** The inline player for voice messages and audio files. Nothing loads until Play. */
function audioEl(a) {
  const voice = isVoice(a);
  const audio = new Audio();
  audio.preload = 'none';
  let loaded = false;
  const load = () => {
    if (!loaded) {
      audio.src = fileUrl(a);
      loaded = true;
    }
  };
  // Recorded WebM has no duration in its header; the sender measured it (a.d, in ms).
  const total = () => (isFinite(audio.duration) && audio.duration > 0 ? audio.duration : (a.d || 0) / 1000);
  const btn = h('button', { class: 'vplay', type: 'button', 'aria-label': t('Play') }, icon('play'));
  const pos = h('input', { type: 'range', class: 'vbar', min: 0, max: 1000, value: 0, step: 1, 'aria-label': t('Position') });
  const time = h('span', { class: 'vtime' }, a.d ? mmss(a.d / 1000) : '');
  const setPos = () => {
    const d = total();
    pos.value = d > 0 ? Math.min(1000, (audio.currentTime / d) * 1000) : 0;
    time.textContent = audio.currentTime > 0 ? `${mmss(audio.currentTime)} / ${mmss(d)}` : mmss(d);
  };
  btn.addEventListener('click', () => {
    load();
    if (audio.paused) audio.play().catch((e) => toast(err(e)));
    else audio.pause();
  });
  audio.addEventListener('play', () => {
    btn.replaceChildren(icon('pause'));
    btn.setAttribute('aria-label', t('Pause'));
  });
  const stopped = () => {
    btn.replaceChildren(icon('play'));
    btn.setAttribute('aria-label', t('Play'));
  };
  audio.addEventListener('pause', stopped);
  audio.addEventListener('ended', () => {
    stopped();
    audio.currentTime = 0;
    setPos();
  });
  audio.addEventListener('timeupdate', setPos);
  audio.addEventListener('error', () => {
    stopped();
    toast(t('This browser can’t play this recording. Download it instead.'));
  });
  pos.addEventListener('input', () => {
    load();
    const d = total();
    if (d > 0) audio.currentTime = (pos.value / 1000) * d;
  });
  return h(
    'div',
    { class: `voice${voice ? '' : ' track'}` },
    btn,
    h('div', { class: 'vmid' }, voice ? null : h('span', { class: 'vname', translate: 'no' }, a.n), pos),
    time,
    h('a', { class: 'icon-btn small', href: `${fileUrl(a)}?dl=1`, title: t('Download'), 'aria-label': t('Download') }, icon('download')),
  );
}

function attachmentsEl(m, conv) {
  const wrap = h('div', { class: `atts${m.a.length === 1 ? ' one' : ''}` });
  const visuals = m.a.filter((a) => a.s === 'ok' && a.th && ['image', 'video'].includes(kindOf(a.m, a.n)));
  for (const a of m.a) {
    const kind = kindOf(a.m, a.n);
    if (m.pending) {
      if (a.local) wrap.append(h('div', { class: 'thumb' }, h('img', { src: a.local, alt: '' })));
      else {
        const voice = kind === 'audio' && isVoice(a);
        wrap.append(
          h('div', { class: 'filecard' }, h('span', { class: 'ext' }, kind === 'audio' ? icon('mic') : ext(a.n)), h('span', { class: 'info' }, h('b', { translate: 'no' }, voice ? t('Voice message') : a.n), h('small', null, voice && a.d ? `${mmss(a.d / 1000)} · ${bytes(a.z)}` : bytes(a.z)))),
        );
      }
      continue;
    }
    if (a.s !== 'ok') {
      wrap.append(goneCard(a));
      continue;
    }
    if (kind === 'audio') wrap.append(audioEl(a));
    else if (a.th && (kind === 'image' || kind === 'video')) {
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
  const opens = kind === 'video' || a.m === 'application/pdf' || a.m === 'text/plain' || kind === 'image';
  return h(
    'a',
    { class: 'filecard', href: opens ? fileUrl(a) : `${fileUrl(a)}?dl=1`, target: opens ? '_blank' : null, rel: 'noopener' },
    h('span', { class: 'ext' }, ext(a.n)),
    h('span', { class: 'info' }, h('b', { translate: 'no' }, a.n), h('small', null, bytes(a.cz || a.z))),
  );
}

function goneCard(a) {
  const label = { ref: t('Bounced: your Tank was full'), rec: t('Recycled by Dustin'), del: t('Deleted by you'), gone: t('Not yours to open') }[a.s] || t('Unavailable');
  const action = a.s === 'ref' ? t('Catch it') : a.s === 'rec' || a.s === 'del' ? t('Rescue') : null;
  return h(
    'div',
    { class: 'filecard gone' },
    h('span', { class: 'ext' }, kindOf(a.m, a.n) === 'audio' ? icon('mic') : ext(a.n)),
    h('span', { class: 'info' }, h('b', { translate: 'no' }, isVoice(a) ? t('Voice message') : a.n), h('small', null, `${label} · ${bytes(a.cz || a.z)}`)),
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
                toast(was === 'ref' ? t('Caught it.') : t('Rescued.'));
              } catch (e) {
                toast(e.code === 410 ? t('Nobody holds that file any more.') : err(e));
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
    const res = await rpc('files');
    S.files = res;
    const st = new Map(res.f.map((f) => [f.b, f.s]));
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
    /* offline: the next hello refreshes */
  }
}

// Voice messages: Opus, mono, 24 kbps, about 180 KB a minute. The container is whatever this
// browser records Opus in (WebM in Chrome and Firefox, MP4 in Safari).
const VOICE_TYPES = ['audio/webm;codecs=opus', 'audio/ogg;codecs=opus', 'audio/mp4;codecs=opus', 'audio/webm', 'audio/mp4'];
const VOICE_BPS = 24000;
const VOICE_MAX_S = 15 * 60;

function lineView(pane, conv) {
  const c = conv.c;
  const head = h('header', { class: 'bar' });
  const list = h('div', { class: 'msgs', role: 'log', 'aria-live': 'polite' });
  const tray = h('div', { class: 'tray', hidden: true });
  const traySum = h('div', { class: 'tray-sum', hidden: true });
  const ta = h('textarea', { rows: 1, placeholder: t('Message'), 'aria-label': t('Message'), maxlength: S.tmax });
  const picker = h('input', { type: 'file', multiple: true, hidden: true });
  const items = [];
  let atBottom = true;
  let recording = null;

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
        h('b', { translate: 'no' }, convTitle(cv)),
        h('small', null, cv.g ? (invited ? t('Crew · {0} aboard, {1} invited', joined, invited) : t('Crew · {0} aboard', joined)) : h('span', { class: 'callsign', translate: 'no' }, other || '')),
      ),
      iconBtn('info', cv.g ? t('Crew details') : t('Line details'), () => (cv.g ? crewInfo(cv) : lineInfo(cv))),
    );
  }

  function render() {
    const box = S.msgs.get(c);
    if (!box) return;
    const keepFromBottom = list.scrollHeight - list.scrollTop;
    const cv = S.convs.get(c) || conv;
    list.replaceChildren();
    if (box.more) list.append(h('button', { class: 'btn small ghost more', type: 'button', onclick: () => older() }, t('Show earlier messages')));
    if (!box.list.length) list.append(h('div', { class: 'day' }, cv.g ? t('This crew is quiet. Say something.') : t('The door is open. Say hello.')));
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
    if (!mine && cv.g && !cont) b.append(h('span', { class: 'from', translate: 'no' }, displayName(m.f, cv), h('span', { class: 'cs' }, ` | ${m.f}`)));
    if (m.a?.length) b.append(attachmentsEl(m, cv));
    if (m.x) b.append(h('div', { class: 'text', translate: 'no' }, linkify(m.x)));
    let meta;
    if (m.pending) {
      meta = m.failed
        ? h('button', { class: 'btn small danger', type: 'button', onclick: () => deliver(m) }, t('Not sent. Tap to retry'))
        : h('span', { class: 'meta sending' }, m.progress != null ? t('Sending {0}%', Math.round(m.progress * 100)) : t('Sending…'));
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
          h('span', { translate: 'no' }, it.file.name),
          h('span', null, bytes(it.file.size)),
          h(
            'button',
            {
              class: 'x',
              type: 'button',
              'aria-label': t('Remove {0}', it.file.name),
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
    traySum.textContent =
      `${filesWord(items.length)}, ${bytes(total)}.` +
      (total > room ? ` ${t('This is more than your Tank has room for.')}` : m.ar && m.u + total > m.q ? ` ${t('Dustin will recycle your oldest files to make room.')}` : '');
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
  const dropHint = h('div', { class: 'drop', hidden: true }, t('Drop to add to the tray'));
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

  /** Show a message at once (optimistic), then upload and send it. */
  function queue(text, sending) {
    const k = Math.floor(Math.random() * 2 ** 52);
    const box = S.msgs.get(c);
    const tmp = {
      id: `tmp-${k}`,
      k,
      c,
      f: S.me.id,
      t: Date.now() + S.skew,
      x: text,
      a: sending.map((it) => ({ n: it.file.name, m: it.file.type, z: it.file.size, d: it.dur, s: 'ok', local: it.kind === 'image' ? it.url : null })),
      pending: true,
      items: sending,
    };
    box.list.push(tmp);
    atBottom = true;
    render();
    deliver(tmp);
  }

  function sendNow() {
    const text = ta.value;
    if (!text.trim() && !items.length) return;
    const sending = items.splice(0);
    ta.value = '';
    ta.style.height = 'auto';
    paintTray();
    queue(text, sending);
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
          it.meta = await uploadFile(
            it.file,
            (p) => {
              tmp.progress = (done + p * it.file.size) / total;
              render();
            },
            it.dur ? { d: it.dur } : {},
          );
        }
        done += it.file.size;
        metas.push(it.meta);
      }
      tmp.progress = null;
      const res = await rpc('send', { c, x: tmp.x, a: metas, k: tmp.k });
      const box = S.msgs.get(c);
      const i = box.list.indexOf(tmp);
      if (box.list.some((m) => m.id === res.id)) {
        if (i >= 0) box.list.splice(i, 1);
      } else if (i >= 0) Object.assign(tmp, { id: res.id, t: res.t, pending: false });
      for (const it of tmp.items) if (it.url) setTimeout(() => URL.revokeObjectURL(it.url), 30000);
      render();
      if (metas.length) refreshFileStates();
    } catch (e) {
      tmp.failed = true;
      render();
      toast(err(e));
    }
  }

  // ---- voice messages ----
  async function startVoice() {
    if (recording) return;
    unlockAudio();
    if (!navigator.mediaDevices?.getUserMedia || !window.MediaRecorder) return toast(t('This browser can’t record audio.'));
    let stream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
    } catch {
      return toast(t('No microphone, or ESC isn’t allowed to use it. Check the browser’s site settings.'));
    }
    const type = VOICE_TYPES.find((x) => MediaRecorder.isTypeSupported(x)) || '';
    let rec;
    try {
      rec = new MediaRecorder(stream, { ...(type ? { mimeType: type } : {}), audioBitsPerSecond: VOICE_BPS });
    } catch {
      stream.getTracks().forEach((tr) => tr.stop());
      return toast(t('This browser can’t record audio.'));
    }
    const chunks = [];
    let started = 0;
    let keep = false;
    const clock = h('span', { class: 'rec-time' }, '0:00');
    const stop = (send) => {
      keep = send;
      if (rec.state !== 'inactive') rec.stop();
    };
    const timer = setInterval(() => {
      if (!started) return;
      const s = (performance.now() - started) / 1000;
      clock.textContent = mmss(s);
      if (s >= VOICE_MAX_S) stop(true);
    }, 250);
    const recBar = h(
      'div',
      { class: 'recbar', role: 'status' },
      h('span', { class: 'rec-dot', 'aria-hidden': 'true' }),
      clock,
      h('span', { class: 'grow hint' }, t('Recording a voice message…')),
      h('button', { class: 'btn', type: 'button', onclick: () => stop(false) }, t('Discard')),
      h('button', { class: 'btn primary', type: 'button', onclick: () => stop(true) }, icon('send', 'i'), t('Send')),
    );
    rec.onstart = () => (started = performance.now());
    rec.ondataavailable = (e) => {
      if (e.data.size) chunks.push(e.data);
    };
    rec.onstop = () => {
      clearInterval(timer);
      stream.getTracks().forEach((tr) => tr.stop());
      recBar.remove();
      composer.hidden = false;
      recording = null;
      const dur = started ? Math.round(performance.now() - started) : 0;
      if (!keep || !chunks.length || dur < 400) return;
      const base = (rec.mimeType || type || 'audio/webm').split(';')[0];
      const extn = base.includes('ogg') ? 'ogg' : base.includes('mp4') ? 'm4a' : 'webm';
      const stamp = new Date().toISOString().slice(0, 19).replace(/[-:T]/g, '');
      const file = new File(chunks, `voice-${stamp}.${extn}`, { type: base });
      queue('', [{ file, url: null, kind: 'audio', dur }]);
    };
    recording = rec;
    rec.start(1000);
    composer.hidden = true;
    composer.after(recBar);
  }

  const send = h('button', { class: 'btn primary', type: 'button', 'aria-label': t('Send'), onclick: sendNow }, icon('send', 'i'));
  const composer = h(
    'form',
    { class: 'composer', onsubmit: (e) => (e.preventDefault(), sendNow()) },
    iconBtn('clip', t('Attach files'), () => picker.click()),
    ta,
    window.MediaRecorder ? iconBtn('mic', t('Record a voice message'), startVoice, 'mic') : null,
    send,
    picker,
  );
  header();
  pane.append(head, list, tray, traySum, composer, dropHint);

  const view = {
    c,
    header,
    render,
    update(kind) {
      if (kind === 'conv' || kind === 'me') header();
      if (kind === 'me' && items.length) paintTray();
    },
    leave() {
      for (const it of items) if (it.url) URL.revokeObjectURL(it.url);
      if (recording && recording.state !== 'inactive') {
        recording.onstop = null;
        recording.stop();
        recording.stream.getTracks().forEach((tr) => tr.stop());
      }
    },
  };
  if (S.msgs.has(c)) render();
  else {
    list.append(h('div', { class: 'day' }, t('Loading…')));
    loadHistory(c, false).then(render, (e) => toast(err(e)));
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
    x.onload = () => (x.status === 200 ? ok(x.response) : no(Object.assign(new Error(x.response?.e || t('Upload failed ({0})', x.status)), { status: x.status, data: x.response })));
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
      const res = await xhrPut(`/api/up/${u}?o=${o}`, blob.slice(o, end), (n) => onp((o + n) / Math.max(1, blob.size)));
      if (res.b) {
        onp(1);
        return res;
      }
      o = res.o;
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
    const bmp = await createImageBitmap(file);
    return { src: bmp, w: bmp.width, h: bmp.height, done: () => bmp.close() };
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

function canvasOf(W, H) {
  return typeof OffscreenCanvas !== 'undefined' ? new OffscreenCanvas(W, H) : Object.assign(document.createElement('canvas'), { width: W, height: H });
}
async function encodeCanvas(canvas, q) {
  const out = (type) => (canvas.convertToBlob ? canvas.convertToBlob({ type, quality: q }) : new Promise((res) => canvas.toBlob(res, type, q)));
  let blob = await out('image/webp');
  if (!blob || blob.type !== 'image/webp') blob = await out('image/jpeg');
  return blob;
}
async function encodeScaled(src, w, hh, max, q) {
  const s = Math.min(1, max / Math.max(w, hh));
  const W = Math.max(1, Math.round(w * s)), H = Math.max(1, Math.round(hh * s));
  const canvas = canvasOf(W, H);
  const g = canvas.getContext('2d');
  g.imageSmoothingQuality = 'high';
  g.drawImage(src, 0, 0, W, H);
  return encodeCanvas(canvas, q);
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
async function uploadFile(file, onp, extra = {}) {
  const pv = await previews(file).catch(() => null);
  const sizes = [file.size, pv?.thumb?.size || 0, pv?.preview?.size || 0];
  const total = sizes.reduce((a, b) => a + b, 0) || 1;
  let base = 0;
  const step = (n) => (p) => onp(Math.min(1, (base + p * n) / total));
  const meta = { n: file.name || 'file', m: (file.type || 'application/octet-stream').split(';')[0], z: file.size, ...extra };
  const o = await uploadBlob(file, step(file.size));
  meta.b = o.b;
  base += file.size;
  if (pv?.thumb) {
    const th = await uploadBlob(pv.thumb, step(pv.thumb.size));
    meta.t = th.b;
    meta.tz = th.z;
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

// ---- pictures (profile and crew) ----

/** Ask for one file; must be called straight from a click. Resolves null if cancelled. */
function pickFile(accept) {
  return new Promise((resolve) => {
    const input = h('input', { type: 'file', accept, hidden: true });
    const done = (f) => {
      resolve(f);
      input.remove();
    };
    input.addEventListener('change', () => done(input.files?.[0] || null));
    input.addEventListener('cancel', () => done(null));
    document.body.append(input);
    input.click();
  });
}

/** Centre-cropped square, at most 256 px, WebP when the browser can (usually 10 to 40 KB). */
async function squarePicture(file) {
  const d = await decodeImage(file);
  if (!d) throw new Error('This browser can’t read that picture.');
  try {
    const side = Math.min(d.w, d.h);
    const size = Math.min(256, side);
    const canvas = canvasOf(size, size);
    const g = canvas.getContext('2d');
    g.imageSmoothingQuality = 'high';
    g.drawImage(d.src, (d.w - side) / 2, (d.h - side) / 2, side, side, 0, 0, size, size);
    return await encodeCanvas(canvas, 0.82);
  } finally {
    d.done();
  }
}

/** Pick, crop and save a picture: the profile's, or a crew's when `conv` is given. */
async function choosePicture(conv) {
  const file = await pickFile('image/*');
  if (!file) return false;
  try {
    const blob = await squarePicture(file);
    const res = await fetch(`/api/pic${conv ? `?c=${conv}` : ''}`, { method: 'PUT', body: blob, cache: 'no-store' });
    const j = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(j.e || t('Request failed ({0})', res.status));
    toast(t('Picture saved.'));
    return true;
  } catch (e) {
    toast(err(e));
    return false;
  }
}
async function removePicture(conv) {
  try {
    await rpc('pic_clear', conv ? { c: conv } : {});
    toast(t('Picture removed.'));
    return true;
  } catch (e) {
    toast(err(e));
    return false;
  }
}

// ============================================================================================
// Peek: the image viewer
// ============================================================================================

function peek(list, startB) {
  if (!list.length) return;
  let i = Math.max(0, list.findIndex((a) => a.b === startB));
  let zoom = 1, tx = 0, ty = 0;
  const img = h('img', { alt: '' });
  const note = h('div', { class: 'peek-note', hidden: true });
  const title = h('div', { class: 'title' });
  const keepBtn = iconBtn('anchor', t('Make it a Keeper'), async () => {
    const a = list[i];
    const f = S.files?.f?.find((x) => x.b === a.b);
    const on = !f?.pin;
    try {
      await rpc('file_op', { o: on ? 'pin' : 'unpin', b: [a.b] });
      toast(on ? t('Kept. Dustin will never recycle it.') : t('No longer a Keeper.'));
    } catch (e) {
      toast(err(e));
    }
  });
  const dl = h('a', { class: 'icon-btn', title: t('Download'), 'aria-label': t('Download') }, icon('download'));
  const stage = h('div', { class: 'peek-stage' }, img, note);
  const prev = h('button', { class: 'peek-nav prev', type: 'button', 'aria-label': t('Previous') }, icon('back'));
  const next = h('button', { class: 'peek-nav next', type: 'button', 'aria-label': t('Next') }, h('span', { class: 'flip' }, icon('send')));
  const d = h(
    'dialog',
    { class: 'peek', 'aria-label': t('Picture') },
    h('div', { class: 'peek-wrap' }, h('div', { class: 'peek-bar' }, iconBtn('close', t('Close'), () => d.close()), title, keepBtn, dl), stage),
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
    title.replaceChildren(h('b', { translate: 'no' }, a.n), h('small', null, `${bytes(a.z)}${a.w ? ` · ${a.w} × ${a.h}` : ''}`));
    dl.href = `${fileUrl(a)}?dl=1`;
    let triedPreview = false;
    img.onerror = () => {
      if (a.pv && !triedPreview) {
        triedPreview = true;
        img.src = fileUrl(a, 'p');
        note.textContent = t('Showing a preview: this browser can’t open the original format. Download keeps the original.');
        note.hidden = false;
      } else {
        note.textContent = t('This browser can’t show this picture. Download it instead.');
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
    drag = { x: e.clientX, y: e.clientY, tx, ty };
    stage.setPointerCapture(e.pointerId);
  });
  stage.addEventListener('pointermove', (e) => {
    if (!drag || zoom <= 1) return;
    tx = drag.tx + e.clientX - drag.x;
    ty = drag.ty + e.clientY - drag.y;
    apply();
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
        h('h2', null, t('Your callsign')),
        flags(me.id, 'big'),
        h('span', { class: 'callsign big-cs', translate: 'no' }, me.id),
        h('p', { class: 'hint' }, t('Your callsign is the only way to reach you on ESC. Share it with people you want to talk to: they knock, you decide whether to open the door.')),
        h(
          'div',
          { class: 'foot' },
          h('button', { class: 'btn', type: 'button', onclick: () => navigator.clipboard?.writeText(me.id).then(() => toast(t('Callsign copied.'))) }, icon('copy', 'i'), t('Copy callsign')),
          h(
            'button',
            {
              class: 'btn primary',
              type: 'button',
              onclick: async () => {
                if (navigator.share) {
                  try {
                    await navigator.share({ title: t('My ESC callsign'), text: t('Knock on me on ESC: {0}', me.id), url: link });
                  } catch {
                    /* closed */
                  }
                } else {
                  await navigator.clipboard?.writeText(link);
                  toast(t('Link copied.'));
                }
              },
            },
            icon('share', 'i'),
            t('Share a link'),
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
    'aria-label': t('Callsign'),
  });
  input.addEventListener('input', () => {
    const v = cleanCS(input.value);
    preview.innerHTML = [...v].map((ch) => flagSVG(ch)).join('');
    onChange?.(v);
  });
  return { input, preview };
}

function knockDialog(prefill = '') {
  dialog((d) => {
    const errEl = h('p', { class: 'err' });
    const { input, preview } = callsignInput(() => (errEl.textContent = ''));
    const note = h('textarea', { rows: 3, maxlength: 280, placeholder: t('Optional: a few words so they know it’s you') });
    const form = h(
      'form',
      {
        onsubmit: async (e) => {
          e.preventDefault();
          const to = cleanCS(input.value);
          if (!CS_RE.test(to)) {
            errEl.textContent = t('Callsigns are 4 characters from 2-9 and B C D F G H J K M N P Q R S T V W X Z.');
            return;
          }
          if (to === S.me.id) {
            errEl.textContent = t('That’s your own callsign.');
            return;
          }
          try {
            const res = await rpc('knock', { to, x: note.value });
            d.close();
            if (res.c) go(`/l/${res.c}`);
            else toast(t('Knocked on {0}. If they open the door, your Line appears here.', to));
          } catch (ex) {
            errEl.textContent = err(ex);
          }
        },
      },
      h('h2', null, t('Knock on a callsign')),
      h('div', { class: 'row' }, flags('K'), h('span', { class: 'hint' }, t('Flag K: “{0}.”', t(MEANING.K)))),
      h('label', { class: 'field' }, h('span', null, t('Callsign')), input),
      preview,
      h('label', { class: 'field' }, h('span', null, t('Note')), note),
      h('p', { class: 'hint' }, t('They see your callsign, your name, your picture and this note. You only hear back if they open the door.')),
      errEl,
      h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, t('Cancel')), h('button', { class: 'btn primary', type: 'submit' }, t('Knock'))),
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
  const errEl = h('p', { class: 'err' });
  function paintChips() {
    chips.replaceChildren(
      ...list.map((cs, i) =>
        h('span', { class: 'c', translate: 'no' }, flags(cs), cs, h('button', { type: 'button', 'aria-label': t('Remove {0}', cs), onclick: () => (list.splice(i, 1), paintChips()) }, '×')),
      ),
    );
  }
  function take() {
    const v = cleanCS(input.value);
    if (!v) return true;
    if (!CS_RE.test(v)) {
      errEl.textContent = t('{0} is not a callsign.', v);
      return false;
    }
    if (v === S.me.id) {
      errEl.textContent = t('You are already aboard.');
      return false;
    }
    if (!list.includes(v)) {
      if (list.length >= max) {
        errEl.textContent = t('A crew holds at most {0} people.', S.gmax);
        return false;
      }
      list.push(v);
    }
    input.value = '';
    preview.replaceChildren();
    errEl.textContent = '';
    paintChips();
    return true;
  }
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ',' || e.key === ' ') {
      e.preventDefault();
      take();
    }
  });
  return { el: h('div', { class: 'field' }, chips, input, preview, errEl), list, take };
}

function crewDialog() {
  dialog((d) => {
    const name = h('input', { type: 'text', maxlength: 80, placeholder: t('Family, Weekend trip, Night shift…'), required: true });
    const people = chipInput(S.gmax - 1);
    const errEl = h('p', { class: 'err' });
    d.append(
      h(
        'form',
        {
          onsubmit: async (e) => {
            e.preventDefault();
            if (!people.take()) return;
            try {
              const res = await rpc('crew_new', { n: name.value, ids: people.list });
              d.close();
              go(`/l/${res.c}`);
              if (people.list.length) toast(t('Crew created. People join when they accept.'));
            } catch (ex) {
              errEl.textContent = err(ex);
            }
          },
        },
        h('h2', null, t('New crew')),
        h('label', { class: 'field' }, h('span', null, t('Crew name')), name),
        h('label', { class: 'field' }, h('span', null, t('Invite callsigns (up to {0})', S.gmax - 1))),
        people.el,
        h('p', { class: 'hint' }, t('Nobody joins until they accept your invite. You can add a crew picture once it exists.')),
        errEl,
        h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, t('Cancel')), h('button', { class: 'btn primary', type: 'submit' }, t('Create crew'))),
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
    const errEl = h('p', { class: 'err' });
    const roleName = { o: t('Owner'), a: t('Admin'), m: '' };
    const members = conv.m.map((m) =>
      h(
        'div',
        { class: 'member' },
        avatar(m.a, true),
        h(
          'div',
          { class: 'txt' },
          h('b', { translate: 'no' }, m.a === S.me.id ? t('You') : displayName(m.a, conv)),
          h('small', null, h('span', { class: 'callsign', translate: 'no' }, m.a), m.s === 'i' ? ` · ${t('invited')}` : roleName[m.r] ? ` · ${roleName[m.r]}` : ''),
        ),
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
                    errEl.textContent = err(ex);
                  }
                },
              },
              m.s === 'i' ? t('Cancel invite') : t('Remove'),
            )
          : null,
        me?.r === 'o' && m.a !== S.me.id && m.s === 'j'
          ? h('button', { class: 'btn small ghost', type: 'button', onclick: () => rpc('crew_set', { c: conv.c, a: m.a, admin: m.r !== 'a' }).then(() => d.close(), (ex) => (errEl.textContent = err(ex))) }, m.r === 'a' ? t('Remove admin') : t('Make admin'))
          : null,
      ),
    );
    d.append(
      h(
        'div',
        { class: 'body' },
        h('div', { class: 'row' }, crewAvatar(conv), h('h2', { class: 'grow', translate: 'no' }, conv.n)),
        boss
          ? h(
              'div',
              { class: 'row' },
              h('button', { class: 'btn small', type: 'button', onclick: () => choosePicture(conv.c).then((ok) => ok && d.close()) }, icon('camera', 'i'), conv.pic ? t('Change crew picture') : t('Add a crew picture')),
              conv.pic ? h('button', { class: 'btn small ghost', type: 'button', onclick: () => removePicture(conv.c).then((ok) => ok && d.close()) }, t('Remove picture')) : null,
            )
          : null,
        boss
          ? h(
              'form',
              { class: 'row', onsubmit: (e) => (e.preventDefault(), rpc('crew_set', { c: conv.c, n: rename.value }).then(() => toast(t('Renamed.')), (ex) => (errEl.textContent = err(ex)))) },
              h('span', { class: 'grow' }, rename),
              h('button', { class: 'btn', type: 'submit' }, t('Rename')),
            )
          : null,
        h('div', null, ...members),
        h('label', { class: 'field' }, h('span', null, t('Invite more callsigns'))),
        people.el,
        errEl,
        h(
          'div',
          { class: 'foot' },
          h(
            'button',
            {
              class: 'btn danger',
              type: 'button',
              onclick: async () => {
                if (!confirm(t('Leave {0}? You stop getting its messages.', conv.n))) return;
                try {
                  await rpc('crew_leave', { c: conv.c });
                  d.close();
                  go('');
                } catch (ex) {
                  errEl.textContent = err(ex);
                }
              },
            },
            t('Leave crew'),
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
                  toast(t('Invited. They join when they accept.'));
                  d.close();
                } catch (ex) {
                  errEl.textContent = err(ex);
                }
              },
            },
            t('Send invites'),
          ),
        ),
      ),
    );
  });
}

function lineInfo(conv) {
  const other = convOther(conv);
  dialog((d) => {
    const nick = h('input', { type: 'text', maxlength: 40, value: S.me.nick?.[other] || '', placeholder: conv.m.find((m) => m.a === other)?.n || t('Nickname') });
    d.append(
      h(
        'div',
        { class: 'body' },
        h('div', { class: 'row' }, avatar(other), h('h2', { class: 'grow', translate: 'no' }, convTitle(conv))),
        flags(other, 'big'),
        h('span', { class: 'callsign', translate: 'no' }, other),
        h(
          'form',
          { class: 'field', onsubmit: (e) => (e.preventDefault(), rpc('nick', { a: other, n: nick.value }).then(() => (toast(t('Nickname saved.')), d.close()), (ex) => toast(err(ex)))) },
          h('span', null, t('Your private nickname for them')),
          h('div', { class: 'row' }, h('span', { class: 'grow' }, nick), h('button', { class: 'btn', type: 'submit' }, t('Save'))),
        ),
        h('p', { class: 'hint' }, t('Only you see nicknames.')),
        h(
          'div',
          { class: 'foot' },
          h(
            'button',
            {
              class: 'btn danger',
              type: 'button',
              onclick: async () => {
                if (!confirm(t('Bolt {0}? They can never knock on you again. They are not told.', other))) return;
                try {
                  await rpc('block', { a: other, on: true });
                  toast(t('{0} is bolted out.', other));
                  d.close();
                } catch (ex) {
                  toast(err(ex));
                }
              },
            },
            t('Bolt this callsign'),
          ),
          h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, t('Close')),
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
  if (f.d === 'drive') return t('Stash');
  const c = f.c?.length ? S.convs.get(f.c[0]) : null;
  return c ? convTitle(c) : f.d === 'sent' ? t('Sent') : t('Received');
};
const fileTitle = (f) => (isVoice(f) ? (f.du ? `${t('Voice message')} · ${mmss(f.du / 1000)}` : t('Voice message')) : f.n);

function stashView(pane) {
  const grid = h('div', { class: 'grid' });
  const filter = h('select', { 'aria-label': t('Show') }, h('option', { value: 'all' }, t('All my files')), h('option', { value: 'drive' }, t('My uploads')), h('option', { value: 'sent' }, t('Sent')), h('option', { value: 'recv' }, t('Received')));
  const sort = sortSelect();
  const search = h('input', { type: 'search', placeholder: t('Search names'), 'aria-label': t('Search names') });
  const picker = h('input', { type: 'file', multiple: true, hidden: true });
  const progress = h('span', { class: 'hint' });
  picker.addEventListener('change', async () => {
    const files = [...picker.files];
    picker.value = '';
    for (const [n, f] of files.entries()) {
      try {
        const meta = await uploadFile(f, (p) => (progress.textContent = t('Uploading {0} of {1}: {2}%', n + 1, files.length, Math.round(p * 100))));
        await rpc('stash_add', { f: meta });
      } catch (e) {
        toast(`${f.name}: ${err(e)}`);
      }
    }
    progress.textContent = '';
    await getFiles(true);
    paintGrid();
  });
  const opts = () => ({ show: filter.value, sort: sort.value, q: search.value, onlyActive: true });
  function paintGrid() {
    const fs = filterSort(S.files?.f || [], opts());
    grid.replaceChildren(...(fs.length ? fs.map((f) => tile(f)) : [h('div', { class: 'empty' }, h('p', null, t('Nothing here yet. Upload to your Stash, or send files in a Line or Crew.')))]));
  }
  function tile(f) {
    const kind = kindOf(f.m, f.n);
    const face = h('button', {
      class: 'face',
      type: 'button',
      title: f.n,
      onclick: () =>
        kind === 'image'
          ? peek(filterSort(S.files?.f || [], opts()).filter((x) => kindOf(x.m, x.n) === 'image'), f.b)
          : window.open(kind === 'video' || kind === 'audio' || f.m === 'application/pdf' ? fileUrl(f) : `${fileUrl(f)}?dl=1`, '_blank', 'noopener'),
    });
    face.append(f.th ? h('img', { src: fileUrl(f, 't'), alt: '', loading: 'lazy' }) : kind === 'audio' ? icon('mic') : ext(f.n));
    if (f.pin) face.append(h('span', { class: 'keep keep-mark' }, t('Keeper')));
    return h(
      'div',
      { class: 'tile' },
      face,
      h(
        'div',
        { class: 'cap' },
        h('div', null, h('b', { translate: 'no' }, fileTitle(f)), h('small', null, `${bytes(f.cz)} · ${when(f.t)} · ${placeName(f)}`)),
        iconBtn('anchor', f.pin ? t('Remove Keeper') : t('Make it a Keeper'), async () => {
          await rpc('file_op', { o: f.pin ? 'unpin' : 'pin', b: [f.b] }).catch((e) => toast(err(e)));
          await getFiles(true);
          paintGrid();
        }),
        iconBtn('send', t('Send to a Line or Crew'), () => sendToDialog(f)),
      ),
    );
  }
  for (const el of [filter, sort, search]) el.addEventListener('input', paintGrid);
  pane.append(
    bar(h('div', { class: 'title' }, h('b', null, t('Stash')), h('small', null, t('Every file you hold: your uploads and everything sent or received')))),
    h('div', { class: 'tools' }, h('button', { class: 'btn primary', type: 'button', onclick: () => picker.click() }, icon('plus', 'i'), t('Upload to Stash')), filter, sort, search, progress, picker),
    h('div', { class: 'scroll' }, grid),
  );
  grid.append(h('div', { class: 'empty' }, t('Loading…')));
  getFiles(true).then(paintGrid, (e) => toast(err(e)));
  return {
    update(kind) {
      if (kind === 'files') getFiles(true).then(paintGrid);
    },
  };
}

function sortSelect() {
  return h(
    'select',
    { 'aria-label': t('Sort') },
    h('option', { value: 'new' }, t('Newest first')),
    h('option', { value: 'old' }, t('Oldest first')),
    h('option', { value: 'big' }, t('Biggest first')),
    h('option', { value: 'small' }, t('Smallest first')),
    h('option', { value: 'name' }, t('Name A to Z')),
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
    name: (a, b) => a.n.localeCompare(b.n, LOCALE),
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
        h('h2', { translate: 'no' }, t('Send {0}', fileTitle(f))),
        h('p', { class: 'hint' }, t('It is sent from your Stash: nothing is uploaded again.')),
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
                    await rpc('send', { c: c.c, x: '', a: [{ b: f.b, n: f.n, m: f.m, z: f.z, ...(f.du ? { d: f.du } : {}) }], k: Math.floor(Math.random() * 2 ** 52) });
                    d.close();
                    toast(t('Sent to {0}.', convTitle(c)));
                  } catch (e) {
                    toast(err(e));
                  }
                },
              },
              c.g ? crewAvatar(c) : avatar(convOther(c) || ''),
              h('span', { class: 'txt' }, h('span', { class: 't1' }, h('b', { translate: 'no' }, convTitle(c)))),
            ),
          ),
        ),
        h('div', { class: 'foot' }, h('button', { class: 'btn', type: 'button', onclick: () => d.close() }, t('Close'))),
      ),
    );
  });
}

function roomView(pane) {
  const selected = new Set();
  const show = h('select', { 'aria-label': t('Where') }, h('option', { value: 'all' }, t('Everywhere')), h('option', { value: 'drive' }, t('Stash uploads')), h('option', { value: 'sent' }, t('Sent by me')), h('option', { value: 'recv' }, t('Received')));
  for (const c of [...S.convs.values()].filter((x) => x.me === 'j')) show.append(h('option', { value: `c:${c.c}` }, t('In {0}', convTitle(c))));
  const kind = h(
    'select',
    { 'aria-label': t('Type') },
    h('option', { value: 'all' }, t('All types')),
    h('option', { value: 'image' }, t('Pictures')),
    h('option', { value: 'video' }, t('Videos')),
    h('option', { value: 'audio' }, t('Audio and voice')),
    h('option', { value: 'doc' }, t('Documents')),
    h('option', { value: 'other' }, t('Other')),
  );
  const keep = h('select', { 'aria-label': t('Keepers') }, h('option', { value: 'all' }, t('Keepers and others')), h('option', { value: 'hide' }, t('Hide Keepers')), h('option', { value: 'only' }, t('Only Keepers')));
  const sort = sortSelect();
  sort.value = 'big';
  const search = h('input', { type: 'search', placeholder: t('Search names'), 'aria-label': t('Search names') });
  const body = h('div', { class: 'page' });
  const tbody = h('tbody');
  const selInfo = h('p');
  const selbar = h(
    'div',
    { class: 'selbar' },
    selInfo,
    h('button', { class: 'btn small', type: 'button', onclick: () => pickBiggest(10) }, t('Select the 10 biggest')),
    h('button', { class: 'btn small', type: 'button', onclick: () => op('pin') }, t('Make Keepers')),
    h('button', { class: 'btn small', type: 'button', onclick: () => op('unpin') }, t('Remove Keeper')),
    h('button', { class: 'btn small danger solid', type: 'button', onclick: () => op('delete') }, icon('trash', 'i'), t('Delete')),
  );
  const table = h(
    'table',
    { class: 'rows' },
    h('thead', null, h('tr', null, h('th', null, ''), h('th', null, ''), h('th', null, t('Name')), h('th', null, t('Size')), h('th', null, t('Added')), h('th', null, ''))),
    tbody,
  );
  let shown = [];
  function paintRoom() {
    const files = S.files?.f || [];
    shown = filterSort(files, { show: show.value, kind: kind.value, keep: keep.value, sort: sort.value, q: search.value, onlyActive: true });
    for (const b of [...selected]) if (!shown.some((f) => f.b === b)) selected.delete(b);
    const all = h('input', { type: 'checkbox', 'aria-label': t('Select all shown'), checked: shown.length > 0 && shown.every((f) => selected.has(f.b)) });
    all.addEventListener('change', () => {
      for (const f of shown) all.checked ? selected.add(f.b) : selected.delete(f.b);
      paintRoom();
    });
    tbody.replaceChildren(
      ...shown.map((f) => {
        const cb = h('input', { type: 'checkbox', checked: selected.has(f.b), 'aria-label': t('Select {0}', f.n) });
        const tr = h(
          'tr',
          { class: selected.has(f.b) ? 'sel' : null },
          h('td', null, cb),
          h('td', null, f.th ? h('img', { class: 'mini', src: fileUrl(f, 't'), alt: '', loading: 'lazy' }) : h('span', { class: 'mini' }, kindOf(f.m, f.n) === 'audio' ? icon('mic') : ext(f.n))),
          h('td', { class: 'name' }, h('div', { translate: 'no' }, isVoice(f) ? `${fileTitle(f)} · ${f.n}` : f.n), h('small', { class: 'hint', translate: 'no' }, placeName(f))),
          h('td', { class: 'num' }, bytes(f.cz)),
          h('td', { class: 'num' }, when(f.t)),
          h('td', null, f.pin ? h('span', { class: 'keep-mark' }, t('Keeper')) : (S.files?.queue || []).includes(f.b) ? h('span', { class: 'state-mark bad' }, t('Next for Dustin')) : ''),
        );
        cb.addEventListener('change', () => {
          cb.checked ? selected.add(f.b) : selected.delete(f.b);
          paintSel();
          tr.classList.toggle('sel', cb.checked);
        });
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
    selInfo.textContent = files.length
      ? t('{0} selected. Deleting frees {1}.', files.length, bytes(files.reduce((s, f) => s + f.cz, 0)))
      : t('{0} shown, {1}.', filesWord(shown.length), bytes(shown.reduce((s, f) => s + f.cz, 0)));
  }
  function pickBiggest(n) {
    [...shown].sort((a, b) => b.cz - a.cz).slice(0, n).forEach((f) => selected.add(f.b));
    paintRoom();
  }
  async function op(o) {
    if (!selected.size) return toast(t('Select some files first.'));
    if (o === 'delete' && !confirm(t('Delete {0} file(s) from your account? Others keep their copies.', selected.size))) return;
    try {
      await rpc('file_op', { o, b: [...selected] });
      if (o === 'delete') selected.clear();
      await getFiles(true);
      paintRoom();
      toast(o === 'delete' ? t('Deleted.') : o === 'pin' ? t('They are Keepers now.') : t('Keeper removed.'));
    } catch (e) {
      toast(err(e));
    }
  }
  const dustin = h('div', { class: 'dustin' });
  function paintDustin() {
    const m = S.me;
    const q = S.files?.queue || [];
    const qBytes = (S.files?.f || []).filter((f) => q.includes(f.b)).reduce((s, f) => s + f.cz, 0);
    let text;
    if (!m.ar) {
      text = [
        h('p', null, h('b', null, t('Auto-recycling is off.')), ' ', t('Your Tank holds {0}; at 95% you get a Red Alert and new files will probably not arrive.', bytes(m.q))),
        h('button', { class: 'btn small', type: 'button', onclick: () => setAutoRecycle(true) }, t('Turn on auto-recycling')),
      ];
    } else if (m.cl) {
      text = [
        h('p', null, h('b', null, t('Dustin comes by in {0}.', until(m.cl))), ' ', t('He will recycle the {0} oldest files that aren’t Keepers ({1}) to bring your Tank under {2}. Make Keepers of anything you want to keep.', q.length, bytes(qBytes), bytes(m.q))),
        h(
          'div',
          { class: 'row' },
          h('button', { class: 'btn small', type: 'button', onclick: () => ((show.value = 'all'), (keep.value = 'hide'), (sort.value = 'old'), paintRoom()) }, t('Show his list')),
          h(
            'button',
            {
              class: 'btn small danger',
              type: 'button',
              onclick: async () => {
                try {
                  const res = await rpc('file_op', { o: 'cleanup' });
                  toast(t('Dustin recycled {0} file(s).', res.n));
                  await getFiles(true);
                  paintRoom();
                } catch (e) {
                  toast(err(e));
                }
              },
            },
            t('Recycle now'),
          ),
        ),
      ];
    } else {
      text = [h('p', null, h('b', null, t('Dustin is on duty.')), ' ', t('When your Tank passes {0}, he recycles your oldest files first. Keepers are never touched.', bytes(m.q)), q.length ? ` ${t('Next up: {0} file(s).', q.length)}` : '')];
    }
    dustin.replaceChildren(icon('bot', 'bot'), h('div', { class: 'grow' }, ...text));
  }
  const lost = h('section');
  function paintLost() {
    const fs = (S.files?.f || []).filter((f) => f.s !== 'ok' && f.live);
    lost.hidden = !fs.length;
    lost.replaceChildren(
      h('h2', null, t('Bounced, recycled or deleted')),
      h('p', { class: 'hint' }, t('These are still held by someone else, so you can get them back while they last.')),
      ...fs.map((f) =>
        h(
          'div',
          { class: 'member' },
          h('span', { class: 'mini' }, kindOf(f.m, f.n) === 'audio' ? icon('mic') : ext(f.n)),
          h('div', { class: 'txt' }, h('b', { translate: 'no' }, fileTitle(f)), h('small', null, `${{ ref: t('Bounced'), rec: t('Recycled by Dustin'), del: t('Deleted by you') }[f.s]} · ${bytes(f.cz)} · ${placeName(f)}`)),
          h(
            'button',
            {
              class: 'btn small',
              type: 'button',
              onclick: async () => {
                try {
                  await rpc('file_op', { o: 'get', b: [f.b] });
                  setAttState(f.b, 'ok');
                  await getFiles(true);
                  paintRoom();
                } catch (e) {
                  toast(err(e));
                }
              },
            },
            f.s === 'ref' ? t('Catch it') : t('Rescue'),
          ),
        ),
      ),
    );
  }
  for (const el of [show, kind, keep, sort, search]) el.addEventListener('input', paintRoom);
  body.append(dustin, h('section', null, table), lost);
  const sub = h('small');
  pane.append(bar(h('div', { class: 'title' }, h('b', null, t('Cleaning Room')), sub)), h('div', { class: 'tools' }, show, kind, keep, sort, search), h('div', { class: 'scroll' }, body), selbar);
  const paintSub = () => (sub.textContent = S.me.pin ? t('Tank: {0} of {1}, {2} in Keepers', bytes(S.me.u), bytes(S.me.q), bytes(S.me.pin)) : t('Tank: {0} of {1}', bytes(S.me.u), bytes(S.me.q)));
  paintSub();
  getFiles(true).then(paintRoom, (e) => toast(err(e)));
  return {
    update(k) {
      paintSub();
      if (k === 'files') getFiles(true).then(paintRoom);
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
/** A setting with a label, a hint and an on/off switch. */
function toggleRow(label, hint, checked, onchange) {
  const sw = h('input', { type: 'checkbox', class: 'switch', checked, 'aria-label': label });
  sw.addEventListener('change', () => onchange(sw.checked, sw));
  return { row: h('div', { class: 'kv' }, h('div', null, h('b', null, label), hint ? h('span', { class: 'hint' }, hint) : null), sw), sw };
}
/** A row of buttons where exactly one is pressed. */
function segmented(label, options, value, onpick) {
  const box = h('div', { class: 'seg', role: 'group', 'aria-label': label });
  const paintSeg = (v) =>
    box.replaceChildren(
      ...options.map(([val, text]) =>
        h(
          'button',
          {
            type: 'button',
            'aria-pressed': String(v === val),
            onclick: () => {
              paintSeg(val);
              onpick(val);
            },
          },
          text,
        ),
      ),
    );
  paintSeg(value);
  return box;
}

function settingsView(pane) {
  const me = S.me;
  const page = h('div', { class: 'page' });
  const name = h('input', { type: 'text', maxlength: 40, value: me.n, placeholder: t('How people see you once they’re in') });
  const picBox = h('div', { class: 'row pic-row' });
  function paintPic() {
    picBox.replaceChildren(
      ...[
        avatar(S.me.id),
        h('button', { class: 'btn', type: 'button', onclick: () => choosePicture() }, icon('camera', 'i'), S.me.pic ? t('Change picture') : t('Add a picture')),
        S.me.pic ? h('button', { class: 'btn ghost', type: 'button', onclick: () => removePicture() }, t('Remove picture')) : null,
      ].filter(Boolean),
    );
  }
  const door = toggleRow(t('Accept knocks from new people'), t('When closed, nobody new can knock. People you already talk to can still invite you to crews.'), me.acc, (on) =>
    rpc('me_set', { acc: on }).then(() => toast(on ? t('Door open: people can knock.') : t('Door closed: nobody new can knock.')), (e) => toast(err(e))),
  );
  const ar = toggleRow(
    t('Let Dustin recycle old files'),
    `${t('When your Tank ({0}) is full, the oldest files that aren’t Keepers are recycled, so new files always arrive. Text never counts.', bytes(me.q))} ${t('Turning this on while over gives you time to pick Keepers first.')}`,
    me.ar,
    (on) => setAutoRecycle(on),
  );
  const notifyOn = () => prefs.notify && 'Notification' in window && Notification.permission === 'granted';
  const notifyRow = toggleRow(t('Browser notifications'), t('A notification when a message, knock or invite arrives while ESC is in the background.'), notifyOn(), async (on, sw) => {
    if (on && !('Notification' in window)) {
      sw.checked = false;
      return toast(t('This browser has no notifications for web pages. On iPhone, add ESC to the Home Screen first.'));
    }
    if (on && Notification.permission !== 'granted') {
      const p = await Notification.requestPermission().catch(() => 'denied');
      if (p !== 'granted') {
        sw.checked = false;
        toast(t('This browser blocks notifications for ESC. Allow them in the browser’s site settings.'));
        return;
      }
    }
    prefs.notify = on;
    prefs.askedNotify = true;
    savePrefs();
    paint();
  });
  const soundRow = toggleRow(t('Sound alerts'), t('A chime for new messages and a knock-knock for knocks and invites.'), prefs.sound, (on) => {
    prefs.sound = on;
    savePrefs();
    savePf({ sound: on });
    if (on) {
      unlockAudio();
      chime('msg');
    }
  });
  const soundOpenRow = toggleRow(t('Sounds in the open conversation'), t('A short tone when a message arrives in the Line or Crew on your screen.'), prefs.soundOpen, (on) => {
    prefs.soundOpen = on;
    savePrefs();
    savePf({ soundOpen: on });
    if (on) {
      unlockAudio();
      chime('open');
    }
  });
  const lifeline = h('div', { class: 'stack' });
  function paintLifeline() {
    const cur = S.me.rec;
    const email = h('input', { type: 'email', placeholder: t('you@example.com') });
    lifeline.replaceChildren(
      h('p', { class: 'hint' }, cur === 'email' ? t('Email Lifeline: a reset link goes to {0}.', S.me.em) : cur === 'code' ? t('Code Lifeline: you saved a recovery code.') : t('No Lifeline: if you forget your password, this account is lost for good.')),
      h('div', { class: 'row' }, h('span', { class: 'grow' }, email), h('button', { class: 'btn', type: 'button', onclick: () => rpc('lifeline', { k: 'email', e: email.value }).then(() => toast(t('Email Lifeline saved.')), (e) => toast(err(e))) }, t('Use this email'))),
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
                const res = await rpc('lifeline', { k: 'code' });
                dialog((d) =>
                  d.append(
                    h(
                      'div',
                      { class: 'body' },
                      h('h2', null, t('Your Lifeline code')),
                      h('p', null, t('Write it down or save it in a password manager. It is shown only now, and it works once.')),
                      h('div', { class: 'code-show', translate: 'no' }, res.code),
                      h(
                        'div',
                        { class: 'foot' },
                        h('button', { class: 'btn', type: 'button', onclick: () => navigator.clipboard?.writeText(res.code).then(() => toast(t('Copied.'))) }, t('Copy')),
                        h('button', { class: 'btn primary', type: 'button', onclick: () => d.close() }, t('I saved it')),
                      ),
                    ),
                  ),
                );
              } catch (e) {
                toast(err(e));
              }
            },
          },
          t('Make a Lifeline code'),
        ),
        cur !== 'none'
          ? h('button', { class: 'btn ghost danger', type: 'button', onclick: () => confirm(t('Remove your Lifeline? A forgotten password would then lose this account.')) && rpc('lifeline', { k: 'none' }).then(() => toast(t('Lifeline removed.')), (e) => toast(err(e))) }, t('Remove Lifeline'))
          : null,
      ),
    );
  }
  const devices = h('div');
  async function paintDevices() {
    const res = await rpc('devices').catch(() => ({ s: [] }));
    devices.replaceChildren(
      ...res.s
        .sort((a, b) => b.t - a.t)
        .map((s) =>
          h(
            'div',
            { class: 'member' },
            h('div', { class: 'txt' }, h('b', null, s.l || t('A device')), h('small', null, s.me ? t('This device') : t('Signed in {0}', when(s.t)))),
            s.me ? null : h('button', { class: 'btn small ghost danger', type: 'button', onclick: () => rpc('device_end', { h: s.h }).then(paintDevices, (e) => toast(err(e))) }, t('Sign out')),
          ),
        ),
    );
  }
  const bolted = h('div');
  function paintBolted() {
    const list = S.me.blk || [];
    bolted.replaceChildren(
      ...(list.length
        ? list.map((cs) => h('div', { class: 'member' }, avatar(cs, true), h('div', { class: 'txt' }, h('span', { class: 'callsign', translate: 'no' }, cs)), h('button', { class: 'btn small', type: 'button', onclick: () => rpc('block', { a: cs, on: false }).catch((e) => toast(err(e))) }, t('Unbolt'))))
        : [h('p', { class: 'hint' }, t('Nobody is bolted out.'))]),
    );
  }
  const pw = { cur: h('input', { type: 'password', autocomplete: 'current-password' }), n1: h('input', { type: 'password', autocomplete: 'new-password', minlength: 8 }), n2: h('input', { type: 'password', autocomplete: 'new-password' }) };
  const pwErr = h('p', { class: 'err' });
  const tickets = h('div', { class: 'stack' });
  const ticketSection = section(t('Tickets'), tickets);
  async function paintTickets() {
    if (!(S.mi > 0 || S.me.adm)) return (ticketSection.hidden = true);
    const res = await rpc('tickets').catch(() => ({ i: [], left: 0 }));
    tickets.replaceChildren(
      h('p', { class: 'hint' }, S.me.adm ? t('Tickets let new people create an account here.') : t('You can hand out {0} more ticket(s).', res.left)),
      ...res.i.map((tk) =>
        h(
          'div',
          { class: 'member' },
          h('div', { class: 'txt' }, h('b', { class: 'callsign', translate: 'no' }, tk.c), h('small', null, t('{0} use(s) left · until {1}', tk.u, fDate.format(new Date(tk.x))))),
          h('button', { class: 'btn small', type: 'button', onclick: () => navigator.clipboard?.writeText(`${location.origin}/#ticket=${tk.c}`).then(() => toast(t('Ticket link copied.'))) }, t('Copy link')),
        ),
      ),
      h('button', { class: 'btn', type: 'button', onclick: () => rpc('ticket_new', { u: 1, d: 7 }).then(paintTickets, (e) => toast(err(e))) }, t('Create a Ticket')),
    );
  }
  page.append(
    section(
      t('You'),
      h('div', { class: 'row' }, flags(me.id, 'big'), h('span', { class: 'callsign big-cs', translate: 'no' }, me.id)),
      picBox,
      h('form', { class: 'row', onsubmit: (e) => (e.preventDefault(), rpc('me_set', { n: name.value }).then(() => toast(t('Name saved.')), (ex) => toast(err(ex)))) }, h('span', { class: 'grow' }, name), h('button', { class: 'btn', type: 'submit' }, t('Save name'))),
      h('p', { class: 'hint' }, t('Your name and picture are only shown to people you share a Line or Crew with, and to people you knock on.')),
    ),
    section(
      t('Look and language'),
      h('div', { class: 'kv wrap' }, h('b', null, t('Theme')), segmented(t('Theme'), [['auto', t('Automatic')], ['light', t('Light')], ['dark', t('Dark')]], prefs.theme, (v) => ((prefs.theme = v), savePrefs(), savePf({ theme: v }), applyTheme(), paint()))),
      h(
        'div',
        { class: 'kv wrap' },
        h('b', null, t('Language')),
        segmented(t('Language'), [['auto', t('Like the browser')], ['pt', 'Português'], ['en', 'English']], prefs.lang, (v) => {
          prefs.lang = v;
          savePrefs();
          savePf({ lang: v }).then(() => location.reload()); // wait for the save, or the reload cancels it
        }),
      ),
    ),
    section(
      t('Alerts'),
      soundRow.row,
      soundOpenRow.row,
      notifyRow.row,
      h(
        'div',
        null,
        h(
          'button',
          {
            class: 'btn',
            type: 'button',
            onclick: () => {
              unlockAudio();
              setTimeout(() => chime('msg'), 30);
              pageAlert({ who: avatar(S.me.id, true), title: t('Test alert'), body: t('This is how a new message looks.'), go: null });
              sysNotify(t('Test alert'), t('This is how a new message looks.'), '');
            },
          },
          icon('bell', 'i'),
          t('Test the alerts'),
        ),
      ),
    ),
    section(t('Door'), door.row),
    section(t('Auto-recycling'), ar.row),
    section(t('Lifeline'), h('p', { class: 'hint' }, t('How you get back in if you forget your password.')), lifeline),
    section(
      t('Password'),
      h(
        'form',
        { class: 'stack', onsubmit: changePassword },
        h('label', { class: 'field' }, h('span', null, t('Current password')), pw.cur),
        h('label', { class: 'field' }, h('span', null, t('New password (at least 8 characters)')), pw.n1),
        h('label', { class: 'field' }, h('span', null, t('Repeat new password')), pw.n2),
        pwErr,
        h('div', null, h('button', { class: 'btn', type: 'submit' }, t('Change password'))),
      ),
    ),
    section(t('Devices'), h('p', { class: 'hint' }, t('Where you are signed in.')), devices),
    section(t('Bolted callsigns'), bolted),
    ticketSection,
    section(t('This device'), h('div', null, h('button', { class: 'btn danger', type: 'button', onclick: signOut }, t('Sign out')))),
  );
  async function changePassword(e) {
    e.preventDefault();
    pwErr.textContent = '';
    if (pw.n1.value.length < 8) return (pwErr.textContent = t('Use at least 8 characters.'));
    if (pw.n1.value !== pw.n2.value) return (pwErr.textContent = t('The new passwords don’t match.'));
    try {
      const { s } = await api('/api/pre', { id: S.me.id });
      const old = await deriveKey(pw.cur.value, unb64u(s));
      const salt = crypto.getRandomValues(new Uint8Array(16));
      const key = await deriveKey(pw.n1.value, salt);
      await rpc('password', { old: b64u(old), s: b64u(salt), k: b64u(key) });
      pw.cur.value = pw.n1.value = pw.n2.value = '';
      toast(t('Password changed. Your other devices were signed out.'));
      paintDevices();
    } catch (ex) {
      pwErr.textContent = err(ex);
    }
  }
  pane.append(bar(h('div', { class: 'title' }, h('b', null, t('Settings')))), h('div', { class: 'scroll' }, page));
  paintPic();
  paintLifeline();
  paintDevices();
  paintBolted();
  paintTickets();
  return {
    update(k) {
      if (k === 'me') {
        door.sw.checked = S.me.acc;
        ar.sw.checked = S.me.ar;
        paintPic();
        paintLifeline();
        paintBolted();
      }
      if (k === 'prefs') notifyRow.sw.checked = notifyOn();
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
  pane.append(bar(h('div', { class: 'title' }, h('b', null, t('Control Tower')), h('small', null, t('Run this relay and its Ring')))), h('div', { class: 'scroll' }, page));
  async function paintTower() {
    let tw;
    try {
      tw = await rpc('tower');
    } catch (e) {
      page.replaceChildren(h('p', { class: 'err' }, err(e)));
      return;
    }
    const s = tw.set;
    const numIn = (v, attrs = {}) => h('input', { type: 'number', value: v, min: 0, step: 'any', ...attrs });
    const f = {
      default_quota: numIn((s.default_quota / GB).toFixed(2)),
      flex_pct: numIn(s.flex_pct, { max: 100 }),
      cleanup_h: numIn(s.cleanup_h),
      reserve_pct: numIn(s.reserve_pct, { max: 90 }),
      reserve_min: numIn((s.reserve_min / GB).toFixed(1)),
      member_invites: numIn(s.member_invites),
      req_pending_max: numIn(s.req_pending_max, { min: 1 }),
      req_daily_max: numIn(s.req_daily_max, { min: 1 }),
      signup: h('select', null, ...['invite', 'open', 'closed'].map((v) => h('option', { value: v, selected: s.signup === v }, { invite: t('With a Ticket only'), open: t('Open to anyone'), closed: t('Closed') }[v]))),
    };
    const fieldRow = (label, el, hint) => h('label', { class: 'field' }, h('span', null, label), el, hint ? h('span', { class: 'hint' }, hint) : null);
    const setErr = h('p', { class: 'err' });
    const smtp = {
      host: h('input', { type: 'text', value: s['smtp.host'], placeholder: 'smtp.gmail.com' }),
      port: numIn(s['smtp.port'] || 465),
      user: h('input', { type: 'text', value: s['smtp.user'] }),
      pass: h('input', { type: 'password', placeholder: s['smtp.pass_set'] ? t('Saved. Type to replace.') : t('App password') }),
      from: h('input', { type: 'email', value: s['smtp.from'], placeholder: t('esc@your-domain') }),
      tls: h('select', null, h('option', { value: 'ssl', selected: s['smtp.tls'] !== 'starttls' }, t('SSL (usually port 465)')), h('option', { value: 'starttls', selected: s['smtp.tls'] === 'starttls' }, t('STARTTLS (usually port 587)'))),
    };
    const testTo = h('input', { type: 'email', placeholder: t('Send a test to…') });
    const room = tw.disk.room;
    const over = tw.committed > room + tw.stored;
    page.replaceChildren(
      section(
        t('Overview'),
        h(
          'div',
          { class: 'stats' },
          ...[
            [t('Free disk'), bytes(tw.disk.free)],
            [t('Room for files'), bytes(room)],
            [t('Files stored'), bytes(tw.stored)],
            [t('Tanks promised'), bytes(tw.committed)],
            [t('People'), String(tw.accounts.length)],
            [t('Online now'), String(tw.online)],
          ].map(([k, v]) => h('div', { class: 'stat' }, h('small', null, k), h('b', null, v))),
        ),
        over ? h('p', { class: 'hint' }, t('The Tanks you promised add up to more than the free disk. That’s fine while people don’t fill them; files stop arriving for everyone once the disk reserve is reached.')) : null,
      ),
      section(
        t('Ring rules'),
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
                toast(t('Saved.'));
                paintTower();
              } catch (ex) {
                setErr.textContent = err(ex);
              }
            },
          },
          fieldRow(t('Default Tank size (GB)'), f.default_quota, t('Text never counts. Each person can get their own size below.')),
          fieldRow(t('Spillway (% over the Tank)'), f.flex_pct, t('With auto-recycling on, a full Tank still takes new files up to this much over, so they always arrive.')),
          fieldRow(t('Dustin’s grace period (hours)'), f.cleanup_h, t('When someone turns auto-recycling back on while over, this is how long they get to pick Keepers.')),
          fieldRow(t('Disk reserve (%)'), f.reserve_pct),
          fieldRow(t('Disk reserve, at least (GB)'), f.reserve_min, t('Uploads stop before the disk gets this full.')),
          fieldRow(t('Sign-up'), f.signup),
          fieldRow(t('Tickets each member may hand out'), f.member_invites, t('0 means only admins create Tickets.')),
          fieldRow(t('Knocks waiting at once (per person)'), f.req_pending_max),
          fieldRow(t('Knocks per day (per person)'), f.req_daily_max),
          setErr,
          h('div', null, h('button', { class: 'btn primary', type: 'submit' }, t('Save ring rules'))),
        ),
      ),
      section(
        t('People'),
        h(
          'div',
          { class: 'table-wrap' },
          h(
            'table',
            { class: 'rows' },
            h('thead', null, h('tr', null, h('th', null, t('Callsign')), h('th', null, t('Tank used')), h('th', null, t('Own Tank (GB)')), h('th', null, t('Admin')), h('th', null, t('Disabled')), h('th', null, ''))),
            h(
              'tbody',
              null,
              ...tw.accounts.map((a) => {
                const q = h('input', { type: 'number', step: 'any', min: 0, value: a.qo ? (a.q / GB).toFixed(2) : '', placeholder: t('default') });
                q.style.width = '7em';
                const adm = h('input', { type: 'checkbox', checked: a.adm, disabled: a.id === S.me.id });
                const dis = h('input', { type: 'checkbox', checked: a.dis, disabled: a.id === S.me.id });
                return h(
                  'tr',
                  null,
                  h('td', null, h('div', { class: 'row' }, avatar(a.id, true), h('div', null, h('span', { class: 'callsign', translate: 'no' }, a.id), h('div', { class: 'hint', translate: 'no' }, a.n || '')))),
                  h('td', { class: 'num' }, t('{0} of {1}', bytes(a.u), bytes(a.q)), h('div', { class: 'hint' }, a.ar ? filesWord(a.files) : `${filesWord(a.files)}, ${t('recycling off')}`)),
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
                            toast(t('{0} updated.', a.id));
                            paintTower();
                          } catch (e) {
                            toast(err(e));
                          }
                        },
                      },
                      t('Save'),
                    ),
                  ),
                );
              }),
            ),
          ),
        ),
      ),
      section(
        t('Tickets'),
        h('p', { class: 'hint' }, t('A Ticket lets one new person create an account. Send them the link.')),
        ...tw.invites.map((i) =>
          h(
            'div',
            { class: 'member' },
            h('div', { class: 'txt' }, h('b', { class: 'callsign', translate: 'no' }, i.c), h('small', null, t('{0} use(s) left', i.u), i.adm ? `, ${t('makes an admin')}` : '', ' · ', i.x > 9e15 ? t('until used') : t('until {0}', fDate.format(new Date(i.x))), i.by ? ` · ${t('by {0}', i.by)}` : '')),
            h('button', { class: 'btn small', type: 'button', onclick: () => navigator.clipboard?.writeText(`${location.origin}/#ticket=${i.c}`).then(() => toast(t('Ticket link copied.'))) }, t('Copy link')),
            h('button', { class: 'btn small ghost danger', type: 'button', onclick: () => rpc('ticket_del', { c: i.c }).then(paintTower, (e) => toast(err(e))) }, t('Tear up')),
          ),
        ),
        h(
          'div',
          { class: 'row' },
          h('button', { class: 'btn', type: 'button', onclick: () => rpc('ticket_new', { u: 1, d: 7 }).then(paintTower, (e) => toast(err(e))) }, t('Create a Ticket')),
          h('button', { class: 'btn ghost', type: 'button', onclick: () => rpc('ticket_new', { u: 1, d: 7, adm: true }).then(paintTower, (e) => toast(err(e))) }, t('Create an admin Ticket')),
        ),
      ),
      section(
        t('Email Lifelines'),
        h('p', { class: 'hint' }, t('To email password-reset links, the relay needs an email account to send from (for Gmail, use an app password).')),
        fieldRow(t('SMTP server'), smtp.host),
        fieldRow(t('Port'), smtp.port),
        fieldRow(t('Security'), smtp.tls),
        fieldRow(t('User'), smtp.user),
        fieldRow(t('Password'), smtp.pass),
        fieldRow(t('Send as'), smtp.from),
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
                  toast(t('Mail settings saved.'));
                  paintTower();
                } catch (e) {
                  toast(err(e));
                }
              },
            },
            t('Save mail settings'),
          ),
          h('span', { class: 'grow' }, testTo),
          h('button', { class: 'btn', type: 'button', onclick: () => rpc('tower_mail_test', { to: testTo.value }).then(() => toast(t('Test email sent.')), (e) => toast(err(e))) }, t('Send test')),
        ),
      ),
      section(
        t('The Ring'),
        h('p', { class: 'hint' }, t('Every Star (relay) keeps a full copy of everything. Add another computer to the Ring for redundancy.')),
        ...tw.relays.map((rl) => h('div', { class: 'member' }, h('span', { class: 'flags' }, '★'), h('div', { class: 'txt' }, h('b', { translate: 'no' }, rl.me ? t('{0} (this Star)', rl.n) : rl.n), h('small', { translate: 'no' }, rl.u || t('no public address yet'))))),
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
                      h('h2', null, t('Light a new Star')),
                      h('p', null, t('On the new computer, copy the esc program, then run (one line):')),
                      h('div', { class: 'code-show', translate: 'no' }, `esc join ${url} ${sp.spark} --url <${t('its public address')}>`),
                      h('p', { class: 'hint' }, t('The Spark works once, within 24 hours. Then run "esc serve" there: it copies the whole Ring and joins in.')),
                      h('div', { class: 'foot' }, h('button', { class: 'btn primary', type: 'button', onclick: () => d.close() }, t('Done'))),
                    ),
                  ),
                );
              } catch (e) {
                toast(err(e));
              }
            },
          },
          t('Light a new Star'),
        ),
      ),
    );
  }
  paintTower();
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
  const info = (S.info = await api('/api/info', null, 'GET').catch(() => ({ signup: 'invite', star: '', mail: false, offline: true })));
  const params = new URLSearchParams(location.hash.slice(1));
  const ticket = params.get('ticket') || '';
  const lifelineTok = params.get('lifeline') || '';
  const share = cleanCS(location.hash.slice(1));
  if (CS_RE.test(share)) sessionStorage.setItem('esc-knock', share);
  let mode = lifelineTok ? 'reset' : ticket ? 'create' : 'signin';
  const box = h('div', { class: 'panel' });
  const offlineNote = info.offline
    ? h(
        'p',
        { class: 'err' },
        location.hostname.endsWith('.trycloudflare.com')
          ? t('Cannot reach this Star. Quick-tunnel links (*.trycloudflare.com) change when the tunnel restarts — open the current link.')
          : t('Cannot reach the relay. Check your connection and address.'),
      )
    : null;
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': t('What do you want to do') });
  function paintSeg() {
    seg.replaceChildren(
      ...[
        ['signin', t('Sign in')],
        ['create', t('New account')],
        ['life', t('Lost password')],
      ].map(([m, label]) => h('button', { type: 'button', 'aria-pressed': String(mode === m || (m === 'life' && mode === 'reset')), onclick: () => ((mode = m), paintWelcome()) }, label)),
    );
  }
  function paintWelcome() {
    paintSeg();
    // Not offline: `offlineNote` is null, and replaceChildren(null) would print the word "null" on the screen.
    box.replaceChildren(...[offlineNote, seg, mode === 'signin' ? signIn() : mode === 'create' ? create() : mode === 'reset' ? resetByLink() : lostPassword()].filter(Boolean));
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
    const errEl = h('p', { class: 'err' });
    const btn = h('button', { class: 'btn primary', type: 'submit' }, t('Sign in'));
    setTimeout(() => cs.focus(), 30);
    return h(
      'form',
      {
        class: 'stack',
        onsubmit: async (e) => {
          e.preventDefault();
          errEl.textContent = '';
          const id = cleanCS(cs.value);
          if (!CS_RE.test(id)) return (errEl.textContent = t('Type your 4-character callsign.'));
          const done = busy(btn, t('Checking…'));
          try {
            const { s } = await api('/api/pre', { id });
            const k = await deriveKey(pw.value, unb64u(s));
            await api('/api/login', { id, k: b64u(k) });
            start();
          } catch (ex) {
            errEl.textContent = err(ex);
            done();
          }
        },
      },
      h('label', { class: 'field' }, h('span', null, t('Callsign')), cs),
      preview,
      h('label', { class: 'field' }, h('span', null, t('Password')), pw),
      errEl,
      btn,
    );
  }
  function create() {
    const tk = h('input', { type: 'text', value: ticket, placeholder: t('Ticket code'), autocapitalize: 'characters' });
    const name = h('input', { type: 'text', maxlength: 40, placeholder: t('Optional') });
    const p1 = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8, required: true });
    const p2 = h('input', { type: 'password', autocomplete: 'new-password', required: true });
    const errEl = h('p', { class: 'err' });
    const btn = h('button', { class: 'btn primary', type: 'submit' }, t('Create my account'));
    const needTicket = info.signup !== 'open';
    return h(
      'form',
      {
        class: 'stack',
        onsubmit: async (e) => {
          e.preventDefault();
          errEl.textContent = '';
          if (p1.value.length < 8) return (errEl.textContent = t('Use at least 8 characters for the password.'));
          if (p1.value !== p2.value) return (errEl.textContent = t('The passwords don’t match.'));
          const done = busy(btn, t('Creating…'));
          try {
            const salt = crypto.getRandomValues(new Uint8Array(16));
            const k = await deriveKey(p1.value, salt);
            const res = await api('/api/signup', { t: tk.value.trim().toUpperCase(), s: b64u(salt), k: b64u(k), n: name.value });
            history.replaceState(null, '', location.pathname);
            revealCallsign(res.id);
          } catch (ex) {
            errEl.textContent = err(ex);
            done();
          }
        },
      },
      info.signup === 'closed' ? h('p', { class: 'hint' }, t('Sign-up is closed here. Only admin Tickets work.')) : null,
      needTicket || ticket ? h('label', { class: 'field' }, h('span', null, needTicket ? t('Ticket') : t('Ticket (optional)')), tk) : null,
      h('label', { class: 'field' }, h('span', null, t('Your name')), name, h('span', { class: 'hint' }, t('Only people you talk to see it.'))),
      h('label', { class: 'field' }, h('span', null, t('Password (at least 8 characters)')), p1),
      h('label', { class: 'field' }, h('span', null, t('Repeat password')), p2),
      h('p', { class: 'hint' }, t('You get a 4-character callsign. It is the only way people can reach you.')),
      errEl,
      btn,
    );
  }
  function lostPassword() {
    const { input: cs, preview } = callsignInput();
    const code = h('input', { type: 'text', placeholder: 'XXXXX-XXXXX-XXXXX-XXXXX', autocapitalize: 'characters', spellcheck: 'false' });
    const p1 = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8 });
    const errEl = h('p', { class: 'err' });
    const ok = h('p', { class: 'hint' });
    return h(
      'div',
      { class: 'stack' },
      h('label', { class: 'field' }, h('span', null, t('Callsign')), cs),
      preview,
      h('p', { class: 'hint' }, t('If you set an Email Lifeline, we send a reset link. If you saved a Lifeline code, type it with a new password.')),
      info.mail
        ? h(
            'button',
            {
              class: 'btn',
              type: 'button',
              onclick: async () => {
                errEl.textContent = '';
                try {
                  await api('/api/lifeline/email', { id: cleanCS(cs.value) });
                  ok.textContent = t('If that callsign has an Email Lifeline, a link is on its way. It works for 30 minutes.');
                } catch (ex) {
                  errEl.textContent = err(ex);
                }
              },
            },
            t('Email me a reset link'),
          )
        : null,
      h('label', { class: 'field' }, h('span', null, t('Lifeline code')), code),
      h('label', { class: 'field' }, h('span', null, t('New password')), p1),
      errEl,
      ok,
      h(
        'button',
        {
          class: 'btn primary',
          type: 'button',
          onclick: async () => {
            errEl.textContent = '';
            if (p1.value.length < 8) return (errEl.textContent = t('Use at least 8 characters.'));
            try {
              const salt = crypto.getRandomValues(new Uint8Array(16));
              const k = await deriveKey(p1.value, salt);
              await api('/api/lifeline/code', { id: cleanCS(cs.value), code: code.value, s: b64u(salt), k: b64u(k) });
              toast(t('Password reset. Your Lifeline code is used up: make a new one in Settings.'));
              start();
            } catch (ex) {
              errEl.textContent = err(ex);
            }
          },
        },
        t('Reset with the code'),
      ),
    );
  }
  function resetByLink() {
    const p1 = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8 });
    const p2 = h('input', { type: 'password', autocomplete: 'new-password' });
    const errEl = h('p', { class: 'err' });
    return h(
      'form',
      {
        class: 'stack',
        onsubmit: async (e) => {
          e.preventDefault();
          if (p1.value.length < 8) return (errEl.textContent = t('Use at least 8 characters.'));
          if (p1.value !== p2.value) return (errEl.textContent = t('The passwords don’t match.'));
          try {
            const salt = crypto.getRandomValues(new Uint8Array(16));
            const k = await deriveKey(p1.value, salt);
            await api('/api/lifeline/token', { t: lifelineTok, s: b64u(salt), k: b64u(k) });
            history.replaceState(null, '', location.pathname);
            start();
          } catch (ex) {
            errEl.textContent = err(ex);
          }
        },
      },
      h('h2', null, t('Choose a new password')),
      h('label', { class: 'field' }, h('span', null, t('New password')), p1),
      h('label', { class: 'field' }, h('span', null, t('Repeat')), p2),
      errEl,
      h('button', { class: 'btn primary', type: 'submit' }, t('Save the new password')),
    );
  }
  function revealCallsign(id) {
    box.replaceChildren(
      h(
        'div',
        { class: 'reveal' },
        h('h2', null, t('You are')),
        flags(id, 'big'),
        h('span', { class: 'callsign', translate: 'no' }, id),
        h('p', null, t('That’s your callsign. Share it with the people you want to talk to: they knock, and you decide whether to open the door.')),
        h('p', { class: 'hint' }, t('Tip: in Settings, set up a Lifeline so a forgotten password doesn’t lose your account.')),
        h(
          'div',
          { class: 'row' },
          h('button', { class: 'btn', type: 'button', onclick: () => navigator.clipboard?.writeText(id).then(() => toast(t('Callsign copied.'))) }, t('Copy callsign')),
          h('button', { class: 'btn primary', type: 'button', onclick: () => start() }, t('Come aboard')),
        ),
      ),
    );
  }
  const halyard = h('div', { class: 'halyard', 'aria-hidden': 'true' }, ...['E', 'S', 'C'].map((c) => flagNode(c)));
  const themeBtn = iconBtn(themeIcon(), t('Change theme'), () => {
    cycleTheme();
    themeBtn.replaceChildren(icon(themeIcon()));
  });
  app.replaceChildren(
    h(
      'main',
      { class: 'welcome' },
      h(
        'div',
        { class: 'welcome-inner' },
        h('div', { class: 'wordmark' }, h('div', { class: 'wm-top' }, halyard, themeBtn), h('h1', null, t('Extremely Simple Communication')), h('p', { translate: 'no' }, info.star ? t('Star {0}', info.star) : '')),
        box,
      ),
    ),
  );
  paintWelcome();
}

// ============================================================================================
// Boot
// ============================================================================================

function mountShell() {
  $('#app').replaceChildren(h('div', { class: 'alerts' }), h('div', { class: 'shell' }, h('aside', { class: 'rail', 'aria-label': t('Lines and crews') }), h('main', { class: 'pane' })));
}

async function start() {
  const res = await fetch('/api/me', { cache: 'no-store' }).catch(() => null);
  if (!res || res.status === 401) return welcome();
  const share = cleanCS(location.hash.slice(1));
  if (CS_RE.test(share)) {
    sessionStorage.setItem('esc-knock', share);
    history.replaceState(null, '', location.pathname);
  }
  $('#app').replaceChildren(h('div', { class: 'empty' }, t('Raising the flags…')));
  if (!WS.started) {
    WS.started = true;
    connect();
  } else hello().catch(() => {});
}

function defs() {
  const tpl = document.createElement('template');
  tpl.innerHTML = `<svg class="sr" aria-hidden="true"><defs><clipPath id="esc-pennant"><path d="${PENNANT}"/></clipPath></defs></svg>`;
  document.body.prepend(tpl.content.firstChild);
}

addEventListener('hashchange', () => {
  if (!S.me) {
    if (/ticket=|lifeline=/.test(location.hash)) welcome();
    return;
  }
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
