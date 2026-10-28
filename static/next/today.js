/* Today page (stub). The real page arrives in a later PR; for now it shows the
   shared day ribbon with the now marker so the shell can be checked. */
(function () {
  'use strict';

  let timer = null;
  let ribbon = null;

  SD.page('today', {
    mount(el) {
      const now = SD.local();
      const ribbonBox = SD.h('div');
      el.append(
        SD.h('header', { class: 'page-head' },
          SD.h('p', { class: 'date-line', text: `${SD.fmt.date(now.date, 'long')} · ${SD.fmt.time(new Date())}` }),
          SD.h('h1', { class: 'verdict', text: 'Today' })),
        SD.h('section', { class: 'card today-stub', 'aria-labelledby': 'today-stub-h' },
          SD.h('h2', { class: 'section-title', id: 'today-stub-h', text: 'Price bands today' }),
          SD.h('p', { class: 'muted', text: 'The Today page arrives in a later update. This is the shared day ribbon.' }),
          SD.h('div', { style: 'margin-top: 12px' }, ribbonBox)));

      SD.tariff.get().then((tariff) => {
        if (!ribbonBox.isConnected) return;
        ribbon = SD.ribbon(ribbonBox, tariff, {
          nowMinute: SD.local().minute,
          labels: window.matchMedia('(min-width: 760px)').matches ? 'long' : 'short',
        });
        timer = setInterval(() => ribbon && ribbon.setNow(SD.local().minute), 30000);
      }).catch((err) => {
        ribbonBox.append(SD.h('p', { class: 'muted', role: 'alert', text: `Could not load the plan: ${err.message}` }));
      });
    },
    unmount() {
      clearInterval(timer);
      ribbon = null;
    },
  });
})();
