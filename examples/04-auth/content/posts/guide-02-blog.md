+++
title   = "Guide 02: Blog with m6-md"
date    = "2025-01-06"
summary = "Add a blog using Markdown source files. m6-md converts them to JSON; m6-html serves the blog from that single file."
tags    = ["guide", "tutorial", "m6-md"]
cover   = "https://picsum.photos/seed/guide02/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
**Example 02 — Blog** →
[Example 03](/blog/guide-03-contact-form) →
[Example 04](/blog/guide-04-auth) →
[Example 05](/blog/guide-05-cms) →
[Example 06](/blog/guide-06-systemd) →
[Example 07](/blog/guide-07-dev-to-production)

---

`examples/02-blog/` adds a blog. Markdown source files in `content/posts/` are processed by `m6-md` into a single JSON file at `data/posts.json`. m6-html serves the blog from that file. No per-post JSON files, no `[[route_group]]`.

```bash
cd examples/02-blog
cargo install m6-md   # separate project, one-time install

# Generate posts.json from Markdown source, then start the site
m6-md content/posts/ --output data/posts.json
./dev.sh
```

To update content, re-run `m6-md content/posts/ --output data/posts.json`. m6-html reads `data/posts.json` once, at start, so restart the stack to see the change, or send `POST /genid` where the site has a generation (Example 05 does).

## What's new

<pre class="mermaid">
flowchart LR
    MDFiles["content/posts/\n*.md"] --> m6md["m6-md"]
    m6md --> JSON["data/posts.json"]
    JSON --> m6html["m6-html\n(Tera templates)"]
    m6html --> Browser
</pre>

`content/posts/` contains Markdown source files with TOML frontmatter. `m6-md` produces `data/posts.json` — a single JSON object with a `documents` array containing every post's metadata and rendered HTML body. m6 itself has no knowledge of Markdown.

## Source file format

```
+++
title   = "Hello World"
date    = "2024-01-15"
summary = "A brief introduction to the blog."
tags    = ["rust", "web"]
+++

Post body in **Markdown**. GFM extensions supported: tables, strikethrough, footnotes, task lists, autolinks.
```

Any frontmatter key beyond `title` and `date` passes through to the JSON as-is. The `body`, `stem`, and `path` keys are reserved — m6-md sets them and will warn if you try to use them in frontmatter.

Files beginning with `_` are skipped — use this for drafts and partials.

## `data/posts.json` (produced by m6-md)

```json
{
  "documents": [
    {
      "stem":    "hello-world",
      "path":    "/hello-world",
      "title":   "Hello World",
      "date":    "2024-01-15",
      "body":    "<p>Post body in <strong>Markdown</strong>...</p>",
      "summary": "A brief introduction to the blog.",
      "tags":    ["rust", "web"]
    }
  ]
}
```

Sorted by `date` descending. `body` is fully rendered HTML — m6-html just inserts it with the `safe` filter, no further processing. The write is atomic: m6-md writes to a `.tmp` file then renames, so m6-html never sees a partial file.

## New routes in `site.toml`

```toml
[[route]]
path    = "/blog"
backend = "m6-html"

[[route]]
path    = "/blog/{stem}"
backend = "m6-html"
```

No `[[route_group]]`. The routes are fixed patterns — m6-html handles any `/blog/{stem}` and looks up the matching post in the params at request time.

## New routes in `configs/m6-html.conf`

```toml
[[route]]
path     = "/blog"
template = "templates/post-index.html"
params   = ["data/posts.json"]

[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json"]
```

Both routes load the same file. The index template iterates `documents`. The post template filters by `stem`.

## `templates/post-index.html`

```html
{% extends "base.html" %}
{% block title %}Blog · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Blog</h1>
  {% for doc in documents %}
    <article>
      <h2><a href="/blog/{{ doc.stem }}">{{ doc.title }}</a></h2>
      <time>{{ doc.date }}</time>
      {% if doc.summary %}<p>{{ doc.summary }}</p>{% endif %}
    </article>
  {% endfor %}
{% endblock %}
```

Note: links use `/blog/{{ doc.stem }}` — not `{{ doc.path }}`. The `path` field from m6-md is `/{stem}` (without the `/blog/` prefix). Use `doc.stem` to construct URLs in a blog context.

## `templates/post.html`

```html
{% extends "base.html" %}
{% block title %}
  {%- set post = documents | filter(attribute="stem", value=stem) | first -%}
  {{ post.title }} · {{ site_name }}
{% endblock %}
{% block content %}
  {% set post = documents | filter(attribute="stem", value=stem) | first %}
  <article>
    <h1>{{ post.title }}</h1>
    <time>{{ post.date }}</time>
    {{ post.body | safe }}
  </article>
{% endblock %}
```

`stem` is a built-in key provided by m6-html — the `{stem}` capture from the matched URL. The `filter` built-in finds the matching post from the `documents` array.

**Important Tera behaviour:** `{% set %}` at the top level of a child template is silently discarded. Set `post` independently inside each `{% block %}` where you need it. This differs from Jinja2.

## Running m6-md

```bash
# One-shot generation
m6-md content/posts/ --output data/posts.json

# Re-run it whenever a post changes (with entr or similar)
find content/posts/ -name '*.md' | entr m6-md content/posts/ --output data/posts.json
```

m6-md has no watch mode of its own and does not signal the server. After it writes `data/posts.json`, the running m6-html still serves the file it read at start: restart it, or send `POST /genid` to m6-http where the site is a generation source.

---

**← [Example 01 — Static Site](/blog/guide-01-static-site)** | **[Example 03 — Contact Form](/blog/guide-03-contact-form) →**
