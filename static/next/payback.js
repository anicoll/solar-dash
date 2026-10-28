/* Payback page (stub). The real page arrives in a later PR. */
(function () {
  'use strict';

  SD.page('payback', {
    mount(el, hashRest) {
      el.append(
        SD.h('header', { class: 'page-head' }, SD.h('h1', { class: 'verdict', text: 'Payback' })),
        SD.h('section', { class: 'card payback-stub' },
          SD.h('h2', { class: 'section-title', text: 'Payback' }),
          SD.h('p', { class: 'muted', text: 'This page arrives in a later update.' }),
          hashRest ? SD.h('p', { class: 'muted', text: `Opened with: ${hashRest}` }) : null));
    },
    unmount() {},
  });
})();
