// DOM Elements
const searchInput = document.getElementById('search-input');
const suggestionsDropdown = document.getElementById('suggestions-dropdown');
const clearBtn = document.getElementById('clear-btn');
const btnSearch = document.getElementById('btn-search');
const resultsList = document.getElementById('results-list');
const queryMetaCount = document.getElementById('query-meta-count');
const documentGraphRows = document.getElementById('document-graph-rows');

// Ingest Form Elements
const ingestForm = document.getElementById('ingest-form');
const ingestUrl = document.getElementById('ingest-url');
const ingestTitle = document.getElementById('ingest-title');
const ingestBody = document.getElementById('ingest-body');
const ingestLinks = document.getElementById('ingest-links');

// Telemetry Stats Elements
const statTotalQueries = document.getElementById('stat-total-queries');
const statTotalDocs = document.getElementById('stat-total-docs');
const statQueueSize = document.getElementById('stat-queue-size');
const statTrieNodes = document.getElementById('stat-trie-nodes');
const statDbSize = document.getElementById('stat-db-size');
const queueProgressBar = document.getElementById('queue-progress-bar');
const consoleTerminal = document.getElementById('console-terminal');

// Simulation Buttons
const btnSurge = document.getElementById('btn-surge');
const btnSingleSim = document.getElementById('btn-single-sim');

// State variables
let selectedIndex = -1;
let currentSuggestions = [];
let isSimulating = false;
let pollingInterval = null;

// Simulation Traffic Terms for surge queue testing
const simTerms = [
  "rust programming", "rust concurrency", "rust compiler", "tokio async runtime",
  "react js tutorial", "react hooks guide", "sqlite database", "trie prefix tree",
  "javascript async await", "how google pagerank works", "data structures",
  "rest api design", "concurrency vs parallelism", "sqlite wal mode"
];

// Initialize Workspace
document.addEventListener('DOMContentLoaded', () => {
  setupEventListeners();
  fetchStats();
  fetchDocumentsDirectory();
  
  // Poll system stats and logs every 1.5 seconds
  pollingInterval = setInterval(fetchStats, 1500);
});

// Setup DOM Listeners
function setupEventListeners() {
  // Autocomplete typing response
  searchInput.addEventListener('input', handleTyping);
  searchInput.addEventListener('keydown', handleKeyDown);

  // Clear search input
  clearBtn.addEventListener('click', () => {
    searchInput.value = '';
    clearBtn.classList.remove('active');
    hideDropdown();
    searchInput.focus();
  });

  // Close dropdown on click outside
  document.addEventListener('click', (e) => {
    if (!e.target.closest('.search-wrapper')) {
      hideDropdown();
    }
  });

  // Execute Search
  btnSearch.addEventListener('click', () => executeQuerySearch(searchInput.value));

  // Ingest crawled document form
  ingestForm.addEventListener('submit', handleDocIngestion);

  // Simulations
  btnSurge.addEventListener('click', triggerSurgeSimulation);
  btnSingleSim.addEventListener('click', triggerSingleSimulation);
}

// Tokenizes input and updates Trie prefix suggestions
async function handleTyping() {
  const query = searchInput.value;
  
  if (query.trim() === '') {
    clearBtn.classList.remove('active');
    hideDropdown();
    return;
  }
  
  clearBtn.classList.add('active');

  try {
    const response = await fetch(`/api/search?q=${encodeURIComponent(query)}`);
    const data = await response.json();
    currentSuggestions = data.suggestions || [];
    renderDropdown(query);
  } catch (err) {
    console.error('Error fetching suggestions:', err);
  }
}

