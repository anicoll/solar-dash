/* History page: one day in detail, or a week, month, year or everything as bars.

   Routes (the part after #history):
     ''                       today
     day/2026-10-27           one day
     week/2026-10-26          the week starting that Monday
     month/2026-10
     year/2026
     all

   The numbers come from the same endpoints as the current dashboard:
     /api/day, /api/live/series   the power curves of a day
     /api/daily                   energy per day (or per month or year with group=)
     /api/costs                   dollars: import, feed-in, credit, supply, total
     /api/tariff                  price bands (through SD.tariff) */
(function () {
  'use strict';

  const DASH = '–';
  const SVGNS = 'http://www.w3.org/2000/svg';
  const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const BAND_NAME = { free: 'Free', shoulder: 'Shoulder', peak: 'Peak' };
  const BANDS = ['free', 'shoulder', 'peak'];
  const VIEWS = [['day', 'Day'], ['week', 'Wk'], ['month', 'Mo'], ['year', 'Yr'], ['all', 'All']];
  const LIVE_GAP_SECS = 3 * 60; // a longer silence in the inverter's readings is a gap in the line
  const LIVE_MIN_MINUTES = 30; // fewer minute readings than this and the cloud curve is used
  const LIFETIME_YEARS = 19; // /api/daily?group= covers at most 20 years

  /* ---------------------------------------------------------------- small helpers */

  function S(tag, attrs, ...kids) {
    const el = document.createElementNS(SVGNS, tag);
    for (const [k, v] of Object.entries(attrs || {})) if (v != null) el.setAttribute(k, v);
    for (const c of kids.flat()) if (c != null) el.append(c);
    return el;
  }

  const num = (n) => n != null && Number.isFinite(Number(n));
  const pad = (n) => String(n).padStart(2, '0');
  const sum = (list) => list.reduce((a, b) => a + (num(b) ? b : 0), 0);
  const money = (dollars) => SD.fmt.dollars(dollars);
  const wide = () => window.matchMedia('(min-width: 760px)').matches;

  // Calendar maths on 'YYYY-MM-DD' strings, with no time zone in the way.
  const utc = (d) => new Date(`${d}T12:00:00Z`);
  const iso = (dt) => dt.toISOString().slice(0, 10);
  const addDays = (d, n) => iso(new Date(utc(d).getTime() + n * 86400000));
  const mondayOf = (d) => addDays(d, -((utc(d).getUTCDay() + 6) % 7));
  const monthEnd = (ym) => iso(new Date(Date.UTC(+ym.slice(0, 4), +ym.slice(5, 7), 0, 12)));
  const addMonths = (ym, n) => {
    const t = +ym.slice(0, 4) * 12 + (+ym.slice(5, 7) - 1) + n;
    return `${Math.floor(t / 12)}-${pad((t % 12) + 1)}`;
  };
  const validDate = (v) => /^\d{4}-\d{2}-\d{2}$/.test(v || '') && iso(utc(v)) === v;
  const validMonth = (v) => /^\d{4}-(0[1-9]|1[0-2])$/.test(v || '');
  const validYear = (v) => /^\d{4}$/.test(v || '');

  /** The route text as a view: {view, key} with key the first day, month or year. Bad input means today. */
  function parseRoute(rest) {
    const today = SD.local().date;
    const [view, key] = String(rest || '').split('/');
    if (view === 'day' && validDate(key)) return { view, key: key > today ? today : key };
    if (view === 'week' && validDate(key)) return { view, key: mondayOf(key) };
    if (view === 'month' && validMonth(key)) return { view, key };
    if (view === 'year' && validYear(key)) return { view, key };
    if (view === 'all') return { view, key: '' };
    return { view: 'day', key: today };
  }

  const hashOf = (r) => `history/${r.view}${r.key ? '/' + r.key : ''}`;

  /** First and last day (clipped to today) a view covers; null for All. */
  function rangeOf(r) {
    const today = SD.local().date;
    let from, to;
    if (r.view === 'day') { from = to = r.key; }
    else if (r.view === 'week') { from = r.key; to = addDays(r.key, 6); }
    else if (r.view === 'month') { from = `${r.key}-01`; to = monthEnd(r.key); }
    else if (r.view === 'year') { from = `${r.key}-01-01`; to = `${r.key}-12-31`; }
    else { from = `${+today.slice(0, 4) - LIFETIME_YEARS}-01-01`; to = today; }
    return { from, to: to > today ? today : to, fullTo: to };
  }

  /** The same kind of period `n` steps away. */
  function shift(r, n) {
    if (r.view === 'day') return { view: 'day', key: addDays(r.key, n) };
    if (r.view === 'week') return { view: 'week', key: addDays(r.key, 7 * n) };
    if (r.view === 'month') return { view: 'month', key: addMonths(r.key, n) };
    if (r.view === 'year') return { view: 'year', key: String(+r.key + n) };
    return r;
  }

  /** Switching period keeps the place: the period that holds the same day. */
  function switchTo(r, view) {
    const today = SD.local().date;
    const { from, fullTo } = rangeOf(r);
    // The day to carry over: today when the period holds it, else the period's first day.
    const day = r.view === 'all' || (from <= today && today <= fullTo) ? today : from;
    if (view === 'day') return { view, key: day };
    if (view === 'week') return { view, key: mondayOf(day) };
    if (view === 'month') return { view, key: day.slice(0, 7) };
    if (view === 'year') return { view, key: day.slice(0, 4) };
    return { view: 'all', key: '' };
  }

  function label(r) {
    const today = SD.local().date;
    if (r.view === 'day') {
      const text = SD.fmt.date(r.key, 'long');
      return r.key.slice(0, 4) === today.slice(0, 4) ? text : `${text} ${r.key.slice(0, 4)}`;
    }
    if (r.view === 'week') {
      const end = addDays(r.key, 6);
      const y = end.slice(0, 4) === today.slice(0, 4) ? '' : ` ${end.slice(0, 4)}`;
      return `${SD.fmt.date(r.key, 'short')} to ${SD.fmt.date(end, 'short')}${y}`;
    }
    if (r.view === 'month') return SD.fmt.date(`${r.key}-01`, 'month');
    if (r.view === 'year') return r.key;
    return 'All time';
  }

  const NOUN = { day: 'day', week: 'week', month: 'month', year: 'year' };

  /** A request that may fail without failing the page: null instead. */
  const quiet = (p) => p.catch((err) => { console.error(err); return null; });

  /* ---------------------------------------------------------------- the page */

  let seq = 0; // bumps on every render, so a slow answer for an old view is dropped
  let route = null;
  let ui = null; // the controls
  let content = null;
  let redraw = null; // redraws the charts when the width changes
  let observer = null;
  let lastWidth = 0;

  function mount(el, rest) {
    route = parseRoute(rest);
    ui = buildControls();
    content = SD.h('div', { class: 'history-content', 'aria-live': 'polite' });
    el.append(ui.root, content);

    // Charts are drawn at their pixel width, so draw them again when it changes.
    if (window.ResizeObserver) {
      observer = new ResizeObserver(() => {
        const w = content.clientWidth;
        if (w && Math.abs(w - lastWidth) > 1 && redraw) { lastWidth = w; redraw(); }
      });
      observer.observe(content);
    }
    render();
  }

  function update(rest) {
    route = parseRoute(rest);
    render();
  }

  function unmount() {
    seq++;
    if (observer) observer.disconnect();
    observer = null;
    redraw = null;
    ui = content = null;
  }

  function buildControls() {
    const seg = SD.h('div', { class: 'seg', role: 'group', 'aria-label': 'Period' });
    const buttons = {};
    for (const [view, text] of VIEWS) {
      buttons[view] = SD.h('button', { type: 'button', text, 'aria-pressed': 'false', onclick: () => SD.go(hashOf(switchTo(route, view))) });
      seg.append(buttons[view]);
    }
    const prev = SD.h('button', { type: 'button', class: 'history-step', text: '‹', onclick: () => SD.go(hashOf(shift(route, -1))) });
    const next = SD.h('button', { type: 'button', class: 'history-step', text: '›', onclick: () => SD.go(hashOf(shift(route, 1))) });
    const name = SD.h('span', { class: 'history-label', 'aria-live': 'polite' });
    const stepper = SD.h('div', { class: 'history-nav' }, prev, name, next);
    const root = SD.h('div', { class: 'history-bar' },
      SD.h('div', { class: 'history-top' },
        SD.h('a', { class: 'history-back', href: '#today', text: '‹ Today' }), seg),
      stepper);
    return { root, buttons, prev, next, name, stepper };
  }

  function syncControls() {
    for (const [view] of VIEWS) ui.buttons[view].setAttribute('aria-pressed', String(view === route.view));
    ui.name.textContent = label(route);
    ui.stepper.hidden = route.view === 'all';
    const noun = NOUN[route.view] || 'period';
    ui.prev.setAttribute('aria-label', `Previous ${noun}`);
    ui.next.setAttribute('aria-label', `Next ${noun}`);
    ui.next.disabled = rangeOf(shift(route, 1)).from > SD.local().date;
  }

  function render() {
    const mine = ++seq;
    redraw = null;
    syncControls();
    document.title = `History · ${label(route)} · solar-dash`;
    content.replaceChildren(SD.h('p', { class: 'muted history-loading', text: 'Loading…' }));
    const done = (build) => {
      if (mine !== seq) return;
      content.replaceChildren(...build.nodes);
      lastWidth = content.clientWidth;
      redraw = build.redraw || null;
      if (redraw) redraw();
    };
    const fail = (err) => {
      if (mine !== seq) return;
      console.error(err);
      content.replaceChildren(SD.h('div', { class: 'card', role: 'alert' }, SD.h('p', { text: `Could not load this period. ${err.message}` })));
    };
    const load = route.view === 'day' ? loadDay(route.key) : loadPeriod(route);
    load.then(done).catch(fail);
  }

  /* ---------------------------------------------------------------- legend and tiles */

  const swatchStyle = { free: 'background: var(--sun); box-shadow: inset 0 0 0 1px var(--sun-line)', shoulder: 'background: var(--bar)', peak: 'background: var(--peak)' };
  const bandSwatch = (band) => SD.h('span', { class: 'swatch', style: swatchStyle[band], 'aria-hidden': 'true' });

  /** Cents per kWh of each import band, as the plan has them: {free:0, shoulder:44, peak:58.3}. */
  function bandCents(tariff) {
    const out = {};
    for (const s of SD.tariff.segments(tariff)) if (!(s.band in out)) out[s.band] = s.cents;
    return out;
  }

  function tile(name, big, sub) {
    return SD.h('div', { class: 'history-tile' },
      SD.h('div', { class: 'label', text: name }),
      SD.h('div', { class: 'fig fig-lg', text: big }),
      sub ? SD.h('div', { class: 'history-tile-sub', text: sub }) : null);
  }

  /* ---------------------------------------------------------------- the day view */

  /** Power curves: the cloud's, with the inverter's own minute readings over them when it has enough. */
  function dayCurves(day, rows) {
    const cloud = {};
    for (const s of day.series || []) cloud[s.key] = s.data || [];
    const live = (rows || []).filter((r) => r.slice(1).some((v) => v != null));
    const out = { source: 'cloud', rows: live };
    const keys = { pv: ['pv_w', 1], load: ['load_w', 2], soc: ['soc', 5] };
    const first = live.length >= LIVE_MIN_MINUTES ? live[0][0] : null;
    for (const [name, [key, idx]] of Object.entries(keys)) {
      let series = (cloud[key] || []).map((p) => [p[0], p[1]]);
      if (first != null) {
        series = series.filter((p) => p[0] < first);
        let prev = null;
        for (const r of live) {
          if (r[idx] == null) continue;
          if (prev != null && r[0] - prev > LIVE_GAP_SECS) series.push([r[0] - 60, null]);
          series.push([r[0], r[idx]]);
          prev = r[0];
        }
        out.source = 'inverter';
      }
      out[name] = series;
    }
    return out;
  }

  const inWindow = (from, to, m) => (from < to ? from <= m && m < to : m >= from || m < to);

  /** Cents per kWh sold at a minute of the day, from the plan's export windows. */
  function exportCentsAt(tariff, minute) {
    const toMin = (t) => { const [h, m] = t.split(':').map(Number); return h * 60 + m; };
    const w = (tariff.export || []).find((x) => inWindow(toMin(x.from), toMin(x.to), minute));
    return w ? w.cents : tariff.default_export_cents || 0;
  }

  /**
   * What the inverter's minute readings say about grid use: kWh bought inside the credit
   * window, and kWh sold in each import band (a minute counts in the band whose time it is).
   */
  function gridFromRows(rows, tariff) {
    if (!tariff || !tariff.configured || rows.length < LIVE_MIN_MINUTES) return null;
    const credit = SD.tariff.creditWindow(tariff);
    const out = { creditImport: 0, sold: { free: 0, shoulder: 0, peak: 0 }, soldWeight: { free: 0, shoulder: 0, peak: 0 } };
    for (const r of rows) {
      if (r[3] == null) continue;
      const minute = SD.local(r[0]).minute;
      const kwh = Math.abs(r[3]) * 60 / 3.6e6; // a row is one minute of watts
      if (r[3] > 0 && credit && inWindow(credit.start, credit.end, minute)) out.creditImport += kwh;
      if (r[3] < 0) {
        const band = SD.tariff.bandAt(tariff, minute);
        if (band in out.sold) {
          out.sold[band] += kwh;
          out.soldWeight[band] += kwh * exportCentsAt(tariff, minute);
        }
      }
    }
    return out;
  }

  async function loadDay(date) {
    const today = SD.local().date;
    const [dayData, tariffRaw, daily, costs] = await Promise.all([
      SD.fetchJSON(`/api/day?date=${date}`),
      quiet(SD.tariff.get(date)),
      quiet(SD.fetchJSON(`/api/daily?from=${date}&to=${date}`)),
      quiet(SD.fetchJSON(`/api/costs?from=${date}&to=${date}`)),
    ]);
    const live = await quiet(SD.fetchJSON(`/api/live/series?from=${dayData.start}&to=${dayData.end - 1}`));
    const tariff = tariffRaw && tariffRaw.configured ? tariffRaw : null;
    const d = (daily && daily.days && daily.days[0]) || {};
    const c = (costs && costs.days && costs.days[0]) || null;
    const curves = dayCurves(dayData, (live && live.data) || []);
    const grid = gridFromRows(curves.rows, tariff);
    const hasCurve = curves.pv.length > 0 || curves.load.length > 0;
    const hasEnergy = num(d.pv_kwh) || num(d.load_kwh) || num(d.import_kwh);

    if (!hasCurve && !hasEnergy) {
      return { nodes: [SD.h('div', { class: 'card history-empty' }, SD.h('p', { text: date === today ? 'No data yet today.' : 'No data for this day.' }))] };
    }

    const nodes = [];

    // The verdict: what the power cost and whether the $1 was earned.
    const credit = tariff ? SD.tariff.creditWindow(tariff) : null;
    const verdict = [];
    if (c) verdict.push(`Power cost ${SD.fmt.cents(c.import * 100)}`);
    if (credit && c) {
      const amount = credit.creditCents % 100 === 0 ? `$${credit.creditCents / 100}` : SD.fmt.cents(credit.creditCents);
      const now = SD.local();
      if (c.credit > 0) verdict.push(`the ${amount} was earned`);
      else if (date === today && now.minute < credit.end && credit.start <= credit.end) {
        verdict.push(now.minute < credit.start ? `the ${amount} window is still to come` : `the ${amount} window is open`);
      } else if (grid && credit.maxKwh != null) {
        const over = Math.max(0.1, Math.round((grid.creditImport - credit.maxKwh) * 10) / 10);
        verdict.push(`the ${amount} was missed by ${SD.fmt.kwh(over)}`);
      } else verdict.push(`the ${amount} was missed`);
    }
    const head = SD.h('section', { class: 'history-head' });
    if (verdict.length) head.append(SD.h('h1', { class: 'verdict history-verdict', text: verdict.length > 1 ? `${verdict[0]}, and ${verdict[1]}.` : `${verdict[0]}.` }));
    else head.append(SD.h('h1', { class: 'verdict history-verdict', text: SD.fmt.date(date, 'long') }));

    const facts = [];
    if (c) {
      const total = c.actual;
      facts.push(total < 0 ? `Day total ${SD.fmt.cents(-total * 100)} back after supply.` : `Day total ${SD.fmt.cents(total * 100)} with supply.`);
    }
    const solarText = num(d.pv_kwh) ? `Solar made ${SD.fmt.kwh(d.pv_kwh)}` : null;
    let ownText = null;
    if (num(d.load_kwh) && d.load_kwh > 0 && num(d.import_kwh)) {
      const own = Math.max(0, Math.min(100, Math.round((1 - d.import_kwh / d.load_kwh) * 100)));
      ownText = `the house ran on its own power ${own}% of the time`;
    }
    if (solarText && ownText) facts.push(`${solarText} and ${ownText}.`);
    else if (solarText) facts.push(`${solarText}.`);
    else if (ownText) facts.push(`The ${ownText.slice(4)}.`);
    if (facts.length) head.append(SD.h('p', { class: 'lede history-lede', text: facts.join(' ') }));
    nodes.push(head);

    // The power chart.
    let redraw = null;
    if (hasCurve) {
      const chartBox = SD.h('div', { class: 'history-chart' });
      const ribbonBox = SD.h('div', { class: 'history-ribbon' });
      const legend = SD.h('p', { class: 'history-legend' },
        SD.h('span', {}, SD.h('i', { class: 'history-key history-key-solar', 'aria-hidden': 'true' }), 'Solar'),
        SD.h('span', {}, SD.h('i', { class: 'history-key history-key-home', 'aria-hidden': 'true' }), 'Home'),
        curves.soc.length ? SD.h('span', {}, SD.h('i', { class: 'history-key history-key-battery', 'aria-hidden': 'true' }), 'Battery %') : null);
      const smoothSeg = SD.h('div', { class: 'seg history-smooth', role: 'group', 'aria-label': 'Line style' });
      for (const [on, text] of [[true, 'Smooth'], [false, 'Raw']]) {
        smoothSeg.append(SD.h('button', {
          type: 'button', text, 'aria-pressed': String(smoothOn() === on),
          onclick: () => {
            setSmooth(on);
            for (const b of smoothSeg.children) b.setAttribute('aria-pressed', String(b === smoothSeg.children[on ? 0 : 1]));
            redraw();
          },
        }));
      }
      nodes.push(SD.h('section', { class: 'card history-card', 'aria-labelledby': 'history-power-h' },
        SD.h('div', { class: 'card-head' },
          SD.h('h2', { class: 'section-title', id: 'history-power-h', text: 'Power through the day' }),
          smoothSeg),
        chartBox, ribbonBox, legend));
      let ribbon = null;
      redraw = () => {
        drawPower(chartBox, dayData, smoothOn() ? smoothCurves(curves) : curves, tariff, d);
        if (!ribbon || ribbon.long !== wide()) {
          ribbon = { long: wide(), r: SD.ribbon(ribbonBox, tariffRaw, { showCredit: true, labels: wide() ? 'long' : 'short' }) };
        }
      };
    }

    // Where the solar went.
    if (num(d.pv_kwh) && d.pv_kwh > 0) nodes.push(solarCard(d));

    // Price bands.
    const table = tariff && c ? bandTable(d, c, tariff, grid) : null;
    if (table) nodes.push(table);

    return { nodes: [wrapDay(nodes)], redraw };
  }

  /** On a wide screen the cards after the chart sit side by side. */
  function wrapDay(nodes) {
    const [head, ...cards] = nodes;
    const chart = cards.find((n) => n.getAttribute('aria-labelledby') === 'history-power-h');
    const rest = cards.filter((n) => n !== chart);
    return SD.h('div', { class: 'history-day' }, head, chart,
      rest.length ? SD.h('div', { class: 'history-pair' }, rest) : null);
  }

  /** "Where the solar went": home, battery and sold, as one bar and three figures. */
  function solarCard(d) {
    const pv = d.pv_kwh;
    // Home: what the house used that did not come from the grid or the battery. The rest of
    // the solar went to the battery or the grid, shared out by how much each one took in total.
    const n = (v) => (num(v) ? v : 0);
    const home = Math.max(0, Math.min(pv, n(d.load_kwh) - n(d.import_kwh) - n(d.battery_discharge_kwh)));
    const rest = pv - home;
    const took = n(d.battery_charge_kwh) + n(d.export_kwh);
    const battery = took > 0 ? (rest * n(d.battery_charge_kwh)) / took : 0;
    const sold = rest - battery;
    const items = [['Home', home, 'history-seg-home'], ['Battery', battery, 'history-seg-battery'], ['Sold', sold, 'history-seg-sold']];
    const bar = SD.h('div', { class: 'history-stack', role: 'img', 'aria-label': items.map(([n, v]) => `${n} ${SD.fmt.kwh(v)}`).join(', ') });
    for (const [, v, cls] of items) if (v > 0) bar.append(SD.h('div', { class: cls, style: `flex: ${v} 1 0` }));
    const figs = SD.h('div', { class: 'history-figs' });
    for (const [n, v, cls] of items) {
      figs.append(SD.h('div', {}, SD.h('div', { class: 'label' }, SD.h('i', { class: `history-dot ${cls}`, 'aria-hidden': 'true' }), n), SD.h('div', { class: 'fig fig-md', text: SD.fmt.kwh(v) })));
    }
    return SD.h('section', { class: 'card history-card', 'aria-labelledby': 'history-solar-h' },
      SD.h('h2', { class: 'section-title', id: 'history-solar-h', text: `Where the ${SD.fmt.kwh(pv)} of solar went` }),
      bar, figs);
  }

  /**
   * "By price band": bought and sold in each band with what it cost and earned. The day's
   * dollars (from /api/costs) are shared out over the bands by kWh times price, so the rows add up.
   */
  function bandTable(d, c, tariff, grid) {
    const cents = bandCents(tariff);
    const bands = BANDS.filter((b) => b in cents);
    const bought = { free: d.import_free_kwh, shoulder: d.import_shoulder_kwh, peak: d.import_peak_kwh };
    if (!bands.some((b) => num(bought[b]))) return null;

    const weights = bands.map((b) => (num(bought[b]) ? bought[b] * cents[b] : 0));
    const wTotal = sum(weights);
    const cost = (i) => (wTotal > 0 ? (c.import * weights[i]) / wTotal : 0);

    // Sold per band needs the minute readings; scale them to the day's metered export.
    let sold = null;
    if (grid) {
      const raw = sum(Object.values(grid.sold));
      const scale = num(d.export_kwh) && raw > 0.05 ? d.export_kwh / raw : 1;
      const wSum = sum(Object.values(grid.soldWeight));
      sold = bands.map((b) => ({
        kwh: grid.sold[b] * scale,
        earned: wSum > 0 ? (c.feed_in * grid.soldWeight[b]) / wSum : 0,
      }));
    }

    const th = (text, cls) => SD.h('th', { scope: 'col', class: cls, text });
    const head = SD.h('tr', {}, th('Band', 'history-left'), th('Bought'), th('Cost'), sold ? th('Sold') : null, sold ? th('Earned') : null);
    const rows = bands.map((b, i) => SD.h('tr', {},
      SD.h('th', { scope: 'row', class: 'history-left' }, bandSwatch(b), BAND_NAME[b]),
      SD.h('td', { text: num(bought[b]) ? bought[b].toFixed(1) : DASH }),
      SD.h('td', { text: money(cost(i)) }),
      sold ? SD.h('td', { text: sold[i].kwh.toFixed(1) }) : null,
      sold ? SD.h('td', { text: money(sold[i].earned) }) : null));
    const cols = sold ? 4 : 2;
    const extra = [];
    if (!sold) extra.push(SD.h('tr', {}, SD.h('th', { scope: 'row', class: 'history-left', text: `Sold ${num(d.export_kwh) ? d.export_kwh.toFixed(1) : DASH} kWh` }), SD.h('td', { colspan: cols, text: `${money(c.feed_in)} earned` })));
    if (c.credit > 0) extra.push(SD.h('tr', {}, SD.h('th', { scope: 'row', class: 'history-left', text: 'Credit' }), SD.h('td', { colspan: cols, class: 'history-right', text: money(c.credit) })));
    const total = SD.h('tr', { class: 'history-total' },
      SD.h('th', { scope: 'row', class: 'history-left', text: `Day, with ${money(c.supply)} supply` }),
      SD.h('td', { colspan: cols, text: money(c.actual) }));
    const billed = c.source === 'globird' || c.source === 'bill';
    const note = billed
      ? 'kWh. The money figures are what GloBird billed for this day.'
      : 'kWh. The money figures are our estimate until GloBird has this day.';
    return SD.h('section', { class: 'card history-card', 'aria-labelledby': 'history-band-h' },
      SD.h('h2', { class: 'section-title', id: 'history-band-h', text: 'By price band' }),
      SD.h('table', { class: 'history-table' }, SD.h('thead', {}, head), SD.h('tbody', {}, rows, extra, total)),
      SD.h('p', { class: 'label history-note', text: note }));
  }

  /* ---------------------------------------------------------------- the power chart */

  const niceMax = (v) => {
    const steps = [1, 2, 3, 4, 5, 6, 8, 10, 12, 15, 20, 25, 30, 40, 50];
    return steps.find((s) => s >= v) || Math.ceil(v / 10) * 10;
  };

  /* ---------------------------------------------------------------- smoothing */

  // Smooth or raw lines on the day chart, remembered in this browser.
  const SMOOTH_KEY = 'sd-history-smooth';
  const SMOOTH_SECS = 300; // average over 5 minutes either side
  function smoothOn() {
    try { return localStorage.getItem(SMOOTH_KEY) !== 'raw'; } catch { return true; }
  }
  function setSmooth(on) {
    try { localStorage.setItem(SMOOTH_KEY, on ? 'smooth' : 'raw'); } catch { /* not saved; fine */ }
  }

  /** Each reading replaced by the average of the readings within 5 minutes of it; gaps stay gaps. */
  function smoothSeries(series) {
    const out = [];
    let lo = 0;
    let hi = 0;
    let total = 0;
    let count = 0;
    let runStart = 0;
    for (let i = 0; i < series.length; i++) {
      const [ts, v] = series[i];
      if (v == null) { out.push([ts, null]); runStart = i + 1; lo = hi = runStart; total = count = 0; continue; }
      // Grow the window forward to ts + 5 min (stopping at a gap), shrink it from behind.
      while (hi < series.length && series[hi][1] != null && series[hi][0] <= ts + SMOOTH_SECS) { total += series[hi][1]; count++; hi++; }
      while (lo < hi && (lo < runStart || series[lo][0] < ts - SMOOTH_SECS)) { if (lo >= runStart) { total -= series[lo][1]; count--; } lo++; }
      out.push([ts, count ? total / count : v]);
    }
    return out;
  }

  /** The day's curves with solar and home smoothed (battery % is already smooth). */
  function smoothCurves(curves) {
    return { ...curves, pv: smoothSeries(curves.pv), load: smoothSeries(curves.load) };
  }

  /* ---------------------------------------------------------------- hover details */

  /**
   * A small box that follows the pointer over a chart. `contentAt(frac)` gets the pointer's
   * position across the chart (0..1) and returns {at, nodes} (at = where to centre the box,
   * 0..1) or null. Charts redraw on resize, so each draw calls this again with its own
   * `contentAt` and the line to move; the listeners are only added once per box.
   */
  function chartTip(box, svg, height, contentAt, { touch = true } = {}) {
    if (!box.sdTip) {
      box.sdTip = SD.h('div', { class: 'history-tip', hidden: true });
      const hide = () => {
        box.sdTip.hidden = true;
        if (box.sdHover) box.sdHover.setAttribute('visibility', 'hidden');
      };
      box.addEventListener('pointermove', (e) => {
        if (e.pointerType !== 'mouse' && !box.sdTouch) return;
        const rect = box.getBoundingClientRect();
        const got = box.sdContentAt((e.clientX - rect.left) / rect.width);
        if (!got) return hide();
        const tip = box.sdTip;
        tip.replaceChildren(...got.nodes);
        tip.hidden = false;
        const w = tip.offsetWidth;
        const cx = got.at * rect.width;
        tip.style.left = `${Math.max(0, Math.min(rect.width - w, cx - w / 2))}px`;
        box.sdHover.setAttribute('x1', cx);
        box.sdHover.setAttribute('x2', cx);
        box.sdHover.removeAttribute('visibility');
      });
      box.addEventListener('pointerleave', hide);
    }
    box.sdContentAt = contentAt;
    box.sdTouch = touch;
    box.sdHover = S('line', { x1: 0, x2: 0, y1: 0, y2: height, class: 'history-hover', visibility: 'hidden' });
    svg.append(box.sdHover);
    box.sdTip.hidden = true;
    box.append(box.sdTip);
  }

  const tipLine = (text) => SD.h('span', { text });

  /** kWh bought and sold in each hour, from the inverter's minute readings. */
  function hourlyGrid(rows) {
    const bought = new Array(24).fill(0);
    const sold = new Array(24).fill(0);
    for (const r of rows) {
      if (r[3] == null) continue;
      const hr = Math.floor(SD.local(r[0]).minute / 60);
      const kwh = Math.abs(r[3]) * 60 / 3.6e6; // a row is one minute of watts
      if (r[3] > 0) bought[hr] += kwh;
      else sold[hr] += kwh;
    }
    return { bought, sold };
  }

  /** The reading nearest a timestamp, or null when the nearest is more than 10 minutes away. */
  function nearest(series, ts) {
    let best = null;
    for (const p of series) {
      if (p[1] == null) continue;
      if (!best || Math.abs(p[0] - ts) < Math.abs(best[0] - ts)) best = p;
    }
    return best && Math.abs(best[0] - ts) <= 600 ? best[1] : null;
  }

  function drawPower(box, day, curves, tariff, totals) {
    const width = Math.max(box.clientWidth || box.parentElement.clientWidth || 300, 200);
    const height = width >= 560 ? 230 : 170;
    const top = 16;
    const bottom = height - 1;
    const len = day.end - day.start;
    // Wall-clock minute, so the chart lines up with the price strip on days the clocks change.
    const minuteOf = (ts) => (len === 86400 ? (ts - day.start) / 60 : SD.local(ts).minute);
    const x = (ts) => (minuteOf(ts) / 1440) * width;
    const maxKw = niceMax(Math.max(0.5, ...[...curves.pv, ...curves.load].map((p) => (p[1] == null ? 0 : p[1] / 1000))));
    const yKw = (w) => bottom - (w / 1000 / maxKw) * (bottom - top);
    const ySoc = (pct) => bottom - (pct / 100) * (bottom - top);

    const svg = S('svg', { width, height, viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': powerSummary(day, curves, maxKw) });

    // Price band tints behind everything.
    if (tariff) {
      for (const s of SD.tariff.segments(tariff)) {
        if (s.band === 'shoulder') continue;
        svg.append(S('rect', { x: (s.start / 1440) * width, y: 0, width: ((s.end - s.start) / 1440) * width, height: bottom, class: `history-tint-${s.band}` }));
      }
    }
    for (const frac of [0.5, 1]) {
      const yy = bottom - frac * (bottom - top);
      svg.append(S('line', { x1: 0, x2: width, y1: yy, y2: yy, class: 'history-grid' }));
      svg.append(S('text', { x: width - 2, y: yy - 3, 'text-anchor': 'end', class: 'history-axis-label' }, `${+(maxKw * frac).toFixed(1)} kW`));
    }
    svg.append(S('line', { x1: 0, x2: width, y1: bottom, y2: bottom, class: 'history-base' }));

    // Solar as an area, one piece per unbroken run of readings.
    const runs = (series) => {
      const out = [];
      let cur = [];
      for (const p of series) {
        if (p[1] == null) { if (cur.length) out.push(cur); cur = []; } else cur.push(p);
      }
      if (cur.length) out.push(cur);
      return out;
    };
    const line = (pts, y) => pts.map((p, i) => `${i ? 'L' : 'M'}${x(p[0]).toFixed(1)} ${y(p[1]).toFixed(1)}`).join('');
    for (const run of runs(curves.pv)) {
      if (run.length < 2) continue;
      const area = `${line(run, yKw)}L${x(run[run.length - 1][0]).toFixed(1)} ${bottom}L${x(run[0][0]).toFixed(1)} ${bottom}Z`;
      svg.append(S('path', { d: area, class: 'history-solar-area' }));
      svg.append(S('path', { d: line(run, yKw), class: 'history-solar-line' }));
    }
    for (const run of runs(curves.load)) if (run.length > 1) svg.append(S('path', { d: line(run, yKw), class: 'history-home-line' }));
    for (const run of runs(curves.soc)) if (run.length > 1) svg.append(S('path', { d: line(run, ySoc), class: 'history-battery-line' }));

    box.replaceChildren(svg);
    // Hover: the readings at that time, and what was bought and sold in that hour.
    const hourly = curves.source === 'inverter' ? hourlyGrid(curves.rows) : null;
    chartTip(box, svg, bottom, (frac) => {
      if (frac < 0 || frac >= 1) return null;
      const minute = Math.floor(frac * 1440);
      const hr = Math.floor(minute / 60);
      // Timestamp for that wall-clock minute (same as x() above, run backwards).
      const ts = len === 86400 ? day.start + minute * 60 : day.start + minute * 60;
      const pv = nearest(curves.pv, ts);
      const load = nearest(curves.load, ts);
      const soc = nearest(curves.soc, ts);
      const nodes = [SD.h('b', { text: SD.fmt.minute(minute) })];
      const now = [];
      if (pv != null) now.push(`Solar ${SD.fmt.kw(pv / 1000)}`);
      if (load != null) now.push(`home ${SD.fmt.kw(load / 1000)}`);
      if (soc != null) now.push(`battery ${Math.round(soc)}%`);
      if (now.length) nodes.push(tipLine(now.join(', ')));
      if (hourly) {
        const from = SD.fmt.minute(hr * 60);
        const to = SD.fmt.minute(((hr + 1) % 24) * 60);
        const b = hourly.bought[hr];
        let cost = '';
        if (b > 0.005 && tariff) {
          const band = SD.tariff.bandAt(tariff, hr * 60 + 30);
          cost = band === 'free' ? ', free' : `, about ${SD.fmt.cents(b * SD.tariff.centsAt(tariff, hr * 60 + 30))}`;
        }
        nodes.push(tipLine(`${from} to ${to}: bought ${b > 0.005 ? SD.fmt.kwh(b, 2) : 'nothing'}${cost}`));
        nodes.push(tipLine(`sold ${hourly.sold[hr] > 0.005 ? SD.fmt.kwh(hourly.sold[hr], 2) : 'nothing'}`));
      }
      return nodes.length > 1 ? { at: frac, nodes } : null;
    });
  }

  /** A sentence for screen readers: the day's peaks and the battery's range. */
  function powerSummary(day, curves, maxKw) {
    const peak = (series) => series.reduce((best, p) => (p[1] != null && (!best || p[1] > best[1]) ? p : best), null);
    const at = (ts) => SD.fmt.time(ts);
    const out = [];
    const pv = peak(curves.pv);
    if (pv && pv[1] >= 100) out.push(`Solar peaked at ${SD.fmt.kw(pv[1] / 1000)} at ${at(pv[0])}.`);
    const load = peak(curves.load);
    if (load) out.push(`Home use peaked at ${SD.fmt.kw(load[1] / 1000)} at ${at(load[0])}.`);
    const soc = curves.soc.filter((p) => p[1] != null);
    if (soc.length) {
      const lo = Math.min(...soc.map((p) => p[1]));
      const hi = Math.max(...soc.map((p) => p[1]));
      out.push(`Battery ranged from ${Math.round(lo)}% to ${Math.round(hi)}%.`);
    }
    return `Power through the day, up to ${maxKw} kW. ${out.join(' ')}`.trim();
  }

  /* ---------------------------------------------------------------- week, month, year, all */

  async function loadPeriod(r) {
    const { from, to } = rangeOf(r);
    const grouped = r.view === 'year' ? 'month' : r.view === 'all' ? 'year' : null;
    const g = grouped ? `&group=${grouped}` : '';
    const prev = r.view === 'all' ? null : rangeOf(shift(r, -1));
    const [energy, costs, prevEnergy, tariff] = await Promise.all([
      SD.fetchJSON(`/api/daily?from=${from}&to=${to}${g}`),
      quiet(SD.fetchJSON(`/api/costs?from=${from}&to=${to}${g}`)),
      prev ? quiet(SD.fetchJSON(`/api/daily?from=${prev.from}&to=${prev.fullTo < SD.local().date ? prev.fullTo : prev.to}&group=month`)) : Promise.resolve(null),
      quiet(SD.tariff.get(r.view === 'week' || r.view === 'month' ? to : undefined)),
    ]);

    // One bar slot per day (or month, or year), in order, including days still to come.
    let slots, totals;
    const today = SD.local().date;
    if (grouped) {
      const byPeriod = new Map((energy.buckets || []).map((b) => [b.period, b]));
      if (r.view === 'year') {
        slots = MONTHS.map((m, i) => ({ key: `${r.key}-${pad(i + 1)}`, text: m, long: `${m} ${r.key}`, href: `history/month/${r.key}-${pad(i + 1)}`, row: byPeriod.get(`${r.key}-${pad(i + 1)}`) }));
      } else {
        const keys = [...byPeriod.keys()].sort();
        const firstYear = keys.length ? +keys[0] : +today.slice(0, 4);
        slots = [];
        for (let y = firstYear; y <= +today.slice(0, 4); y++) slots.push({ key: String(y), text: String(y), long: String(y), href: `history/year/${y}`, row: byPeriod.get(String(y)) });
      }
      totals = energy.totals || {};
    } else {
      const byDate = new Map((energy.days || []).map((d) => [d.date, d]));
      slots = [];
      for (let day = from; day <= rangeOf(r).fullTo; day = addDays(day, 1)) {
        const row = byDate.get(day);
        slots.push({
          key: day,
          text: r.view === 'week' ? SD.fmt.date(day, 'medium').split(' ')[0] : String(+day.slice(8)),
          long: SD.fmt.date(day, 'medium'),
          href: `history/day/${day}`,
          row: row && (num(row.import_kwh) || num(row.pv_kwh)) ? row : null,
          future: day > today,
        });
      }
      totals = sumRows((energy.days || []).filter((d) => num(d.pv_kwh) || num(d.import_kwh)));
    }
    // Money for each slot, for the hover details.
    const costRows = (costs && (grouped ? costs.buckets : costs.days)) || [];
    const costBy = new Map(costRows.map((c) => [grouped ? c.period : c.date, c]));
    for (const s of slots) s.cost = costBy.get(s.key) || null;
    const rows = slots.filter((s) => s.row);
    if (!rows.length) {
      return { nodes: [SD.h('div', { class: 'card history-empty' }, SD.h('p', { text: 'No data for this period.' }))] };
    }

    const nodes = [summaryCard(r, totals, costs && costs.totals, tariff, slots), tiles(r, totals, costs && costs.totals, prevEnergy && prevEnergy.totals, rows)];
    const box = nodes[0].querySelector('.history-chart');
    return { nodes, redraw: () => drawBars(box, r, slots) };
  }

  /** Adds up day rows of /api/daily. */
  function sumRows(days) {
    const keys = ['pv_kwh', 'load_kwh', 'import_kwh', 'export_kwh', 'import_free_kwh', 'import_shoulder_kwh', 'import_peak_kwh'];
    const out = { days: days.length };
    for (const k of keys) {
      const have = days.filter((d) => num(d[k]));
      out[k] = have.length ? sum(have.map((d) => d[k])) : null;
    }
    return out;
  }

  function summaryCard(r, totals, costs, tariff, slots) {
    const today = SD.local().date;
    const { from, fullTo } = rangeOf(r);
    const name = r.view === 'week' ? 'This week' : r.view === 'month' ? SD.fmt.date(`${r.key}-01`, 'month').split(' ')[0] : r.view === 'year' ? r.key : 'All time';
    const open = from <= today && today <= fullTo && r.view !== 'all';
    const lead = open ? (r.view === 'week' ? 'This week so far' : `${name} so far`) : r.view === 'week' ? `Week of ${SD.fmt.date(r.key, 'short')}` : name;
    let text = null;
    if (costs && num(costs.actual)) {
      const power = costs.actual < 0 ? `${money(-costs.actual)} back from power` : `${money(costs.actual)} of power`;
      text = `${lead}: ${power}${num(costs.savings) && costs.savings > 0 ? `, ${money(costs.savings).replace(/\.00$/, '')} saved` : ''}.`;
    }
    const intro = { week: 'Each bar is a day', month: 'Each bar is a day', year: 'Each bar is a month', all: 'Each bar is a year' }[r.view];
    const link = r.view === 'all' ? 'Tap a bar to open that year.' : r.view === 'year' ? 'Tap a bar to open that month.' : 'Tap a bar to open that day.';
    const chartBox = SD.h('div', { class: 'history-chart' });
    return SD.h('section', { class: 'card history-card history-period', 'aria-labelledby': 'history-sum' },
      SD.h('h2', { class: 'sr-only', id: 'history-sum', text: `${label(r)} summary` }),
      SD.h('p', { class: 'history-sentence' },
        text ? SD.h('b', { text: `${text} ` }) : null,
        `${intro}: what you bought, coloured by what it cost. ${link}`),
      chartBox,
      legendFor(r, tariff, slots));
  }

  function legendFor(r, tariff, slots) {
    const prices = tariff && tariff.configured && r.view !== 'year' && r.view !== 'all' ? bandCents(tariff) : {};
    const items = BANDS.map((b) => SD.h('span', {}, bandSwatch(b), b in prices ? `${BAND_NAME[b]}, ${SD.fmt.cents(prices[b])}` : BAND_NAME[b]));
    const unsplit = slots.some((s) => s.row && !BANDS.some((b) => num(s.row[`import_${b}_kwh`])));
    if (unsplit) items.push(SD.h('span', {}, SD.h('span', { class: 'swatch history-sw-plain', 'aria-hidden': 'true' }), 'Not split by price'));
    items.push(SD.h('span', { class: 'history-muted', text: 'Bar height is kWh bought' }));
    return SD.h('p', { class: 'history-legend' }, items);
  }

  function tiles(r, totals, costs, prevTotals, rows) {
    const out = SD.h('div', { class: 'history-tiles' });
    const unit = { week: 'day', month: 'day', year: 'month', all: 'year' }[r.view];
    const bestOf = (key) => rows.reduce((best, s) => (num(s.row[key]) && (!best || s.row[key] > best.row[key]) ? s : best), null);

    if (num(totals.pv_kwh)) {
      const best = bestOf('pv_kwh');
      out.append(tile('Solar made', SD.fmt.kwh(totals.pv_kwh, totals.pv_kwh >= 100 ? 0 : 1),
        best ? `Best ${unit} ${SD.fmt.kwh(best.row.pv_kwh)}, ${best.long}` : null));
    }
    if (num(totals.import_kwh)) {
      const split = [totals.import_free_kwh, totals.import_shoulder_kwh, totals.import_peak_kwh];
      const free = num(totals.import_free_kwh) && totals.import_kwh > 0 && split.some(num)
        ? `${Math.round((totals.import_free_kwh / totals.import_kwh) * 100)}% in the free window` : null;
      out.append(tile('Bought', SD.fmt.kwh(totals.import_kwh, totals.import_kwh >= 100 ? 0 : 1), free));
    }
    if (num(totals.export_kwh)) {
      const bits = [];
      if (costs && num(costs.feed_in)) bits.push(`${money(costs.feed_in)} earned`);
      if (costs && costs.credit > 0) bits.push(`plus ${money(costs.credit).replace(/\.00$/, '')} in credits`);
      out.append(tile('Sold', SD.fmt.kwh(totals.export_kwh, totals.export_kwh >= 100 ? 0 : 1), bits.join(', ') || null));
    }
    if (num(totals.load_kwh) && totals.load_kwh > 0 && num(totals.import_kwh)) {
      const own = (t) => (t && num(t.load_kwh) && t.load_kwh > 0 && num(t.import_kwh) ? Math.round(Math.max(0, 1 - t.import_kwh / t.load_kwh) * 100) : null);
      const now = own(totals);
      const before = own(prevTotals);
      let name = null;
      if (r.view === 'week') name = 'Last week';
      else if (r.view === 'month') name = SD.fmt.date(`${addMonths(r.key, -1)}-01`, 'month').split(' ')[0];
      else if (r.view === 'year') name = String(+r.key - 1);
      out.append(tile('Ran on own power', `${now}%`, before != null && name ? `${name} was ${before}%` : null));
    }
    return out;
  }

  /* ---------------------------------------------------------------- the bar chart */

  function drawBars(box, r, slots) {
    const width = Math.max(box.clientWidth || 300, 200);
    const height = width >= 560 ? 230 : 170;
    const top = 16;
    const axisH = 20;
    const base = height - axisH;
    const n = slots.length;
    const slotW = width / n;
    const barW = Math.max(2, Math.min(slotW * 0.72, 36));
    const total = (s) => (s.row ? (num(s.row.import_kwh) ? s.row.import_kwh : sum(BANDS.map((b) => s.row[`import_${b}_kwh`]))) : 0);
    const max = niceMax(Math.max(1, ...slots.map(total)));
    const y = (v) => base - (v / max) * (base - top);

    const sumTotal = sum(slots.map(total));
    const label0 = { week: 'Day', month: 'Day', year: 'Month', all: 'Year' }[r.view];
    const svg = S('svg', {
      width, height, viewBox: `0 0 ${width} ${height}`, role: 'group',
      'aria-label': `Energy bought from the grid, one bar per ${label0.toLowerCase()}, ${SD.fmt.kwh(sumTotal)} in all. Each bar links to that ${label0.toLowerCase()}.`,
    });
    for (const frac of [0.5, 1]) {
      const yy = y(max * frac);
      svg.append(S('line', { x1: 0, x2: width, y1: yy, y2: yy, class: 'history-grid' }));
      svg.append(S('text', { x: 2, y: yy - 3, class: 'history-axis-label' }, `${+(max * frac).toFixed(1)} kWh`));
    }
    svg.append(S('line', { x1: 0, x2: width, y1: base, y2: base, class: 'history-base' }));

    // Which x labels fit: every one on a week, year or short list, else every few.
    const every = n <= 12 ? 1 : n <= 16 ? 2 : 5;
    slots.forEach((s, i) => {
      const cx = slotW * i + slotW / 2;
      const bx = cx - barW / 2;
      if (i % every === 0 || (every > 1 && i === n - 1 && i % every >= 3)) {
        svg.append(S('text', { x: cx, y: height - 5, 'text-anchor': 'middle', class: `history-axis-label${s.future ? ' is-future' : ''}` }, s.text));
      }
      const parts = [];
      if (s.row) {
        const split = BANDS.map((b) => [b, s.row[`import_${b}_kwh`]]);
        if (split.some(([, v]) => num(v))) {
          let acc = 0;
          for (const [b, v] of split) {
            if (!num(v) || v <= 0) continue;
            parts.push(S('rect', { x: bx, y: y(acc + v), width: barW, height: Math.max(0.5, y(acc) - y(acc + v)), class: `history-bar history-bar-${b}` }));
            acc += v;
          }
        } else if (total(s) > 0) {
          parts.push(S('rect', { x: bx, y: y(total(s)), width: barW, height: y(0) - y(total(s)), class: 'history-bar history-bar-plain' }));
        } else {
          parts.push(S('rect', { x: bx, y: base - 1.5, width: barW, height: 1.5, class: 'history-bar history-bar-free' }));
        }
      }
      const said = s.row ? `${s.long}: ${SD.fmt.kwh(total(s))} bought` : `${s.long}: no data`;
      // The link covers the whole column, so a thin bar is still easy to hit.
      const hit = S('rect', { x: slotW * i, y: 0, width: slotW, height: height - axisH, class: 'history-hit' });
      if (s.row || !s.future) {
        svg.append(S('a', { href: `#${s.href}`, 'aria-label': said, class: 'history-bar-link' }, S('title', {}, said), hit, parts));
      } else svg.append(S('g', {}, parts));
    });
    box.replaceChildren(svg);
    // Hover (mouse only: a tap on a bar opens that period): bought by band, sold, and the money.
    chartTip(box, svg, base, (frac) => {
      const i = Math.floor(frac * n);
      const s = slots[i];
      if (!s || !s.row) return null;
      const nodes = [SD.h('b', { text: s.long })];
      const split = BANDS.filter((b) => num(s.row[`import_${b}_kwh`]) && s.row[`import_${b}_kwh`] >= 0.05)
        .map((b) => `${BAND_NAME[b].toLowerCase()} ${SD.fmt.kwh(s.row[`import_${b}_kwh`])}`);
      nodes.push(tipLine(`Bought ${SD.fmt.kwh(total(s))}${split.length ? ` (${split.join(', ')})` : ''}`));
      if (s.cost && num(s.cost.import)) nodes.push(tipLine(`Cost ${money(s.cost.import)}`));
      if (num(s.row.export_kwh)) {
        const earned = s.cost && num(s.cost.feed_in) ? `, earned ${money(s.cost.feed_in)}` : '';
        nodes.push(tipLine(`Sold ${SD.fmt.kwh(s.row.export_kwh)}${earned}`));
      }
      return { at: (i + 0.5) / n, nodes };
    }, { touch: false });
  }

  SD.page('history', { mount, update, unmount });
})();
