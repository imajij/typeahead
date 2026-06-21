/* Googol — Google-style typeahead frontend.
   Debounced suggestions with abort + stale guards, keyboard navigation that
   fills the input (Esc restores), localStorage history, trending zero-state,
   did-you-mean correction, and a classic results page. */

const input = document.getElementById('q');
const searchbox = document.getElementById('searchbox');
const searchZone = document.getElementById('search-zone');
const panel = document.getElementById('suggest-panel');
const list = document.getElementById('suggest-list');
const clearBtn = document.getElementById('clear-btn');
const brandHome = document.getElementById('brand-home');
const btnSearch = document.getElementById('btn-search');
const btnLucky = document.getElementById('btn-lucky');
const trendingChips = document.getElementById('trending-chips');
const resultsMeta = document.getElementById('results-meta');
const resultsList = document.getElementById('results');
const didYouMean = document.getElementById('didyoumean');

const HISTORY_KEY = 'googol.history';
const HISTORY_MAX = 10;
const DEBOUNCE_MS = 110;
const SUGGEST_LIMIT = 8;

const ICONS = {
  search: '<svg class="sg-icon" viewBox="0 0 24 24" fill="none"><circle cx="11" cy="11" r="7" stroke="currentColor" stroke-width="2"/><path d="M16.5 16.5L21 21" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>',
  clock: '<svg class="sg-icon" viewBox="0 0 24 24" fill="none"><circle cx="12" cy="12" r="9" stroke="currentColor" stroke-width="2"/><path d="M12 7v5l3.5 2" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>',
  trend: '<svg class="sg-icon" viewBox="0 0 24 24" fill="none"><path d="M3 17l6-6 4 4 8-8" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M15 7h6v6" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  wand: '<svg class="sg-icon" viewBox="0 0 24 24" fill="none"><path d="M5 19L17 7" stroke="currentColor" stroke-width="2" stroke-linecap="round"/><path d="M17 3v2M17 9v2M14 6h2M20 6h2M7 13l1.2 1.2M4 21l.8-.8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>',
};

// ---------- state ----------

let items = [];            // currently rendered dropdown items [{text, type}]
let selectedIndex = -1;
let typedText = '';        // what the user actually typed (restored on Esc)
let debounceTimer = null;
let suggestAbort = null;
let trendingCache = { data: [], at: 0 };

// ---------- utilities ----------

function escapeHtml(str) {
  return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#039;');
}

function escapeRegExp(str) {
  return str.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function normalize(q) {
  return q.toLowerCase().split(/\s+/).filter(Boolean).join(' ');
}

function formatCount(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(1).replace(/\.0$/, '') + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1).replace(/\.0$/, '') + 'k';
  return String(n);
}

/* Google-style emphasis: the part you typed stays regular,
   everything you did not type yet is bold. */
function emphasize(suggestion, typed) {
  const s = escapeHtml(suggestion);
  const t = normalize(typed);
  if (!t) return '<b>' + s + '</b>';
  const idx = suggestion.toLowerCase().indexOf(t);
  if (idx === -1) return '<b>' + s + '</b>';
  const before = escapeHtml(suggestion.slice(0, idx));
  const match = escapeHtml(suggestion.slice(idx, idx + t.length));
  const after = escapeHtml(suggestion.slice(idx + t.length));
  return (before ? '<b>' + before + '</b>' : '') + match + (after ? '<b>' + after + '</b>' : '');
}

// ---------- local search history ----------

function getHistory() {
  try { return JSON.parse(localStorage.getItem(HISTORY_KEY)) || []; }
  catch { return []; }
}

function addToHistory(query) {
  const q = normalize(query);
  if (!q) return;
  const hist = getHistory().filter((h) => h !== q);
  hist.unshift(q);
  localStorage.setItem(HISTORY_KEY, JSON.stringify(hist.slice(0, HISTORY_MAX)));
}

function removeFromHistory(query) {
  localStorage.setItem(HISTORY_KEY, JSON.stringify(getHistory().filter((h) => h !== query)));
}

// ---------- trending ----------

async function getTrending() {
  if (Date.now() - trendingCache.at < 30000) return trendingCache.data;
  try {
    const res = await fetch('/api/trending');
    const data = await res.json();
    trendingCache = { data: data.trending || [], at: Date.now() };
  } catch { /* keep stale */ }
  return trendingCache.data;
}

