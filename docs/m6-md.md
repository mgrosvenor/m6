# m6-md

Converts a directory of Markdown files into a single JSON file for use as m6-html params. Part of the m6 examples repo — a separate project from the m6 core binaries, not required to use m6.

---

## CLI

```
m6-md <input-dir> --output <output-file> [--log-level debug]
```

| Argument / Flag | Required | Notes |
|---|---|---|
| `<input-dir>` | yes | Directory of `*.md` source files. Non-recursive. |
| `--output <file>` | yes | Path to write JSON output. Written atomically (temp file + rename). |
| `--log-level` | no | `debug` `info` `warn` `error`. Default `info`. |

Logs to stdout as structured JSON. Exits 0 on success, 1 on runtime error, 2 on bad arguments or malformed frontmatter.

---

## Source Files

`*.md` files in `<input-dir>`. Non-recursive — subdirectories ignored. Files beginning with `_` are skipped (convention for drafts or partials).

Each file may begin with a TOML frontmatter block delimited by `+++`:

```
+++
title = "Hello World"
date  = "2024-01-15"
+++

Post body in Markdown...
```

Frontmatter is optional. A file with no `+++` block is processed as body-only — `title` defaults to the filename stem, `date` defaults to the file's modification time in ISO 8601 format.

---

## Frontmatter

### Defined keys

m6-md recognises two keys and uses them for sorting and the `stem`/`path` fields. All other keys pass through to the JSON as-is.

| Key | Type | Default | Notes |
|---|---|---|---|
| `title` | string | filename stem | Display title |
| `date` | string | file mtime | ISO 8601 date or datetime — `"2024-01-15"` or `"2024-01-15T09:30:00Z"` |

### Passthrough keys

Any other frontmatter key is included in the output JSON unchanged. Common examples:

```toml
+++
title   = "Hello World"
date    = "2024-01-15"
summary = "A brief introduction."
tags    = ["rust", "web"]
author  = "Jane Smith"
draft   = false
+++
```

All of `summary`, `tags`, `author`, `draft` appear in the JSON output. m6-md does not filter or validate them.

---

## Output Format

A single JSON object with a `documents` array. Each element represents one source file.

```json
{
  "documents": [
    {
      "stem":  "hello-world",
      "path":  "/hello-world",
      "title": "Hello World",
      "date":  "2024-01-15",
      "body":  "<p>Post body rendered as HTML.</p>",
      "summary": "A brief introduction.",
      "tags":    ["rust", "web"],
      "author":  "Jane Smith",
      "draft":   false
    },
    {
      "stem":  "second-post",
      "path":  "/second-post",
      "title": "Second Post",
      "date":  "2024-01-20",
      "body":  "<p>...</p>"
    }
  ]
}
```

### Fixed output fields

m6-md always writes these fields regardless of frontmatter content:

| Field | Value |
|---|---|
| `stem` | Filename without extension — `hello-world.md` → `"hello-world"` |
| `path` | `"/" + stem` — used for linking in templates |
| `title` | From frontmatter, or filename stem if absent |
| `date` | From frontmatter, or file mtime as ISO 8601 if absent |
| `body` | Full document body rendered to HTML |

All frontmatter keys other than `title` and `date` are merged in after the fixed fields. If a frontmatter key conflicts with a fixed field name (`stem`, `path`, `body`) it is ignored with a warning.

### Sort order

`documents` array sorted by `date` descending (newest first). Files with identical dates sorted alphabetically by stem.

### Markdown rendering

comrak with GitHub Flavored Markdown extensions enabled: tables, strikethrough, autolinks, task lists, footnotes. HTML in source Markdown is passed through as-is.

---

## Atomicity

Output is written atomically: m6-md writes to `<output-file>.tmp` then renames to `<output-file>`. m6-html and m6-http never observe a partial file.

---

## Error Handling

| Condition | Behaviour |
|---|---|
| `<input-dir>` does not exist | Exit 2 |
| `--output` not specified | Exit 2 |
| `--output` directory does not exist | Exit 2 |
| Source file has malformed TOML frontmatter | Exit 2, names the file and line |
| Source file unreadable | Exit 1 |
| Output file not writable | Exit 1 |


---

## Integration with m6-html

With a single output file, blog routes in `configs/m6-html.conf` use the `documents` array directly:

```toml
global_params = ["data/site.json"]

[[route]]
path     = "/blog"
template = "templates/post-index.html"
params   = ["data/posts.json"]

[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json"]
```

`site.toml` needs no `[[route_group]]` for the blog — the routes are fixed patterns, not derived from a file glob. m6-http routes any `/blog/{stem}` to m6-html regardless of whether a matching post exists; m6-html returns the rendered page or 404 based on whether the stem is found in the `documents` array.

### Index template

```html
{% for doc in documents %}
  <article>
    <h2><a href="{{ doc.path }}">{{ doc.title }}</a></h2>
    <time>{{ doc.date }}</time>
    {% if doc.summary %}<p>{{ doc.summary }}</p>{% endif %}
  </article>
{% endfor %}
```

### Post template

`{stem}` is available as a built-in key in m6-html. Filter `documents` to find the matching post:

```html
{% set post = documents | filter(attribute="stem", value=stem) | first %}
{% if not post %}
  {# m6-html returns 404 for unmatched routes, but defensive check: #}
  <p>Post not found.</p>
{% else %}
  <h1>{{ post.title }}</h1>
  <time>{{ post.date }}</time>
  {{ post.body | safe }}
{% endif %}
```

### Cache invalidation

When m6-md rewrites `data/posts.json`, m6-http's inotify detects the file change. The invalidation map (derived from `[[route]]` and `[[route_group]]` declarations) maps `data/posts.json` to all routes that use it as a param — `/blog` and `/blog/{stem}` (all known stems). Those cache entries are evicted. The next request to any blog path re-populates the cache.

This requires that the invalidation map tracks which param files feed which routes, not just `[[route_group]]` file globs. See m6-http spec.

---

## `site.toml` Changes for Blog Routes

Compared to earlier examples, `[[route_group]]` is no longer needed for the blog. The blog section of `site.toml` simplifies to:

```toml
[[route]]
path    = "/blog"
backend = "m6-html"

[[route]]
path    = "/blog/{stem}"
backend = "m6-html"
```

No glob. No file-per-post in `content/posts/`. One output file at `data/posts.json`.

---

## Cargo.toml

```toml
[package]
name    = "m6-md"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "m6-md"

[dependencies]
comrak    = { version = "0.21", default-features = false, features = ["shortcodes"] }
toml      = "0.8"
serde     = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow    = "1"
tracing   = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
```

---

## Limitations

- Non-recursive — one level of `*.md` files only. Nested collections require separate m6-md invocations with separate `--output` files.
- All documents loaded into memory — not suitable for very large collections (thousands of long posts) without adjustment.
- No syntax highlighting — `body` HTML contains plain `<code>` blocks. Syntax highlighting is a template/CSS concern.
- No cross-document linking resolution — `[other post](other-post.md)` style links are not rewritten to `/blog/other-post`. Use explicit paths.
