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

  /** Calls `fn(dataIndex)` when a bar of chart `id` is clicked (replaces any earlier handler). */
  function onBarClick(id, fn) {
    const c = getChart(id);
    if (!c) return;
    c.off('click');
    c.on('click', (p) => { if (p.seriesType === 'bar') fn(p.dataIndex); });
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

  /**
   * Splits pv/load/battery/grid watts into the six pairwise flows the way the Sungrow app does:
   * solar charges the battery first (the battery sits on the inverter's DC side), then feeds the
   * home, then exports; battery discharge serves the home before export; the grid covers the rest.
   */
  function splitFlows(s) {
    const pos = (w) => Math.max(w == null ? 0 : w, 0);
    const pv = pos(s.pv_w), load = pos(s.load_w), charge = pos(s.battery_w), discharge = pos(-s.battery_w);
    const imp = pos(s.grid_w), exp = pos(-s.grid_w);
    const pvBat = Math.min(pv, charge);
    let r = pv - pvBat;
    const pvHome = Math.min(r, load);
    r -= pvHome;
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

  let lastDayChartAt = 0; // when the Day chart last drew today, for the auto refresh

  /** Loads the Now part of the home page. Returns the delay before the next refresh. */
  async function loadNow() {
    const today = todayStr();
    const [{ sample, car }, daily, savings] = await Promise.all([
      fetchLive(),
      api(`/api/daily?from=${today}&to=${today}`),
      loadSavings().catch((err) => { if (err.message === 'Not logged in.') throw err; return null; }),
    ]);
    if (state.view !== 'home') return LIVE_REFRESH_MS;
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
    // While History shows today's Day chart, refresh it (and its totals) on the slower
    // cadence: the chart needs heavier requests than the tiles.
    if (curveDate() === today && Date.now() - lastDayChartAt >= REFRESH_MS - 1000) {
      await loadDay(today);
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

  const SMOOTH_SECS = 10 * 60; // minimum width of the rolling mean behind the Smooth toggle
  const dayDraws = {};        // chart id -> redraw of its last fetched data

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

  /** Centred rolling mean of a `[t, v]` series, at least `minWidth` wide (same unit as t) and 4 samples wide; null points stay as gaps and are never averaged across. */
  function smoothSeries(data, minWidth) {
    const gaps = [];
    for (let k = 1; k < data.length && gaps.length < 200; k++) {
      if (data[k][1] != null && data[k - 1][1] != null) gaps.push(data[k][0] - data[k - 1][0]);
    }
    gaps.sort((x, y) => x - y);
    const half = Math.max(minWidth, 4 * (gaps[gaps.length >> 1] || 0)) / 2;
    const out = new Array(data.length);
    let i = 0;
    while (i < data.length) {
      if (data[i][1] == null) { out[i] = data[i]; i++; continue; }
      let end = i;
      while (end < data.length && data[end][1] != null) end++;
      let lo = i, hi = i, sum = 0;
      for (let k = i; k < end; k++) {
        while (hi < end && data[hi][0] <= data[k][0] + half) sum += data[hi++][1];
        while (data[lo][0] < data[k][0] - half) sum -= data[lo++][1];
        out[k] = [data[k][0], sum / (hi - lo)];
      }
      i = end;
    }
    return out;
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
    const current = () => state.view === 'home' && curveDate() === date;
    const isToday = date === todayStr();
    $('day-empty').textContent = isToday ? 'No data yet today' : 'No data for this day';
    // Marked before the requests so the home poll does not load the same chart twice.
    if (isToday) lastDayChartAt = Date.now();
    try {
      const [day, daily] = await Promise.all([
        api(`/api/day?date=${date}`),
        api(`/api/daily?from=${date}&to=${date}`),
      ]);
      if (!current()) return;
      renderCards(daily.days[0] || {});
      await loadDayChart('day-chart', 'day-source', date, current, day);
    } catch (err) {
      if (isToday) lastDayChartAt = 0;
      throw err;
    }
  }

  /**
   * Draws the power chart for `date` (the History Day chart).
   * `current()` says whether the page is still showing this chart; `day` is an
   * already-fetched /api/day response, if the caller has one.
   */
  async function loadDayChart(chartId, noteId, date, current, day) {
    day = day || await api(`/api/day?date=${date}`);
    const [live, charging] = await Promise.all([
      fetchLiveDay(day.start, day.end),
      fetchCharging(day.start, day.end),
    ]);
    if (!current()) return;
    const merged = mergeDay(day.series, live);
    // Kept so the Smooth toggle can redraw without refetching.
    dayDraws[chartId] = () => drawDayChart(chartId, noteId, date, day, merged, charging);
    drawDayChart(chartId, noteId, date, day, merged, charging);
  }

  /** Draws the power chart from fetched data, smoothed when the toggle is on. */
  function drawDayChart(chartId, noteId, date, day, { byKey, source }, charging) {
    const start = day.start * 1000;
    const end = day.end * 1000;
    const hasData = Object.values(byKey).some((d) => d.length > 0);
    const note = $(noteId);
    note.textContent = source;
    note.hidden = !hasData;

    const th = baseTheme();
    const narrow = window.innerWidth < 560;
    const step = (narrow ? 6 : 3) * 3600 * 1000;

    const smooth = state.smooth;
    const series = DAY_SERIES.map(([key, label, colourVar, sign, kind]) => {
      const colour = css(colourVar);
      const s = {
        name: label, type: 'line', showSymbol: false, sync: true,
        smooth: smooth ? 0.3 : false,
        data: (smooth ? smoothSeries(byKey[key] || [], SMOOTH_SECS) : byKey[key] || []).map(([t, v]) => [t * 1000, v == null ? null : sign * v / 1000]),
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

  // ---------- History: periods, date pill, cards ----------

  const DOW = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
  const LONG_MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
  /** Custom ranges up to this many days show daily bars; longer ones show monthly bars. */
  const CUSTOM_DAILY_MAX = 92;
  /** /api/daily?group= covers at most 20 years; Lifetime looks back 19 whole years and this one. */
  const LIFETIME_YEARS = 19;
  const ENERGY_KEYS = ['pv_kwh', 'load_kwh', 'import_kwh', 'export_kwh'];

  const utcMs = (d) => Date.UTC(+d.slice(0, 4), +d.slice(5, 7) - 1, +d.slice(8, 10));
  const daysBetween = (a, b) => Math.round((utcMs(b) - utcMs(a)) / 86400000);
  /** 0 for Monday to 6 for Sunday. */
  const dowOf = (d) => (new Date(utcMs(d)).getUTCDay() + 6) % 7;
  const mondayOf = (d) => addDays(d, -dowOf(d));
  const monthEnd = (ym) => `${ym}-${pad(new Date(Date.UTC(+ym.slice(0, 4), +ym.slice(5, 7), 0)).getUTCDate())}`;
  const validDate = (v) => /^\d{4}-\d{2}-\d{2}$/.test(v || '') && new Date(utcMs(v)).toISOString().slice(0, 10) === v;
  const dmy = (d) => `${+d.slice(8)} ${MONTHS[+d.slice(5, 7) - 1]} ${d.slice(0, 4)}`;

  /** The first month with data ("2023-02") once Lifetime has loaded; null until then. */
  let lifetimeFirst = null;
  const lifetimeCache = { at: 0, promise: null };

  /** /api/daily by month for the whole history, cached for a minute. */
  function loadLifetime() {
    if (lifetimeCache.promise && Date.now() - lifetimeCache.at < COSTS_TTL_MS) return lifetimeCache.promise;
    lifetimeCache.at = Date.now();
    const from = `${+todayStr().slice(0, 4) - LIFETIME_YEARS}-01-01`;
    const p = api(`/api/daily?from=${from}&to=${todayStr()}&group=month`).then((r) => {
      lifetimeFirst = r.buckets.length ? r.buckets[0].period : null;
      return r;
    });
    lifetimeCache.promise = p;
    p.catch(() => { lifetimeCache.at = 0; });
    return p;
  }

  /** The date History shows a power curve for (Day, or a one-day Custom range); null for bars. */
  const curveDate = () => (state.period === 'day' ? state.day
    : state.period === 'custom' && state.from === state.to ? state.from : null);

  /** The first and last day History covers. */
  function rangeOf() {
    const today = todayStr();
    switch (state.period) {
      case 'day': return [state.day, state.day];
      case 'week': return [state.week, addDays(state.week, 6)];
      case 'month': return [`${state.month}-01`, monthEnd(state.month)];
      case 'year': return [`${state.year}-01-01`, `${state.year}-12-31`];
      case 'lifetime': return [lifetimeFirst ? `${lifetimeFirst}-01` : `${today.slice(0, 4)}-01-01`, today];
      default: return [state.from, state.to];
    }
  }

  function periodLabel() {
    const [from, to] = rangeOf();
    switch (state.period) {
      case 'day': return `${DOW[dowOf(from)]} ${dmy(from)}`;
      case 'month': return `${LONG_MONTHS[+state.month.slice(5) - 1]} ${state.month.slice(0, 4)}`;
      case 'year': return String(state.year);
      case 'lifetime': return lifetimeFirst ? `Since ${MONTHS[+lifetimeFirst.slice(5) - 1]} ${lifetimeFirst.slice(0, 4)}` : 'Lifetime';
      default: {
        if (from === to) return dmy(from);
        const sameYear = from.slice(0, 4) === to.slice(0, 4);
        const sameMonth = sameYear && from.slice(5, 7) === to.slice(5, 7);
        if (state.period === 'week' && sameMonth) return `${+from.slice(8)}–${dmy(to)}`;
        return `${sameYear ? dayLabel(from) : dmy(from)} – ${dmy(to)}`;
      }
    }
  }

  /** Does the period on show reach today? Then "next" is disabled. */
  function atCurrent() {
    const today = todayStr();
    switch (state.period) {
      case 'day': return state.day >= today;
      case 'week': return addDays(state.week, 6) >= today;
      case 'month': return state.month >= today.slice(0, 7);
      case 'year': return state.year >= +today.slice(0, 4);
      case 'lifetime': return true;
      default: return state.to >= today;
    }
  }

  function shiftPeriod(dir) {
    const today = todayStr();
    switch (state.period) {
      case 'day': return go({ day: addDays(state.day, dir) });
      case 'week': return go({ week: addDays(state.week, 7 * dir) });
      case 'month': return go({ month: shiftMonth(state.month, dir) });
      case 'year': return go({ year: state.year + dir });
      case 'custom': {
        const n = daysBetween(state.from, state.to) + 1;
        if (dir < 0) return go({ from: addDays(state.from, -n), to: addDays(state.from, -1) });
        const to = addDays(state.to, n);
        return go({ from: addDays(state.to, 1), to: to > today ? today : to });
      }
      default: return undefined;
    }
  }

  /** Updates the tabs, the date pill and which chart pane shows. */
  function updateHistoryNav() {
    for (const b of document.querySelectorAll('.ptab')) {
      const on = b.dataset.period === state.period;
      b.setAttribute('aria-selected', String(on));
      b.tabIndex = on ? 0 : -1;
      if (on) {
        // On a phone the tab strip scrolls; keep the current tab in view.
        const strip = b.parentElement;
        if (b.offsetLeft < strip.scrollLeft) strip.scrollLeft = b.offsetLeft;
        else if (b.offsetLeft + b.offsetWidth > strip.scrollLeft + strip.clientWidth) strip.scrollLeft = b.offsetLeft + b.offsetWidth - strip.clientWidth;
      }
    }
    const life = state.period === 'lifetime';
    $('dp-text').textContent = periodLabel();
    $('dp-prev').disabled = life;
    $('dp-next').disabled = atCurrent();
    $('dp-label').disabled = life;
    const curve = curveDate() != null;
    $('view-day').hidden = !curve;
    $('view-bars').hidden = curve;
    $('hist-panel').setAttribute('aria-labelledby', `ptab-${state.period}`);
  }

  // ---------- History: cards ----------

  const fmtKwh = (v) => (v == null ? '–'
    : v.toLocaleString('en-AU', { minimumFractionDigits: v >= 10000 ? 0 : 1, maximumFractionDigits: v >= 10000 ? 0 : 1 }));

  /** The Money hash that matches the period on show. */
  function moneyHashFor() {
    const [from, to] = rangeOf();
    switch (state.period) {
      case 'day': case 'week': case 'month': return `#money=month:${from.slice(0, 7)}`;
      case 'year': return `#money=year:${state.year}`;
      case 'lifetime': return '#money=all';
      default:
        if (from.slice(0, 7) === to.slice(0, 7)) return `#money=month:${from.slice(0, 7)}`;
        return from.slice(0, 4) === to.slice(0, 4) ? `#money=year:${from.slice(0, 4)}` : '#money=all';
    }
  }

  /** Fills the energy cards from `t` (kWh totals; null for none). */
  function renderCards(t) {
    t = t || {};
    const { pv_kwh: pv, load_kwh: load, import_kwh: imp, export_kwh: exp } = t;
    $('hc-pv').textContent = fmtKwh(pv);
    $('hc-load').textContent = fmtKwh(load);
    const used = pv > 0 && exp != null ? Math.max(0, Math.min(100, Math.round(((pv - exp) / pv) * 100))) : null;
    $('hc-pv-sub').textContent = used == null ? '' : `${used}% used at home or stored`;
    $('hc-load-sub').textContent = pv == null || load == null ? ''
      : pv >= load ? `Made ${fmtKwh(pv - load)} kWh more than you used`
        : `Used ${fmtKwh(load - pv)} kWh more than you made`;
    $('hc-grid').textContent = imp == null && exp == null ? '–'
      : `${fmtKwh(imp)} in · ${fmtKwh(exp)} out kWh`;
  }

  function resetCostCard() {
    $('hc-cost').textContent = '–';
    $('hc-cost-sub').textContent = '';
    $('hc-cost-link').setAttribute('href', moneyHashFor());
  }

  /** Fills the Cost card from /api/costs. `current()` says whether the page still shows this range. */
  async function loadCostCard(current) {
    resetCostCard();
    const today = todayStr();
    const [from, to0] = rangeOf();
    const to = to0 > today ? today : to0;
    if (from > to) return;
    let costs;
    try {
      costs = await api(`/api/costs?from=${from}&to=${to}&group=month`);
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
      return; // the energy cards still show; the cost stays a dash
    }
    if (!current() || !costs.configured || !(costs.totals.days > 0)) return;
    $('hc-cost').textContent = money(costs.totals.actual);
    $('hc-cost-sub').textContent = `Saved ${money(costs.totals.savings)} against no solar or battery`;
  }

  // ---------- History: charts ----------

  function sum(days, key) {
    const vals = days.map((d) => d[key]).filter((v) => v != null);
    return vals.length ? vals.reduce((a, b) => a + b, 0) : null;
  }

  function sumTotals(days) {
    const t = {};
    for (const key of ENERGY_KEYS) t[key] = sum(days, key);
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

  const hasEnergy = (rows) => rows.some((d) => ENERGY_KEYS.some((k) => d[k] != null));

  /** Month buckets from /api/daily?group=month, summed into years. */
  function bucketsToYears(buckets) {
    const years = [];
    for (const b of buckets) {
      const y = b.period.slice(0, 4);
      let row = years[years.length - 1];
      if (!row || row.period !== y) { row = { period: y }; years.push(row); }
      for (const k of ENERGY_KEYS) {
        if (b[k] != null) row[k] = (row[k] || 0) + b[k];
      }
    }
    return years;
  }

  function drawBars(rows, labels, title, thin, alt, hint, onClick) {
    renderCards(sumTotals(rows));
    const option = energyOption(labels, rows, title, window.innerWidth < 560, thin);
    showChart('bars-chart', hasEnergy(rows), option,
      `${alt}: stacked bars of PV generation and grid import above the axis, grid export below, with a line for load, in kWh.`);
    $('bars-hint').textContent = hint;
    onBarClick('bars-chart', onClick);
  }

  /** Daily bars for a week, a month or a Custom range of up to CUSTOM_DAILY_MAX days. */
  async function loadDailyBars(from, to, current) {
    const { days } = await api(`/api/daily?from=${from}&to=${to}`);
    if (!current()) return;
    let labels, alt;
    if (state.period === 'week') {
      labels = days.map((d) => `${DOW[dowOf(d.date)]} ${+d.date.slice(8)}`);
      alt = `Daily energy for the week of ${dmy(from)}`;
    } else if (state.period === 'month') {
      labels = days.map((d) => String(+d.date.slice(8)));
      alt = `Daily energy for ${LONG_MONTHS[+state.month.slice(5) - 1]} ${state.month.slice(0, 4)}`;
    } else {
      labels = days.map((d, i) => (i === 0 || d.date.endsWith('-01') ? dayLabel(d.date) : String(+d.date.slice(8))));
      alt = `Daily energy from ${dmy(from)} to ${dmy(to)}`;
    }
    drawBars(days, labels, (i) => days[i].date, true, alt, 'Click a day to open its power curve.',
      (i) => go({ period: 'day', day: days[i].date }));
  }

  /** Year: one bar per month. */
  async function loadYearBars(from, to, current) {
    const { days } = await api(`/api/daily?from=${from}&to=${to}`);
    if (!current()) return;
    const months = MONTHS.map(() => []);
    for (const d of days) months[+d.date.slice(5, 7) - 1].push(d);
    const agg = months.map((ds) => sumTotals(ds));
    drawBars(agg, MONTHS, (i) => `${MONTHS[i]} ${state.year}`, false, `Monthly energy for ${state.year}`,
      'Click a month to open it.', (i) => go({ period: 'month', month: `${state.year}-${pad(i + 1)}` }));
  }

  /** Lifetime (a bar per year) and long Custom ranges (a bar per month), from /api/daily?group=month. */
  async function loadGroupedBars(from, to, yearly, current) {
    const res = yearly ? await loadLifetime() : await api(`/api/daily?from=${from}&to=${to}&group=month`);
    if (!current()) return;
    const rows = yearly ? bucketsToYears(res.buckets) : res.buckets;
    const monthName = (p) => `${MONTHS[+p.slice(5) - 1]} ${p.slice(0, 4)}`;
    drawBars(rows, rows.map((r) => (yearly ? r.period : `${MONTHS[+r.period.slice(5) - 1]} ${r.period.slice(2, 4)}`)),
      (i) => (yearly ? rows[i].period : monthName(rows[i].period)), false,
      `${yearly ? 'Yearly' : 'Monthly'} energy from ${dmy(from)} to ${dmy(to)}`,
      yearly ? 'Click a year to open it.' : 'Click a month to open it.',
      (i) => go(yearly ? { period: 'year', year: +rows[i].period } : { period: 'month', month: rows[i].period }));
    updateHistoryNav(); // Lifetime's label needs the first month
  }

  /** Loads whatever the History period needs: the cards and the curve or bars. */
  async function loadHistory() {
    updateHistoryNav();
    // Lifetime's range moves once its first month is known, so it is keyed by period alone.
    const key = () => (state.period === 'lifetime' ? 'lifetime' : `${state.period}|${rangeOf().join('|')}`);
    const snapshot = key();
    const current = () => state.view === 'home' && key() === snapshot;
    // Lifetime starts at the first month with data, which the cost range needs.
    if (state.period === 'lifetime') await loadLifetime();
    if (!current()) return;
    const [from, to] = rangeOf();
    const date = curveDate();
    const cost = loadCostCard(current);
    if (date) await loadDay(date);
    else if (state.period === 'lifetime') await loadGroupedBars(from, to, true, current);
    else if (state.period === 'year') await loadYearBars(from, to, current);
    else if (state.period === 'custom' && daysBetween(from, to) + 1 > CUSTOM_DAILY_MAX) await loadGroupedBars(from, to, false, current);
    else await loadDailyBars(from, to, current);
    await cost;
  }

  // ---------- History: date picker popover ----------

  const pop = { open: false, month: '', year: 0 };
  const calCache = new Map(); // "2026-10" -> { at, promise } of the month's /api/daily rows

  function loadCalMonth(ym) {
    const hit = calCache.get(ym);
    if (hit && Date.now() - hit.at < 5 * 60 * 1000) return hit.promise;
    const today = todayStr();
    const to = monthEnd(ym) > today ? today : monthEnd(ym);
    const p = to < `${ym}-01` ? Promise.resolve({ days: [] }) : api(`/api/daily?from=${ym}-01&to=${to}`);
    calCache.set(ym, { at: Date.now(), promise: p });
    p.catch(() => calCache.delete(ym));
    return p;
  }

  function popBtn(cls, text, onclick, label) {
    const b = el('button', cls, text);
    b.type = 'button';
    b.onclick = onclick;
    if (label) b.setAttribute('aria-label', label);
    return b;
  }

  function closePop(refocus) {
    if (!pop.open) return;
    pop.open = false;
    $('dp-pop').hidden = true;
    $('dp-label').setAttribute('aria-expanded', 'false');
    if (refocus) $('dp-label').focus();
  }

  function openPop() {
    const today = todayStr();
    pop.open = true;
    pop.month = (state.period === 'week' ? state.week : state.period === 'day' ? state.day : today).slice(0, 7);
    pop.year = state.period === 'month' ? +state.month.slice(0, 4) : +today.slice(0, 4);
    $('dp-label').setAttribute('aria-expanded', 'true');
    $('dp-pop').hidden = false;
    drawPop(true);
  }

  /** Rebuilds the popover for the period; `focus` puts keyboard focus inside it. */
  async function drawPop(focus) {
    const host = $('dp-pop');
    const kind = state.period;
    const wasInside = host.contains(document.activeElement);
    let body;
    try {
      if (kind === 'day' || kind === 'week') body = await calendarBody();
      else if (kind === 'month') body = monthBody();
      else if (kind === 'year') body = await yearBody();
      else body = customBody();
    } catch (err) {
      if (err.message === 'Not logged in.') return;
      throw err;
    }
    if (!pop.open || !body) return;
    host.replaceChildren(...body.nodes);
    if (focus || wasInside) {
      const target = body.focus ? body.focus() : null;
      (target || host.querySelector('button:not([disabled]), input')).focus();
    }
  }

  /** Month calendar, Monday first, each day shaded by solar made. */
  async function calendarBody() {
    const today = todayStr();
    const ym = pop.month;
    let rows = [];
    try { rows = (await loadCalMonth(ym)).days; } catch (err) { if (err.message === 'Not logged in.') throw err; }
    if (!pop.open || pop.month !== ym) return null;
    const pv = {};
    let max = 0;
    for (const d of rows) { if (d.pv_kwh != null) { pv[d.date] = d.pv_kwh; max = Math.max(max, d.pv_kwh); } }
    const head = el('div', 'pop-head');
    const next = popBtn('pop-step', '›', () => { pop.month = shiftMonth(ym, 1); drawPop(); }, 'Next month');
    next.disabled = ym >= today.slice(0, 7);
    head.append(
      popBtn('pop-step', '‹', () => { pop.month = shiftMonth(ym, -1); drawPop(); }, 'Previous month'),
      el('span', 'pop-title', `${LONG_MONTHS[+ym.slice(5) - 1]} ${ym.slice(0, 4)}`),
      next,
    );
    const dows = el('div', 'cal');
    dows.setAttribute('aria-hidden', 'true');
    for (const d of ['Mo', 'Tu', 'We', 'Th', 'Fr', 'Sa', 'Su']) dows.appendChild(el('span', 'cal-dow', d));
    const grid = el('div', 'cal');
    grid.setAttribute('role', 'group');
    grid.setAttribute('aria-label', `${LONG_MONTHS[+ym.slice(5) - 1]} ${ym.slice(0, 4)}, shaded by solar made`);
    for (let i = dowOf(`${ym}-01`); i > 0; i--) grid.appendChild(el('span', 'cal-pad'));
    const wk = state.period === 'week';
    const last = +monthEnd(ym).slice(8);
    const cells = [];
    for (let n = 1; n <= last; n++) {
      const date = `${ym}-${pad(n)}`;
      const b = popBtn('cal-day', String(n), () => {
        closePop(true);
        go(wk ? { period: 'week', week: mondayOf(date) } : { period: 'day', day: date });
      });
      b.disabled = date > today;
      b.setAttribute('aria-label', `${DOW[dowOf(date)]} ${dmy(date)}${pv[date] != null ? `, ${fmtKwh(pv[date])} kWh solar` : ''}`);
      if (pv[date] != null && max > 0) b.style.setProperty('--shade', `${Math.round(8 + (pv[date] / max) * 40)}%`);
      const inWeek = wk && mondayOf(date) === state.week;
      if (inWeek) {
        b.classList.add('in-week');
        if (dowOf(date) === 0 || n === 1) b.classList.add('wk-start');
        if (dowOf(date) === 6 || n === last) b.classList.add('wk-end');
      }
      if (wk ? inWeek : date === state.day) { b.classList.add('sel'); b.setAttribute('aria-current', 'date'); }
      if (date === today) b.classList.add('today');
      cells.push(b);
      grid.appendChild(b);
    }
    const enabled = cells.filter((c) => !c.disabled);
    const initial = enabled.find((c) => c.classList.contains('sel')) || enabled.find((c) => c.classList.contains('today')) || enabled[0] || null;
    for (const c of cells) c.tabIndex = c === initial ? 0 : -1;
    // Arrow keys move between days; only one day is a Tab stop.
    grid.onkeydown = (e) => {
      const step = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7 }[e.key];
      if (step == null) return;
      e.preventDefault();
      const t = cells[cells.indexOf(document.activeElement) + step];
      if (t && !t.disabled) { for (const c of cells) c.tabIndex = -1; t.tabIndex = 0; t.focus(); }
    };
    const foot = el('div', 'pop-foot');
    foot.append(el('span', 'pop-note', 'Shaded by solar made'),
      popBtn('pop-link', wk ? 'This week' : 'Today', () => {
        closePop(true);
        go(wk ? { period: 'week', week: mondayOf(today) } : { period: 'day', day: today });
      }));
    return { nodes: [head, dows, grid, foot], focus: () => initial };
  }

  function monthBody() {
    const today = todayStr();
    const head = el('div', 'pop-head');
    const next = popBtn('pop-step', '›', () => { pop.year += 1; drawPop(); }, 'Next year');
    next.disabled = pop.year >= +today.slice(0, 4);
    head.append(popBtn('pop-step', '‹', () => { pop.year -= 1; drawPop(); }, 'Previous year'),
      el('span', 'pop-title', String(pop.year)), next);
    const grid = el('div', 'chips chips-4');
    let focus = null;
    MONTHS.forEach((m, i) => {
      const ym = `${pop.year}-${pad(i + 1)}`;
      const b = popBtn('chip', m, () => { closePop(true); go({ period: 'month', month: ym }); });
      b.disabled = ym > today.slice(0, 7);
      if (ym === state.month) { b.classList.add('sel'); b.setAttribute('aria-current', 'date'); focus = b; }
      grid.appendChild(b);
    });
    const foot = el('div', 'pop-foot');
    foot.appendChild(popBtn('pop-link', 'This month', () => { closePop(true); go({ period: 'month', month: today.slice(0, 7) }); }));
    return { nodes: [head, grid, foot], focus: () => focus };
  }

  async function yearBody() {
    const thisYear = +todayStr().slice(0, 4);
    let years = [];
    try { years = bucketsToYears((await loadLifetime()).buckets).map((r) => +r.period); } catch (err) { if (err.message === 'Not logged in.') throw err; }
    if (!years.includes(thisYear)) years.push(thisYear);
    if (!years.includes(state.year)) years.push(state.year);
    years.sort((a, b) => b - a);
    const grid = el('div', 'chips chips-4');
    let focus = null;
    for (const y of years) {
      const b = popBtn('chip', String(y), () => { closePop(true); go({ period: 'year', year: y }); });
      if (y === state.year) { b.classList.add('sel'); b.setAttribute('aria-current', 'date'); focus = b; }
      grid.appendChild(b);
    }
    const foot = el('div', 'pop-foot');
    foot.appendChild(popBtn('pop-link', 'This year', () => { closePop(true); go({ period: 'year', year: thisYear }); }));
    return { nodes: [el('div', 'pop-note', 'Years with data'), grid, foot], focus: () => focus };
  }

  function customBody() {
    const today = todayStr();
    const quarterStart = `${today.slice(0, 4)}-${pad(Math.floor((+today.slice(5, 7) - 1) / 3) * 3 + 1)}-01`;
    const presets = [
      ['Last 7 days', addDays(today, -6)],
      ['Last 30 days', addDays(today, -29)],
      ['This quarter', quarterStart],
      ['Last 12 months', `${shiftMonth(today.slice(0, 7), -11)}-01`],
    ];
    const chips = el('div', 'chips');
    for (const [label, from] of presets) {
      const b = popBtn('chip', label, () => { closePop(true); go({ period: 'custom', from, to: today }); });
      if (from === state.from && today === state.to) { b.classList.add('sel'); b.setAttribute('aria-current', 'true'); }
      chips.appendChild(b);
    }
    const field = (label, value) => {
      const l = el('label', 'pop-field', label);
      const i = document.createElement('input');
      i.type = 'date';
      i.value = value;
      i.max = today;
      l.appendChild(i);
      return [l, i];
    };
    const [fromL, fromI] = field('From', state.from);
    const [toL, toI] = field('To', state.to);
    const fields = el('div', 'pop-fields');
    fields.append(fromL, toL);
    const err = el('div', 'pop-error');
    err.setAttribute('role', 'alert');
    const apply = popBtn('btn pop-apply', 'Apply', () => {
      const f = fromI.value, t = toI.value;
      let msg = '';
      if (!validDate(f) || !validDate(t)) msg = 'Pick both dates.';
      else if (t < f) msg = 'The end date is before the start.';
      else if (daysBetween(f, t) + 1 > 366 * 20) msg = 'Pick at most 20 years.';
      err.textContent = msg;
      if (msg) return;
      closePop(true);
      go({ period: 'custom', from: f, to: t > today ? today : t });
    });
    const hint = el('div', 'pop-note', 'One day shows its power curve, up to 92 days shows days, longer shows months.');
    return { nodes: [chips, fields, err, apply, hint], focus: () => chips.firstChild };
  }

  function initHistory() {
    const tabs = [...document.querySelectorAll('.ptab')];
    for (const b of tabs) {
      b.onclick = () => switchPeriod(b.dataset.period);
      b.onkeydown = (e) => {
        const i = tabs.indexOf(b);
        const t = e.key === 'ArrowRight' ? tabs[(i + 1) % tabs.length] : e.key === 'ArrowLeft' ? tabs[(i + tabs.length - 1) % tabs.length]
          : e.key === 'Home' ? tabs[0] : e.key === 'End' ? tabs[tabs.length - 1] : null;
        if (!t) return;
        e.preventDefault();
        t.focus();
        switchPeriod(t.dataset.period);
      };
    }
    $('dp-prev').onclick = () => shiftPeriod(-1);
    $('dp-next').onclick = () => shiftPeriod(1);
    $('dp-label').onclick = () => (pop.open ? closePop(false) : openPop());
    const wrap = document.querySelector('.dp');
    document.addEventListener('pointerdown', (e) => { if (pop.open && !wrap.contains(e.target)) closePop(false); });
    document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && pop.open) { e.preventDefault(); closePop(true); } });
    wrap.addEventListener('focusout', (e) => { if (pop.open && e.relatedTarget && !wrap.contains(e.relatedTarget)) closePop(false); });
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

  /** "21 Oct" for a YYYY-MM-DD string. */
  const dayLabel = (d) => `${+d.slice(8)} ${MONTHS[+d.slice(5, 7) - 1]}`;

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
        name: 'Actual', type: 'line', data: actual, showSymbol: false, smooth: 0.3,
        lineStyle: { width: 3, color: css('--save') }, itemStyle: { color: css('--save') }, z: 4,
        markLine: {
          silent: true, symbol: 'none', label: { show: false },
          lineStyle: { color: css('--muted'), width: 1.5, type: 'solid' }, data: [{ yAxis: 0 }],
        },
      },
    ];
    if (projected) {
      series.push({
        name: 'Projected', type: 'line', data: projected, showSymbol: false, smooth: 0.3,
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

  // ---------- Money ----------

  const MONEY_MODES = ['month', 'year', 'all'];
  const NOT_CONFIGURED_COST = 'Add a [tariff] section to the config file to see what your power costs.';
  const NOT_CONFIGURED_PETROL =
    'Set petrol_cents_per_litre in the [teslamate] section of the config file to compare the car with petrol.';

  /** "September 2026" for "2026-09". */
  const longMonth = (ym) => new Date(Date.UTC(+ym.slice(0, 4), +ym.slice(5, 7) - 1, 1))
    .toLocaleString('en-AU', { month: 'long', year: 'numeric', timeZone: 'UTC' });

  /** "1,234.5". */
  const kwhText = (v) => (v == null ? '–' : v.toLocaleString('en-AU', { maximumFractionDigits: 1 }));

  /** "1,234 km", or a dash when there is no figure. */
  const kmText = (km) => (km == null ? '–' : `${Math.round(km).toLocaleString('en-AU')} km`);

  /**
   * What the Money tab shows for a mode ("month", "year" or "all") and a month
   * ("2026-09") or year ("2025"): the date range, how /api/costs groups it, the
   * label above the chart and the words used in sentences ("this month").
   */
  function moneyPeriod(mode, when) {
    const today = todayStr();
    if (mode === 'month') {
      const last = new Date(Date.UTC(+when.slice(0, 4), +when.slice(5, 7), 0)).getUTCDate();
      return {
        from: `${when}-01`, to: `${when}-${pad(last)}`, group: 'day',
        label: longMonth(when), words: when === today.slice(0, 7) ? 'this month' : longMonth(when),
        chartTitle: 'Day by day', isNow: when >= today.slice(0, 7),
      };
    }
    if (mode === 'year') {
      return {
        from: `${when}-01-01`, to: `${when}-12-31`, group: 'month',
        label: when, words: when === today.slice(0, 4) ? 'this year' : when,
        chartTitle: 'Month by month', isNow: when >= today.slice(0, 4),
      };
    }
    // /api/costs takes at most 20 years of months or years.
    return {
      from: `${+today.slice(0, 4) - 19}-01-01`, to: today, group: 'year',
      label: 'All time', words: 'all time', chartTitle: 'Year by year', isNow: true,
    };
  }

  /** A row of a bill: name and detail on the left, an amount on the right. `cls` colours the amount. */
  function moneyLine(name, detail, amount, cls, swatch) {
    const row = el('div', 'line');
    const left = el('div', 'line-name');
    if (swatch) {
      const s = el('span', 'swatch');
      s.style.background = `var(${swatch})`;
      left.appendChild(s);
    }
    const text = el('div');
    text.append(el('div', '', name), el('div', 'line-detail', detail));
    left.appendChild(text);
    row.append(left, el('div', `num ${cls || ''}`.trim(), amount));
    return row;
  }

  /** A thin bar cut into parts: `[share, colourVar]` pairs, widths in proportion to the shares. */
  function splitBar(parts) {
    const bar = el('div', 'split');
    bar.setAttribute('aria-hidden', 'true');
    const sum = parts.reduce((a, [v]) => a + v, 0);
    for (const [v, colour] of parts) {
      if (!(v > 0)) continue;
      const s = el('span');
      s.style.width = `${(v / sum) * 100}%`;
      s.style.background = `var(${colour})`;
      bar.appendChild(s);
    }
    return bar;
  }

  /** The big "Saved" panel at the top. `t` is /api/costs' `totals`. */
  function renderMoneyHero(t, words) {
    const pct = t.baseline > 0 ? Math.round((t.savings / t.baseline) * 100) : null;
    $('money-saved').textContent = money(t.savings, 0);
    $('money-pct').textContent = pct == null ? '' : `${pct}% of what ${words === 'all time' ? 'your power' : words} would have cost`;
    $('money-paid').textContent = money(t.actual);
    $('money-without').textContent = money(t.baseline);
    // Paid is coral, the rest of the without-solar cost is the saving.
    const paid = Math.max(0, Math.min(t.actual, t.baseline));
    $('money-split').replaceChildren(...(t.baseline > 0 ? [splitBar([[paid, '--cost'], [t.baseline - paid, '--save']])] : []));
    // Days GloBird has billed use its figures; the rest are our estimate.
    const note = $('money-billed');
    note.hidden = !(t.billed_days > 0);
    note.textContent = t.billed_days === t.days
      ? `All ${daysText(t.days)} from your GloBird bill`
      : `${t.billed_days} of ${daysText(t.days)} from your GloBird bill, the rest estimated`;
  }

  /** "Your bill": laid out like the power bill, so the total can be checked against it. */
  function renderMoneyBill(t) {
    const rows = [
      moneyLine('Supply charge', `${t.days} ${t.days === 1 ? 'day' : 'days'} at ${money(t.supply / t.days, 3)} a day`, money(t.supply)),
      moneyLine('Power from the grid', `${kwhText(t.import_kwh)} kWh`, money(t.import)),
      moneyLine('Feed-in', `${kwhText(t.export_kwh)} kWh sent`, money(-t.feed_in), 'save'),
    ];
    // Plans without credits would only ever show an empty row.
    if (t.credit_days > 0) {
      rows.push(moneyLine('ZeroHero credits', `${t.credit_days} of ${t.days} days`, money(-t.credit), 'save'));
    }
    const total = moneyLine('Total', '', money(t.actual));
    total.classList.add('total');
    $('money-bill').replaceChildren(...rows, total);
  }

  /** "Where the saving came from": the three parts of `savings`, as a bar and rows. */
  function renderMoneySources(t) {
    const parts = [
      ['Power you didn\'t have to buy', t.not_bought, '--save'],
      ['ZeroHero credits', t.credit, '--c-grid'],
      ['Feed-in', t.feed_in, '--c-pv'],
    ];
    const sum = parts.reduce((a, p) => a + Math.max(0, p[1]), 0);
    const rows = parts.map(([name, v, colour]) =>
      moneyLine(name, sum > 0 && v > 0 ? `${Math.round((v / sum) * 100)}% of the saving` : '', money(v), '', colour));
    $('money-sources').replaceChildren(...(sum > 0 ? [splitBar(parts.map(([, v, c]) => [Math.max(0, v), c]))] : []), ...rows);
  }

  /** Stacked bars: what you paid at the bottom, the rest of the without-solar cost (the saving) on top. */
  function moneyOption(rows) {
    const th = baseTheme();
    return {
      animation: false,
      textStyle: { fontFamily: css('--font') },
      grid: { left: 8, right: 16, top: 48, bottom: 8, containLabel: true },
      legend: { top: 0, left: 8, textStyle: { color: th.fg }, itemWidth: 12, itemHeight: 10 },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle(),
        formatter: (params) => {
          const r = rows[params[0].dataIndex];
          const lines = [
            [params[0].marker, 'You paid', money(r.actual)],
            ['', 'Without solar or battery', money(r.baseline)],
            [params[1].marker, 'Saved', money(r.savings)],
          ];
          const note = r.actual < 0 ? '<div style="margin-top:4px;color:var(--muted)">Credits were more than the bill, so the paid bar shows $0.</div>' : '';
          return moneyTooltip(lines, r.title) + note;
        },
      },
      xAxis: {
        type: 'category', data: rows.map((r) => r.label),
        axisLabel: { color: th.muted, fontFamily: css('--mono'), hideOverlap: true },
        axisLine: { lineStyle: { color: th.line } },
      },
      yAxis: {
        type: 'value', name: 'Dollars ($)', nameTextStyle: { color: th.muted, align: 'left' },
        axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => `$${v.toLocaleString('en-AU')}` },
        splitLine: { lineStyle: { color: th.line } },
      },
      series: [
        { name: 'You paid', type: 'bar', stack: 'cost', data: rows.map((r) => +Math.max(0, r.actual).toFixed(2)), itemStyle: { color: css('--cost') }, barMaxWidth: 28 },
        { name: 'Saved', type: 'bar', stack: 'cost', data: rows.map((r) => +Math.max(0, r.savings).toFixed(2)), itemStyle: { color: css('--save-bar') }, barMaxWidth: 28 },
      ],
    };
  }

  /** Runs a request whose failure should only hide its panel (a login problem still stops everything). */
  const quiet = (promise) => promise.catch((err) => {
    if (err.message === 'Not logged in.') throw err;
    return null;
  });

  /**
   * The car panel and, for a month, the charging sessions. /api/ev/summary takes
   * at most 400 days, so All time shows the last 12 months of the car.
   */
  async function loadMoneyCar(mode, when, p) {
    const today = todayStr();
    const from = mode === 'all' ? addDays(today, -364) : p.from;
    const to = mode === 'all' ? today : p.to;
    const [summary, sessionData, live] = await Promise.all([
      quiet(api(`/api/ev/summary?from=${from}&to=${to}`)),
      quiet(api(`/api/ev/sessions?from=${Math.floor(adelaideMs(from, 0) / 1000)}&to=${Math.floor(adelaideMs(to, 24) / 1000)}`)),
      quiet(api('/api/live')),
    ]);
    if (state.view !== 'money' || state.mode !== mode || state.when !== when) return;
    const sessions = (sessionData && sessionData.sessions) || [];
    renderMoneyCar(summary, sessions, (live && live.car && live.car.display_name) || 'Car',
      mode === 'all' ? 'in the last 12 months' : p.words);
    renderMoneySessions(mode === 'month' ? sessions : []);
  }

  function renderMoneyCar(summary, sessions, name, words) {
    const t = summary && summary.total;
    const hasData = t && (t.km > 0 || t.home_kwh > 0 || t.away_kwh > 0);
    $('money-car').hidden = !hasData;
    if (!hasData) return;
    $('money-car-title').textContent = `${name} ${words}`;
    // Solar share of the home sessions that were costed, from the session list.
    const costed = sessions.filter((c) => c.solar_kwh != null);
    const total = costed.reduce((a, c) => a + c.solar_kwh + c.battery_kwh + c.grid_kwh, 0);
    const solar = total > 0 ? `${Math.round((costed.reduce((a, c) => a + c.solar_kwh, 0) / total) * 100)}%` : '–';
    const cards = [
      kpi('', 'Charged at home', kwhText(t.home_kwh), 'kWh'),
      kpi('', 'From solar', solar, '', 'of the home charging'),
    ];
    let note = '';
    if (summary.configured) {
      cards.splice(1, 0, kpi('', 'Cost to charge', money(t.charging_cost)));
      cards.push(
        kpi('', 'Same km in petrol', money(t.petrol_cost), '', kmText(t.km)),
        kpi('save', 'Saved vs petrol', money(t.saved)),
      );
      note = `Compared with a petrol car using ${summary.petrol_litres_per_100km} L/100km at ${summary.petrol_cents_per_litre} c/L.`;
      if (t.away_kwh > 0 && t.away_cost == null) {
        note += ` ${Math.round(t.away_kwh)} kWh charged away from home is not counted: set away_cents_per_kwh to include it.`;
      }
    } else {
      note = NOT_CONFIGURED_PETROL;
    }
    $('money-car-cards').replaceChildren(...cards);
    $('money-car-note').textContent = note;
  }

  /** Fills the "Charging sessions" table with the month's last 10 sessions; the panel is hidden when there are none. */
  function renderMoneySessions(sessions) {
    $('money-sessions').hidden = sessions.length === 0;
    fillTable('money-sessions-table', sessions.slice(-10).reverse().map((c) => {
      const used = c.energy_used_kwh == null ? c.energy_added_kwh : c.energy_used_kwh;
      const solar = solarPct(c);
      const [, month, day] = fmtDate.format(new Date(c.started_at * 1000)).split('-');
      return [
        [`${+day} ${MONTHS[+month - 1]}` + (c.ended_at == null ? ' (now)' : '')],
        [kwh(used)], [solar == null ? '–' : `${solar}%`], [money(c.cost)],
      ];
    }));
  }

  // ---------- Money: Bill check ----------

  // /api/globird/compare takes at most 400 days.
  const BILL_MAX_DAYS = 400;
  // A day or a period counts as different when it is out by more than this share of GloBird's bill...
  const BILL_SHARE = 0.05;
  // ...and by more than this many dollars (a day) or $1 (the whole period).
  const BILL_DAY_DOLLARS = 0.5;
  const BILL_PERIOD_DOLLARS = 1;
  const BILL_NO_ESTIMATE = 'GloBird has bills for this period, but no complete day has an estimate of ours to compare with yet.';
  // The "Days that differ" list shows this many days until "Show all" is pressed.
  const BILL_DAYS_SHORT = 5;
  const BILL_DAYS_MAX = 10;

  /** The money columns of the breakdown: label, key in `globird`/`ours`. Our estimate has no "other". */
  const BILL_ROWS = [
    ['Import', 'Import', 'import'],
    ['Feed-in and export credits', 'Export credits', 'export'],
    ['Supply', 'Supply', 'supply'],
    ['ZeroHero credit', 'ZeroHero', 'credit'],
    ['Other', 'Other', 'other'],
  ];

  /** "+$1.20" or "-$1.20". */
  const signedMoney = (v) => (v > 0 ? '+' : '') + money(v);

  /** The plural-aware "4 days". */
  const daysText = (n) => `${n} ${n === 1 ? 'day' : 'days'}`;

  /** From and to for the Bill check: the Money period, cut to the last 400 days. */
  function billRange(p) {
    return { from: p.from > addDays(p.to, 1 - BILL_MAX_DAYS) ? p.from : addDays(p.to, 1 - BILL_MAX_DAYS), to: p.to };
  }

  /** "GloBird billed $X · we estimated $Y · difference $Z (P%)", with the difference amber when it is large. */
  function renderBillHeadline(t) {
    const diff = t.diff.total;
    const base = Math.abs(t.globird.total);
    const small = Math.abs(diff) <= BILL_PERIOD_DOLLARS || Math.abs(diff) <= base * BILL_SHARE;
    const pct = base > 0 ? ` (${diff > 0 ? '+' : ''}${((diff / base) * 100).toFixed(1)}%)` : '';
    const d = el('span', small ? 'diff' : 'diff warn', `difference ${signedMoney(diff)}${pct}`);
    $('bill-headline').replaceChildren(
      `GloBird billed ${money(t.globird.total)} · we estimated ${money(t.ours.total)} · `, d,
      ` for ${daysText(t.days_compared)}`);
  }

  /** A table row: a label with a long and a short form (the short one shows on phones), then the numbers. */
  function billRow(long, short, cells) {
    const tr = document.createElement('tr');
    const name = el('td');
    name.append(el('span', 'long', long), el('span', 'short', short));
    tr.append(name, ...cells.map((text) => el('td', '', text)));
    return tr;
  }

  /** The breakdown table: money rows, then kWh rows. `t` is the compare `totals`. */
  function renderBillTable(t) {
    const rows = BILL_ROWS.map(([long, short, key]) => {
      const g = t.globird[key];
      const o = t.ours[key]; // undefined for "other"
      return billRow(long, short, [money(g), o == null ? '–' : money(o), money((o || 0) - g)]);
    });
    rows.push(
      billRow('Grid import (kWh)', 'Import kWh', [kwh(t.globird.import_kwh), kwh(t.ours.import_kwh), kwh(t.diff.import_kwh)]),
      billRow('Export (kWh)', 'Export kWh', [kwh(t.globird.export_kwh), kwh(t.ours.export_kwh), kwh(t.diff.export_kwh)]),
    );
    $('bill-table').tBodies[0].replaceChildren(...rows);
  }

  /** Bars of ours minus GloBird per day. A day with no estimate gets no bar. */
  function billOption(days) {
    const th = baseTheme();
    return {
      animation: false,
      textStyle: { fontFamily: css('--font') },
      grid: { left: 8, right: 16, top: 40, bottom: 8, containLabel: true },
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle(),
        formatter: (params) => {
          const d = days[params[0].dataIndex];
          return moneyTooltip([
            ['', 'GloBird billed', money(d.globird.total)],
            ['', 'We estimated', d.ours ? money(d.ours.total) : '–'],
            [params[0].marker, 'Difference', d.diff ? signedMoney(d.diff.total) : '–'],
          ], dayLabel(d.date));
        },
      },
      xAxis: {
        type: 'category', data: days.map((d) => dayLabel(d.date)),
        axisLabel: { color: th.muted, fontFamily: css('--mono'), hideOverlap: true },
        axisLine: { lineStyle: { color: th.line } },
      },
      yAxis: {
        type: 'value', name: 'Ours minus GloBird ($)', nameTextStyle: { color: th.muted, align: 'left' },
        axisLabel: { color: th.muted, fontFamily: css('--mono'), formatter: (v) => `$${v.toLocaleString('en-AU')}` },
        splitLine: { lineStyle: { color: th.line } },
      },
      series: [{
        name: 'Difference', type: 'bar', barMaxWidth: 28, itemStyle: { color: css('--c-load') },
        data: days.map((d) => (d.diff && d.complete ? d.diff.total : null)),
      }],
    };
  }

  /** The bucket where our estimate is furthest from GloBird's on a day, as "Import +$1.20". */
  function billBiggest(d) {
    let best = null;
    for (const [name, , key] of BILL_ROWS) {
      if (key === 'other') continue;
      const diff = d.ours[key] - d.globird[key];
      if (best == null || Math.abs(diff) > Math.abs(best[1])) best = [name, diff];
    }
    return `${best[0]} ${signedMoney(best[1])}`;
  }

  let billExpanded = false;

  /**
   * The days where the two totals are far apart: the 5 furthest apart (up to 10
   * after "Show all"), newest first. A day GloBird has only posted the supply
   * charge for is skipped: it would always look wrong.
   */
  function renderBillDays(days) {
    const differ = days.filter((d) => d.diff && d.complete
      && Math.abs(d.diff.total) > Math.max(BILL_DAY_DOLLARS, BILL_SHARE * Math.abs(d.globird.total)));
    const top = differ.sort((a, b) => Math.abs(b.diff.total) - Math.abs(a.diff.total)).slice(0, BILL_DAYS_MAX);
    const shown = top.slice(0, billExpanded ? BILL_DAYS_MAX : BILL_DAYS_SHORT)
      .sort((a, b) => (a.date < b.date ? 1 : -1));
    const items = shown.map((d) => {
      const li = el('li');
      li.append(
        el('b', '', dayLabel(d.date)),
        el('span', 'line-detail',
          `GloBird ${money(d.globird.total)}, ours ${money(d.ours.total)}, difference ${signedMoney(d.diff.total)}`),
        el('span', 'line-detail', `Biggest: ${billBiggest(d)}`));
      return li;
    });
    if (!items.length) items.push(el('li', '', 'None'));
    $('bill-days').replaceChildren(...items);
    const more = $('bill-more');
    more.hidden = top.length <= BILL_DAYS_SHORT;
    more.textContent = billExpanded ? 'Show fewer' : `Show all ${top.length}`;
    more.setAttribute('aria-expanded', String(billExpanded));
    more.onclick = () => { billExpanded = !billExpanded; renderBillDays(days); };
  }

  /** Categories our tariff has no place for, with what GloBird charged under them. */
  function renderBillNotes(categories) {
    const odd = categories.filter((c) => c.bucket === 'other' || /critical peak/i.test(c.category));
    $('bill-notes').hidden = odd.length === 0;
    $('bill-notes').replaceChildren(...odd.map((c) =>
      el('div', '', `${c.category}: ${money(c.amount)} over ${daysText(c.days)}. Our tariff doesn't model this.`)));
  }

  /** The Bill check card. It stays hidden when there is no [globird] section (and no data). */
  async function loadBillCheck(mode, when, p) {
    const { from, to } = billRange(p);
    const data = await quiet(api(`/api/globird/compare?from=${from}&to=${to}`));
    if (state.view !== 'money' || state.mode !== mode || state.when !== when) return;
    const hasDays = !!data && data.days.length > 0;
    $('bill-check').hidden = !data || (!data.configured && !hasDays);
    if ($('bill-check').hidden) return;
    const compared = hasDays && data.totals.days_compared > 0;
    $('bill-nodata').hidden = compared;
    $('bill-nodata').textContent = hasDays
      ? BILL_NO_ESTIMATE : 'No GloBird data yet. The first sync runs a minute after start-up.';
    $('bill-body').hidden = !compared;
    if (!compared) return;

    renderBillHeadline(data.totals);
    renderBillTable(data.totals);
    renderBillDays(data.days);
    renderBillNotes(data.categories);
    const charted = data.days.filter((d) => d.diff && d.complete);
    showChart('bill-chart', charted.length > 0, billOption(data.days),
      `Difference each day: bars of our estimate minus what GloBird billed, in dollars, for ${daysText(data.days.length)}. `
      + `${daysText(charted.length)} have an estimate and a complete bill. Select a bar to open that day.`);
    onBarClick('bill-chart', (i) => go({ view: 'home', period: 'day', day: data.days[i].date }));
  }

  // ---------- Money: Battery and evening credit ----------

  const BATTERY_DAYS = 30;
  // The "Missed days" table shows this many days until "Show all" is pressed.
  const BATTERY_ROWS_SHORT = 5;
  const BATTERY_REASONS = {
    car_charging: 'Car charging',
    battery_low: 'Battery ran low',
    high_load: 'High load',
    small_import: 'Small import',
    no_data: 'No data',
  };
  const batteryReason = (d) => BATTERY_REASONS[d.reason] || '–';
  const percent = (v) => (v == null ? '–' : `${Math.round(v)}%`);

  /** "Tue 21 Oct" for a YYYY-MM-DD date. */
  function longDayLabel(d) {
    const wd = new Date(`${d}T12:00:00Z`).toLocaleDateString('en-AU', { weekday: 'short', timeZone: 'UTC' });
    return `${wd} ${dayLabel(d)}`;
  }

  /** What one day says, for a dot's tooltip and the line under the strip. */
  function batteryDayText(d) {
    const source = d.credit_source === 'globird' ? "from GloBird's bill" : 'our estimate';
    const head = `${longDayLabel(d.date)}: ${d.credit === 'unknown' ? 'credit not known yet' : `credit ${d.credit} (${source})`}`;
    if (d.credit !== 'missed') return head;
    return `${head}. ${batteryReason(d)}, imported ${kwh(d.window_import_kwh)} kWh.`;
  }

  /** Three small averages: exported in the evening, battery at 11:00, bought before 11:00. */
  function renderBatteryStats(s) {
    const stat = (label, value, unit, note) => kpi('', label, value, unit, note);
    $('battery-stats').replaceChildren(
      stat('Exported 18:00 to 21:00', kwh(s.avg_window_export_kwh), 'kWh a day',
        s.avg_window_export_cents == null ? '' : `worth ${money(s.avg_window_export_cents / 100)} a day`),
      stat('Battery at 11:00', percent(s.avg_soc_at_11), 'average',
        s.battery_floor_pct == null ? '' : `lowest seen ${percent(s.battery_floor_pct)}`),
      stat('Bought before 11:00', money((s.avg_morning_import_cents || 0) / 100), 'a day',
        s.avg_morning_import_kwh == null ? '' : `${kwh(s.avg_morning_import_kwh)} kWh a day`));
  }

  /** One dot per day, oldest first. Pressing one shows its details underneath. */
  function renderBatteryStrip(days) {
    const detail = $('battery-dot-detail');
    detail.textContent = '';
    const buttons = [...days].reverse().map((d) => {
      const b = el('button');
      b.type = 'button';
      b.title = batteryDayText(d);
      b.setAttribute('aria-label', batteryDayText(d));
      b.append(el('i', `cdot ${d.credit}`));
      b.onclick = () => {
        for (const other of $('battery-strip').children) other.classList.toggle('selected', other === b);
        detail.textContent = batteryDayText(d);
      };
      return b;
    });
    $('battery-strip').replaceChildren(...buttons);
  }

  let batteryExpanded = false;

  /** The missed days, newest first: 5 until "Show all" is pressed. */
  function renderBatteryMissed(days) {
    const missed = days.filter((d) => d.credit === 'missed');
    $('battery-missed-days').hidden = missed.length === 0;
    const shown = batteryExpanded ? missed : missed.slice(0, BATTERY_ROWS_SHORT);
    $('battery-table').tBodies[0].replaceChildren(...shown.map((d) => {
      const tr = document.createElement('tr');
      tr.append(el('td', '', dayLabel(d.date)), el('td', '', batteryReason(d)),
        el('td', '', d.window_import_kwh == null ? '–' : `${d.window_import_kwh.toFixed(2)} kWh`),
        el('td', '', percent(d.soc_at_21)));
      return tr;
    }));
    const more = $('battery-more');
    more.hidden = missed.length <= BATTERY_ROWS_SHORT;
    more.textContent = batteryExpanded ? 'Show fewer' : `Show all ${missed.length}`;
    more.setAttribute('aria-expanded', String(batteryExpanded));
    more.onclick = () => { batteryExpanded = !batteryExpanded; renderBatteryMissed(days); };
  }

  /** The Battery and evening credit card: the last 30 days, whatever period Money shows. */
  async function loadBatteryCard(mode, when) {
    const data = await quiet(api(`/api/battery?days=${BATTERY_DAYS}`));
    if (state.view !== 'money' || state.mode !== mode || state.when !== when) return;
    $('battery-card').hidden = !data || !data.configured;
    if ($('battery-card').hidden) return;
    const s = data.summary;
    const known = s.earned + s.missed;
    $('battery-nodata').hidden = known > 0;
    $('battery-body').hidden = known === 0;
    if (known === 0) return;

    $('battery-headline').textContent = `Credit earned ${s.earned} of ${known} days`;
    const missed = $('battery-missed');
    missed.className = s.missed ? 'battery-missed' : 'battery-missed none';
    missed.textContent = s.missed ? `${money(s.missed_dollars, s.missed_dollars % 1 ? 2 : 0)} missed` : 'None missed';
    $('battery-count').textContent = String(data.days.length);
    renderBatteryStats(s);
    renderBatteryStrip(data.days);
    renderBatteryMissed(data.days);
  }

  async function loadMoneyView(mode, when) {
    for (const b of document.querySelectorAll('[data-money]')) {
      b.setAttribute('aria-pressed', String(b.dataset.money === mode));
    }
    const p = moneyPeriod(mode, when);
    $('money-label').textContent = p.label;
    $('money-nav').hidden = mode === 'all';
    const unit = mode === 'month' ? 'month' : 'year';
    $('money-prev').setAttribute('aria-label', `Previous ${unit}`);
    $('money-next').setAttribute('aria-label', `Next ${unit}`);
    $('money-next').disabled = p.isNow;
    $('money-today').textContent = `This ${unit}`;
    $('money-today').disabled = p.isNow;

    const url = (group) => `/api/costs?from=${p.from}&to=${p.to}&group=${group}`;
    const [data, months] = await Promise.all([api(url(p.group)), mode === 'all' ? api(url('month')) : null]);
    if (state.view !== 'money' || state.mode !== mode || state.when !== when) return;
    setConfigured('money', data.configured, NOT_CONFIGURED_COST);
    if (!data.configured) return;
    if (mode === 'all' && months.buckets.length) $('money-label').textContent = `Since ${longMonth(months.buckets[0].period)}`;

    const t = data.totals;
    const hasData = t.days > 0;
    $('money-numbers').hidden = !hasData;
    if (hasData) {
      renderMoneyHero(t, p.words);
      renderMoneyBill(t);
      renderMoneySources(t);
    }
    // One row per bar: a day, a month or a year.
    const rows = (data.days || data.buckets).map((r) => {
      const key = r.date || r.period;
      const title = mode === 'month' ? dayLabel(key) : mode === 'year' ? longMonth(key) : key;
      const label = mode === 'month' ? String(+key.slice(8)) : mode === 'year' ? MONTHS[+key.slice(5) - 1] : key;
      return { ...r, key, title, label };
    });
    $('money-chart-title').textContent = p.chartTitle;
    showChart('money-chart', rows.length > 0, moneyOption(rows),
      `${p.chartTitle}: stacked bars of what you paid and what you saved, in dollars. Select a bar to look closer.`);
    onBarClick('money-chart', (i) => {
      const key = rows[i].key;
      if (mode === 'month') go({ view: 'home', period: 'day', day: key });
      else go({ mode: mode === 'year' ? 'month' : 'year', when: key });
    });
    await loadMoneyCar(mode, when, p);
    await loadBillCheck(mode, when, p);
    await loadBatteryCard(mode, when);
  }

  // ---------- routing (hash) ----------

  // Home is the Now content with History below it; Day, Month and Year are History's periods.
  const VIEWS = ['home', 'money', 'payback'];
  const PERIODS = ['day', 'week', 'month', 'year', 'lifetime', 'custom'];
  let period = 'day'; // the period History opens on: the last one shown
  // Money's `mode` is month, year or all; `when` is its month (2026-09) or year (2025).
  const state = { view: 'home', period: 'day', day: '', week: '', month: '', year: 0, from: '', to: '', mode: 'month', when: '', smooth: true };
  const SMOOTH_KEY = 'solar-dash.smooth';
  let timer = null;
  let pollToken = 0;

  let seenToday = todayStr();

  /**
   * After midnight, moves a Day view that was on the old today to the new one.
   * A view on any other date stays where it is.
   */
  function checkMidnight() {
    const today = todayStr();
    if (today === seenToday) return;
    const wasToday = seenToday;
    seenToday = today;
    if (state.view !== 'home') return;
    if (state.period === 'day' && state.day === wasToday) go({ day: today });
    else updateHistoryNav();
  }

  /** Loads the Now part of home, then schedules the next refresh (15 s from the inverter, 60 s from the cloud). */
  async function pollNow() {
    clearTimeout(timer);
    timer = null;
    // A hidden tab does not poll; visibilitychange restarts it.
    if (document.hidden) return;
    checkMidnight();
    const token = ++pollToken;
    let delay = REFRESH_MS;
    await guarded(async () => { delay = await loadNow(); });
    if (token === pollToken && state.view === 'home') timer = setTimeout(pollNow, delay);
  }

  function parseHash() {
    let raw = window.location.hash.replace(/^#/, '');
    try { raw = decodeURIComponent(raw); } catch (e) { raw = ''; } // malformed escapes: fall back to home
    let [view, value] = raw.split('=');
    // Home is the empty hash, the old #now and #history, and the period hashes.
    if (view === '' || view === 'now' || view === 'history') view = 'home';
    // The old Savings and Cost tabs are now Money. #savings=all etc. keep their range.
    const oldTab = view === 'savings' || view === 'cost';
    if (oldTab) view = 'money';
    if (PERIODS.includes(view)) period = view;
    const periodView = PERIODS.includes(view);
    const today = todayStr();
    state.view = periodView || !VIEWS.includes(view) ? 'home' : view;
    state.period = period;
    state.day = validDate(value) && view === 'day' ? value : today;
    state.month = /^\d{4}-\d{2}$/.test(value || '') && view === 'month' ? value : today.slice(0, 7);
    state.year = /^\d{4}$/.test(value || '') && view === 'year' ? +value : +today.slice(0, 4);
    state.week = validDate(value) && view === 'week' ? mondayOf(value) : mondayOf(today);
    // Custom: "custom=2026-09-01..2026-10-22"; anything else is the last 30 days.
    const [cFrom, cTo] = view === 'custom' ? (value || '').split('..') : [];
    const okRange = validDate(cFrom) && validDate(cTo) && cFrom <= cTo;
    state.from = okRange ? cFrom : addDays(today, -29);
    state.to = okRange ? cTo : today;
    // Money: "money=month:2026-09", "money=year:2025" or "money=all"; "money" is this month.
    const [mode, when = ''] = (view === 'money' ? value || '' : '').split(':');
    state.mode = MONEY_MODES.includes(mode) ? mode : 'month';
    const wanted = state.mode === 'month' ? /^\d{4}-\d{2}$/ : /^\d{4}$/;
    state.when = state.mode === 'all' ? '' : wanted.test(when) ? when : today.slice(0, state.mode === 'month' ? 7 : 4);
    if (oldTab) window.history.replaceState(null, '', '#' + viewHash('money'));
  }

  /** The hash for a view, carrying that view's current date or range. */
  function viewHash(v) {
    if (v === 'home') v = state.period;
    return v === 'day' ? `day=${state.day}` : v === 'month' ? `month=${state.month}`
      : v === 'week' ? `week=${state.week}` : v === 'custom' ? `custom=${state.from}..${state.to}`
      : v === 'year' ? `year=${state.year}` : v === 'money' ? `money=${state.mode}${state.when && ':' + state.when}` : v;
  }

  function setHash() {
    const v = state.view;
    const h = viewHash(v);
    if (window.location.hash !== '#' + h) window.location.hash = h; // triggers render via hashchange
    else render();
  }

  let homeShown = false; // whether the previous render showed home, so its live polling is already running

  function render() {
    parseHash();
    const home = state.view === 'home';
    document.getElementById('view-home').hidden = !home;
    document.getElementById('view-money').hidden = state.view !== 'money';
    document.getElementById('view-payback').hidden = state.view !== 'payback';
    closePop(false);
    const brand = document.querySelector('.brand');
    if (home) brand.setAttribute('aria-current', 'page');
    else brand.removeAttribute('aria-current');
    for (const tab of document.querySelectorAll('.tab')) {
      const isCurrent = tab.dataset.view === state.view;
      if (isCurrent) {
        tab.setAttribute('aria-current', 'page');
        // On a phone the tab row scrolls; keep the current tab in view.
        const nav = tab.parentElement;
        if (tab.offsetLeft + tab.offsetWidth > nav.scrollLeft + nav.clientWidth) nav.scrollLeft = tab.offsetLeft + tab.offsetWidth - nav.clientWidth + 16;
      }
      else tab.removeAttribute('aria-current');
      const v = tab.dataset.view;
      tab.setAttribute('href', '#' + viewHash(v));
    }
    // Live polling runs while home shows. A change of History period or date on home leaves it alone.
    if (!home) {
      clearTimeout(timer);
      timer = null;
      pollToken++;
    } else if (!homeShown) {
      lastDayChartAt = 0;
      pollNow();
    }
    homeShown = home;
    if (home) {
      guarded(() => loadHistory());
    } else if (state.view === 'money') guarded(() => loadMoneyView(state.mode, state.when));
    else guarded(() => loadPaybackView());
  }

  function shiftMonth(ym, n) {
    const [y, m] = ym.split('-').map(Number);
    const t = new Date(Date.UTC(y, m - 1 + n, 1));
    return `${t.getUTCFullYear()}-${pad(t.getUTCMonth() + 1)}`;
  }

  /**
   * Switches the History period, keeping the date in view: today when the old
   * period holds it, otherwise the old period's last day. Custom takes the old range.
   */
  function switchPeriod(to) {
    if (to === state.period) return;
    const today = todayStr();
    const [from, end] = rangeOf();
    const anchor = from <= today && today <= end ? today : state.period === 'lifetime' ? today : end > today ? today : end;
    const patch = { period: to, day: anchor, week: mondayOf(anchor), month: anchor.slice(0, 7), year: +anchor.slice(0, 4) };
    if (to === 'custom') Object.assign(patch, state.period === 'lifetime' ? { from: addDays(today, -29), to: today } : { from, to: end > today ? today : end });
    go(patch);
  }

  /** Switches Money between month, year and all time, keeping the year in view. */
  function switchMoney(mode) {
    const today = todayStr();
    const year = state.mode === 'all' ? today.slice(0, 4) : state.when.slice(0, 4);
    const month = year === today.slice(0, 4) ? today.slice(0, 7) : `${year}-01`;
    go({ mode, when: mode === 'month' ? month : mode === 'year' ? year : '' });
  }

  function go(patch) { Object.assign(state, patch); setHash(); }

  /** Syncs the Smooth buttons and remembers the choice (best effort) when `save` is set. */
  function setSmooth(on, save) {
    state.smooth = on;
    for (const b of document.querySelectorAll('.smooth-toggle')) b.setAttribute('aria-pressed', String(on));
    if (!save) return;
    try { localStorage.setItem(SMOOTH_KEY, on ? '1' : '0'); } catch (err) { /* private window or blocked storage */ }
    for (const draw of Object.values(dayDraws)) draw();
  }

  function init() {
    try { state.smooth = localStorage.getItem(SMOOTH_KEY) !== '0'; } catch (err) { /* keep the default */ }
    for (const b of document.querySelectorAll('.smooth-toggle')) {
      b.onclick = () => setSmooth(b.getAttribute('aria-pressed') !== 'true', true);
    }
    setSmooth(state.smooth, false);
    if (!chartsAvailable()) {
      chartsError = "Charts couldn't load (no internet to cdnjs?)";
      updateBanner();
    }
    initHistory();

    for (const b of document.querySelectorAll('[data-money]')) b.onclick = () => switchMoney(b.dataset.money);
    $('money-prev').onclick = () => go({ when: state.mode === 'month' ? shiftMonth(state.when, -1) : String(+state.when - 1) });
    $('money-next').onclick = () => go({ when: state.mode === 'month' ? shiftMonth(state.when, 1) : String(+state.when + 1) });
    $('money-today').onclick = () => go({ when: todayStr().slice(0, state.mode === 'month' ? 7 : 4) });
    window.addEventListener('hashchange', render);
    window.addEventListener('resize', () => { for (const c of Object.values(charts)) c.resize(); });
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && state.view === 'home') pollNow();
    });
    render();
  }

  init();
})();
