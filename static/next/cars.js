/* Cars page: was charging the car cheap fuel?

   One card per car, so it is clear where one car ends and the next begins:
   the TeslaMate cars with data in the chosen month first, then the manual cars
   (a plug-in hybrid, say) TeslaMate does not track. A card is an accordion. Its
   header is a number-plate name, the model, the source and when it was owned,
   and closed it also shows the month's headline figure. Inside:
     TeslaMate car  cost per 100 km against a petrol car, when the charging
                    happened drawn on the price ribbon, then the sessions in words
     manual car     petrol per 100 km, a few stats, then the month's fills
   Same data and maths as the current dashboard's Cars view:
     /api/ev/cars      each car's months (km, petrol cost, charging cost),
                       the comparison petrol car and its L/100 km, owned_since
     /api/ev/sessions  the month's charging sessions, costed from the house data
                       (they carry no car id, so they go in a card only when one
                       TeslaMate car was active that month)
     /api/tariff       (through SD.tariff) the price bands for the ribbon
     /api/fuel/cars    the manual cars: totals, L/100 km, owned_since and the fills
   The manual cars' fills are added, changed and deleted here:
     POST /api/admin/cars/ID/fills     PUT/DELETE /api/admin/fills/ID
   The address names the month and a car to open: #cars, #cars/2026-09,
   #cars/zippy or #cars/2026-09/zippy. Which cars are open is kept in
   localStorage by car key (the name, plus the model when two cars share it). */
