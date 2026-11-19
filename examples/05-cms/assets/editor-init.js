/* CMS editor initialisation — loaded as a static file to avoid HTML-minifier mangling */

const stemEl      = document.getElementById('stem');
const titleEl     = document.getElementById('title');
const summaryEl   = document.getElementById('summary');
const dateEl      = document.getElementById('date');
const coverEl     = document.getElementById('cover');
const coverImg    = document.getElementById('cover-preview');
const coverPh     = document.getElementById('cover-ph');
const statusEl    = document.getElementById('status');
const tagWidget   = document.getElementById('tag-widget');
const tagInput    = document.getElementById('tag-input');

// ── Cover preview ──────────────────────────────────────────────────────────────
function updateCoverPreview() {
  var url = coverEl.value.trim();
  if (url) {
    coverImg.src = url;
    coverImg.style.display = 'block';
    coverPh.style.display  = 'none';
  } else {
    coverImg.style.display = 'none';
    coverPh.style.display  = '';
  }
}
coverImg.onerror = function() {
  coverImg.style.display = 'none';
  coverPh.style.display  = '';
};
coverEl.addEventListener('input', updateCoverPreview);
updateCoverPreview();

// ── Tag widget ────────────────────────────────────────────────────────────────
var tags = [];

function renderTags() {
  tagWidget.querySelectorAll('.tag-chip').forEach(c => c.remove());
  tags.forEach((tag, i) => {
    const chip = document.createElement('span');
    chip.className = 'tag-chip';
    chip.innerHTML = tag + '<button type="button" aria-label="Remove" onclick="removeTag(' + i + ')">×</button>';
    tagWidget.insertBefore(chip, tagInput);
  });
}

function addTag(raw) {
  raw.split(',').map(s => s.trim().toLowerCase()).filter(Boolean).forEach(t => {
    if (!tags.includes(t)) tags.push(t);
  });
  tagInput.value = '';
  renderTags();
}

function removeTag(i) {
  tags.splice(i, 1);
  renderTags();
  tagInput.focus();
}

tagInput.addEventListener('keydown', function(e) {
  if (e.key === 'Enter' || e.key === ',' || e.key === 'Tab') {
    e.preventDefault();
    if (tagInput.value.trim()) addTag(tagInput.value);
  } else if (e.key === 'Backspace' && !tagInput.value && tags.length) {
    removeTag(tags.length - 1);
  }
});
tagInput.addEventListener('blur', function() {
  if (tagInput.value.trim()) addTag(tagInput.value);
});

