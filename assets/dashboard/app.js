'use strict';
// amm read-only dashboard. Server strings are only ever set via textContent.

const COLUMNS = [
  { key: 'queued', label: 'Queued', states: ['draft', 'queued'] },
  { key: 'running', label: 'Running', states: ['running'] },
  { key: 'done', label: 'Done', states: ['done', 'merging'] },
  { key: 'merged', label: 'Merged', states: ['merged'] },
  { key: 'attention', label: 'Needs attention', states: ['blocked', 'failed', 'cancelled'] },
];

const view = { state: null, openWorker: null, detail: null, skew: 0 };
let stateTimer = null, stateLast = 0;

const $ = (id) => document.getElementById(id);

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = String(text);
  return e;
}

// ---------- formatting ----------

function fmtCost(n) {
  const v = Number(n) || 0;
  return '$' + (v > 0 && v < 0.01 ? v.toFixed(4) : v.toFixed(2));
}

function nowMs() { return Date.now() + view.skew; }

function fmtAgo(ts) {
  if (!ts) return 'never';
  const s = Math.max(0, Math.floor((nowMs() - ts) / 1000));
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  if (s < 86400) return Math.floor(s / 3600) + 'h ago';
  return Math.floor(s / 86400) + 'd ago';
}

// A span whose text is refreshed every second from its data-ts.
function agoSpan(ts, prefix) {
  const s = el('span', 'ago', (prefix || '') + fmtAgo(ts));
  s.dataset.ts = String(ts || 0);
  s.dataset.prefix = prefix || '';
  return s;
}

function tickAgo() {
  document.querySelectorAll('.ago').forEach((s) => {
    s.textContent = s.dataset.prefix + fmtAgo(Number(s.dataset.ts));
  });
}

function badge(text, cls) { return el('span', 'badge' + (cls ? ' ' + cls : ''), text); }

// ---------- fetching ----------

async function getJson(url) {
  const r = await fetch(url, { credentials: 'same-origin', headers: { Accept: 'application/json' } });
  if (!r.ok) throw new Error(url + ' ' + r.status);
  return r.json();
}

async function fetchState() {
  stateLast = Date.now();
  try {
    view.state = await getJson('/dash/state');
    view.skew = view.state.now ? view.state.now - Date.now() : 0;
    renderAll();
  } catch (e) {
    $('runs').replaceChildren(el('p', 'muted', 'Could not load state: ' + e.message));
  }
}

function scheduleState() {
  if (stateTimer) return;
  const wait = Math.max(0, 500 - (Date.now() - stateLast));
  stateTimer = setTimeout(() => { stateTimer = null; fetchState(); }, wait);
}

async function fetchDetail(id) {
  try {
    const d = await getJson('/dash/worker/' + encodeURIComponent(id));
    if (view.openWorker !== id) return;
    view.detail = d;
    renderPanel();
  } catch (e) {
    if (view.openWorker !== id) return;
    view.detail = null;
    $('panel').replaceChildren(closeButton(), el('p', 'muted', 'Could not load worker: ' + e.message));
  }
}

// ---------- header / sessions ----------

function renderHeader(s) {
  const mk = (label, v) => {
    const d = el('span', null, label + ' ');
    d.appendChild(el('b', null, fmtCost(v)));
    return d;
  };
  $('totals').replaceChildren(mk('Workers', s.totals.workersUsd), mk('Managers', s.totals.managersUsd));
}

function setLive(on) {
  $('live').className = 'live ' + (on ? 'on' : 'off');
  $('live-text').textContent = on ? 'live' : 'disconnected';
}

function renderSessions(s) {
  const box = $('sessions');
  box.className = 'sessions';
  box.replaceChildren(...s.sessions.map((x) => {
    const d = el('div', 'session');
    d.appendChild(el('div', null, x.cwd));
    const m = el('div', 'meta');
    m.append(el('span', null, fmtCost(x.costUsd)), agoSpan(x.lastSeen, 'seen '));
    d.appendChild(m);
    return d;
  }));
}

// ---------- runs / cards ----------

function currentWorker(brief) {
  return brief.workers.length ? brief.workers[brief.workers.length - 1] : null;
}

function renderCard(brief) {
  const w = currentWorker(brief);
  const c = el('button', 'card');
  c.type = 'button';
  if (w) {
    if (w.id === view.openWorker) c.classList.add('sel');
    c.addEventListener('click', () => openPanel(w.id));
  } else {
    c.disabled = true;
  }
  c.appendChild(el('div', 't', '#' + brief.seq + ' ' + brief.title));
  const m = el('div', 'meta');
  m.append(badge(brief.tier, brief.tier), badge(brief.state), el('span', null, 'attempt ' + brief.attempt));
  if (w) m.append(el('span', null, w.toolCalls + ' calls'), agoSpan(w.lastEventAt, 'last '));
  c.appendChild(m);
  if (w && w.firedRules.length) {
    const r = el('div', 'meta');
    w.firedRules.forEach((x) => r.appendChild(badge(x, 'rule')));
    c.appendChild(r);
  }
  return c;
}

function renderBoard(run) {
  const board = el('div', 'board');
  COLUMNS.forEach((col) => {
    const items = run.briefs.filter((b) => col.states.includes(b.state));
    const d = el('div', 'col' + (col.key === 'attention' ? ' attention' : ''));
    d.appendChild(el('h4', null, col.label + ' (' + items.length + ')'));
    items.forEach((b) => d.appendChild(renderCard(b)));
    board.appendChild(d);
  });
  return board;
}

