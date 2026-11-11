/* Bill page: what this GloBird bill is so far, where it is heading, whether the
   numbers can be trusted, how each line is made up, and what is left to cut.

   Data (fetched together, none of them blocks the others):
     /api/bill/current     the period, the so-far lines, the projection range, the daily
                           series, the previous bill, past bills and cars (required)
     /api/globird/compare  our estimate against GloBird's meter, day by day
     /api/tariff           band times, through SD.tariff
     /api/bill/compare-dynamic  "What would Amber cost?" (only with [amber] and a covered day)
   Every newer field of /api/bill/current may be missing (an older server): the part
   that needs it is left out and nothing else changes.

   Layout: on a phone (under 760 px) the panels start closed, a tap on a panel's header
   opens or closes it, and several may be open. The open set is remembered in
   localStorage. From 760 px every panel is open and the headers are plain headings. */
(function () {
  'use strict';

  const { h } = SD;
  const SVGNS = 'http://www.w3.org/2000/svg';
  const STORE_KEY = 'sd.bill.open';
  const MINUS = '−';

  // The check says "matches" while our estimate is within this many dollars of GloBird's
  // for the whole bill so far. Each wording tier is the smallest one that holds.
  const MATCH_TIERS = [0.1, 0.3, 0.5, 1];
  // A day's bar in the day-by-day check counts as close when it is within this many dollars.
  const DAY_CLOSE = 0.1;
  // A day column is this tall, above and below the zero line.
  const HALF_PX = 64;

  let alive = false;
  let desktopQuery = null;
  let onQueryChange = null;

  /* ---------------------------------------------------------------- helpers */

  const num = (n) => n != null && Number.isFinite(Number(n));
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
  const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
  // Whole dollars; a tiny negative reads as $0, not -$0.
  const whole = (dollars) => SD.fmt.dollars(Math.round(dollars) + 0, 0);
  // A dollar figure that drops the cents when it is a round number.
  const tidy = (dollars) => SD.fmt.dollars(dollars, Math.abs(dollars % 1) < 0.005 ? 0 : 2);

  /** Short signed money: cents under a dollar, dollars above. -0.25 -> '-25c', 1.5 -> '$1.50'. */
  function amt(dollars) {
    const r = Math.round(dollars * 100) / 100;
    const a = Math.abs(r);
    const text = a < 1 ? `${Math.round(a * 100)}c` : SD.fmt.dollars(a);
    return r < 0 ? MINUS + text : text;
  }

  /** Same, for a screen reader. */
  function said(dollars) {
    const r = Math.round(dollars * 100) / 100;
    const a = Math.abs(r);
    const text = a < 1 ? `${Math.round(a * 100)}c` : SD.fmt.dollars(a);
    return r < 0 ? `minus ${text}` : text;
  }

  /** A credit line's amount: minus sign in front, but a zero reads as $0.00, not -$0.00. */
  const off = (dollars) => (Math.abs(Number(dollars) || 0) < 0.005 ? SD.fmt.dollars(0) : `${MINUS}${SD.fmt.dollars(dollars)}`);

  /** Every date from `from` to `to`, inclusive, as 'YYYY-MM-DD'. */
  function datesBetween(from, to) {
    const out = [];
    const end = Date.parse(`${to}T00:00:00Z`);
    for (let t = Date.parse(`${from}T00:00:00Z`); t <= end && out.length < 400; t += 86400000) {
      out.push(new Date(t).toISOString().slice(0, 10));
    }
    return out;
  }

  const monthShort = (date) => SD.fmt.date(date, 'month').split(' ')[0].slice(0, 3);
  const monthLong = (date) => SD.fmt.date(date, 'month').split(' ')[0];
  const hhmm = (s) => {
    if (typeof s !== 'string' || !/^\d{1,2}:\d{2}$/.test(s)) return null;
    const [a, b] = s.split(':').map(Number);
    return a * 60 + b;
  };

  /** Fetch that turns any failure into null. */
  function quiet(url) {
    return SD.fetchJSON(url).catch(() => null);
  }

  /** Run a renderer; a part that throws is left out, never the page. */
  function safe(fn) {
    try {
      return fn() || null;
    } catch (err) {
      console.error(err);
      return null;
    }
  }

  const svg = (tag, attrs) => {
    const el = document.createElementNS(SVGNS, tag);
    for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
    return el;
  };

  const readOpen = () => {
    try {
      const v = JSON.parse(localStorage.getItem(STORE_KEY) || '[]');
      return new Set(Array.isArray(v) ? v.filter((k) => typeof k === 'string') : []);
    } catch (_) {
      return new Set();
    }
  };
  const writeOpen = (set) => {
    try { localStorage.setItem(STORE_KEY, JSON.stringify([...set])); } catch (_) { /* private mode: not remembered */ }
  };

  /* ---------------------------------------------------------------- empty states */

  function emptyState(title, text, extra) {
    return h('section', { class: 'card bill-empty' },
      h('h2', { class: 'section-title', text: title }),
      h('p', { class: 'muted', text }),
      extra || null);
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

  /** A rounded top for the gap scale: 20c, 50c, $1, $2, $5 ... */
  function niceGap(v) {
    for (const step of [0.2, 0.5, 1, 2, 5, 10, 20, 50]) if (v <= step) return step;
    return Math.ceil(v / 50) * 50;
  }

  const signed = (v) => `${v > 0 ? '+' : v < 0 ? '−' : ''}${SD.fmt.dollars(Math.abs(v))}`;

  /** The hover/tap details for one day: both totals, then where they differ. */
  function dayDetail(d) {
    const g = d.globird;
    const o = d.ours;
    const out = [
      h('b', { text: SD.fmt.date(d.date, 'medium') }),
      h('span', { text: `Our estimate ${SD.fmt.dollars(o.total)}, GloBird ${SD.fmt.dollars(g.total)}` }),
      h('span', { text: `Gap ${signed(d.diff.total)}` }),
    ];
    const parts = [
      ['Power bought', o.import - g.import, d.diff.import_kwh],
      ['Feed-in', o.export - g.export, d.diff.export_kwh],
      ['Supply', o.supply - g.supply, null],
      ['Evening credit', o.credit - g.credit, null],
      ['Charges we do not price', -g.other, null],
    ];
    for (const [name, gap, kwh] of parts) {
      if (Math.abs(gap) < 0.005) continue;
      const k = kwh != null && Math.abs(kwh) >= 0.05 ? ` (${kwh > 0 ? '+' : '−'}${SD.fmt.kwh(Math.abs(kwh))})` : '';
      out.push(h('span', { class: 'bill-tip-part', text: `${name} ${signed(gap)}${k}` }));
    }
    return out;
  }

  /** Bars of (our estimate minus GloBird's) for each checked day, on a dollar scale. */
  function renderCheckCard(check) {
    const rows = check.rows;
    const top = niceGap(Math.max(DAY_CLOSE * 2, ...rows.map((d) => Math.abs(d.diff.total))) * 1.15); // room for the labels
    const pct = (v) => `${Math.min(100, (Math.abs(v) / top) * 100).toFixed(1)}%`;
    const biggest = rows.reduce((a, d) => (Math.abs(d.diff.total) > Math.abs(a.diff.total) ? d : a), rows[0]);
    // Label every bar while they fit (a week); on a longer bill only the biggest gap.
    const dayEvery = rows.length <= 14 ? 1 : rows.length <= 21 ? 2 : 5;

    const tip = h('div', { class: 'bill-tip', hidden: true, role: 'status' });
    const plot = h('div', { class: 'bill-check-plot' });
    let active = null;
    function show(col, d) {
      if (active) active.classList.remove('is-active');
      active = col;
      col.classList.add('is-active');
      tip.replaceChildren(...dayDetail(d));
      tip.hidden = false;
      const box = plot.getBoundingClientRect();
      const c = col.getBoundingClientRect();
      const w = tip.offsetWidth;
      tip.style.left = `${Math.max(0, Math.min(box.width - w, c.left - box.left + c.width / 2 - w / 2))}px`;
    }
    function hide() {
      if (active) active.classList.remove('is-active');
      active = null;
      tip.hidden = true;
    }

    rows.forEach((d, i) => {
      const v = d.diff.total;
      const close = Math.abs(v) <= DAY_CLOSE;
      const said = `${SD.fmt.date(d.date, 'medium')}: our estimate ${signed(v)} against GloBird`;
      const bar = h('span', { class: `bill-check-bar${close ? ' is-close' : ''}`, style: `height: ${pct(v)}` });
      const value = rows.length <= 7 || d === biggest ? h('span', { class: 'bill-check-value', text: signed(v).replace('.00', '') }) : null;
      const col = h('button', {
        type: 'button', class: `bill-check-col ${v < 0 ? 'is-under' : 'is-over'}`, 'aria-label': said,
        onmouseenter: () => show(col, d),
        onfocus: () => show(col, d),
        onclick: () => (active === col ? hide() : show(col, d)),
      },
        h('span', { class: 'bill-check-up' }, v >= 0 ? value : null, v >= 0 ? bar : null),
        h('span', { class: 'bill-check-down' }, v < 0 ? bar : null, v < 0 ? value : null),
        h('span', { class: 'bill-check-day', text: i % dayEvery === 0 || i === rows.length - 1 ? String(+d.date.slice(8)) : '' }));
      plot.append(col);
    });
    plot.addEventListener('mouseleave', hide);
    plot.addEventListener('focusout', (e) => { if (!plot.contains(e.relatedTarget)) hide(); });

    const scale = h('div', { class: 'bill-check-scale', 'aria-hidden': 'true' },
      h('span', { text: `+${SD.fmt.dollars(top)}` }),
      h('span', { text: '$0' }),
      h('span', { text: `−${SD.fmt.dollars(top)}` }));
    const band = h('div', { class: 'bill-check-band', 'aria-hidden': 'true', style: `height: ${((DAY_CLOSE / top) * 100).toFixed(1)}%` });

    const far = rows.filter((d) => Math.abs(d.diff.total) > DAY_CLOSE).length;
    const summary = far === 0
      ? `Every checked day is within ${SD.fmt.cents(DAY_CLOSE * 100)} of GloBird.`
      : `${plural(far, 'day is', 'days are')} more than ${SD.fmt.cents(DAY_CLOSE * 100)} out. The biggest gap was ${SD.fmt.dollars(Math.abs(biggest.diff.total))} ${biggest.diff.total < 0 ? 'under' : 'over'} on ${SD.fmt.date(biggest.date, 'short')}.`;

    return h('section', { class: 'card bill-check-card', id: 'bill-check-card', 'aria-labelledby': 'bill-check-h' },
      h('div', { class: 'card-head bill-wrap' },
        h('h2', { class: 'section-title', id: 'bill-check-h', text: 'Bill check, day by day' })),
      h('p', { class: 'bill-check-key' },
        'Each bar is one day: our estimate minus what GloBird charged. Above the line we estimated more, below it less. ',
        h('span', { class: 'bill-check-key-band' }), ` Within ${SD.fmt.cents(DAY_CLOSE * 100)}.`),
      h('div', { class: 'bill-check-chart' }, scale,
        h('div', { class: 'bill-check-area' }, band, h('div', { class: 'bill-check-zero', 'aria-hidden': 'true' }), plot, tip)),
      h('p', { class: 'bill-check-month', 'aria-hidden': 'true', text: `${SD.fmt.date(rows[0].date, 'short')} to ${SD.fmt.date(rows[rows.length - 1].date, 'short')} · tap or hover a day for its details` }),
      h('p', { class: 'bill-note', text: summary }));
  }

  /* ---------------------------------------------------------------- 1. hero: amount, verdict, projection track */

  /** The projection range as [low, high], or null when the server sent no projection at all. */
  function projectionRange(bill) {
    const a = num(bill.projected_total_dollars) ? Number(bill.projected_total_dollars) : null;
    const low = num(bill.projected_low_dollars) ? Number(bill.projected_low_dollars) : a;
    const high = num(bill.projected_high_dollars) ? Number(bill.projected_high_dollars) : a;
    if (low == null || high == null) return null;
    return [Math.min(low, high), Math.max(low, high), a];
  }

  /** The projection track: solid is spent, hatched is the range still to come, the tick is last bill. */
  function renderTrack(bill, range, prev) {
    const spent = Number(bill.so_far.total_dollars);
    const [low, high, pace] = range;
    const last = prev ? Number(prev.total_dollars) : null;
    const scale = Math.max(spent, high, last || 0, 1) * 1.17;
    const pct = (v) => clamp((v / scale) * 100, 0, 100);
    const spread = high - low > 0.005;
    // Which end of the range is "this bill's pace" and which is "last bill's pace".
    const lowIsPace = pace == null || Math.abs(low - pace) <= Math.abs(high - pace);

    const caps = [{ x: 0, anchor: 'left', fig: SD.fmt.dollars(spent), text: 'so far' }];
    if (spread) {
      caps.push({ x: pct(low), fig: SD.fmt.dollars(low), text: lowIsPace ? 'at this pace' : 'like last bill' });
      caps.push({ x: pct(high), fig: SD.fmt.dollars(high), text: lowIsPace ? 'like last bill' : 'at this pace' });
    } else {
      caps.push({ x: pct(high), fig: SD.fmt.dollars(high), text: 'projected' });
    }
    if (last != null) caps.push({ x: pct(last), fig: SD.fmt.dollars(last), text: 'last bill' });
    caps.sort((a, b) => a.x - b.x);

    // Captions sit under the track; one that would run into its neighbour drops to a second row.
    const width = 21;
    const rows = [];
    const nodes = caps.map((c) => {
      const anchor = c.anchor || (c.x > 78 ? 'right' : c.x < 8 ? 'left' : 'center');
      const [a, b] = anchor === 'left' ? [c.x, c.x + width] : anchor === 'right' ? [c.x - width, c.x] : [c.x - width / 2, c.x + width / 2];
      let row = rows.findIndex((end) => a >= end - 0.5);
      if (row < 0) { row = rows.length; rows.push(b); } else rows[row] = b;
      const shift = anchor === 'right' ? 'translateX(-100%)' : anchor === 'center' ? 'translateX(-50%)' : '';
      return h('span', {
        style: `left: ${c.x}%; top: ${row * 34}px;${shift ? ` transform: ${shift};` : ''} text-align: ${anchor === 'right' ? 'right' : anchor === 'center' ? 'center' : 'left'}`,
      }, h('b', { text: c.fig }), c.text);
    });

    const label = `${SD.fmt.dollars(spent)} so far. `
      + (spread
        ? `Projected ${SD.fmt.dollars(low)} ${lowIsPace ? 'at this pace' : 'if the rest is like last bill'}, ${SD.fmt.dollars(high)} ${lowIsPace ? 'if the rest is like last bill' : 'at this pace'}.`
        : `Projected ${SD.fmt.dollars(high)}.`)
      + (last != null ? ` Last bill was ${SD.fmt.dollars(last)}.` : '');

    return h('div', { class: 'bill-proj' },
      h('div', { class: 'bill-track', role: 'img', 'aria-label': label },
        h('span', { class: 'bill-t-done', style: `width: ${pct(Math.max(0, spent))}%` }),
        spread ? h('span', { class: 'bill-t-range', style: `left: ${pct(low)}%; width: ${Math.max(0, pct(high) - pct(low))}%` }) : null,
        last != null ? h('span', { class: 'bill-t-mark', style: `left: ${pct(last)}%` }) : null),
      h('div', { class: 'bill-t-caps', 'aria-hidden': 'true', style: `height: ${rows.length * 34 + 2}px` }, nodes));
  }

  function stat(label, fig, sub) {
    return h('div', { class: 'bill-stat' },
      h('p', { class: 'label', text: label }),
      h('p', { class: 'bill-stat-fig fig', text: fig }),
      sub ? h('p', { class: 'label', text: sub }) : null);
  }

  function renderHero(bill, badge) {
    const s = bill.so_far;
    const elapsed = bill.days_elapsed;
    const prev = bill.previous && num(bill.previous.total_dollars) && bill.previous.days > 0 ? bill.previous : null;
    const range = projectionRange(bill);
    const root = h('section', { class: 'card bill-hero', 'aria-labelledby': 'bill-hero-h' },
      h('h2', { class: 'label bill-hero-h', id: 'bill-hero-h', text: 'This GloBird bill so far' }));

    if (elapsed < 1) {
      // Nothing to add up yet: only the plan's supply charge is known.
      root.append(h('div', {},
        h('p', { class: 'bill-amount', text: SD.fmt.dollars(0) }),
        range ? h('p', { class: 'bill-verdict', text: `Heading for about ${whole(range[2] != null ? range[2] : range[1])}.` }) : null,
        h('p', { class: 'muted bill-note', text: 'The first full day finishes tonight, then the figures fill in.' })));
      return root;
    }

    root.append(h('p', { class: 'bill-amount', text: SD.fmt.dollars(s.total_dollars) }));
    if (range) {
      const lo = Math.round(range[0]);
      const hi = Math.round(range[1]);
      const text = lo === hi ? `about ${whole(range[1])}` : `${whole(range[0])} to ${whole(range[1])}`;
      root.append(h('p', { class: 'bill-verdict' },
        'Heading for ', h('b', { text }), ` by ${SD.fmt.date(bill.to, 'short')}.`,
        prev ? ` Last bill was ${SD.fmt.dollars(prev.total_dollars)}.` : ''));
      const track = safe(() => renderTrack(bill, range, prev));
      if (track) root.append(track);
    }

    const days = num(s.supply_days) && s.supply_days > 0 ? s.supply_days : elapsed;
    const stats = [];
    if (num(s.total_dollars) && days > 0) {
      stats.push(stat('Per day', amt(s.total_dollars / days), prev ? `was ${amt(prev.total_dollars / prev.days)}` : null));
    }
    if (num(s.without_system_dollars)) {
      stats.push(stat('No system', SD.fmt.dollars(s.without_system_dollars),
        num(bill.projected_without_system_dollars) ? `~${whole(bill.projected_without_system_dollars)} bill` : null));
    }
    if (num(s.credit_nights) && (s.credit_nights > 0 || s.credit_possible_nights > 0)) {
      const possible = num(s.credit_possible_nights) ? s.credit_possible_nights : null;
      stats.push(stat('Credits', possible != null ? `${s.credit_nights} of ${possible}` : String(s.credit_nights),
        num(s.credit_dollars) ? `${tidy(s.credit_dollars)} earned` : null));
    }
    if (stats.length) root.append(h('div', { class: 'bill-stats', style: `grid-template-columns: repeat(${stats.length}, minmax(0, 1fr))` }, stats));
    if (badge) root.append(badge);
    return root;
  }

  /* ---------------------------------------------------------------- panels */

  /**
   * One collapsible card. `o` = {key, area, title, summary: string | (open, desktop) => string,
   * fig: children | null, body: node, collapse: true when the whole body hides while closed}.
   * Parts of a body that only show when open carry class bill-more.
   */
  function makePanel(ctx, o) {
    const id = `bill-${o.key}`;
    const sum = h('span', { class: 'label bill-ph-sum' });
    const btn = h('button', { type: 'button', class: 'bill-ph', 'aria-controls': `${id}-body` },
      h('span', { class: 'bill-ph-t' }, h('span', { class: 'section-title bill-st', id: `${id}-t`, text: o.title }), sum),
      o.fig ? h('span', { class: `bill-ph-k fig ${o.figClass || ''}` }, o.fig) : null,
      h('span', { class: 'bill-chev', 'aria-hidden': 'true', text: '›' }));
    const el = h('section', { class: `card bill-panel ${o.area || ''}`, 'data-key': o.key, 'aria-labelledby': `${id}-t` },
      h('h2', { class: 'bill-ph-h' }, btn),
      h('div', { class: `bill-body${o.collapse ? ' bill-more' : ''}`, id: `${id}-body` }, o.body));
    const panel = { key: o.key, el, btn, sum, summary: o.summary };
    btn.addEventListener('click', () => {
      if (ctx.desktop()) return;
      if (ctx.open.has(o.key)) ctx.open.delete(o.key); else ctx.open.add(o.key);
      writeOpen(ctx.open);
      ctx.apply();
    });
    ctx.panels.push(panel);
    return el;
  }

  function applyPanels(ctx) {
    const desk = ctx.desktop();
    for (const p of ctx.panels) {
      const open = desk || ctx.open.has(p.key);
      p.el.classList.toggle('is-open', open);
      if (desk) p.btn.removeAttribute('aria-expanded');
      else p.btn.setAttribute('aria-expanded', String(open));
      p.btn.disabled = desk;
      p.sum.textContent = typeof p.summary === 'function' ? p.summary(open, desk) : p.summary;
    }
  }

  /* ---------------------------------------------------------------- 3. where it comes from */

  function waterfall(bill) {
    const s = bill.so_far;
    if (![s.total_dollars, s.supply_dollars, s.feed_in_dollars].every(num)) return null;
    const total = Number(s.total_dollars);
    const feed = Number(s.feed_in_dollars);
    const credit = num(s.credit_dollars) ? Number(s.credit_dollars) : 0;
    const supply = Number(s.supply_dollars);
    // Worked back from the total so the rows always add up to it.
    const charged = total + feed + credit;
    const bought = charged - supply;
    const bands = Object.fromEntries((s.bands || []).map((b) => [b.band, b]));
    const shoulder = bands.shoulder ? Number(bands.shoulder.dollars) : 0;
    const peak = bands.peak ? Number(bands.peak.dollars) : 0;
    const boughtKwh = (bands.shoulder ? Number(bands.shoulder.kwh) : 0) + (bands.peak ? Number(bands.peak.kwh) : 0);
    const scale = Math.max(charged, 0.01);
    const p = (v) => clamp((v / scale) * 100, 0, 100);
    const place = (from, to) => `left: ${p(from)}%; width: ${Math.max(0, p(to) - p(from))}%`;
    const hasCredit = credit > 0 || s.credit_nights > 0;
    const supplySub = num(s.supply_days) && num(s.supply_cents_per_day)
      ? `${plural(s.supply_days, 'day', 'days')} × ${SD.fmt.cents(s.supply_cents_per_day)}` : null;

    const row = (name, sub, bar, value, cls) => h('div', { class: `bill-wf-row${cls ? ` ${cls}` : ''}` },
      h('span', { class: 'bill-wf-nm' }, name, sub ? h('small', { text: sub }) : null),
      h('span', { class: 'bill-wf-bar' }, bar),
      h('span', { class: `bill-wf-v fig${cls === 'is-cr' ? ' is-credit' : ''}`, text: value }));
    const bar = (cls, from, to) => h('span', { class: cls, style: place(from, to) });
    const guide = (at) => h('span', { class: 'bill-wf-guide', style: `left: ${p(at)}%` });

    const split = shoulder + peak > 0 ? shoulder / (shoulder + peak) : 1;
    const midway = supply + Math.max(0, bought) * split;
    const rows = [
      row('Supply', supplySub, bar('bill-f-supply', 0, supply), SD.fmt.dollars(supply)),
      row('Bought', boughtKwh > 0 ? SD.fmt.kwh(boughtKwh, 1) : null,
        [bar('bill-f-shoulder', supply, midway), bar('bill-f-peak', midway, charged)], `+${SD.fmt.dollars(Math.max(0, bought))}`),
      row('Feed-in', num(s.feed_in_kwh) ? SD.fmt.kwh(s.feed_in_kwh, 0) : null,
        [guide(charged), bar('bill-f-feed', charged - feed, charged)], `${MINUS}${SD.fmt.dollars(feed)}`, 'is-cr'),
    ];
    if (hasCredit) {
      rows.push(row('Credits', s.credit_nights > 0 ? plural(s.credit_nights, 'evening', 'evenings') : null,
        [guide(charged - feed), bar('bill-f-credit', charged - feed - credit, charged - feed)], `${MINUS}${SD.fmt.dollars(credit)}`, 'is-cr'));
    }
    rows.push(row(total < 0 ? 'You are owed' : 'You owe', null, bar('bill-f-total', 0, Math.max(0, total)), SD.fmt.dollars(total), 'is-tot'));

    const said1 = `Supply ${SD.fmt.dollars(supply)} plus power bought ${SD.fmt.dollars(Math.max(0, bought))} makes ${SD.fmt.dollars(charged)} charged. `
      + `Feed-in takes off ${SD.fmt.dollars(feed)}${hasCredit ? ` and evening credits ${SD.fmt.dollars(credit)}` : ''}, leaving ${SD.fmt.dollars(total)}.`;
    return {
      title: 'Where it comes from',
      summary: `${SD.fmt.dollars(charged)} charged, ${SD.fmt.dollars(feed + credit)} came back`,
      fig: SD.fmt.dollars(total),
      collapse: true,
      body: h('div', { class: 'bill-wf', role: 'img', 'aria-label': said1 }, rows),
    };
  }

  /* ---------------------------------------------------------------- 4. line by line */

  function bandLabel(band, tariff) {
    const name = { free: 'Free', shoulder: 'Shoulder', peak: 'Peak' }[band] || band;
    if (band === 'shoulder' || !tariff) return name;
    // Times only when the band is one clean run of the day.
    const runs = SD.tariff.segments(tariff).filter((s) => s.band === band);
    if (runs.length !== 1) return name;
    return `${name} ${SD.fmt.minute(runs[0].start)} to ${SD.fmt.minute(runs[0].end % 1440)}`;
  }

  /** "11 pm to 11 am and 2 pm to 4 pm" for a band, or null when there is no plan. */
  function bandTimes(tariff, band) {
    if (!tariff || !tariff.configured) return null;
    const runs = SD.tariff.segments(tariff).filter((s) => s.band === band);
    if (!runs.length) return null;
    if (runs.length > 1 && runs[0].start === 0 && runs[runs.length - 1].end === 1440) {
      const first = runs.shift();
      runs[runs.length - 1] = { start: runs[runs.length - 1].start, end: first.end };
    }
    return runs.map((r) => `${SD.fmt.minute(r.start)} to ${SD.fmt.minute(r.end % 1440)}`).join(' and ');
  }

  /** One bill line: label, amount, then (when the panel is open) "kWh x rate" and a plain reason. */
  function item(o) {
    return h('div', { class: `bill-ln${o.zero ? ' is-zero' : ''}${o.total ? ' is-total' : ''}` },
      h('dt', {}, o.sw ? h('span', { class: `bill-sw ${o.sw}`, 'aria-hidden': 'true' }) : null, o.label),
      h('dd', { class: `bill-ln-v fig${o.credit ? ' is-credit' : ''}`, text: o.amount }),
      o.calc ? h('dd', { class: 'bill-calc bill-more', text: o.calc }) : null,
      o.why ? h('dd', { class: 'bill-why bill-more' }, o.why) : null);
  }

  const WINDOW_NAME = { super: 'Super export', peak: 'Evening export', other: 'Other exports', tier: 'Tiered export' };
  const WINDOW_WHY = {
    super: 'The top rate, paid on exports in this window.',
    peak: 'Paid on exports in the wider evening window, outside the top rate.',
    other: 'Everything exported outside those windows.',
    tier: 'Paid in tiers, by how much is exported.',
  };

  function linesPanel(bill, tariff) {
    const s = bill.so_far;
    const rows = [];
    const supplyCalc = num(s.supply_days) && num(s.supply_cents_per_day)
      ? `${plural(s.supply_days, 'day', 'days')} × ${SD.fmt.cents(s.supply_cents_per_day)}` : null;
    rows.push(item({
      sw: 'is-supply', label: 'Supply', amount: SD.fmt.dollars(s.supply_dollars), calc: supplyCalc,
      why: 'Fixed daily fee for being connected, even when you buy nothing.',
    }));

    const bands = Array.isArray(s.bands) ? s.bands : [];
    const ordered = ['free', 'shoulder', 'peak'].map((k) => bands.find((b) => b.band === k)).filter(Boolean)
      .concat(bands.filter((b) => !['free', 'shoulder', 'peak'].includes(b.band)));
    for (const b of ordered) {
      const rate = num(b.cents_per_kwh) ? ` × ${SD.fmt.cents(b.cents_per_kwh)}` : '';
      let why;
      if (b.band === 'free') {
        why = num(s.free_value_dollars) && s.free_value_dollars > 0
          ? ['Would have cost ', h('b', { text: SD.fmt.dollars(s.free_value_dollars) }), ' at shoulder price.']
          : 'Power bought in this window costs nothing.';
      } else if (b.band === 'shoulder') {
        const when = bandTimes(tariff, 'shoulder');
        why = when ? `${when}. The everyday rate.` : 'The everyday rate, outside the free and peak times.';
      } else if (b.band === 'peak') {
        why = 'The dearest power, so each kWh bought here counts for most.';
      }
      rows.push(item({
        sw: `is-${b.band}`, label: bandLabel(b.band, tariff), amount: SD.fmt.dollars(b.dollars),
        zero: Number(b.dollars) === 0, calc: `${SD.fmt.kwh(b.kwh, 1)}${rate}`, why,
      }));
    }

    const windows = Array.isArray(s.feed_in_windows) ? s.feed_in_windows : null;
    if (windows && windows.length) {
      for (const w of windows) {
        const from = hhmm(w.from);
        const to = hhmm(w.to);
        const when = from != null && to != null ? ` ${SD.fmt.minute(from)} to ${SD.fmt.minute(to % 1440)}` : '';
        rows.push(item({
          sw: 'is-feed', credit: true, label: `${WINDOW_NAME[w.window] || 'Feed-in'}${when}`,
          amount: off(w.dollars), zero: Number(w.dollars) === 0,
          calc: `${SD.fmt.kwh(w.kwh, 1)}${num(w.cents_per_kwh) ? ` × ${SD.fmt.cents(w.cents_per_kwh)}` : ''}`,
          why: WINDOW_WHY[w.window] || 'Paid for power sent to the grid.',
        }));
      }
    } else if (num(s.feed_in_dollars)) {
      rows.push(item({
        sw: 'is-feed', credit: true, label: 'Feed-in', amount: off(s.feed_in_dollars),
        calc: num(s.feed_in_kwh) ? SD.fmt.kwh(s.feed_in_kwh, 1) : null, why: 'Paid for power sent to the grid.',
      }));
    }

    const nights = num(s.credit_nights) ? s.credit_nights : 0;
    const possible = num(s.credit_possible_nights) ? s.credit_possible_nights : null;
    if (nights > 0 || (possible != null && possible > 0) || Number(s.credit_dollars) > 0) {
      const win = SD.tariff.creditWindow(tariff);
      const per = nights > 0 ? Number(s.credit_dollars) / nights : win ? win.creditCents / 100 : null;
      const calc = possible != null
        ? `${nights} of ${plural(possible, 'evening', 'evenings')}${per != null ? ` × ${tidy(per)}` : ''}`
        : plural(nights, 'evening', 'evenings');
      rows.push(item({
        sw: 'is-credit', credit: true, label: 'ZeroHero credit', amount: off(s.credit_dollars),
        calc,
        why: win
          ? `Each evening buying ${num(win.maxKwh) ? SD.fmt.kwh(win.maxKwh, 1) : 'almost nothing'} or less between ${SD.fmt.minute(win.start)} and ${SD.fmt.minute(win.end)}.`
          : 'Each evening with almost no power bought in the credit window.',
      }));
    }
    rows.push(item({ total: true, label: 'So far', amount: SD.fmt.dollars(s.total_dollars) }));

    return {
      title: 'Line by line',
      summary: (open, desk) => (desk ? 'Rates and what each line means' : open ? 'Tap to hide the workings' : 'Tap for rates and what each line means'),
      fig: null,
      body: h('dl', { class: 'bill-dl' }, rows),
    };
  }

  /* ---------------------------------------------------------------- 5. each day */

  /** A round top for the day scale, in dollars. */
  function niceTop(v) {
    for (const step of [0.2, 0.3, 0.4, 0.5, 0.6, 0.8, 1, 1.5, 2, 3, 4, 5, 8, 10, 15, 20, 30, 50, 100]) if (v <= step) return step;
    return Math.ceil(v / 50) * 50;
  }

  const scaleLabel = (v) => (v < 1 ? `${Math.round(v * 100)}c` : SD.fmt.dollars(v, 0));

  function daysPanel(bill, today) {
    const daily = Array.isArray(bill.daily) ? bill.daily.filter((d) => d && typeof d.date === 'string' && num(d.total_dollars)) : null;
    if (!daily || !daily.length) return null;
    const dates = datesBetween(bill.from, bill.to);
    const byDate = new Map(daily.map((d) => [d.date, d]));
    const lastDone = daily[daily.length - 1].date;
    const top = niceTop(Math.max(0.1, ...daily.map((d) => Math.abs(Number(d.total_dollars)))));
    const anyCredit = daily.some((d) => d.credit === 'earned' || d.credit === 'missed');

    // Day numbers shown on a phone: the first, the 1st, every fifth and today, never two side by side.
    const shown = new Set();
    dates.forEach((date, i) => {
      const n = Number(date.slice(8, 10));
      const isToday = date === today && !byDate.has(date);
      if (!(i === 0 || isToday || n === 1 || n % 5 === 0)) return;
      const near = [i - 1, i - 2].filter((j) => shown.has(j));
      if (near.length && !isToday) return;
      if (near.length) near.forEach((j) => { if (j !== 0) shown.delete(j); });
      shown.add(i);
    });
    const cols = dates.map((date, i) => {
      const d = byDate.get(date);
      const n = Number(date.slice(8, 10));
      const isToday = date === today && !d;
      let up = null;
      let down = null;
      let dot = 'none';
      if (d) {
        const v = Number(d.total_dollars);
        const bar = h('span', { class: `bill-b ${v >= 0 ? 'is-pos' : 'is-neg'}`, style: `height: ${Math.max(2, (Math.abs(v) / top) * HALF_PX).toFixed(1)}px` });
        if (v >= 0) up = bar; else down = bar;
        dot = d.credit === 'earned' ? 'earned' : d.credit === 'missed' ? 'missed' : d.credit === 'incomplete' ? 'future' : 'none';
        if (d.incomplete) { up = h('span', { class: 'bill-b is-future', title: 'No full data' }); down = null; }
      } else {
        up = h('span', { class: 'bill-b is-future' });
        dot = isToday ? 'tonight' : anyCredit ? 'future' : 'none';
      }
      const major = shown.has(i);
      return h('div', { class: 'bill-col' },
        h('div', { class: 'bill-up' }, up),
        h('div', { class: 'bill-down' }, down),
        h('div', { class: `bill-dot is-${dot}` }, h('i')),
        h('div', { class: `bill-dn${isToday ? ' is-today' : ''}${major ? '' : ' is-minor'}`, text: String(n) }));
    });

    const earned = daily.filter((d) => d.credit === 'earned').length;
    const missed = daily.filter((d) => d.credit === 'missed').length;
    const possible = earned + missed;
    const vals = daily.map((d) => amt(Number(d.total_dollars))).map((t) => t.replace(MINUS, 'minus '));
    const label = `Net cost per day from ${SD.fmt.date(daily[0].date, 'short')}: ${vals.join(', ')}. `
      + (possible ? (missed === 0 ? 'Every evening earned the credit. ' : `${plural(missed, 'evening', 'evenings')} missed the credit. `) : '')
      + (dates.includes(today) && !byDate.has(today) ? `Today, ${SD.fmt.date(today, 'short')}, is still running.` : '');

    const chart = h('div', { class: 'bill-dchart', role: 'img', 'aria-label': label.trim() },
      h('div', { class: 'bill-scale-wrap', 'aria-hidden': 'true' },
        h('div', { class: 'bill-scale' }, h('span', { text: `+${scaleLabel(top)}` }), h('span', { text: '0' }), h('span', { text: `${MINUS}${scaleLabel(top)}` })),
        anyCredit ? h('div', { class: 'bill-row-lab', text: 'credit' }) : null),
      h('div', { class: 'bill-cols', 'aria-hidden': 'true', style: `grid-template-columns: repeat(${dates.length}, minmax(0, 1fr))` }, cols));

    // Open: a legend and the best, dearest and longest-running figures.
    const best = daily.reduce((a, d) => (Number(d.total_dollars) < Number(a.total_dollars) ? d : a), daily[0]);
    const dear = daily.reduce((a, d) => (Number(d.total_dollars) > Number(a.total_dollars) ? d : a), daily[0]);
    let streak = 0;
    for (let i = daily.length - 1; i >= 0 && daily[i].credit === 'earned'; i--) streak++;
    const sum = (name, fig, sub, cls) => h('div', { class: cls || null }, h('p', { class: 'label', text: name }), h('p', { class: 'bill-stat-fig fig', text: fig }), sub ? h('p', { class: 'label', text: sub }) : null);
    const s = bill.so_far;
    const perDay = num(s.without_system_dollars) && num(s.supply_days) && s.supply_days > 0 ? s.without_system_dollars / s.supply_days : null;
    const legend = h('div', { class: 'bill-legend', 'aria-hidden': 'true' },
      h('span', {}, h('i', { class: 'bill-lg-cost' }), 'Cost'),
      h('span', {}, h('i', { class: 'bill-lg-earn' }), 'Earned'),
      anyCredit ? h('span', {}, h('i', { class: 'bill-lg-dot' }), 'Credit earned') : null,
      missed > 0 ? h('span', {}, h('i', { class: 'bill-lg-miss' }), 'Credit missed') : null,
      dates.includes(today) && !byDate.has(today) && anyCredit ? h('span', {}, h('i', { class: 'bill-lg-tonight' }), 'Tonight') : null);
    const more = h('div', { class: 'bill-more' }, legend,
      h('div', { class: 'bill-sums' },
        sum('Best day', amt(Number(best.total_dollars)), SD.fmt.date(best.date, 'medium')),
        sum('Dearest', amt(Number(dear.total_dollars)), SD.fmt.date(dear.date, 'medium')),
        anyCredit ? sum('Streak', String(streak), streak === 1 ? 'night in a row' : 'nights in a row') : null,
        perDay != null ? sum('Without system', SD.fmt.dollars(perDay), 'a day', 'bill-hide-phone') : null));

    return {
      title: 'Each day',
      summary: anyCredit ? 'Net cost, credit dot under each day' : 'Net cost of each day',
      fig: possible ? [`${earned}/${possible} `, h('span', { class: 'bill-ph-u', text: 'credits' })] : null,
      body: h('div', {}, chart, more),
    };
  }

  /* ---------------------------------------------------------------- 6. against last bill */

  function comparePanel(bill) {
    const prev = bill.previous;
    const s = bill.so_far;
    if (!prev || !(prev.days > 0) || !num(prev.total_dollars)) return null;
    const days = num(s.supply_days) && s.supply_days > 0 ? s.supply_days : bill.days_elapsed;
    if (!(days > 0)) return null;
    const bandSum = (s.bands || []).filter((b) => b.band !== 'free').reduce((a, b) => a + Number(b.dollars || 0), 0);
    const defs = [
      ['Supply', prev.supply_dollars, s.supply_dollars, 1],
      ['Bought', prev.import_dollars, bandSum, 1],
      ['Feed-in', prev.feed_in_dollars, s.feed_in_dollars, -1],
      ['Credit', prev.credit_dollars, s.credit_dollars, -1],
    ];
    const body = [];
    for (const [name, p, t, sign] of defs) {
      if (!num(p) || !num(t)) continue;
      if (Math.abs(p) < 0.005 && Math.abs(t) < 0.005) continue;
      const last = (sign * p) / prev.days;
      const now = (sign * t) / days;
      body.push(cmpRow(name, last, now));
    }
    const lastTotal = prev.total_dollars / prev.days;
    const nowTotal = s.total_dollars / days;
    body.push(cmpRow('Total', lastTotal, nowTotal, true));

    const diff = nowTotal - lastTotal;
    const table = h('table', { class: 'bill-cmp' },
      h('caption', { class: 'sr-only', text: 'Cost per day, this bill against last bill' }),
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col', text: 'Per day' }), h('th', { scope: 'col', text: 'Last bill' }),
        h('th', { scope: 'col', text: 'This bill' }), h('th', { scope: 'col', text: 'Change' }))),
      h('tbody', {}, body));
    const notes = [`Last bill ran ${SD.fmt.date(prev.from, 'short')} to ${SD.fmt.date(prev.to, 'short')} (${plural(prev.days, 'day', 'days')}).`];
    if (num(prev.credit_nights) && prev.credit_nights > 0 && prev.credit_nights < prev.days) {
      notes.push(`It earned the credit on ${prev.credit_nights} of ${prev.days} evenings.`);
    }
    return {
      title: 'Against last bill',
      summary: `${amt(nowTotal)} a day, was ${amt(lastTotal)}`,
      fig: [`${diff > 0.0049 ? '+' : ''}${amt(diff)}`, h('span', { class: 'bill-ph-u', text: '/day' })],
      figClass: diff < -0.0049 ? 'is-good' : diff > 0.0049 ? 'is-bad' : '',
      collapse: true,
      body: h('div', {}, table, h('p', { class: 'bill-note', text: notes.join(' ') })),
    };
  }

  function cmpRow(name, last, now, total) {
    const d = now - last;
    const same = Math.abs(d) < 0.005;
    return h('tr', { class: total ? 'is-tot' : null },
      h('th', { scope: 'row', text: name }),
      h('td', { text: amt(last) }),
      h('td', { text: amt(now) }),
      h('td', { class: same ? 'muted' : d < 0 ? 'is-good' : 'is-bad', text: same ? 'same' : `${d > 0 ? '+' : ''}${amt(d)}` }));
  }

  /* ---------------------------------------------------------------- 6b. what would Amber cost */

  /**
   * The made-up Amber plan against the plan in force, over the days that have
   * stored Amber prices. Left out (null) without Amber or without a covered day.
   */
  function amberPanel(amber) {
    const t = amber && amber.configured && amber.totals;
    if (!t || !(t.days > 0) || !num(t.current_dollars) || !num(t.dynamic_dollars)) return null;
    const diff = Number(t.difference_dollars);
    const row = (name, value, cls, total) => h('tr', { class: total ? 'is-tot' : null },
      h('th', { scope: 'row', text: name }), h('td', { class: cls || null, text: value }));
    const table = h('table', { class: 'bill-cmp bill-amber' },
      h('caption', { class: 'sr-only', text: 'Your bill so far on your plan and on Amber' }),
      h('tbody', {},
        row('Your plan', SD.fmt.dollars(t.current_dollars)),
        row('On Amber', SD.fmt.dollars(t.dynamic_dollars)),
        row('Difference', `${diff > 0.0049 ? '+' : ''}${amt(diff)}`, diff < -0.0049 ? 'is-good' : diff > 0.0049 ? 'is-bad' : 'muted', true)));
    const a = amber.assumptions || {};
    const notes = [
      `An estimate from your own usage on ${plural(t.days, 'day', 'days')} that have Amber prices`
        + (t.skipped_days > 0 ? ` (${plural(t.skipped_days, 'day', 'days')} left out for missing prices)` : '') + '.',
    ];
    if (num(a.supply_cents_per_day) && num(a.fee_cents_per_day)) {
      notes.push(`It assumes ${a.supply_cents_per_day}c a day supply and ${a.fee_cents_per_day}c a day Amber fee; change them in [amber.compare] in the config file.`);
    }
    return {
      title: 'What would Amber cost?',
      summary: `${plural(t.days, 'day', 'days')} covered`,
      fig: `${diff > 0.0049 ? '+' : ''}${amt(diff)}`,
      figClass: diff < -0.0049 ? 'is-good' : diff > 0.0049 ? 'is-bad' : '',
      collapse: true,
      body: h('div', {}, table, h('p', { class: 'bill-note', text: notes.join(' ') })),
    };
  }

  /* ---------------------------------------------------------------- 7. past bills */

  function historyPanel(bill) {
    const past = (Array.isArray(bill.history) ? bill.history : []).filter((b) => b && num(b.total_dollars) && b.to).slice(-5);
    if (!past.length) return null;
    const range = projectionRange(bill);
    const bars = past.map((b) => ({
      mon: monthShort(b.to), name: monthLong(b.to), value: Number(b.total_dollars), without: num(b.without_system_dollars) ? Number(b.without_system_dollars) : null, proj: false,
    }));
    if (range) {
      bars.push({ mon: monthShort(bill.to), name: monthLong(bill.to), value: range[0], without: num(bill.projected_without_system_dollars) ? Number(bill.projected_without_system_dollars) : null, proj: true });
    }
    const max = Math.max(1, ...bars.map((b) => b.value));
    const cols = { style: `grid-template-columns: repeat(${bars.length}, minmax(0, 1fr))` };
    const sumPast = past.reduce((a, b) => a + Number(b.total_dollars), 0);
    const withs = past.map((b) => b.without_system_dollars).filter(num).map(Number);
    const withSum = withs.length === past.length ? withs.reduce((a, b) => a + b, 0) : null;
    const dear = past.reduce((a, b) => (Number(b.total_dollars) > Number(a.total_dollars) ? b : a), past[0]);

    const label = `Bills: ${bars.map((b) => (b.proj ? `${b.name} about ${whole(b.value)} projected` : `${b.name} ${SD.fmt.dollars(b.value)}`)).join(', ')}.`
      + (bars.some((b) => b.without != null)
        ? ` Without the system each would have been about ${whole(Math.min(...bars.filter((b) => b.without != null).map((b) => b.without)))} to ${whole(Math.max(...bars.filter((b) => b.without != null).map((b) => b.without)))}.` : '');

    return {
      title: 'Your GloBird bills',
      summary: `${plural(past.length, 'bill', 'bills')}${withSum != null ? `, ~${whole(withSum)} without the system` : ''}`,
      fig: whole(sumPast),
      collapse: true,
      body: h('div', {},
        h('div', Object.assign({ class: 'bill-hist', role: 'img', 'aria-label': label }, cols),
          bars.map((b) => h('div', { class: 'bill-hcol' },
            h('span', { class: 'bill-amt fig', text: b.proj ? `~${whole(b.value)}` : whole(b.value) }),
            h('span', { class: `bill-hbar${b.proj ? ' is-proj' : ''}`, style: `height: ${Math.max(4, (Math.max(0, b.value) / max) * 112).toFixed(1)}px` })))),
        h('div', Object.assign({ class: 'bill-hlab', 'aria-hidden': 'true' }, cols),
          bars.map((b) => h('span', {}, h('b', { text: b.mon }), b.without != null ? `vs ${b.proj ? '~' : ''}${whole(b.without)}` : ''))),
        h('p', { class: 'bill-note' },
          past.length > 1 ? `${monthLong(dear.to)} was the dearest at ${SD.fmt.dollars(dear.total_dollars)}. ` : '',
          h('a', { class: 'bill-link', href: '#payback', text: 'See what the system saves in Payback' }))),
    };
  }

  /* ---------------------------------------------------------------- 8. tiles: cars and the free window */

  function tiles(bill) {
    const out = [];
    const c = bill.cars;
    if (c && num(c.sessions) && c.sessions > 0 && num(c.kwh)) {
      const self = num(c.self_kwh) && c.kwh > 0 ? Math.round((c.self_kwh / c.kwh) * 100) : null;
      const icon = svg('svg', { width: 18, height: 18, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', 'stroke-width': 1.6, 'aria-hidden': 'true' });
      icon.append(svg('path', { d: 'M4 16v-4l2-5h12l2 5v4z' }), svg('circle', { cx: 7.5, cy: 16.5, r: 1.5 }), svg('circle', { cx: 16.5, cy: 16.5, r: 1.5 }));
      out.push(h('a', { class: 'card bill-tile', href: '#cars' },
        h('h3', {}, icon, 'Cars'),
        h('p', { class: 'bill-tile-fig fig', text: num(c.cost_dollars) ? amt(c.cost_dollars) : SD.fmt.kwh(c.kwh, 0) }),
        h('p', { class: 'bill-tile-s', text: `${plural(c.sessions, 'session', 'sessions')}, ${SD.fmt.kwh(c.kwh, 0)}${self != null && self > 0 ? `. ${self}% came from your own solar and battery.` : '.'}` })));
    }
    const s = bill.so_far;
    if (num(s.free_value_dollars)) {
      const free = (s.bands || []).find((b) => b.band === 'free');
      out.push(h('div', { class: 'card bill-tile' },
        h('h3', {}, h('span', { class: 'bill-sw is-free', 'aria-hidden': 'true' }), 'Free window'),
        h('p', { class: 'bill-tile-fig fig', text: SD.fmt.dollars(s.free_value_dollars) }),
        h('p', { class: 'bill-tile-s', text: free && num(free.kwh) ? `${SD.fmt.kwh(free.kwh, 1)} at shoulder price` : 'What the free power would have cost' })));
    }
    return out.length ? h('div', { class: `bill-tiles${out.length === 1 ? ' is-one' : ''}` }, out) : null;
  }

  /* ---------------------------------------------------------------- 9. what's left to cut */

  function cutPanel(bill, tariff) {
    const s = bill.so_far;
    if (!num(s.supply_dollars) || !Array.isArray(s.bands)) return null;
    const byBand = Object.fromEntries(s.bands.map((b) => [b.band, b]));
    const shoulder = byBand.shoulder ? Number(byBand.shoulder.dollars) : 0;
    const peak = byBand.peak ? Number(byBand.peak.dollars) : 0;
    const supply = Number(s.supply_dollars);
    const power = shoulder + peak;
    const lead = peak > shoulder ? 'peak' : 'shoulder';
    const leadDollars = lead === 'peak' ? peak : shoulder;
    const missed = Array.isArray(bill.daily) ? bill.daily.filter((d) => d && d.credit === 'missed').length : 0;
    const nights = num(s.supply_days) && s.supply_days > 0 ? s.supply_days : bill.days_elapsed;

    const parts = [];
    if (power < 0.005) {
      parts.push(`No power has been bought at shoulder or peak prices so far. What is left is the ${SD.fmt.dollars(supply)} supply charge.`);
    } else {
      parts.push(supply > power
        ? `The supply charge (${SD.fmt.dollars(supply)}) is bigger than all the power you bought (${SD.fmt.dollars(power)}).`
        : `You have spent ${SD.fmt.dollars(power)} on power so far, on top of ${SD.fmt.dollars(supply)} supply.`);
      const piece = (band, dollars) => h('span', {}, h('b', { class: band === lead ? 'bill-hl' : null, text: SD.fmt.dollars(dollars) }), ` was ${band}`);
      const bits = [];
      if (shoulder > 0) bits.push(piece('shoulder', shoulder));
      if (peak > 0) bits.push(piece('peak', peak));
      const second = h('span', {}, 'Of that, ');
      bits.forEach((b, i) => { if (i) second.append(' and '); second.append(b); });
      second.append('.');
      if (missed > 0 && peak > 0) second.append(` ${plural(missed, 'evening', 'evenings')} missed the credit.`);
      parts.push(second);
      if (nights > 0 && leadDollars > 0) {
        const when = bandTimes(tariff, lead);
        parts.push(`About ${amt(leadDollars / nights)} a night of ${lead}${when ? `, bought between ${when}` : ''}.`);
      }
    }

    const charged = supply + power;
    const w = (v) => `${((v / Math.max(charged, 0.01)) * 100).toFixed(1)}%`;
    return {
      title: 'Left to cut',
      summary: power < 0.005 ? 'Supply is fixed. Nothing else was bought.' : `Supply is fixed. ${lead === 'peak' ? 'Peak' : 'Shoulder'} is what's biggest.`,
      fig: SD.fmt.dollars(leadDollars, 2),
      collapse: true,
      body: h('div', {},
        h('div', { class: 'bill-pbar', role: 'img', 'aria-label': `Of ${SD.fmt.dollars(charged)} charged: supply ${SD.fmt.dollars(supply)}, shoulder ${SD.fmt.dollars(shoulder)}, peak ${SD.fmt.dollars(peak)}.` },
          h('span', { style: `width: ${w(supply)}; background: var(--bar)` }),
          h('span', { style: `width: ${w(shoulder)}; background: var(--shoulder)` }),
          h('span', { style: `width: ${w(peak)}; background: var(--peak)` })),
        h('p', { class: 'label', text: `Supply ${SD.fmt.dollars(supply)} · shoulder ${SD.fmt.dollars(shoulder)} · peak ${SD.fmt.dollars(peak)}` }),
        h('p', { class: 'bill-cut-text' }, parts.flatMap((p, i) => (i ? [' ', p] : [p])))),
    };
  }

  /* ---------------------------------------------------------------- 10. how this is worked out */

  function renderHow(bill) {
    const source = bill.uses_globird_figures
      ? 'Days GloBird has posted use its own billed amounts. Newer days use our meter readings and your plan\'s prices until GloBird catches up.'
      : 'The figures are our estimate from the meter, using your plan\'s prices.';
    const d = (title, text) => h('details', {}, h('summary', { text }), h('p', { text: title }));
    return h('div', { class: 'bill-how a-how' },
      d('It adds up every complete day in this bill: supply, power bought, feed-in and the evening credit. Today joins once it is over and the figures are in, usually the next morning.', 'How "so far" is worked out'),
      d('The low end and the high end are two ways of finishing the bill: carry this bill\'s average day to the end of the period, or assume the days left are like last bill\'s average day. The real bill usually lands between the two.', 'How the projection range is worked out'),
      d(`${source} "Without system" is the same house load bought from the grid with no solar or battery.`, 'Where the numbers come from'));
  }

  /* ---------------------------------------------------------------- page */

  function periodLabel(bill) {
    return `${SD.fmt.date(bill.from, 'short')} to ${SD.fmt.date(bill.to, 'short')}`;
  }

  function renderHead(bill) {
    const elapsed = num(bill.days_elapsed) ? bill.days_elapsed : 0;
    const total = num(bill.days_total) ? bill.days_total : null;
    const left = total != null ? Math.max(0, total - elapsed) : null;
    const line = total != null
      ? `Day ${Math.min(elapsed + 1, total)} of ${total}${left != null ? ` · closes in ${plural(left, 'day', 'days')}` : ''}`
      : null;
    return h('header', { class: 'page-head bill-head' },
      h('h1', { class: 'bill-title', text: 'Bill' }),
      h('p', { class: 'bill-period' }, h('b', { text: periodLabel(bill) }), line ? h('br') : null, line));
  }

  function build(el, bill, extras) {
    const { compare, tariff, amber } = extras;
    const today = SD.local().date;
    const desktop = () => desktopQuery.matches;
    const ctx = { panels: [], open: readOpen(), desktop, apply: () => applyPanels(ctx) };

    el.append(renderHead(bill));
    if (bill.period_source && bill.period_source !== 'setting') {
      el.append(h('p', { class: 'bill-guess' }, 'Bill dates are a guess. ',
        h('a', { class: 'bill-link', href: '#admin', text: 'Set your bill start day in Settings.' })));
    }

    // The trust badge sits at the foot of the hero; on a phone it opens the day-by-day check.
    const check = safe(() => checkSummary(compare));
    let checkCard = null;
    let badge = null;
    const setExpanded = () => {
      if (!badge) return;
      if (desktop()) badge.removeAttribute('aria-expanded');
      else badge.setAttribute('aria-expanded', String(checkCard.classList.contains('is-open')));
    };
    if (check) {
      checkCard = safe(() => renderCheckCard(check));
      if (checkCard) {
        badge = safe(() => renderBadge(check, () => {
          if (desktop()) {
            checkCard.scrollIntoView({ behavior: 'smooth', block: 'center' });
            return;
          }
          checkCard.classList.toggle('is-open');
          setExpanded();
        }));
      }
    }

    const grid = h('div', { class: 'bill-grid' });
    const hero = safe(() => renderHero(bill, badge));
    grid.append(h('div', { class: 'bill-heroset a-hero' }, hero, badge ? checkCard : null));

    if (bill.days_elapsed > 0 && bill.so_far) {
      const panel = (key, area, spec) => {
        const built = safe(spec);
        if (!built) return null;
        return safe(() => makePanel(ctx, Object.assign({ key, area }, built)));
      };
      const side = h('div', { class: 'bill-side a-side' },
        safe(() => tiles(bill)),
        panel('amber', '', () => amberPanel(amber)),
        panel('cut', '', () => cutPanel(bill, tariff)));
      grid.append(...[
        panel('flow', 'a-flow', () => waterfall(bill)),
        panel('lines', 'a-lines', () => linesPanel(bill, tariff)),
        panel('days', 'a-days', () => daysPanel(bill, today)),
        panel('cmp', 'a-compare', () => comparePanel(bill)),
        panel('hist', 'a-history', () => historyPanel(bill)),
        side.children.length ? side : null,
      ].filter(Boolean));
    }
    const how = safe(() => renderHow(bill));
    if (how) grid.append(how);
    el.append(grid);

    ctx.apply();
    setExpanded();
    onQueryChange = () => { ctx.apply(); setExpanded(); };
    desktopQuery.addEventListener('change', onQueryChange);
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
              h('a', { class: 'card-link', href: '#admin', text: 'Open settings' })));
          return;
        }
        const today = SD.local().date;
        const last = bill.to < today ? bill.to : today;
        const [compare, tariff, amber] = await Promise.all([
          quiet(`/api/globird/compare?from=${bill.from}&to=${last}`),
          SD.tariff.get().catch(() => null),
          // {configured: false} without [amber], and the panel stays out.
          quiet(`/api/bill/compare-dynamic?from=${bill.from}&to=${last}`),
        ]);
        if (!alive) return;
        build(el, bill, { compare, tariff, amber });
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
