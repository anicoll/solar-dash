/* Admin page (the gear in the header): the data that lives in the database.

   Cards, in the order they are used: Settings, Plans (with a preview), Past
   bills, Purchases, Cars, Petrol prices, Backup and Recent changes. One GET /api/admin
   feeds every card but Backup; each card draws on its own, so one that fails
   shows its own message and the others stay.

   Writes (same endpoints as the classic Admin at /classic#admin):
     PUT    /api/admin/settings                    PUT/POST/DELETE /api/admin/tariffs[/id]
     POST   /api/admin/tariffs/preview             PUT /api/admin/bills (text/csv), DELETE /api/admin/bills/DATE
     POST/PUT/DELETE /api/admin/purchases[/id]     PUT /api/petrol-prices, DELETE /api/petrol-prices/DATE
     POST/PUT/DELETE /api/admin/cars[/id]          (manual cars; their fills are on the Cars page)
     GET    /api/admin/backup, /api/admin/backup/download, /api/admin?download=1 */
(function () {
  'use strict';

  const { h } = SD;
  const CARDS = ['settings', 'plans', 'bills', 'purchases', 'cars', 'petrol', 'backup', 'changes'];

  let alive = false;
  let data = null; // the last answer of GET /api/admin
  let boxes = {}; // card id -> the element its content goes in
  let petrolEditing = null; // the from-date being edited, or null when adding
  let purchaseEditing = null; // the purchase id being edited, or null when adding
  let carEditing = null; // the manual car id being edited, or null when adding
  let draft = null; // the plan being edited or added, as the text of its inputs
  let previewOk = false; // the preview on screen matches the draft; Save needs it
  const timers = [];

  /* ---------------------------------------------------------------- helpers */

  /** Sends a request and returns its JSON; failures carry the API's own message. */
  async function call(method, url, payload, contentType) {
    const init = { method };
    if (payload !== undefined) {
      init.headers = { Accept: 'application/json', 'Content-Type': contentType || 'application/json' };
      init.body = contentType ? payload : JSON.stringify(payload);
    }
    try {
      return await SD.fetchJSON(url, init);
    } catch (err) {
      if (err.status == null) throw new Error('Network error. Nothing was saved.');
      throw err;
    }
  }

  const dollars = (v) => SD.fmt.dollars(Number(v));
  const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
  const str = (v) => (v == null ? '' : String(v));
  /** The number in a text box, or null when it is empty. */
  const numOrNull = (text) => (text.trim() === '' ? null : Number(text));
  const when = new Intl.DateTimeFormat('en-AU', {
    timeZone: SD.tz, day: 'numeric', month: 'short', hour: 'numeric', minute: '2-digit', hourCycle: 'h12',
  });

  function inp(type, props) {
    const i = document.createElement('input');
    i.type = type;
    Object.assign(i, props);
    return i;
  }

  let uid = 0;
  /** A labelled control. The control is `wrap.input`. */
  function field(label, control, hint) {
    const id = `adm-f${++uid}`;
    control.id = id;
    const wrap = h('div', { class: 'adm-field' }, h('label', { for: id, text: label }), control);
    if (hint) {
      const hintId = `${id}-hint`;
      control.setAttribute('aria-describedby', hintId);
      wrap.append(h('span', { class: 'adm-hint', id: hintId, text: hint }));
    }
    wrap.input = control;
    return wrap;
  }

  function btn(label, onclick, cls, aria) {
    return h('button', { type: 'button', class: `adm-btn ${cls || ''}`, text: label, onclick, 'aria-label': aria });
  }

  /** A line that shows an error, or a short confirmation that fades. */
  function note() {
    const p = h('p', { class: 'adm-msg', role: 'status', hidden: true });
    p.error = (text) => { p.className = 'adm-msg is-error'; p.setAttribute('role', 'alert'); p.textContent = text; p.hidden = false; };
    p.ok = (text) => {
      p.className = 'adm-msg is-ok';
      p.setAttribute('role', 'status');
      p.textContent = text;
      p.hidden = false;
      timers.push(setTimeout(() => { p.hidden = true; }, 4000));
    };
    p.clear = () => { p.hidden = true; };
    return p;
  }

  /** A form with its message line. `onSave` may throw; its message shows beside the button. */
  function form(fields, saveLabel, onSave, onCancel) {
    const f = h('form', { class: 'adm-form', novalidate: true });
    const msg = note();
    const save = h('button', { type: 'submit', class: 'adm-btn primary', text: saveLabel });
    const actions = h('div', { class: 'adm-actions' }, save);
    if (onCancel) actions.append(btn('Cancel', onCancel));
    f.append(h('div', { class: 'adm-fields' }, fields), actions, msg);
    f.msg = msg;
    f.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      msg.clear();
      save.disabled = true;
      try { await onSave(msg); } catch (err) { if (alive) msg.error(err.message); } finally { save.disabled = false; }
    });
    return f;
  }

  /** A Delete button that asks first (Yes, delete / Keep) in place of itself, then runs. */
  function deleteButton(label, aria, question, run, errorLine) {
    const holder = h('span', { class: 'adm-del' });
    const show = () => holder.replaceChildren(btn('Delete', ask, 'small danger', aria));
    function ask() {
      const yes = btn('Yes, delete', async () => {
        yes.disabled = true;
        try { await run(); } catch (err) { show(); errorLine.error(err.message); }
      }, 'small danger solid');
      const no = btn('Keep', () => { show(); }, 'small');
      holder.replaceChildren(h('span', { class: 'adm-ask', role: 'alert', text: question }), yes, no);
      yes.focus();
    }
    show();
    return holder;
  }

  /** A card shell. Returns the element; its body is `.adm-body`. */
  function card(id, title, wide) {
    const body = h('div', { class: 'adm-body' });
    const section = h('section', { class: `card adm-card${wide ? ' is-wide' : ''}`, 'aria-labelledby': `adm-h-${id}` },
      h('div', { class: 'card-head' }, h('h2', { class: 'section-title', id: `adm-h-${id}`, text: title })), body);
    boxes[id] = body;
    return section;
  }

  const sub = (text) => h('h3', { class: 'adm-sub', text });
  const blurb = (text) => h('p', { class: 'adm-blurb muted', text });

  /* ---------------------------------------------------------------- loading */

  /** Reloads /api/admin and redraws the named cards (all of them without names). */
  async function refresh(...ids) {
    const fresh = await call('GET', '/api/admin');
    if (!alive) return;
    data = fresh;
    drawAll(ids.length ? ids : CARDS.filter((c) => c !== 'backup'));
  }

  const DRAW = { settings: drawSettings, plans: drawPlans, bills: drawBills, purchases: drawPurchases, cars: drawCars, petrol: drawPetrol, backup: drawBackup, changes: drawChanges };

  /** Each card draws on its own, so a fault in one shows in that card only. */
  function drawAll(ids) {
    for (const id of ids) {
      if (!boxes[id]) continue;
      try {
        const done = DRAW[id]();
        if (done && done.catch) done.catch((err) => failed(id, err));
      } catch (err) {
        failed(id, err);
      }
    }
  }

  function failed(id, err) {
    console.error(`Admin card ${id} failed`, err);
    if (boxes[id]) boxes[id].replaceChildren(h('p', { class: 'adm-msg is-error', role: 'alert', text: `This card could not be shown. ${err.message}` }));
  }

  /* ---------------------------------------------------------------- Settings */

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
    const set = data.settings;
    const def = data.setting_defaults;
    const shown = (key) => (set[key] != null ? set[key] : '');
    const hint = (key) => `Default: ${def[key] == null ? 'not set' : def[key]}. Leave empty for the default.`;
    const dec = (key, extra) => inp('number', Object.assign({ step: '0.1', min: '0', inputMode: 'decimal', value: shown(key) }, extra));

    const f = {
      bill_start_day: dec('bill_start_day', { step: '1', min: '1', max: '28', inputMode: 'numeric' }),
      grid_pull_warn_kw: dec('grid_pull_warn_kw'),
      grid_pull_limit_kw: dec('grid_pull_limit_kw'),
      petrol_cents_per_litre: dec('petrol_cents_per_litre'),
      petrol_litres_per_100km: dec('petrol_litres_per_100km'),
      away_cents_per_kwh: dec('away_cents_per_kwh'),
      price_rise_percent: dec('price_rise_percent', { max: '20' }),
      petrol_grade: inp('text', { value: shown('petrol_grade'), autocomplete: 'off' }),
    };
    const cars = document.createElement('textarea');
    cars.rows = 3;
    cars.value = carsToText(set.compare_cars);
    cars.placeholder = 'Model 3 = Toyota Corolla';

    boxes.settings.replaceChildren(form([
      field('Bill period starts on day (1 to 28)', f.bill_start_day, 'Default: worked out from the last bill, else the calendar month. Leave empty for the default.'),
      field('Grid pull warning (kW)', f.grid_pull_warn_kw, hint('grid_pull_warn_kw')),
      field('Grid pull limit (kW)', f.grid_pull_limit_kw, `${hint('grid_pull_limit_kw')} The main breaker is 63 A.`),
      field('Petrol price before the first date (c/L)', f.petrol_cents_per_litre, hint('petrol_cents_per_litre')),
      field('Petrol car use (L per 100 km)', f.petrol_litres_per_100km, hint('petrol_litres_per_100km')),
      field('Charging away from home (c/kWh)', f.away_cents_per_kwh, hint('away_cents_per_kwh')),
      field('Yearly price rise in Payback (%, 0 to 20)', f.price_rise_percent, hint('price_rise_percent')),
      field('Petrol grade', f.petrol_grade, hint('petrol_grade')),
      field('Petrol car to compare with each car', cars, 'One per line, as "Tesla model = petrol car". Leave empty for none.'),
    ], 'Save settings', async (msg) => {
      const payload = {};
      for (const key of ['petrol_cents_per_litre', 'petrol_litres_per_100km', 'away_cents_per_kwh', 'price_rise_percent', 'bill_start_day', 'grid_pull_warn_kw', 'grid_pull_limit_kw']) {
        payload[key] = numOrNull(f[key].value);
      }
      payload.petrol_grade = f.petrol_grade.value.trim() || null;
      payload.compare_cars = textToCars(cars.value);
      await call('PUT', '/api/admin/settings', payload);
      // Only the cards that show a setting or the change log are redrawn, so this form keeps its place.
      const fresh = await call('GET', '/api/admin');
      if (!alive) return;
      data = fresh;
      drawAll(['petrol', 'changes']);
      msg.ok('Saved.');
    }));
  }

  /* ---------------------------------------------------------------- Plans */

  const PLAN_NUMBERS = 'Prices are in cents, as on the bill. Times are local. The first window that matches wins, and a window that ends before it starts runs past midnight.';
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
      if (!d.credit.from || !d.credit.to) throw new Error('The no-import credit needs a from and a to time.');
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

  const plansNewestFirst = () => data.tariffs.slice().sort((a, b) => (b.valid_from || '').localeCompare(a.valid_from || ''));

  function drawPlans() {
    const box = boxes.plans;
    const plans = plansNewestFirst();
    const err = note();
    const list = h('ul', { class: 'adm-list' });
    if (!plans.length) list.append(h('li', { class: 'adm-empty', text: 'No plans yet. Without one, costs and savings are off.' }));
    for (const p of plans) {
      const name = p.tariff.name || 'Unnamed plan';
      const label = p.tariff.name || p.valid_from || 'from the start';
      list.append(h('li', { class: 'adm-row' },
        h('div', { class: 'adm-row-text' },
          h('strong', { text: name }),
          h('span', { text: `${p.valid_from ? `From ${p.valid_from}` : 'From the start'} · ${p.tariff.supply_cents_per_day} c/day supply · ${p.tariff.default_import_cents} c/kWh import` })),
        h('div', { class: 'adm-row-actions' },
          btn('Edit', () => startDraft(draftFrom(p)), 'small', `Edit plan ${label}`),
          deleteButton('Delete', `Delete plan ${label}`,
            plans.length === 1 ? 'Delete the only plan? Costs and savings switch off.' : 'Delete this plan?',
            async () => { await call('DELETE', `/api/admin/tariffs/${p.id}`); await refresh('plans', 'changes'); }, err))));
    }
    const add = btn('Add plan', () => {
      // A copy of the newest plan is the starting point; the new one needs its own start date.
      const d = plans[0] ? draftFrom(plans[0]) : draftFrom({ tariff: { supply_cents_per_day: '', default_import_cents: '', default_export_cents: 0 } });
      d.id = null;
      d.valid_from = '';
      startDraft(d);
    });
    box.replaceChildren(
      blurb('A plan applies from its start date until the next plan starts. Costs and savings use the plan in force on each day.'),
      list, err, h('div', { class: 'adm-actions' }, add), h('div', { class: 'adm-editor', id: 'adm-plan-editor' }));
    if (draft) drawEditor();
  }

  function startDraft(d) {
    draft = d;
    previewOk = false;
    drawPlans();
    const editor = document.getElementById('adm-plan-editor');
    if (editor) editor.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  }

  /** The draft changed: the preview on screen no longer describes it. */
  function stale() {
    previewOk = false;
    const save = document.getElementById('adm-plan-save');
    if (save) save.disabled = true;
    const out = document.getElementById('adm-plan-preview');
    if (out) { out.hidden = true; out.replaceChildren(); }
  }

  /** An input kept in step with `obj[key]`. */
  function bound(type, obj, key, props) {
    const i = inp(type, props || {});
    i.value = obj[key];
    i.addEventListener('input', () => { obj[key] = i.value; stale(); });
    return i;
  }
  const money2 = (obj, key) => bound('number', obj, key, { step: '0.01', inputMode: 'decimal' });

  /** Rows of from, to, price for one list of windows, with add and remove. */
  function windowRows(title, list, label) {
    const wrap = h('div', { class: 'adm-group' }, sub(title));
    if (!list.length) wrap.append(h('p', { class: 'adm-empty', text: 'None.' }));
    list.forEach((w, i) => {
      wrap.append(h('div', { class: 'adm-wrow' },
        field('From', bound('time', w, 'from')),
        field('To', bound('time', w, 'to')),
        field('Cents per kWh', money2(w, 'cents')),
        btn('Remove', () => { list.splice(i, 1); stale(); drawEditor(); }, 'small danger', `Remove ${label} ${i + 1}`)));
    });
    wrap.append(btn(`Add ${label}`, () => { list.push({ from: '', to: '', cents: '' }); stale(); drawEditor(); }, 'small'));
    return wrap;
  }

  /** A checkbox that turns an optional group on or off. */
  function optionalGroup(title, on, toggle, fieldsFn) {
    const box = inp('checkbox', { checked: on });
    box.id = `adm-c${++uid}`;
    box.onchange = () => { toggle(box.checked); stale(); drawEditor(); };
    const wrap = h('div', { class: 'adm-group' }, h('div', { class: 'adm-check' }, box, h('label', { for: box.id, text: title })));
    if (on) wrap.append(fieldsFn());
    return wrap;
  }

  function drawEditor() {
    const box = document.getElementById('adm-plan-editor');
    if (!box) return;
    box.replaceChildren();
    if (!draft) return;
    previewOk = false; // a redraw starts without a preview on screen
    const d = draft;
    const editing = d.id != null;
    const only = data.tariffs.length === 0 || (editing && data.tariffs.length === 1);

    const basics = h('div', { class: 'adm-fields' },
      field('Name', bound('text', d, 'name', { autocomplete: 'off' })),
      field('Starts on', bound('date', d, 'valid_from'), only ? 'Can stay empty for the only plan.' : 'Leave empty only for the earliest plan.'),
      field('Supply charge (c per day)', money2(d, 'supply')),
      field('Default import price (c/kWh)', money2(d, 'imp')),
      field('Default export price (c/kWh)', money2(d, 'exp'), 'Paid for power sent to the grid outside any export window.'));

    const credit = optionalGroup('Credit for almost no grid import', !!d.credit,
      (on) => { d.credit = on ? { from: '', to: '', max_kwh: '', credit_cents: '' } : null; },
      () => h('div', { class: 'adm-fields' },
        field('From', bound('time', d.credit, 'from')),
        field('To', bound('time', d.credit, 'to')),
        field('Import at most (kWh)', money2(d.credit, 'max_kwh')),
        field('Credit (cents a day)', money2(d.credit, 'credit_cents'))));
    const tier = optionalGroup('Different price for the first kWh exported each day', !!d.tier,
      (on) => { d.tier = on ? { first_kwh_per_day: '', cents: '' } : null; },
      () => h('div', { class: 'adm-fields' },
        field('First kWh a day', money2(d.tier, 'first_kwh_per_day')),
        field('Price for them (c/kWh)', money2(d.tier, 'cents'))));

    const msg = note();
    const out = h('div', { class: 'adm-preview', id: 'adm-plan-preview', hidden: true });
    const preview = btn('Preview', null);
    const save = btn('Save plan', null, 'primary');
    save.id = 'adm-plan-save';
    save.disabled = true;

    preview.onclick = async () => {
      msg.clear();
      preview.disabled = true;
      try {
        const result = await call('POST', '/api/admin/tariffs/preview', planFromDraft(d));
        if (!alive) return;
        drawPreview(out, result);
        previewOk = true;
        save.disabled = false;
      } catch (err) {
        previewOk = false;
        save.disabled = true;
        out.hidden = true;
        if (alive) msg.error(err.message);
      } finally {
        preview.disabled = false;
      }
    };
    save.onclick = async () => {
      msg.clear();
      save.disabled = true;
      try {
        const plan = planFromDraft(d);
        if (editing) await call('PUT', `/api/admin/tariffs/${d.id}`, plan);
        else await call('POST', '/api/admin/tariffs', plan);
        draft = null;
        previewOk = false;
        await refresh('plans', 'changes');
        if (boxes.plans) boxes.plans.prepend(savedLine('Plan saved.'));
      } catch (err) {
        if (!alive) return;
        save.disabled = !previewOk;
        msg.error(err.message);
      }
    };

    box.append(
      sub(editing ? 'Edit plan' : 'Add a plan'), blurb(PLAN_NUMBERS), basics,
      windowRows('Import windows', d.imports, 'import window'),
      windowRows('Export windows', d.exports, 'export window'),
      credit, tier,
      h('div', { class: 'adm-actions' }, preview, save, btn('Cancel', () => { draft = null; previewOk = false; drawPlans(); })),
      msg, out,
      blurb('Preview prices your last 30 complete days on the plans as they are and with this change. Save turns on after a preview with no error.'));
  }

  /** A confirmation line at the top of a card that goes away by itself. */
  function savedLine(text) {
    const p = note();
    p.ok(text);
    return p;
  }

  /** Shows the answer of the preview endpoint: totals, then a day-by-day table. */
  function drawPreview(out, result) {
    out.replaceChildren();
    out.hidden = false;
    const n = result.days.length;
    const total = result.total;
    if (!n) {
      out.append(h('p', { text: 'No days with data in the last 30 days, so there is nothing to compare. The plan itself is valid.' }));
      return;
    }
    const money = (c) => (c == null ? 'n/a' : dollars(c / 100));
    const diff = total.current_cents == null ? null : total.new_cents - total.current_cents;
    const head = h('p', { class: 'adm-preview-head' },
      `Over ${plural(n, 'day')} with data: `,
      h('strong', { text: `${money(total.current_cents)} now` }), ', ',
      h('strong', { text: `${money(total.new_cents)} with this plan` }),
      diff == null ? '.' : diff === 0 ? ', no difference.' : `, ${money(Math.abs(diff))} ${diff > 0 ? 'more' : 'less'}.`);
    const rows = result.days.map((day) => {
      const change = day.current_cents == null ? null : day.new_cents - day.current_cents;
      return h('tr', {}, h('td', { text: day.date }), h('td', { text: money(day.current_cents) }), h('td', { text: money(day.new_cents) }),
        h('td', { text: change == null ? 'n/a' : (change > 0 ? '+' : change < 0 ? '-' : '') + money(Math.abs(change)) }));
    });
    out.append(head, h('details', { class: 'adm-days' }, h('summary', { text: 'Day by day' }),
      h('div', { class: 'adm-table' }, h('table', {},
        h('thead', {}, h('tr', {}, ['Day', 'Now', 'New', 'Change'].map((t) => h('th', { text: t, scope: 'col' })))),
        h('tbody', {}, rows)))));
  }

  /* ---------------------------------------------------------------- Past bills */

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

  function drawBills() {
    const box = boxes.bills;
    const bills = data.bills.slice().sort((a, b) => b.from.localeCompare(a.from));
    const err = note();
    const list = h('ul', { class: 'adm-list' });
    if (!bills.length) list.append(h('li', { class: 'adm-empty', text: 'No bills yet.' }));
    for (const b of bills) {
      list.append(h('li', { class: 'adm-row' },
        h('div', { class: 'adm-row-text' },
          h('strong', { text: `${b.from} to ${b.to}` }),
          h('span', { text: `${dollars(b.bill_total)} billed${b.excluded ? `, ${dollars(b.excluded)} excluded` : ''} · ${b.import_kwh} kWh in, ${b.export_kwh} kWh out` }),
          b.note ? h('span', { class: 'muted', text: b.note }) : null),
        h('div', { class: 'adm-row-actions' },
          deleteButton('Delete', `Delete the bill from ${b.from}`, 'Delete this bill?',
            async () => { await call('DELETE', `/api/admin/bills/${b.from}`); await refresh('bills', 'changes'); }, err))));
    }

    // Upload: pick a file, confirm the replacement, send it as text/csv.
    const file = inp('file', { accept: '.csv,text/csv,text/plain' });
    const msg = note();
    const ask = h('div', { class: 'adm-confirm', hidden: true });
    file.onchange = async () => {
      msg.clear();
      ask.hidden = true;
      const chosen = file.files[0];
      if (!chosen) return;
      try {
        const text = await chosen.text();
        const yes = btn('Yes, replace', async () => {
          yes.disabled = true;
          try {
            await call('PUT', '/api/admin/bills', text, 'text/csv');
            await refresh('bills', 'changes');
            if (boxes.bills) boxes.bills.prepend(savedLine('Bills replaced.'));
          } catch (e) {
            ask.hidden = true;
            if (alive) msg.error(e.message);
          }
        }, 'small danger solid');
        ask.replaceChildren(
          h('span', { role: 'alert', text: `Replace your ${plural(bills.length, 'bill')} with the ${plural(countBillLines(text), 'bill')} in ${chosen.name}?` }),
          yes, btn('Keep mine', () => { ask.hidden = true; file.value = ''; }, 'small'));
        ask.hidden = false;
        yes.focus();
      } catch (e) {
        msg.error(e.message);
      }
    };
    const download = btn('Download as CSV', () => {
      const url = URL.createObjectURL(new Blob([billsToCsv(data.bills)], { type: 'text/csv' }));
      const a = h('a', { href: url, download: 'bills.csv' });
      a.click();
      URL.revokeObjectURL(url);
    });
    download.disabled = !bills.length;

    box.replaceChildren(
      blurb('Real bills for periods the plan cannot price. Each bill is spread evenly over its days.'),
      list, err,
      sub('Replace all from a CSV file'),
      field('CSV file', file, `Columns: ${BILLS_HEADER}`), ask, msg,
      h('div', { class: 'adm-actions' }, download));
  }

  /* ---------------------------------------------------------------- Purchases */

  function drawPurchases() {
    const box = boxes.purchases;
    const rows = data.purchases.slice().sort((a, b) => b.date.localeCompare(a.date) || b.id - a.id);
    const err = note();
    const list = h('ul', { class: 'adm-list' });
    if (!rows.length) list.append(h('li', { class: 'adm-empty', text: 'No purchases yet.' }));
    for (const p of rows) {
      list.append(h('li', { class: 'adm-row' },
        h('div', { class: 'adm-row-text' },
          h('strong', { text: p.name }),
          h('span', { text: `${p.date} · ${dollars(p.cost_dollars)}${p.battery_kwh != null ? ` · ${p.battery_kwh} kWh battery` : ''}` })),
        h('div', { class: 'adm-row-actions' },
          btn('Edit', () => { purchaseEditing = p.id; drawPurchases(); }, 'small', `Edit ${p.name}`),
          deleteButton('Delete', `Delete ${p.name}`, 'Delete this purchase?',
            async () => { await call('DELETE', `/api/admin/purchases/${p.id}`); await refresh('purchases', 'changes'); }, err))));
    }

    const editing = rows.find((p) => p.id === purchaseEditing);
    const name = inp('text', { value: editing ? editing.name : '', autocomplete: 'off' });
    const date = inp('date', { value: editing ? editing.date : '' });
    const cost = inp('number', { step: '0.01', min: '0', inputMode: 'decimal', value: editing ? editing.cost_dollars : '' });
    const kwh = inp('number', { step: '0.1', min: '0', inputMode: 'decimal', value: editing && editing.battery_kwh != null ? editing.battery_kwh : '' });
    box.replaceChildren(
      blurb('What the system cost, for Payback. Give the battery size on the row that bought the battery.'),
      list, err, sub(editing ? `Edit ${editing.name}` : 'Add a purchase'),
      form([field('Name', name), field('Date', date), field('Cost (dollars)', cost), field('Battery size (kWh, optional)', kwh)],
        editing ? 'Save purchase' : 'Add purchase',
        async () => {
          const payload = { name: name.value, date: date.value, cost_dollars: numOrNull(cost.value), battery_kwh: numOrNull(kwh.value) };
          if (editing) await call('PUT', `/api/admin/purchases/${editing.id}`, payload);
          else await call('POST', '/api/admin/purchases', payload);
          purchaseEditing = null;
          await refresh('purchases', 'changes');
          if (boxes.purchases) boxes.purchases.prepend(savedLine('Purchase saved.'));
        },
        editing ? () => { purchaseEditing = null; drawPurchases(); } : null));
  }

  /* ---------------------------------------------------------------- Cars */

  const CAR_KINDS = [['phev', 'Plug-in hybrid'], ['petrol', 'Petrol'], ['hybrid', 'Hybrid'], ['diesel', 'Diesel']];

  function sourceBadge(source) {
    return h('span', { class: 'src-badge', text: source === 'manual' ? 'Manual' : 'TeslaMate' });
  }

  /** The TeslaMate cars are read-only; the manual cars (no TeslaMate) can be added, renamed and deleted. */
  async function drawCars() {
    const box = boxes.cars;
    let tesla = [];
    let teslaError = null;
    try {
      tesla = (await call('GET', '/api/ev/cars')).cars || [];
    } catch (err) {
      teslaError = err;
    }
    if (!alive || !boxes.cars) return;
    const manual = data.manual_cars;
    const err = note();

    const teslaList = h('ul', { class: 'adm-list adm-cars' });
    for (const c of tesla) {
      const detail = [c.name && c.name !== c.label ? c.name : null, c.first_day ? `since ${c.first_day}` : null].filter(Boolean).join(' · ');
      teslaList.append(h('li', { class: 'adm-row' },
        h('div', { class: 'adm-row-text' }, h('strong', null, sourceBadge('teslamate'), c.label), detail ? h('span', { text: detail }) : null)));
    }
    if (teslaError) teslaList.append(h('li', { class: 'adm-empty', text: `Could not load the TeslaMate cars. ${teslaError.message}` }));
    else if (!tesla.length) teslaList.append(h('li', { class: 'adm-empty', text: 'No TeslaMate cars yet.' }));

    const list = h('ul', { class: 'adm-list adm-cars' });
    if (!manual.length) list.append(h('li', { class: 'adm-empty', text: 'No manual cars yet.' }));
    for (const c of manual) {
      const kind = (CAR_KINDS.find(([k]) => k === c.kind) || [null, c.kind])[1];
      list.append(h('li', { class: 'adm-row' },
        h('div', { class: 'adm-row-text' },
          h('strong', null, sourceBadge('manual'), c.name),
          h('span', { text: `${[c.model, kind].filter(Boolean).join(' · ')} · ${plural(c.fills, 'fill')}` })),
        h('div', { class: 'adm-row-actions' },
          btn('Edit', () => { carEditing = c.id; drawCars(); }, 'small', `Edit ${c.name}`),
          deleteButton('Delete', `Delete ${c.name}`,
            c.fills ? `Delete ${c.name} and its ${plural(c.fills, 'fill')}?` : `Delete ${c.name}?`,
            async () => { await call('DELETE', `/api/admin/cars/${c.id}`); await refresh('cars', 'changes'); }, err))));
    }

    const editing = manual.find((c) => c.id === carEditing);
    const name = inp('text', { value: editing ? editing.name : '', autocomplete: 'off' });
    const model = inp('text', { value: editing && editing.model ? editing.model : '', autocomplete: 'off' });
    const kind = h('select', null, CAR_KINDS.map(([k, label]) => h('option', { value: k, text: label })));
    kind.value = editing ? editing.kind : 'phev';
    box.replaceChildren(
      blurb('Cars that TeslaMate does not track get a petrol fill log on the Cars page. Cars from TeslaMate appear here as they sync.'),
      sub('From TeslaMate'), teslaList,
      sub('Manual cars'), list, err,
      sub(editing ? `Edit ${editing.name}` : 'Add a car'),
      form([field('Name', name), field('Model (optional)', model), field('Type', kind, 'Only shown; it does not change any sums.')],
        editing ? 'Save car' : 'Add car',
        async () => {
          const payload = { name: name.value, model: model.value, kind: kind.value };
          if (editing) await call('PUT', `/api/admin/cars/${editing.id}`, payload);
          else await call('POST', '/api/admin/cars', payload);
          carEditing = null;
          await refresh('cars', 'changes');
          if (boxes.cars) boxes.cars.prepend(savedLine('Car saved.'));
        },
        editing ? () => { carEditing = null; drawCars(); } : null));
  }

  /* ---------------------------------------------------------------- Petrol prices */

  function drawPetrol() {
    const box = boxes.petrol;
    const petrol = data.petrol;
    const prices = petrol.prices.slice().reverse(); // newest first
    const err = note();
    const list = h('ul', { class: 'adm-list' });
    if (!prices.length) list.append(h('li', { class: 'adm-empty', text: 'No prices yet.' }));
    for (const p of prices) {
      list.append(h('li', { class: 'adm-row' },
        h('div', { class: 'adm-row-text' }, h('strong', { text: p.from }), h('span', { text: `${p.cents_per_litre} c/L` })),
        h('div', { class: 'adm-row-actions' },
          btn('Edit', () => { petrolEditing = p.from; drawPetrol(); }, 'small', `Edit the price from ${p.from}`),
          deleteButton('Delete', `Delete the price from ${p.from}`, 'Delete this price?',
            async () => { await call('DELETE', `/api/petrol-prices/${p.from}`); await refresh('petrol', 'changes'); }, err))));
    }

    const editing = prices.find((p) => p.from === petrolEditing);
    const from = inp('date', { value: editing ? editing.from : '', readOnly: !!editing });
    const cents = inp('number', { step: '0.1', min: '0', inputMode: 'decimal', value: editing ? editing.cents_per_litre : '' });
    box.replaceChildren(
      blurb('The pump price from a date onwards, in cents a litre. Used to price the petrol the cars would have burned.'
        + (petrol.fallback_cents_per_litre != null ? ` Before the first date: ${petrol.fallback_cents_per_litre} c/L (from Settings).` : '')),
      list, err, sub(editing ? `Edit the price from ${editing.from}` : 'Add a price'),
      form([field('From date', from, editing ? 'To move a price to another date, delete it and add it again.' : ''),
        field('Cents per litre', cents)],
      editing ? 'Save price' : 'Add price',
      async () => {
        await call('PUT', '/api/petrol-prices', { from: from.value, cents_per_litre: Number(cents.value) });
        petrolEditing = null;
        await refresh('petrol', 'changes');
        if (boxes.petrol) boxes.petrol.prepend(savedLine('Price saved.'));
      },
      editing ? () => { petrolEditing = null; drawPetrol(); } : null));
  }

  /* ---------------------------------------------------------------- Backup */

  const fmtDay = new Intl.DateTimeFormat('en-AU', { timeZone: 'UTC', day: 'numeric', month: 'short', year: 'numeric' });

  async function drawBackup() {
    const box = boxes.backup;
    const exportLink = h('a', { class: 'adm-btn', href: '/api/admin?download=1', download: '', text: 'Export all data' });
    let latest;
    try {
      latest = (await call('GET', '/api/admin/backup')).latest;
    } catch (err) {
      if (alive) box.replaceChildren(h('p', { class: 'adm-msg is-error', role: 'alert', text: `Could not check for backups. ${err.message}` }), h('div', { class: 'adm-actions' }, exportLink));
      return;
    }
    if (!alive) return;
    if (!latest) {
      box.replaceChildren(blurb('No backups yet. One is made every night at 03:30.'), h('div', { class: 'adm-actions' }, exportLink));
      return;
    }
    const day = fmtDay.format(new Date(`${latest.date}T00:00:00Z`));
    box.replaceChildren(
      blurb(`Latest backup: ${day}, ${(latest.bytes / 1e6).toFixed(1)} MB. A new one is made every night at 03:30.`),
      h('div', { class: 'adm-actions' },
        h('a', { class: 'adm-btn primary', href: '/api/admin/backup/download', download: `solar-${latest.date}.db`, text: 'Download backup' }),
        exportLink));
  }

  /* ---------------------------------------------------------------- Recent changes */

  function drawChanges() {
    const box = boxes.changes;
    if (!data.changes.length) {
      box.replaceChildren(h('p', { class: 'adm-empty', text: 'No changes yet.' }));
      return;
    }
    box.replaceChildren(h('ul', { class: 'adm-list' }, data.changes.map((c) => h('li', { class: 'adm-row' },
      h('div', { class: 'adm-row-text' },
        h('strong', { text: `${c.what} · ${c.action}${c.key ? ` · ${c.key}` : ''}` }),
        h('span', { class: 'muted', text: when.format(new Date(c.at * 1000)) }))))));
  }

  /* ---------------------------------------------------------------- page */

  SD.page('admin', {
    mount(el) {
      alive = true;
      data = null;
      boxes = {};
      draft = null;
      previewOk = false;
      petrolEditing = null;
      purchaseEditing = null;
      carEditing = null;
      const titles = {
        settings: 'Settings', plans: 'Plans', bills: 'Past bills', purchases: 'Purchases', cars: 'Cars',
        petrol: 'Petrol prices', backup: 'Backup', changes: 'Recent changes',
      };
      const wide = new Set(['settings', 'plans']);
      const loadError = h('p', { class: 'adm-msg is-error', role: 'alert', hidden: true });
      el.append(
        h('div', { class: 'adm-top' },
          h('a', { class: 'adm-back', href: '#today', text: '‹ Today' }),
          h('h1', { class: 'adm-title', text: 'Settings' })),
        loadError,
        h('div', { class: 'adm-grid' }, CARDS.map((id) => card(id, titles[id], wide.has(id)))),
        h('p', { class: 'adm-foot' }, h('a', { href: '/classic', text: 'Open the old dashboard' })));
      for (const id of CARDS) boxes[id].append(h('p', { class: 'muted', text: 'Loading…' }));
      drawBackup().catch((err) => failed('backup', err));
      refresh().catch((err) => {
        if (!alive) return;
        loadError.textContent = `Could not load the settings. ${err.message}`;
        loadError.hidden = false;
        for (const id of CARDS) if (id !== 'backup') boxes[id].replaceChildren(h('p', { class: 'muted', text: 'Not loaded.' }));
      });
    },
    unmount() {
      alive = false;
      data = null;
      boxes = {};
      while (timers.length) clearTimeout(timers.pop());
    },
  });
})();
