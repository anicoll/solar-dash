/* Cars page: was charging the car cheap fuel?

   One block per car with data in the chosen month (cost per 100 km against a
   petrol car), then one block for each car TeslaMate does not track (a plug-in
   hybrid, say) with its petrol fills, then when the charging happened drawn on
   the price ribbon, and the sessions in words. Same data and maths as the
   current dashboard's Cars view:
     /api/ev/cars      each car's months (km, petrol cost, charging cost),
                       the comparison petrol car and its L/100 km
     /api/ev/sessions  the month's charging sessions, costed from the house data
     /api/tariff       (through SD.tariff) the price bands for the ribbon
     /api/fuel/cars    the manual cars: totals, L/100 km and the fills
   The manual cars' fills are added, changed and deleted here:
     POST /api/admin/cars/ID/fills     PUT/DELETE /api/admin/fills/ID
   The month lives in the address: #cars, or #cars/2026-09 for September. */
(function () {
  'use strict';

  const h = SD.h;
  const SESSIONS_SHOWN = 4;

  let root = null; // the page element while mounted
  let alive = false; // false after unmount, so late fetches draw nothing
  let carsPromise = null;
  let fuelPromise = null; // never rejects: resolves {cars} or {error}
  let manualBox = null; // where the manual cars are drawn, so a save redraws only them
  let manualMonth = null; // the month manualBox shows
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
      h('p', { class: 'cars-sub' }, sourceBadge('teslamate'), sub));

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

  function manualBlock(car, ym) {
    const when = ym === thisMonth() ? 'this month' : `in ${monthOnly(ym)}`;
    const totals = car.totals;
    const kind = KIND_WORDS[car.kind] || car.kind;
    const sub = [car.model && car.model !== car.name ? car.model : null, kind].filter(Boolean).join(' · ');
    const month = (car.months || []).find((m) => m.month === ym);
    const fills = car.fills.filter((f) => f.date.startsWith(ym));

    const el = h('section', { class: 'cars-car cars-manual', 'aria-label': car.name },
      h('p', { class: 'cars-sub' }, sourceBadge('manual'), `${car.name} · ${sub}`));

    if (!totals.fills) {
      el.append(h('p', { class: 'lede', text: 'No fills yet. Add the first to see its fuel economy and what the petrol costs.' }));
    } else {
      el.append(h('div', { class: 'cars-stats' },
        stat(car.l_per_100km == null ? '–' : car.l_per_100km.toFixed(1), 'L/100 km', car.l_per_100km == null ? 'needs two full tanks with the odometer' : 'full tank to full tank'),
        stat(car.cost_per_km == null ? '–' : SD.fmt.cents(car.cost_per_km * 100), 'per km', 'petrol only'),
        stat(SD.fmt.dollars(month ? month.cost_dollars : 0), `petrol ${when}`, month ? `${litresText(month.litres)} in ${month.fills} ${month.fills === 1 ? 'fill' : 'fills'}` : 'no fills'),
        stat(SD.fmt.dollars(totals.cost_dollars), 'petrol so far', `since ${SD.fmt.date(totals.first_date, 'short')}`)));
    }

    const addOpen = adding === car.car_id;
    const addButton = h('button', {
      type: 'button', class: 'cars-btn primary', text: 'Add fill', 'aria-expanded': String(addOpen),
      onclick: () => { adding = addOpen ? null : car.car_id; editing = null; drawManual(); },
    });
    const card = h('div', { class: 'card cars-fills' },
      h('div', { class: 'card-head' },
        h('h2', { class: 'section-title', text: `Fills in ${monthOnly(ym)}` }), addButton));
    if (addOpen) card.append(fillForm(car, null));

    const list = h('ul', { class: 'cars-fill-list' });
    for (const fill of fills) {
      list.append(fill.id === editing ? h('li', { class: 'cars-fill-edit' }, fillForm(car, fill)) : fillRow(car, fill));
    }
    if (fills.length) {
      card.append(h('div', { class: 'cars-fill-head', 'aria-hidden': 'true' },
        h('span', { text: 'Date' }), h('span', { text: 'Cost' }), h('span', { text: 'Litres' }), h('span', { text: 'Price' }),
        h('span', { text: 'Odometer' }), h('span', { text: 'Economy' }), h('span')), list);
      if (fills.some((f) => priceMark(f))) {
        card.append(h('p', { class: 'cars-key label' }, '▲ ▼ dearer or cheaper than the Adelaide average that month.'));
      }
    } else if (totals.fills || !addOpen) {
      card.append(h('p', { class: 'muted cars-none', text: totals.fills ? `No fills in ${monthName(ym)}.` : 'Nothing logged yet.' }));
    }
    el.append(card);
    return el;
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
    box.replaceChildren(...cars.map((car) => manualBlock(car, ym)));
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

  async function draw(ym) {
    const my = ++token;
    showAll = false;
    adding = null;
    editing = null;
    manualBox = null;
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
    parts.push(...active.map(({ car, m }) => carBlock(car, m, data, ym)), manualBox);
    body.replaceChildren(...parts);
    drawManual();
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
    if (my !== token || !alive || !sessions.length) return;
    body.append(chargedCard(sessions, tariff), sessionsCard(sessions, tariff));
  }

  SD.page('cars', {
    mount(el, hashRest) {
      root = el;
      alive = true;
      carsPromise = SD.fetchJSON('/api/ev/cars');
      carsPromise.catch(() => {}); // handled where it is awaited
      fuelPromise = loadFuel();
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
      manualBox = null;
      root = null;
    },
  });
})();