// ── Frontmatter parser ────────────────────────────────────────────────────────
function parseFrontmatter(text) {
  var m = text.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)/);
  if (!m) return { meta: {}, body: text };
  var meta = {};
  var lines = m[1].split('\n');
  for (var li = 0; li < lines.length; li++) {
    var line = lines[li];
    var ci = line.indexOf(':');
    if (ci < 0) continue;
    var key = line.slice(0, ci).trim();
    var val = line.slice(ci + 1).trim();
    if (val.charAt(0) === '[' && val.charAt(val.length - 1) === ']') {
      val = val.slice(1, -1).split(',').map(function(s) {
        return s.trim().replace(/^['"]|['"]$/g, '');
      }).join(', ');
    } else {
      val = val.replace(/^['"]|['"]$/g, '');
    }
    meta[key] = val;
  }
  return { meta: meta, body: m[2] };
}

// ── Init ──────────────────────────────────────────────────────────────────────
var rawBody  = atob(document.getElementById('body-b64').value || '');
var initTags = atob(document.getElementById('tags-b64').value || '');
var parsed   = parseFrontmatter(rawBody);
var meta     = parsed.meta;
var cleanBody = parsed.body;

if (!titleEl.value   && meta.title)   titleEl.value   = meta.title;
if (!summaryEl.value && meta.summary) summaryEl.value = meta.summary;
if (!dateEl.value    && meta.date)    dateEl.value    = meta.date;
if (!coverEl.value   && meta.cover)   coverEl.value   = meta.cover;
addTag(initTags || meta.tags || '');

// Default date to today for new posts
if (!dateEl.value) {
  var now = new Date();
  var yyyy = now.getFullYear();
  var mm   = String(now.getMonth() + 1).padStart(2, '0');
  var dd   = String(now.getDate()).padStart(2, '0');
  dateEl.value = yyyy + '-' + mm + '-' + dd;
}

// ── Language preference ───────────────────────────────────────────────────────
var VALID_LANGS = ['en_AU', 'en_US', 'de_DE', 'es_ES', 'fr_FR', 'it_IT', 'nl_NL', 'pl_PL', 'pt_PT', 'sv_SE', 'zh_CN', 'ja_JP', 'ko_KR'];
var editorLang = localStorage.getItem('cms_editor_lang') || 'en_AU';
if (!VALID_LANGS.includes(editorLang)) editorLang = 'en_AU';

var langEl = document.getElementById('ed-lang');
if (langEl) langEl.value = editorLang;

function setEditorLang(lang) {
  if (!VALID_LANGS.includes(lang)) return;
  localStorage.setItem('cms_editor_lang', lang);
  location.reload();
}

// ── Vditor WYSIWYG editor ─────────────────────────────────────────────────────
// Vditor only accepts a fixed set of lang codes; custom codes (en_AU, de_DE …)
// throw "options.lang error".  The i18n option bypasses that check entirely —
// load the translations file first, then pass the result in directly.
var editor;

function initEditor(i18n) {
  try {
    var opts = {
      height:  520,
      mode:    'ir',
      cdn:     '/assets/vditor',
      value:   cleanBody,
      cache:   { enable: false },
      toolbar: [
        'headings', 'bold', 'italic', 'strike', '|',
        'line', 'quote', 'list', 'ordered-list', 'check', '|',
        'table', 'link', 'upload', '|',
        'code', 'inline-code', '|',
        'undo', 'redo', '|',
        'edit-mode', 'preview', 'fullscreen',
      ],
      after: function() {
        if (!stemEl.value) titleEl.focus();
      },
    };
    if (i18n) {
      opts.i18n = i18n;
    } else {
      opts.lang = 'en_US';   // fallback if i18n file missing
    }
    editor = new Vditor('editor', opts);
  } catch (e) {
    document.getElementById('editor').textContent = 'Editor error: ' + e;
    console.error('Vditor init failed:', e);
  }
}

// Dynamically load the i18n script (sets window.VditorI18n), then init.
(function () {
  var s = document.createElement('script');
  s.src = '/assets/vditor/dist/js/i18n/' + editorLang + '.js';
  s.onload  = function () { initEditor(window.VditorI18n); };
  s.onerror = function () { initEditor(null); };
  document.head.appendChild(s);
}());

// ── Status ────────────────────────────────────────────────────────────────────
var statusTimer;
function setStatus(msg, ok) {
  statusEl.textContent = msg;
  statusEl.style.color = ok ? '#16a34a' : '#dc2626';
  clearTimeout(statusTimer);
  statusTimer = setTimeout(function() { statusEl.textContent = ''; }, 3000);
}

// ── API ───────────────────────────────────────────────────────────────────────
async function save() {
  if (tagInput.value.trim()) addTag(tagInput.value);
  var stem = stemEl.value;
  var payload = {
    title:   titleEl.value,
    body:    editor.getValue(),
    summary: summaryEl.value,
    tags:    tags.slice(),
    date:    dateEl.value  || null,
    cover:   coverEl.value || null,
  };
  var r = await fetch(
    stem ? '/api/drafts/' + stem : '/api/drafts',
    { method: stem ? 'PATCH' : 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload) }
  );
  if (r.ok) {
    var d = await r.json();
    if (!stem && d.stem) stemEl.value = d.stem;
    setStatus('Saved \u2713', true);
  } else {
    setStatus('Save failed: ' + r.status, false);
  }
}

async function publish() {
  await save();
  var stem = stemEl.value;
  if (!stem) return;
  var r = await fetch('/api/publish/' + stem, { method: 'POST' });
  if (r.ok) { var d = await r.json(); window.location = d.path; }
  else setStatus('Publish failed: ' + r.status, false);
}

async function unpublish() {
  var stem = stemEl.value;
  if (!stem) return;
  var r = await fetch('/api/unpublish/' + stem, { method: 'POST' });
  if (r.ok) window.location = '/cms';
  else setStatus('Unpublish failed: ' + r.status, false);
}
