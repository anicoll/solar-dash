/* Bill page (stub). The real page arrives in a later PR. */
(function () {
  'use strict';

  SD.page('bill', {
    mount(el, hashRest) {
      el.append(
        SD.h('header', { class: 'page-head' }, SD.h('h1', { class: 'verdict', text: 'Bill' })),
        SD.h('section', { class: 'card bill-stub' },
          SD.h('h2', { class: 'section-title', text: 'Bill' }),
          SD.h('p', { class: 'muted', text: 'This page arrives in a later update.' }),
          hashRest ? SD.h('p', { class: 'muted', text: `Opened with: ${hashRest}` }) : null));
    },
    unmount() {},
  });
})();
