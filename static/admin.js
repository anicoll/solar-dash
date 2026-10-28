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
  async function call(method, url, payload, contentType) {
    const init = { method, credentials: 'same-origin', headers: { Accept: 'application/json' } };
    if (payload !== undefined) {
      init.headers['Content-Type'] = contentType || 'application/json';
      init.body = contentType ? payload : JSON.stringify(payload);
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
    // Each card draws on its own, so a fault in one card shows in that card
    // instead of leaving every card after it blank.
    const cards = [
      ['admin-plans', drawPlans],
      ['admin-petrol', drawPetrol],
      ['admin-purchases', drawPurchases],
      ['admin-bills', drawBills],
      ['admin-settings', drawSettings],
      ['admin-changes', drawChanges],
      ['admin-backup', drawBackup],
    ];
    const failed = (id) => (err) => {
      console.error(`Admin card ${id} failed`, err);
      body(id).replaceChildren(el('p', 'aform-error', `This card could not be shown: ${err.message}`));
    };
    for (const [id, draw] of cards) {
      try {
        // drawBackup is async; the others draw at once.
        const done = draw();
        if (done && done.catch) done.catch(failed(id));
      } catch (err) {
        failed(id)(err);
      }
    }
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

  const dollars = (v) => (v < 0 ? '-$' : '$') + Math.abs(Number(v)).toLocaleString('en-AU', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
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

  // ---------- Electricity plans ----------

  const PLAN_NUMBERS = 'Numbers are in cents, as on the bill. Windows are local times; the first window that matches wins, and a window that ends before it starts runs past midnight.';

  /** The plan being edited or added, as the text of its inputs, or null when none is. */
  let draft = null;
  /** True while the preview on screen matches the form; Save needs it. */
  let previewOk = false;

  const str = (v) => (v == null ? '' : String(v));
  const windowDraft = (w) => ({ from: str(w.from), to: str(w.to), cents: str(w.cents) });

  /** A draft (all text) from a plan row of GET /api/admin. */
  function draftFrom(plan) {
    const t = plan.tariff;
    return {
      id: plan.id,
      valid_from: str(plan.valid_from),
      name: str(t.name),
      supply: str(t.supply_cents_per_day),
      imp: str(t.default_import_cents),
      exp: str(t.default_export_cents),
      imports: (t.import || []).map(windowDraft),
      exports: (t.export || []).map(windowDraft),
      credit: t.zero_import_credit
        ? { from: str(t.zero_import_credit.from), to: str(t.zero_import_credit.to), max_kwh: str(t.zero_import_credit.max_kwh), credit_cents: str(t.zero_import_credit.credit_cents) }
        : null,
      tier: t.export_tier ? { first_kwh_per_day: str(t.export_tier.first_kwh_per_day), cents: str(t.export_tier.cents) } : null,
    };
  }

  /** The number in a text box; throws a message naming the field when it is empty or not a number. */
  function needNumber(text, label) {
    const n = text.trim() === '' ? NaN : Number(text);
    if (!Number.isFinite(n)) throw new Error(`${label} needs a number.`);
    return n;
  }

  function windowsToJson(list, label) {
    return list.map((w, i) => {
      if (!w.from || !w.to) throw new Error(`${label} ${i + 1} needs a from and a to time.`);
      return { from: w.from, to: w.to, cents: needNumber(w.cents, `${label} ${i + 1} price`) };
    });
  }

  /** The body for the plan API from a draft. Throws a message for a missing or bad number. */
  function planFromDraft(d) {
    const tariff = {
      name: d.name.trim() || null,
      supply_cents_per_day: needNumber(d.supply, 'Supply charge'),
      default_import_cents: needNumber(d.imp, 'Default import price'),
      default_export_cents: needNumber(d.exp, 'Default export price'),
      import: windowsToJson(d.imports, 'Import window'),
      export: windowsToJson(d.exports, 'Export window'),
      zero_import_credit: null,
    };
    if (d.credit) {
      if (!d.credit.from || !d.credit.to) throw new Error('The zero-import credit needs a from and a to time.');
      tariff.zero_import_credit = {
        from: d.credit.from, to: d.credit.to,
        max_kwh: needNumber(d.credit.max_kwh, 'Credit maximum kWh'),
        credit_cents: needNumber(d.credit.credit_cents, 'Credit amount'),
      };
    }
    if (d.tier) {
      tariff.export_tier = {
        first_kwh_per_day: needNumber(d.tier.first_kwh_per_day, 'Export tier kWh'),
        cents: needNumber(d.tier.cents, 'Export tier price'),
      };
    }
    const plan = { valid_from: d.valid_from || null, tariff };
    if (d.id != null) plan.id = d.id;
    return plan;
  }

  /** Newest plan first, as the list shows them. */
  const plansNewestFirst = () => data.tariffs.slice().sort((a, b) => (b.valid_from || '').localeCompare(a.valid_from || ''));

  function drawPlans() {
    const box = body('admin-plans');
    box.replaceChildren();
    const plans = plansNewestFirst();

    box.append(el('p', 'panel-note',
      'A plan applies from its start date until the next plan starts. Only the earliest plan can have no start date. Costs and savings use the plan in force on each day.'));
    const list = el('ul', 'alist');
    if (!plans.length) list.append(el('li', 'alist-empty', 'No plans yet. Without one, costs and savings are off.'));
    for (const p of plans) {
      const li = el('li', 'arow');
      const text = el('div', 'arow-text');
      text.append(el('strong', null, p.tariff.name || 'Unnamed plan'),
        el('span', 'num', (p.valid_from ? `From ${p.valid_from}` : 'From the start')
          + ` · ${p.tariff.supply_cents_per_day} c/day supply · ${p.tariff.default_import_cents} c/kWh import`));
      const actions = el('div', 'arow-actions');
      actions.append(
        rowButton('Edit', `Edit plan ${p.tariff.name || p.valid_from || ''}`, () => { startDraft(draftFrom(p)); }),
        rowButton('Delete', `Delete plan ${p.tariff.name || p.valid_from || ''}`, () => confirmDelete(
          `Delete the plan "${p.tariff.name || p.valid_from || 'from the start'}"?`
          + (plans.length === 1 ? ' It is the only plan, so costs and savings will switch off.' : ''),
          () => call('DELETE', `/api/admin/tariffs/${p.id}`), 'admin-plans'), 'danger'));
      li.append(text, actions);
      list.append(li);
    }
    box.append(list, listError());

    const add = el('button', 'btn', 'Add plan');
    add.type = 'button';
    add.onclick = () => {
      // A copy of the newest plan is the starting point; the new one needs its own start date.
      const newest = plans[0];
      const d = newest ? draftFrom(newest) : draftFrom({ tariff: { supply_cents_per_day: '', default_import_cents: '', default_export_cents: 0 } });
      d.id = null;
      d.valid_from = '';
      startDraft(d);
    };
    box.append(add);

    const editorBox = el('div', 'plan-editor');
    editorBox.id = 'plan-editor';
    box.append(editorBox);
    if (draft) drawEditor();
  }

  function startDraft(d) {
    draft = d;
    previewOk = false;
    drawPlans();
    const editor = $('plan-editor');
    if (editor) editor.scrollIntoView({ block: 'nearest' });
  }

  /** A text or number or time input kept in step with `obj[key]`. */
  function bound(type, obj, key, attrs) {
    const i = input(type, attrs || {});
    i.value = obj[key];
    i.addEventListener('input', () => { obj[key] = i.value; stale(); });
    return i;
  }

  /** The draft changed: the preview on screen no longer describes it. */
  function stale() {
    previewOk = false;
    const save = $('plan-save');
    if (save) save.disabled = true;
    const out = $('plan-preview');
    if (out) { out.hidden = true; out.replaceChildren(); }
  }

  /** Rows of from, to, price for one list of windows, with add and remove. */
  function windowRows(title, list, label) {
    const wrap = el('div', 'wlist');
    wrap.append(el('h4', 'sub-title', title));
    list.forEach((w, i) => {
      const row = el('div', 'wrow');
      const from = bound('time', w, 'from', { required: true });
      const to = bound('time', w, 'to', { required: true });
      const cents = bound('number', w, 'cents', { step: '0.01', inputMode: 'decimal' });
      row.append(field('From', from), field('To', to), field('Cents per kWh', cents),
        rowButton('Remove', `Remove ${label} ${i + 1}`, () => { list.splice(i, 1); stale(); drawEditor(); }, 'danger'));
      wrap.append(row);
    });
    const add = el('button', 'btn small', `Add ${label}`);
    add.type = 'button';
    add.onclick = () => { list.push({ from: '', to: '', cents: '' }); stale(); drawEditor(); };
    wrap.append(add);
    return wrap;
  }

  /** A checkbox that turns an optional group on (with `make()` as its starting values) or off. */
  function optionalGroup(title, on, toggle, fieldsFn) {
    const wrap = el('div', 'wlist');
    const label = el('label', 'check');
    const box = input('checkbox', { checked: on });
    box.onchange = () => { toggle(box.checked); stale(); drawEditor(); };
    label.append(box, el('span', null, title));
    wrap.append(label);
    if (on) wrap.append(fieldsFn());
    return wrap;
  }

  function drawEditor() {
    const box = $('plan-editor');
    box.replaceChildren();
    if (!draft) return;
    previewOk = false; // a redraw starts without a preview on screen
    const d = draft;
    const plans = data.tariffs;
    const editing = d.id != null;
    box.append(el('h3', 'sub-title', editing ? 'Edit plan' : 'Add a plan'));
    box.append(el('p', 'panel-note', PLAN_NUMBERS));

    const basics = [
      field('Name', bound('text', d, 'name', { autocomplete: 'off' })),
      field('Starts on', bound('date', d, 'valid_from'),
        plans.length === 0 || (editing && plans.length === 1) ? 'Can stay empty for the only plan.' : 'Leave empty only for the earliest plan.'),
      field('Supply charge (c per day)', bound('number', d, 'supply', { step: '0.01', inputMode: 'decimal' })),
      field('Default import price (c/kWh)', bound('number', d, 'imp', { step: '0.01', inputMode: 'decimal' })),
      field('Default export price (c/kWh)', bound('number', d, 'exp', { step: '0.01', inputMode: 'decimal' }), 'Paid for power sent to the grid outside any export window.'),
    ];

    const credit = optionalGroup('Credit for (almost) no grid import', !!d.credit,
      (on) => { d.credit = on ? { from: '', to: '', max_kwh: '', credit_cents: '' } : null; },
      () => {
        const g = el('div', 'afields');
        g.append(field('From', bound('time', d.credit, 'from', { required: true })),
          field('To', bound('time', d.credit, 'to', { required: true })),
          field('Import at most (kWh)', bound('number', d.credit, 'max_kwh', { step: '0.01', inputMode: 'decimal' })),
          field('Credit (cents a day)', bound('number', d.credit, 'credit_cents', { step: '0.01', inputMode: 'decimal' })));
        return g;
      });
    const tier = optionalGroup('Different price for the first kWh exported each day', !!d.tier,
      (on) => { d.tier = on ? { first_kwh_per_day: '', cents: '' } : null; },
      () => {
        const g = el('div', 'afields');
        g.append(field('First kWh a day', bound('number', d.tier, 'first_kwh_per_day', { step: '0.01', inputMode: 'decimal' })),
          field('Price for them (c/kWh)', bound('number', d.tier, 'cents', { step: '0.01', inputMode: 'decimal' })));
        return g;
      });

    const msg = el('p', 'aform-error');
    msg.setAttribute('role', 'alert');
    msg.hidden = true;
    const showError = (err) => { msg.textContent = err.message; msg.hidden = false; };
    const out = el('div', 'apreview');
    out.id = 'plan-preview';
    out.hidden = true;

    const preview = el('button', 'btn', 'Preview');
    preview.type = 'button';
    const save = el('button', 'btn primary', 'Save plan');
    save.type = 'button';
    save.id = 'plan-save';
    save.disabled = true;
    const cancel = el('button', 'btn', 'Cancel');
    cancel.type = 'button';
    cancel.onclick = () => { draft = null; previewOk = false; drawPlans(); };

    preview.onclick = async () => {
      msg.hidden = true;
      preview.disabled = true;
      try {
        const result = await call('POST', '/api/admin/tariffs/preview', planFromDraft(d));
        drawPreview(out, result);
        previewOk = true;
        save.disabled = false;
      } catch (err) {
        previewOk = false;
        save.disabled = true;
        out.hidden = true;
        showError(err);
      } finally {
        preview.disabled = false;
      }
    };
    save.onclick = async () => {
      msg.hidden = true;
      save.disabled = true;
      try {
        const plan = planFromDraft(d);
        if (editing) await call('PUT', `/api/admin/tariffs/${d.id}`, plan);
        else await call('POST', '/api/admin/tariffs', plan);
        draft = null;
        previewOk = false;
        await load();
      } catch (err) {
        save.disabled = !previewOk;
        showError(err);
      }
    };

    const actions = el('div', 'aform-actions');
    actions.append(preview, save, cancel);
    const grid = el('div', 'afields');
    grid.append(...basics);
    box.append(grid, windowRows('Import windows', d.imports, 'import window'),
      windowRows('Export windows', d.exports, 'export window'), credit, tier,
      msg, actions, out,
      el('p', 'panel-note', 'Preview prices your last 30 complete days on the plans as they are now and with this change. Save turns on after a preview with no error.'));
  }

  /** Shows the answer of the preview endpoint: totals, then a day-by-day table. */
  function drawPreview(out, result) {
    out.replaceChildren();
    out.hidden = false;
    const n = result.days.length;
    const total = result.total;
    if (!n) {
      out.append(el('p', 'panel-note', 'No days with data in the last 30 days, so there is nothing to compare. The plan itself is valid.'));
      return;
    }
    const money = (c) => (c == null ? 'n/a' : dollars(c / 100));
    const diff = total.current_cents == null ? null : total.new_cents - total.current_cents;
    const head = el('p', 'preview-head');
    head.append(document.createTextNode(`Over ${plural(n, 'day')} with data: `),
      el('strong', null, `${money(total.current_cents)} now`),
      document.createTextNode(', '),
      el('strong', null, `${money(total.new_cents)} with this plan`));
    if (diff != null) {
      const word = diff > 0 ? 'more' : 'less';
      head.append(document.createTextNode(diff === 0 ? ', no difference.' : `, ${money(Math.abs(diff))} ${word}.`));
    }
    const details = el('details', 'how-days');
    details.append(el('summary', null, 'Day by day'));
    const table = el('table', 'data');
    const thead = el('thead');
    const hr = el('tr');
    for (const h of ['Day', 'Now', 'New', 'Change']) hr.append(el('th', null, h));
    thead.append(hr);
    const tbody = el('tbody');
    for (const day of result.days) {
      const tr = el('tr');
      const change = day.current_cents == null ? null : day.new_cents - day.current_cents;
      tr.append(el('td', null, day.date), el('td', null, money(day.current_cents)), el('td', null, money(day.new_cents)),
        el('td', null, change == null ? 'n/a' : (change > 0 ? '+' : change < 0 ? '-' : '') + money(Math.abs(change))));
      tbody.append(tr);
    }
    table.append(thead, tbody);
    const wrap = el('div', 'table-wrap');
    wrap.append(table);
    details.append(wrap);
    out.append(head, details);
  }

  // ---------- Past bills ----------

  /** The same header as bills.rs expects. */
  const BILLS_HEADER = 'from,to,bill_total,excluded,import_kwh,import_dollars,export_kwh,export_dollars,supply_dollars,note';
  const BILL_COLUMNS = BILLS_HEADER.split(',');

  /** The current bills as CSV text. A note cannot hold a line break in this format. */
  function billsToCsv(bills) {
    const lines = [BILLS_HEADER];
    for (const b of bills) {
      lines.push(BILL_COLUMNS.map((c) => (c === 'note' ? String(b.note).replace(/[\r\n]+/g, ' ') : b[c])).join(','));
    }
    return lines.join('\n') + '\n';
  }

  /** How many bill lines a CSV text holds: not blank, not a comment, minus the header. */
  function countBillLines(text) {
    const lines = text.split(/\r?\n/).map((l) => l.trim()).filter((l) => l && !l.startsWith('#'));
    return Math.max(0, lines.length - 1);
  }

  const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

  function drawBills() {
    const box = body('admin-bills');
    box.replaceChildren();
    const bills = data.bills.slice().sort((a, b) => b.from.localeCompare(a.from));

    box.append(el('p', 'panel-note',
      'Real bills for periods the plan cannot price. Each bill is spread evenly over its days. Upload a CSV to replace the whole list.'));
    const list = el('ul', 'alist');
    if (!bills.length) list.append(el('li', 'alist-empty', 'No bills yet.'));
    for (const b of bills) {
      const li = el('li', 'arow');
      const text = el('div', 'arow-text');
      text.append(el('strong', 'num', `${b.from} to ${b.to}`),
        el('span', 'num', `${dollars(b.bill_total)} billed`
          + (b.excluded ? `, ${dollars(b.excluded)} excluded` : '')
          + ` · ${b.import_kwh} kWh in, ${b.export_kwh} kWh out`));
      if (b.note) text.append(el('span', 'muted', b.note));
      const actions = el('div', 'arow-actions');
      actions.append(rowButton('Delete', `Delete the bill from ${b.from}`, () => confirmDelete(
        `Delete the bill from ${b.from} to ${b.to}?`,
        () => call('DELETE', `/api/admin/bills/${b.from}`), 'admin-bills'), 'danger'));
      li.append(text, actions);
      list.append(li);
    }
    box.append(list, listError());

    // Upload: pick a file, confirm the replacement, send it as text/csv.
    const file = input('file', { accept: '.csv,text/csv,text/plain' });
    const msg = el('p', 'aform-error');
    msg.setAttribute('role', 'alert');
    msg.hidden = true;
    file.onchange = async () => {
      msg.hidden = true;
      const chosen = file.files[0];
      if (!chosen) return;
      try {
        const text = await chosen.text();
        const question = `Replace your ${plural(bills.length, 'bill')} with the ${plural(countBillLines(text), 'bill')} in ${chosen.name}?`;
        if (!window.confirm(question)) return;
        await call('PUT', '/api/admin/bills', text, 'text/csv');
        await load();
      } catch (err) {
        msg.textContent = err.message;
        msg.hidden = false;
      } finally {
        file.value = '';
      }
    };
    const actions = el('div', 'aform-actions');
    const download = el('button', 'btn', 'Download as CSV');
    download.type = 'button';
    download.disabled = !bills.length;
    download.onclick = () => {
      const url = URL.createObjectURL(new Blob([billsToCsv(data.bills)], { type: 'text/csv' }));
      const a = el('a');
      a.href = url;
      a.download = 'bills.csv';
      a.click();
      URL.revokeObjectURL(url);
    };
    actions.append(download);
    box.append(el('h3', 'sub-title', 'Replace all from a CSV file'),
      field('CSV file', file, `Columns: ${BILLS_HEADER}`), msg, actions);
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
      bill_start_day: input('number', { step: '1', min: '1', max: '28', inputMode: 'numeric', value: shown('bill_start_day') }),
      grid_pull_warn_kw: input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: shown('grid_pull_warn_kw') }),
      grid_pull_limit_kw: input('number', { step: '0.1', min: '0', inputMode: 'decimal', value: shown('grid_pull_limit_kw') }),
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
        field('Bill period starts on day of month (1 to 28)', fields.bill_start_day, 'Default: worked out from the last bill, else the calendar month. Leave empty for the default.'),
        field('Grid pull warning (kW, below the limit)', fields.grid_pull_warn_kw, hint('grid_pull_warn_kw')),
        field('Grid pull limit (kW, house main breaker is 63 A)', fields.grid_pull_limit_kw, hint('grid_pull_limit_kw')),
        field('Petrol grade', fields.petrol_grade, hint('petrol_grade')),
        field('Petrol car to compare with each car', cars, 'One per line, "Tesla model = petrol car". Leave empty for none.'),
      ],
      'Save settings',
      async () => {
        const payload = {};
        for (const key of ['petrol_cents_per_litre', 'petrol_litres_per_100km', 'away_cents_per_kwh', 'price_rise_percent', 'bill_start_day', 'grid_pull_warn_kw', 'grid_pull_limit_kw']) {
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

  // ---------- Database backup ----------

  const fmtDay = new Intl.DateTimeFormat('en-AU', { timeZone: 'UTC', day: 'numeric', month: 'short', year: 'numeric' });

  async function drawBackup() {
    const box = body('admin-backup');
    let latest = null;
    try {
      latest = (await call('GET', '/api/admin/backup')).latest;
    } catch (err) {
      box.replaceChildren(el('p', 'aform-error', err.message));
      return;
    }
    if (!latest) {
      box.replaceChildren(el('p', 'panel-note', 'No backups yet. A backup is made every night at 03:30.'));
      return;
    }
    const day = fmtDay.format(new Date(latest.date + 'T00:00:00Z'));
    const mb = (latest.bytes / 1e6).toFixed(1);
    const link = el('a', 'btn primary', 'Download');
    link.href = '/api/admin/backup/download';
    link.download = `solar-${latest.date}.db`;
    const actions = el('div', 'aform-actions');
    actions.append(link);
    box.replaceChildren(
      el('p', 'panel-note', `Latest backup: ${day}, ${mb} MB. A new one is made every night at 03:30.`),
      actions);
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
