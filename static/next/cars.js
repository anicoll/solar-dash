/* Cars page: was charging the car cheap fuel?

   One block per car with data in the chosen month (cost per 100 km against a
   petrol car), when the charging happened drawn on the price ribbon, and the
   sessions in words. Same data and maths as the current dashboard's Cars view:
     /api/ev/cars      each car's months (km, petrol cost, charging cost),
                       the comparison petrol car and its L/100 km
     /api/ev/sessions  the month's charging sessions, costed from the house data
     /api/tariff       (through SD.tariff) the price bands for the ribbon
   The month lives in the address: #cars, or #cars/2026-09 for September. */
(function () {
  'use strict';

  const h = SD.h;
  const SESSIONS_SHOWN = 4;

  let root = null; // the page element while mounted
  let alive = false; // false after unmount, so late fetches draw nothing
  let carsPromise = null;
  let ribbon = null;
  let token = 0; // bumps on every draw, so a slow fetch cannot overwrite a newer one
  let showAll = false;

  /* ---------------------------------------------------------------- months */

  const MONTH_RE = /^\d{4}-(0[1-9]|1[0-2])$/;

  const thisMonth = () => SD.local().date.slice(0, 7);

  function shiftMonth(ym, by) {
    const n = Number(ym.slice(0, 4)) * 12 + (Number(ym.slice(5, 7)) - 1) + by;
    return `${Math.floor(n / 12)}-${String((n % 12) + 1).padStart(2, '0')}`;
  }

  /** The month named in the address, or this month. Never later than this month. */
  function monthFrom(rest) {
    const ym = (rest || '').split('/')[0];
    return MONTH_RE.test(ym) && ym <= thisMonth() ? ym : thisMonth();
  }

  const monthName = (ym) => SD.fmt.date(`${ym}-01`, 'month');
  /** Just "October" (the year is in the month selector). */
  const monthOnly = (ym) => monthName(ym).split(' ')[0];

  /** Unix seconds of local midnight (Adelaide) at the start of a 'YYYY-MM-DD'. */
  function localMidnight(dateStr) {
    const [y, m, d] = dateStr.split('-').map(Number);
    const target = Date.UTC(y, m - 1, d) / 1000;
    let ts = target - 9.5 * 3600; // close; the loop corrects for the exact offset
    for (let i = 0; i < 3; i++) {
      const l = SD.local(ts);
      const [ly, lm, ld] = l.date.split('-').map(Number);
      ts -= (Date.UTC(ly, lm - 1, ld) / 1000 - target) + l.minute * 60 + l.sec;
    }
    return ts;
  }

  /* ---------------------------------------------------------------- numbers */

  /** kWh a session put into the car (what it drew from the wall when known). */
  const sessionKwh = (s) => (s.energy_used_kwh == null ? s.energy_added_kwh : s.energy_used_kwh);

  const isHome = (s) => !s.geofence || /^home$/i.test(s.geofence.trim());

  /** Anything that cost nothing at all, with a cent of slack for rounding. */
  const costsNothing = (s) => s.cost != null && s.cost < 0.005;

  /** A car's figures for one month, or null when it did not drive or charge. */
  function carMonth(car, ym) {
    const m = (car.months || []).find((x) => x.month === ym);
    if (!m) return null;
    return m.km > 0 || m.home_kwh > 0 || m.away_kwh > 0 ? m : null;
  }

  /** Dollars per 100 km from a total and the km it covered. */
  const per100 = (dollars, km) => (dollars == null || !(km > 0) ? null : (dollars / km) * 100);

  /** "A sedan", "A Tesla Model 3": the comparison car with an article. */
  function withArticle(name) {
    if (!name) return 'A petrol car';
    return `${/^[aeiou]/i.test(name) ? 'An' : 'A'} ${name}`;
  }

  /* ---------------------------------------------------------------- headline block */

  function bar(label, value, maxValue, kind) {
    const pct = maxValue > 0 ? Math.max(2, (value / maxValue) * 100) : 2;
    return h('div', { class: 'cars-bar-row' },
      h('span', { class: 'cars-bar-label', text: label }),
      h('div', { class: 'cars-bar-track' },
        h('div', { class: `cars-bar cars-bar-${kind}${kind === 'petrol' ? ' hatch' : ''}`, style: `width: ${pct.toFixed(1)}%` })),
      h('span', { class: 'cars-bar-value fig', text: SD.fmt.dollars(value) }));
  }

  function carBlock(car, m, data, ym) {
    const when = ym === thisMonth() ? 'this month' : `in ${monthOnly(ym)}`;
    const model = car.model ? `Model ${car.model}` : (car.marketing_name || null);
    const name = car.name || car.label;
    const sub = [name, model !== name ? model : null, m.km > 0 ? `${Math.round(m.km).toLocaleString('en-AU')} km ${when}` : `no driving ${when}`]
      .filter(Boolean).join(' · ');

    const ev100 = m.km > 0 ? m.with_system.cents_per_km : null; // cents a km is dollars per 100 km
    const petrol100 = per100(m.petrol_cost, m.km);
    const saved = m.with_system.saved;

    const el = h('section', { class: 'cars-car', 'aria-label': `${name} ${when}` },
      h('p', { class: 'cars-sub', text: sub }));

    if (ev100 == null) {
      el.append(h('p', { class: 'lede', text: `${name} did not drive ${when}, so there is no cost per 100 km to show.` }));
      return el;
    }

    el.append(h('div', { class: 'cars-headline' },
      h('span', { class: 'cars-big fig', text: SD.fmt.dollars(ev100) }),
      h('span', { class: 'cars-big-unit', text: 'per 100 km' })));

    if (petrol100 == null || !data.configured) {
      el.append(h('p', { class: 'lede', text: 'Add a petrol price in Admin to compare this with a petrol car.' }));
      return el;
    }

    const litres = data.petrol_litres_per_100km;
    const grade = data.petrol_grade ? ` of ${data.petrol_grade}` : '';
    const sentence = h('p', { class: 'cars-sentence' },
      `${withArticle(car.compare_with)}${litres ? ` on ${litres.toFixed(1)} L/100 km${grade}` : ''} would cost `,
      h('b', { text: SD.fmt.dollars(petrol100) }), ' per 100 km.');
    if (saved != null && saved > 0) {
      sentence.append(' That\'s ', h('b', { text: SD.fmt.dollars(saved, Math.abs(saved) >= 100 ? 0 : 2) }), ` kept ${when}.`);
    }
    el.append(sentence);

    const max = Math.max(ev100, petrol100);
    el.append(h('div', {
      class: 'cars-bars', role: 'img',
      'aria-label': `Cost per 100 km: ${name} ${SD.fmt.dollars(ev100)}, petrol ${SD.fmt.dollars(petrol100)}.`,
    }, bar(name, ev100, max, 'ev'), bar('Petrol', petrol100, max, 'petrol')));
    return el;
  }

  /* ---------------------------------------------------------------- when it charged */

  /** Where a session sits on the 24 h day: [startMinute, endMinute] pieces. */
  function dayPieces(s) {
    const startLocal = SD.local(s.started_at);
    const end = s.ended_at == null ? Math.floor(Date.now() / 1000) : s.ended_at;
    const mins = Math.max(1, Math.min(24 * 60, Math.round((end - s.started_at) / 60)));
    const a = startLocal.minute;
    if (a + mins <= 1440) return [[a, a + mins]];
    return [[a, 1440], [0, a + mins - 1440]]; // runs past midnight
  }

  /** The dearest paid band a session overlapped, 'shoulder' when it cannot tell. */
  function paidBand(s, tariff) {
    for (const [a, b] of dayPieces(s)) {
      for (let m = a; m < b; m += 15) {
        if (SD.tariff.bandAt(tariff, m) === 'peak') return 'peak';
      }
    }
    return 'shoulder';
  }

  const bandWord = { free: 'free window', shoulder: 'shoulder', peak: 'peak' };

  function sessionKind(s, tariff) {
    if (!isHome(s) || s.cost == null) return 'away';
    return costsNothing(s) ? 'free' : paidBand(s, tariff);
  }

  function chargedCard(sessions, tariff) {
    const card = h('section', { class: 'card cars-when', 'aria-labelledby': 'cars-when-h' },
      h('div', { class: 'card-head' },
        h('h2', { class: 'section-title', id: 'cars-when-h', text: 'When it charged' }),
        h('span', { class: 'label', text: `${sessions.length} ${sessions.length === 1 ? 'session' : 'sessions'}` })));

    const kinds = sessions.map((s) => sessionKind(s, tariff));
    const costed = sessions.filter((s, i) => kinds[i] !== 'away');
    const totalKwh = costed.reduce((a, s) => a + (sessionKwh(s) || 0), 0);
    const freeKwh = costed.reduce((a, s) => a + (costsNothing(s) ? sessionKwh(s) || 0 : 0), 0);

    if (totalKwh > 0) {
      const paid = new Set(costed.filter((s) => !costsNothing(s)).map((s) => sessionKind(s, tariff)));
      const share = Math.round((freeKwh / totalKwh) * 100);
      const p = h('p', { class: 'cars-when-line' }, h('b', { text: `${share}% of charging cost nothing` }), ': free window or spare solar.');
      if (paid.size) {
        const n = costed.filter((s) => !costsNothing(s)).length;
        p.append(` ${n === 1 ? 'One session' : `${n} sessions`} paid ${[...paid].map((b) => bandWord[b]).join(' and ')}.`);
      }
      card.append(p);
    }

    // One thin row per session, placed by its time of day.
    const rows = h('div', {
      class: 'cars-rows', role: 'img',
      'aria-label': `${sessions.length} charging ${sessions.length === 1 ? 'session' : 'sessions'} by time of day. ${sessions.map((s, i) => `${SD.fmt.date(SD.local(s.started_at).date, 'short')} ${SD.fmt.time(s.started_at)}, ${kinds[i] === 'free' ? 'cost nothing' : kinds[i] === 'away' ? 'away from home' : `paid ${kinds[i]}`}`).join('; ')}.`,
    });
    sessions.forEach((s, i) => {
      const row = h('div', { class: 'cars-row' });
      for (const [a, b] of dayPieces(s)) {
        row.append(h('span', {
          class: `cars-seg cars-seg-${kinds[i]}${kinds[i] === 'away' ? ' hatch' : ''}`,
          style: `left: ${((a / 1440) * 100).toFixed(2)}%; width: ${Math.max(0.8, ((b - a) / 1440) * 100).toFixed(2)}%`,
        }));
      }
      rows.append(row);
    });
    card.append(rows);

    const ribbonBox = h('div', { class: 'cars-ribbon' });
    card.append(ribbonBox);
    if (tariff && tariff.configured) {
      ribbon = SD.ribbon(ribbonBox, tariff, { showCredit: false, labels: 'short' });
    }

    const legend = h('p', { class: 'cars-legend' },
      h('span', null, h('i', { class: 'swatch cars-key-free' }), 'Cost nothing'));
    const has = (k) => kinds.includes(k);
    if (has('shoulder')) legend.append(h('span', null, h('i', { class: 'swatch cars-key-line' }), 'Paid shoulder'));
    if (has('peak')) legend.append(h('span', null, h('i', { class: 'swatch band-peak' }), 'Paid peak'));
    if (has('away')) legend.append(h('span', null, h('i', { class: 'swatch hatch' }), 'Not costed'));
    card.append(legend);
    return card;
  }

  /* ---------------------------------------------------------------- sessions list */

  /** Where the energy came from, in words. */
  function sourceWords(s, tariff) {
    if (!isHome(s)) return 'away from home';
    if (s.cost == null) return s.ended_at == null ? 'charging now' : 'still being costed';
    const parts = [];
    const total = (s.solar_kwh || 0) + (s.battery_kwh || 0) + (s.grid_kwh || 0);
    const small = Math.max(0.3, total * 0.05); // ignore slivers
    if (s.solar_kwh > small) parts.push('solar');
    if (s.battery_kwh > small) parts.push('battery');
    if (s.grid_kwh > small) {
      if (costsNothing(s)) parts.push('free window');
      else {
        const minute = SD.local(s.started_at).minute;
        const band = sessionKind(s, tariff);
        const cents = band === 'peak' ? peakCents(tariff, s) : SD.tariff.centsAt(tariff, minute);
        parts.push(cents == null ? band : `${band}, ${SD.fmt.cents(cents)}`);
      }
    }
    return parts.length ? parts.join(' and ') : 'from the grid';
  }

  /** The import price in the dearest minute the session overlapped. */
  function peakCents(tariff, s) {
    let best = null;
    for (const [a, b] of dayPieces(s)) {
      for (let m = a; m < b; m += 15) {
        const c = SD.tariff.centsAt(tariff, m);
        if (c != null && (best == null || c > best)) best = c;
      }
    }
    return best;
  }

  function timeRange(s) {
    const a = SD.fmt.time(s.started_at);
    return s.ended_at == null ? `${a} to now` : `${a} to ${SD.fmt.time(s.ended_at)}`;
  }

  function sessionItem(s, tariff) {
    const kwh = sessionKwh(s);
    return h('li', { class: 'cars-session' },
      h('b', { class: 'cars-session-when', text: `${SD.fmt.date(SD.local(s.started_at).date, 'medium')} · ${timeRange(s)}` }),
      h('span', { class: 'cars-session-cost fig', text: s.cost == null ? SD.fmt.dollars(null) : SD.fmt.dollars(s.cost) }),
      h('span', { class: 'label cars-session-what', text: `${kwh == null ? SD.fmt.kwh(null) : SD.fmt.kwh(kwh)} · ${sourceWords(s, tariff)}` }));
  }

  function sessionsCard(sessions, tariff) {
    const newest = sessions.slice().reverse();
    const list = h('ul', { class: 'cars-sessions' });
    const card = h('section', { class: 'card cars-list', 'aria-labelledby': 'cars-sess-h' },
      h('h2', { class: 'section-title', id: 'cars-sess-h', text: 'Sessions' }), list);

    const draw = () => {
      list.replaceChildren(...(showAll ? newest : newest.slice(0, SESSIONS_SHOWN)).map((s) => sessionItem(s, tariff)));
    };
    draw();
    if (newest.length > SESSIONS_SHOWN) {
      const btn = h('button', { type: 'button', class: 'card-link cars-more', 'aria-expanded': String(showAll) });
      const label = () => { btn.textContent = showAll ? 'Show fewer' : `All ${newest.length} sessions`; };
      label();
      btn.addEventListener('click', () => {
        showAll = !showAll;
        btn.setAttribute('aria-expanded', String(showAll));
        label();
        draw();
      });
      card.append(btn);
    }
    return card;
  }

  /* ---------------------------------------------------------------- page */

  function monthBar(ym, first) {
    const go = (by) => SD.go(`cars/${shiftMonth(ym, by)}`);
    const prev = h('button', { type: 'button', class: 'icon-btn', 'aria-label': `Previous month, ${monthName(shiftMonth(ym, -1))}`, onclick: () => go(-1) }, '‹');
    const next = h('button', { type: 'button', class: 'icon-btn', 'aria-label': `Next month, ${monthName(shiftMonth(ym, 1))}`, onclick: () => go(1) }, '›');
    prev.disabled = !!first && ym <= first;
    next.disabled = ym >= thisMonth();
    return h('div', { class: 'cars-month' }, prev, h('span', { class: 'cars-month-name', 'aria-live': 'polite', text: monthName(ym) }), next);
  }

  function note(text, role) {
    return h('section', { class: 'card' }, h('p', { class: 'muted', role, text }));
  }

  async function draw(ym) {
    const my = ++token;
    showAll = false;
    if (ribbon) { ribbon.destroy(); ribbon = null; }
    const head = h('header', { class: 'page-head cars-head' }, h('h1', { class: 'cars-title', text: 'Cars' }));
    const body = h('div', { class: 'stack' });
    root.replaceChildren(head, body);
    head.append(monthBar(ym, null));
    body.append(h('p', { class: 'muted', role: 'status', text: 'Loading…' }));

    let data;
    try {
      data = await carsPromise;
    } catch (err) {
      if (my !== token || !alive) return;
      body.replaceChildren(note(`Could not load the cars: ${err.message}`, 'alert'));
      return;
    }
    if (my !== token || !alive) return;

    const cars = (data && data.cars) || [];
    if (!cars.length) {
      head.querySelector('.cars-month').remove();
      body.replaceChildren(note('No car history yet. It appears here once the car has been driven or charged and the history has synced.'));
      return;
    }
    const first = cars.map((c) => c.first_day).filter(Boolean).sort()[0];
    head.querySelector('.cars-month').replaceWith(monthBar(ym, first && first.slice(0, 7)));

    const active = cars.map((car) => ({ car, m: carMonth(car, ym) })).filter((x) => x.m);
    if (!active.length) {
      body.replaceChildren(note(`No driving or charging in ${monthName(ym)}.`));
      return;
    }
    body.replaceChildren(...active.map(({ car, m }) => carBlock(car, m, data, ym)));

    // The month's charging sessions, costed by the server.
    const lastDay = shiftMonth(ym, 1) + '-01';
    let sessions = [];
    let tariff = null;
    try {
      const probe = ym === thisMonth() ? SD.local().date : `${ym}-15`;
      const [res, t] = await Promise.all([
        SD.fetchJSON(`/api/ev/sessions?from=${localMidnight(`${ym}-01`)}&to=${localMidnight(lastDay) - 1}`),
        SD.tariff.get(probe).catch(() => null),
      ]);
      sessions = (res.sessions || []).filter((s) => sessionKwh(s) > 0);
      tariff = t;
    } catch (err) {
      if (my !== token || !alive) return;
      body.append(note(`Could not load the charging sessions: ${err.message}`, 'alert'));
      return;
    }
    if (my !== token || !alive || !sessions.length) return;
    body.append(chargedCard(sessions, tariff), sessionsCard(sessions, tariff));
  }

  SD.page('cars', {
    mount(el, hashRest) {
      root = el;
      alive = true;
      carsPromise = SD.fetchJSON('/api/ev/cars');
      carsPromise.catch(() => {}); // handled where it is awaited
      draw(monthFrom(hashRest));
    },
    update(hashRest) {
      draw(monthFrom(hashRest));
    },
    unmount() {
      alive = false;
      token++;
      if (ribbon) ribbon.destroy();
      ribbon = null;
      root = null;
    },
  });
})();
