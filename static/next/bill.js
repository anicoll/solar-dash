/* Bill page: what this GloBird bill is so far, where it is heading, whether the
   numbers can be trusted, and what is left to cut.

   Data (all fetched together, none of them blocks the others):
     /api/bill/current   the period, the so-far lines and the projection (required)
     /api/globird/compare  our estimate against GloBird's meter, day by day
     /api/battery        which evenings earned the credit (and why one was missed)
     /api/payback        saved this year and since install
     /api/admin          past bills, for the "last six bills" bars on desktop
     /api/tariff         band times, through SD.tariff
   Anything that fails or is empty hides its own part of the page. */
(function () {
  'use strict';

  const { h } = SD;
  const SVGNS = 'http://www.w3.org/2000/svg';

  // The check says "matches" while our estimate is within this many dollars of GloBird's
  // for the whole bill so far. Each wording tier is the smallest one that holds.
  const MATCH_TIERS = [0.1, 0.3, 0.5, 1];
  // A day's bar in the day-by-day chart counts as close when it is within this many dollars.
  const DAY_CLOSE = 0.1;
  // The calendar needs at most this many days of /api/battery (the API allows 92).
  const BATTERY_MAX_DAYS = 92;

  const REASON_TEXT = {
    car_charging: 'A car was charging in the evening window.',
    battery_low: 'The battery was close to empty.',
    high_load: 'The house used more than the battery could cover.',
    small_import: 'A small amount was bought from the grid.',
    no_data: 'There is no reading for that evening.',
  };

  let alive = false;
  let desktopQuery = null;
  let onQueryChange = null;

  /* ---------------------------------------------------------------- helpers */

  const num = (n) => n != null && Number.isFinite(Number(n));
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
  const dayNumber = (date) => Number(date.slice(8, 10));
  // Whole dollars; a tiny negative reads as $0, not -$0.
  const whole = (dollars) => SD.fmt.dollars(Math.round(dollars) + 0, 0);

  /** Every date from `from` to `to`, inclusive, as 'YYYY-MM-DD'. */
  function datesBetween(from, to) {
    const out = [];
    const end = Date.parse(`${to}T00:00:00Z`);
    for (let t = Date.parse(`${from}T00:00:00Z`); t <= end && out.length < 400; t += 86400000) {
      out.push(new Date(t).toISOString().slice(0, 10));
    }
    return out;
  }

  /** Whole days from `a` to `b` (both 'YYYY-MM-DD'). */
  const daysBetween = (a, b) => Math.round((Date.parse(`${b}T00:00:00Z`) - Date.parse(`${a}T00:00:00Z`)) / 86400000);

  /** Fetch that turns a failure into null (and remembers a 404 for the friendly empty state). */
  function quiet(url) {
    return SD.fetchJSON(url).catch((err) => {
      if (err && err.status === 404) return { missing: true };
      return null;
    });
  }

  const svg = (tag, attrs) => {
    const el = document.createElementNS(SVGNS, tag);
    for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
    return el;
  };

  /* ---------------------------------------------------------------- empty states */

  function emptyState(title, text, extra) {
    return h('section', { class: 'card bill-empty' },
      h('h2', { class: 'section-title', text: title }),
      h('p', { class: 'muted', text }),
      extra || null);
  }

  /* ---------------------------------------------------------------- 1. period and projection */

  function renderHero(bill) {
    const sofar = bill.so_far;
    const elapsed = bill.days_elapsed;
    const dayNo = Math.min(elapsed + 1, bill.days_total);
    const projected = bill.projected_total_dollars;
    const without = bill.projected_without_system_dollars;

    const root = h('section', { class: 'bill-hero', 'aria-labelledby': 'bill-hero-h' },
      h('h2', { class: 'sr-only', id: 'bill-hero-h', text: 'This bill so far' }),
      h('p', { class: 'date-line', text: `Day ${dayNo} of ${bill.days_total} in this GloBird bill` }));

    if (elapsed < 1) {
      // Nothing to add up yet: only the plan's supply charge is known.
      root.append(
        h('p', { class: 'bill-amount', text: SD.fmt.dollars(0) }),
        h('p', { class: 'bill-heading', text: `so far${num(projected) ? `, heading for about ${whole(projected)}` : ''}.` }),
        h('p', { class: 'muted bill-note', text: 'The first full day finishes tonight, then the figures fill in.' }));
      return root;
    }

    root.append(
      h('p', { class: 'bill-amount', text: SD.fmt.dollars(sofar.total_dollars) }),
      h('p', { class: 'bill-heading', text: `so far, heading for about ${whole(projected)}.` }));

    // The bar: solid is spent, hatched is still to come, and the whole bar is
    // scaled to what the bill would be with no system, when that is known.
    const scale = Math.max(num(without) ? without : 0, projected, sofar.total_dollars, 1);
    const done = Math.max(0, sofar.total_dollars) / scale * 100;
    const rest = Math.max(0, projected - Math.max(0, sofar.total_dollars)) / scale * 100;
    const label = `${SD.fmt.dollars(sofar.total_dollars)} so far, about ${whole(projected)} projected`
      + (num(without) ? `, against about ${whole(without)} without the system` : '');
    root.append(h('div', { class: 'bill-bar-wrap' },
      h('div', { class: 'bill-bar', role: 'img', 'aria-label': label },
        h('span', { class: 'bill-bar-done', style: `width: ${done}%` }),
        h('span', { class: 'bill-bar-rest', style: `left: ${done}%; width: ${rest}%` })),
      h('div', { class: 'bill-bar-caps' },
        h('span', { text: 'So far · projected' }),
        num(without) ? h('span', { text: `Without the system: about ${whole(without)}` }) : null),
      h('p', { class: 'bill-note muted', text: 'Counts complete days, up to the end of yesterday.' })));
    return root;
  }

  /* ---------------------------------------------------------------- 2. trust badge and day-by-day chart */

  /** What the compare says: rows we can check, the gap, and how to word it. Null when there is nothing to show. */
  function checkSummary(compare) {
    if (!compare || !compare.days || !compare.totals) return null;
    const rows = compare.days.filter((d) => d.complete && d.ours && d.diff);
    if (!rows.length || !compare.totals.days_compared) return null;
    const gap = compare.totals.diff.total;
    const tier = MATCH_TIERS.find((t) => Math.abs(gap) <= t + 1e-9);
    return {
      rows,
      posted: compare.days.length,
      compared: compare.totals.days_compared,
      lastDate: rows.reduce((a, d) => (d.date > a ? d.date : a), rows[0].date),
      gap,
      tier: tier == null ? null : tier,
      totals: compare.totals,
      categories: compare.categories || [],
    };
  }

  /** A short plain reason for a large gap, only from what the API reports. */
  function gapReason(check) {
    const odd = check.categories.filter((c) => c.bucket === 'other' && Math.abs(c.amount) >= 0.5);
    if (odd.length) {
      const names = odd.slice(0, 2).map((c) => c.category.toLowerCase()).join(' and ');
      return `GloBird has charges we do not price, such as ${names}.`;
    }
    const parts = [['import', 'power bought'], ['export', 'feed-in'], ['supply', 'supply'], ['credit', 'the evening credit']];
    let best = null;
    for (const [key, name] of parts) {
      const d = check.totals.ours[key] - check.totals.globird[key];
      if (best == null || Math.abs(d) > Math.abs(best.d)) best = { d, name };
    }
    if (best && Math.abs(best.d) >= 0.5) {
      return `Most of the gap is in ${best.name}, where we are ${SD.fmt.dollars(Math.abs(best.d))} ${best.d < 0 ? 'under' : 'over'}.`;
    }
    return null;
  }

  function renderBadge(check, onToggle) {
    const close = check.tier != null;
    const title = close
      ? `Matches GloBird's meter within ${SD.fmt.cents(Math.round(check.tier * 100))}`
      : `Our estimate is ${SD.fmt.dollars(Math.abs(check.gap))} ${check.gap < 0 ? 'under' : 'over'} GloBird's bill`;
    const sub = close
      ? `Checked to ${SD.fmt.date(check.lastDate, 'short')}, ${check.compared} of ${plural(check.posted, 'day', 'days')}`
      : (gapReason(check) || `Over ${plural(check.compared, 'day', 'days')} checked. Tap for the day-by-day gap.`);
    const icon = svg('svg', { width: 16, height: 16, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', 'stroke-width': 2.4, 'stroke-linecap': 'round', 'aria-hidden': 'true' });
    icon.append(svg('path', { d: close ? 'M5 12l5 5 9-10' : 'M12 6v8M12 18h.01' }));
    const button = h('button', {
      type: 'button', class: `bill-badge ${close ? 'is-ok' : 'is-warn'}`, 'aria-controls': 'bill-check-card', onclick: onToggle,
    },
      h('span', { class: 'bill-badge-icon', 'aria-hidden': 'true' }, icon),
      h('span', { class: 'bill-badge-text' }, h('b', { text: title }), h('span', { class: 'muted', text: sub })),
      h('span', { class: 'bill-badge-chev', 'aria-hidden': 'true', text: '›' }));
    return button;
  }

  /** Bars of (our estimate minus GloBird's) for each checked day. */
  function renderCheckCard(check) {
    const rows = check.rows;
    const W = 300;
    const H = 100;
    const max = Math.max(DAY_CLOSE * 2, ...rows.map((d) => Math.abs(d.diff.total)));
    const half = 46;
    const mid = H / 2;
    const slot = W / rows.length;
    const barW = Math.max(1.5, Math.min(14, slot * 0.7));
    const biggest = rows.reduce((a, d) => (Math.abs(d.diff.total) > Math.abs(a.diff.total) ? d : a), rows[0]);
    const label = `Daily difference between our estimate and GloBird's bill for ${plural(rows.length, 'day', 'days')}. `
      + `The biggest gap was ${SD.fmt.dollars(Math.abs(biggest.diff.total))} ${biggest.diff.total < 0 ? 'under' : 'over'} on ${SD.fmt.date(biggest.date, 'short')}.`;

    const chart = svg('svg', { class: 'bill-check-svg', viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none', role: 'img', 'aria-label': label });
    const bandH = (DAY_CLOSE / max) * half;
    chart.append(svg('rect', { x: 0, y: mid - bandH, width: W, height: bandH * 2, class: 'bill-check-band' }));
    rows.forEach((d, i) => {
      const v = d.diff.total;
      const hgt = Math.max(1, (Math.abs(v) / max) * half);
      const bar = svg('rect', {
        x: i * slot + (slot - barW) / 2, y: v >= 0 ? mid - hgt : mid, width: barW, height: hgt,
        class: Math.abs(v) <= DAY_CLOSE ? 'bill-check-bar is-close' : 'bill-check-bar',
      });
      bar.append(svg('title'));
      bar.firstChild.textContent = `${SD.fmt.date(d.date, 'short')}: ${v > 0 ? '+' : ''}${SD.fmt.dollars(v)}`;
      chart.append(bar);
    });
    chart.append(svg('line', { x1: 0, y1: mid, x2: W, y2: mid, class: 'bill-check-zero', 'vector-effect': 'non-scaling-stroke' }));

    const far = rows.filter((d) => Math.abs(d.diff.total) > DAY_CLOSE).length;
    const summary = far === 0
      ? `Every checked day is within ${SD.fmt.cents(DAY_CLOSE * 100)} of GloBird.`
      : `${plural(far, 'day is', 'days are')} more than ${SD.fmt.cents(DAY_CLOSE * 100)} out. The biggest gap was ${SD.fmt.dollars(Math.abs(biggest.diff.total))} ${biggest.diff.total < 0 ? 'under' : 'over'} on ${SD.fmt.date(biggest.date, 'short')}.`;

    return h('section', { class: 'card bill-check-card', id: 'bill-check-card', 'aria-labelledby': 'bill-check-h' },
      h('div', { class: 'card-head bill-wrap' },
        h('h2', { class: 'section-title', id: 'bill-check-h', text: 'Bill check, day by day' }),
        h('span', { class: 'label', text: "Our estimate minus GloBird's meter" })),
      chart,
      h('div', { class: 'bill-axis' },
        h('span', { text: SD.fmt.date(rows[0].date, 'short') }),
        h('span', { text: SD.fmt.date(rows[rows.length - 1].date, 'short') })),
      h('p', { class: 'bill-note', text: summary }),
      h('p', { class: 'label', text: `The shaded band is within ${SD.fmt.cents(DAY_CLOSE * 100)}.` }));
  }

  /* ---------------------------------------------------------------- 3. lines like the GloBird bill */

  function bandLabel(band, tariff) {
    const name = { free: 'Free', shoulder: 'Shoulder', peak: 'Peak' }[band] || band;
    if (band === 'shoulder' || !tariff) return name;
    // Times only when the band is one clean run of the day.
    const runs = SD.tariff.segments(tariff).filter((s) => s.band === band);
    if (runs.length !== 1) return name;
    return `${name} ${SD.fmt.minute(runs[0].start)} to ${SD.fmt.minute(runs[0].end % 1440)}`;
  }

  function line(label, amount, opts) {
    opts = opts || {};
    return h('div', { class: `bill-line${opts.total ? ' is-total' : ''}${opts.rule ? ' has-rule' : ''}` },
      h('dt', {}, opts.band ? h('span', { class: `swatch band-${opts.band}` }) : null, label),
      h('dd', { class: opts.credit ? 'is-credit' : '', text: amount }));
  }

  function renderLines(bill, tariff, hasCredit) {
    const s = bill.so_far;
    const rows = [line(`Supply, ${plural(bill.days_elapsed, 'day', 'days')}`, SD.fmt.dollars(s.supply_dollars))];
    for (const b of s.bands) {
      rows.push(line(`${bandLabel(b.band, tariff)} · ${SD.fmt.kwh(b.kwh, b.kwh < 10 ? 1 : 0)}`, SD.fmt.dollars(b.dollars), { band: b.band }));
    }
    rows.push(line(`Feed-in · ${SD.fmt.kwh(s.feed_in_kwh, 0)}`, `−${SD.fmt.dollars(s.feed_in_dollars)}`, { credit: true }));
    if (hasCredit || s.credit_nights > 0) {
      rows.push(line(`ZeroHero credits · ${plural(s.credit_nights, 'night', 'nights')}`, `−${SD.fmt.dollars(s.credit_dollars)}`, { credit: true, rule: true }));
    } else {
      rows[rows.length - 1].classList.add('has-rule');
    }
    rows.push(line('So far', SD.fmt.dollars(s.total_dollars), { total: true }));
    return h('section', { class: 'card bill-lines', 'aria-labelledby': 'bill-lines-h' },
      h('h2', { class: 'section-title', id: 'bill-lines-h', text: 'Like your GloBird bill' }),
      h('p', { class: 'label', text: 'Same lines, same order, so you can tick it off.' }),
      h('dl', {}, rows));
  }

  /* ---------------------------------------------------------------- 4. what's left to cut */

  /** One or two plain sentences from the band split. Null when there is nothing worth saying. */
  function cutText(bill, missedNights) {
    const s = bill.so_far;
    const byBand = Object.fromEntries(s.bands.map((b) => [b.band, b]));
    const shoulder = byBand.shoulder ? byBand.shoulder.dollars : 0;
    const peak = byBand.peak ? byBand.peak.dollars : 0;
    const power = shoulder + peak;
    const parts = [];

    if (power < 0.005) {
      return [`No power has been bought at shoulder or peak prices so far. What is left is the ${SD.fmt.dollars(s.supply_dollars)} supply charge.`];
    }
    if (s.supply_dollars > power) {
      parts.push(`The supply charge (${SD.fmt.dollars(s.supply_dollars)}) is bigger than all the power you bought (${SD.fmt.dollars(power)}).`);
    } else {
      parts.push(`You have spent ${SD.fmt.dollars(power)} on power so far, on top of ${SD.fmt.dollars(s.supply_dollars)} supply.`);
    }
    const lead = shoulder >= peak ? 'shoulder' : 'peak';
    const piece = (band, dollars) => h('span', {}, h('b', { class: band === lead ? 'bill-hl' : null, text: SD.fmt.dollars(dollars) }), ` was ${band}`);
    const bits = [];
    if (shoulder > 0) bits.push(piece('shoulder', shoulder));
    if (peak > 0) bits.push(piece('peak', peak));
    const second = h('span', {}, 'Of that, ');
    bits.forEach((b, i) => { if (i) second.append(' and '); second.append(b); });
    second.append(missedNights > 0 && peak > 0
      ? `. ${plural(missedNights, 'evening', 'evenings')} missed the credit.` : '.');
    parts.push(second);
    return parts;
  }

  function renderCut(bill, missedNights) {
    const parts = cutText(bill, missedNights);
    if (!parts) return null;
    return h('section', { class: 'bill-cut', 'aria-labelledby': 'bill-cut-h' },
      h('h2', { class: 'section-title', id: 'bill-cut-h', text: "What's left to cut" }),
      h('p', {}, parts.flatMap((p, i) => (i ? [' ', p] : [p]))));
  }

  /* ---------------------------------------------------------------- 5. evening credits */

  /** The newest-first run of earned evenings. */
  function streakOf(daysOldestFirst) {
    let n = 0;
    for (let i = daysOldestFirst.length - 1; i >= 0; i--) {
      const c = daysOldestFirst[i].credit;
      if (c === 'unknown') continue; // tonight: not decided yet
      if (c !== 'earned') break;
      n++;
    }
    return n;
  }

  function missText(day, tariff) {
    const win = SD.tariff.creditWindow(tariff);
    const when = win ? `between ${SD.fmt.minute(win.start)} and ${SD.fmt.minute(win.end)}` : 'in the evening window';
    const parts = [];
    if (num(day.window_import_kwh) && day.window_import_kwh > 0) {
      parts.push(`${SD.fmt.kwh(day.window_import_kwh, 2)} came from the grid ${when}.`);
      if (num(day.first_import_at)) {
        const soc = num(day.soc_at_first_import) ? `, when the battery was at ${Math.round(day.soc_at_first_import)}%` : '';
        parts.push(`The first was at ${SD.fmt.time(day.first_import_at)}${soc}.`);
      } else if (REASON_TEXT[day.reason]) {
        parts.push(REASON_TEXT[day.reason]);
      }
    } else if (REASON_TEXT[day.reason]) {
      parts.push(REASON_TEXT[day.reason]);
    }
    if (num(day.credit_cents) && day.credit_cents > 0) parts.push(`That evening cost the ${SD.fmt.cents(day.credit_cents)} credit.`);
    return parts.join(' ');
  }

  /**
   * `battery` is /api/battery. Returns {node, missedNights}, or null when the plan
   * has no credit or the period has no decided evening yet.
   */
  function renderCredits(bill, battery, tariff) {
    if (!battery || !battery.configured || !Array.isArray(battery.days)) return null;
    const today = SD.local().date;
    const byDate = new Map(battery.days.map((d) => [d.date, d]));
    const dates = datesBetween(bill.from, bill.to);
    const inPeriod = battery.days.filter((d) => d.date >= bill.from && d.date <= bill.to);
    const decided = inPeriod.filter((d) => d.credit === 'earned' || d.credit === 'missed');
    if (!decided.length) return null;

    const earned = decided.filter((d) => d.credit === 'earned');
    const missed = decided.filter((d) => d.credit === 'missed');
    const cents = (d) => (num(d.credit_cents) ? d.credit_cents : 0);
    const earnedDollars = earned.reduce((a, d) => a + cents(d), 0) / 100;
    const possible = decided.reduce((a, d) => a + cents(d), 0) / 100;

    const cells = dates.map((date) => {
      const d = byDate.get(date);
      let state;
      let word;
      if (date > today) { state = 'future'; word = 'still to come'; }
      else if (d && d.credit === 'earned') { state = 'earned'; word = 'credit earned'; }
      else if (d && d.credit === 'missed') { state = 'missed'; word = 'credit missed'; }
      else if (date === today) { state = 'tonight'; word = 'tonight'; }
      else { state = 'unknown'; word = 'not known'; }
      return h('li', { class: `bill-day is-${state}`, 'aria-label': `${SD.fmt.date(date, 'short')}: ${word}`, text: String(dayNumber(date)) });
    });

    const run = streakOf([...inPeriod].sort((a, b) => (a.date < b.date ? -1 : 1)));
    const latest = missed[0] ? [...missed].sort((a, b) => (a.date < b.date ? 1 : -1))[0] : null;
    const legend = h('ul', { class: 'bill-legend', 'aria-hidden': 'true' },
      h('li', {}, h('span', { class: 'bill-key is-earned' }), 'Earned'),
      h('li', {}, h('span', { class: 'bill-key is-missed' }), 'Missed'),
      h('li', {}, h('span', { class: 'bill-key is-tonight' }), 'Tonight'),
      h('li', {}, h('span', { class: 'bill-key is-future' }), 'To come'));

    const node = h('section', { class: 'card bill-credits', 'aria-labelledby': 'bill-credit-h' },
      h('div', { class: 'card-head' },
        h('h2', { class: 'section-title', id: 'bill-credit-h', text: 'Evening credits this bill' }),
        h('span', { class: 'label', text: possible > 0 ? `${whole(earnedDollars)} of ${whole(possible)}` : `${earned.length} of ${plural(decided.length, 'night', 'nights')}` })),
      h('ol', { class: 'bill-days' }, cells),
      legend,
      run >= 3 ? h('p', { class: 'label', text: `${run} nights in a row so far.` }) : null,
      latest
        ? h('div', { class: 'bill-miss' }, h('b', { text: `${SD.fmt.date(latest.date, 'short')}, missed. ` }), missText(latest, tariff))
        : h('div', { class: 'bill-miss is-clear', text: 'No missed evenings this bill.' }));
    return { node, missedNights: missed.length };
  }

  /* ---------------------------------------------------------------- 6. saved, last six bills */

  function savedBox(label, dollars) {
    return h('div', {}, h('div', { class: 'label', text: label }), h('div', { class: 'fig fig-lg', text: whole(dollars) }));
  }

  function renderPastBills(adminData, bill) {
    const past = (adminData && Array.isArray(adminData.bills) ? adminData.bills : [])
      .filter((b) => b.to < bill.from && num(b.bill_total))
      .sort((a, b) => (a.from < b.from ? -1 : 1))
      .slice(-5);
    if (!past.length) return null;
    const bars = past.map((b) => ({ label: SD.fmt.date(b.to, 'short'), value: b.bill_total, projected: false }));
    bars.push({ label: 'Now', value: bill.projected_total_dollars, projected: true });
    const max = Math.max(1, ...bars.map((b) => b.value));
    const cols = { style: `grid-template-columns: repeat(${bars.length}, minmax(0, 1fr))` };
    return h('div', { class: 'bill-past' },
      h('p', { class: 'label', text: 'Last six bills' }),
      h('div', Object.assign({ class: 'bill-past-bars', role: 'img',
        'aria-label': `Bill totals: ${bars.map((b) => `${b.label} ${whole(b.value)}${b.projected ? ' projected' : ''}`).join(', ')}` }, cols),
        bars.map((b) => h('div', { class: `bill-past-bar${b.projected ? ' is-projected' : ''}`, style: `height: ${Math.max(2, (Math.max(0, b.value) / max) * 100)}%` }))),
      h('div', Object.assign({ class: 'bill-past-labels', 'aria-hidden': 'true' }, cols),
        bars.map((b) => h('span', {}, h('b', { text: whole(b.value) }), h('br'), b.label))),
      h('p', { class: 'label', text: "The striped bar is this bill's projection." }));
  }

  function renderSaved(bill, payback, adminData) {
    const s = bill.so_far;
    const boxes = [];
    if (bill.days_elapsed > 0 && num(s.without_system_dollars)) {
      boxes.push(savedBox('This bill', s.without_system_dollars - s.total_dollars));
    }
    if (payback && payback.configured) {
      const year = SD.local().date.slice(0, 4);
      const thisYear = (payback.by_year || []).find((y) => String(y.year) === year);
      if (thisYear) boxes.push(savedBox(year, thisYear.saved));
      if (num(payback.saved)) {
        const first = payback.purchases && payback.purchases[0] && payback.purchases[0].date;
        boxes.push(savedBox(first ? `Since ${first.slice(0, 4)}` : 'Since install', payback.saved));
      }
    }
    if (!boxes.length) return null;
    return h('section', { class: 'card bill-saved', 'aria-labelledby': 'bill-saved-h' },
      h('div', { class: 'card-head' },
        h('h2', { class: 'section-title', id: 'bill-saved-h', text: 'Saved by the system' }),
        h('a', { class: 'card-link', href: '#payback', text: 'Payback' })),
      h('div', { class: 'bill-saved-grid' }, boxes),
      renderPastBills(adminData, bill));
  }

  /* ---------------------------------------------------------------- 7. how this is worked out */

  function renderHow(bill) {
    const source = bill.uses_globird_figures
      ? "Days GloBird has already billed use GloBird's own figures; the other days are our estimate from the meter."
      : 'The figures are our estimate from the meter, using your plan\'s prices.';
    return h('details', { class: 'bill-how' },
      h('summary', { text: 'How this is worked out' }),
      h('p', { text: 'The amount so far adds up every complete day in this bill: the supply charge, power bought, feed-in and the evening credit. Today is left out until it is over.' }),
      h('p', { text: `${source} "Without the system" is what the same days would have cost with no solar or battery.` }),
      h('p', { text: 'The projection takes your average day so far and carries it to the end of the bill, so it keeps moving as the days add up.' }));
  }

  /* ---------------------------------------------------------------- page */

  function periodLabel(bill) {
    return `${SD.fmt.date(bill.from, 'short')} to ${SD.fmt.date(bill.to, 'short')}`;
  }

  function build(el, bill, extras) {
    const { compare, battery, payback, admin, tariff } = extras;
    const check = checkSummary(compare);
    const hasCredit = !!(battery && battery.configured);
    const credits = renderCredits(bill, battery, tariff);

    let checkCard = null;
    let badge = null;
    // On a wide screen the chart is always on show, so the badge only takes you to it.
    const setExpanded = () => {
      if (desktopQuery.matches) badge.removeAttribute('aria-expanded');
      else badge.setAttribute('aria-expanded', String(checkCard.classList.contains('is-open')));
    };
    if (check) {
      checkCard = renderCheckCard(check);
      badge = renderBadge(check, () => {
        if (desktopQuery.matches) {
          checkCard.scrollIntoView({ behavior: 'smooth', block: 'center' });
          return;
        }
        checkCard.classList.toggle('is-open');
        setExpanded();
      });
      setExpanded();
    }

    const hasBands = bill.days_elapsed > 0;
    const hero = h('div', { class: 'bill-hero-box' }, renderHero(bill));
    const badgeRow = badge ? h('div', { class: 'bill-badge-row' }, badge) : null;
    const lines = hasBands ? renderLines(bill, tariff, hasCredit) : null;
    const cut = hasBands ? renderCut(bill, credits ? credits.missedNights : 0) : null;
    const saved = renderSaved(bill, payback, admin);

    // One column on a phone, in reading order. On a wide screen the same pieces
    // go into a wide column (the verdict) and a side column (the bill lines).
    const left = [hero, badgeRow, checkCard, cut];
    const right = [lines, credits && credits.node, saved];
    const phone = [hero, badgeRow, checkCard, lines, cut, credits && credits.node, saved];
    const body = h('div', { class: 'bill-body' });
    const arrange = () => {
      if (desktopQuery.matches) {
        body.replaceChildren(
          h('div', { class: 'bill-col bill-col-main' }, left.filter(Boolean)),
          h('div', { class: 'bill-col bill-col-side' }, right.filter(Boolean)));
      } else {
        body.replaceChildren(h('div', { class: 'bill-col' }, phone.filter(Boolean)));
      }
    };
    arrange();
    const onChange = () => { arrange(); if (check) setExpanded(); };
    onQueryChange = onChange;
    desktopQuery.addEventListener('change', onChange);

    el.append(
      h('header', { class: 'page-head bill-head' },
        h('h1', { class: 'bill-title', text: 'Bill' }),
        h('p', { class: 'bill-period', text: periodLabel(bill) })),
      body,
      renderHow(bill));
  }

  SD.page('bill', {
    mount(el) {
      alive = true;
      desktopQuery = window.matchMedia('(min-width: 760px)');
      el.append(h('p', { class: 'muted bill-loading', role: 'status', text: 'Loading your bill' }));

      const load = () => SD.fetchJSON('/api/bill/current').catch((err) => {
        if (err && err.status === 404) return { missing: true };
        throw err;
      });

      load().then(async (bill) => {
        if (!alive) return;
        el.replaceChildren();
        if (bill.missing) {
          el.append(
            h('header', { class: 'page-head' }, h('h1', { class: 'bill-title', text: 'Bill' })),
            emptyState('Your bill is not available yet', 'This page needs a newer version of the server. It will fill in after the next update.'));
          return;
        }
        if (!bill.configured) {
          el.append(
            h('header', { class: 'page-head' }, h('h1', { class: 'bill-title', text: 'Bill' })),
            emptyState('No plan set up yet', 'Add your electricity plan in Settings and this page will show what the bill is so far.',
              h('a', { class: 'card-link', href: '/#admin', text: 'Open settings' })));
          return;
        }
        const today = SD.local().date;
        const last = bill.to < today ? bill.to : today;
        const batteryDays = Math.min(BATTERY_MAX_DAYS, Math.max(1, daysBetween(bill.from, today) + 1));
        const [compare, battery, payback, admin, tariff] = await Promise.all([
          quiet(`/api/globird/compare?from=${bill.from}&to=${last}`),
          quiet(`/api/battery?days=${batteryDays}`),
          quiet('/api/payback'),
          quiet('/api/admin'),
          SD.tariff.get().catch(() => null),
        ]);
        if (!alive) return;
        build(el, bill, { compare, battery, payback, admin, tariff });
      }).catch((err) => {
        if (!alive) return;
        el.replaceChildren(
          h('header', { class: 'page-head' }, h('h1', { class: 'bill-title', text: 'Bill' })),
          emptyState('Could not load your bill', err.message || 'Something went wrong.',
            h('button', { type: 'button', class: 'bill-retry', text: 'Try again', onclick: () => location.reload() })));
      });
    },
    unmount() {
      alive = false;
      if (desktopQuery && onQueryChange) desktopQuery.removeEventListener('change', onQueryChange);
      onQueryChange = null;
    },
  });
})();
