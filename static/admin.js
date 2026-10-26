// solar-dash Admin view: edits the data that lives in the database (petrol prices,
// purchases, settings) through /api/admin and /api/petrol-prices. Plain JS, no build
// step. app.js calls SolarAdmin.load() whenever the #admin view shows.
(function () {
  'use strict';

  const TZ = 'Australia/Adelaide';
  const fmtWhen = new Intl.DateTimeFormat('en-AU', {
    timeZone: TZ, day: 'numeric', month: 'short', year: 'numeric',
    hour: '2-digit', minute: '2-digit', hourCycle: 'h23',
  });

  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const body = (id) => $(id).querySelector('.admin-card-body');

  /** The last answer of GET /api/admin. */
  let data = null;

  /**
   * Sends a request and returns its JSON. Throws an Error holding the API's own
   * message for a failed one, so forms can show it next to the button. A 401
   * sends the browser to the login page.
   */
  async function call(method, url, payload) {
    const init = { method, credentials: 'same-origin', headers: { Accept: 'application/json' } };
    if (payload !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(payload);
    }
    let res;
    try {
      res = await fetch(url, init);
    } catch (err) {
      throw new Error('Network error. Nothing was saved.');
    }
    if (res.status === 401) {
      window.location.href = '/login';
      throw new Error('Not logged in.');
    }
    let json = null;
    try { json = await res.json(); } catch (err) { /* no body */ }
    if (!res.ok) throw new Error((json && json.error) || `Request failed (${res.status}).`);
    return json;
  }

  /** Reloads everything from the server and redraws every card. */
  async function load() {
    const failure = $('admin-error');
    try {
      data = await call('GET', '/api/admin');
      failure.hidden = true;
    } catch (err) {
      failure.textContent = err.message;
      failure.hidden = false;
      return;
    }
    drawPetrol();
    drawPurchases();
    drawSettings();
    drawChanges();
  }

  // ---------- small form pieces ----------

  /** A labelled input. Returns the wrapper; the input is `wrapper.input`. */
  function field(label, input, hint) {
    const wrap = el('label', 'afield');
    wrap.append(el('span', 'afield-label', label), input);
    if (hint) wrap.append(el('span', 'afield-hint', hint));
    wrap.input = input;
    return wrap;
  }

  function input(type, attrs) {
    const i = document.createElement('input');
    i.type = type;
    Object.assign(i, attrs);
    return i;
  }

  /** A form with an error line. `onSave` may throw; its message is shown. */
  function form(fields, saveLabel, onSave, onCancel) {
    const f = el('form', 'aform');
    f.noValidate = true;
    const grid = el('div', 'afields');
    grid.append(...fields);
    const msg = el('p', 'aform-error');
    msg.setAttribute('role', 'alert');
    msg.hidden = true;
    const row = el('div', 'aform-actions');
    const save = el('button', 'btn primary', saveLabel);
    save.type = 'submit';
    row.append(save);
    if (onCancel) {
      const cancel = el('button', 'btn', 'Cancel');
      cancel.type = 'button';
      cancel.onclick = onCancel;
      row.append(cancel);
    }
    f.append(grid, msg, row);
    f.onsubmit = async (ev) => {
      ev.preventDefault();
      msg.hidden = true;
      save.disabled = true;
      try {
        await onSave();
      } catch (err) {
        msg.textContent = err.message;
        msg.hidden = false;
      } finally {
        save.disabled = false;
      }
    };
    return f;
  }

  /** A "Delete" / "Edit" style button for a list row. */
  function rowButton(label, aria, onClick, cls) {
    const b = el('button', 'btn small' + (cls ? ' ' + cls : ''), label);
    b.type = 'button';
    b.setAttribute('aria-label', aria);
    b.onclick = onClick;
    return b;
  }

  /** Asks first, then runs the delete. Errors show in the card's message line. */
  async function confirmDelete(question, run, cardId) {
    if (!window.confirm(question)) return;
    try {
      await run();
      await load();
    } catch (err) {
      const note = body(cardId).querySelector('.list-error');
      note.textContent = err.message;
      note.hidden = false;
    }
  }

  function listError() {
    const p = el('p', 'aform-error list-error');
    p.setAttribute('role', 'alert');
    p.hidden = true;
    return p;
  }

  const dollars = (v) => '$' + Number(v).toLocaleString('en-AU', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  /** The number in a text box, or null when it is empty. */
  const numOrNull = (text) => (text.trim() === '' ? null : Number(text));

  // ---------- Petrol prices ----------

  let petrolEditing = null; // the from-date being edited, or null when adding

  function drawPetrol() {
    const box = body('admin-petrol');
    box.replaceChildren();
    const petrol = data.petrol;
    const prices = petrol.prices.slice().reverse(); // newest first

    box.append(el('p', 'panel-note',
      'The pump price from a date onwards, in cents a litre. Used to price the petrol cars would have burned.'
      + (petrol.fallback_cents_per_litre != null
        ? ` Before the first date: ${petrol.fallback_cents_per_litre} c/L (Settings).` : '')));

    const list = el('ul', 'alist');
    if (!prices.length) list.append(el('li', 'alist-empty', 'No prices yet.'));
    for (const p of prices) {
      const li = el('li', 'arow');
      const text = el('div', 'arow-text');
      text.append(el('strong', 'num', p.from), el('span', 'num', `${p.cents_per_litre} c/L`));
      const actions = el('div', 'arow-actions');
      actions.append(
        rowButton('Edit', `Edit the price from ${p.from}`, () => { petrolEditing = p.from; drawPetrol(); }),
        rowButton('Delete', `Delete the price from ${p.from}`, () => confirmDelete(
          `Delete the petrol price from ${p.from}?`,
          () => call('DELETE', `/api/petrol-prices/${p.from}`), 'admin-petrol'), 'danger'));
      li.append(text, actions);
      list.append(li);
    }
    box.append(list, listError());

    const editing = prices.find((p) => p.from === petrolEditing);
    const from = input('date', { required: true, value: editing ? editing.from : '' });
    from.readOnly = !!editing;
    const cents = input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: editing ? editing.cents_per_litre : '' });
    box.append(el('h3', 'sub-title', editing ? `Edit the price from ${editing.from}` : 'Add a price'));
    box.append(form(
      [field('From date', from, editing ? 'To move a price to another date, delete it and add it again.' : ''),
        field('Cents per litre', cents)],
      editing ? 'Save price' : 'Add price',
      async () => {
        await call('PUT', '/api/petrol-prices', { from: from.value, cents_per_litre: Number(cents.value) });
        petrolEditing = null;
        await load();
      },
      editing ? () => { petrolEditing = null; drawPetrol(); } : null));
  }

  // ---------- Purchases ----------

  let purchaseEditing = null; // the purchase id being edited, or null when adding

  function drawPurchases() {
    const box = body('admin-purchases');
    box.replaceChildren();
    const rows = data.purchases.slice().sort((a, b) => b.date.localeCompare(a.date) || b.id - a.id);

    box.append(el('p', 'panel-note',
      'What the system cost, for the Payback view. Give the battery size on the row that bought the battery.'));
    const list = el('ul', 'alist');
    if (!rows.length) list.append(el('li', 'alist-empty', 'No purchases yet.'));
    for (const p of rows) {
      const li = el('li', 'arow');
      const text = el('div', 'arow-text');
      text.append(el('strong', null, p.name), el('span', 'num', `${p.date} · ${dollars(p.cost_dollars)}`
        + (p.battery_kwh != null ? ` · ${p.battery_kwh} kWh battery` : '')));
      const actions = el('div', 'arow-actions');
      actions.append(
        rowButton('Edit', `Edit ${p.name}`, () => { purchaseEditing = p.id; drawPurchases(); }),
        rowButton('Delete', `Delete ${p.name}`, () => confirmDelete(
          `Delete the purchase "${p.name}"?`,
          () => call('DELETE', `/api/admin/purchases/${p.id}`), 'admin-purchases'), 'danger'));
      li.append(text, actions);
      list.append(li);
    }
    box.append(list, listError());

    const editing = rows.find((p) => p.id === purchaseEditing);
    const name = input('text', { required: true, value: editing ? editing.name : '', autocomplete: 'off' });
    const date = input('date', { required: true, value: editing ? editing.date : '' });
    const cost = input('number', { step: '0.01', min: '0', inputMode: 'decimal', value: editing ? editing.cost_dollars : '' });
    const kwhBox = input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: editing && editing.battery_kwh != null ? editing.battery_kwh : '' });
    box.append(el('h3', 'sub-title', editing ? `Edit ${editing.name}` : 'Add a purchase'));
    box.append(form(
      [field('Name', name), field('Date', date), field('Cost (dollars)', cost),
        field('Battery kWh (optional)', kwhBox)],
      editing ? 'Save purchase' : 'Add purchase',
      async () => {
        const payload = { name: name.value, date: date.value, cost_dollars: numOrNull(cost.value), battery_kwh: numOrNull(kwhBox.value) };
        if (editing) await call('PUT', `/api/admin/purchases/${editing.id}`, payload);
        else await call('POST', '/api/admin/purchases', payload);
        purchaseEditing = null;
        await load();
      },
      editing ? () => { purchaseEditing = null; drawPurchases(); } : null));
  }

  // ---------- Settings ----------

  /** compare_cars as "model = name" lines, and back. */
  const carsToText = (cars) => Object.entries(cars || {}).map(([k, v]) => `${k} = ${v}`).join('\n');
  function textToCars(text) {
    const cars = {};
    for (const line of text.split('\n')) {
      if (!line.trim()) continue;
      const at = line.indexOf('=');
      if (at < 1 || !line.slice(at + 1).trim()) throw new Error(`Cars: "${line.trim()}" should look like "model = name".`);
      cars[line.slice(0, at).trim()] = line.slice(at + 1).trim();
    }
    return Object.keys(cars).length ? cars : null;
  }

  function drawSettings() {
    const box = body('admin-settings');
    box.replaceChildren();
    const set = data.settings;
    const def = data.setting_defaults;
    const shown = (key) => (set[key] != null ? set[key] : '');
    const hint = (key) => `Default: ${def[key] == null ? 'not set' : def[key]}. Leave empty for the default.`;

    const fields = {
      petrol_cents_per_litre: input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: shown('petrol_cents_per_litre') }),
      petrol_litres_per_100km: input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: shown('petrol_litres_per_100km') }),
      away_cents_per_kwh: input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: shown('away_cents_per_kwh') }),
      price_rise_percent: input('number', { step: '0.1', min: '0', max: '20', inputMode: 'decimal', value: shown('price_rise_percent') }),
      petrol_grade: input('text', { value: shown('petrol_grade'), autocomplete: 'off' }),
    };
    const cars = document.createElement('textarea');
    cars.rows = 3;
    cars.value = carsToText(set.compare_cars);
    cars.placeholder = 'Model 3 = Toyota Corolla';

    box.append(form(
      [
        field('Petrol price before the first date (c/L)', fields.petrol_cents_per_litre, hint('petrol_cents_per_litre')),
        field('Petrol car use (L per 100 km)', fields.petrol_litres_per_100km, hint('petrol_litres_per_100km')),
        field('Charging away from home (c/kWh)', fields.away_cents_per_kwh, hint('away_cents_per_kwh')),
        field('Yearly price rise in the Payback forecast (%, 0 to 20)', fields.price_rise_percent, hint('price_rise_percent')),
        field('Petrol grade', fields.petrol_grade, hint('petrol_grade')),
        field('Petrol car to compare with each car', cars, 'One per line, "Tesla model = petrol car". Leave empty for none.'),
      ],
      'Save settings',
      async () => {
        const payload = {};
        for (const key of ['petrol_cents_per_litre', 'petrol_litres_per_100km', 'away_cents_per_kwh', 'price_rise_percent']) {
          payload[key] = numOrNull(fields[key].value);
        }
        payload.petrol_grade = fields.petrol_grade.value.trim() || null;
        payload.compare_cars = textToCars(cars.value);
        await call('PUT', '/api/admin/settings', payload);
        await load();
        const saved = el('p', 'saved-note', 'Saved.');
        saved.setAttribute('role', 'status');
        body('admin-settings').append(saved);
      }));
  }

  // ---------- Recent changes ----------

  function drawChanges() {
    const box = body('admin-changes');
    box.replaceChildren();
    if (!data.changes.length) {
      box.append(el('p', 'panel-note', 'No changes yet.'));
      return;
    }
    const list = el('ul', 'alist');
    for (const c of data.changes) {
      const li = el('li', 'arow');
      const text = el('div', 'arow-text');
      text.append(el('strong', null, `${c.what} · ${c.action}${c.key ? ' · ' + c.key : ''}`),
        el('span', 'num muted', fmtWhen.format(new Date(c.at * 1000))));
      li.append(text);
      list.append(li);
    }
    box.append(list);
  }

  window.SolarAdmin = { load };
})();
