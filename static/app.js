// solar-dash dashboard: plain JS, ECharts for the charts. No build step.
(function () {
  'use strict';

  const TZ = 'Australia/Adelaide';
  const REFRESH_MS = 60 * 1000;      // Now, from iSolarCloud
  const LIVE_REFRESH_MS = 15 * 1000; // Now, from the inverter
  // The Day chart uses inverter data when it spans at least this many minutes.
  const LIVE_MIN_MINUTES = 30;
  // A longer gap between inverter minutes breaks the line.
  const LIVE_GAP_SECS = 3 * 60;

  // Points shown on the Now tab (see config.example.toml).
  const P_PV = 83033, P_LOAD = 83106, P_SOC = 83252;

  // ---------- small helpers ----------

  const $ = (id) => document.getElementById(id);
  const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const kw = (w) => (w / 1000).toFixed(2);
  const kwh = (v) => (v == null ? '–' : v.toFixed(1));
  const pad = (n) => String(n).padStart(2, '0');
  /** Escapes text for the few places ECharts tooltips take an HTML string. */
  const esc = (t) => String(t).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
  /** Dollars: "$6,912" for `dp` 0, "$186.40" for 2. Negative amounts get a leading minus. */
  function money(v, dp = 2) {
    if (v == null || !Number.isFinite(v)) return '–';
    const t = Math.abs(v).toLocaleString('en-AU', { minimumFractionDigits: dp, maximumFractionDigits: dp });
    return `${v < 0 ? '-' : ''}$${t}`;
  }
  /** Creates an element with optional class and text. */
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };

  const fmtDate = new Intl.DateTimeFormat('en-CA', {
    timeZone: TZ, year: 'numeric', month: '2-digit', day: '2-digit',
  });
  const fmtClock = new Intl.DateTimeFormat('en-GB', {
    timeZone: TZ, hour: '2-digit', minute: '2-digit', hourCycle: 'h23',
  });
  const fmtParts = new Intl.DateTimeFormat('en-US', {
    timeZone: TZ, hourCycle: 'h23', year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit',
  });
  const fmtLong = new Intl.DateTimeFormat('en-AU', {
    timeZone: TZ, weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit',
    minute: '2-digit', hourCycle: 'h23',
  });
  const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

  /** Today's date in Adelaide as YYYY-MM-DD. */
  function todayStr() { return fmtDate.format(new Date()); }

  /** Adds days to a YYYY-MM-DD string (pure calendar maths, no zones involved). */
  function addDays(dateStr, n) {
    const [y, m, d] = dateStr.split('-').map(Number);
    const t = new Date(Date.UTC(y, m - 1, d + n));
    return `${t.getUTCFullYear()}-${pad(t.getUTCMonth() + 1)}-${pad(t.getUTCDate())}`;
  }

  /** Unix ms of a local Adelaide wall-clock time on `dateStr` (hour may be 24). */
  function adelaideMs(dateStr, hour) {
    const [y, m, d] = dateStr.split('-').map(Number);
    const wall = Date.UTC(y, m - 1, d, hour, 0, 0);
    // Find the zone offset at that instant, then correct once more for DST edges.
    let guess = wall;
    for (let i = 0; i < 2; i++) {
      const p = {};
      for (const part of fmtParts.formatToParts(new Date(guess))) p[part.type] = part.value;
      const asUtc = Date.UTC(+p.year, +p.month - 1, +p.day, +p.hour, +p.minute, +p.second);
      guess -= asUtc - wall;
    }
    return guess;
  }

  function clock(ms) { return fmtClock.format(new Date(ms)); }

  function showBanner(msg) {
    const b = $('banner');
    b.textContent = msg;
    b.hidden = !msg;
  }
  let apiError = '';
  let chartsError = '';
  function updateBanner() { showBanner([chartsError, apiError].filter(Boolean).join(' ')); }

  /** GET JSON. A 401 sends the browser to the login page. */
  async function api(url) {
    let res;
    try {
      res = await fetch(url, { credentials: 'same-origin', headers: { Accept: 'application/json' } });
    } catch (err) {
      throw new Error('Network error while loading data.');
    }
    if (res.status === 401 || res.redirected && new URL(res.url).pathname === '/login') {
      window.location.href = '/login';
      throw new Error('Not logged in.');
    }
    if (!res.ok) {
      let detail = '';
      try { detail = (await res.json()).error || ''; } catch (e) { /* ignore */ }
      throw new Error(`Request failed (${res.status}${detail ? ': ' + detail : ''}).`);
    }
    return res.json();
  }

  /** Runs a loader and shows its failure in the banner. */
  async function guarded(fn) {
    try {
      await fn();
      if (apiError) { apiError = ''; updateBanner(); }
    } catch (err) {
      if (err.message === 'Not logged in.') return;
      apiError = err.message || 'Something went wrong.';
      updateBanner();
    }
  }

  function tile(label, valueText, unit, colour) {
    const t = el('div', 'tile');
    if (colour) t.style.setProperty('--tile', `var(${colour})`);
    const v = el('div', 'value', valueText);
    if (unit) v.appendChild(el('span', 'unit', unit));
    t.append(el('div', 'label', label), v);
    return t;
  }

  const TOTALS = [
    ['PV generation', 'pv_kwh', '--c-pv'],
    ['Load', 'load_kwh', '--c-load'],
    ['Grid import', 'import_kwh', '--c-import'],
    ['Grid export', 'export_kwh', '--c-export'],
    ['Battery charge', 'battery_charge_kwh', '--c-charge'],
    ['Battery discharge', 'battery_discharge_kwh', '--c-discharge'],
  ];

  function renderTotals(container, totals) {
    container.replaceChildren(...TOTALS.map(([label, key, colour]) =>
      tile(label, kwh(totals[key]), 'kWh', colour)));
  }

  // ---------- charts ----------

  const charts = {};
  const chartsAvailable = () => typeof window.echarts !== 'undefined';

  function getChart(id) {
    if (!chartsAvailable()) return null;
    if (!charts[id]) charts[id] = window.echarts.init($(id), null, { renderer: 'canvas' });
    return charts[id];
  }

  function baseTheme() {
    return { fg: css('--fg'), muted: css('--muted'), line: css('--border') };
  }

  function tooltipStyle() {
    return {
      backgroundColor: css('--panel-2'), borderColor: css('--border-strong'),
      textStyle: { color: css('--fg'), fontSize: 13 },
    };
  }

  function showChart(id, hasData, option, label) {
    const empty = $(id.replace('-chart', '-empty'));
    empty.hidden = hasData;
    $(id).setAttribute('aria-label', label);
    const c = getChart(id);
    if (!c) return;
    if (hasData) { c.setOption(option, true); c.resize(); } else { c.clear(); }
  }

  // ---------- savings data (shared, fetched at most once a minute) ----------

  const COSTS_TTL_MS = 60 * 1000;
  // /api/costs allows at most 1500 days per request.
  const COSTS_MAX_DAYS = 1500;
  const savingsCache = { at: 0, promise: null };

  /**
   * All-history costs. Resolves to `{ configured: false }`, or `{ configured: true,
   * saved, days, totals }` where `saved` is /api/payback's total when that is
   * configured (it counts from the first purchase) and /api/costs' otherwise.
   * Cached for COSTS_TTL_MS so a 15 s poll does not hit the server every time.
   */
  const paybackCache = { at: 0, promise: null };

  /** /api/payback, cached like loadSavings. */
  function loadPayback() {
    if (paybackCache.promise && Date.now() - paybackCache.at < COSTS_TTL_MS) return paybackCache.promise;
    paybackCache.at = Date.now();
    const p = api('/api/payback');
    paybackCache.promise = p;
    p.catch(() => { paybackCache.at = 0; });
    return p;
  }

  function loadSavings() {
    if (savingsCache.promise && Date.now() - savingsCache.at < COSTS_TTL_MS) return savingsCache.promise;
    savingsCache.at = Date.now();
    const p = (async () => {
      const today = todayStr();
      const payback = await loadPayback().catch((err) => {
        if (err.message === 'Not logged in.') throw err;
        return null;
      });
      const earliest = addDays(today, 1 - COSTS_MAX_DAYS);
      let from = earliest;
      if (payback && payback.configured && payback.purchases.length) {
        from = payback.purchases[0].date > earliest ? payback.purchases[0].date : earliest;
      }
      const costs = await api(`/api/costs?from=${from}&to=${today}`);
      if (!costs.configured) return { configured: false };
      const saved = payback && payback.configured ? payback.saved : costs.totals.savings;
      const purchases = payback && payback.configured ? payback.purchases : [];
      return { configured: true, saved, days: costs.days, totals: costs.totals, purchases };
    })();
    savingsCache.promise = p;
    p.catch(() => { savingsCache.at = 0; });
    return p;
  }

  /** Running total of `savings`, as `[date, total]` pairs. */
  function runningTotal(days) {
    let total = 0;
    return days.map((d) => [d.date, (total += d.savings)]);
  }

  const SVG_NS = 'http://www.w3.org/2000/svg';

  /** A small filled line of `values` as an SVG, built without HTML strings. */
  function sparkline(values, label) {
    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('viewBox', '0 0 300 70');
    svg.setAttribute('preserveAspectRatio', 'none');
    svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', label);
    if (values.length < 2) return svg;
    // At most ~120 points is plenty at this size.
    const stride = Math.max(1, Math.floor(values.length / 120));
    const pts = values.filter((_, i) => i % stride === 0 || i === values.length - 1);
    const lo = Math.min(0, ...pts), hi = Math.max(...pts, lo + 1);
    const xy = pts.map((v, i) => [(i / (pts.length - 1)) * 300, 66 - ((v - lo) / (hi - lo)) * 62]);
    const line = xy.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`).join('');
    const area = document.createElementNS(SVG_NS, 'path');
    area.setAttribute('d', `${line}L300 70L0 70Z`);
    area.setAttribute('fill', css('--save'));
    area.setAttribute('fill-opacity', '0.12');
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', line);
    path.setAttribute('fill', 'none');
    path.setAttribute('stroke', css('--save'));
    path.setAttribute('stroke-width', '2.5');
    path.setAttribute('stroke-linejoin', 'round');
    path.setAttribute('vector-effect', 'non-scaling-stroke');
    svg.append(area, path);
    return svg;
  }

  /** "Feb 2023" for a YYYY-MM-DD string. */
  const monthYear = (d) => `${MONTHS[+d.slice(5, 7) - 1]} ${d.slice(0, 4)}`;

  const NOT_CONFIGURED_TARIFF =
    'Add a [tariff] section to the config file to see what solar and the battery save you.';

  function renderHero(data) {
    const empty = $('hero-empty'), body = $('hero-body');
    if (!data || !data.configured) {
      empty.textContent = data ? NOT_CONFIGURED_TARIFF : 'Savings are unavailable right now.';
      empty.hidden = false;
      body.hidden = true;
      return;
    }
    empty.hidden = true;
    body.hidden = false;
    const today = todayStr();
    const month = today.slice(0, 7);
    const sumOf = (rows) => rows.reduce((a, d) => a + d.savings, 0);
    $('hero-total').textContent = money(data.saved, 0);
    $('hero-since').textContent = data.days.length ? `All time, since ${monthYear(data.days[0].date)}` : 'All time';
    $('hero-month').textContent = money(sumOf(data.days.filter((d) => d.date.startsWith(month))));
    const t = data.days.find((d) => d.date === today);
    $('hero-today').textContent = t ? money(t.savings) : money(0);
    const first = data.days.length ? monthYear(data.days[0].date) : 'the start';
    $('hero-spark').replaceChildren(sparkline(runningTotal(data.days).map((r) => r[1]),
      `Running total of savings since ${first}, now ${money(data.saved, 0)}`));
  }

  // ---------- Now ----------

  /** "5s ago", "3 min ago", "2 h ago". */
  function ageText(secs) {
    const s = Math.max(0, Math.round(secs));
    if (s < 60) return `${s}s ago`;
    const mins = Math.round(s / 60);
    return mins < 120 ? `${mins} min ago` : `${Math.round(mins / 60)} h ago`;
  }

  /** The newest inverter sample and car state; either is null if there is none (or the request failed). */
  async function fetchLive() {
    try {
      const live = await api('/api/live');
      return { sample: live.sample || null, car: live.car || null };
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
      return { sample: null, car: null };
    }
  }

  /** A power tile: coloured label, big value, optional extra node underneath. */
  function powerTile(label, valueText, unit, colourVar, extra) {
    const t = el('div', 'ptile');
    t.style.setProperty('--tile', `var(${colourVar})`);
    const v = el('div', 'value', valueText);
    if (unit) v.appendChild(el('span', 'unit', unit));
    t.append(el('div', 'label', label), v);
    if (extra) t.appendChild(extra);
    return t;
  }

  /** Battery charge bar with a percentage. */
  function socBar(pct) {
    const row = el('div', 'soc');
    const bar = el('div', 'bar'), fill = el('span', 'fill');
    fill.style.width = `${Math.max(0, Math.min(100, pct))}%`;
    bar.appendChild(fill);
    row.append(bar, el('span', 'pct', `${Math.round(pct)}%`));
    return row;
  }

  function setStatus(text, stale) {
    $('updated-text').textContent = text;
    $('updated').classList.toggle('stale', !!stale);
  }

  const FLOW_MIN_W = 20;     // below this a flow counts as idle
  const FLOW_FULL_W = 5000;  // flows at or above this animate at full speed
  // Sungrow-style layout: solar on top, home in the middle, battery bottom-left, grid bottom-right.
  // tx/ty/anchor place the three text lines (label, value, note) beside or below each node.
  // r is the disc radius (default 28). The car, left of Home, only shows while it is plugged in.
  const FLOW_NODES = {
    solar: { x: 200, y: 64, colour: '--c-pv', label: 'Solar', tx: 238, ty: 22, anchor: 'start' },
    home: { x: 200, y: 188, colour: '--c-load', label: 'Home', tx: 238, ty: 154, anchor: 'start' },
    battery: { x: 70, y: 280, colour: '--c-battery', label: 'Battery', tx: 70, ty: 324, anchor: 'middle' },
    grid: { x: 330, y: 280, colour: '--c-grid', label: 'Grid', tx: 330, ty: 324, anchor: 'middle' },
    car: { x: 126, y: 188, r: 24, colour: '--c-ev', label: 'Car', tx: 126, ty: 106, anchor: 'middle' },
  };
  // Rounded right-angle routes. Each path runs in its positive direction (from -> to); `rev` is the colour when it runs backwards.
  const FLOW_EDGES = {
    pvHome: { d: 'M200 92V160', colour: '--c-pv' },
    pvBat: { d: 'M172 64H84Q70 64 70 78V252', colour: '--c-pv' },
    pvGrid: { d: 'M228 64H316Q330 64 330 78V252', colour: '--c-pv' },
    batHome: { d: 'M94.6 268H171Q185 268 185 254V212', colour: '--c-battery' },
    gridHome: { d: 'M305.4 268H229Q215 268 215 254V212', colour: '--c-grid' },
    batGrid: { d: 'M94.6 292H305.4', colour: '--c-battery', rev: '--c-grid' },
    homeCar: { d: 'M172 188H150', colour: '--c-ev' },
  };
  const FLOW_ICONS = {
    home: ['M3 11.5L12 3.5l9 8', 'M5.5 10v10.5h13V10', 'M10 20.5v-6h4v6', 'M16.5 7V4h2v5'],
    grid: ['M12 2L7.5 22M12 2l4.5 20', 'M9 5h6', 'M6 8.5h12', 'M8.2 15h7.6', 'M9.4 8.5l5.7 6.5M14.6 8.5L8.9 15', 'M6 8.5v2M18 8.5v2'],
    car: ['M2.5 16.5v-3.5l2-1 2-4.5h8l3.5 4.5 3.5 1v3.5h-2', 'M2.5 16.5h2', 'M8.5 16.5h7', 'M7.5 12h11'],
  };

  /** SVG element with the given attributes and optional class. */
  function svgEl(tag, attrs, cls) {
    const e = document.createElementNS(SVG_NS, tag);
    for (const k in attrs) e.setAttribute(k, attrs[k]);
    if (cls) e.setAttribute('class', cls);
    return e;
  }

  /** SVG <text> at (x, y). */
  function svgText(x, y, text, cls, anchor) {
    const t = svgEl('text', { x, y, 'text-anchor': anchor }, cls);
    t.textContent = text;
    return t;
  }

  /** Sets the --flow colour of an SVG element to a theme colour token. */
  function flowColour(e, colourVar) {
    e.style.setProperty('--flow', `var(${colourVar})`);
    return e;
  }

  /** Stroked icon group from a list of path strings. */
  function flowPaths(list, cls) {
    const g = svgEl('g', {}, cls);
    for (const d of list) g.appendChild(svgEl('path', { d }));
    return g;
  }

  /** Line icon for a node on a 24x24 grid; returns the group plus the parts that animate. */
  function flowIcon(kind) {
    const g = svgEl('g', {}, 'flow-icon');
    const parts = {};
    if (kind === 'solar') {
      const rays = svgEl('g', {}, 'flow-rays');
      for (let i = 0; i < 8; i++) {
        const a = i * Math.PI / 4, c = Math.cos(a), s = Math.sin(a);
        rays.appendChild(svgEl('path', { d: `M${(12 + 7.6 * c).toFixed(2)} ${(12 + 7.6 * s).toFixed(2)}L${(12 + 10 * c).toFixed(2)} ${(12 + 10 * s).toFixed(2)}` }));
      }
      parts.rays = rays;
      g.append(svgEl('circle', { cx: 12, cy: 12, r: 4.2 }), rays);
    } else if (kind === 'battery') {
      parts.fill = svgEl('rect', { x: 4.6, y: 9, width: 0, height: 6, rx: 0.8 }, 'flow-soc');
      g.append(svgEl('rect', { x: 2.5, y: 6.5, width: 17, height: 11, rx: 2.4 }),
        svgEl('path', { d: 'M21.5 10.5v3' }), parts.fill);
    } else if (kind === 'car') {
      g.append(flowPaths(FLOW_ICONS.car),
        svgEl('circle', { cx: 6.5, cy: 16.5, r: 2 }), svgEl('circle', { cx: 17.5, cy: 16.5, r: 2 }));
    } else {
      g.appendChild(flowPaths(FLOW_ICONS[kind]));
    }
    return { g, parts };
  }

  /** Shared gradients and glow filters; ids are prefixed flow- (there is one diagram per page). */
  function flowDefs() {
    const defs = svgEl('defs', {});
    for (const [id, dev] of [['flow-blur-lg', 7], ['flow-blur-sm', 2.2]]) {
      const f = svgEl('filter', { id, filterUnits: 'userSpaceOnUse', x: -40, y: -40, width: 480, height: 460 });
      f.appendChild(svgEl('feGaussianBlur', { stdDeviation: dev }));
      defs.appendChild(f);
    }
    for (const kind in FLOW_NODES) {
      const g = svgEl('radialGradient', { id: `flow-g-${kind}`, cx: 0.5, cy: 0.38, r: 0.75 });
      for (const [off, op] of [[0, 0.5], [1, 0.07]]) {
        const stop = svgEl('stop', { offset: off, 'stop-opacity': op });
        stop.style.setProperty('stop-color', `var(${FLOW_NODES[kind].colour})`);
        g.appendChild(stop);
      }
      defs.appendChild(g);
    }
    return defs;
  }

  /** Card-like round node: glow, tinted disc, icon and its three text lines. */
  function flowNode(kind) {
    const n = FLOW_NODES[kind];
    const g = flowColour(svgEl('g', {}, 'flow-node'), n.colour);
    const icon = flowIcon(kind);
    const r = n.r || 28, scale = 1.5 * r / 28;
    icon.g.setAttribute('transform', `translate(${n.x - 12 * scale} ${n.y - 12 * scale}) scale(${scale})`);
    const label = svgText(n.tx, n.ty, n.label, 'flow-label', n.anchor);
    const value = svgText(n.tx, n.ty + 18, '', 'flow-value', n.anchor);
    const note = svgText(n.tx, n.ty + 34, '', 'flow-note', n.anchor);
    g.append(
      svgEl('circle', { cx: n.x, cy: n.y, r: r + 3, filter: 'url(#flow-blur-lg)' }, 'flow-halo'),
      svgEl('circle', { cx: n.x, cy: n.y, r }, 'flow-disc'),
      svgEl('circle', { cx: n.x, cy: n.y, r, fill: `url(#flow-g-${kind})` }, 'flow-tint'),
      icon.g, label, value, note,
    );
    return { g, label, value, note, ...icon.parts };
  }

  /** Dots per flow and seconds per traversal for a flow of `w` watts. */
  function flowPace(w) {
    const a = Math.min(1, Math.abs(w) / FLOW_FULL_W);
    return { n: a >= 0.7 ? 4 : a >= 0.3 ? 3 : 2, secs: 3.5 - 2.5 * a };
  }

  /** Sets dur on every animation in a flow. */
  function flowSetDur(motions, secs) {
    for (const m of motions) m.setAttribute('dur', `${secs.toFixed(2)}s`);
  }

  /** Glowing line plus staggered travelling dots for a flow; reuses the current ones when only the speed changed. */
  function flowUpdate(f, w) {
    const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    const on = w != null && Math.abs(w) >= FLOW_MIN_W;
    const pace = on ? flowPace(w) : { n: 0, secs: 0 };
    const key = on ? `${w < 0 ? '-' : '+'}${reduce ? 0 : pace.n}` : 'off';
    if (key === f.key) {
      if (f.motions.length && Math.abs(pace.secs - f.secs) > 0.15 * f.secs) { f.secs = pace.secs; flowSetDur(f.motions, pace.secs); }
      return on;
    }
    f.key = key; f.secs = pace.secs; f.motions = [];
    f.holder.replaceChildren();
    if (on) flowColour(f.holder, w < 0 && f.rev ? f.rev : f.colour);
    if (!on) return false;
    f.holder.append(
      svgEl('path', { d: f.d, filter: 'url(#flow-blur-sm)' }, 'flow-line-glow'),
      svgEl('path', { d: f.d }, 'flow-line'),
    );
    const dur = `${pace.secs.toFixed(2)}s`;
    for (let i = 0; !reduce && i < pace.n; i++) {
      const begin = `${(-pace.secs * i / pace.n).toFixed(2)}s`;
      const dot = svgEl('g', { opacity: 0 }, 'flow-dot');
      dot.append(svgEl('circle', { r: 6, filter: 'url(#flow-blur-sm)' }, 'flow-dot-halo'), svgEl('circle', { r: 2.6 }, 'flow-dot-core'));
      const move = svgEl('animateMotion', { dur, begin, repeatCount: 'indefinite', calcMode: 'linear' });
      if (w < 0) { move.setAttribute('keyPoints', '1;0'); move.setAttribute('keyTimes', '0;1'); }
      move.appendChild(svgEl('mpath', { href: `#${f.id}` }));
      const fade = svgEl('animate', { attributeName: 'opacity', values: '0;1;1;0', keyTimes: '0;0.12;0.88;1', dur, begin, repeatCount: 'indefinite' });
      dot.append(move, fade);
      f.motions.push(move, fade);
      f.holder.appendChild(dot);
    }
    return true;
  }

  let flowView = null;  // the one live diagram, updated in place so animations keep running between refreshes

  /** Builds the static diagram skeleton once. */
  function flowBuild() {
    const root = svgEl('svg', { viewBox: '0 0 400 366', role: 'img' }, 'flow');
    const view = { root, flows: {}, nodes: {} };
    root.appendChild(flowDefs());
    for (const kind in FLOW_EDGES) {
      const e = FLOW_EDGES[kind], id = `flow-p-${kind}`;
      const g = flowColour(svgEl('g', {}), e.colour);
      g.appendChild(svgEl('path', { id, d: e.d }, 'flow-track'));
      const holder = svgEl('g', {});
      g.appendChild(holder);
      root.appendChild(g);
      view.flows[kind] = { id, g, d: e.d, colour: e.colour, rev: e.rev, holder, key: '', motions: [], secs: 0 };
    }
    for (const kind in FLOW_NODES) {
      view.nodes[kind] = flowNode(kind);
      root.appendChild(view.nodes[kind].g);
    }
    return view;
  }

  /** Splits pv/load/battery/grid watts into the six pairwise flows (pv first to home, then battery, then grid; battery discharge serves home before export). */
  function splitFlows(s) {
    const pos = (w) => Math.max(w == null ? 0 : w, 0);
    const pv = pos(s.pv_w), load = pos(s.load_w), charge = pos(s.battery_w), discharge = pos(-s.battery_w);
    const imp = pos(s.grid_w), exp = pos(-s.grid_w);
    const pvHome = Math.min(pv, load);
    let r = pv - pvHome;
    const pvBat = Math.min(r, charge);
    r -= pvBat;
    const pvGrid = Math.min(r, exp);
    let loadLeft = load - pvHome;
    const batHome = Math.min(discharge, loadLeft);
    loadLeft -= batHome;
    const batGrid = Math.min(discharge - batHome, exp - pvGrid);
    const gridHome = Math.min(imp, loadLeft);
    const gridBat = Math.min(imp - gridHome, charge - pvBat);
    return { pvHome, pvBat, pvGrid, batHome, gridHome, batGrid: batGrid - gridBat };
  }

  /** True while the car's cable is in: it then gets a node in the flow diagram. */
  const carPlugged = (car) => car.plugged_in === true || car.charging_state === 'Charging';

  /** "charging", "complete" or "plugged in", for a plugged-in car. */
  function carWord(car) {
    return car.charging_state === 'Charging' ? 'charging' : car.charging_state === 'Complete' ? 'complete' : 'plugged in';
  }

  /** Animated Solar / Home / Battery / Grid power-flow diagram in the Sungrow app layout, plus the car while it is plugged in. */
  function flowDiagram(s, car) {
    if (!flowView) flowView = flowBuild();
    const word = (w, pos, neg) => (w == null || Math.abs(w) < FLOW_MIN_W ? 'idle' : w > 0 ? pos : neg);
    const val = (w) => (w == null ? '–' : `${kw(Math.abs(w))} kW`);
    const batWord = word(s.battery_w, 'charging', 'discharging');
    const gridWord = word(s.grid_w, 'importing', 'exporting');
    const soc = s.soc_pct == null ? '' : `${Math.round(s.soc_pct)}%`;
    const watts = { solar: s.pv_w, home: s.load_w, grid: s.grid_w, battery: s.battery_w };
    const showCar = !!car && carPlugged(car), charging = showCar && car.charging_state === 'Charging';
    const notes = {
      solar: [word(s.pv_w, 'generating', 'generating'), '--c-pv'],
      // The car sits behind the house meter, so its power is already part of the home load.
      home: [word(s.load_w, 'consuming', 'consuming') + (charging ? ' · incl. car' : ''), '--c-load'],
      grid: [gridWord, s.grid_w < 0 ? '--c-export' : '--c-import'],
      battery: [soc ? `${batWord} · ${soc}` : batWord, s.battery_w < 0 ? '--c-discharge' : '--c-charge'],
    };
    const { root, flows, nodes } = flowView;
    const split = splitFlows(s);
    const carW = !charging ? 0 : car.charger_power_kw == null ? null : car.charger_power_kw * 1000;
    split.homeCar = carW;
    for (const kind in flows) flowUpdate(flows[kind], split[kind]);
    for (const g of [nodes.car.g, flows.homeCar.g]) g.toggleAttribute('hidden', !showCar);  // SVG elements have no .hidden property
    let carText = '';
    if (showCar) {
      const pct = car.battery_level_pct == null ? '' : `${Math.round(car.battery_level_pct)}%`;
      watts.car = carW;
      notes.car = [pct ? `${carWord(car)} · ${pct}` : carWord(car), '--c-ev'];
      nodes.car.label.textContent = car.display_name || 'Car';
      carText = `, car ${carWord(car)}${charging ? ` ${val(carW)}` : ''}${pct ? ` at ${pct}` : ''}`;
    }
    for (const kind in watts) {
      const n = nodes[kind];
      n.g.classList.toggle('on', watts[kind] != null && Math.abs(watts[kind]) >= FLOW_MIN_W);
      n.value.textContent = val(watts[kind]);
      n.note.textContent = notes[kind][0];
      n.note.style.setProperty('--note', `var(${notes[kind][1]})`);
    }
    nodes.solar.rays.classList.toggle('spin', s.pv_w != null && s.pv_w >= FLOW_MIN_W);
    const pct = s.soc_pct == null ? 0 : Math.max(0, Math.min(100, s.soc_pct));
    nodes.battery.fill.setAttribute('width', (12.8 * pct / 100).toFixed(2));
    nodes.battery.fill.classList.toggle('low', pct <= 15);
    root.setAttribute('aria-label', `Solar ${val(s.pv_w)}, home ${val(s.load_w)}, battery ${batWord} ${val(s.battery_w)}` +
      `${soc ? ` at ${soc}` : ''}, grid ${gridWord} ${val(s.grid_w)}${carText}`);
    return root;
  }

  function renderLiveTiles(sample, car) {
    const host = $('live-tiles'), diagram = flowDiagram(sample, car);
    if (diagram.parentNode !== host || host.childNodes.length !== 1) host.replaceChildren(diagram);
    setStatus(`Live from inverter · updated ${ageText(Date.now() / 1000 - sample.ts)}`, false);
  }

  /** "h m" for a duration in hours: "1 h 30 m", "45 m". */
  function hoursText(h) {
    const mins = Math.round(h * 60);
    return mins >= 60 ? `${Math.floor(mins / 60)} h ${mins % 60} m` : `${mins} m`;
  }

  /** The share of a costed session's energy that came from solar, 0 to 100, or null. */
  function solarPct(session) {
    const total = session.solar_kwh + session.battery_kwh + session.grid_kwh;
    return session.cost == null || !(total > 0) ? null : Math.round(session.solar_kwh / total * 100);
  }

  const CAR_SESSION_MS = 5 * 60 * 1000;  // costing a session reads its live samples, so not on every 15 s refresh
  let carSession = { at: 0, charging: null, value: null };

  /** The session shown on the car line: the one still charging, else the latest one that ended today. Null if none, or if the request fails.
   * Reused for 5 minutes, or until the car starts or stops charging. */
  async function fetchCarSession(car) {
    const charging = car.charging_state === 'Charging';
    if (Date.now() - carSession.at < CAR_SESSION_MS && charging === carSession.charging) return carSession.value;
    const now = Math.floor(Date.now() / 1000), today = todayStr();
    let value = null;
    try {
      const { sessions } = await api(`/api/ev/sessions?from=${now - 36 * 3600}&to=${now}`);
      const todays = sessions.filter((c) => c.ended_at == null || fmtDate.format(new Date(c.ended_at * 1000)) === today);
      value = todays.length ? todays[todays.length - 1] : null;
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
      return null;
    }
    carSession = { at: Date.now(), charging, value };
    return value;
  }

  /** One-line summary of the car under the power diagram; hidden when there is no car. `session` is from fetchCarSession. */
  function renderCarTile(car, session) {
    const host = $('car-tile');
    host.hidden = !car;
    if (!car) return;
    const pct = (v) => (v == null ? null : `${Math.round(v)}%`);
    const level = pct(car.battery_level_pct), limit = pct(car.charge_limit_soc_pct);
    let parts;
    if (carPlugged(car)) {
      const charging = car.charging_state === 'Charging';
      parts = [
        charging && car.charger_power_kw != null ? `charging ${car.charger_power_kw.toFixed(1)} kW` : carWord(car),
        level && limit ? `${level} → ${limit}` : level,
        car.charge_energy_added_kwh == null ? null : `+${car.charge_energy_added_kwh.toFixed(1)} kWh`,
        charging && car.time_to_full_charge_h != null ? `full in ${hoursText(car.time_to_full_charge_h)}` : null,
      ];
    } else {
      parts = [level, car.state, car.geofence];
    }
    if (session && session.cost != null) {
      const solar = solarPct(session);
      parts.push(solar == null ? null : `${solar}% solar`, money(session.cost));
    }
    const line = el('span', 'car-text');
    line.appendChild(el('span', 'car-name', car.display_name || 'Car'));
    line.append(...parts.filter(Boolean).map((p) => ` · ${p}`));
    host.replaceChildren(line, el('span', 'car-age', `updated ${ageText(Date.now() / 1000 - car.updated_ts)}`));
  }

  function renderCloudTiles(latest) {
    const byId = {};
    for (const p of latest.points) byId[p.point_id] = p;
    const watt = (id) => {
      const p = byId[id];
      if (!p || p.value == null) return null;
      return p.unit === 'kW' ? p.value * 1000 : p.value;
    };
    const pv = watt(P_PV), load = watt(P_LOAD);
    const soc = byId[P_SOC] && byId[P_SOC].value;
    $('live-tiles').replaceChildren(
      powerTile('Solar', pv == null ? '–' : kw(pv), 'kW', '--c-pv'),
      powerTile('Home', load == null ? '–' : kw(load), 'kW', '--c-load'),
      powerTile('Battery charge', soc == null ? '–' : String(Math.round(soc)), '%', '--c-battery',
        soc == null ? null : socBar(soc)),
    );
    const ts = Math.max(0, ...[P_PV, P_LOAD, P_SOC].map((id) => (byId[id] ? byId[id].ts : 0)));
    setStatus(ts
      ? `From iSolarCloud · updated ${ageText(Date.now() / 1000 - ts)} (${fmtLong.format(new Date(ts * 1000))})`
      : 'No readings yet', true);
  }

  function stat(label, valueText, unit, note) {
    const s = el('div', 'stat');
    const v = el('div', 'value', valueText);
    if (unit) v.appendChild(el('span', 'unit', unit));
    s.append(el('div', 'label', label), v);
    if (note) s.appendChild(el('div', 'note', note));
    return s;
  }

  function renderToday(t, costDay) {
    const pct = (t.load_kwh > 0 && t.import_kwh != null)
      ? Math.round(Math.max(0, Math.min(100, (1 - t.import_kwh / t.load_kwh) * 100))) : null;
    const items = [
      stat('Generated', kwh(t.pv_kwh), 'kWh'),
      stat('Used', kwh(t.load_kwh), 'kWh'),
      stat('Imported', kwh(t.import_kwh), 'kWh'),
      stat('Exported', kwh(t.export_kwh), 'kWh'),
      stat('Self-powered', pct == null ? '–' : String(pct), '%'),
    ];
    if (costDay) {
      items.push(stat('Cost so far', money(costDay.actual), '',
        costDay.credit > 0 ? `${money(costDay.credit)} evening credit earned` : ''));
    }
    $('today-tiles').replaceChildren(...items);
  }

  let lastNowChartAt = 0;

  /** Loads the Now view. Returns the delay before the next refresh. */
  async function loadNow() {
    const today = todayStr();
    const [{ sample, car }, daily, savings] = await Promise.all([
      fetchLive(),
      api(`/api/daily?from=${today}&to=${today}`),
      loadSavings().catch((err) => { if (err.message === 'Not logged in.') throw err; return null; }),
    ]);
    if (state.view !== 'now') return LIVE_REFRESH_MS;
    renderHero(savings);
    const todayTotals = daily.days[0] || {};
    renderToday(todayTotals,
      savings && savings.configured ? savings.days.find((d) => d.date === today) : null);
    // Say where today's totals come from (see "source" in /api/daily).
    $('today-source').textContent =
      todayTotals.source === 'inverter' ? '(inverter)'
      : todayTotals.source === 'cloud' ? '(iSolarCloud)' : '';
    let delay = REFRESH_MS;
    if (sample) {
      renderLiveTiles(sample, car);
      delay = LIVE_REFRESH_MS;
    } else {
      renderCloudTiles(await api('/api/latest'));
    }
    renderCarTile(car, car ? await fetchCarSession(car) : null);
    // The chart needs two heavier requests, so it refreshes on the slower cadence.
    if (Date.now() - lastNowChartAt >= REFRESH_MS - 1000) {
      lastNowChartAt = Date.now();
      try {
        await loadDayChart('now-chart', 'now-source', today, () => state.view === 'now');
      } catch (err) {
        lastNowChartAt = 0;
        throw err;
      }
    }
    return delay;
  }

  // ---------- Day ----------

  const DAY_SERIES = [
    // key, label, colour var, sign, kind
    ['pv_w', 'Solar', '--c-pv', 1, 'area'],
    ['load_w', 'Home', '--c-load', 1, 'line'],
    ['grid_import_w', 'Grid import', '--c-import', 1, 'line'],
    ['grid_export_w', 'Grid export', '--c-export', -1, 'area'],
    ['battery_charge_w', 'Battery charge', '--c-charge', 1, 'line'],
    ['battery_discharge_w', 'Battery discharge', '--c-discharge', 1, 'line'],
  ];

  /** Inverter minute rows for [start, end) (unix seconds), or [] if unavailable. */
  async function fetchLiveDay(start, end) {
    try {
      return (await api(`/api/live/series?from=${start}&to=${end - 1}`)).data || [];
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
      return [];
    }
  }

  /**
   * The car's charging sessions overlapping [start, end) (unix seconds) and the
   * car's name. The sessions are [] if the request fails (or TeslaMate is off):
   * the chart is drawn without them.
   */
  async function fetchCharging(start, end) {
    const { car } = await fetchLive();
    const name = (car && car.display_name) || 'Car';
    try {
      const sessions = (await api(`/api/ev/sessions?from=${start}&to=${end - 1}`)).sessions || [];
      return { name, sessions };
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
      return { name, sessions: [] };
    }
  }

  /** One `[ts, value]` series from inverter rows; `pick(row)` gives the value or null. */
  function liveSeries(rows, pick) {
    const out = [];
    for (const row of rows) {
      const v = pick(row);
      if (v == null) continue;
      if (out.length && row[0] - out[out.length - 1][0] > LIVE_GAP_SECS) out.push([row[0] - 60, null]);
      out.push([row[0], v]);
    }
    return out;
  }

  /**
   * Picks the data for the Day chart: the cloud series, or the inverter's when it
   * covers at least LIVE_MIN_MINUTES. When the inverter started mid-day, the cloud
   * series is used up to that moment and the inverter's after it.
   */
  function mergeDay(cloudSeries, rows) {
    const byKey = {};
    for (const s of cloudSeries) byKey[s.key] = s.data;
    const hasCloud = Object.values(byKey).some((d) => d.length > 0);
    const liveRows = rows.filter((r) => r.slice(1).some((v) => v != null));
    if (liveRows.length < LIVE_MIN_MINUTES) {
      return { byKey, source: hasCloud ? 'Source: iSolarCloud (5-min)' : '' };
    }
    const first = liveRows[0][0];
    const gridPart = (sign) => (r) => (r[3] == null ? null : Math.max(sign * r[3], 0));
    const livePart = {
      pv_w: (r) => r[1],
      load_w: (r) => r[2],
      grid_import_w: gridPart(1),
      grid_export_w: gridPart(-1),
      battery_charge_w: (r) => (r[4] == null ? null : Math.max(r[4], 0)),
      battery_discharge_w: (r) => (r[4] == null ? null : Math.max(-r[4], 0)),
      soc: (r) => r[5],
    };
    let usedCloud = false;
    const merged = {};
    for (const [key, pick] of Object.entries(livePart)) {
      const before = (byKey[key] || []).filter((d) => d[0] < first);
      if (before.length) usedCloud = true;
      merged[key] = before.concat(liveSeries(liveRows, pick));
    }
    return {
      byKey: merged,
      source: usedCloud
        ? `Source: iSolarCloud (5-min) until ${clock(first * 1000)}, then inverter (1-min)`
        : 'Source: inverter (1-min)',
    };
  }

  async function loadDay(date) {
    $('day-input').value = date;
    const current = () => state.view === 'day' && state.day === date;
    const [day, daily] = await Promise.all([
      api(`/api/day?date=${date}`),
      api(`/api/daily?from=${date}&to=${date}`),
    ]);
    if (!current()) return;
    renderTotals($('day-totals'), daily.days[0] || {});
    await loadDayChart('day-chart', 'day-source', date, current, day);
  }

  /**
   * Draws the power chart for `date` (the Day view and the Now view share it).
   * `current()` says whether the view is still showing this chart; `day` is an
   * already-fetched /api/day response, if the caller has one.
   */
  async function loadDayChart(chartId, noteId, date, current, day) {
    day = day || await api(`/api/day?date=${date}`);
    const start = day.start * 1000;
    const end = day.end * 1000;
    const [live, charging] = await Promise.all([
      fetchLiveDay(day.start, day.end),
      fetchCharging(day.start, day.end),
    ]);
    if (!current()) return;
    const { byKey, source } = mergeDay(day.series, live);
    const hasData = Object.values(byKey).some((d) => d.length > 0);
    const note = $(noteId);
    note.textContent = source;
    note.hidden = !hasData;

    const th = baseTheme();
    const narrow = window.innerWidth < 560;
    const step = (narrow ? 6 : 3) * 3600 * 1000;

    const series = DAY_SERIES.map(([key, label, colourVar, sign, kind]) => {
      const colour = css(colourVar);
      const s = {
        name: label, type: 'line', showSymbol: false, sync: true,
        data: (byKey[key] || []).map(([t, v]) => [t * 1000, sign * v / 1000]),
        lineStyle: { width: 2, color: colour }, itemStyle: { color: colour },
        emphasis: { disabled: true }, z: 3,
      };
      if (kind === 'area') s.areaStyle = { color: colour, opacity: 0.22 };
      return s;
    });
    // SOC on the right axis, with the free and peak windows shaded on the first series.
    series.push({
      name: 'Battery SOC', type: 'line', yAxisIndex: 1, showSymbol: false,
      data: (byKey.soc || []).map(([t, v]) => [t * 1000, v]),
      lineStyle: { width: 2, type: 'dashed', color: css('--c-soc') }, itemStyle: { color: css('--c-soc') },
      emphasis: { disabled: true }, z: 2,
    });
    series[0].markArea = {
      silent: true, z: 1,
      label: { color: th.muted, fontSize: 12, position: 'insideTop' },
      data: [
        [{ name: 'Free 11–2', xAxis: adelaideMs(date, 11), itemStyle: { color: css('--free') } },
          { xAxis: adelaideMs(date, 14) }],
        [{ name: 'Peak', xAxis: adelaideMs(date, 18), itemStyle: { color: css('--peak') } },
          { xAxis: adelaideMs(date, 21) }],
      ],
    };

    // Charging sessions are bands along the bottom of the plot, on a hidden
    // 0-1 axis, so they stay clear of the Free and Peak shading. A session that
    // is still running ends now.
    const ev = css('--c-ev');
    const chargingName = `${charging.name} charging`;
    const nowMs = Date.now();
    const bands = charging.sessions.map((c) => ({
      from: Math.max(c.started_at * 1000, start),
      to: Math.min(c.ended_at == null ? nowMs : c.ended_at * 1000, end),
    }));
    if (bands.length) {
      series.push({
        name: chargingName, type: 'line', yAxisIndex: 2, data: [],
        lineStyle: { width: 2, color: ev }, itemStyle: { color: ev },
        markArea: {
          silent: true, z: 1,
          itemStyle: { color: ev, opacity: 0.55 },
          label: { color: th.fg, fontSize: 11, position: 'insideLeft' },
          data: bands.map((b) => [{ name: chargingName, xAxis: b.from, yAxis: 0 }, { xAxis: b.to, yAxis: 0.07 }]),
        },
      });
    }

    const option = {
      animation: false,
      grid: { left: 8, right: 8, top: narrow ? 108 : 56, bottom: 8, containLabel: true },
      legend: { top: 0, left: 8, right: 8, textStyle: { color: th.fg }, itemWidth: 16, itemHeight: 8 },
      textStyle: { fontFamily: css('--font') },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'line' }, ...tooltipStyle(),
        formatter(params) {
          params = params.filter((p) => p.value[1] != null);
          if (!params.length) return '';
          const rows = params.map((p) => {
            const v = p.value[1];
            const isSoc = p.seriesName === 'Battery SOC';
            const txt = isSoc ? `${v.toFixed(0)} %` : `${Math.abs(v).toFixed(2)} kW`;
            return `<div style="display:flex;justify-content:space-between;gap:16px">`
              + `<span>${p.marker}${p.seriesName}</span><b>${txt}</b></div>`;
          });
          const t = params[0].value[0];
          if (bands.some((b) => t >= b.from && t <= b.to)) {
            rows.push(`<div><span style="display:inline-block;margin-right:4px;border-radius:10px;`
              + `width:10px;height:10px;background:${ev}"></span>${esc(chargingName)}</div>`);
          }
          return `<div style="margin-bottom:4px">${clock(t)}</div>${rows.join('')}`;
        },
      },
      xAxis: {
        type: 'value', min: start, max: end, interval: step,
        axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => clock(v) },
        axisLine: { lineStyle: { color: th.line } }, splitLine: { show: false },
      },
      yAxis: [
        {
          type: 'value', name: 'kW', nameTextStyle: { color: th.muted },
          axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => v.toFixed(1) },
          splitLine: { lineStyle: { color: th.line } },
        },
        {
          type: 'value', name: 'SOC %', min: 0, max: 100, nameTextStyle: { color: th.muted },
          axisLabel: { color: th.muted, fontFamily: css('--mono') }, splitLine: { show: false },
        },
        // Holds the charging bands only.
        { type: 'value', min: 0, max: 1, show: false },
      ],
      series,
    };
    const peak = Math.max(0, ...(byKey.pv_w || []).map((d) => d[1]));
    showChart(chartId, hasData, option,
      hasData
        ? `Power chart for ${date}: peak solar ${kw(peak)} kW; PV, load, grid import and export, battery charge and discharge in kW, battery state of charge in percent.`
        : `Power chart for ${date}: no data`);
  }

  // ---------- Month ----------

  function sum(days, key) {
    const vals = days.map((d) => d[key]).filter((v) => v != null);
    return vals.length ? vals.reduce((a, b) => a + b, 0) : null;
  }

  function sumTotals(days) {
    const t = {};
    for (const [, key] of TOTALS) t[key] = sum(days, key);
    return t;
  }

  function energyOption(labels, days, tooltipTitle, narrow, thin = true) {
    const th = baseTheme();
    const v = (key) => days.map((d) => (d[key] == null ? null : d[key]));
    return {
      animation: false,
      grid: { left: 8, right: 8, top: narrow ? 72 : 44, bottom: 8, containLabel: true },
      legend: { top: 0, left: 8, right: 8, textStyle: { color: th.fg }, itemWidth: 16, itemHeight: 8 },
      textStyle: { fontFamily: css('--font') },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle(),
        formatter(params) {
          const rows = params.map((p) => {
            const val = p.value == null ? null : Math.abs(p.value);
            return `<div style="display:flex;justify-content:space-between;gap:16px">`
              + `<span>${p.marker}${p.seriesName}</span><b>${val == null ? '–' : val.toFixed(1) + ' kWh'}</b></div>`;
          });
          return `<div style="margin-bottom:4px">${esc(tooltipTitle(params[0].dataIndex))}</div>${rows.join('')}`;
        },
      },
      xAxis: {
        type: 'category', data: labels,
        axisLabel: { color: th.muted, fontFamily: css('--mono'), interval: narrow && thin ? 'auto' : 0 },
        axisLine: { lineStyle: { color: th.line } },
      },
      yAxis: {
        type: 'value', name: 'kWh', nameTextStyle: { color: th.muted },
        axisLabel: { color: th.muted, fontFamily: css('--mono') }, splitLine: { lineStyle: { color: th.line } },
      },
      series: [
        { name: 'PV generation', type: 'bar', stack: 'e', data: v('pv_kwh'), itemStyle: { color: css('--c-pv') } },
        { name: 'Grid import', type: 'bar', stack: 'e', data: v('import_kwh'), itemStyle: { color: css('--c-import') } },
        { name: 'Grid export', type: 'bar', stack: 'e', data: days.map((d) => (d.export_kwh == null ? null : -d.export_kwh)), itemStyle: { color: css('--c-export') } },
        { name: 'Load', type: 'line', data: v('load_kwh'), showSymbol: false, lineStyle: { width: 2, color: css('--c-load') }, itemStyle: { color: css('--c-load') }, z: 5 },
      ],
    };
  }

  async function loadMonth(ym) {
    $('month-input').value = ym;
    const [y, m] = ym.split('-').map(Number);
    const last = new Date(Date.UTC(y, m, 0)).getUTCDate();
    const from = `${ym}-01`, to = `${ym}-${pad(last)}`;
    const { days } = await api(`/api/daily?from=${from}&to=${to}`);
    if (state.view !== 'month' || state.month !== ym) return;
    const hasData = days.some((d) => TOTALS.some(([, k]) => d[k] != null));
    renderTotals($('month-totals'), sumTotals(days));
    const labels = days.map((d) => String(+d.date.slice(8)));
    const option = energyOption(labels, days, (i) => days[i].date, window.innerWidth < 560);
    showChart('month-chart', hasData, option,
      `Daily energy for ${MONTHS[m - 1]} ${y}: stacked bars of PV generation and grid import above the axis, grid export below, with a line for load, in kWh.`);
  }

  // ---------- Year ----------

  async function loadYear(year) {
    $('year-input').value = year;
    const { days } = await api(`/api/daily?from=${year}-01-01&to=${year}-12-31`);
    if (state.view !== 'year' || state.year !== year) return;
    const months = MONTHS.map(() => []);
    for (const d of days) months[+d.date.slice(5, 7) - 1].push(d);
    const agg = months.map((ds) => sumTotals(ds));
    const hasData = days.some((d) => TOTALS.some(([, k]) => d[k] != null));
    renderTotals($('year-totals'), sumTotals(days));
    const option = energyOption(MONTHS, agg, (i) => `${MONTHS[i]} ${year}`, window.innerWidth < 560, false);
    showChart('year-chart', hasData, option,
      `Monthly energy for ${year}: stacked bars of PV generation and grid import above the axis, grid export below, with a line for load, in kWh.`);
  }

  // ---------- shared bits for the money views ----------

  /** A big-number card. `cls` is "save", "cost" or "". */
  function kpi(cls, label, valueText, unit, note) {
    const k = el('div', `kpi ${cls}`.trim());
    const v = el('div', 'value', valueText);
    if (unit) v.appendChild(el('span', 'unit', unit));
    k.append(el('div', 'label', label), v);
    if (note) k.appendChild(el('div', 'note', note));
    return k;
  }

  /** Shows the "add this to the config" message instead of a view's content. */
  function setConfigured(prefix, ok, message) {
    const note = $(`${prefix}-unconfigured`);
    note.textContent = message;
    note.hidden = ok;
    $(`${prefix}-body`).hidden = !ok;
  }

  /** Groups costed days by month ("YYYY-MM"), or by day, oldest first. */
  function bucketDays(days, byDay) {
    const map = new Map();
    for (const d of days) {
      const key = byDay ? d.date : d.date.slice(0, 7);
      let b = map.get(key);
      if (!b) map.set(key, (b = { key, actual: 0, baseline: 0, savings: 0, credit: 0, creditDays: 0, days: 0 }));
      b.actual += d.actual;
      b.baseline += d.baseline;
      b.savings += d.savings;
      b.credit += d.credit;
      if (d.credit > 0) b.creditDays++;
      b.days++;
    }
    return [...map.values()];
  }

  /** Label for a bucket key: "Feb 2023" for months, "21 Oct" for days. */
  function bucketLabel(key) {
    return key.length === 7 ? monthYear(`${key}-01`) : `${+key.slice(8)} ${MONTHS[+key.slice(5, 7) - 1]}`;
  }

  /** Tooltip HTML from `[marker, name, text]` rows; every API-derived string is escaped. */
  function moneyTooltip(rows, title) {
    return `<div style="margin-bottom:4px">${esc(title)}</div>${rows.map(([marker, name, text]) =>
      `<div style="display:flex;justify-content:space-between;gap:16px"><span>${marker}${esc(name)}</span><b>${esc(text)}</b></div>`).join('')}`;
  }

  /** Fills a `<table class="data">`'s body from arrays of `[text, className]` cells. */
  function fillTable(tableId, rows) {
    const body = $(tableId).tBodies[0];
    body.replaceChildren(...rows.map((cells) => {
      const tr = document.createElement('tr');
      for (const [text, cls] of cells) tr.appendChild(el('td', cls || '', text));
      return tr;
    }));
  }

  // ---------- Savings ----------

  const RANGES = ['month', 'year', 'all'];

  function inRange(days, range) {
    const today = todayStr();
    if (range === 'month') return days.filter((d) => d.date.startsWith(today.slice(0, 7)));
    if (range === 'year') return days.filter((d) => d.date.startsWith(today.slice(0, 4)));
    return days;
  }

  function savingsOption(buckets, byDay, purchases) {
    const th = baseTheme();
    let run = 0;
    const cumulative = buckets.map((b) => +(run += b.savings).toFixed(2));
    const unit = byDay ? 'day' : 'month';
    const marks = purchases
      .map((p) => ({ name: p.name, key: byDay ? p.date : p.date.slice(0, 7) }))
      .filter((p) => buckets.some((b) => b.key === p.key))
      .slice(1) // the first purchase is where the line starts
      .map((p) => ({ name: p.name, xAxis: bucketLabel(p.key) }));
    return {
      animation: false,
      textStyle: { fontFamily: css('--font') },
      grid: { left: 8, right: 16, top: 64, bottom: 8, containLabel: true },
      legend: { top: 0, left: 8, textStyle: { color: th.fg }, itemWidth: 16, itemHeight: 8 },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle(),
        formatter: (params) => moneyTooltip(
          params.map((p) => [p.marker, p.seriesName, money(p.value)]), params[0].axisValueLabel),
      },
      xAxis: {
        type: 'category', data: buckets.map((b) => bucketLabel(b.key)),
        axisLabel: { color: th.muted, fontFamily: css('--mono'), hideOverlap: true },
        axisLine: { lineStyle: { color: th.line } },
      },
      yAxis: {
        type: 'value', name: 'Dollars ($)', nameTextStyle: { color: th.muted, align: 'left' },
        axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => `$${v.toLocaleString('en-AU')}` },
        splitLine: { lineStyle: { color: th.line } },
      },
      series: [
        { name: `Saved per ${unit}`, type: 'bar', data: buckets.map((b) => +b.savings.toFixed(2)), itemStyle: { color: css('--save-bar') }, barMaxWidth: 28 },
        {
          name: 'Saved (cumulative)', type: 'line', data: cumulative, showSymbol: false,
          lineStyle: { width: 3, color: css('--save') }, itemStyle: { color: css('--save') }, z: 5,
          markLine: marks.length ? {
            silent: true, symbol: 'none', lineStyle: { color: css('--save-label'), type: 'dashed' },
            label: { color: css('--save-label'), formatter: '{b}' }, data: marks,
          } : undefined,
        },
      ],
    };
  }

  async function loadSavingsView(range) {
    for (const b of document.querySelectorAll('[data-range]')) {
      b.setAttribute('aria-pressed', String(b.dataset.range === range));
    }
    const data = await loadSavings();
    if (state.view !== 'savings' || state.range !== range) return;
    setConfigured('savings', data.configured, NOT_CONFIGURED_TARIFF);
    if (!data.configured) return;
    const days = inRange(data.days, range);
    const sumOf = (key) => days.reduce((a, d) => a + d[key], 0);
    const saved = range === 'all' ? data.saved : sumOf('savings');
    const creditDays = days.filter((d) => d.credit > 0).length;
    $('savings-cards').replaceChildren(
      kpi('save', 'Saved', money(saved, 0), '', days.length ? `about ${money(saved / days.length)} a day` : ''),
      kpi('', 'What you paid', money(sumOf('actual'), 0), '', 'supply charge, imports, minus credits'),
      kpi('', 'Without solar or battery', money(sumOf('baseline'), 0), '', 'same usage at your tariff'),
      kpi('', 'Evening credit earned', creditDays.toLocaleString('en-AU'), 'days', `of ${days.length.toLocaleString('en-AU')} · ${money(sumOf('credit'), 0)} in credits`),
    );
    const byDay = range === 'month';
    const buckets = bucketDays(days, byDay);
    showChart('savings-chart', buckets.length > 0, savingsOption(buckets, byDay, data.purchases || []),
      buckets.length
        ? `Running total of savings, rising to ${money(saved, 0)}, with savings per ${byDay ? 'day' : 'month'} as bars, in dollars.`
        : 'Running total of savings: no data');
    $('savings-table-title').textContent = byDay ? 'By day' : 'By month';
    $('savings-table').tHead.rows[0].cells[0].textContent = byDay ? 'Day' : 'Month';
    const nowKey = byDay ? todayStr() : todayStr().slice(0, 7);
    fillTable('savings-table', buckets.slice().reverse().map((b) => [
      [bucketLabel(b.key) + (b.key === nowKey && !byDay ? ' (so far)' : '')],
      [money(b.actual)], [money(b.baseline)], [money(b.savings), 'save'],
      [byDay ? (b.creditDays ? 'yes' : 'no') : `${b.creditDays} / ${b.days}`],
    ]));
  }

  // ---------- Payback ----------

  const NOT_CONFIGURED_PAYBACK =
    'Add a [tariff] section and at least one [[system.purchase]] section to the config file to see when the system pays for itself.';

  /** A thin progress bar. */
  function meter(pct, colourVar) {
    const m = el('div', 'meter');
    const f = el('span');
    f.style.width = `${Math.max(0, Math.min(100, pct))}%`;
    f.style.background = `var(${colourVar})`;
    m.appendChild(f);
    return m;
  }

  /** "Mar 2028" for "2028-03". */
  const ymLabel = (ym) => `${MONTHS[+ym.slice(5, 7) - 1]} ${ym.slice(0, 4)}`;

  /** Month after `ym` ("YYYY-MM"). */
  const nextMonth = (ym) => shiftMonth(ym, 1);

  /**
   * Chart option for net position: actual month-end points, a dashed line to the
   * projected payback month, and the purchases as dots.
   */
  function paybackOption(pb) {
    const th = baseTheme();
    const months = pb.series.map((p) => p.month);
    const actual = pb.series.map((p) => p.net);
    const last = pb.series[pb.series.length - 1];
    const proj = pb.projected_payback;
    let projected = null;
    if (typeof proj === 'string' && /^\d{4}-\d{2}$/.test(proj) && last && proj > last.month) {
      const extra = [];
      for (let m = nextMonth(last.month); m <= proj; m = nextMonth(m)) extra.push(m);
      months.push(...extra);
      projected = months.map(() => null);
      projected[pb.series.length - 1] = last.net;
      // The same growth curve as the server: savings so far follow (e^(gt) - 1),
      // scaled so the line reaches zero in the projected month.
      const g = Math.log(1 + (pb.price_rise_percent || 0) / 100);
      const grown = (months) => (g > 0 ? Math.expm1(g * months / 12) : months);
      extra.forEach((_, i) => {
        projected[pb.series.length + i] = +(last.net * (1 - grown(i + 1) / grown(extra.length))).toFixed(2);
      });
    }
    const dots = pb.purchases
      .map((p) => ({ name: p.name, i: months.indexOf(p.date.slice(0, 7)) }))
      .filter((p) => p.i >= 0 && p.i < pb.series.length)
      .map((p) => ({ name: p.name, value: [ymLabel(months[p.i]), actual[p.i]] }));
    const labels = months.map(ymLabel);
    const series = [
      {
        name: 'Actual', type: 'line', data: actual, showSymbol: false,
        lineStyle: { width: 3, color: css('--save') }, itemStyle: { color: css('--save') }, z: 4,
        markLine: {
          silent: true, symbol: 'none', label: { show: false },
          lineStyle: { color: css('--muted'), width: 1.5, type: 'solid' }, data: [{ yAxis: 0 }],
        },
      },
    ];
    if (projected) {
      series.push({
        name: 'Projected', type: 'line', data: projected, showSymbol: false,
        lineStyle: { width: 3, type: 'dashed', color: css('--save') }, itemStyle: { color: css('--save') },
        markPoint: {
          symbol: 'circle', symbolSize: 12, label: { show: false },
          itemStyle: { color: css('--bg'), borderColor: css('--save'), borderWidth: 2.5 },
          data: [{ name: 'Payback', coord: [ymLabel(proj), 0] }],
        },
      });
    }
    if (dots.length) {
      series.push({
        name: 'Purchase', type: 'scatter', data: dots, symbolSize: 12, z: 6,
        itemStyle: { color: css('--c-pv') },
        label: { show: true, formatter: (p) => p.name, color: css('--c-pv'), position: 'top', fontSize: 12 },
      });
    }
    return {
      animation: false,
      textStyle: { fontFamily: css('--font') },
      grid: { left: 8, right: 24, top: 56, bottom: 28, containLabel: true },
      legend: { top: 0, left: 8, textStyle: { color: th.fg }, itemWidth: 16, itemHeight: 8 },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'line' }, ...tooltipStyle(),
        formatter: (params) => {
          const rows = params.filter((p) => p.seriesName !== 'Purchase' && p.value != null)
            .map((p) => [p.marker, p.seriesName, money(p.value, 0)]);
          return rows.length ? moneyTooltip(rows, params[0].axisValueLabel) : '';
        },
      },
      xAxis: {
        type: 'category', data: labels, boundaryGap: false,
        axisLabel: { color: th.muted, fontFamily: css('--mono'), hideOverlap: true },
        axisLine: { show: false }, axisTick: { show: false },
      },
      yAxis: {
        type: 'value', name: 'Net position ($)', nameTextStyle: { color: th.muted, align: 'left' },
        axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => `${v < 0 ? '-' : ''}$${Math.abs(v).toLocaleString('en-AU')}` },
        splitLine: { lineStyle: { color: th.line } },
      },
      series,
    };
  }

  async function loadPaybackView() {
    const [pb, savings] = await Promise.all([
      loadPayback(),
      loadSavings().catch((err) => { if (err.message === 'Not logged in.') throw err; return null; }),
    ]);
    if (state.view !== 'payback') return;
    setConfigured('payback', pb.configured, NOT_CONFIGURED_PAYBACK);
    if (!pb.configured) return;

    const proj = pb.projected_payback;
    const projText = proj === null ? 'Paid back' : proj === 'never' ? 'Not at this rate' : ymLabel(proj);
    const projNote = proj === null ? 'savings now cover the cost'
      : proj === 'never' ? 'savings are not keeping up'
      : `at about ${money(pb.rate_per_day)} a day (last 12 months), rising ${pb.price_rise_percent}% a year`;
    const recovered = el('div', 'kpi');
    const rv = el('div', 'value', `${pb.recovered_pct.toFixed(1)}%`);
    recovered.append(el('div', 'label', 'Cost recovered'), rv, meter(pb.recovered_pct, '--save'),
      el('div', 'note', `${money(pb.saved, 0)} saved of ${money(pb.total_cost, 0)} spent`));
    const net = kpi('', 'Net position today', money(pb.net, 0), '', 'savings minus what the system cost');
    net.classList.add(pb.net < 0 ? 'cost' : 'positive');
    $('payback-cards').replaceChildren(
      kpi('save', 'Projected payback', projText, '', projNote), recovered, net);

    const hasSeries = pb.series.length > 0;
    showChart('payback-chart', hasSeries, paybackOption(pb),
      `Net position over time, in dollars: starts at minus ${money(pb.total_cost, 0)} cost, now ${money(pb.net, 0)}`
      + (typeof proj === 'string' && proj !== 'never' ? `, projected to reach zero in ${ymLabel(proj)}.` : '.'));

    const days = savings && savings.configured ? savings.days : [];
    $('payback-parts').replaceChildren(...pb.purchases.map((p) => {
      const card = el('div', 'part');
      const head = el('div', 'part-head');
      head.append(el('span', 'part-name', p.name), el('span', 'part-date', ymLabel(p.date)));
      const grid = el('div', 'part-grid');
      const cell = (label, text) => {
        const c = el('div');
        c.append(el('span', 'label', label), el('span', 'num', text));
        return c;
      };
      grid.appendChild(cell('Cost', money(p.cost, 0)));
      if (days.length) {
        const since = days.filter((d) => d.date >= p.date).reduce((a, d) => a + d.savings, 0);
        grid.appendChild(cell('Saved since', money(since, 0)));
      }
      card.append(head, grid);
      return card;
    }));

    fillTable('payback-years', pb.by_year.slice().reverse().map((y) => [
      [String(y.year) + (y.partial ? ' (partial)' : '')],
      [money(y.saved, 0), 'save'],
      [`${y.days}`],
      [y.days ? money(y.saved / y.days) : '–'],
    ]));
  }

  // ---------- Cost ----------

  const NOT_CONFIGURED_COST = 'Add a [tariff] section to the config file to see what your power costs.';

  /** Clustered bars of paid against without-solar cost. `labels`: x labels. */
  function costOption(labels, actual, baseline, perUnit) {
    const th = baseTheme();
    return {
      animation: false,
      textStyle: { fontFamily: css('--font') },
      grid: { left: 8, right: 16, top: 48, bottom: 8, containLabel: true },
      legend: { top: 0, left: 8, textStyle: { color: th.fg }, itemWidth: 12, itemHeight: 10 },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle(),
        formatter: (params) => moneyTooltip(
          params.map((p) => [p.marker, p.seriesName, money(p.value)]), params[0].axisValueLabel),
      },
      xAxis: {
        type: 'category', data: labels,
        axisLabel: { color: th.muted, fontFamily: css('--mono'), hideOverlap: true },
        axisLine: { lineStyle: { color: th.line } },
      },
      yAxis: {
        type: 'value', name: `$ per ${perUnit}`, nameTextStyle: { color: th.muted, align: 'left' },
        axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => `$${v.toLocaleString('en-AU')}` },
        splitLine: { lineStyle: { color: th.line } },
      },
      series: [
        { name: 'Without solar or battery', type: 'bar', data: baseline, itemStyle: { color: css('--baseline') }, barMaxWidth: 26 },
        { name: 'What you paid', type: 'bar', data: actual, itemStyle: { color: css('--save') }, barMaxWidth: 26 },
      ],
    };
  }

  async function loadCostView() {
    const data = await loadSavings();
    if (state.view !== 'cost') return;
    setConfigured('cost', data.configured, NOT_CONFIGURED_COST);
    if (!data.configured) return;
    const today = todayStr();
    const t = data.days.find((d) => d.date === today);
    const avg = (rows, key) => (rows.length ? rows.reduce((a, d) => a + d[key], 0) / rows.length : null);
    const last30 = data.days.filter((d) => d.date > addDays(today, -30));
    const per = (v) => (v == null ? '–' : money(v));
    $('cost-cards').replaceChildren(
      kpi('', 'Today without solar or battery', per(t && t.baseline), '', 'same usage at your tariff'),
      kpi('save', 'Today it actually cost', per(t && t.actual), '',
        t ? `${t.import_kwh.toFixed(1)} kWh imported, ${t.export_kwh.toFixed(1)} kWh exported` : 'no data yet today'),
      kpi('', 'Average a day without solar', per(avg(last30, 'baseline')), '', `last ${last30.length} days`),
      kpi('save', 'Average a day paid', per(avg(last30, 'actual')), '', `last ${last30.length} days`),
    );

    const recent = data.days.filter((d) => d.date > addDays(today, -14));
    showChart('cost-days-chart', recent.length > 0,
      costOption(recent.map((d) => bucketLabel(d.date)), recent.map((d) => d.actual), recent.map((d) => d.baseline), 'day'),
      'Daily cost for the last 14 days: what you paid, against what the same usage would have cost without solar or battery, in dollars.');

    const monthStart = shiftMonth(today.slice(0, 7), -11);
    const buckets = bucketDays(data.days.filter((d) => d.date.slice(0, 7) >= monthStart), false);
    showChart('cost-months-chart', buckets.length > 0,
      costOption(buckets.map((b) => bucketLabel(b.key)), buckets.map((b) => +b.actual.toFixed(2)),
        buckets.map((b) => +b.baseline.toFixed(2)), 'month'),
      'Monthly cost for the last 12 months: what you paid, against what the same usage would have cost without solar or battery, in dollars.');
    fillTable('cost-table', buckets.slice().reverse().map((b) => [
      [bucketLabel(b.key) + (b.key === today.slice(0, 7) ? ' (so far)' : '')],
      [money(b.actual)], [money(b.baseline)], [money(b.actual / b.days)], [money(b.baseline / b.days)],
    ]));
    await loadCostSessions();
  }

  /** Fills the "Charging sessions" table with the last 10 sessions of the last 60 days; the panel is hidden when there are none. */
  async function loadCostSessions() {
    const now = Math.floor(Date.now() / 1000);
    let sessions = [];
    try {
      sessions = (await api(`/api/ev/sessions?from=${now - 60 * 86400}&to=${now}`)).sessions || [];
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
    }
    if (state.view !== 'cost') return;
    $('cost-sessions').hidden = sessions.length === 0;
    const shown = sessions.slice(-10).reverse();
    fillTable('cost-sessions-table', shown.map((c) => {
      const used = c.energy_used_kwh == null ? c.energy_added_kwh : c.energy_used_kwh;
      const solar = solarPct(c);
      const [, month, day] = fmtDate.format(new Date(c.started_at * 1000)).split('-');
      return [
        [`${+day} ${MONTHS[+month - 1]}` + (c.ended_at == null ? ' (now)' : '')],
        [kwh(used)], [solar == null ? '–' : `${solar}%`], [money(c.cost)],
      ];
    }));
  }

  // ---------- routing (hash) ----------

  const VIEWS = ['now', 'day', 'month', 'year', 'savings', 'payback', 'cost'];
  const state = { view: 'now', day: '', month: '', year: 0, range: 'all' };
  let timer = null;
  let pollToken = 0;

  /** Loads Now, then schedules the next refresh (15 s from the inverter, 60 s from the cloud). */
  async function pollNow() {
    clearTimeout(timer);
    timer = null;
    // A hidden tab does not poll; visibilitychange restarts it.
    if (document.hidden) return;
    const token = ++pollToken;
    let delay = REFRESH_MS;
    await guarded(async () => { delay = await loadNow(); });
    if (token === pollToken && state.view === 'now') timer = setTimeout(pollNow, delay);
  }

  function parseHash() {
    let raw = window.location.hash.replace(/^#/, '');
    try { raw = decodeURIComponent(raw); } catch (e) { raw = ''; } // malformed escapes: fall back to Now
    const [view, value] = raw.split('=');
    const today = todayStr();
    state.view = VIEWS.includes(view) ? view : 'now';
    state.range = view === 'savings' && RANGES.includes(value) ? value : 'all';
    state.day = /^\d{4}-\d{2}-\d{2}$/.test(value || '') && view === 'day' ? value : today;
    state.month = /^\d{4}-\d{2}$/.test(value || '') && view === 'month' ? value : today.slice(0, 7);
    state.year = /^\d{4}$/.test(value || '') && view === 'year' ? +value : +today.slice(0, 4);
  }

  /** The hash for a view, carrying that view's current date or range. */
  function viewHash(v) {
    return v === 'day' ? `day=${state.day}` : v === 'month' ? `month=${state.month}`
      : v === 'year' ? `year=${state.year}` : v === 'savings' ? `savings=${state.range}` : v;
  }

  function setHash() {
    const v = state.view;
    const h = viewHash(v);
    if (window.location.hash !== '#' + h) window.location.hash = h; // triggers render via hashchange
    else render();
  }

  function render() {
    parseHash();
    for (const v of document.querySelectorAll('.view')) v.hidden = v.id !== `view-${state.view}`;
    for (const tab of document.querySelectorAll('.tab')) {
      if (tab.dataset.view === state.view) {
        tab.setAttribute('aria-current', 'page');
        // On a phone the tab row scrolls; keep the current tab in view.
        const nav = tab.parentElement;
        if (tab.offsetLeft + tab.offsetWidth > nav.scrollLeft + nav.clientWidth) nav.scrollLeft = tab.offsetLeft + tab.offsetWidth - nav.clientWidth + 16;
      }
      else tab.removeAttribute('aria-current');
      const v = tab.dataset.view;
      tab.setAttribute('href', '#' + viewHash(v));
    }
    clearTimeout(timer);
    timer = null;
    pollToken++;
    lastNowChartAt = 0;
    if (state.view === 'now') pollNow();
    else if (state.view === 'day') guarded(() => loadDay(state.day));
    else if (state.view === 'month') guarded(() => loadMonth(state.month));
    else if (state.view === 'savings') guarded(() => loadSavingsView(state.range));
    else if (state.view === 'payback') guarded(() => loadPaybackView());
    else if (state.view === 'cost') guarded(() => loadCostView());
    else guarded(() => loadYear(state.year));
  }

  function shiftMonth(ym, n) {
    const [y, m] = ym.split('-').map(Number);
    const t = new Date(Date.UTC(y, m - 1 + n, 1));
    return `${t.getUTCFullYear()}-${pad(t.getUTCMonth() + 1)}`;
  }

  function go(patch) { Object.assign(state, patch); setHash(); }

  function init() {
    if (!chartsAvailable()) {
      chartsError = "Charts couldn't load (no internet to cdnjs?)";
      updateBanner();
    }
    $('day-prev').onclick = () => go({ day: addDays(state.day, -1) });
    $('day-next').onclick = () => go({ day: addDays(state.day, 1) });
    $('day-today').onclick = () => go({ day: todayStr() });
    $('day-input').onchange = (e) => { if (e.target.value) go({ day: e.target.value }); };
    $('month-prev').onclick = () => go({ month: shiftMonth(state.month, -1) });
    $('month-next').onclick = () => go({ month: shiftMonth(state.month, 1) });
    $('month-today').onclick = () => go({ month: todayStr().slice(0, 7) });
    $('month-input').onchange = (e) => { if (e.target.value) go({ month: e.target.value }); };
    $('year-prev').onclick = () => go({ year: state.year - 1 });
    $('year-next').onclick = () => go({ year: state.year + 1 });
    $('year-today').onclick = () => go({ year: +todayStr().slice(0, 4) });
    $('year-input').onchange = (e) => { const y = +e.target.value; if (y >= 2000 && y <= 2100) go({ year: y }); };

    for (const b of document.querySelectorAll('[data-range]')) b.onclick = () => go({ range: b.dataset.range });
    window.addEventListener('hashchange', render);
    window.addEventListener('resize', () => { for (const c of Object.values(charts)) c.resize(); });
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && state.view === 'now') pollNow();
    });
    render();
  }

  init();
})();