// Render dropdown suggestions lists
function renderDropdown(typedText) {
  suggestionsDropdown.innerHTML = '';
  selectedIndex = -1;

  if (currentSuggestions.length === 0) {
    hideDropdown();
    return;
  }

  currentSuggestions.forEach((item, index) => {
    const div = document.createElement('div');
    div.classList.add('suggestion-item');
    div.dataset.index = index;

    // Highlight search match
    const text = item.query;
    const matchIdx = text.toLowerCase().indexOf(typedText.toLowerCase());
    let suggestionMarkup = '';
    
    if (matchIdx !== -1) {
      const before = text.substring(0, matchIdx);
      const matched = text.substring(matchIdx, matchIdx + typedText.length);
      const after = text.substring(matchIdx + typedText.length);
      suggestionMarkup = `${escapeHtml(before)}<span class="matched">${escapeHtml(matched)}</span>${escapeHtml(after)}`;
    } else {
      suggestionMarkup = escapeHtml(text);
    }

    div.innerHTML = `
      <span class="suggestion-text">${suggestionMarkup}</span>
      <span class="suggestion-count">${formatCount(item.count)} suggestions</span>
    `;

    div.addEventListener('click', () => {
      searchInput.value = item.query;
      hideDropdown();
      executeQuerySearch(item.query);
    });

    suggestionsDropdown.appendChild(div);
  });

  suggestionsDropdown.classList.add('active');
}

function hideDropdown() {
  suggestionsDropdown.classList.remove('active');
  selectedIndex = -1;
}

// Handle Keyboard navigation in suggestions list
function handleKeyDown(e) {
  if (!suggestionsDropdown.classList.contains('active')) return;

  const items = suggestionsDropdown.querySelectorAll('.suggestion-item');
  if (items.length === 0) return;

  if (e.key === 'ArrowDown') {
    e.preventDefault();
    selectedIndex = (selectedIndex + 1) % items.length;
    updateSelectedSuggestion(items);
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    selectedIndex = (selectedIndex - 1 + items.length) % items.length;
    updateSelectedSuggestion(items);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (selectedIndex >= 0 && selectedIndex < currentSuggestions.length) {
      const selectedQuery = currentSuggestions[selectedIndex].query;
      searchInput.value = selectedQuery;
      hideDropdown();
      executeQuerySearch(selectedQuery);
    } else {
      hideDropdown();
      executeQuerySearch(searchInput.value);
    }
  } else if (e.key === 'Escape') {
    hideDropdown();
  }
}

function updateSelectedSuggestion(items) {
  items.forEach((item, idx) => {
    if (idx === selectedIndex) {
      item.classList.add('selected');
      item.scrollIntoView({ block: 'nearest' });
    } else {
      item.classList.remove('selected');
    }
  });
}

// Execute Query Search (Calculates TF-IDF + PageRank, also logs search phrase count to queue)
async function executeQuerySearch(query) {
  if (!query || query.trim() === '') return;
  
  const q = query.trim();
  hideDropdown();

  // 1. Send search term logging POST (asynchronous queue write)
  try {
    fetch('/api/search', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ query: q })
    });
  } catch (err) {}

  // 2. Fetch TF-IDF matches from backend database
  queryMetaCount.textContent = 'Executing query ranking...';
  resultsList.innerHTML = '<div style="text-align: center; padding: 2rem; color: var(--text-secondary);"><i class="fa-solid fa-spinner fa-spin"></i> Scoring index matches...</div>';

  try {
    const response = await fetch(`/api/query?q=${encodeURIComponent(q)}`);
    const data = await response.json();
    renderQueryResults(q, data.results || []);
  } catch (err) {
    console.error('Error executing query search:', err);
    resultsList.innerHTML = `<div style="color: var(--warning); padding: 1rem;">Failed to fetch query scores.</div>`;
  }
}

