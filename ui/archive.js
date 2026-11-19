// The archive: every night, its account and words, search, and burning.
'use strict';

(() => {
  const { api, live, el, roman, longDay, shortDay } = LH;
  const $ = (id) => document.getElementById(id);

  let nights = [];
  let openSession = null;
  let chosen = Number(location.hash.slice(1)) || null;

  const status = (s) => {
    if (s.id === openSession) return 'the rite is open';
    if (!s.ended_at) return 'left unsealed';
    return s.summary ? 'sealed' : 'sealed without an account';
  };

  function drawNights() {
    const list = $('nights');
    const next = (nights.length ? nights[0].id : 0) + 1;
    const unwritten = openSession === null
      ? [el('div', { class: 'night unwritten' },
          el('span', { class: 'night-name' }, `NOX ${roman(next)}`), el('span', { class: 'small muted' }, 'Not yet written'))]
      : [];
    list.replaceChildren(...unwritten, ...nights.map((s) =>
      el('button', {
        type: 'button', class: 'night', 'aria-pressed': String(s.id === chosen),
        onclick: () => choose(s.id),
      },
      el('span', { class: 'night-name' }, `NOX ${roman(s.id)}`),
      el('span', { class: 'small muted' }, `${longDay(s.day)} · ${status(s)}`))));
    if (!nights.length) list.append(el('p', { class: 'empty', style: 'margin:0' }, 'No night has been kept yet.'));
  }

  async function loadNights() {
    const body = await api('/api/sessions');
    nights = body.sessions;
    openSession = body.open;
    if (!chosen || !nights.some((s) => s.id === chosen)) chosen = nights.length ? nights[0].id : null;
    drawNights();
    if (chosen) await showNight(chosen);
    else $('night').replaceChildren(el('section', { class: 'card account' }, el('p', { class: 'empty', style: 'margin:0' }, 'The archive is empty.')));
  }

  async function choose(id) {
    chosen = id;
    history.replaceState(null, '', `#${id}`);
    drawNights();
    await showNight(id);
  }

  function oathState(s) {
    if (!s.oath) return 'Oath: none sworn';
    if (s.oath_kept === true) return 'Oath: kept';
    if (s.oath_kept === false) return 'Oath: broken';
    return 'Oath: unpaid';
  }

  async function showNight(id) {
    let body;
    try { body = await api(`/api/sessions/${id}`); } catch (e) {
      $('night').replaceChildren(el('section', { class: 'card account' }, el('p', { class: 'empty', style: 'margin:0' }, e.message)));
      return;
    }
    const s = body.session;
    const account = s.summary
      || (body.open ? 'The rite is open. The account is written when it closes.' : 'No account was written for this night.');
    const copyText = () => [
      `NOX ${roman(s.id)} · ${shortDay(s.day)}`, '', account, '',
      s.oath ? `Sworn: ${s.oath}` : '', '',
      ...body.lines.map((l) => `${l.speaker === 'her' ? 'Her' : 'You'}: ${l.text}`),
    ].join('\n');

    const hear = el('button', { type: 'button', class: 'btn', disabled: !s.summary,
      onclick: () => { line.send({ cmd: 'read_aloud', session: s.id }); hear.textContent = 'She speaks it'; setTimeout(() => { hear.textContent = 'Hear it spoken'; }, 3000); } }, 'Hear it spoken');
    const copy = el('button', { type: 'button', class: 'btn', onclick: async () => {
      try { await navigator.clipboard.writeText(copyText()); copy.textContent = 'Copied'; } catch { copy.textContent = 'Copy failed'; }
      setTimeout(() => { copy.textContent = 'Copy it out'; }, 2500);
    } }, 'Copy it out');
    const burn = el('button', { type: 'button', class: 'btn quiet', disabled: body.open, title: body.open ? 'Close the rite first' : '' }, 'Burn this night');
    const keep = el('button', { type: 'button', class: 'btn', hidden: true }, 'Keep it');
    burn.onclick = () => {
      if (burn.dataset.armed) {
        line.send({ cmd: 'burn_session', session: s.id });
        burn.disabled = keep.hidden = true;
        burn.textContent = 'Burning…';
        return;
      }
      burn.dataset.armed = '1';
      burn.textContent = 'Burn it: every word, and what she learned';
      burn.classList.replace('quiet', 'primary');
      keep.hidden = false;
    };
    keep.onclick = () => { delete burn.dataset.armed; burn.textContent = 'Burn this night'; burn.classList.replace('primary', 'quiet'); keep.hidden = true; };

    const vigilText = s.focused_minutes ? `Vigil: ${s.focused_minutes} min` : 'Vigil: not begun';
    $('night').replaceChildren(
      el('section', { class: 'card account' },
        el('div', { class: 'row-between' },
          el('h1', { class: 'night-title' }, `NOX ${roman(s.id)}`),
          el('div', { class: 'meta' }, el('span', {}, shortDay(s.day)), el('span', {}, vigilText), el('span', { class: 'bright', style: 'color: var(--bone)' }, oathState(s)))),
        el('h2', { class: 'h', style: 'margin-top: 4px' }, 'The account'),
        el('p', { class: 'script account-text' }, account),
        el('div', { class: 'pair', style: 'padding-top: 4px' }, hear, copy, burn, keep)),
      el('section', { class: 'pair-cards' },
        el('div', { class: 'card' }, el('h2', { class: 'h' }, 'What was sworn'),
          el('p', { style: 'margin:0' }, s.oath || 'Nothing was sworn.'),
          s.oath_day ? el('span', { class: 'small muted' }, `for ${longDay(s.oath_day)}`) : null),
        el('div', { class: 'card' }, el('h2', { class: 'h' }, 'What she learned'),
          body.learned.length
            ? el('div', { class: 'facts-of-night' }, ...body.learned.map((f) => el('p', { style: 'margin:0' }, f.text)))
            : el('p', { class: 'empty', style: 'margin:0' }, 'Nothing new.'))),
      el('section', { class: 'card', style: 'background:none;border:0;padding:0' },
        el('h2', { class: 'h' }, 'Every word'),
        el('div', { class: 'words' }, body.lines.length
          ? body.lines.map((l) => el('div', { class: `word ${l.speaker}` },
              el('span', { class: 'who' }, l.speaker === 'her' ? 'Her' : 'You'), el('span', {}, l.text)))
          : el('div', { class: 'empty' }, 'Nothing was said.'))),
    );
  }

  // --- search --------------------------------------------------------------

  let searchTimer = null;
  $('search').oninput = () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(runSearch, 250);
  };

  async function runSearch() {
    const q = $('search').value.trim();
    const box = $('hits');
    if (!q) { box.hidden = true; box.replaceChildren(); return; }
    let body;
    try { body = await api(`/api/search?q=${encodeURIComponent(q)}`); } catch (e) { box.hidden = false; box.replaceChildren(el('p', { class: 'empty' }, e.message)); return; }
    box.hidden = false;
    const terms = q.toLowerCase().split(/\W+/).filter((t) => t.length > 2);
    const mark = (text) => {
      if (!terms.length) return [text];
      const re = new RegExp(`(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})`, 'ig');
      return text.split(re).map((part, i) => (i % 2 ? el('mark', {}, part) : part));
    };
    box.replaceChildren(...(body.hits.length ? body.hits.map((h) =>
      el('button', { type: 'button', class: 'hit', onclick: () => choose(h.session) },
        el('span', { class: 'muted' }, `NOX ${roman(h.session)} · ${shortDay(h.day)} · ${h.speaker === 'her' ? 'her' : 'you'}`),
        ...mark(h.text))
    ) : [el('p', { class: 'empty', style: 'margin:0' }, 'Nothing was ever said of it.')]));
  }

  // --- live ----------------------------------------------------------------

  let pending = null;
  const later = () => { clearTimeout(pending); pending = setTimeout(() => loadNights().catch(() => {}), 200); };
  const line = live({
    onOpen: () => { $('conn').hidden = true; later(); },
    onDown: () => { $('conn').hidden = false; },
    onEvent: (e) => {
      if (['session_started', 'session_ended', 'memory_changed', 'lagged'].includes(e.event)) later();
      else if ((e.event === 'user_spoke' || e.event === 'she_spoke') && chosen === openSession) later();
    },
  });

  window.addEventListener('hashchange', () => {
    const id = Number(location.hash.slice(1));
    if (id && id !== chosen) choose(id);
  });
})();