function renderRun(run) {
  const sec = el('article', 'run');
  const h = el('div', 'run-head');
  h.append(
    el('span', 'title', run.repo + ' @ ' + run.baseBranch),
    badge(run.state),
    el('span', 'muted', run.integrationBranch),
    el('span', 'muted', run.merges + ' merges'),
    el('span', 'muted', 'tests ' + (run.lastTestOk === null ? 'n/a' : run.lastTestOk ? 'ok' : 'failing')),
    el('span', null, fmtCost(run.costUsd)),
    agoSpan(run.createdAt, 'started '),
  );
  sec.append(h, renderBoard(run));
  return sec;
}

function renderRuns(s) {
  const box = $('runs');
  if (!s.runs.length) { box.replaceChildren(el('p', 'muted', 'No runs yet.')); return; }
  box.replaceChildren(...s.runs.map(renderRun));
}

function renderAll() {
  const s = view.state;
  if (!s) return;
  renderHeader(s);
  renderSessions(s);
  renderRuns(s);
}

// ---------- panel ----------

function closeButton() {
  const b = el('button', 'close', 'Close');
  b.type = 'button';
  b.addEventListener('click', closePanel);
  return b;
}

function openPanel(id) {
  view.openWorker = id;
  view.detail = null;
  const p = $('panel');
  p.hidden = false;
  p.replaceChildren(closeButton(), el('p', 'muted', 'Loading...'));
  renderAll();
  fetchDetail(id);
}

function closePanel() {
  view.openWorker = null;
  view.detail = null;
  $('panel').hidden = true;
  renderAll();
}

function listSection(title, items) {
  const f = document.createDocumentFragment();
  f.appendChild(el('h3', null, title));
  if (!items || !items.length) { f.appendChild(el('p', 'muted', 'none')); return f; }
  const ul = el('ul');
  items.forEach((i) => ul.appendChild(el('li', null, i)));
  f.appendChild(ul);
  return f;
}

function renderReport(report) {
  const f = document.createDocumentFragment();
  f.appendChild(el('h3', null, 'Report'));
  if (!report) { f.appendChild(el('p', 'muted', 'no report yet')); return f; }
  f.appendChild(el('p', null, 'Status: ' + report.status));
  if (report.files && report.files.length) f.appendChild(listSection('Files', report.files));
  if (report.notes) f.appendChild(el('pre', null, report.notes));
  return f;
}

function eventClass(kind) {
  if (/deny|fail|error|lost|block/i.test(kind)) return 'bad';
  if (/merge|pass|ok|done/i.test(kind)) return 'ok';
  return '';
}

function summarize(detail) {
  let s;
  try { s = JSON.stringify(detail); } catch (e) { s = String(detail); }
  if (!s || s === '{}') return '';
  return s.length > 300 ? s.slice(0, 300) + '...' : s;
}

function renderTimeline(events) {
  const f = document.createDocumentFragment();
  f.appendChild(el('h3', null, 'Timeline (' + events.length + ')'));
  const ul = el('ul', 'timeline');
  events.slice().reverse().forEach((e) => {
    const li = el('li');
    const head = el('div');
    head.append(el('span', 'k ' + eventClass(e.kind), e.kind), document.createTextNode(' '), agoSpan(e.ts, ''));
    li.appendChild(head);
    const d = summarize(e.detail);
    if (d) li.appendChild(el('div', 'd', d));
    ul.appendChild(li);
  });
  f.appendChild(ul);
  return f;
}

function renderPanel() {
  const d = view.detail;
  const p = $('panel');
  if (!d) return;
  const w = d.worker;
  const head = el('div', 'meta');
  head.append(badge(w.state), badge(w.tier, w.tier), el('span', null, w.model || 'model n/a'),
    el('span', null, w.toolCalls + ' calls'), el('span', null, fmtCost(w.costUsd)),
    agoSpan(w.lastEventAt, 'last '));
  const kids = [closeButton(), el('h2', null, d.brief.title), head];
  if (w.branch) kids.push(el('p', 'muted', w.branch));
  if (w.firedRules.length) {
    const r = el('div', 'meta');
    w.firedRules.forEach((x) => r.appendChild(badge(x, 'rule')));
    kids.push(r);
  }
  kids.push(el('h3', null, 'Goal'), el('pre', null, d.brief.goal),
    listSection('Scope', d.brief.scope), listSection('Checks', d.brief.checks),
    listSection('Notes', d.brief.notes), renderReport(d.brief.report), renderTimeline(d.events));
  const top = p.scrollTop;
  p.replaceChildren(...kids);
  p.scrollTop = top;
}

// ---------- events ----------

function onEvent(msg) {
  scheduleState();
  let ev = null;
  try { ev = JSON.parse(msg.data); } catch (e) { return; }
  if (ev && view.openWorker && ev.workerId === view.openWorker) fetchDetail(view.openWorker);
}

function connect() {
  const es = new EventSource('/dash/events');
  es.addEventListener('amm', onEvent);
  es.onopen = () => { setLive(true); scheduleState(); };
  es.onerror = () => setLive(false); // EventSource reconnects by itself
}

document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && view.openWorker) closePanel(); });

setLive(false);
fetchState();
connect();
setInterval(tickAgo, 1000);
