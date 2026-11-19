// The grimoire: the shelf, the real page (pdf.js, served from this machine),
// her question, and every question asked before.
import * as pdfjs from './pdfjs/pdf.min.mjs';

const { api, live, el, shortDay } = LH;
const $ = (id) => document.getElementById(id);

pdfjs.GlobalWorkerOptions.workerSrc = '/pdfjs/pdf.worker.min.mjs';

let shelf = null;          // the last /api/grimoire
let view = null;           // { book, page } shown in the viewer
let reading = null;        // the page she reads, from the core
let readingKey = '';       // to follow her when she turns to another page
let whisperTimer = null;

const bookById = (id) => shelf?.books.find((b) => b.id === id) || null;
const readable = (b) => b && b.status === 'ok' && !b.missing;
const viewable = (b) => b && b.status !== 'failed' && !b.missing;

function whisper(text, isError) {
  const w = $('whisper');
  w.textContent = text;
  w.classList.toggle('error', !!isError);
  clearTimeout(whisperTimer);
  whisperTimer = setTimeout(() => { w.textContent = ''; }, isError ? 12000 : 6000);
}

// --- the page ------------------------------------------------------------

let loading = null;         // pdf.js loading task of the book open in the viewer
let docBook = null;
let task = null;
let drawn = 0;

// One book open at a time. (pdf.js 6: only the loading task can be destroyed.)
function documentOf(book) {
  if (loading && docBook === book) return loading.promise;
  if (loading) loading.destroy();
  docBook = book;
  loading = pdfjs.getDocument({
    url: `/api/books/${book}/file`,
    cMapUrl: '/pdfjs/cmaps/',
    cMapPacked: true,
    standardFontDataUrl: '/pdfjs/standard_fonts/',
    wasmUrl: '/pdfjs/wasm/',
    isEvalSupported: false,
    enableXfa: false,
  });
  return loading.promise;
}

async function render() {
  if (!view) return;
  const mine = ++drawn;
  const canvas = $('canvas');
  try {
    const d = await documentOf(view.book);
    if (mine !== drawn) return;
    const page = await d.getPage(Math.min(Math.max(view.page, 1), d.numPages));
    if (mine !== drawn) return;
    const width = $('sheet').clientWidth;
    const scale = width / page.getViewport({ scale: 1 }).width;
    const dpr = window.devicePixelRatio || 1;
    const viewport = page.getViewport({ scale: scale * dpr });
    if (task) task.cancel();
    canvas.width = Math.floor(viewport.width);
    canvas.height = Math.floor(viewport.height);
    canvas.style.width = `${width}px`;
    canvas.style.height = `${Math.floor(viewport.height / dpr)}px`;
    canvas.hidden = false;
    $('sheet-empty').hidden = true;
    task = page.render({ canvas, viewport });
    await task.promise;
  } catch (e) {
    if (e && e.name === 'RenderingCancelledException') return;
    if (mine !== drawn) return;
    if (loading) loading.destroy();
    loading = null;             // a failed book is fetched afresh next time
    docBook = null;
    canvas.hidden = true;
    $('sheet-empty').hidden = false;
    $('sheet-empty').textContent = `The page will not show: ${e.message || e}`;
  }
}

function show(book, page) {
  const b = bookById(book);
  if (!viewable(b)) return;
  const last = Math.max(b.pages, 1);
  view = { book, page: Math.min(Math.max(page, 1), last) };
  drawLeaf();
  drawShelf();
  render();
}

function drawLeaf() {
  const b = view && bookById(view.book);
  $('pager').hidden = !b;
  $('read-this').disabled = !b || !readable(b) || !shelf?.open;
  if (!b) {
    $('leaf-h').textContent = 'The grimoire';
    $('leaf-meta').replaceChildren();
    $('canvas').hidden = true;
    $('sheet-empty').hidden = false;
    return;
  }
  $('leaf-h').textContent = b.title;
  const hers = reading && reading.book === b.id && reading.page === view.page;
  $('leaf-meta').replaceChildren(...[
    el('span', {}, `Page ${view.page} of ${b.pages}`),
    hers ? el('span', { class: 'bright', style: 'color: var(--blood)' }, 'She reads this page') : null,
    b.id === shelf.current ? el('span', {}, `Your place: page ${b.last_page || '—'}`) : null,
  ].filter(Boolean));
  $('page-no').value = view.page;
  $('page-no').max = b.pages;
  $('page-of').textContent = `of ${b.pages}`;
  $('prev').disabled = view.page <= 1;
  $('next').disabled = view.page >= b.pages;
}

// --- the shelf and the questions -------------------------------------------

function bookState(b) {
  if (b.missing) return 'Gone from the folder';
  if (b.status === 'failed') return `Unreadable: ${b.error || 'not a PDF she can open'}`;
  if (b.status === 'no_text') return `${b.pages} pages · no text to read (a scan?)`;
  const place = b.last_page ? `read to page ${b.last_page}` : 'not begun';
  return `${b.pages} pages · ${place}`;
}

function drawShelf() {
  const box = $('books');
  if (!shelf.books.length) {
    box.replaceChildren(el('p', { class: 'empty', style: 'margin:0' },
      shelf.dir_exists
        ? 'No PDF in the books folder yet. Add some, then read the shelf again.'
        : `There is no books folder at ${shelf.dir}. Set BOOKS_DIR, or create it.`));
    return;
  }
  box.replaceChildren(...shelf.books.map((b) => {
    const current = b.id === shelf.current;
    return el('div', { class: 'book-row' },
      el('button', {
        type: 'button', class: `night book${readable(b) ? '' : ' unread'}`,
        'aria-pressed': String(view?.book === b.id), disabled: !viewable(b),
        onclick: () => show(b.id, b.id === view?.book ? view.page : Math.max(b.last_page, 1)),
      },
      el('span', { class: 'book-name' }, b.title),
      el('span', { class: 'small muted' }, bookState(b))),
      readable(b) && !current
        ? el('button', { type: 'button', class: 'btn quiet small-text', onclick: () => line.send({ cmd: 'set_book', book: b.id }) }, 'Read this book')
        : current ? el('span', { class: 'small muted book-current' }, 'The book you read') : null);
  }));
}