// Render search result scorecards
function renderQueryResults(query, results) {
  resultsList.innerHTML = '';
  
  if (results.length === 0) {
    queryMetaCount.textContent = `0 matching pages found in inverted index.`;
    resultsList.innerHTML = `
      <div style="text-align: center; padding: 2.5rem 1rem; color: var(--text-secondary); border: 1px dashed var(--border-color); border-radius: 10px;">
        <i class="fa-solid fa-circle-exclamation" style="font-size: 1.5rem; margin-bottom: 0.5rem; color: var(--warning);"></i>
        <p>No document matched your query keywords.</p>
        <p style="font-size: 0.75rem; margin-top: 0.25rem;">Try searching terms like "rust", "concurrency", "async", "sqlite", or "trie".</p>
      </div>
    `;
    return;
  }

  queryMetaCount.textContent = `Found ${results.length} ranked matches in inverted postings:`;

  results.forEach((match) => {
    const itemDiv = document.createElement('div');
    itemDiv.classList.add('result-item');

    // Convert PageRank fraction to a percentage
    const pagerankPercent = (match.pagerank_score * 100).toFixed(2) + '%';

    itemDiv.innerHTML = `
      <div class="result-url">${escapeHtml(match.url)}</div>
      <div class="result-title">${escapeHtml(match.title)}</div>
      <div class="score-badge-row">
        <span class="score-badge badge-total" title="Score = TF-IDF * (1.0 + 10.0 * PageRank)">Score: ${match.score.toFixed(4)}</span>
        <span class="score-badge" title="Sum of word weights across matching postings">TF-IDF: ${match.tf_idf_score.toFixed(4)}</span>
        <span class="score-badge" title="Document node weight in PageRank link graph">PageRank: ${pagerankPercent}</span>
      </div>
      <p class="result-snippet">${match.snippet}</p>
    `;
    resultsList.appendChild(itemDiv);
  });
}

// Ingest Crawler documents (Submit mock crawling links)
async function handleDocIngestion(e) {
  e.preventDefault();

  const url = ingestUrl.value.trim();
  const title = ingestTitle.value.trim();
  const body = ingestBody.value.trim();
  
  // Parse links comma-separated list
  const linkList = ingestLinks.value.split(',')
    .map(link => link.trim())
    .filter(link => link.length > 0);

  const submitBtn = document.getElementById('btn-ingest-submit');
  submitBtn.disabled = true;
  submitBtn.innerHTML = '<i class="fa-solid fa-spinner fa-spin"></i> Indexing Page & Solving PageRank...';

  try {
    const response = await fetch('/api/documents', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ url, title, body, links: linkList })
    });

    if (response.ok) {
      // Clear forms
      ingestUrl.value = '';
      ingestTitle.value = '';
      ingestBody.value = '';
      ingestLinks.value = '';
      
      // Refresh directory and stats instantly
      fetchStats();
      fetchDocumentsDirectory();
    } else {
      const errData = await response.json();
      alert(`Crawler error: ${errData.error || 'Unknown error'}`);
    }
  } catch (err) {
    console.error('Error crawling doc:', err);
    alert('Failed to connect to crawling endpoint.');
  } finally {
    submitBtn.disabled = false;
    submitBtn.innerHTML = '<i class="fa-solid fa-arrow-up-from-bracket"></i> Crawl &amp; Index Document';
  }
}

// Fetch PageRank Document Directory table
async function fetchDocumentsDirectory() {
  try {
    const response = await fetch('/api/documents');
    if (!response.ok) return;
    const docs = await response.json();
    renderDocumentsDirectory(docs);
  } catch (err) {
    console.error('Error fetching documents directory:', err);
  }
}

function renderDocumentsDirectory(docs) {
  documentGraphRows.innerHTML = '';
  
  if (docs.length === 0) {
    documentGraphRows.innerHTML = '<tr><td colspan="2" style="text-align: center; color: var(--text-secondary);">No documents indexed.</td></tr>';
    return;
  }

  docs.forEach((doc) => {
    const tr = document.createElement('tr');
    const pagerankPercent = (doc.pagerank * 100).toFixed(2) + '%';

    tr.innerHTML = `
      <td>
        <div class="graph-cell-title">${escapeHtml(doc.title)}</div>
        <div class="graph-cell-url">${escapeHtml(doc.url)}</div>
      </td>
      <td class="graph-cell-pr">${pagerankPercent}</td>
    `;
    documentGraphRows.appendChild(tr);
  });
}