async function renderTrendingChips() {
  const trending = await getTrending();
  trendingChips.innerHTML = '';
  trending.slice(0, 8).forEach((t) => {
    const li = document.createElement('li');
    const btn = document.createElement('button');
    btn.className = 'chip';
    btn.innerHTML = ICONS.trend + '<span>' + escapeHtml(t.query) + '</span>';
    btn.addEventListener('click', () => runSearch(t.query));
    li.appendChild(btn);
    trendingChips.appendChild(li);
  });
}

// ---------- suggestion dropdown ----------

function openPanel() {
  if (items.length === 0) { closePanel(); return; }
  panel.hidden = false;
  searchZone.classList.add('open');
  input.setAttribute('aria-expanded', 'true');
}

function closePanel() {
  panel.hidden = true;
  searchZone.classList.remove('open');
  input.setAttribute('aria-expanded', 'false');
  input.removeAttribute('aria-activedescendant');
  selectedIndex = -1;
}

function renderItems(sections, typed) {
  list.innerHTML = '';
  items = [];
  selectedIndex = -1;

  sections.forEach((section) => {
    if (section.label && section.entries.length) {
      const label = document.createElement('li');
      label.className = 'sg-label';
      label.textContent = section.label;
      list.appendChild(label);
    }

    section.entries.forEach((entry) => {
      const idx = items.length;
      items.push(entry);

      const li = document.createElement('li');
      li.className = 'sg-item ' + entry.type;
      li.id = 'sg-' + idx;
      li.setAttribute('role', 'option');
      li.dataset.index = idx;

      const icon = entry.type === 'history' ? ICONS.clock
        : entry.type === 'trending' ? ICONS.trend
        : entry.type === 'fuzzy' ? ICONS.wand
        : ICONS.search;

      let side = '';
      if (entry.type === 'fuzzy') side = '<span class="sg-side">~fixed</span>';
      else if (entry.count) side = '<span class="sg-side">' + formatCount(entry.count) + '</span>';
      if (entry.type === 'history') side = '<button class="sg-remove" type="button">Remove</button>';

      li.innerHTML = icon + '<span class="sg-text">' + emphasize(entry.text, typed) + '</span>' + side;

      li.addEventListener('mousemove', () => setSelected(idx, { fillInput: false }));
      li.addEventListener('click', () => runSearch(entry.text));

      const removeBtn = li.querySelector('.sg-remove');
      if (removeBtn) {
        removeBtn.addEventListener('click', (e) => {
          e.stopPropagation();
          removeFromHistory(entry.text);
          input.focus();
          refreshSuggestions();
        });
      }

      list.appendChild(li);
    });
  });

  if (items.length) openPanel();
  else closePanel();
}

function setSelected(idx, { fillInput = true } = {}) {
  selectedIndex = idx;
  list.querySelectorAll('.sg-item').forEach((el) => {
    const isSel = Number(el.dataset.index) === idx;
    el.classList.toggle('selected', isSel);
    el.setAttribute('aria-selected', String(isSel));
    if (isSel) input.setAttribute('aria-activedescendant', el.id);
  });
  if (idx === -1) input.removeAttribute('aria-activedescendant');
  if (fillInput) input.value = idx === -1 ? typedText : items[idx].text;
}

/* Zero-state: recent searches + trending, shown for an empty focused box. */
async function showZeroState() {
  const hist = getHistory().slice(0, 6).map((h) => ({ text: h, type: 'history' }));
  const trending = (await getTrending()).slice(0, 5)
    .filter((t) => !hist.some((h) => h.text === t.query))
    .map((t) => ({ text: t.query, type: 'trending', count: t.score }));

  if (input.value.trim() !== '' || document.activeElement !== input) return; // stale
  renderItems([
    { label: hist.length ? 'Recent' : '', entries: hist },
    { label: 'Trending', entries: trending },
  ], '');
}

