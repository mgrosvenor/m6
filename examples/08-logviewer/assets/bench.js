(function () {
  'use strict';

  // ── Paths under test ───────────────────────────────────────────────────────
  //
  //  cache-hit HTML   /                   m6-html → cached by m6-http
  //  cache-miss HTML  /?_nocache          same route, ?_nocache bypasses m6-http cache
  //  cache-hit file   style.css           m6-file static → cached by m6-http
  //  cache-miss file  style.css?_nocache  same file, ?_nocache bypasses m6-http cache
  //
  //  ?_nocache skips both the cache read and the cache write in m6-http,
  //  so the request always hits the backend.  Same resource, fair comparison.

  const SUITES = [
    { label: 'HTML cache-hit  (/)',          url: '/',                          warmup: 5 },
    { label: 'HTML cache-miss (/)',           url: '/?_nocache',                 warmup: 3 },
    { label: 'File cache-hit  (style.css)',  url: '/assets/style.css',          warmup: 5 },
    { label: 'File cache-miss (style.css)',  url: '/assets/style.css?_nocache', warmup: 3 },
  ];

  const COLORS = ['#2a9d8f', '#e76f51', '#457b9d', '#c77dff'];

  const runBtn      = document.getElementById('bench-run');
  const stopBtn     = document.getElementById('bench-stop');
  const exportBtn   = document.getElementById('bench-export');
  const nInp        = document.getElementById('bench-n');
  const statusEl    = document.getElementById('bench-status');
  const printHeader = document.getElementById('print-header');
  const cdfsEl      = document.getElementById('bench-cdfs');
  const zoomOverlay = document.getElementById('bench-zoom-overlay');
  const tbl         = document.getElementById('bench-tbl');
  const tbody       = document.getElementById('bench-tbody');

  let abortCtl = null;   // AbortController for in-flight fetches

  stopBtn.addEventListener('click', function () {
    if (abortCtl) abortCtl.abort();
  });

  exportBtn.addEventListener('click', function () {
    printHeader.textContent =
      'm6-http benchmark  —  ' + new Date().toLocaleString() +
      '  —  ' + location.host;
    window.print();
  });

  // ── Zoom overlay ───────────────────────────────────────────────────────────

  zoomOverlay.addEventListener('click', closeZoom);
  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') closeZoom(); });

  function closeZoom() {
    zoomOverlay.classList.remove('active');
    zoomOverlay.innerHTML = '';
  }

  function openZoom(svg) {
    zoomOverlay.innerHTML = '';
    zoomOverlay.appendChild(svg.cloneNode(true));
    zoomOverlay.classList.add('active');
  }

  // ── Statistics ─────────────────────────────────────────────────────────────

  function pct(sorted, p) {
    return sorted[Math.min(Math.floor(p / 100 * sorted.length), sorted.length - 1)];
  }

  function calcStats(sorted) {
    const avg = sorted.reduce((a, b) => a + b, 0) / sorted.length;
    return { min: sorted[0], p50: pct(sorted, 50), p99: pct(sorted, 99), max: sorted[sorted.length - 1], avg };
  }

  // ── Protocol detection via Resource Timing API ────────────────────────────

  // Increase the buffer so it doesn't overflow during a full benchmark run
  // (default 150 entries is exhausted after the first suite).
  performance.setResourceTimingBufferSize(5000);

  function getProto(url) {
    // nextHopProtocol is empty for ?_nocache URLs in some browsers because
    // the entry is created before the response is finalised.  Protocol is a
    // property of the connection, not the URL, so fall back to '/' which
    // always has a populated entry after suite 1.
    const lookup = u => {
      const abs = new URL(u, location.href).href;
      const e = performance.getEntriesByName(abs, 'resource');
      return e.length ? (e[e.length - 1].nextHopProtocol || '') : '';
    };
    return lookup(url) || lookup('/') || '?';
  }

  // ── Timer resolution detection ─────────────────────────────────────────────
  //
  // performance.now() is quantised by the browser (typically to 1 ms in
  // non-cross-origin-isolated pages).  We detect the effective step size by
  // spinning until the clock advances and recording the smallest gap seen.

  function detectTimerResolutionUs() {
    const gaps = [];
    let prev = performance.now();
    for (let i = 0; i < 10000 && gaps.length < 30; i++) {
      const curr = performance.now();
      if (curr > prev) { gaps.push((curr - prev) * 1000); prev = curr; }
    }
    return gaps.length ? Math.min(...gaps) : 1000; // fallback: 1 ms
  }

  // ── Timed fetch (bypasses browser cache; server cache unaffected) ──────────

  async function timedFetch(url, signal) {
    const t0 = performance.now();
    const r  = await fetch(url, { cache: 'no-store', signal });
    await r.arrayBuffer();
    return (performance.now() - t0) * 1000; // µs, corrected by caller
  }

  // ── Run one suite ──────────────────────────────────────────────────────────

  async function runSuite(suite, n, signal, resUs) {
    for (let i = 0; i < suite.warmup; i++) await timedFetch(suite.url, signal);

    const raw = [];
    for (let i = 0; i < n; i++) {
      raw.push(await timedFetch(suite.url, signal));
      if (i % 25 === 0) {
        statusEl.textContent = `Measuring: ${suite.label} — ${i + 1}/${n}…`;
        await new Promise(r => setTimeout(r, 0));
      }
    }

    // Weighted midpoint correction.
    //
    // Naive midpoint adds R/2 to every sample, assuming uniform density within
    // each quantisation bucket.  Latency skews toward zero, so the 0 ms bucket's
    // true centroid is closer to its lower edge.
    //
    // Assuming the density is linear within each bucket, interpolated from the
    // bucket counts of this and the next bucket:
    //
    //   E[true | measured = v] = v + R × (n_v + 2·n_{v+R}) / (3·(n_v + n_{v+R}))
    //
    // Derivation: for linear density ρ(x) = a + (b−a)·x/R on [0,R],
    //   E[X] = R·(a + 2b) / (3·(a+b)),  where a ∝ n_v, b ∝ n_{v+R}.
    //
    // Edge cases:
    //   n_{v+R} = 0  →  correction = R/3  (all density at left)
    //   n_v = n_{v+R} →  correction = R/2  (flat: reduces to naive midpoint)
    const freq = new Map();
    for (const v of raw) {
      const b = Math.round(v / resUs) * resUs;
      freq.set(b, (freq.get(b) || 0) + 1);
    }
    const lats = raw.map(v => {
      const b  = Math.round(v / resUs) * resUs;
      const n0 = freq.get(b)         || 0;
      const n1 = freq.get(b + resUs) || 0;
      const corr = (n0 + n1) > 0
        ? resUs * (n0 + 2 * n1) / (3 * (n0 + n1))
        : resUs / 2;
      return b + corr;
    });

    const sorted = lats.slice().sort((a, b) => a - b);
    return { label: suite.label, proto: getProto(suite.url), sorted, n, ...calcStats(sorted) };
  }

  // ── SVG helpers ────────────────────────────────────────────────────────────

  const SVG_NS = 'http://www.w3.org/2000/svg';

  function svgNode(tag, attrs, text) {
    const el = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
    if (text != null) el.textContent = text;
    return el;
  }

  function fmtMs(v) {
    // v is in µs; display in ms to 2 d.p.
    return (v / 1000).toFixed(2) + 'ms';
  }

  // ── CDF chart for one suite ────────────────────────────────────────────────

  function renderOneCDF(result, color) {
    const W   = 380;
    const PAD = { top: 26, right: 16, bottom: 50, left: 56 };
    const H   = 220;
    const cW  = W - PAD.left - PAD.right;
    const cH  = H - PAD.top - PAD.bottom;

    const { sorted, min, p50, p99, max, proto } = result;

    // X axis: log10 scale.  Clip at p99.5 so extreme outliers don't compress
    // the interesting region.  Guard against zero/negative values.
    const xClip   = Math.max(pct(sorted, 99.5) * 1.08, 1);
    const lg      = v => Math.log10(Math.max(v, 0.1));
    const lgMin   = lg(Math.max(sorted[0], 0.1));
    const lgMax   = lg(xClip);
    const lgRange = lgMax - lgMin || 1;
    const sx      = v => Math.min(Math.max((lg(v) - lgMin) / lgRange, 0), 1) * cW;
    const sy      = p => cH - (p / 100) * cH; // p ∈ [0,100]

    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('viewBox', `0 0 ${W} ${H}`);
    svg.setAttribute('width', W);
    svg.setAttribute('height', H);

    // Background
    svg.appendChild(svgNode('rect', { x: 0, y: 0, width: W, height: H, fill: '#fafafa', rx: 4 }));

    // Horizontal grid lines + Y-axis labels
    for (let p = 0; p <= 100; p += 25) {
      const y = PAD.top + sy(p);
      svg.appendChild(svgNode('line', { x1: PAD.left, y1: y, x2: PAD.left + cW, y2: y, stroke: '#e4e4e4', 'stroke-width': 1 }));
      svg.appendChild(svgNode('text', { x: PAD.left - 5, y: y + 4, 'text-anchor': 'end', 'font-size': 9, fill: '#999' }, p + '%'));
    }

    // Vertical grid lines + X-axis labels (one tick per decade: 1, 10, 100, …)
    for (let p = Math.floor(lgMin); p <= Math.ceil(lgMax); p++) {
      const v = Math.pow(10, p);
      if (lg(v) < lgMin - 0.01 || lg(v) > lgMax + 0.01) continue;
      const x = PAD.left + sx(v);
      svg.appendChild(svgNode('line', { x1: x, y1: PAD.top, x2: x, y2: PAD.top + cH, stroke: '#e4e4e4', 'stroke-width': 1 }));
      svg.appendChild(svgNode('text', { x, y: PAD.top + cH + 13, 'text-anchor': 'middle', 'font-size': 9, fill: '#999' }, fmtMs(v)));
    }

    // CDF polyline
    const pts = sorted.map((v, i) => {
      const x = PAD.left + sx(v);
      const y = PAD.top  + sy((i + 1) / sorted.length * 100);
      return x + ',' + y;
    }).join(' ');
    svg.appendChild(svgNode('polyline', { points: pts, fill: 'none', stroke: color, 'stroke-width': 2, 'stroke-linejoin': 'round' }));

    // Annotation dashes for min, p50, p99, max
    const marks = [
      { v: min, label: 'min' },
      { v: p50, label: 'p50' },
      { v: p99, label: 'p99' },
      { v: max, label: 'max' },
    ];
    for (const m of marks) {
      if (m.v > xClip) continue;
      const x = PAD.left + sx(m.v);
      svg.appendChild(svgNode('line', {
        x1: x, y1: PAD.top, x2: x, y2: PAD.top + cH,
        stroke: color, 'stroke-width': 1, 'stroke-dasharray': '3,3', opacity: 0.65,
      }));
      svg.appendChild(svgNode('text', { x, y: PAD.top + cH + 27, 'text-anchor': 'middle', 'font-size': 9, fill: color, 'font-weight': '600' }, m.label));
      svg.appendChild(svgNode('text', { x, y: PAD.top + cH + 38, 'text-anchor': 'middle', 'font-size': 8, fill: '#666' }, fmtMs(m.v)));
    }

    // Title + protocol badge
    const pc = proto.includes('3') ? '#0a7' : proto.includes('2') ? '#059' : '#a60';
    svg.appendChild(svgNode('text', { x: PAD.left, y: 17, 'font-size': 11, fill: '#222', 'font-weight': '600' }, result.label));
    svg.appendChild(svgNode('text', { x: W - PAD.right, y: 17, 'text-anchor': 'end', 'font-size': 10, fill: pc }, proto));

    return svg;
  }

  // ── Render all CDFs ────────────────────────────────────────────────────────

  function renderCDFs(results) {
    cdfsEl.innerHTML = '';
    for (let i = 0; i < results.length; i++) {
      const svg = renderOneCDF(results[i], COLORS[i % COLORS.length]);
      svg.addEventListener('click', function () { openZoom(svg); });
      cdfsEl.appendChild(svg);
    }
    cdfsEl.style.display = 'grid';
  }

  // ── Results table ──────────────────────────────────────────────────────────

  function renderTable(results) {
    tbody.innerHTML = '';
    for (const r of results) {
      const pc = r.proto.includes('3') ? 'ph3' : r.proto.includes('2') ? 'ph2' : 'ph1';
      const tr = document.createElement('tr');
      tr.innerHTML =
        `<td>${r.label}</td>` +
        `<td class="${pc}">${r.proto}</td>` +
        `<td>${fmtMs(r.p50)}</td>` +
        `<td>${fmtMs(r.p99)}</td>` +
        `<td>${fmtMs(r.min)}</td>` +
        `<td>${fmtMs(r.max)}</td>` +
        `<td>${fmtMs(r.avg)}</td>` +
        `<td>${r.n}</td>`;
      tbody.appendChild(tr);
    }
    tbl.style.display = 'table';
  }

  // ── Main ───────────────────────────────────────────────────────────────────

  runBtn.addEventListener('click', async function () {
    runBtn.disabled = true;
    stopBtn.disabled = false;
    exportBtn.disabled = true;
    cdfsEl.style.display = 'none';
    tbl.style.display = 'none';
    tbody.innerHTML = '';

    abortCtl = new AbortController();
    const { signal } = abortCtl;

    const n = Math.max(20, parseInt(nInp.value, 10) || 200);
    const results = [];

    statusEl.textContent = 'Detecting timer resolution…';
    await new Promise(r => setTimeout(r, 0));
    const resUs = detectTimerResolutionUs();
    const resNote = `timer resolution ${fmtMs(resUs)}, midpoint-corrected`;

    try {
      for (const suite of SUITES) {
        statusEl.textContent = `Warming up: ${suite.label}…`;
        await new Promise(r => setTimeout(r, 0));
        const r = await runSuite(suite, n, signal, resUs);
        results.push(r);
        statusEl.textContent =
          `✓ ${suite.label}  p50=${fmtMs(r.p50)}  p99=${fmtMs(r.p99)}  [${r.proto}]`;
        await new Promise(r => setTimeout(r, 0));
      }

      const protos = [...new Set(results.map(r => r.proto))].join(', ');
      statusEl.textContent =
        `Complete — protocol: ${protos}  (${resNote}). ` +
        `Clear Alt-Svc in about:networking and reload to benchmark H2.`;
      renderCDFs(results);
      renderTable(results);
      exportBtn.disabled = false;
    } catch (e) {
      if (e.name === 'AbortError') {
        statusEl.textContent = results.length
          ? `Stopped after ${results.length}/${SUITES.length} suite(s).`
          : 'Stopped.';
        if (results.length) { renderCDFs(results); renderTable(results); exportBtn.disabled = false; }
      } else {
        statusEl.textContent = 'Error: ' + e.message;
      }
    } finally {
      abortCtl = null;
      runBtn.disabled = false;
      stopBtn.disabled = true;
    }
  });
})();
