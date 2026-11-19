// Shared by the chamber and the archive: the API, the live socket, and words.
'use strict';

const LH = (() => {
  async function api(path) {
    const res = await fetch(path, { headers: { Accept: 'application/json' } });
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || res.statusText);
    return body;
  }

  // One WebSocket, reopened after a drop. onEvent gets every core event;
  // onOpen runs on every (re)connect so the page can re-read its state.
  function live({ onEvent, onOpen, onDown }) {
    let ws = null;
    let delay = 500;
    const queue = [];
    function connect() {
      ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
      ws.onopen = () => {
        delay = 500;
        while (queue.length) ws.send(queue.shift());
        onOpen && onOpen();
      };
      ws.onmessage = (m) => {
        let event;
        try { event = JSON.parse(m.data); } catch { return; }
        onEvent(event);
      };
      ws.onclose = () => {
        onDown && onDown();
        setTimeout(connect, delay);
        delay = Math.min(delay * 2, 8000);
      };
    }
    connect();
    return {
      send(cmd) {
        const text = JSON.stringify(cmd);
        if (ws && ws.readyState === WebSocket.OPEN) ws.send(text);
        else queue.push(text);
      },
    };
  }

  function el(tag, attrs = {}, ...children) {
    const node = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (k === 'class') node.className = v;
      else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
      else if (v !== false && v != null) node.setAttribute(k, v === true ? '' : v);
    }
    for (const c of children.flat()) {
      if (c != null) node.append(c instanceof Node ? c : document.createTextNode(String(c)));
    }
    return node;
  }

  function roman(n) {
    if (!(n > 0)) return '—';
    const table = [[1000, 'M'], [900, 'CM'], [500, 'D'], [400, 'CD'], [100, 'C'], [90, 'XC'],
      [50, 'L'], [40, 'XL'], [10, 'X'], [9, 'IX'], [5, 'V'], [4, 'IV'], [1, 'I']];
    let out = '';
    for (const [v, s] of table) while (n >= v) { out += s; n -= v; }
    return out;
  }

  const ONES = ['zero', 'one', 'two', 'three', 'four', 'five', 'six', 'seven', 'eight', 'nine', 'ten',
    'eleven', 'twelve', 'thirteen', 'fourteen', 'fifteen', 'sixteen', 'seventeen', 'eighteen', 'nineteen'];
  const TENS = ['', '', 'twenty', 'thirty', 'forty', 'fifty', 'sixty', 'seventy', 'eighty', 'ninety'];
  function words(n) {
    if (n < 20) return ONES[n];
    if (n < 100) return TENS[Math.floor(n / 10)] + (n % 10 ? '-' + ONES[n % 10] : '');
    return String(n);
  }
  const Cap = (s) => s.charAt(0).toUpperCase() + s.slice(1);

  // "2026-11-19" -> Date at local midnight
  const dayDate = (d) => { const [y, m, dd] = d.split('-').map(Number); return new Date(y, m - 1, dd); };
  const longDay = (d) => dayDate(d).toLocaleDateString('en-GB', { weekday: 'long', day: 'numeric', month: 'long' });
  const shortDay = (d) => dayDate(d).toLocaleDateString('en-GB', { day: 'numeric', month: 'short', year: 'numeric' });

  return { api, live, el, roman, words, Cap, longDay, shortDay };
})();