async function refreshSuggestions() {
  const raw = input.value;
  const q = normalize(raw);
  typedText = raw;

  if (!q) {
    showZeroState();
    return;
  }

  if (suggestAbort) suggestAbort.abort();
  suggestAbort = new AbortController();

  let data;
  try {
    const res = await fetch('/api/search?q=' + encodeURIComponent(q) + '&limit=' + SUGGEST_LIMIT, {
      signal: suggestAbort.signal,
    });
    data = await res.json();
  } catch (err) {
    if (err.name !== 'AbortError') console.error('suggest fetch failed:', err);
    return;
  }

  if (normalize(input.value) !== q) return; // stale response

  const server = data.suggestions || [];
  const histMatches = getHistory()
    .filter((h) => h.includes(q) && h !== q)
    .filter((h) => !server.some((s) => s.query === h))
    .slice(0, 2)
    .map((h) => ({ text: h, type: 'history' }));

  const serverEntries = server.map((s) => ({
    text: s.query,
    type: s.fuzzy ? 'fuzzy' : 'suggest',
    count: s.count,
  }));

  renderItems([{ entries: histMatches.concat(serverEntries) }], q);
}

// ---------- search execution ----------

function setView(view) {
  document.body.classList.toggle('home', view === 'home');
  document.body.classList.toggle('serp', view === 'serp');
}

async function runSearch(query, { push = true } = {}) {
  const q = normalize(query);
  if (!q) return;

  closePanel();
  input.value = q;
  typedText = q;
  clearBtn.hidden = false;
  setView('serp');
  document.title = q + ' — Googol';

  if (push) {
    const url = '?q=' + encodeURIComponent(q);
    if (location.search !== url) history.pushState({ q }, '', url);
  }

  addToHistory(q);

  // Feed the engine: executed searches become future suggestions.
  fetch('/api/search', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ query: q }),
  }).catch(() => {});

  didYouMean.hidden = true;
  resultsMeta.textContent = '';
  resultsList.innerHTML =
    '<li class="loading"><span class="dot"></span><span class="dot"></span><span class="dot"></span><span class="dot"></span></li>';

  let data;
  try {
    const res = await fetch('/api/query?q=' + encodeURIComponent(q));
    data = await res.json();
  } catch {
    resultsList.innerHTML = '<li class="no-results"><p>Could not reach the engine. Is the server running?</p></li>';
    return;
  }

  if (normalize(input.value) !== q) return; // user already searched again

  renderResults(q, data.results || [], data.elapsed_ms || 0);
  maybeShowDidYouMean(q);
}

function renderResults(query, results, elapsedMs) {
  resultsList.innerHTML = '';

  const secs = (elapsedMs / 1000).toFixed(3) + 's';

  if (results.length === 0) {
    resultsMeta.textContent = '0 hits · ' + secs;
    resultsList.innerHTML =
      '<li class="no-results">' +
      '<p>googol: no documents matched <b>' + escapeHtml(query) + '</b></p>' +
      '<p>try:</p>' +
      '<ul><li>checking the spelling of every word</li>' +
      '<li>different or more general keywords</li>' +
      '<li>feeding the index a new page from the <a href="console.html">engine console</a></li></ul>' +
      '</li>';
    return;
  }

  resultsMeta.textContent =
    results.length + ' hit' + (results.length === 1 ? '' : 's') + ' · ' + secs;

  const terms = normalize(query).split(' ').filter((t) => t.length >= 2);
  const palette = ['var(--indigo)', 'var(--vermilion)', 'var(--amber)', 'var(--green)'];

  results.forEach((match) => {
    let domain = match.url;
    let path = '';
    try {
      const u = new URL(match.url);
      domain = u.hostname;
      path = u.pathname.split('/').filter(Boolean).join(' › ');
    } catch { /* keep raw */ }

    const favColor = palette[(domain.charCodeAt(0) || 0) % palette.length];

    let snippet = escapeHtml(match.snippet);
    terms.forEach((t) => {
      snippet = snippet.replace(new RegExp('(' + escapeRegExp(escapeHtml(t)) + ')', 'gi'), '<b>$1</b>');
    });

    const li = document.createElement('li');
    li.className = 'result';
    li.innerHTML =
      '<div class="crumb">' +
        '<span class="favicon" style="--fav:' + favColor + '">' + escapeHtml(domain.charAt(0)) + '</span>' +
        '<span class="crumb-text">' +
          '<span class="site">' + escapeHtml(domain) + '</span>' +
          '<span class="url">' + escapeHtml(domain) + (path ? ' › ' + escapeHtml(path) : '') + '</span>' +
        '</span>' +
      '</div>' +
      '<h3><a href="' + escapeHtml(match.url) + '" target="_blank" rel="noopener">' + escapeHtml(match.title) + '</a></h3>' +
      '<p class="snippet">' + snippet + '</p>' +
      '<p class="scores">score=' + match.score.toFixed(3) +
        ' tfidf=' + match.tf_idf_score.toFixed(3) +
        ' pr=' + (match.pagerank_score * 100).toFixed(1) + '%</p>';

    resultsList.appendChild(li);
  });
}

