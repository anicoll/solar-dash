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
    const el = document.createElement('div');
    el.className = 'tile';
    if (colour) el.style.setProperty('--tile', `var(${colour})`);
    const l = document.createElement('div'); l.className = 'label'; l.textContent = label;
    const v = document.createElement('div'); v.className = 'value'; v.textContent = valueText;
    if (unit) { const u = document.createElement('span'); u.className = 'unit'; u.textContent = unit; v.appendChild(u); }
    el.append(l, v);
    return el;
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
    return { fg: css('--fg'), muted: css('--muted'), line: css('--line') };
  }

  function tooltipStyle() {
    return {
      backgroundColor: css('--card'), borderColor: css('--line'),
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

  // ---------- Now ----------

  /** "5s ago", "3 min ago", "2 h ago". */
  function ageText(secs) {
    const s = Math.max(0, Math.round(secs));
    if (s < 60) return `${s}s ago`;
    const mins = Math.round(s / 60);
    return mins < 120 ? `${mins} min ago` : `${Math.round(mins / 60)} h ago`;
  }

  /** The newest inverter sample, or null if there is none (or the request failed). */
  async function fetchLive() {
    try {
      return (await api('/api/live')).sample || null;
    } catch (err) {
      if (err.message === 'Not logged in.') throw err;
      return null;
    }
  }

  /** Power tile in kW with a direction word: positive means `pos`, negative `neg`. */
  function flowTile(label, w, pos, neg, colourPos, colourNeg) {
    if (w == null) return tile(label, '–', 'kW', colourPos);
    const word = w > 0 ? pos : w < 0 ? neg : 'idle';
    return tile(`${label} · ${word}`, kw(Math.abs(w)), 'kW', w < 0 ? colourNeg : colourPos);
  }

  function renderLiveTiles(sample) {
    const power = (label, w, colour) => tile(label, w == null ? '–' : kw(w), 'kW', colour);
    $('live-tiles').replaceChildren(
      power('Solar (PV)', sample.pv_w, '--c-pv'),
      power('Load', sample.load_w, '--c-load'),
      flowTile('Grid', sample.grid_w, 'importing', 'exporting', '--c-import', '--c-export'),
      flowTile('Battery', sample.battery_w, 'charging', 'discharging', '--c-charge', '--c-discharge'),
      tile('State of charge', sample.soc_pct == null ? '–' : Math.round(sample.soc_pct), '%', '--c-soc'),
    );
    $('updated').textContent = `Live from inverter · updated ${ageText(Date.now() / 1000 - sample.ts)}`;
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
      tile('Solar (PV)', pv == null ? '–' : kw(pv), 'kW', '--c-pv'),
      tile('Load', load == null ? '–' : kw(load), 'kW', '--c-load'),
      tile('Battery', soc == null ? '–' : Math.round(soc), '%', '--c-soc'),
    );
    const ts = Math.max(0, ...[P_PV, P_LOAD, P_SOC].map((id) => (byId[id] ? byId[id].ts : 0)));
    $('updated').textContent = ts
      ? `From iSolarCloud · updated ${ageText(Date.now() / 1000 - ts)} (${fmtLong.format(new Date(ts * 1000))})`
      : 'No readings yet';
  }

  /** Loads the Now view. Returns the delay before the next refresh. */
  async function loadNow() {
    const today = todayStr();
    const [sample, daily] = await Promise.all([
      fetchLive(),
      api(`/api/daily?from=${today}&to=${today}`),
    ]);
    const todayTotals = daily.days[0] || {};
    renderTotals($('today-tiles'), todayTotals);
    // Say where today's totals come from (see "source" in /api/daily).
    $('today-source').textContent =
      todayTotals.source === 'inverter' ? '(inverter)'
      : todayTotals.source === 'cloud' ? '(iSolarCloud)' : '';
    if (sample) {
      renderLiveTiles(sample);
      return LIVE_REFRESH_MS;
    }
    renderCloudTiles(await api('/api/latest'));
    return REFRESH_MS;
  }

  // ---------- Day ----------

  const DAY_SERIES = [
    // key, label, colour var, sign, kind
    ['pv_w', 'PV', '--c-pv', 1, 'area'],
    ['load_w', 'Load', '--c-load', 1, 'line'],
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
    const [day, daily] = await Promise.all([
      api(`/api/day?date=${date}`),
      api(`/api/daily?from=${date}&to=${date}`),
    ]);
    if (state.view !== 'day' || state.day !== date) return;
    const start = day.start * 1000;
    const end = day.end * 1000;
    const live = await fetchLiveDay(day.start, day.end);
    if (state.view !== 'day' || state.day !== date) return;
    const { byKey, source } = mergeDay(day.series, live);
    const hasData = Object.values(byKey).some((d) => d.length > 0);
    renderTotals($('day-totals'), daily.days[0] || {});
    const note = $('day-source');
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

    const option = {
      animation: false,
      grid: { left: 8, right: 8, top: narrow ? 84 : 56, bottom: 8, containLabel: true },
      legend: { top: 0, left: 8, right: 8, textStyle: { color: th.fg }, itemWidth: 16, itemHeight: 8 },
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
          return `<div style="margin-bottom:4px">${clock(params[0].value[0])}</div>${rows.join('')}`;
        },
      },
      xAxis: {
        type: 'value', min: start, max: end, interval: step,
        axisLabel: { color: th.muted, formatter: (v) => clock(v) },
        axisLine: { lineStyle: { color: th.line } }, splitLine: { show: false },
      },
      yAxis: [
        {
          type: 'value', name: 'kW', nameTextStyle: { color: th.muted },
          axisLabel: { color: th.muted, formatter: (v) => v.toFixed(1) },
          splitLine: { lineStyle: { color: th.line } },
        },
        {
          type: 'value', name: 'SOC %', min: 0, max: 100, nameTextStyle: { color: th.muted },
          axisLabel: { color: th.muted }, splitLine: { show: false },
        },
      ],
      series,
    };
    const peak = Math.max(0, ...(byKey.pv_w || []).map((d) => d[1]));
    showChart('day-chart', hasData, option,
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
      tooltip: {
        trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle(),
        formatter(params) {
          const rows = params.map((p) => {
            const val = p.value == null ? null : Math.abs(p.value);
            return `<div style="display:flex;justify-content:space-between;gap:16px">`
              + `<span>${p.marker}${p.seriesName}</span><b>${val == null ? '–' : val.toFixed(1) + ' kWh'}</b></div>`;
          });
          return `<div style="margin-bottom:4px">${tooltipTitle(params[0].dataIndex)}</div>${rows.join('')}`;
        },
      },
      xAxis: {
        type: 'category', data: labels,
        axisLabel: { color: th.muted, interval: narrow && thin ? 'auto' : 0 },
        axisLine: { lineStyle: { color: th.line } },
      },
      yAxis: {
        type: 'value', name: 'kWh', nameTextStyle: { color: th.muted },
        axisLabel: { color: th.muted }, splitLine: { lineStyle: { color: th.line } },
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

  // ---------- routing (hash) ----------

  const state = { view: 'now', day: '', month: '', year: 0 };
  let timer = null;
  let pollToken = 0;

  /** Loads Now, then schedules the next refresh (15 s from the inverter, 60 s from the cloud). */
  async function pollNow() {
    clearTimeout(timer);
    const token = ++pollToken;
    let delay = REFRESH_MS;
    await guarded(async () => { delay = await loadNow(); });
    if (token === pollToken && state.view === 'now') timer = setTimeout(pollNow, delay);
  }

  function parseHash() {
    const raw = decodeURIComponent(window.location.hash.replace(/^#/, ''));
    const [view, value] = raw.split('=');
    const today = todayStr();
    state.view = ['now', 'day', 'month', 'year'].includes(view) ? view : 'now';
    state.day = /^\d{4}-\d{2}-\d{2}$/.test(value || '') && view === 'day' ? value : today;
    state.month = /^\d{4}-\d{2}$/.test(value || '') && view === 'month' ? value : today.slice(0, 7);
    state.year = /^\d{4}$/.test(value || '') && view === 'year' ? +value : +today.slice(0, 4);
  }

  function setHash() {
    const v = state.view;
    const h = v === 'day' ? `day=${state.day}` : v === 'month' ? `month=${state.month}` : v === 'year' ? `year=${state.year}` : 'now';
    if (window.location.hash !== '#' + h) window.location.hash = h; // triggers render via hashchange
    else render();
  }

  function render() {
    parseHash();
    for (const el of document.querySelectorAll('.view')) el.hidden = el.id !== `view-${state.view}`;
    for (const tab of document.querySelectorAll('.tab')) {
      if (tab.dataset.view === state.view) tab.setAttribute('aria-current', 'page');
      else tab.removeAttribute('aria-current');
      const v = tab.dataset.view;
      tab.setAttribute('href', '#' + (v === 'day' ? `day=${state.day}` : v === 'month' ? `month=${state.month}` : v === 'year' ? `year=${state.year}` : 'now'));
    }
    clearTimeout(timer);
    timer = null;
    pollToken++;
    if (state.view === 'now') pollNow();
    else if (state.view === 'day') guarded(() => loadDay(state.day));
    else if (state.view === 'month') guarded(() => loadMonth(state.month));
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

    window.addEventListener('hashchange', render);
    window.addEventListener('resize', () => { for (const c of Object.values(charts)) c.resize(); });
    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', render);
    document.addEventListener('visibilitychange', () => { if (!document.hidden && state.view === 'now') pollNow(); });
    render();
  }

  init();
})();