(function () {
  'use strict';

  const h = SD.h;
  const SESSIONS_SHOWN = 4;
  const OPEN_KEY = 'solar-dash.cars.open';

  let root = null; // the page element while mounted
  let alive = false; // false after unmount, so late fetches draw nothing
  let carsPromise = null;
  let fuelPromise = null; // never rejects: resolves {cars} or {error}
  let manualBox = null; // where the manual cards are drawn, so a save redraws only them
  let manualMonth = null; // the month manualBox shows
  let manualKeys = null; // car_id -> car key of the manual cars
  let singleCar = false; // only one car in all: its card starts open
  let opened = {}; // car key -> true/false: the cards left open or closed
  let adding = null; // id of the manual car whose Add fill form is open
  let editing = null; // id of the fill being edited
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

  /** The car named after the month in the address (#cars/2026-09/zippy or #cars/zippy), or ''. */
  function carFrom(rest) {
    const parts = (rest || '').split('/');
    return (MONTH_RE.test(parts[0]) ? parts[1] : parts[0]) || '';
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

  /** Dollars, but under a dollar in cents: 0.0204 -> '2c' (rounded), 0.009 -> '0.9c'. */
  function money(dollars, round) {
    if (dollars == null || !Number.isFinite(dollars)) return SD.fmt.dollars(null);
    return Math.abs(dollars) < 1 ? SD.fmt.cents(dollars * 100, { round: !!round }) : SD.fmt.dollars(dollars);
  }

  const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

  /** "1 week", "3 weeks", "4 months", "1 year 2 months": how long from a 'YYYY-MM-DD' until today. */
  function sinceWords(from) {
    const [y, m, d] = from.split('-').map(Number);
    const [ty, tm, td] = SD.local().date.split('-').map(Number);
    let months = (ty - y) * 12 + (tm - m) - (td < d ? 1 : 0);
    if (months < 0) months = 0;
    if (months >= 12) {
      const years = Math.floor(months / 12);
      return [plural(years, 'year'), months % 12 ? plural(months % 12, 'month') : null].filter(Boolean).join(' ');
    }
    if (months >= 1) return plural(months, 'month');
    const days = Math.max(0, Math.round((Date.UTC(ty, tm - 1, td) - Date.UTC(y, m - 1, d)) / 86400000));
    return days >= 7 ? plural(Math.floor(days / 7), 'week') : days === 1 ? '1 day' : days ? `${days} days` : 'today';
  }

  /** "1 Jul 2026" from a 'YYYY-MM-DD'. */
  const dateWithYear = (ymd) => {
    const [y, m, d] = ymd.split('-').map(Number);
    return new Date(Date.UTC(y, m - 1, d)).toLocaleDateString('en-AU', { timeZone: 'UTC', day: 'numeric', month: 'short', year: 'numeric' });
  };

  /** "27 June" from a 'YYYY-MM-DD'. */
  const dayAndMonth = (ymd) => {
    const [y, m, d] = ymd.split('-').map(Number);
    return new Date(Date.UTC(y, m - 1, d)).toLocaleDateString('en-AU', { timeZone: 'UTC', day: 'numeric', month: 'long' });
  };

  /** Dollars per 100 km from a total and the km it covered. */
  const per100 = (dollars, km) => (dollars == null || !(km > 0) ? null : (dollars / km) * 100);

  /** "A sedan", "A Tesla Model 3": the comparison car with an article. */
  function withArticle(name) {
    if (!name) return 'A petrol car';
    return `${/^[aeiou]/i.test(name) ? 'An' : 'A'} ${name}`;
  }

  /* ---------------------------------------------------------------- car cards */

  const slug = (text) => String(text || '').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');

  /** Each car's key for the address and for remembering it is open: its name, plus the
      model when two cars share a name, plus the source and id if that is not enough. */
  function carKeys(all) {
    const keys = new Map();
    const same = (c, f) => all.filter((o) => f(o) === f(c)).length > 1;
    const base = (c) => slug(c.name || c.label) || 'car';
    const withModel = (c) => (same(c, base) ? `${base(c)}-${slug(c.model ? (c.source === 'manual' ? c.model : `model ${c.model}`) : '')}`.replace(/-$/, '') : base(c));
    for (const c of all) {
      keys.set(c, same(c, withModel) ? `${withModel(c)}-${c.source === 'manual' ? 'm' : 't'}${c.car_id}` : withModel(c));
    }
    return keys;
  }

  function loadOpen() {
    try {
      const saved = JSON.parse(localStorage.getItem(OPEN_KEY));
      return saved && typeof saved === 'object' && !Array.isArray(saved) ? saved : {};
    } catch { return {}; }
  }

  function saveOpen() {
    try { localStorage.setItem(OPEN_KEY, JSON.stringify(opened)); } catch { /* not saved; fine */ }
  }

  const isOpen = (key) => (key in opened ? !!opened[key] : singleCar);

  /** One car's card. `info`: key, name, model, meta, source, owned, big, small. The header
      button opens and closes the body; closed, it also shows the month's headline figure. */
  function carCard(info, ...content) {
    const open = isOpen(info.key);
    const id = `cars-${info.key}`;
    const body = h('div', { class: 'cars-body', id: `${id}-body`, hidden: !open }, content);
    const toggle = h('button', { type: 'button', class: 'cars-toggle', 'aria-expanded': String(open), 'aria-controls': `${id}-body` },
      h('span', { class: 'cars-plate', id: `${id}-h`, text: info.name }),
      info.model ? h('span', { class: 'cars-model', text: info.model }) : null,
      h('span', { class: 'cars-meta', text: info.meta }),
      info.owned ? h('span', { class: 'cars-owned', text: info.owned }) : null,
      h('span', { class: 'cars-right' },
        sourceBadge(info.source),
        h('span', { class: 'cars-sum' }, h('b', { class: 'fig', text: info.big }), h('span', { text: info.small })),
        h('span', { class: 'cars-chev', 'aria-hidden': 'true', text: '›' })));
    toggle.addEventListener('click', () => {
      const now = toggle.getAttribute('aria-expanded') !== 'true';
      toggle.setAttribute('aria-expanded', String(now));
      body.hidden = !now;
      opened[info.key] = now;
      saveOpen();
    });
    return h('article', { class: 'card cars-card', id, 'aria-labelledby': `${id}-h` },
      h('header', { class: 'cars-card-head' }, toggle), body);
  }

  /** "Owned since 1 Jul 2026 · 4 months", or '' when there is no date. `since` falls back
      to the first day of data; a TeslaMate car says where its own data starts. */
  function ownedLine(ownedSince, fallback, dataFrom) {
    const date = ownedSince || fallback;
    if (!date) return '';
    const parts = [`Owned since ${dateWithYear(date)}`, sinceWords(date)];
    if (dataFrom && date < dataFrom) parts.push(`TeslaMate data from ${SD.fmt.date(dataFrom, 'short')}`);
    return parts.join(' · ');
  }

  /** A titled part of a card body, with a label at the right of the title. */
  function section(id, title, label, ...content) {
    return h('section', { class: 'cars-sec', 'aria-labelledby': id },
      h('div', { class: 'card-head' },
        h('h3', { class: 'section-title', id, text: title }),
        label ? h('span', { class: 'label', text: label }) : null),
      content);
  }

  /* ---------------------------------------------------------------- headline block */

  function bar(label, value, maxValue, kind) {
    const pct = maxValue > 0 ? Math.max(2, (value / maxValue) * 100) : 2;
    return h('div', { class: 'cars-bar-row' },
      h('span', { class: 'cars-bar-label', text: label }),
      h('div', { class: 'cars-bar-track' },
        h('div', { class: `cars-bar cars-bar-${kind}${kind === 'petrol' ? ' hatch' : ''}`, style: `width: ${pct.toFixed(1)}%` })),
      h('span', { class: 'cars-bar-value fig', text: money(value, true) }));
  }

  /** The TeslaMate car's card. Its charging sections are added to `slot` once the sessions load. */
  function carBlock(car, m, data, ym, key, slot) {
    const when = ym === thisMonth() ? 'this month' : `in ${monthOnly(ym)}`;
    const name = car.name || car.label;
    const ev100 = m.km > 0 ? m.with_system.cents_per_km : null; // cents a km is dollars per 100 km
    const petrol100 = per100(m.petrol_cost, m.km);
    const saved = m.with_system.saved;
    const keptDollars = saved != null && saved > 0 ? SD.fmt.dollars(saved, saved >= 100 ? 0 : 2) : null;

    const info = {
      key, name, source: 'teslamate',
      model: car.model ? `Tesla Model ${car.model}` : (car.marketing_name || null),
      meta: [car.marketing_name && car.model ? car.marketing_name : car.trim, m.km > 0 ? `${Math.round(m.km).toLocaleString('en-AU')} km ${when}` : `no driving ${when}`]
        .filter(Boolean).join(' · '),
      owned: ownedLine(car.owned_since, car.first_day, car.first_day),
      big: ev100 == null ? '–' : money(ev100, true),
      small: ev100 == null ? `no driving ${when}` : `per 100 km${keptDollars ? ` · ${keptDollars} kept` : ''}`,
    };
    const content = [];

    if (ev100 == null) {
      content.push(h('p', { class: 'lede', text: `${name} did not drive ${when}, so there is no cost per 100 km to show.` }));
    } else {
      content.push(h('div', { class: 'cars-headline' },
        h('span', { class: 'cars-big fig', text: money(ev100, true) }),
        h('span', { class: 'cars-big-unit', text: 'per 100 km' })));
      if (petrol100 == null || !data.configured) {
        content.push(h('p', { class: 'lede', text: 'Add a petrol price in Admin to compare this with a petrol car.' }));
      } else {
        const litres = data.petrol_litres_per_100km;
        const grade = data.petrol_grade ? ` of ${data.petrol_grade}` : '';
        const sentence = h('p', { class: 'cars-sentence' },
          `${withArticle(car.compare_with)}${litres ? ` on ${litres.toFixed(1)} L/100 km${grade}` : ''} would cost `,
          h('b', { text: money(petrol100) }), ' per 100 km.');
        if (keptDollars) sentence.append(` ${name} kept `, h('b', { text: keptDollars }), ` in your pocket ${when}.`);
        const max = Math.max(ev100, petrol100);
        content.push(sentence, h('div', {
          class: 'cars-bars', role: 'img',
          'aria-label': `Cost per 100 km: ${name} ${money(ev100, true)}, petrol ${money(petrol100)}.`,
        }, bar(name, ev100, max, 'ev'), bar('Petrol', petrol100, max, 'petrol')));
      }
    }
    content.push(slot);
    return carCard(info, content);
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

  function chargedCard(sessions, tariff, name, ym, key) {
    const kwhAll = sessions.reduce((a, s) => a + (sessionKwh(s) || 0), 0);
    const card = section(`cars-${key}-when`, `When ${name} charged in ${monthOnly(ym)}`, `${plural(sessions.length, 'session')} · ${SD.fmt.kwh(kwhAll)}`);
    card.classList.add('cars-when');

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
      h('span', { class: 'cars-session-cost fig', text: money(s.cost) }),
      h('span', { class: 'label cars-session-what', text: `${kwh == null ? SD.fmt.kwh(null) : SD.fmt.kwh(kwh)} · ${sourceWords(s, tariff)}` }));
  }

  function sessionsCard(sessions, tariff, name, ym, key) {
    const newest = sessions.slice().reverse();
    const list = h('ul', { class: 'cars-sessions' });
    const total = sessions.reduce((a, s) => a + (s.cost || 0), 0);
    const card = section(`cars-${key}-sess`, `${name}'s charging sessions in ${monthOnly(ym)}`, `${plural(sessions.length, 'session')} · ${money(total)}`, list);

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

  /* ---------------------------------------------------------------- manual cars */

  const KIND_WORDS = { phev: 'Plug-in hybrid', petrol: 'Petrol', hybrid: 'Hybrid', diesel: 'Diesel' };
  /** A difference in c/L smaller than this is not worth a marker. */
  const MARK_CENTS = 2;

  /** The small pill saying where a car's numbers come from. */
  function sourceBadge(source) {
    return h('span', { class: 'src-badge', text: source === 'manual' ? 'Manual' : 'TeslaMate' });
  }

  const litresText = (l) => `${l.toFixed(1)} L`;
  const cplText = (c) => `${c.toFixed(1)}c/L`;
  const economyText = (v) => `${v.toFixed(1)} L/100 km`;
  const kmText = (km) => `${Math.round(km).toLocaleString('en-AU')} km`;
  /** A typed number: a comma is taken as a decimal point; empty or bad is null. */
  const typed = (text) => {
    const t = text.trim().replace(',', '.');
    return t === '' || !Number.isFinite(Number(t)) ? null : Number(t);
  };

  /** One headline figure with its label and a line of small print. */
  function stat(value, label, small) {
    return h('div', { class: 'cars-stat' },
      h('span', { class: 'cars-stat-value fig fig-lg', text: value }),
      h('span', { class: 'cars-stat-label', text: label }),
      small ? h('span', { class: 'label', text: small }) : null);
  }

  /** The marker beside a price that is above or below the month's average. */
  function priceMark(fill) {
    const diff = fill.avg_cents_per_litre == null ? 0 : fill.cents_per_litre - fill.avg_cents_per_litre;
    if (Math.abs(diff) < MARK_CENTS) return null;
    const words = `${Math.abs(diff).toFixed(0)}c ${diff > 0 ? 'above' : 'below'} the Adelaide average`;
    return h('span', { class: `cars-mark ${diff > 0 ? 'is-above' : 'is-below'}`, title: words },
      h('span', { 'aria-hidden': 'true', text: diff > 0 ? '▲' : '▼' }),
      h('span', { class: 'sr-only', text: ` (${words})` }));
  }

  /** A tidy fill row; Edit swaps it for the form. */
  function fillRow(car, fill) {
    const cell = (cls, text) => h('span', { class: `cars-cell ${cls}${text == null ? ' is-empty' : ''}`, text: text == null ? '–' : text });
    const price = h('span', { class: 'cars-cell cars-c-cpl' }, cplText(fill.cents_per_litre), priceMark(fill));
    const edit = h('button', {
      type: 'button', class: 'card-link cars-edit',
      'aria-label': `Edit the fill on ${SD.fmt.date(fill.date, 'medium')}`,
      onclick: () => { editing = fill.id; adding = null; drawManual(); },
    }, 'Edit');
    return h('li', { class: 'cars-fill' },
      h('b', { class: 'cars-cell cars-c-date', text: SD.fmt.date(fill.date, 'medium') + (fill.full_tank ? '' : ' · part fill') }),
      h('span', { class: 'cars-cell cars-c-cost fig', text: SD.fmt.dollars(fill.cost_dollars) }),
      cell('cars-c-litres', litresText(fill.litres)),
      price,
      cell('cars-c-odo', fill.odometer_km == null ? null : kmText(fill.odometer_km)),
      cell('cars-c-eco', fill.l_per_100km == null ? null : economyText(fill.l_per_100km)),
      fill.note ? h('span', { class: 'cars-cell cars-c-note label', text: fill.note }) : null,
      edit);
  }

  /** The add or edit form for a fill. `fill` is null when adding. */
  function fillForm(car, fill) {
    let n = 0;
    const field = (label, control, cls) => {
      const id = `cars-f${car.car_id}-${++n}`;
      control.id = id;
      return h('div', { class: `cars-field ${cls || ''}` }, h('label', { for: id, text: label }), control);
    };
    const text = (extra) => h('input', Object.assign({ type: 'text', autocomplete: 'off' }, extra));
    const today = SD.local().date;
    const date = h('input', { type: 'date', max: today, value: fill ? fill.date : today, required: true });
    const litres = text({ inputmode: 'decimal', placeholder: '0.0', value: fill ? fill.litres : '' });
    const cost = text({ inputmode: 'decimal', placeholder: '0.00', value: fill ? fill.cost_dollars : '' });
    const odo = text({ inputmode: 'decimal', placeholder: 'optional', value: fill && fill.odometer_km != null ? fill.odometer_km : '' });
    const note = text({ maxlength: '200', placeholder: 'optional', value: fill ? fill.note : '' });
    const full = h('input', { type: 'checkbox', id: `cars-f${car.car_id}-full` });
    full.checked = fill ? fill.full_tank : true;
    const live = h('p', { class: 'cars-live fig', 'aria-live': 'polite' });
    const msg = h('p', { class: 'cars-msg', role: 'alert', hidden: true });

    const showLive = () => {
      const l = typed(litres.value);
      const c = typed(cost.value);
      live.textContent = l > 0 && c != null && c >= 0 ? `${cplText((c / l) * 100)} per litre` : '';
    };
    litres.addEventListener('input', showLive);
    cost.addEventListener('input', showLive);
    showLive();

    const save = h('button', { type: 'submit', class: 'cars-btn primary', text: fill ? 'Save fill' : 'Add fill' });
    const close = () => { adding = null; editing = null; drawManual(); };
    const actions = h('div', { class: 'cars-actions' }, save, h('button', { type: 'button', class: 'cars-btn', text: 'Cancel', onclick: close }));
    if (fill) actions.append(deleteControl(fill, msg));

    const form = h('form', { class: 'cars-form', novalidate: true },
      h('h3', { class: 'cars-form-title', text: fill ? 'Edit fill' : `Add a fill for ${car.name}` }),
      h('div', { class: 'cars-fields' },
        field('Date', date, 'cars-field-date'),
        field('Litres', litres),
        field('Cost ($)', cost),
        field('Odometer (km)', odo),
        h('div', { class: 'cars-field cars-field-check' }, full, h('label', { for: full.id, text: 'Filled to full' })),
        field('Note', note, 'cars-field-wide')),
      live, actions, msg);
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      msg.hidden = true;
      const body = { date: date.value, litres: typed(litres.value), cost_dollars: typed(cost.value), odometer_km: typed(odo.value), full_tank: full.checked, note: note.value };
      if (!body.date || body.litres == null || body.cost_dollars == null) {
        msg.textContent = 'Enter the date, the litres and the cost.';
        msg.hidden = false;
        return;
      }
      save.disabled = true;
      try {
        await SD.fetchJSON(fill ? `/api/admin/fills/${fill.id}` : `/api/admin/cars/${car.car_id}/fills`, {
          method: fill ? 'PUT' : 'POST',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        await saved(body.date.slice(0, 7));
      } catch (err) {
        if (!alive) return;
        msg.textContent = err.status == null ? 'Network error. Nothing was saved.' : err.message;
        msg.hidden = false;
        save.disabled = false;
      }
    });
    return form;
  }

  /** Delete with a question first, in place of the button. */
  function deleteControl(fill, msg) {
    const holder = h('span', { class: 'cars-del' });
    const show = () => holder.replaceChildren(h('button', { type: 'button', class: 'cars-btn danger', text: 'Delete', onclick: ask }));
    function ask() {
      const yes = h('button', {
        type: 'button', class: 'cars-btn danger solid', text: 'Yes, delete',
        onclick: async () => {
          yes.disabled = true;
          try {
            await SD.fetchJSON(`/api/admin/fills/${fill.id}`, { method: 'DELETE' });
            await saved(null);
          } catch (err) {
            if (!alive) return;
            show();
            msg.textContent = err.status == null ? 'Network error. Nothing was deleted.' : err.message;
            msg.hidden = false;
          }
        },
      });
      holder.replaceChildren(
        h('span', { class: 'cars-ask', role: 'alert', text: 'Delete this fill?' }), yes,
        h('button', { type: 'button', class: 'cars-btn', text: 'Keep', onclick: show }));
      yes.focus();
    }
    show();
    return holder;
  }

  /** After a write: reload the manual cars, and show the month the fill is in. */
  async function saved(month) {
    adding = null;
    editing = null;
    fuelPromise = loadFuel();
    if (!alive) return;
    if (month && month !== manualMonth) {
      SD.go(`cars/${month}`);
      return;
    }
    drawManual();
  }

  function manualBlock(car, ym, key) {
    const when = ym === thisMonth() ? 'this month' : `in ${monthOnly(ym)}`;
    const totals = car.totals;
    const month = (car.months || []).find((m) => m.month === ym);
    const fills = car.fills.filter((f) => f.date.startsWith(ym));
    const monthCost = fills.reduce((a, f) => a + f.cost_dollars, 0);
    const since = car.owned_since || totals.first_date;
    const per100km = car.cost_per_km == null ? null : SD.fmt.cents(car.cost_per_km * 100, { round: true });

    const info = {
      key, name: car.name, source: 'manual',
      model: car.model && car.model !== car.name ? car.model : null,
      meta: [KIND_WORDS[car.kind] || car.kind, fills.length ? `${plural(fills.length, 'fill')} ${when}` : `no fills ${when}`].filter(Boolean).join(' · '),
      owned: ownedLine(car.owned_since, totals.first_date, null),
      big: per100km || '–',
      small: `per 100 km petrol · ${money(monthCost, true)} ${when === 'this month' ? when : `in ${monthOnly(ym).slice(0, 3)}`}`,
    };
    const content = [];

    if (!totals.fills) {
      content.push(h('p', { class: 'lede', text: 'No fills yet. Add the first to see its fuel economy and what the petrol costs.' }));
    } else {
      const months = since ? (Date.parse(SD.local().date) - Date.parse(since)) / 86400000 / 30.44 : 0;
      const perMonth = months >= 1 ? ` · about ${SD.fmt.dollars(totals.cost_dollars / months, 0)} a month` : '';
      const last = car.fills.reduce((a, f) => (!a || f.date > a.date ? f : a), null); // the list order is not relied on
      content.push(
        h('div', { class: 'cars-headline' },
          h('span', { class: 'cars-big fig', text: per100km || '–' }),
          h('span', { class: 'cars-big-unit', text: 'per 100 km of petrol' })),
        h('p', { class: 'cars-sentence', text: `${since ? `Averaged full tank to full tank since ${dayAndMonth(since)}. ` : ''}Charging at home isn't counted, so this is the petrol side only.` }),
        h('div', { class: 'cars-stats' },
          stat(car.l_per_100km == null ? '–' : car.l_per_100km.toFixed(1), 'L/100 km', car.l_per_100km == null ? 'needs two full tanks with the odometer' : 'km on battery included'),
          stat(SD.fmt.dollars(month ? month.cost_dollars : 0), `petrol ${when}`, month ? `${litresText(month.litres)} in ${plural(month.fills, 'fill')}` : 'no fills'),
          stat(SD.fmt.dollars(totals.cost_dollars), 'petrol since you got it', since ? `${sinceWords(since)}${perMonth}` : null),
          stat(last ? SD.fmt.date(last.date, 'short') : '–', 'last fill', last ? cplText(last.cents_per_litre) : null)));
    }

    const addOpen = adding === car.car_id;
    const addButton = h('button', {
      type: 'button', class: 'cars-btn primary', text: 'Add fill', 'aria-expanded': String(addOpen),
      onclick: () => { adding = addOpen ? null : car.car_id; editing = null; drawManual(); },
    });
    const list = h('ul', { class: 'cars-fill-list' });
    for (const fill of fills) {
      list.append(fill.id === editing ? h('li', { class: 'cars-fill-edit' }, fillForm(car, fill)) : fillRow(car, fill));
    }
    const part = section(`cars-${key}-fills`, `${car.name}'s fills in ${monthOnly(ym)}`, `${plural(fills.length, 'fill')} · ${SD.fmt.dollars(monthCost)}`);
    if (fills.length) {
      part.append(h('div', { class: 'cars-fill-head', 'aria-hidden': 'true' },
        h('span', { text: 'Date' }), h('span', { text: 'Cost' }), h('span', { text: 'Litres' }), h('span', { text: 'Price' }),
        h('span', { text: 'Odometer' }), h('span', { text: 'Economy' }), h('span')), list);
      if (fills.some((f) => priceMark(f))) {
        part.append(h('p', { class: 'cars-key label' }, '▲ ▼ dearer or cheaper than the Adelaide average that month.'));
      }
    } else if (totals.fills || !addOpen) {
      part.append(h('p', { class: 'muted cars-none', text: totals.fills ? `No fills in ${monthOnly(ym)}. Use ‹ to see earlier months.` : 'Nothing logged yet.' }));
    }
    part.append(h('div', { class: 'cars-add' }, addButton));
    if (addOpen) part.append(fillForm(car, null));
    content.push(part);
    return carCard(info, content);
  }

  /** Redraws the manual cars alone (after a save, an edit or Cancel). */
  async function drawManual() {
    if (!manualBox || !alive) return;
    const box = manualBox;
    const ym = manualMonth;
    const fuel = await fuelPromise;
    if (!alive || box !== manualBox) return;
    if (fuel.error) {
      box.replaceChildren(note(`Could not load the manual cars: ${fuel.error.message}`, 'alert'));
      return;
    }
    const cars = fuel.cars;
    if (!cars.length) {
      box.replaceChildren(h('p', { class: 'muted cars-hint' },
        'Have a car TeslaMate does not track? ', h('a', { href: '#admin', text: 'Add it in Admin' }), ' to log its petrol.'));
      return;
    }
    box.replaceChildren(...cars.map((car) => manualBlock(car, ym, manualKeys.get(car.car_id))));
  }

  function loadFuel() {
    return SD.fetchJSON('/api/fuel/cars').catch((error) => ({ error }));
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

  async function draw(ym, wanted) {
    const my = ++token;
    showAll = false;
    adding = null;
    editing = null;
    manualBox = null;
    opened = loadOpen();
    if (ribbon) { ribbon.destroy(); ribbon = null; }
    const head = h('header', { class: 'page-head cars-head' }, h('h1', { class: 'cars-title', text: 'Cars' }));
    const body = h('div', { class: 'stack' });
    root.replaceChildren(head, body);
    head.append(monthBar(ym, null));
    body.append(h('p', { class: 'muted', role: 'status', text: 'Loading…' }));

    let data = null;
    let failure = null;
    try {
      data = await carsPromise;
    } catch (err) {
      failure = err;
    }
    const fuel = await fuelPromise;
    if (my !== token || !alive) return;

    const cars = (data && data.cars) || [];
    const manual = (fuel.cars || []);
    // The earliest month with anything, so the month bar stops there.
    const first = [...cars.map((c) => c.first_day), ...manual.map((c) => c.totals.first_date)]
      .filter(Boolean).sort()[0];
    head.querySelector('.cars-month').replaceWith(monthBar(ym, first && first.slice(0, 7)));

    // The manual cars have their own box, so a save redraws only them.
    manualBox = h('div', { class: 'cars-manual-box stack' });
    manualMonth = ym;

    const active = cars.map((car) => ({ car, m: carMonth(car, ym) })).filter((x) => x.m);
    const keys = carKeys([...cars, ...manual]);
    manualKeys = new Map(manual.map((car) => [car.car_id, keys.get(car)]));
    singleCar = active.length + manual.length === 1;
    if (wanted && [...keys.values()].includes(wanted)) { opened[wanted] = true; saveOpen(); }
    // Where each active TeslaMate car's charging sections go once the sessions arrive.
    const slots = active.map(() => h('div', { class: 'cars-slot' }));
    const parts = [];
    if (failure) {
      parts.push(note(`Could not load the cars: ${failure.message}`, 'alert'));
    } else if (!cars.length) {
      if (!manual.length && !fuel.error) {
        head.querySelector('.cars-month').remove();
        parts.push(note('No car history yet. It appears here once the car has been driven or charged and the history has synced.'));
      }
    } else if (!active.length) {
      parts.push(note(`No driving or charging in ${monthName(ym)}.`));
    }
    parts.push(...active.map(({ car, m }, i) => carBlock(car, m, data, ym, keys.get(car), slots[i])), manualBox);
    body.replaceChildren(...parts);
    await drawManual();
    if (my !== token || !alive) return;
    if (wanted) {
      const target = body.querySelector(`#${CSS.escape(`cars-${wanted}`)}`);
      if (target) target.scrollIntoView({ block: 'start' });
    }
    if (failure || !active.length) return;

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
    // The sessions carry no car id, so they can only be placed when one car drove.
    if (my !== token || !alive || !sessions.length || active.length !== 1) return;
    const { car } = active[0];
    const name = car.name || car.label;
    const key = keys.get(car);
    slots[0].append(chargedCard(sessions, tariff, name, ym, key), sessionsCard(sessions, tariff, name, ym, key));
  }

  SD.page('cars', {
    mount(el, hashRest) {
      root = el;
      alive = true;
      carsPromise = SD.fetchJSON('/api/ev/cars');
      carsPromise.catch(() => {}); // handled where it is awaited
      fuelPromise = loadFuel();
      draw(monthFrom(hashRest), carFrom(hashRest));
    },
    update(hashRest) {
      draw(monthFrom(hashRest), carFrom(hashRest));
    },
    unmount() {
      alive = false;
      token++;
      if (ribbon) ribbon.destroy();
      ribbon = null;
      manualBox = null;
      manualKeys = null;
      root = null;
    },
  });
})();