/* If the engine's best guess for the full query is a typo correction,
   surface it Google-style. */
async function maybeShowDidYouMean(query) {
  try {
    const res = await fetch('/api/search?q=' + encodeURIComponent(query) + '&limit=3');
    const data = await res.json();
    const best = (data.suggestions || [])[0];
    if (normalize(input.value) !== query) return;
    if (best && best.fuzzy && best.query !== query) {
      didYouMean.innerHTML =
        '<span class="dym-label">did you mean:</span> <a href="?q=' + encodeURIComponent(best.query) + '">' +
        escapeHtml(best.query) + '</a>';
      didYouMean.hidden = false;
      didYouMean.querySelector('a').addEventListener('click', (e) => {
        e.preventDefault();
        runSearch(best.query);
      });
    }
  } catch { /* cosmetic only */ }
}

// ---------- navigation / view ----------

function goHome({ push = true } = {}) {
  setView('home');
  document.title = 'Googol — a tiny search engine';
  input.value = '';
  typedText = '';
  clearBtn.hidden = true;
  closePanel();
  resultsList.innerHTML = '';
  resultsMeta.textContent = '';
  didYouMean.hidden = true;
  if (push && location.search) history.pushState({}, '', location.pathname);
  renderTrendingChips();
}

function applyLocation() {
  const q = new URLSearchParams(location.search).get('q');
  if (q && q.trim()) runSearch(q, { push: false });
  else goHome({ push: false });
}

// ---------- events ----------

input.addEventListener('input', () => {
  clearBtn.hidden = input.value === '';
  clearTimeout(debounceTimer);
  debounceTimer = setTimeout(refreshSuggestions, DEBOUNCE_MS);
});

input.addEventListener('focus', () => {
  if (input.value.trim() === '') showZeroState();
  else refreshSuggestions();
});

input.addEventListener('keydown', (e) => {
  const open = !panel.hidden && items.length > 0;

  if (e.key === 'ArrowDown' && open) {
    e.preventDefault();
    setSelected(selectedIndex >= items.length - 1 ? -1 : selectedIndex + 1);
  } else if (e.key === 'ArrowUp' && open) {
    e.preventDefault();
    setSelected(selectedIndex <= -1 ? items.length - 1 : selectedIndex - 1);
  } else if (e.key === 'Escape') {
    if (open) {
      input.value = typedText;
      closePanel();
    }
  } else if (e.key === 'Enter') {
    e.preventDefault();
    runSearch(input.value);
  }
});

searchbox.addEventListener('submit', (e) => {
  e.preventDefault();
  runSearch(input.value);
});

clearBtn.addEventListener('click', () => {
  input.value = '';
  typedText = '';
  clearBtn.hidden = true;
  input.focus();
  showZeroState();
});

document.addEventListener('click', (e) => {
  if (!e.target.closest('#search-zone')) closePanel();
});

document.addEventListener('keydown', (e) => {
  if (e.key === '/' && document.activeElement !== input && !e.target.closest('input, textarea')) {
    e.preventDefault();
    input.focus();
  }
});

brandHome.addEventListener('click', (e) => {
  e.preventDefault();
  goHome();
});

btnSearch.addEventListener('click', () => runSearch(input.value));

btnLucky.addEventListener('click', async () => {
  const trending = await getTrending();
  if (trending.length) {
    runSearch(trending[Math.floor(Math.random() * trending.length)].query);
  } else {
    input.focus();
  }
});

window.addEventListener('popstate', applyLocation);

// ---------- boot ----------

applyLocation();