// Fetch System Telemetry & Logs
async function fetchStats() {
  try {
    const response = await fetch('/api/stats');
    if (!response.ok) return;
    const data = await response.json();

    // Update Telemetries
    statTotalQueries.textContent = data.total_queries.toLocaleString();
    statTotalDocs.textContent = data.total_indexed_documents.toLocaleString();
    statQueueSize.textContent = data.queue_size;
    statTrieNodes.textContent = data.active_trie_nodes.toLocaleString();
    if (statDbSize) statDbSize.textContent = formatBytes(data.database_size_bytes);

    // Queue Gauge (max 50 before flush)
    const queuePercentage = Math.min(100, (data.queue_size / 50) * 100);
    queueProgressBar.style.width = `${queuePercentage}%`;

    // Render console logs
    renderLogs(data.recent_logs || []);
    
    // Also pull graph list periodically to show PageRank shifts on traffic updates
    fetchDocumentsDirectory();
  } catch (err) {
    console.error('Error polling stats:', err);
  }
}

let lastLogFirstLine = '';
function renderLogs(logs) {
  if (logs.length === 0 || logs[0] === lastLogFirstLine) return;
  lastLogFirstLine = logs[0];

  consoleTerminal.innerHTML = '';
  logs.forEach(log => {
    const div = document.createElement('div');
    div.classList.add('console-line');

    const timestampMatch = log.match(/^\[([0-9:]+)\]\s(.*)$/);
    if (timestampMatch) {
      const ts = timestampMatch[1];
      const rest = timestampMatch[2];

      let msgClass = 'msg';
      if (rest.includes('Flushing') || rest.includes('rebuilt') || rest.includes('indexed') || rest.includes('PageRank')) {
        msgClass = 'event';
      }

      div.innerHTML = `
        <span class="timestamp">[${ts}]</span>
        <span class="${msgClass}">${escapeHtml(rest)}</span>
      `;
    } else {
      div.textContent = log;
    }
    
    consoleTerminal.appendChild(div);
  });
}

// Trigger single random query log
function triggerSingleSimulation() {
  const term = simTerms[Math.floor(Math.random() * simTerms.length)];
  try {
    fetch('/api/search', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ query: term })
    });
  } catch (err) {}
}

// Trigger 200 concurrent queries surge simulation
async function triggerSurgeSimulation() {
  if (isSimulating) return;
  isSimulating = true;

  btnSurge.disabled = true;
  btnSurge.innerHTML = '<i class="fa-solid fa-spinner fa-spin"></i> Ingesting Surge...';

  // Increase polling frequency for live visual updates
  clearInterval(pollingInterval);
  pollingInterval = setInterval(fetchStats, 200);

  const numRequests = 200;
  for (let i = 0; i < numRequests; i++) {
    const term = simTerms[Math.floor(Math.random() * simTerms.length)];
    try {
      fetch('/api/search', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ query: term })
      });
    } catch (err) {}
    await sleep(6);
  }

  setTimeout(() => {
    btnSurge.disabled = false;
    btnSurge.innerHTML = '<i class="fa-solid fa-network-wired"></i> Traffic Surge (200 Queries)';
    isSimulating = false;
    
    // Slow polling back down
    clearInterval(pollingInterval);
    pollingInterval = setInterval(fetchStats, 1500);
  }, 3000);
}

// Utilities
function escapeHtml(str) {
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

function formatCount(num) {
  if (num >= 1000000) {
    return (num / 1000000).toFixed(1).replace(/\.0$/, '') + 'M';
  }
  if (num >= 1000) {
    return (num / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
  }
  return num.toString();
}

function formatBytes(bytes) {
  if (bytes === 0) return '0 Bytes';
  const k = 1024;
  const dm = 1;
  const sizes = ['Bytes', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + ' ' + sizes[i];
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}