function drawAsked() {
  const t = shelf.tally;
  $('tally').textContent = t.answered ? `${t.right} of ${t.answered} right` : '';
  const box = $('asked');
  if (!shelf.questions.length) {
    box.replaceChildren(el('p', { class: 'empty', style: 'margin:0' }, 'None yet. Read a page with her.'));
    return;
  }
  box.replaceChildren(...shelf.questions.slice(0, 30).map((q) => {
    const verdict = q.correct === true ? 'Right' : q.correct === false ? 'Wrong' : q.answer ? 'Weighing' : 'Unanswered';
    return el('div', { class: 'q' },
      el('span', { class: 'muted small q-where' },
        `${q.title || 'A lost book'} · p. ${q.page} · ${shortDay(q.asked_at.slice(0, 10))}${q.first_id ? ' · asked again' : ''}`),
      el('p', { style: 'margin:0' }, q.question),
      q.answer ? el('p', { class: 'small', style: 'margin:0' }, el('span', { class: 'muted' }, 'You: '), q.answer) : null,
      el('span', { class: `small verdict ${q.correct === true ? 'right' : q.correct === false ? 'wrong' : ''}` },
        verdict, q.judgement && q.correct !== null ? el('span', { class: 'muted' }, ` · ${q.judgement}`) : null));
  }));
}

function drawReading() {
  const asking = reading && reading.question;
  $('question-card').hidden = !asking;
  if (asking) {
    $('question').textContent = reading.question;
    $('question-tag').textContent = reading.again ? 'Once answered wrong' : `${reading.title}, page ${reading.page}`;
  }
  $('leave').hidden = !reading;
  $('her-line').hidden = !reading || !$('her-line').textContent;
  const closed = !shelf?.open;
  for (const id of ['read-next', 'read-oath', 'read-random']) $(id).disabled = closed;
  $('answer-text').disabled = closed;
  $('read-note').textContent = closed
    ? 'The rite is closed. Open it in the Chamber, and she will read with you.'
    : 'She explains the page in three sentences, then asks two questions.';
  drawLeaf();
}

async function load() {
  try {
    shelf = await api('/api/grimoire');
  } catch (e) {
    whisper(`Could not read the shelf: ${e.message}`, true);
    return;
  }
  reading = shelf.reading;
  const folder = shelf.dir.replace(/\/+$/, '').split('/').pop() || shelf.dir;
  $('dir').textContent = shelf.indexing ? 'Reading the shelf…' : `From the folder “${folder}”`;
  $('dir').title = shelf.dir;
  $('reindex').disabled = shelf.indexing;
  $('reindex').textContent = shelf.indexing ? 'Reading…' : 'Read the shelf again';
  const key = reading ? `${reading.book}:${reading.page}` : '';
  const turned = key !== readingKey;
  readingKey = key;
  if (!reading) $('her-line').textContent = '';
  drawShelf();
  drawAsked();
  drawReading();
  if (reading && turned) {
    show(reading.book, reading.page);
  } else if (view && !bookById(view.book)) {
    view = null;
    drawLeaf();
  } else if (!view && shelf.current) {
    const b = bookById(shelf.current);
    show(b.id, Math.max(b.last_page, 1));
  }
}

// --- the live line -----------------------------------------------------------

let pending = null;
const later = () => { clearTimeout(pending); pending = setTimeout(load, 150); };

const line = live({
  onOpen: () => { $('conn').hidden = true; later(); },
  onDown: () => { $('conn').hidden = false; },
  onEvent: (e) => {
    switch (e.event) {
      case 'reading_changed':
      case 'shelf_changed':
      case 'session_started':
      case 'session_ended':
      case 'memory_changed':
      case 'lagged':
        later(); break;
      case 'she_spoke':
        if (reading) { $('her-line').textContent = e.text; $('her-line').hidden = false; }
        break;
      case 'notice':
        whisper(e.text); break;
      case 'error':
        whisper(e.message, true); break;
    }
  },
});

// --- controls ----------------------------------------------------------------

$('prev').onclick = () => view && show(view.book, view.page - 1);
$('next').onclick = () => view && show(view.book, view.page + 1);
$('page-no').onchange = () => {
  const n = Number($('page-no').value);
  if (view && n >= 1) show(view.book, n);
};
$('read-this').onclick = () => view && line.send({ cmd: 'read_page', book: view.book, page: view.page });
$('read-next').onclick = () => line.send({ cmd: 'read', from: 'next' });
$('read-oath').onclick = () => line.send({ cmd: 'read', from: 'oath' });
$('read-random').onclick = () => line.send({ cmd: 'read', from: 'random' });
$('leave').onclick = () => line.send({ cmd: 'leave_page' });
$('reindex').onclick = () => line.send({ cmd: 'reindex' });
$('answer').onsubmit = (e) => {
  e.preventDefault();
  const text = $('answer-text').value.trim();
  if (!text) return;
  line.send({ cmd: 'say', text });
  $('answer-text').value = '';
};

let resized = null;
window.addEventListener('resize', () => { clearTimeout(resized); resized = setTimeout(render, 200); });
