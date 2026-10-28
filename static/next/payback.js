/* Payback page: when does the system pay for itself?

   Everything comes from /api/payback (the same data and dates as the current
   dashboard's Payback view). projected_payback is null once paid back, "never",
   or a month "YYYY-MM". A part's saved is null when it is counted with the
   first purchase, and `estimated` marks a rough figure. */
(function () {
  'use strict';

  const h = SD.h;
  const DAY = 86400;

  let alive = false;
  let root = null;
  let relayout = null; // redraws the timeline labels when the width changes
  let observer = null;

  /* ---------------------------------------------------------------- helpers */

  const isMonth = (p) => typeof p === 'string' && /^\d{4}-\d{2}$/.test(p);
  const monthLabel = (ym) => SD.fmt.date(`${ym}-01`, 'month');
  const dollars0 = (n) => SD.fmt.dollars(n, 0);
  /** "28 Oct 2026" for a 'YYYY-MM-DD' date. */
  const dayLabel = (d) => `${SD.fmt.date(d, 'short')} ${d.slice(0, 4)}`;
  /** Unix seconds for a 'YYYY-MM-DD' date, at noon UTC (the day is what matters). */
  const dayTs = (d) => Date.UTC(+d.slice(0, 4), +d.slice(5, 7) - 1, +d.slice(8, 10), 12) / 1000;
  /** Compact dollars for bar labels: $2.4k. */
  const short = (n) => (Math.abs(n) >= 1000 ? `$${(n / 1000).toFixed(1)}k` : dollars0(n));
  const joinNames = (names) => (names.length > 1 ? `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}` : names[0] || '');

  function note(text, role) {
    return h('section', { class: 'card' }, h('p', { class: 'muted', role, text }));
  }

  /* ---------------------------------------------------------------- headline */

  function verdictText(pb) {
    const p = pb.projected_payback;
    if (p === null) return 'Paid for itself.';
    if (p === 'never') return 'Not on track to pay for itself at this rate.';
    return `Paid for itself by ${monthLabel(p)}.`;
  }

  function progressLine(pb) {
    const perMonth = (pb.rate_per_day * 365.25) / 12;
    const parts = [`You're ${pb.recovered_pct.toFixed(1)}% of the way.`];
    if (pb.net < 0) {
      parts.push(perMonth > 0 ? `${dollars0(-pb.net)} to go, at about ${dollars0(perMonth)} a month.` : `${dollars0(-pb.net)} to go.`);
    } else {
      parts.push(`${dollars0(pb.net)} ahead of the cost.`);
    }
    return parts.join(' ');
  }

  /* ---------------------------------------------------------------- timeline */

  /** The first part's own payback sentence, when there is more than one part. */
  function firstPartLine(pb) {
    const first = pb.parts.length > 1 ? pb.parts[0] : null;
    if (!first || first.saved == null) return null;
    const p = first.projected_payback;
    const says = p === null ? 'has paid for itself' : p === 'never' ? "isn't on track to pay for itself at this rate" : `pays for itself around ${monthLabel(p)}`;
    return `The first system ${says}.`;
  }

  /** The dated marks along the bar, as {pct, text, kind}. */
  function timelineModel(pb) {
    const todayStr = SD.local().date;
    const start = dayTs(pb.purchases[0].date);
    const today = dayTs(todayStr);
    const proj = isMonth(pb.projected_payback) ? dayTs(`${pb.projected_payback}-15`) : null;
    // The bar ends at the payback; if that is already close (or past), leave a little room after today.
    const end = Math.max(proj || today, today + 30 * DAY);
    const span = Math.max(end - start, DAY);
    const pct = (ts) => Math.min(100, Math.max(0, ((ts - start) / span) * 100));

    const marks = [];
    marks.push({ pct: 0, text: SD.fmt.date(pb.purchases[0].date, 'month'), kind: 'start' });
    pb.purchases.slice(1).forEach((p) => marks.push({ pct: pct(dayTs(p.date)), text: p.name, sub: SD.fmt.date(p.date, 'month'), kind: 'buy' }));
    marks.push({ pct: pct(today), text: 'Today', kind: 'today', above: true });
    if (pb.parts.length > 1) {
      pb.parts.forEach((part, i) => {
        // A part's own payback, when it is still to come and not the same as the whole system's.
        if (part.saved == null || !isMonth(part.projected_payback) || part.projected_payback === pb.projected_payback) return;
        marks.push({ pct: Math.max(pct(today), pct(dayTs(`${part.projected_payback}-15`))), text: monthLabel(part.projected_payback), kind: 'part', partIndex: i });
      });
    }
    if (proj) marks.push({ pct: 100, text: monthLabel(pb.projected_payback), kind: 'end' });
    return { marks, todayPct: pct(today), hasEnd: !!proj };
  }

  function timelineCard(pb) {
    const card = h('section', { class: 'card pb-timeline', 'aria-label': 'Payback timeline' });
    const model = timelineModel(pb);
    const stage = h('div', { class: 'pb-stage' });
    const track = h('div', { class: 'pb-track' },
      h('div', { class: 'pb-done', style: `width: ${model.todayPct.toFixed(2)}%` }),
      model.hasEnd ? h('div', { class: 'pb-todo hatch-bar', style: `left: ${model.todayPct.toFixed(2)}%` }) : null);
    stage.append(track);

    const labels = [];
    for (const m of model.marks) {
      const tick = h('div', { class: `pb-tick pb-tick-${m.kind}`, style: `left: ${m.pct.toFixed(2)}%` });
      if (m.kind === 'end') tick.append(h('span', { class: 'pb-sun' }));
      if (m.kind === 'today') tick.append(h('span', { class: 'pb-now-dot' }));
      if (m.kind !== 'start') stage.append(tick);
      const label = h('div', { class: `pb-label pb-label-${m.kind}` }, h('span', { text: m.text }), m.sub ? h('span', { class: 'pb-label-sub', text: m.sub }) : null);
      stage.append(label);
      labels.push({ m, label });
    }
    card.append(stage);

    // Hand-placed labels would run into each other on a phone, so each goes in the
    // first row where it fits. Widths are measured after layout.
    relayout = () => {
      const width = stage.clientWidth;
      if (!width) return;
      const rows = [[], [], []];
      const sorted = labels.slice().sort((a, b) => (a.m.kind === 'today' ? -1 : b.m.kind === 'today' ? 1 : a.m.pct - b.m.pct));
      for (const { m, label } of sorted) {
        const w = label.offsetWidth;
        const centre = (m.pct / 100) * width;
        let x0 = centre - w / 2;
        let align = 'center';
        if (m.pct === 0 || x0 < 0) { x0 = 0; align = 'left'; }
        if (m.kind === 'end' || x0 + w > width) { x0 = Math.max(0, width - w); align = 'right'; }
        label.style.left = `${x0}px`;
        label.dataset.align = align;
        if (m.above) { label.style.top = '0px'; continue; }
        let row = rows.findIndex((r) => r.every(([a, b]) => x0 >= b + 8 || x0 + w <= a - 8));
        if (row < 0) row = rows.length - 1;
        rows[row].push([x0, x0 + w]);
        label.style.top = `${74 + row * 34}px`;
      }
      stage.style.height = `${74 + Math.max(1, rows.filter((r) => r.length).length) * 34}px`;
    };

    const first = firstPartLine(pb);
    card.append(h('p', { class: 'pb-note' }, first ? `${first} ` : '', 'Saved so far: ', h('b', { text: dollars0(pb.saved) }), '.'));
    return card;
  }

  /* ---------------------------------------------------------------- each purchase */

  function partStatus(p) {
    const pr = p.projected_payback;
    if (pr === null) return 'paid back';
    if (pr === 'never') return 'not on track at this rate';
    return p.estimated ? `~${monthLabel(pr)}, rough estimate` : `pays back ~${monthLabel(pr)}`;
  }

  function partsCard(pb) {
    const card = h('section', { class: 'card', 'aria-labelledby': 'pb-parts-h' }, h('h2', { class: 'section-title', id: 'pb-parts-h', text: 'Each purchase' }));
    const first = pb.parts[0];
    for (const p of pb.parts) {
      const row = h('div', { class: 'pb-part' });
      const date = SD.fmt.date(p.date, 'month');
      if (p.saved == null) {
        row.append(
          h('div', { class: 'pb-part-top' }, h('b', { text: p.name })),
          h('div', { class: 'pb-part-sub', text: `${date} · ${dollars0(p.cost)} · counted with ${first.name}` }));
      } else {
        const pct = Math.round(p.recovered_pct);
        row.append(
          h('div', { class: 'pb-part-top' }, h('b', { text: p.name }), h('span', { class: 'fig', text: `${pct}%${p.estimated ? ' (rough)' : ''}` })),
          h('div', { class: 'pb-part-sub', text: `${date} · ${dollars0(p.cost)} · ${partStatus(p)}` }),
          h('div', {
            class: 'pb-meter', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(Math.min(100, pct)),
            'aria-label': `${p.name}: ${pct}% paid back`,
          }, h('div', { class: `pb-meter-fill${p.estimated ? ' is-estimate' : ''}`, style: `width: ${Math.max(0, Math.min(100, p.recovered_pct))}%` })));
      }
      card.append(row);
    }
    const rough = pb.parts.find((p) => p.estimated);
    if (rough) card.append(h('p', { class: 'pb-fine', text: `${rough.name} is a rough estimate of what it added on top of the earlier system. The purchases add up to the total.` }));
    return card;
  }

  /* ---------------------------------------------------------------- saved each year */

  function yearsCard(pb) {
    const years = pb.by_year || [];
    if (!years.length) return null;
    const max = Math.max(...years.map((y) => y.saved), 1);
    const cols = h('div', { class: 'pb-years', style: `grid-template-columns: repeat(${years.length}, minmax(0, 1fr))` });
    for (const y of years) {
      cols.append(h('div', { class: 'pb-year' },
        h('span', { class: 'pb-year-val fig', text: short(y.saved) }),
        h('div', { class: `pb-year-bar${y.partial ? ' hatch' : ''}`, style: `height: ${Math.max(2, (y.saved / max) * 100).toFixed(1)}%` })));
    }
    const axis = h('div', { class: 'pb-years-axis', style: `grid-template-columns: repeat(${years.length}, minmax(0, 1fr))` },
      ...years.map((y) => h('span', { text: String(y.year) })));
    const card = h('section', { class: 'card', 'aria-labelledby': 'pb-years-h' },
      h('h2', { class: 'section-title', id: 'pb-years-h', text: 'Saved each year' }),
      h('div', {
        role: 'img',
        'aria-label': `Dollars saved each year: ${years.map((y) => `${y.year} ${dollars0(y.saved)}${y.partial ? ' so far' : ''}`).join(', ')}.`,
      }, cols, axis));
    if (years.some((y) => y.partial)) card.append(h('p', { class: 'pb-fine', text: 'Striped bars are part years.' }));
    return card;
  }

  /* ---------------------------------------------------------------- what moves the date */

  function moveCard(pb) {
    const items = [];
    const perMonth = (pb.rate_per_day * 365.25) / 12;
    if (pb.net < 0 && perMonth > 0) {
      items.push(`Uses your last 12 months of savings, about ${dollars0(pb.last_12_months_saved)}, or ${dollars0(perMonth)} a month.`);
    }
    if (pb.price_rise_percent > 0) {
      let text = `Assumes power prices rise ${pb.price_rise_percent}% a year.`;
      if (pb.net < 0 && pb.rate_per_day > 0 && isMonth(pb.projected_payback)) {
        // With flat prices the rest takes -net / rate days: the same maths as the server with no rise.
        const flat = SD.local(Date.now() / 1000 + Math.ceil(-pb.net / pb.rate_per_day) * DAY).date.slice(0, 7);
        if (flat > pb.projected_payback) text += ` Flat prices push it to about ${monthLabel(flat)}.`;
      }
      items.push(text);
    }
    if (!items.length) return null;
    return h('section', { class: 'card pb-move', 'aria-labelledby': 'pb-move-h' },
      h('h2', { class: 'section-title', id: 'pb-move-h', text: 'What moves the date' }),
      h('ul', null, ...items.map((t) => h('li', { text: t }))));
  }

  /* ---------------------------------------------------------------- timeline of events */

  function eventsCard(pb) {
    const events = pb.timeline || [];
    if (!events.length) return null;
    let plans = 0;
    const list = h('ul', { class: 'pb-events' }, ...events.map((e) => {
      const text = e.kind === 'purchase' ? e.label : `${plans++ ? 'Moved to' : 'On'} ${e.label}`;
      return h('li', null,
        h('span', { class: 'pb-event-date', text: dayLabel(e.date) }),
        h('span', { class: 'pb-event-text' }, text, e.cost != null ? h('span', { class: 'label', text: ` · ${dollars0(e.cost)}` }) : null));
    }));
    return h('details', { class: 'pb-details' }, h('summary', { text: 'Timeline of plans and purchases' }), list);
  }

  /* ---------------------------------------------------------------- page */

  function render(pb) {
    if (!pb.configured || !pb.purchases.length) {
      return [h('header', { class: 'page-head' }, h('h1', { class: 'pb-title', text: 'Payback' })),
        note('Add a plan and at least one purchase in Admin to see when the system pays for itself.')];
    }
    const names = joinNames(pb.purchases.map((p) => p.name));
    const head = h('header', { class: 'page-head pb-head' },
      h('p', { class: 'date-line', text: `${names} · ${dollars0(pb.total_cost)}` }),
      h('h1', { class: 'verdict pb-verdict', text: verdictText(pb) }),
      h('p', { class: 'lede pb-progress', text: progressLine(pb) }));
    const cards = [timelineCard(pb), partsCard(pb), yearsCard(pb), moveCard(pb), eventsCard(pb)].filter(Boolean);
    return [head, h('div', { class: 'stack pb-stack' }, ...cards)];
  }

  SD.page('payback', {
    mount(el) {
      alive = true;
      root = el;
      el.append(h('p', { class: 'muted', role: 'status', text: 'Loading…' }));
      SD.fetchJSON('/api/payback').then((pb) => {
        if (!alive) return;
        el.replaceChildren(...render(pb));
        if (relayout) {
          relayout();
          if (window.ResizeObserver) {
            observer = new ResizeObserver(() => relayout && relayout());
            observer.observe(el);
          }
          if (document.fonts && document.fonts.ready) document.fonts.ready.then(() => alive && relayout && relayout());
        }
      }).catch((err) => {
        if (alive) el.replaceChildren(note(`Could not load the payback: ${err.message}`, 'alert'));
      });
    },
    unmount() {
      alive = false;
      relayout = null;
      if (observer) observer.disconnect();
      observer = null;
      root = null;
    },
  });
})();
