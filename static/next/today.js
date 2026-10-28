/* Today page: what is today costing, and is tonight's $1 safe?

   One function per section, top to bottom on a phone (head, credit window, grid pull,
   right now, your day so far, coming up, evening credit, totals). Each builds a
   section element once and returns {el, update}; update() redraws it from `data`.

   Live parts (head, credit window, grid pull, right now, coming up) redraw on every
   SD.live tick. Slower data (today's cost and totals, the minute series for the day
   chart, the credit history) is fetched again every 5 minutes.

   Everything shown comes from the API; when a field is missing the element hides. */
(function () {
  'use strict';

  const SLOW_MS = 5 * 60 * 1000;
  const FLOW_MIN_W = 20; // below this a flow counts as idle (as on the old home page)
  const NUDGE_MIN_W = 500; // selling less than this is not worth a nudge
  const METER_MIN_W = 5000; // above this the grid pull meter shows even without a car
  const PULL_VOLTS = 240; // /api/live volts is null for now, so amps are worked out at this
  const PULL_SERIES_MS = 30 * 1000; // how often the last-30-minutes line is fetched
  const NIGHTS = 14; // nights in the evening credit strip

  const h = SD.h;
  const kwOf = (w) => SD.fmt.kw(w / 1000);
  const pos = (w) => Math.max(w == null ? 0 : w, 0);
  const HM = (s) => { const [a, b] = s.split(':'); return Number(a) * 60 + Number(b); };
  const inRange = (from, to, m) => (from <= to ? m >= from && m < to : m >= from || m < to);
  const SVGNS = 'http://www.w3.org/2000/svg';
  function svg(tag, attrs) {
    const e = document.createElementNS(SVGNS, tag);
    for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, v);
    return e;
  }

  // Set while the page is mounted.
  let view = null;
  const data = {};

  /* ---------------------------------------------------------------- small helpers */

  /** Minutes from `a` forward to `b` on the clock (0..1439). */
  const minsTo = (a, b) => (b - a + 1440) % 1440;

  /** "$1" for 100 cents, "50c" otherwise. */
  function creditText(cw) {
    const c = cw.creditCents;
    return c % 100 === 0 ? `$${c / 100}` : SD.fmt.cents(c);
  }

  /** "6 to 9 pm", "11 am to 2 pm". */
  function windowText(cw) {
    const a = SD.fmt.minute(cw.start);
    const b = SD.fmt.minute(cw.end);
    const sfx = (s) => s.slice(s.lastIndexOf(' '));
    return sfx(a) === sfx(b) ? `${a.slice(0, a.lastIndexOf(' '))} to ${b}` : `${a} to ${b}`;
  }

  /** "Shoulder · 44c", "Free · 0c" for the price now. */
  function bandLabel(minute) {
    const band = SD.tariff.bandAt(data.tariff, minute);
    if (!band) return null;
    const name = { free: 'Free', shoulder: 'Shoulder', peak: 'Peak' }[band];
    return `${name} · ${SD.fmt.cents(SD.tariff.centsAt(data.tariff, minute))}`;
  }

  /** Export price (cents) at a minute of day, from the plan's export windows. */
  function exportCentsAt(t, minute) {
    const w = (t.export || []).find((x) => inRange(HM(x.from), HM(x.to), minute));
    return w ? w.cents : t.default_export_cents;
  }

  function liveParts() {
    const live = data.live || {};
    return { sample: live.sample || null, car: live.car || null, pull: live.grid_pull || null };
  }

  const isCharging = (car) => !!car && car.charging_state === 'Charging';
  const isPlugged = (car) => !!car && (car.plugged_in === true || isCharging(car));
  const carName = (car) => (car && car.display_name) || 'The car';

  /** The credit window now: {cw, inside, elapsed, length, left} (minutes), or null without a plan. */
  function windowNow() {
    const cw = data.tariff && SD.tariff.creditWindow(data.tariff);
    if (!cw) return null;
    const m = SD.local().minute;
    const inside = inRange(cw.start, cw.end, m);
    const length = minsTo(cw.start, cw.end) || 1440;
    const elapsed = inside ? minsTo(cw.start, m) : 0;
    return { cw, inside, elapsed, length, left: length - elapsed };
  }

  /** Energy bought in the credit window so far today, from the minute samples:
      {kwh, crossedAt} (crossedAt = when the total first passed the credit's limit), or null. */
  function windowImport(win) {
    if (!data.samples || !win) return null;
    const { cw } = win;
    let kwh = 0;
    let crossedAt = null;
    for (const s of data.samples) {
      if (s.grid <= 0 || !inRange(cw.start, cw.end, s.minute)) continue;
      kwh += (s.grid * s.hours) / 1000;
      if (crossedAt == null && kwh > cw.maxKwh) crossedAt = s.ts;
    }
    return { kwh, crossedAt };
  }

  /** What the battery usually uses in the window, and the reserve it keeps (both %). */
  function batteryTypical() {
    const sum = data.battery && data.battery.summary;
    if (!sum || sum.typical_window_soc_used_pct == null) return null;
    return { used: sum.typical_window_soc_used_pct, floor: sum.battery_floor_pct || 0 };
  }

  /* ---------------------------------------------------------------- 1. head and verdict */

  /** Today's power cost in dollars (bought less sold; the supply charge and credit are separate), or null. */
  function powerCost() {
    const d = data.costDay;
    if (!d || d.import == null || d.feed_in == null) return null;
    return d.import - d.feed_in;
  }

  function supplyDollars() {
    const d = data.costDay;
    if (d && d.supply != null) return d.supply;
    return data.tariff && data.tariff.configured ? data.tariff.supply_cents_per_day / 100 : null;
  }

  /** The verdict sentence and the paragraph under it, from the state of the day. */
  function verdict() {
    const { sample, car, pull } = liveParts();
    const win = windowNow();
    const soc = sample && sample.soc_pct != null ? Math.round(sample.soc_pct) : null;
    const typ = batteryTypical();
    const imp = windowImport(win);
    const credit = win ? creditText(win.cw) : '$1';
    const cost = powerCost();
    const costSentence = cost == null ? null : cost >= 0
      ? `Today's power has cost ${SD.fmt.cents(Math.round(cost * 100))} so far.`
      : `Selling has earned ${SD.fmt.cents(Math.round(-cost * 100))} more than buying so far.`;
    const supply = supplyDollars();

    // (a) Close to the main breaker: the headline is about headroom.
    if (pull && pull.now_w != null && pull.warn_w != null && pull.now_w >= pull.warn_w) {
      const spare = pull.limit_w - pull.now_w;
      const text = spare >= 0
        ? `Pulling ${kwOf(pull.now_w)}. ${kwOf(spare)} to spare.`
        : `Pulling ${kwOf(pull.now_w)}. ${kwOf(-spare)} over your limit.`;
      const bits = [];
      if (isCharging(car) && car.charger_power_kw != null) bits.push(`${carName(car)} is charging at ${SD.fmt.kw(car.charger_power_kw, 0)}`);
      if (sample && sample.battery_w >= FLOW_MIN_W) bits.push('the battery is filling');
      const free = data.tariff && SD.tariff.bandAt(data.tariff, SD.local().minute) === 'free';
      let support = bits.length ? `${bits.join(' and ')}${free ? ', all on free power' : ''}.` : '';
      support = support.charAt(0).toUpperCase() + support.slice(1);
      return { text, support };
    }

    // (b) Inside the credit window: the credit is the story.
    if (win && win.inside && imp) {
      if (imp.kwh <= win.cw.maxKwh) {
        const left = batteryLeftText(soc, typ, win);
        const since = SD.fmt.minute(win.cw.start);
        return { text: `On track for tonight's ${credit}.`, support: `${imp.kwh < 0.005 ? `No power bought since ${since}.` : `${SD.fmt.kwh(imp.kwh, 2)} bought since ${since}.`}${left ? ` ${left}` : ''}` };
      }
      const when = imp.crossedAt ? ` bought at ${SD.fmt.time(imp.crossedAt)}` : ' bought';
      const rest = [costSentence, supply == null ? null : `Plus the ${SD.fmt.dollars(supply)} supply charge.`].filter(Boolean).join(' ');
      return { text: `Tonight's ${credit} is gone: ${SD.fmt.kwh(imp.kwh)}${when}.`, support: rest };
    }

    // (c) Everything else: what the day has cost.
    const parts = [];
    if (supply != null && cost != null) parts.push(`Plus the ${SD.fmt.dollars(supply)} supply charge.`);
    const tail = creditOutlook(win, soc, typ);
    if (tail) parts.push(tail);
    if (cost == null) return { text: 'Here is how today is going.', support: parts.join(' ') };
    const text = cost >= 0
      ? `Today's power has cost ${SD.fmt.cents(Math.round(cost * 100))}.`
      : `Today's power has earned ${SD.fmt.cents(Math.round(-cost * 100))}.`;
    return { text, support: parts.join(' ') };
  }

  /** "The battery has 71% and the rest of the evening usually needs about 22%." */
  function batteryLeftText(soc, typ, win) {
    if (soc == null) return '';
    if (!typ) return `The battery has ${soc}%.`;
    const need = Math.round(typ.used * (win.left / win.length));
    return `The battery has ${soc}% and the rest of the evening usually needs about ${need}%.`;
  }

  /** The sentence about tonight's credit for the verdict paragraph. */
  function creditOutlook(win, soc, typ) {
    if (!win) return '';
    const credit = creditText(win.cw);
    const m = SD.local().minute;
    // After the window: say how it went.
    if (!win.inside && win.cw.start < win.cw.end && m >= win.cw.end) {
      const today = data.battery && data.battery.days && data.battery.days.find((d) => d.date === SD.local().date);
      if (today && today.credit === 'earned') return `Tonight's ${credit} credit was earned.`;
      if (today && today.credit === 'missed') return `Tonight's ${credit} credit was missed.`;
      return '';
    }
    if (soc == null || !typ) return '';
    if (soc >= typ.used + typ.floor) return `The battery is at ${soc}% and ready for tonight, so the ${credit} evening credit looks safe.`;
    return `The battery is at ${soc}% and tonight usually needs about ${Math.round(typ.used)}%, so the ${credit} evening credit is not certain.`;
  }

  function headSection() {
    const dateLine = h('p', { class: 'date-line' });
    const title = h('h1', { class: 'verdict' });
    const lede = h('p', { class: 'lede today-lede' });
    const el = h('header', { class: 'page-head' }, dateLine, title, lede);
    return {
      el,
      update() {
        const now = SD.local();
        let line = `${SD.fmt.date(now.date, 'long')} · ${SD.fmt.time(new Date())}`;
        if (data.tariff && SD.tariff.bandAt(data.tariff, now.minute) === 'free') {
          const seg = SD.tariff.segments(data.tariff).find((s) => now.minute >= s.start && now.minute < s.end);
          if (seg) line += ` · free until ${SD.fmt.minute(seg.end % 1440)}`;
        }
        dateLine.textContent = line;
        const v = verdict();
        title.textContent = v.text;
        lede.textContent = v.support;
        lede.hidden = !v.support;
      },
    };
  }

  /* ---------------------------------------------------------------- 2. credit window card */

  function windowSection() {
    const el = h('section', { class: 'card card-accent today-window', 'aria-labelledby': 'today-window-h' });
    return {
      el,
      update() {
        const win = windowNow();
        el.hidden = !(win && win.inside);
        if (el.hidden) return;
        const { sample } = liveParts();
        const { cw } = win;
        const imp = windowImport(win);
        const soc = sample && sample.soc_pct != null ? sample.soc_pct : null;
        const typ = batteryTypical();
        const pct = Math.min(100, (win.elapsed / win.length) * 100);
        const mins = Math.max(0, Math.round(win.left));
        const togo = mins >= 60 ? `${Math.floor(mins / 60)} h ${mins % 60} m to go` : `${mins} m to go`;
        const figs = [];
        if (imp) figs.push(fig('Bought', SD.fmt.kwh(imp.kwh, 2)));
        if (soc != null) figs.push(fig('Battery', `${Math.round(soc)}%`, 'is-battery'));
        if (soc != null && typ) {
          const spare = Math.max(0, soc - typ.used * (win.left / win.length));
          figs.push(fig(`Spare at ${SD.fmt.minute(cw.end)}`, `~${Math.round(spare)}%`));
        }
        fill(el,
          h('div', { class: 'card-head' },
            h('h2', { class: 'section-title', id: 'today-window-h', text: `Credit window, ${windowText(cw)}` }),
            h('span', { class: 'label', text: togo })),
          h('div', { class: 'today-progress', role: 'img', 'aria-label': `${Math.round(pct)}% of the credit window has passed, ${togo}` },
            h('div', { class: 'today-progress-fill', style: `width: ${pct}%` }),
            h('div', { class: 'today-progress-now', style: `left: ${pct}%` })),
          h('div', { class: 'today-axis', 'aria-hidden': 'true' },
            h('span', { text: SD.fmt.minute(cw.start) }),
            h('span', { text: SD.fmt.minute((cw.start + Math.round(win.length / 2)) % 1440) }),
            h('span', { text: SD.fmt.minute(cw.end) })),
          figs.length ? h('div', { class: 'today-figs' }, figs) : null);
      },
    };
  }

  /** Replaces an element's children, skipping the null ones (replaceChildren would print "null"). */
  function fill(el, ...kids) {
    el.replaceChildren(...kids.filter(Boolean));
  }

  /** A label above a figure. */
  function fig(label, value, cls) {
    return h('div', { class: 'today-fig' },
      h('div', { class: 'label', text: label }),
      h('div', { class: `fig fig-md ${cls || ''}`, text: value }));
  }

  /* ---------------------------------------------------------------- 3. grid pull */

  /** Which of the three note-n11 states applies: 'none', 'quiet', 'meter' or 'big'. */
  function pullState() {
    const { pull, car } = liveParts();
    if (!pull) return 'none';
    const now = pull.now_w;
    if (now != null && pull.warn_w != null && now >= pull.warn_w) return 'big';
    if (now != null && (isCharging(car) || now > METER_MIN_W)) return 'meter';
    return pull.peak_today_w != null || now != null ? 'quiet' : 'none';
  }

  /** The bar: a fill for the pull, a hatched warning zone and a line at the limit. */
  function pullMeter(pull, compact) {
    const max = pull.limit_w * (16 / 15); // leaves room past the limit line
    const pct = (w) => `${Math.min(100, (w / max) * 100)}%`;
    const now = pull.now_w;
    const spare = pull.limit_w - now;
    const label = `Grid pull ${SD.fmt.kw(now / 1000)}, ${now >= pull.warn_w ? 'in the warning zone, ' : ''}${spare >= 0 ? `${SD.fmt.kw(spare / 1000)} below` : `${SD.fmt.kw(-spare / 1000)} over`} the ${SD.fmt.kw(pull.limit_w / 1000, 0)} limit`;
    const bar = h('div', { class: `today-meter${compact ? ' is-compact' : ''}`, role: 'img', 'aria-label': label },
      h('div', { class: 'today-meter-warn', style: `left: ${pct(pull.warn_w)}; width: calc(${pct(pull.limit_w)} - ${pct(pull.warn_w)})` }),
      h('div', { class: 'today-meter-fill', style: `width: ${pct(now)}` }),
      h('div', { class: 'today-meter-limit', style: `left: ${pct(pull.limit_w)}` }));
    if (compact) return bar;
    const tick = (w, cls) => h('span', { class: cls, style: `left: ${pct(w)}`, text: String(Math.round(w / 1000)) });
    return h('div', {}, bar,
      h('div', { class: 'today-meter-ticks', 'aria-hidden': 'true' }, tick(0, ''), tick(pull.warn_w, ''), tick(pull.limit_w, 'is-limit')));
  }

  /** Grid pull over the last 30 minutes, with the warning zone and the limit line. */
  function pullSpark(pull, rows) {
    const W = 300;
    const H = 70;
    const ymax = Math.max(pull.limit_w * (16 / 15), ...rows.map((r) => r[1]));
    const y = (w) => (H - (w / ymax) * H).toFixed(1);
    const t1 = Math.floor(Date.now() / 1000);
    const t0 = t1 - 1800;
    const pts = rows.map(([t, w]) => `${(((t - t0) / 1800) * W).toFixed(1)},${y(w)}`);
    const peak = Math.max(...rows.map((r) => r[1]));
    const s = svg('svg', { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none', class: 'today-spark', role: 'img',
      'aria-label': `Grid pull over the last 30 minutes, highest ${SD.fmt.kw(peak / 1000)}, now ${SD.fmt.kw(pull.now_w / 1000)}` });
    s.append(
      svg('rect', { class: 'today-spark-warn', x: 0, y: y(pull.limit_w), width: W, height: Math.max(0, y(pull.warn_w) - y(pull.limit_w)) }),
      svg('line', { class: 'today-spark-limit', x1: 0, x2: W, y1: y(pull.limit_w), y2: y(pull.limit_w) }),
      svg('line', { class: 'today-spark-base', x1: 0, x2: W, y1: H, y2: H }),
      svg('polyline', { class: 'today-spark-line', points: pts.join(' ') }));
    return s;
  }

  /** "Highest today 13.9 kW at 12:06". */
  function peakText(pull) {
    return pull.peak_today_w == null ? null : `Highest today ${kwOf(pull.peak_today_w)}${pull.peak_today_at ? ` at ${SD.fmt.time(pull.peak_today_at)}` : ''}`;
  }

  /** Fetches the last 30 minutes of grid pull, at most every PULL_SERIES_MS. */
  function refreshPullSeries() {
    const nowMs = Date.now();
    if (nowMs - (data.pullAt || 0) < PULL_SERIES_MS) return;
    data.pullAt = nowMs;
    const to = Math.floor(nowMs / 1000);
    SD.fetchJSON(`/api/live/series?from=${to - 1800}&to=${to}`).then((r) => {
      if (!view) return;
      data.pull30 = (r.data || []).filter((row) => row[3] != null).map((row) => [row[0], pos(row[3])]);
      view.sections.pull.update();
    }).catch(() => { data.pullAt = 0; });
  }

  function pullSection() {
    const el = h('section', { class: 'card today-pull', 'aria-labelledby': 'today-pull-h' });
    return {
      el,
      update() {
        const st = pullState();
        el.hidden = st !== 'big';
        if (el.hidden) return;
        const { pull } = liveParts();
        refreshPullSeries();
        const rows = (data.pull30 || []).slice();
        rows.push([Math.floor(Date.now() / 1000), pull.now_w]); // the reading on screen is the last point
        const peak = peakText(pull);
        fill(el,
          h('div', { class: 'card-head' },
            h('h2', { class: 'section-title', id: 'today-pull-h', text: 'Grid pull' }),
            h('span', { class: 'label', text: `Your limit ${SD.fmt.kw(pull.limit_w / 1000, 0)}` })),
          h('div', { class: 'today-pull-now' },
            h('span', { class: 'fig today-pull-kw', text: kwOf(pull.now_w) }),
            h('span', { class: 'today-pull-amps', text: `about ${Math.round(pull.now_w / (pull.volts || PULL_VOLTS))} A` })),
          pullMeter(pull, false),
          rows.length > 1 ? h('div', { class: 'today-pull-recent' },
            h('div', { class: 'label', text: 'Last 30 minutes' }),
            pullSpark(pull, rows),
            h('div', { class: 'today-axis', 'aria-hidden': 'true' },
              h('span', { text: SD.fmt.time(Math.floor(Date.now() / 1000) - 1800) }),
              h('span', { text: 'Now' }))) : null,
          peak ? h('p', { class: 'today-pull-peak', text: peak }) : null);
      },
    };
  }

  /* ---------------------------------------------------------------- 4. right now */

  /** Where power comes from and where it goes, in watts, with the car split out of the home load. */
  function flows(s, car) {
    const load = pos(s.load_w);
    const carW = isCharging(car) && car.charger_power_kw != null ? Math.min(car.charger_power_kw * 1000, load) : 0;
    const from = [
      { kind: 'solar', label: 'Solar', w: pos(s.pv_w) },
      { kind: 'battery', label: 'Battery', w: pos(-s.battery_w) },
      { kind: 'grid-in', label: 'Grid', w: pos(s.grid_w) },
    ];
    const to = [
      { kind: 'home', label: 'Home', w: load - carW },
      { kind: 'car', label: car && car.display_name ? car.display_name : 'Car', w: carW },
      { kind: 'battery', label: 'Battery', w: pos(s.battery_w) },
      { kind: 'grid-out', label: 'Grid', w: pos(-s.grid_w) },
    ];
    return { from: from.filter((x) => x.w >= FLOW_MIN_W), to: to.filter((x) => x.w >= FLOW_MIN_W) };
  }

  /** One balance bar. Segments too narrow for words are listed under it instead. */
  function balanceBar(title, list) {
    const total = list.reduce((a, x) => a + x.w, 0);
    const small = [];
    const segs = list.map((x) => {
      const share = total ? x.w / total : 0;
      const text = share >= 0.3 ? `${x.label} ${kwOf(x.w)}` : share >= 0.16 ? x.label : null;
      if (!text) small.push(`${x.label} ${kwOf(x.w)}`);
      return h('div', { class: `today-seg is-${x.kind}`, style: `flex: ${x.w} 1 0`, text });
    });
    const spoken = list.map((x) => `${x.label} ${kwOf(x.w)}`).join(', ');
    return h('div', { class: 'today-balance' },
      h('span', { class: 'label', text: title }),
      total ? h('div', { class: 'today-bar', role: 'img', 'aria-label': `${title}: ${spoken}` }, segs)
        : h('div', { class: 'today-bar is-empty', role: 'img', 'aria-label': `${title}: nothing` }, h('span', { text: 'Nothing right now' })),
      small.length ? h('span', { class: 'label', text: small.join(' · ') }) : null);
  }

  /** The 0c export nudge text, or null when it does not apply. */
  function nudgeText(s) {
    if (!data.tariff || !data.tariff.configured || s.grid_w == null) return null;
    const sellingW = -s.grid_w;
    const m = SD.local().minute;
    if (sellingW < NUDGE_MIN_W || exportCentsAt(data.tariff, m) > 0) return null;
    // When does selling stop being worth 0c?
    let until = null;
    for (let t = m + 1; t < 1440; t++) {
      if (exportCentsAt(data.tariff, t) > 0) { until = t; break; }
    }
    return `You're selling ${kwOf(sellingW)} for 0c${until == null ? '' : ` until ${SD.fmt.minute(until)}`}. Good time for the dishwasher or a load of washing.`;
  }

  function carLine(car) {
    if (!isPlugged(car) || isCharging(car)) return null;
    const state = car.charging_state === 'Complete' ? 'charge complete' : 'plugged in, not charging';
    const pct = car.battery_level_pct == null ? '' : ` · ${Math.round(car.battery_level_pct)}%`;
    return h('p', { class: 'today-car' }, h('b', { text: carName(car) }), ` ${state}${pct}`);
  }

  function nowSection() {
    const el = h('section', { class: 'card today-now', 'aria-labelledby': 'today-now-h' });
    return {
      el,
      update() {
        const { sample, car, pull } = liveParts();
        const head = h('div', { class: 'card-head' },
          h('h2', { class: 'section-title', id: 'today-now-h', text: 'Right now' }),
          h('span', { class: 'card-sub', text: data.tariff ? bandLabel(SD.local().minute) : null }));
        if (!sample) {
          el.replaceChildren(head, h('p', { class: 'muted today-gap', text: 'No live reading from the inverter right now.' }));
          return;
        }
        const f = flows(sample, car);
        const idle = (w) => w == null || Math.abs(w) < FLOW_MIN_W;
        const bat = sample.battery_w;
        const grid = sample.grid_w;
        const st = pullState();
        let pullBlock = null;
        if (st === 'meter') {
          pullBlock = h('div', { class: 'today-pull-inline' },
            h('div', { class: 'card-head' },
              h('span', { class: 'today-sub-title', text: 'Grid pull' }),
              h('span', { class: 'label', text: `Your limit ${SD.fmt.kw(pull.limit_w / 1000, 0)}` })),
            h('p', { class: 'today-pull-line' }, h('b', { text: `${kwOf(pull.now_w)} now` }), peakText(pull) ? `. ${peakText(pull)}.` : '.'),
            pullMeter(pull, true));
        } else if (st === 'quiet') {
          const bits = [];
          if (pull.now_w != null) bits.push(h('b', { text: `${SD.fmt.kw(pull.now_w / 1000, pull.now_w < 100 ? 0 : 1)} now` }), grid != null && grid < -FLOW_MIN_W ? ", you're selling" : '');
          const peak = peakText(pull);
          if (peak) {
            let tail = '';
            if (pull.limit_w != null && pull.peak_today_w != null) {
              const gap = pull.limit_w - pull.peak_today_w;
              tail = gap >= 0 ? `, ${kwOf(gap)} under your limit` : `, ${kwOf(-gap)} over your limit`;
            }
            bits.push(`${bits.length ? '. ' : ''}${peak}${tail}.`);
          }
          pullBlock = h('div', { class: 'today-pull-inline' },
            h('div', { class: 'card-head' },
              h('span', { class: 'today-sub-title', text: 'Grid pull' }),
              pull.limit_w != null ? h('span', { class: 'label', text: `Your limit ${SD.fmt.kw(pull.limit_w / 1000, 0)}` }) : null),
            h('p', { class: 'today-pull-line' }, bits));
        }
        const nudge = nudgeText(sample);
        fill(el,
          head,
          h('div', { class: 'today-balances' },
            balanceBar('Coming from', f.from),
            balanceBar('Going to', f.to)),
          h('div', { class: 'today-figs' },
            fig('Home', kwOf(pos(sample.load_w))),
            fig(sample.soc_pct == null ? 'Battery' : `Battery ${Math.round(sample.soc_pct)}%`,
              idle(bat) ? 'Idle' : `${bat > 0 ? '+' : '-'}${kwOf(Math.abs(bat))}`, 'is-battery'),
            fig(idle(grid) ? 'Grid' : grid < 0 ? 'Grid, selling' : 'Grid, buying', idle(grid) ? 'Idle' : kwOf(Math.abs(grid)))),
          carLine(car),
          pullBlock,
          nudge ? h('div', { class: 'today-nudge' },
            nudgeIcon(),
            h('span', { text: nudge })) : null);
      },
    };
  }

  function nudgeIcon() {
    const s = svg('svg', { width: 20, height: 20, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', 'stroke-width': 1.6, 'aria-hidden': 'true' });
    s.append(svg('path', { d: 'M9 18h6M10 21h4M12 3a6 6 0 0 0-3.5 10.9c.6.4 1 1.1 1 1.8V16h5v-.3c0-.7.4-1.4 1-1.8A6 6 0 0 0 12 3z' }));
    return s;
  }

  /* ---------------------------------------------------------------- 5. your day so far */

  /** Hourly kWh bought and sold from the day's samples. */
  function hourly() {
    const bought = new Array(24).fill(0);
    const sold = new Array(24).fill(0);
    for (const s of data.samples || []) {
      const kwh = (Math.abs(s.grid) * s.hours) / 1000;
      if (s.grid > 0) bought[s.hour] += kwh;
      else sold[s.hour] += kwh;
    }
    return { bought, sold };
  }

  /** The chart: sold as outlined bars above the line, bought below it filled with the hour's band colour. */
  function dayChart(hrs, nowMinute) {
    const W = 360;
    const H = 120;
    const mid = H / 2;
    const slot = W / 24;
    const maxKwh = Math.max(...hrs.bought, ...hrs.sold, 0.01);
    const per = Math.min(12, (mid - 6) / maxKwh); // pixels per kWh
    const s = svg('svg', { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none', class: 'today-chart', role: 'img',
      'aria-label': dayChartLabel(hrs) });
    const g = svg('g');
    for (let hr = 0; hr < 24; hr++) {
      const x = (hr * slot + 2).toFixed(1);
      const bw = (slot - 4).toFixed(1);
      if (hrs.sold[hr] > 0.005) {
        const ht = Math.max(1, hrs.sold[hr] * per);
        g.append(svg('rect', { class: 'today-sold', x, y: (mid - ht).toFixed(1), width: bw, height: ht.toFixed(1) }));
      }
      if (hrs.bought[hr] > 0.005) {
        const band = (SD.tariff.bandAt(data.tariff, hr * 60 + 30) || 'shoulder');
        g.append(svg('rect', { class: `today-bought is-${band}`, x, y: mid, width: bw, height: Math.max(1, hrs.bought[hr] * per).toFixed(1) }));
      }
    }
    s.append(svg('line', { class: 'today-chart-axis', x1: 0, x2: W, y1: mid, y2: mid }), g);
    if (nowMinute != null) {
      const nx = (nowMinute / 1440) * W;
      s.append(svg('rect', { class: 'today-future', x: nx, y: 0, width: W - nx, height: H }),
        svg('line', { class: 'today-chart-now', x1: nx, x2: nx, y1: 0, y2: H }));
    }
    return s;
  }

  function dayChartLabel(hrs) {
    const sum = (a) => a.reduce((x, y) => x + y, 0);
    const top = hrs.bought.indexOf(Math.max(...hrs.bought));
    return `Kilowatt hours sold above the line and bought below it, by hour. Bought ${SD.fmt.kwh(sum(hrs.bought))}, sold ${SD.fmt.kwh(sum(hrs.sold))}${sum(hrs.bought) > 0 ? `; most buying at ${SD.fmt.minute(top * 60)}` : ''}.`;
  }

  function daySection() {
    const chartBox = h('div', { class: 'today-chart-box' });
    const ribbonBox = h('div', { class: 'today-ribbon' });
    const summary = h('div', { class: 'today-day-sum' });
    const today = SD.local().date;
    const el = h('section', { class: 'card today-day', 'aria-labelledby': 'today-day-h' },
      h('div', { class: 'card-head' },
        h('h2', { class: 'section-title', id: 'today-day-h', text: 'Your day so far' }),
        h('a', { class: 'card-link today-link-phone', href: `#history/day/${today}`, text: 'Open day' }),
        h('a', { class: 'card-link today-link-wide', href: '#history', text: 'Open history' })),
      chartBox, ribbonBox, summary);
    let ribbon = null;
    let ribbonLong = null;
    function drawRibbon(minute) {
      const long = window.matchMedia('(min-width: 760px)').matches;
      if (!ribbon || ribbonLong !== long) {
        ribbonLong = long;
        ribbon = SD.ribbon(ribbonBox, data.tariff, { nowMinute: minute, labels: long ? 'long' : 'short' });
      } else {
        ribbon.setNow(minute);
      }
    }
    return {
      el,
      relayout() { ribbon = null; },
      update() {
        const minute = SD.local().minute;
        el.hidden = !data.tariff && !data.samples;
        if (el.hidden) return;
        if (data.tariff && data.tariff.configured && data.samples) {
          const hrs = hourly();
          chartBox.replaceChildren(
            dayChart(hrs, minute),
            h('span', { class: 'today-chart-tag is-top', text: 'Sold' }),
            h('span', { class: 'today-chart-tag is-bottom', text: 'Bought' }),
            h('span', { class: 'today-chart-tag is-now', style: `left: ${(minute / 1440) * 100}%`, text: minute > 1200 ? null : 'Now' }));
        } else {
          chartBox.replaceChildren();
        }
        if (data.tariff) drawRibbon(minute);
        summary.replaceChildren(...daySummary());
      },
    };
  }

  /** "Bought 11.6 kWh, 10.4 of it free · Sold 8.5 kWh" as two small figures. */
  function daySummary() {
    const d = data.daily || {};
    const hrs = data.samples ? hourly() : null;
    const sum = (a) => a.reduce((x, y) => x + y, 0);
    const bought = d.import_kwh != null ? d.import_kwh : hrs ? sum(hrs.bought) : null;
    const sold = d.export_kwh != null ? d.export_kwh : hrs ? sum(hrs.sold) : null;
    const out = [];
    if (bought != null) {
      out.push(h('div', {}, h('div', { class: 'label', text: 'Bought' }),
        h('div', { class: 'fig today-sum-fig', text: SD.fmt.kwh(bought) }),
        d.import_free_kwh != null ? h('div', { class: 'today-sum-note', text: `${SD.fmt.kwh(d.import_free_kwh)} of it free` }) : null));
    }
    if (sold != null) {
      out.push(h('div', {}, h('div', { class: 'label', text: 'Sold' }),
        h('div', { class: 'fig today-sum-fig', text: SD.fmt.kwh(sold) })));
    }
    return out;
  }

  /* ---------------------------------------------------------------- 6. coming up */

  /** The next three price changes and credit window events, today and tomorrow. */
  function upcoming() {
    const t = data.tariff;
    if (!t || !t.configured) return [];
    const now = SD.local();
    const events = [];
    const days = [{ off: 0, t }];
    if (data.tomorrow && data.tomorrow.configured) days.push({ off: 1, t: data.tomorrow });
    let prev = SD.tariff.segments(t).slice(-1)[0];
    for (const day of days) {
      const segs = SD.tariff.segments(day.t);
      segs.forEach((seg, i) => {
        const same = prev && prev.band === seg.band && prev.cents === seg.cents;
        if (!(i === 0 && (day.off === 0 || same))) events.push({ at: day.off * 1440 + seg.start, kind: 'band', seg, day: day.off });
        if (i === segs.length - 1) prev = seg;
      });
      const cw = SD.tariff.creditWindow(day.t);
      if (cw) {
        const start = day.off * 1440 + cw.start;
        const end = day.off * 1440 + cw.start + (minsTo(cw.start, cw.end) || 1440);
        events.push({ at: start, kind: 'credit', cw, day: day.off });
        if (start <= now.minute) events.push({ at: end, kind: 'credit-end', cw, day: day.off });
      }
    }
    return events.filter((e) => e.at > now.minute).sort((a, b) => a.at - b.at).slice(0, 3);
  }

  function upcomingRow(e) {
    const t = data.tariff;
    const weekday = e.day ? ` ${SD.fmt.date(SD.local(Date.now() / 1000 + 86400).date, 'medium').split(' ')[0]}` : '';
    const when = (m) => `${SD.fmt.minute(m % 1440)}${weekday}`;
    if (e.kind === 'band') {
      const { seg } = e;
      const swatch = h('b', { class: `swatch band-${seg.band}` });
      let text;
      if (seg.band === 'free') {
        const hrs = (seg.end - seg.start) / 60;
        text = `Free power for ${Number.isInteger(hrs) ? hrs : hrs.toFixed(1)} ${hrs === 1 ? 'hour' : 'hours'}`;
      } else {
        text = `${seg.band === 'peak' ? 'Peak' : 'Shoulder'} starts, ${SD.fmt.cents(seg.cents)} a kWh`;
      }
      return { when: when(e.at), body: [swatch, text] };
    }
    const credit = creditText(e.cw);
    if (e.kind === 'credit-end') return { when: when(e.at), body: `Credit window closes. The result shows here a little later.` };
    const { sample } = liveParts();
    const typ = batteryTypical();
    let extra = '';
    if (!e.day && sample && sample.soc_pct != null && typ) extra = ` Battery ${Math.round(sample.soc_pct)}%, tonight usually needs about ${Math.round(typ.used)}%.`;
    return { when: e.day ? when(e.at) : windowText(e.cw), body: `Zero grid import earns ${credit}.${extra}` };
  }

  function upSection() {
    const list = h('ul', { class: 'today-up' });
    const el = h('section', { class: 'card', 'aria-labelledby': 'today-up-h' },
      h('h2', { class: 'section-title', id: 'today-up-h', text: 'Coming up' }), list);
    return {
      el,
      update() {
        const rows = upcoming().map(upcomingRow);
        el.hidden = !rows.length;
        list.replaceChildren(...rows.map((r) => h('li', {}, h('span', { class: 'today-up-when', text: r.when }), h('span', { class: 'today-up-what' }, r.body))));
      },
    };
  }

  /* ---------------------------------------------------------------- 7. evening credit */

  /** Days (oldest first) with a decided credit, plus tonight if it is still to come. */
  function nights() {
    const days = (data.battery && data.battery.days) || [];
    const today = SD.local().date;
    const asc = days.slice().sort((a, b) => (a.date < b.date ? -1 : 1));
    const list = asc.filter((d) => d.credit !== 'unknown' || d.date === today);
    return list.slice(-NIGHTS).map((d) => ({
      date: d.date,
      state: d.credit === 'earned' ? 'earned' : d.credit === 'missed' ? 'missed' : d.date === today ? 'tonight' : 'unknown',
      day: d,
    }));
  }

  function streak(list) {
    let n = 0;
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i].state === 'tonight') continue;
      if (list[i].state !== 'earned') break;
      n++;
    }
    return n;
  }

  const MISS_REASON = {
    car_charging: 'the car was charging',
    no_data: 'there were no readings',
    battery_low: 'the battery ran low',
    high_load: 'the home used more than the battery could supply',
    small_import: 'a small amount was bought',
  };

  /** One sentence for the most recent missed night. */
  function missText(d) {
    const date = SD.fmt.date(d.date, 'short');
    const parts = [];
    if (d.window_import_kwh != null && d.first_import_at != null) {
      parts.push(`${SD.fmt.kwh(d.window_import_kwh)} bought at ${SD.fmt.time(d.first_import_at)}`);
      if (d.soc_at_first_import != null) parts.push(`when the battery was at ${Math.round(d.soc_at_first_import)}%`);
      return `Missed ${date}: ${parts.join(' ')}.`;
    }
    return d.reason && MISS_REASON[d.reason] ? `Missed ${date}: ${MISS_REASON[d.reason]}.` : `Missed ${date}.`;
  }

  function creditSection() {
    const el = h('section', { class: 'card', 'aria-labelledby': 'today-credit-h' });
    return {
      el,
      update() {
        const b = data.battery;
        const cw = data.tariff && SD.tariff.creditWindow(data.tariff);
        el.hidden = !(b && b.configured && b.days && b.days.length);
        if (el.hidden) return;
        const list = nights();
        const n = streak(list);
        const month = SD.local().date.slice(0, 7);
        const decided = b.days.filter((d) => d.date.startsWith(month) && d.credit !== 'unknown');
        const earned = decided.filter((d) => d.credit === 'earned').length;
        const dollars = cw ? cw.creditCents / 100 : null;
        const tonight = list.some((x) => x.state === 'tonight');
        const missed = list.filter((x) => x.state === 'missed').map((x) => x.date);
        const recent = b.days.filter((d) => d.credit === 'missed').sort((x, y) => (x.date < y.date ? 1 : -1))[0];
        const cells = list.map((x) => h('span', { class: `today-night is-${x.state}` }));
        const label = `Last ${list.length} nights: ${list.filter((x) => x.state === 'earned').length} earned${missed.length ? `, ${missed.length} missed (${missed.map((d) => SD.fmt.date(d, 'short')).join(', ')})` : ''}${tonight ? ', tonight to come' : ''}`;
        fill(el,
          h('div', { class: 'card-head' },
            h('h2', { class: 'section-title', id: 'today-credit-h', text: 'Evening credit' }),
            dollars != null && decided.length ? h('span', { class: 'label', text: `${SD.fmt.dollars(earned * dollars, 0)} of ${SD.fmt.dollars(decided.length * dollars, 0)} this month` }) : null),
          h('p', { class: 'today-streak', text: n ? `${n} ${n === 1 ? 'night' : 'nights'} in a row` : 'No streak yet' }),
          h('div', { class: 'today-nights', role: 'img', 'aria-label': label }, cells),
          h('ul', { class: 'today-legend', 'aria-hidden': 'true' },
            h('li', {}, h('span', { class: 'today-night is-earned' }), 'Earned'),
            h('li', {}, h('span', { class: 'today-night is-missed' }), 'Missed'),
            tonight ? h('li', {}, h('span', { class: 'today-night is-tonight' }), 'Tonight') : null),
          h('p', { class: 'today-miss', text: recent ? `${missText(recent)}${tonight ? ' Tonight is still to come.' : ''}` : (tonight ? 'No missed nights lately. Tonight is still to come.' : 'No missed nights lately.') }));
      },
    };
  }

  /* ---------------------------------------------------------------- 8. totals */

  function totalsSection() {
    const el = h('section', { class: 'today-totals', 'aria-label': "Today's totals" });
    return {
      el,
      update() {
        const d = data.daily || {};
        const own = d.load_kwh > 0 && d.import_kwh != null
          ? Math.round(Math.max(0, Math.min(100, (1 - d.import_kwh / d.load_kwh) * 100))) : null;
        const saved = data.costDay && data.costDay.savings != null ? data.costDay.savings : null;
        const tiles = [
          d.pv_kwh != null ? ['Solar made', SD.fmt.kwh(d.pv_kwh)] : null,
          d.load_kwh != null ? ['Home used', SD.fmt.kwh(d.load_kwh)] : null,
          own != null ? ['Ran on own power', `${own}%`] : null,
          saved != null ? ['Saved vs no system', SD.fmt.dollars(saved)] : null,
        ].filter(Boolean);
        el.hidden = !tiles.length;
        el.replaceChildren(...tiles.map(([label, value]) => h('div', { class: 'today-tile' },
          h('div', { class: 'label', text: label }), h('div', { class: 'fig fig-lg', text: value }))));
      },
    };
  }

  /* ---------------------------------------------------------------- data */

  /** Per-minute samples of the day as {ts, hour, minute, grid, hours}; grid in W, + is buying. */
  function toSamples(rows) {
    return rows.filter((r) => r[3] != null).map((r) => {
      const l = SD.local(r[0]);
      return { ts: r[0], hour: l.hour, minute: l.minute, grid: r[3], hours: 1 / 60 };
    });
  }

  /** The cloud's 5-minute series, used when there are no inverter samples (no Modbus). */
  function cloudSamples(day) {
    const get = (key) => ((day.series || []).find((s) => s.key === key) || {}).data || [];
    const imp = new Map(get('grid_import_w'));
    const exp = new Map(get('grid_export_w'));
    const times = [...new Set([...imp.keys(), ...exp.keys()])].sort((a, b) => a - b);
    return times.map((ts) => {
      const l = SD.local(ts);
      return { ts, hour: l.hour, minute: l.minute, grid: (imp.get(ts) || 0) - (exp.get(ts) || 0), hours: 5 / 60 };
    });
  }

  async function loadSamples(today) {
    const now = SD.local();
    const start = Math.floor(Date.now() / 1000) - now.minute * 60 - now.sec;
    try {
      const live = await SD.fetchJSON(`/api/live/series?from=${start}&to=${Math.floor(Date.now() / 1000)}`);
      if (live.data && live.data.length) return toSamples(live.data);
    } catch (err) { /* fall through to the cloud series */ }
    try {
      return cloudSamples(await SD.fetchJSON(`/api/day?date=${today}`));
    } catch (err) { return null; }
  }

  const ok = (p) => p.catch(() => null);

  /** Fetches the slower data and redraws everything. */
  async function refreshSlow() {
    const today = SD.local().date;
    const tomorrow = SD.local(Date.now() / 1000 + 86400).date;
    const [tariff, tomorrowT, costs, daily, samples, battery] = await Promise.all([
      ok(SD.tariff.get()),
      ok(SD.tariff.get(tomorrow)),
      ok(SD.fetchJSON(`/api/costs?from=${today}&to=${today}`)),
      ok(SD.fetchJSON(`/api/daily?from=${today}&to=${today}`)),
      loadSamples(today),
      ok(SD.fetchJSON('/api/battery?days=31')),
    ]);
    if (!view) return;
    data.tariff = tariff;
    data.tomorrow = tomorrowT;
    data.costDay = costs && costs.configured ? (costs.days || []).find((d) => d.date === today) || null : null;
    data.daily = daily && daily.days ? daily.days[0] || null : null;
    data.samples = samples;
    data.battery = battery;
    updateAll();
  }

  function updateAll() {
    if (!view) return;
    for (const s of Object.values(view.sections)) s.update();
  }

  /* ---------------------------------------------------------------- page */

  /** Puts the sections in one stack on a phone, or two columns on a wide screen. */
  function place() {
    const s = view.sections;
    const wide = window.matchMedia('(min-width: 760px)').matches;
    if (wide) {
      view.el.replaceChildren(h('div', { class: 'cols cols-2-1' },
        h('div', { class: 'stack' }, s.head.el, s.window.el, s.pull.el, s.day.el, s.up.el),
        h('div', { class: 'stack' }, s.now.el, s.credit.el, s.totals.el)));
    } else {
      view.el.replaceChildren(h('div', { class: 'stack' },
        s.head.el, s.window.el, s.pull.el, s.now.el, s.day.el, s.up.el, s.credit.el, s.totals.el));
    }
    s.day.relayout();
  }

  SD.page('today', {
    mount(el) {
      view = {
        el,
        timers: [],
        sections: {
          head: headSection(), window: windowSection(), pull: pullSection(), now: nowSection(),
          day: daySection(), up: upSection(), credit: creditSection(), totals: totalsSection(),
        },
      };
      for (const k of Object.keys(data)) delete data[k];
      place();
      const mq = window.matchMedia('(min-width: 760px)');
      const onResize = () => { place(); updateAll(); };
      mq.addEventListener('change', onResize);
      view.stopMq = () => mq.removeEventListener('change', onResize);

      view.unsub = SD.live.subscribe((live) => {
        data.live = live;
        if (data.tariff !== undefined) updateAll();
      });
      refreshSlow().catch(() => {});
      view.timers.push(setInterval(() => refreshSlow().catch(() => {}), SLOW_MS));
      // Inside the credit window the bought total matters, so the samples are fetched every minute.
      view.timers.push(setInterval(async () => {
        const win = windowNow();
        if (!win || !win.inside) return;
        const samples = await loadSamples(SD.local().date);
        if (view && samples) { data.samples = samples; updateAll(); }
      }, 60 * 1000));
    },
    unmount() {
      if (!view) return;
      view.unsub();
      view.stopMq();
      view.timers.forEach(clearInterval);
      view = null;
    },
  });
})();
