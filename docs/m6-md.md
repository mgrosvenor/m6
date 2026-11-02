# m6-md

m6-md converts a directory of Markdown files into one *JSON* (JavaScript Object Notation) document that an m6 route serves as params.
It is a member of the m6 workspace at `m6-md/`, built by `cargo build --workspace`, and it runs as a command-line tool that writes one file and exits.
Where this page and the code disagree, the code is right and this page is a defect.

## 1. How to read this page

The page is one binary's reference, and the list below links every section.
The nine sections after this one are:

2. [The command line](#2-the-command-line)
3. [Input files](#3-input-files)
4. [Frontmatter](#4-frontmatter)
5. [The output file](#5-the-output-file)
6. [Watch mode](#6-watch-mode)
7. [Serving the output](#7-serving-the-output)
8. [Errors and exit codes](#8-errors-and-exit-codes)
9. [Limits](#9-limits)
10. [Summary](#10-summary)

The command line, the input, the output and the watch loop are in `m6-md/src/main.rs`.
Table 5 names the sources for the workspace membership and the renderer's params.

## 2. The command line

m6-md takes one input directory and one output path, and Table 1 gives every argument and flag it accepts.
Take from it that an ordinary run needs those two arguments and nothing else.

```sh
m6-md <input-dir> --output <file> [--log-level <level>] [--watch] [--touch <path>]
```

| argument or flag | required | what it does |
|---|---|---|
| `<input-dir>` | yes | Directory of `*.md` files. m6-md reads one level. |
| `--output <file>` | yes | Path of the JSON file to write. |
| `--log-level <level>` | no | Log filter directive, default `info`. An invalid value falls back to `info`. |
| `--watch` | no | Stay running and regenerate the output on every `.md` change. |
| `--touch <path>` | no | With `--watch`, touch this file after a change triggers a regeneration. |
| `--version`, `-V` | no | Print the m6-md version and exit 0. |

**Table 1: every command-line argument and flag, whether it is required, and what it does.** The version flag writes no file.

The input directory is positional and the output path is required, so a run that names both completes one pass.
The two flags that change what a run does are `--watch` and `--touch`, and section 6 covers them together.
A successful run logs to stdout as one JSON object per line, and usage and runtime errors go to stderr as plain text, so a caller can read the output file and ignore the log.
Section 3 starts with the input directory that feeds that file.

## 3. Input files

A run reads one directory level of `*.md` files, and each file becomes one document in the output.
Three rules decide which files it reads: the extension must be exactly `md`, a name beginning with `_` is skipped, and a subdirectory is ignored.

A file may begin with a *TOML* (Tom's Obvious Minimal Language) frontmatter block at its first character.
A leading byte order mark is stripped before that check, so an editor that writes one does not hide the block.
The block opens with `+++` as the first three characters of the file and closes with the next line that begins with `+++`, and the Markdown after the closing line becomes the body.
A file with no closing `+++` is read whole as body, so a forgotten delimiter costs the frontmatter and not the run.
Frontmatter is optional, and a file with no block is body only.

The input directory must exist and be a directory, and a run that names a file or a missing directory stops with exit 1.
Section 4 gives the keys the block may hold, and section 5 gives what the body becomes.

## 4. Frontmatter

Frontmatter is a *TOML* (Tom's Obvious Minimal Language) block, and m6-md reads two keys from it.
Table 2 gives them.
Take from it that both keys are optional and each has a default taken from the file.

| key | type | default | what m6-md does with it |
|---|---|---|---|
| `title` | string | the file stem with each hyphen replaced by a space | Display title |
| `date` | string, or a TOML date or datetime | the file's modification time as `YYYY-MM-DD` in *UTC* (Coordinated Universal Time) | Sort key |

**Table 2: the two frontmatter keys m6-md reads, their types and their defaults.**

A value of the wrong type falls back to the default, and the run continues.
Every other key is converted to a JSON value and copied into the output document: a string stays a string, an array stays an array, a table becomes an object, and a TOML datetime becomes a string.
The names `stem`, `path` and `body` belong to m6-md, and a frontmatter block that sets one of them is ignored for that key with a warning in the log.

A key such as `summary` or `tags` is site data, and it appears in the output beside the five fields section 5 describes.
`title` and `date` are the only keys that change the output's shape, and the rest of the document is the site's to define.
Section 5 gives the output document that carries them.

## 5. The output file

One run writes one pretty-printed *JSON* (JavaScript Object Notation) object with a single `documents` array, and each element is one input file.
Table 3 gives the five fields m6-md always writes.
Take from it that `path` is the stem with a leading slash, and a site that serves a blog under `/blog` builds its links from the stem.

| field | value |
|---|---|
| `stem` | The file name without its extension: `hello-world.md` gives `hello-world` |
| `path` | `/` followed by the stem: `/hello-world` |
| `title` | The frontmatter `title`, or the default in Table 2 |
| `date` | The frontmatter `date`, or the default in Table 2 |
| `body` | The Markdown after the frontmatter, rendered to *HTML* (HyperText Markup Language) |

**Table 3: the five fields m6-md writes into every document.**

The array is sorted by `date` descending, compared as text, and two documents with the same date are sorted by `stem` ascending.
`body` is rendered by comrak with tables, strikethrough, autolinks, task lists, footnotes and emoji shortcodes enabled, and raw HTML in the source passes through.
The file is written whole: m6-md writes a sibling temp file and renames it over the output, so a reader sees the old document array or the new one.
The temp path is the output path with its extension replaced by `json.tmp`, which makes `data/posts.json` write through `data/posts.json.tmp`.

A source file with a frontmatter block produces one element of that array:

```toml
+++
title   = "Hello World"
date    = "2024-01-15"
summary = "A brief introduction."
tags    = ["rust", "web"]
+++

Post body in **Markdown**.
```

The four frontmatter keys and the body become this element:

```json
{
  "stem": "hello-world",
  "path": "/hello-world",
  "title": "Hello World",
  "date": "2024-01-15",
  "body": "<p>Post body in <strong>Markdown</strong>.</p>\n",
  "summary": "A brief introduction.",
  "tags": ["rust", "web"]
}
```

The order of the array is the order a site lists its posts in, and section 6 covers the mode that keeps the file current while a site runs.

## 6. Watch mode

`--watch` keeps m6-md resident and regenerates the output whenever a `.md` file in the input directory changes.
The first pass runs once before the watcher starts and writes the output file.
It does not touch the `--touch` path, so a site that is already up keeps its old params until the next `.md` change or a manual `site.toml` touch.

The watcher covers the input directory itself and reacts to create, modify and remove events on paths that end in `.md`.
After a relevant event it waits 50 milliseconds and drains the queue, so an editor that deletes and renames a file counts as one change.
A regeneration that fails is logged and retried on the next change, and the process stays up.

The process shuts down on the first `SIGTERM` or `SIGINT` after a clean stop and exits immediately on a second, with exit code 0 either way.
Section 7 shows how a running site uses `--touch` to learn that a post changed.

## 7. Serving the output

A site serves the output through an m6-html route that lists the file in its `params` array, and the route pattern supplies the value the template looks up.
The contract is one key from the file and one variable from the path, and this section shows both.

A renderer route names a template and the params file that feeds it:

```toml
[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json"]
```

The object's keys become template variables, so `documents` is the array of posts, and the `{stem}` segment of the matched path becomes the `stem` variable.
The route that serves the index reads the same file, so one regenerated document feeds both pages.
In `site.toml`, two `[[route]]` entries send `/blog` and `/blog/{stem}` to the m6-html backend, and the blog needs no `[[route_group]]` because both paths are fixed patterns.

A template finds its post in the array and answers 404 when the lookup returns nothing:

```html
{% set post_list = documents | filter(attribute="stem", value=stem) %}
{% if post_list | length == 0 %}{{ not_found() }}{% endif %}
{% set post = post_list | first %}
```

`not_found()` makes the render return the not-found result that m6-html turns into a 404.
The index links each post with `/blog/{{ doc.stem }}`, because `path` carries no `/blog` prefix.

A static params file is read once when m6-html builds its routes, so a running renderer keeps serving the old document array until a reload rebuilds it.
The example runs m6-md with `--watch --touch site.toml`, so a successful regeneration in the watch loop touches the file, which makes the renderer rebuild.
[`m6-http.md`](m6-http.md) owns the cache and its clearing rules, and Example 02 in [`m6-user-guide.md`](m6-user-guide.md) is the full site with both templates.
Section 8 gives the exit codes a run can return.

## 8. Errors and exit codes

m6-md exits with one of three codes, and Table 4 gives the condition behind each.
Take from it that a usage mistake exits 2 and every failure during a run exits 1.

| code | when |
|---|---|
| 0 | The run wrote the output, `--version` printed the version, or watch mode shut down cleanly |
| 1 | A runtime failure: the input directory is missing or is a file, the output path's parent directory is missing, a source file cannot be read, a frontmatter block is malformed, the output cannot be written, or the watcher cannot start |
| 2 | A usage error: `<input-dir>` or `--output` is missing, a flag has no value, a flag is unknown, or a second positional argument is given |

**Table 4: the three exit codes and the conditions behind each.**

A malformed frontmatter block stops the run with the file named in the message, and the frontmatter parser adds the position of the error.
A source file that cannot be read stops the run at that file, and the run writes no output file.

The codes separate a wrong invocation from a failed run, and the message names the file that failed.
Section 9 lists what m6-md leaves out.

## 9. Limits

Four limits follow from reading one directory and writing one file.

- **One directory level.**
  A nested collection needs one m6-md run per directory and one output file per run.
- **The collection is held in memory.**
  Every document is built in memory and serialised to one string, so memory grows with the total size of the source files.
- **No syntax highlighting.**
  `body` carries plain `<code>` blocks, and highlighting belongs to the template or the stylesheet.
- **No link rewriting.**
  A Markdown link such as `[other post](other-post.md)` stays as written, so a site writes the served route path in its source.

The run has no option for any of them, and section 10 says where the surrounding subjects are written down.

## 10. Summary

m6-md is one command, one directory of Markdown and one JSON file, and a site serves that file through a route.
The command line has two required arguments and four optional flags, the frontmatter has two keys, and the output has five fixed fields.
Watch mode keeps the file current, and `--touch` is what tells a running site to reload.

Table 5 gives the sources for what this page leaves out.
Take from it that every subject here has one owner.

| source | what it owns |
|---|---|
| `m6-md/src/main.rs` | m6-md's command line, input, output and watch behaviour |
| `Cargo.toml`, `m6-md/Cargo.toml` | the workspace members list, and the crate's dependencies |
| `m6-core/src/app.rs`, [`m6-render-lib.md`](m6-render-lib.md) | the renderer's static params, read at route build and refreshed on reload |
| [`m6-user-guide.md`](m6-user-guide.md) | Example 02, the blog that serves this output, with both templates |
| [`m6-site-toml.md`](m6-site-toml.md) | `[[route]]`, `[[route_group]]` and the renderer `params` key |
| [`m6-http.md`](m6-http.md) | the cache, and when a change clears it |
| [`m6-testing.md`](m6-testing.md) | how the workspace's tests run |

**Table 5: where the code and the surrounding subjects are written down.**

The tool, its output and the route that serves it are the whole subject, and Table 5 is where the next question goes.
