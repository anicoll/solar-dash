/* solar-dash, new UI: shared code. Loaded first; every page script after it.

   Everything is on one global, `window.SD`. Page scripts add nothing to the
   global scope except by calling SD.page(). Nobody needs to edit this file to
   build a page.

   Times: the API sends unix seconds (UTC). The house is in Adelaide, so every
   "local" time here is Australia/Adelaide whatever the browser's own zone is.
   "Minute of day" means minutes since local midnight, 0..1439.

   ---------------------------------------------------------------- the API

   SD.tz                                   'Australia/Adelaide'
   SD.local(ts?)                           -> {date:'2026-10-28', minute:910, hour:15, min:10, sec:0, weekday:3}
                                              ts = unix seconds or a Date (default now); weekday 0 = Sunday
   SD.h(tag, attrs?, ...children)          -> HTMLElement. attrs: class, text, html is NOT supported,
                                              dataset:{}, on<event>:fn, anything else is setAttribute
                                              (null/false skip). children: strings, nodes, arrays.
   SD.hatchDefs(svg)                       adds <pattern id="sd-hatch"> to an <svg>, so
                                              <rect fill="url(#sd-hatch)"> draws the sold/projected/partial stripes

   SD.fetchJSON(url, init?)                -> Promise<json>. 401 sends the browser to /login (the promise never
                                              settles). Other failures reject with an Error that has .status
                                              and the server's {error} message when there is one.

   SD.fmt.kwh(n, dp=1)                     12.3  -> '12.3 kWh'
   SD.fmt.kw(n, dp=1)                      4.2   -> '4.2 kW'            (n is in kW; watts / 1000 first)
   SD.fmt.dollars(n, dp=2)                 1234.5 -> '$1,234.50', -1.2 -> '-$1.20'   (n is in dollars)
   SD.fmt.cents(c, {round:false})          38 -> '38c', 58.3 -> '58.3c', 120 -> '$1.20', 100 -> '$1.00'   (c is in cents;
                                              round:true makes 58.3 -> '58c')
   SD.fmt.time(ts)                         -> '3:10 pm'   (ts unix seconds or Date)
   SD.fmt.minute(m)                        -> '6 pm', '7:30 pm', '12 am'   (m minute of day; drops ':00')
   SD.fmt.date(x, style='medium')          x = 'YYYY-MM-DD' | unix seconds | Date.
                                              style 'long' 'Wednesday 28 October', 'medium' 'Wed 28 Oct',
                                              'short' '28 Oct', 'month' 'October 2026'
   Every formatter returns an en dash for null, undefined or NaN. Units are joined with a
   non-breaking space.

   SD.theme.get()                          -> 'auto' | 'day' | 'night'   (the saved choice)
   SD.theme.set(pref)                      saves the choice and applies it
   SD.theme.effective()                    -> 'day' | 'night'   (what is showing now)
   SD.theme.sunTimes(dateStr?)             -> {sunrise: ts, sunset: ts} for that local date (default today)
   It sets <html data-theme>, re-checks every minute, and fires `document` event 'sd:theme' with
   detail {theme} on a change (redraw anything that paints its own colours).

   SD.page(name, {mount(el, hashRest), unmount(), update?(hashRest)})
                                           Register a page. name is one of today bill history cars payback.
                                           mount runs when its hash is opened; el is the empty <section id="page-NAME">.
                                           The router hides the section and clears it after unmount(). hashRest is the
                                           part of the hash after the page name: '#history/day/2026-10-27' gives
                                           'day/2026-10-27' ('' when there is none). If only hashRest changes while
                                           the page is open, update(hashRest) is called when the page has one,
                                           otherwise unmount() then mount() run again.
   SD.go(hash)                             navigate: SD.go('history/day/2026-10-27')
   SD.route()                              -> {name, rest} for the current hash

   SD.live.subscribe(fn)                   -> unsubscribe function. fn(data, err): data is the /api/live JSON,
                                              {sample:{ts,pv_w,load_w,grid_w,battery_w,soc_pct}|null, car:{...}|null},
                                              err is an Error when the poll failed (data is then the last good one or
                                              null). Called at once with the last data if there is some, then every
                                              5 s while the tab is visible.
   SD.live.latest()                        -> last data or null

   SD.tariff.get(dateStr?)                 -> Promise<tariff>: the /api/tariff JSON, cached for the session
                                              (per date). {configured:false} when no plan is set.
   SD.tariff.bandAt(t, minute)             -> 'free' | 'shoulder' | 'peak' | null (null when not configured)
   SD.tariff.centsAt(t, minute)            -> import cents per kWh at that minute
   SD.tariff.creditWindow(t)               -> {start, end, maxKwh, creditCents} (minutes; end < start if it
                                              wraps midnight) or null
   SD.tariff.segments(t)                   -> [{start, end, band, cents}] covering 0..1440 in order

   SD.ribbon(el, tariff, {nowMinute, showCredit:true, labels:'short'|'long'})
                                           Draws the 24 h price strip into el (replacing its contents), with the
                                           $ bracket under the credit window, the now marker and an hour axis.
                                           -> {setNow(minute|null), destroy()}.
*/
(function () {
  'use strict';

  const SD = (window.SD = {});
  const TZ = 'Australia/Adelaide';
  SD.tz = TZ;
  const DASH = '–';
  const NBSP = ' ';

  /* ---------------------------------------------------------------- local time */

  const partsFmt = new Intl.DateTimeFormat('en-AU', {
    timeZone: TZ, hourCycle: 'h23', year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit', weekday: 'short',
  });
  const WEEKDAYS = { Sun: 0, Mon: 1, Tue: 2, Wed: 3, Thu: 4, Fri: 5, Sat: 6 };

  function toDate(x) {
    if (x instanceof Date) return x;
    if (x == null) return new Date();
    return new Date(x * 1000);
  }

  SD.local = function (ts) {
    const p = {};
    for (const part of partsFmt.formatToParts(toDate(ts))) p[part.type] = part.value;
    const hour = Number(p.hour) % 24;
    const min = Number(p.minute);
    return {
      date: `${p.year}-${p.month}-${p.day}`,
      minute: hour * 60 + min, hour, min, sec: Number(p.second),
      weekday: WEEKDAYS[p.weekday],
    };
  };

  /* ---------------------------------------------------------------- DOM helper */

  SD.h = function (tag, attrs, ...children) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v == null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'text') el.textContent = v;
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v === true ? '' : v);
    }
    const add = (c) => {
      if (Array.isArray(c)) c.forEach(add);
      else if (c != null && c !== false) el.append(c.nodeType ? c : String(c));
    };
    children.forEach(add);
    return el;
  };

  const SVGNS = 'http://www.w3.org/2000/svg';
  function svgEl(tag, attrs) {
    const el = document.createElementNS(SVGNS, tag);
    for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
    return el;
  }

  SD.hatchDefs = function (svg) {
    if (svg.querySelector('#sd-hatch')) return;
    const defs = svgEl('defs');
    const pattern = svgEl('pattern', {
      id: 'sd-hatch', width: 6, height: 6, patternUnits: 'userSpaceOnUse', patternTransform: 'rotate(45)',
    });
    pattern.append(svgEl('line', { x1: 0, y1: 0, x2: 0, y2: 6, 'stroke-width': 2, style: 'stroke:var(--hatch-ink)' }));
    defs.append(pattern);
    svg.prepend(defs);
  };

  /* ---------------------------------------------------------------- fetch */

  SD.fetchJSON = async function (url, init) {
    const res = await fetch(url, Object.assign({ credentials: 'same-origin', headers: { Accept: 'application/json' } }, init));
    if (res.status === 401) {
      location.assign('/login');
      return new Promise(() => {}); // the page is going away
    }
    if (!res.ok) {
      let message = `${res.status} ${res.statusText}`;
      try {
        const body = await res.json();
        if (body && body.error) message = body.error;
      } catch (_) { /* not JSON */ }
      const err = new Error(message);
      err.status = res.status;
      throw err;
    }
    return res.json();
  };

  /* ---------------------------------------------------------------- formatters */

  const num = (n) => n != null && Number.isFinite(Number(n));
  const money = new Intl.NumberFormat('en-AU', { style: 'currency', currency: 'AUD' });

  function fixed(n, dp) {
    return Number(n).toLocaleString('en-AU', { minimumFractionDigits: dp, maximumFractionDigits: dp });
  }

  function clock(minute, dropZero) {
    const h = Math.floor(minute / 60) % 24;
    const m = Math.round(minute) % 60;
    const h12 = h % 12 === 0 ? 12 : h % 12;
    const ap = h < 12 ? 'am' : 'pm';
    return dropZero && m === 0 ? `${h12} ${ap}` : `${h12}:${String(m).padStart(2, '0')} ${ap}`;
  }

  SD.fmt = {
    kwh: (n, dp = 1) => (num(n) ? `${fixed(n, dp)}${NBSP}kWh` : DASH),
    kw: (n, dp = 1) => (num(n) ? `${fixed(n, dp)}${NBSP}kW` : DASH),
    dollars: (n, dp = 2) => {
      if (!num(n)) return DASH;
      return new Intl.NumberFormat('en-AU', {
        style: 'currency', currency: 'AUD', minimumFractionDigits: dp, maximumFractionDigits: dp,
      }).format(n);
    },
    cents: (c, opts) => {
      if (!num(c)) return DASH;
      c = Number(c);
      if (Math.abs(c) >= 100) return money.format(c / 100);
      const text = (opts && opts.round) || Number.isInteger(c) ? String(Math.round(c)) : c.toFixed(1).replace(/\.0$/, '');
      return `${text}c`;
    },
    time: (ts) => (num(ts) || ts instanceof Date ? clock(SD.local(ts).minute, false) : DASH),
    minute: (m) => (num(m) ? clock(m, true) : DASH),
    date: (x, style = 'medium') => {
      if (x == null) return DASH;
      // A plain date is read as a calendar day, with no time zone shifting it.
      const d = typeof x === 'string' ? new Date(`${x}T12:00:00Z`) : toDate(x);
      const tz = typeof x === 'string' ? 'UTC' : TZ;
      const f = (o) => new Intl.DateTimeFormat('en-AU', Object.assign({ timeZone: tz }, o)).format(d);
      if (style === 'long') return f({ weekday: 'long', day: 'numeric', month: 'long' });
      if (style === 'short') return f({ day: 'numeric', month: 'short' });
      if (style === 'month') return f({ month: 'long', year: 'numeric' });
      return f({ weekday: 'short', day: 'numeric', month: 'short' }).replace(',', '');
    },
  };

  /* ---------------------------------------------------------------- theme */

  // Sunrise and sunset by the NOAA solar calculator formulas, for the house.
  const LAT = -34.86;
  const LON = 138.5;
  const rad = (d) => (d * Math.PI) / 180;
  const deg = (r) => (r * 180) / Math.PI;

  /** Unix seconds of sunrise and sunset on the local calendar date 'YYYY-MM-DD'. */
  function sunTimes(dateStr) {
    const [y, mo, d] = dateStr.split('-').map(Number);
    // The Julian day at noon UTC on that date is close enough to local noon (about 02:30 UTC).
    const jd = Date.UTC(y, mo - 1, d, 12) / 86400000 + 2440587.5;
    const t = (jd - 2451545) / 36525;
    const L0 = (280.46646 + t * (36000.76983 + t * 0.0003032)) % 360;
    const M = 357.52911 + t * (35999.05029 - 0.0001537 * t);
    const e = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    const C =
      Math.sin(rad(M)) * (1.914602 - t * (0.004817 + 0.000014 * t)) +
      Math.sin(rad(2 * M)) * (0.019993 - 0.000101 * t) +
      Math.sin(rad(3 * M)) * 0.000289;
    const trueLong = L0 + C;
    const omega = 125.04 - 1934.136 * t;
    const appLong = trueLong - 0.00569 - 0.00478 * Math.sin(rad(omega));
    const obliqMean = 23 + (26 + (21.448 - t * (46.815 + t * (0.00059 - t * 0.001813))) / 60) / 60;
    const obliq = obliqMean + 0.00256 * Math.cos(rad(omega));
    const decl = Math.asin(Math.sin(rad(obliq)) * Math.sin(rad(appLong)));
    const yy = Math.tan(rad(obliq / 2)) ** 2;
    const eqTime =
      4 * deg(
        yy * Math.sin(2 * rad(L0)) -
          2 * e * Math.sin(rad(M)) +
          4 * e * yy * Math.sin(rad(M)) * Math.cos(2 * rad(L0)) -
          0.5 * yy * yy * Math.sin(4 * rad(L0)) -
          1.25 * e * e * Math.sin(2 * rad(M)),
      );
    const cosHa = Math.cos(rad(90.833)) / (Math.cos(rad(LAT)) * Math.cos(decl)) - Math.tan(rad(LAT)) * Math.tan(decl);
    const ha = deg(Math.acos(Math.max(-1, Math.min(1, cosHa))));
    const noonMin = 720 - 4 * LON - eqTime; // minutes after 00:00 UTC of that date
    const midnight = Date.UTC(y, mo - 1, d) / 1000;
    return {
      sunrise: Math.round(midnight + (noonMin - 4 * ha) * 60),
      sunset: Math.round(midnight + (noonMin + 4 * ha) * 60),
    };
  }

  const THEME_KEY = 'sd-theme';
  const PREFS = ['auto', 'day', 'night'];
  let themePref = 'auto';
  let themeNow = null;
  try {
    const saved = localStorage.getItem(THEME_KEY);
    if (PREFS.includes(saved)) themePref = saved;
  } catch (_) { /* storage blocked: stay on auto */ }

  function effectiveTheme() {
    if (themePref !== 'auto') return themePref;
    const now = Math.floor(Date.now() / 1000);
    const { sunrise, sunset } = sunTimes(SD.local(now).date);
    return now >= sunrise && now < sunset ? 'day' : 'night';
  }

  function applyTheme() {
    const theme = effectiveTheme();
    const button = document.getElementById('sd-theme');
    if (button) {
      const label = `Theme: ${themePref}${themePref === 'auto' ? ` (${theme} now)` : ''}. Change`;
      button.setAttribute('aria-label', label);
      button.title = label;
    }
    if (theme === themeNow) return;
    themeNow = theme;
    document.documentElement.dataset.theme = theme;
    document.dispatchEvent(new CustomEvent('sd:theme', { detail: { theme } }));
  }

  SD.theme = {
    get: () => themePref,
    set(pref) {
      if (!PREFS.includes(pref)) return;
      themePref = pref;
      try { localStorage.setItem(THEME_KEY, pref); } catch (_) { /* not saved */ }
      applyTheme();
    },
    effective: () => themeNow || effectiveTheme(),
    sunTimes: (dateStr) => sunTimes(dateStr || SD.local().date),
  };
  applyTheme();
  setInterval(applyTheme, 60000);

  /* ---------------------------------------------------------------- tariff */

  const tariffCache = new Map();
  const toMin = (hhmm) => {
    const [h, m] = String(hhmm).split(':').map(Number);
    return h * 60 + m;
  };
  // A window holds a minute from `from` up to but not including `to`; it wraps midnight if to <= from.
  const inWindow = (from, to, m) => (from < to ? from <= m && m < to : m >= from || m < to);

  function windowAt(t, minute) {
    return (t.import || []).find((w) => inWindow(toMin(w.from), toMin(w.to), minute));
  }

  SD.tariff = {
    get(dateStr) {
      const key = dateStr || 'today:' + SD.local().date;
      if (!tariffCache.has(key)) {
        const p = SD.fetchJSON('/api/tariff' + (dateStr ? `?date=${encodeURIComponent(dateStr)}` : ''));
        p.catch(() => tariffCache.delete(key)); // do not cache a failure
        tariffCache.set(key, p);
      }
      return tariffCache.get(key);
    },
    bandAt(t, minute) {
      if (!t || !t.configured) return null;
      const w = windowAt(t, minute);
      return w ? w.band : t.default_import_band;
    },
    centsAt(t, minute) {
      if (!t || !t.configured) return null;
      const w = windowAt(t, minute);
      return w ? w.cents : t.default_import_cents;
    },
    creditWindow(t) {
      const c = t && t.configured && t.zero_import_credit;
      if (!c) return null;
      return { start: toMin(c.from), end: toMin(c.to), maxKwh: c.max_kwh, creditCents: c.credit_cents };
    },
    segments(t) {
      const out = [];
      if (!t || !t.configured) return out;
      for (let m = 0; m < 1440; m++) {
        const band = SD.tariff.bandAt(t, m);
        const cents = SD.tariff.centsAt(t, m);
        const last = out[out.length - 1];
        if (last && last.band === band && last.cents === cents) last.end = m + 1;
        else out.push({ start: m, end: m + 1, band, cents });
      }
      return out;
    },
  };

  /* ---------------------------------------------------------------- day ribbon */

  const BAND_NAME = { free: 'Free', shoulder: 'Shoulder', peak: 'Peak' };

  SD.ribbon = function (el, tariff, opts) {
    opts = Object.assign({ nowMinute: null, showCredit: true, labels: 'short' }, opts);
    const long = opts.labels === 'long';
    const pct = (m) => `${(m / 1440) * 100}%`;
    el.replaceChildren();

    if (!tariff || !tariff.configured) {
      el.append(SD.h('p', { class: 'ribbon-note', text: 'No plan is set, so there are no price bands to show. Add one in Settings and admin.' }));
      return { setNow() {}, destroy() { el.replaceChildren(); } };
    }

    const segs = SD.tariff.segments(tariff);
    const credit = opts.showCredit ? SD.tariff.creditWindow(tariff) : null;
    const root = SD.h('div', { class: `ribbon${long ? ' is-long' : ''}${credit ? ' has-credit' : ''}`, role: 'img' });

    // The strip, one block per run of the same price.
    const strip = SD.h('div', { class: 'ribbon-strip' });
    const words = [];
    for (const s of segs) {
      const cents = s.band === 'free' ? '' : SD.fmt.cents(s.cents, { round: !long });
      const label = s.band === 'free' ? 'Free' : long ? `${BAND_NAME[s.band]} ${cents}` : cents;
      strip.append(SD.h('div', { class: `ribbon-seg band-${s.band}`, style: `flex: ${s.end - s.start} 1 0`, text: s.end - s.start >= (long ? 150 : 90) ? label : '' })); // too narrow for words
      words.push(`${BAND_NAME[s.band]}${s.band === 'free' ? '' : ' ' + SD.fmt.cents(s.cents)} ${SD.fmt.minute(s.start)} to ${SD.fmt.minute(s.end % 1440)}`);
    }
    root.append(strip);

    // The credit bracket sits under the strip. A window that wraps midnight is drawn in two pieces.
    if (credit) {
      const pieces = credit.start < credit.end ? [[credit.start, credit.end]] : [[credit.start, 1440], [0, credit.end]];
      for (const [a, b] of pieces) {
        root.append(SD.h('div', { class: 'ribbon-credit', style: `left: ${pct(a)}; width: calc(${pct(b)} - ${pct(a)})` }));
      }
      const amount = credit.creditCents % 100 === 0 ? `$${credit.creditCents / 100}` : SD.fmt.cents(credit.creditCents);
      // Keep the label inside the strip when the window ends near midnight.
      const labelStyle = credit.start > 1440 * 0.7 && long ? `right: ${pct(1440 - Math.min(credit.end || 1440, 1440))}` : `left: ${pct(credit.start)}`;
      root.append(SD.h('div', { class: 'ribbon-credit-label', style: labelStyle, text: long ? `${amount} if zero import` : `${amount} window` }));
      words.push(`${amount} credit for no import from ${SD.fmt.minute(credit.start)} to ${SD.fmt.minute(credit.end)}`);
    }

    // The now marker.
    const now = SD.h('div', { class: 'ribbon-now', 'aria-hidden': 'true' });
    root.append(now);

    // The hour axis: every 3 h on the long form, every 6 h on the short one.
    const step = long ? 3 : 6;
    const axis = SD.h('div', { class: `ribbon-axis ${long ? 'is-long' : 'is-short'}`, 'aria-hidden': 'true' });
    for (let h = 0; h <= 24; h += step) {
      if (long && h === 24) break;
      const tick = SD.h('span', { text: SD.fmt.minute((h % 24) * 60) });
      if (long || h === 0) tick.style.left = pct(h * 60); // starts at its hour
      else if (h === 24) tick.style.right = '0'; // the last label ends at the edge
      else { tick.style.left = pct(h * 60); tick.style.transform = 'translateX(-50%)'; } // centred on its hour
      axis.append(tick);
    }
    root.append(axis);

    let nowMinute = null;
    function setNow(m) {
      nowMinute = m == null ? null : m;
      now.hidden = nowMinute == null;
      if (nowMinute != null) now.style.left = pct(nowMinute);
      root.setAttribute('aria-label', `Price bands over the day. ${words.join('. ')}.${nowMinute != null ? ` Now ${SD.fmt.minute(nowMinute)}.` : ''}`);
    }
    setNow(opts.nowMinute);
    el.append(root);
    return { setNow, destroy: () => el.replaceChildren() };
  };

  /* ---------------------------------------------------------------- live poller */

  const POLL_MS = 5000;
  const live = (() => {
    const subs = new Set();
    let last = null;
    let timer = null;
    let busy = false;

    async function poll() {
      if (busy) return;
      busy = true;
      let err = null;
      try {
        last = await SD.fetchJSON('/api/live');
      } catch (e) {
        err = e;
      }
      busy = false;
      for (const fn of [...subs]) {
        try { fn(last, err); } catch (e) { console.error(e); }
      }
      schedule();
    }

    function schedule() {
      clearTimeout(timer);
      timer = null;
      if (subs.size && !document.hidden) timer = setTimeout(poll, POLL_MS);
    }

    document.addEventListener('visibilitychange', () => {
      if (document.hidden) { clearTimeout(timer); timer = null; }
      else if (subs.size) poll();
    });

    return {
      subscribe(fn) {
        subs.add(fn);
        if (last) fn(last, null);
        if (subs.size === 1 || !timer) { if (!document.hidden && !busy) poll(); }
        return () => { subs.delete(fn); if (!subs.size) schedule(); };
      },
      latest: () => last,
    };
  })();
  SD.live = live;

  /* The header: "Live · N s" counts the age of the newest reading, once a second. */
  function startHeaderLive() {
    const box = document.getElementById('sd-live');
    const text = document.getElementById('sd-live-text');
    if (!box || !text) return;
    let data = null;
    let failed = false;
    function paint() {
      const sample = data && data.sample;
      const age = sample ? Math.max(0, Math.floor(Date.now() / 1000) - sample.ts) : null;
      let state = 'live';
      let words;
      if (age == null) { state = 'off'; words = failed ? 'Offline' : 'No live reading'; }
      else if (age < 60) words = `Live · ${age} s`;
      else { state = 'stale'; words = age < 3600 ? `Last reading ${Math.floor(age / 60)} min ago` : `Last reading ${Math.floor(age / 3600)} h ago`; }
      box.dataset.state = state;
      if (text.textContent !== words) text.textContent = words;
    }
    live.subscribe((d, err) => { data = d; failed = !!err; paint(); });
    setInterval(() => { if (!document.hidden) paint(); }, 1000);
    paint();
  }

  /* ---------------------------------------------------------------- router */

  const NAMES = ['today', 'bill', 'history', 'cars', 'payback'];
  const TITLES = { today: 'Today', bill: 'Bill', history: 'History', cars: 'Cars', payback: 'Payback' };
  const pages = {};
  let current = null; // {name, rest}

  SD.route = function () {
    const raw = decodeURIComponent(location.hash.replace(/^#/, ''));
    const slash = raw.indexOf('/');
    const name = slash < 0 ? raw : raw.slice(0, slash);
    return { name: NAMES.includes(name) ? name : 'today', rest: slash < 0 ? '' : raw.slice(slash + 1) };
  };

  SD.go = function (hash) {
    location.hash = '#' + String(hash).replace(/^#/, '');
  };

  SD.page = function (name, impl) {
    if (!NAMES.includes(name)) throw new Error(`SD.page: unknown page "${name}"`);
    pages[name] = impl;
  };

  function section(name) {
    return document.getElementById(`page-${name}`);
  }

  function render() {
    const next = SD.route();
    if (current && current.name === next.name) {
      if (current.rest === next.rest) return;
      const page = pages[next.name];
      current = next;
      if (page && page.update) { page.update(next.rest); return; }
      remount(next);
      return;
    }
    if (current) unmountCurrent();
    current = next;
    mount(next);
    window.scrollTo(0, 0);
  }

  function unmountCurrent() {
    const el = section(current.name);
    const page = pages[current.name];
    try { if (page && page.unmount) page.unmount(); } catch (e) { console.error(e); }
    el.replaceChildren();
    el.hidden = true;
  }

  function mount(route) {
    const el = section(route.name);
    el.hidden = false;
    document.title = `${TITLES[route.name]} · solar-dash`;
    // The History page belongs under Today and Bill in the phone bar, so it highlights nothing there.
    for (const a of document.querySelectorAll('[data-nav]')) {
      if (a.dataset.nav === route.name) a.setAttribute('aria-current', 'page');
      else a.removeAttribute('aria-current');
    }
    const page = pages[route.name];
    if (!page) { el.append(SD.h('p', { class: 'muted', text: 'This page is not available yet.' })); return; }
    try { page.mount(el, route.rest); } catch (e) {
      console.error(e);
      el.replaceChildren(SD.h('div', { class: 'card', role: 'alert' }, SD.h('p', { text: 'This page failed to load. ' + e.message })));
    }
  }

  function remount(route) {
    const el = section(route.name);
    const page = pages[route.name];
    try { if (page && page.unmount) page.unmount(); } catch (e) { console.error(e); }
    el.replaceChildren();
    mount(route);
  }

  /* ---------------------------------------------------------------- start */

  document.addEventListener('DOMContentLoaded', () => {
    // Pages have registered by now: deferred scripts all run before this event.
    const button = document.getElementById('sd-theme');
    if (button) {
      button.addEventListener('click', () => {
        SD.theme.set(PREFS[(PREFS.indexOf(SD.theme.get()) + 1) % PREFS.length]);
      });
    }
    applyTheme();
    window.addEventListener('hashchange', render);
    render();
    startHeaderLive();
  });
})();
