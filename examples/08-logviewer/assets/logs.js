(function () {
  'use strict';

  const processSelect = document.getElementById('lv-process');
  const autoscrollBox = document.getElementById('lv-autoscroll');
  const clearBtn      = document.getElementById('lv-clear');
  const tbody         = document.getElementById('lv-body');
  const status        = document.getElementById('lv-status');

  // Per-process byte offsets. Reset when process selection changes.
  const offsets = {};

  // Number of lines to request on first load (tail -n behaviour).
  const TAIL_LINES = 200;

  // Maximum rows to keep in the table; oldest are trimmed from the top.
  const MAX_ROWS = 1000;

  let pollTimer  = null;
  let errorCount = 0;   // consecutive network errors; cleared on success
  let polling    = false;  // prevents overlapping polls

  // After this many consecutive NetworkErrors the page reloads.  Firefox
  // caches a dead H2 connection after a server restart and will not retry
  // on its own; a reload is the only way to clear that state.
  const RELOAD_AFTER_ERRORS = 15;

  function currentProcess() {
    return processSelect.value;
  }

  function setStatus(msg) {
    status.textContent = msg;
  }

  function escapeHtml(s) {
    return s
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function levelClass(level) {
    switch ((level || '').toUpperCase()) {
      case 'ERROR': return 'lv-error';
      case 'WARN':  return 'lv-warn';
      case 'DEBUG': return 'lv-debug';
      case 'TRACE': return 'lv-trace';
      default:      return 'lv-info';
    }
  }

  function appendEntry(proc, entry) {
    // tracing-subscriber JSON format:
    // {"timestamp":"…","level":"INFO","fields":{"message":"…"},"target":"…"}
    const ts     = entry.timestamp ? entry.timestamp.replace('T', ' ').replace(/\.\d+Z$/, 'Z') : '—';
    const level  = entry.level || '?';
    const target = entry.target || proc;
    const msg    = (entry.fields && entry.fields.message) ? entry.fields.message : JSON.stringify(entry.fields || entry);

    // Extra fields (everything except message).
    const extras = [];
    if (entry.fields) {
      for (const [k, v] of Object.entries(entry.fields)) {
        if (k !== 'message') extras.push(k + '=' + JSON.stringify(v));
      }
    }
    const fullMsg = extras.length ? msg + '  ' + extras.join(' ') : msg;

    const tr = document.createElement('tr');
    tr.className = levelClass(level);
    tr.innerHTML =
      '<td class="col-time">'   + escapeHtml(ts)      + '</td>' +
      '<td class="col-level">'  + escapeHtml(level)   + '</td>' +
      '<td class="col-target">' + escapeHtml(target)  + '</td>' +
      '<td class="col-msg">'    + escapeHtml(fullMsg) + '</td>';
    tbody.appendChild(tr);
  }

  async function poll() {
    if (polling) return;   // previous fetch still in flight; skip this tick
    polling = true;

    const proc   = currentProcess();
    const offset = offsets[proc] || 0;
    const t0     = performance.now();
    console.debug('[logviewer] poll', proc, 'offset=' + offset);

    let resp;
    try {
      const nParam = offset === 0 ? '&n=' + TAIL_LINES : '';
      resp = await fetch('/logs/tail/' + proc + '.log?offset=' + offset + nParam);
    } catch (e) {
      const elapsed = (performance.now() - t0).toFixed(0);
      console.warn('[logviewer] NetworkError after ' + elapsed + 'ms (' + proc + ')', e);
      errorCount++;
      if (errorCount >= RELOAD_AFTER_ERRORS) {
        // Firefox has cached the dead connection and will not reconnect on its
        // own (e.g. after a server restart).  Reload to clear its state.
        console.warn('[logviewer] too many errors, reloading page');
        window.location.reload();
        return;
      }
      if (errorCount <= 2) {
        setStatus('Reconnecting…');
      } else {
        setStatus('Network error (' + errorCount + '): ' + e.message);
      }
      polling = false;
      schedulePoll(250);
      return;
    }

    const elapsed = (performance.now() - t0).toFixed(0);
    console.debug('[logviewer] response', proc, resp.status, elapsed + 'ms',
                  'X-Log-End=' + resp.headers.get('X-Log-End'));

    if (!resp.ok) {
      errorCount++;
      console.warn('[logviewer] HTTP error', resp.status, proc);
      setStatus(proc + '.log — HTTP ' + resp.status);
      polling = false;
      return;
    }

    errorCount = 0;

    const newOffset = resp.headers.get('X-Log-End');
    if (newOffset !== null) offsets[proc] = parseInt(newOffset, 10);

    const text = await resp.text();
    if (!text.trim()) {
      setStatus('Watching ' + proc + '.log — no new entries');
      polling = false;
      return;
    }

    let count = 0;
    for (const line of text.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        const entry = JSON.parse(trimmed);
        appendEntry(proc, entry);
        count++;
      } catch (_) {
        // Non-JSON line (e.g. text format); show as plain row.
        const tr = document.createElement('tr');
        tr.className = 'lv-plain';
        tr.innerHTML =
          '<td class="col-time">—</td>' +
          '<td class="col-level">—</td>' +
          '<td class="col-target">—</td>' +
          '<td class="col-msg">' + escapeHtml(trimmed) + '</td>';
        tbody.appendChild(tr);
        count++;
      }
    }

    if (count > 0) {
      const excess = tbody.children.length - MAX_ROWS;
      for (let i = 0; i < excess; i++) tbody.removeChild(tbody.firstElementChild);
      if (autoscrollBox.checked) {
        tbody.lastElementChild && tbody.lastElementChild.scrollIntoView({ behavior: 'smooth' });
      }
    }
    setStatus('Watching ' + proc + '.log — ' + (offsets[proc] || 0) + ' bytes read');
    polling = false;
  }

  function schedulePoll(delayMs) {
    if (pollTimer) clearTimeout(pollTimer);
    pollTimer = setTimeout(async function () {
      pollTimer = null;        // clear so we can tell if poll() re-schedules
      await poll();
      if (!pollTimer) {        // poll() didn't re-schedule (no error path taken)
        schedulePoll(1000);
      }
    }, delayMs);
  }

  function startPolling() {
    if (pollTimer) clearTimeout(pollTimer);
    polling    = false;
    errorCount = 0;
    schedulePoll(0);
  }

  processSelect.addEventListener('change', function () {
    console.info('[logviewer] switched to', currentProcess());
    tbody.innerHTML = '';
    setStatus('Switching to ' + currentProcess() + '.log…');
    startPolling();
  });

  clearBtn.addEventListener('click', function () {
    tbody.innerHTML = '';
  });

  startPolling();
})();
