// The chamber: her live state, the controls, the vigil, and what she keeps.
'use strict';

(() => {
  const { api, live, el, roman, words, Cap, longDay } = LH;
  const $ = (id) => document.getElementById(id);

  const STATUS = {
    closed: 'The rite is closed', asleep: 'She sleeps', listening: 'She listens',
    thinking: 'She ponders', speaking: 'She speaks',
  };
  const VOICE_LABEL = {
    closed: 'The rite is closed', asleep: 'The voice slumbers', listening: 'The voice listens',
    thinking: 'The voice ponders', speaking: 'The voice speaks',
  };

  let snap = null;          // the last /api/chamber
  let state = 'closed';
  let muted = false;
  let vigil = { remaining_secs: 1500, total_secs: 1500, running: false };
  let lastHer = '';
  let whisperTimer = null;

  // --- drawing -----------------------------------------------------------

  function drawState() {
    document.body.dataset.state = state;
    document.body.dataset.muted = String(muted);
    $('status').textContent = STATUS[state] || state;
    $('voice-label').textContent = VOICE_LABEL[state] || '';
    $('deafen').setAttribute('aria-pressed', String(muted));
    $('deafen').querySelector('span').textContent = muted ? 'Let her hear' : 'Deafen her';
    const asleep = state === 'asleep';
    $('slumber').setAttribute('aria-pressed', String(asleep));
    $('slumber').querySelector('span').textContent = asleep ? 'Wake her' : 'Let her slumber';
    const closed = state === 'closed';
    $('rite').querySelector('span').textContent = closed ? 'Open the rite' : 'Close the rite';
    $('rite').classList.toggle('lit', closed);
    $('silence').disabled = state !== 'speaking' && state !== 'thinking';
    $('slumber').disabled = closed;
    $('say-text').disabled = closed;
    $('say-text').placeholder = closed ? 'The rite is closed' : 'Speak in writing…';
    $('voice-line').textContent = lastHer || (closed ? 'Open the rite, and she will answer.' : '');
  }

  function drawVigil() {
    const total = vigil.total_secs;
    const started = vigil.running || vigil.remaining_secs < total;
    const left = started ? vigil.remaining_secs : Number($('length').value) * 60;
    const mm = Math.floor(left / 60), ss = left % 60;
    $('clock').textContent = `${String(mm).padStart(2, '0')}:${String(ss).padStart(2, '0')}`;
    $('bar').style.width = started && total ? `${Math.round(((total - left) / total) * 100)}%` : '0';
    $('begin').textContent = vigil.running ? 'Begun' : (started && left > 0 ? 'Resume' : 'Begin');
    $('begin').disabled = vigil.running;
    $('hold').disabled = !vigil.running;
    $('length').disabled = vigil.running;
  }

  function drawReading(r) {
    $('reading-card').hidden = !r;
    if (!r) return;
    $('reading-where').textContent = `${r.title} · page ${r.page} of ${r.pages}`;
    $('reading-question').textContent = r.question
      ? `${r.again ? 'Asked again: ' : ''}${r.question}`
      : 'She reads the page with you.';
  }

  function word(speaker, text) {
    return el('div', { class: `word ${speaker}` },
      el('span', { class: 'who' }, speaker === 'her' ? 'Her' : 'You'), el('span', {}, text));
  }

  function drawWords(lines) {
    const box = $('words');
    box.replaceChildren(...lines.map((l) => word(l.speaker, l.text)));
    if (!lines.length) box.append(el('div', { class: 'empty' }, state === 'closed' ? 'Nothing is spoken while the rite is closed.' : 'Nothing spoken yet tonight.'));
    box.scrollTop = box.scrollHeight;
  }

  function addWord(speaker, text) {
    const box = $('words');
    box.querySelector('.empty')?.remove();
    const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
    box.append(word(speaker, text));
    if (atEnd) box.scrollTop = box.scrollHeight;
  }

  function drawMemory() {
    const s = snap;
    $('ears').textContent = s.mode === 'voice' ? 'Ears: local' : 'Ears: typed';
    $('mouth').textContent = s.mode === 'voice' ? 'Voice: local' : 'Voice: printed';
    if ($('mission-edit').hidden) $('mission').textContent = s.mission;

    // the seven seals: the chain within its current week, and the whole count
    const c = s.streak;
    const inWeek = c === 0 ? 0 : ((c - 1) % 7) + 1;
    $('numeral').textContent = `${c ? roman(inWeek) : '—'} · VII`;
    $('seals').replaceChildren(...Array.from({ length: 7 }, (_, i) => el('span', { class: i < inWeek ? 'kept' : '' })));
    $('seals-text').textContent = c === 0
      ? 'No seal yet. The chain begins with a kept oath.'
      : `${Cap(words(c))} night${c === 1 ? '' : 's'} kept. ` +
        (inWeek === 7 ? 'The seventh seal is set.' : `${Cap(words(7 - inWeek))} seal${7 - inWeek === 1 ? '' : 's'} remain.`) +
        (c > 7 ? ` Week ${roman(Math.ceil(c / 7))}.` : '');

    // the oath: the due one (with its verdict), else the one sworn for tomorrow
    if (s.pending_oath) {
      $('oath-h').textContent = s.pending_oath.day === s.today ? "Today's oath" : `The oath of ${longDay(s.pending_oath.day)}`;
      $('oath').textContent = s.pending_oath.text;
      $('verdict').hidden = false;
    } else if (s.upcoming_oath) {
      $('oath-h').textContent = "Tomorrow's oath";
      $('oath').textContent = s.upcoming_oath.text;
      $('verdict').hidden = true;
    } else {
      $('oath-h').textContent = 'The oath';
      $('oath').textContent = 'Nothing is sworn. Close the rite, and she will ask for one.';
      $('verdict').hidden = true;
    }

    const f = s.focus_today;
    $('focus').textContent = f.minutes ? `Today: ${f.minutes} min in ${f.vigils} vigil${f.vigils === 1 ? '' : 's'}` : 'Today: no vigil kept yet';

    $('facts').replaceChildren(...(s.facts.length ? s.facts.map((fact) =>
      el('div', { class: 'fact' }, el('span', {}, fact.text),
        el('button', { type: 'button', class: 'btn quiet small-text', onclick: (e) => strike(e.target, fact) }, 'Strike out'))
    ) : [el('p', { class: 'empty', style: 'margin:0' }, 'Nothing yet. She learns as you speak.')]));

    const voice = $('voice');
    if (!voice.options.length) {
      voice.append(...s.voices.map((v) => el('option', { value: v.id }, v.name)));
    }
    if (document.activeElement !== voice) voice.value = s.settings.voice;
    if (document.activeElement !== $('pace')) $('pace').value = s.settings.pace;
    $('pace-value').textContent = `· ${s.settings.pace}%`;
    $('speaks-first').checked = s.settings.speaks_first;
    $('voice').disabled = $('pace').disabled = s.mode !== 'voice';
    drawMics(s);
  }

  // The microphone: automatic, the system default, or one by name. A chosen
  // mic that is not plugged in stays chosen, and the automatic one stands in.
  function drawMics(s) {
    const m = s.mics;
    const select = $('mic');
    const note = $('mic-note');
    if (!m || m.error) {
      select.replaceChildren(el('option', { value: '' }, 'No microphone found'));
      select.disabled = true;
      note.textContent = m && m.error ? `Cannot see the microphones: ${m.error}` : '';
      return;
    }
    const choice = m.choice;
    const isDefault = choice && choice.toLowerCase() === 'default';
    const named = choice && !isDefault
      ? (m.inputs.find((i) => i.name === choice) || m.inputs.find((i) => i.name.toLowerCase().includes(choice.toLowerCase())))
      : null;
    const options = [
      el('option', { value: 'auto' }, 'Automatic (never a headset’s own mic)'),
      el('option', { value: 'default' }, 'The system default'),
      ...m.inputs.map((i) => el('option', { value: i.name }, i.system_default ? `${i.name} · system default` : i.name)),
    ];
    if (choice && !isDefault && !named) options.push(el('option', { value: choice }, `${choice} · not plugged in`));
    if (document.activeElement !== select) {
      select.replaceChildren(...options);
      select.value = !choice ? 'auto' : isDefault ? 'default' : (named ? named.name : choice);
    }
    select.disabled = false;
    const through = m.listening || 'no microphone';
    note.textContent = s.mode !== 'voice'
      ? 'Typed mode opens no mic; your choice is kept for voice mode.'
      : m.missing
        ? `Not plugged in: she listens through ${through} until it returns.`
        : `She listens through ${through}${m.headset_avoided ? ' (your headset’s mic is passed over)' : ''}.`;
  }

  // Strike a fact on the second press, so a slip of the thumb costs nothing.
  function strike(button, fact) {
    if (button.dataset.armed) {
      line.send({ cmd: 'forget_fact', id: fact.id });
      button.disabled = true;
      return;
    }
    button.dataset.armed = '1';
    button.textContent = 'Strike it?';
    setTimeout(() => { if (button.isConnected) { delete button.dataset.armed; button.textContent = 'Strike out'; } }, 4000);
  }

  function whisper(text, isError) {
    const w = $('whisper');
    w.textContent = text;
    w.classList.toggle('error', !!isError);
    clearTimeout(whisperTimer);
    whisperTimer = setTimeout(() => { w.textContent = ''; }, isError ? 12000 : 6000);
  }

  async function refresh() {
    try {
      snap = await api('/api/chamber');
    } catch (e) {
      whisper(`Could not read her memory: ${e.message}`, true);
      return;
    }
    state = snap.live.state;
    muted = snap.live.muted;
    vigil = snap.live.vigil;
    const hers = snap.transcript.filter((l) => l.speaker === 'her');
    lastHer = hers.length ? hers[hers.length - 1].text : '';
    drawState();
    drawVigil();
    drawReading(snap.live.reading);
    drawWords(snap.transcript);
    drawMemory();
  }

  // --- the live line -----------------------------------------------------

  let pending = null;
  const later = () => { clearTimeout(pending); pending = setTimeout(refresh, 150); };

  const line = live({
    onOpen: () => { $('conn').hidden = true; refresh(); },
    onDown: () => { $('conn').hidden = false; },
    onEvent: (e) => {
      switch (e.event) {
        case 'state_changed':
          state = e.state; muted = e.muted; drawState(); break;
        case 'user_spoke':
          addWord('me', e.text); break;
        case 'she_spoke':
          lastHer = e.text; addWord('her', e.text); drawState(); break;
        case 'timer_tick':
          vigil = { remaining_secs: e.remaining_secs, total_secs: e.total_secs, running: e.running }; drawVigil(); break;
        case 'session_started':
          lastHer = ''; later(); break;
        case 'session_ended':
        case 'memory_changed':
        case 'lagged':
          later(); break;
        case 'reading_changed':
          drawReading(e.reading); break;
        case 'notice':
          whisper(e.text); break;
        case 'error':
          whisper(e.message, true); break;
        case 'goodbye':
          whisper('She has gone silent: the program ended.', true); break;
      }
    },
  });
  const send = (cmd) => line.send(cmd);

  // --- controls ----------------------------------------------------------

  $('deafen').onclick = () => send({ cmd: 'mute', on: !muted });
  $('silence').onclick = () => send({ cmd: 'stop_speaking' });
  $('slumber').onclick = () => send({ cmd: 'sleep', on: state !== 'asleep' });
  $('rite').onclick = () => send({ cmd: state === 'closed' ? 'open_rite' : 'end_session' });

  for (let m = 5; m <= 90; m += 5) $('length').append(el('option', { value: m }, `${m} min`));
  $('length').value = 25;
  $('length').onchange = drawVigil;
  $('begin').onclick = () => {
    const started = vigil.remaining_secs < vigil.total_secs && vigil.remaining_secs > 0;
    const minutes = Number($('length').value);
    // resume a held vigil of the same length; otherwise begin anew at the chosen length
    send(started && minutes * 60 === vigil.total_secs ? { cmd: 'timer_start' } : { cmd: 'timer_start', minutes });
  };
  $('hold').onclick = () => send({ cmd: 'timer_pause' });
  $('anew').onclick = () => send({ cmd: 'timer_reset' });

  $('say').onsubmit = (e) => {
    e.preventDefault();
    const text = $('say-text').value.trim();
    if (!text) return;
    send({ cmd: 'say', text });
    $('say-text').value = '';
  };

  $('kept').onclick = () => send({ cmd: 'oath_verdict', kept: true });
  $('broken').onclick = () => send({ cmd: 'oath_verdict', kept: false });

  $('rename').onclick = () => {
    $('mission-text').value = snap ? snap.mission : '';
    $('mission').hidden = true;
    $('mission-edit').hidden = false;
    $('mission-text').focus();
  };
  const closeEdit = () => { $('mission-edit').hidden = true; $('mission').hidden = false; };
  $('mission-cancel').onclick = closeEdit;
  $('mission-edit').onsubmit = (e) => {
    e.preventDefault();
    const text = $('mission-text').value.trim();
    if (text) { send({ cmd: 'set_mission', text }); $('mission').textContent = text; }
    closeEdit();
  };

  const setVoice = () => send({ cmd: 'set_voice', voice: Number($('voice').value), pace: Number($('pace').value) });
  $('voice').onchange = setVoice;
  $('pace').oninput = () => { $('pace-value').textContent = `· ${$('pace').value}%`; };
  $('pace').onchange = setVoice;
  $('speaks-first').onchange = (e) => send({ cmd: 'set_speaks_first', on: e.target.checked });
  $('mic').onchange = (e) => {
    const v = e.target.value;
    send({ cmd: 'set_mic', mic: v === 'auto' ? null : v });
    e.target.blur();
  };
  $('mic-look').onclick = refresh;
})();
