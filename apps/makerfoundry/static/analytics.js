// MakerFoundry funnel events (issue #128; the owner agreed to PostHog, 2026-11-10). Our own code,
// MIT OR Apache-2.0, no libraries. It stores nothing in the browser (no cookie, no local storage),
// does nothing when the browser sends Do Not Track or Global Privacy Control, and sends only the
// allowed properties below: never file names, design content, images or typed text. The server
// adds this script only when POSTHOG_KEY is set (src/analytics.rs keeps the same lists).
(function () {
  'use strict';
  var nav = navigator;
  if (nav.doNotTrack === '1' || window.doNotTrack === '1' || nav.globalPrivacyControl === true) return;
  var me = document.currentScript;
  if (!me) return;
  var key = me.getAttribute('data-key');
  var host = me.getAttribute('data-host');
  if (!key || !host) return;

  var ALLOWED = ['app', 'app_version', 'platform', 'utm_source', 'utm_medium', 'utm_campaign',
    'utm_content', 'utm_term', 'step', 'format', 'error_code', 'duration_ms', 'ok'];
  var EVENTS = ['landing_viewed', 'project_started', 'file_imported', 'import_failed',
    'preview_viewed', 'download_clicked', 'error_shown'];

  // A random id that lives only as long as this page; no person profile is made.
  var id = 'a';
  try {
    var bytes = new Uint8Array(12);
    crypto.getRandomValues(bytes);
    id = Array.prototype.map.call(bytes, function (b) { return ('0' + b.toString(16)).slice(-2); }).join('');
  } catch (e) { return; }

  var utm = {};
  try {
    new URLSearchParams(location.search).forEach(function (v, k) {
      if (/^utm_(source|medium|campaign|content|term)$/.test(k)) utm[k] = v.slice(0, 100);
    });
  } catch (e) { /* no utm */ }

  function send(name, props) {
    if (EVENTS.indexOf(name) < 0) return;
    var out = { app: 'makerfoundry', platform: 'web', $process_person_profile: false };
    [utm, props || {}].forEach(function (src) {
      Object.keys(src).forEach(function (k) {
        if (ALLOWED.indexOf(k) >= 0) out[k] = src[k];
      });
    });
    try {
      fetch(host + '/capture/', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ api_key: key, event: name, distinct_id: id, properties: out }),
        keepalive: true,
        credentials: 'omit'
      }).catch(function () {});
    } catch (e) { /* analytics never breaks a page */ }
  }

  if (location.pathname === '/') send('landing_viewed');
  document.addEventListener('click', function (ev) {
    var el = ev.target && ev.target.closest ? ev.target.closest('[data-mf-event]') : null;
    if (!el) return;
    var props = {};
    var fmt = el.getAttribute('data-mf-format');
    if (fmt) props.format = fmt.slice(0, 20);
    send(el.getAttribute('data-mf-event'), props);
  });
})();
