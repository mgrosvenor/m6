# m6-md

m6-md converts a directory of Markdown files into one *JSON* (JavaScript Object Notation) document that an m6 route serves as params.
It is a member of the m6 workspace at `m6-md/`, and it runs as a command-line tool that writes one file and exits.
Sections 2 to 5 describe the command, its input and its output, section 6 shows how a site serves the output, and sections 7 to 9 give the errors, the limits and the sources.

## 1. Contents

1. [Contents](#1-contents)
2. [The command line](#2-the-command-line)
3. [Input files](#3-input-files)
4. [Frontmatter](#4-frontmatter)
5. [The output file](#5-the-output-file)
6. [Serving the output](#6-serving-the-output)
7. [Errors and exit codes](#7-errors-and-exit-codes)
8. [Limits](#8-limits)
9. [Summary](#9-summary)

The command line, the input and the output are in `m6-md/src/main.rs`.

## 2. The command line

m6-md takes one input directory and one output path, and Table 1 gives every argument and flag it accepts.
An ordinary run needs those two arguments.

```sh
m6-md <input-dir> --output <file> [--log-level <level>] [--version] [--version-full]
```

| argument or flag | required | what it does |
|---|---|---|
| `<input-dir>` | yes | Directory of `*.md` files. m6-md reads one level. |
| `--output <file>` | yes | Path of the JSON file to write. |
| `--log-level <level>` | no | `trace`, `debug`, `info`, `warn` (or `warning`) or `error`, in any letter case. Default `info`. Any other value is a usage error, exit 2. |
| `--version`, `-V` | no | Print the tag form, `v1.16.0-rc.1`, and exit 0. |
| `--version-full` | no | Print one `<crate> - <full SemVer>` line per m6 crate in the binary, then exit 0. |

**Table 1: every command-line argument and flag, whether it is required, and what it does.**
The two version flags write no file.

A successful run logs one JSON object per line to stdout, or to journald's native socket under systemd, and usage and runtime errors go to stderr as plain text.
The log opens with the start-up record and closes with one `type=shutdown` line whose reason is `pass complete` or `pass failed` ([`m6-log.md`](m6-log.md) §9).
Section 3 describes the input directory.

## 3. Input files

A run reads one directory level of `*.md` files, and each file becomes one document in the output.
Three rules decide which files it reads: the extension is exactly `md`, a name beginning with `_` is skipped, and a subdirectory is ignored.
The input directory must exist and be a directory, and a run that names a file or a missing directory stops with exit 1.

A file may begin with a *TOML* (Tom's Obvious Minimal Language) frontmatter block.
The block opens with `+++` as the first three characters of the file, after a leading byte order mark is stripped.
It closes at the next line that begins with `+++`, and the Markdown after the closing line becomes the body.
A file with no closing `+++` is read whole as body.
A file with no block is body only.

Section 4 gives the keys the block may hold, and section 5 gives what the body becomes.

## 4. Frontmatter

Frontmatter is a *TOML* block, and m6-md reads two keys from it.
Table 2 gives them, and both are optional.

| key | type | default | what m6-md does with it |
|---|---|---|---|
| `title` | string | the file stem with each hyphen replaced by a space | Display title |
| `date` | string, or a TOML date or datetime | the file's modification time as `YYYY-MM-DD` in *UTC* (Coordinated Universal Time) | Sort key |

**Table 2: the two frontmatter keys m6-md reads, their types and their defaults.**

A value of the wrong type falls back to the default, and the run continues.
Every other key is converted to a JSON value and copied into the output document: a string stays a string, an array stays an array, a table becomes an object, and a TOML datetime becomes a string.
The names `stem`, `path` and `body` belong to m6-md, and a frontmatter block that sets one of them is ignored for that key with a warning in the log.

A key such as `summary` or `tags` appears in the output beside the five fields in Table 3.
Section 5 gives the output document.

## 5. The output file

One run writes one pretty-printed *JSON* object with a single `documents` array, and each element is one input file.
Table 3 gives the five fields m6-md always writes.
A site that serves a blog under `/blog` builds its links from `stem`, because `path` is the stem with a leading slash.

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
m6-md writes a sibling temp file and renames it over the output, so a reader sees the old document array or the new one.
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

Section 6 shows how a site serves the file.

## 6. Serving the output

A site serves the output through an m6-html route that lists the file in its `params` array, and the route pattern supplies the value the template looks up.
This section shows both.

A renderer route names a template and the params file that feeds it:

```toml
[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json"]
```

The object's keys become template variables, so `documents` is the array of posts, and the `{stem}` segment of the matched path becomes the `stem` variable.
The route that serves the index reads the same file, so one regenerated document feeds both pages.
In `site.toml`, two `[[route]]` entries send `/blog` and `/blog/{stem}` to the m6-html backend.

A template finds its post in the array and answers 404 when the lookup returns nothing:

```html
{% set post_list = documents | filter(attribute="stem", value=stem) %}
{% if post_list | length == 0 %}{{ not_found() }}{% endif %}
{% set post = post_list | first %}
```

`not_found()` makes the render return the not-found result that m6-html turns into a 404.
The index links each post with `/blog/{{ doc.stem }}`, because `path` carries no `/blog` prefix.

m6-html reads a static params file when it builds its routes, at start and again on each `POST /genid` reload.
A running renderer keeps serving the old document array until one of those happens, so run m6-md again after a post changes and then restart the renderer or reload it.
[`m6-http.md`](m6-http.md) owns `/genid` and the cache, and Example 02 in [`m6-user-guide.md`](m6-user-guide.md) is the full site with both templates.

## 7. Errors and exit codes

m6-md exits with one of three codes, and Table 4 gives the condition behind each.

| code | when |
|---|---|
| 0 | The run wrote the output, or a version flag printed its answer |
| 1 | A runtime failure: the input directory is missing or is a file, the output path's parent directory is missing, a source file cannot be read, a frontmatter block is malformed, or the output cannot be written |
| 2 | A usage error: `<input-dir>` or `--output` is missing, a flag has no value, a flag is unknown, a second positional argument is given, or `--log-level` is not a level name |

**Table 4: the three exit codes and the conditions behind each.**

A malformed frontmatter block stops the run with the file named in the message, and the frontmatter parser adds the position of the error.
A source file that cannot be read stops the run at that file, and the run writes no output file.

## 8. Limits

Four limits follow from reading one directory and writing one file.

- **One directory level.**
  A nested collection needs one m6-md run per directory and one output file per run.
- **The collection is held in memory.**
  Every document is built in memory and serialised to one string, so memory grows with the total size of the source files.
- **Plain code blocks.**
  `body` carries plain `<code>` blocks, and highlighting belongs to the template or the stylesheet.
- **Links stay as written.**
  A Markdown link such as `[other post](other-post.md)` appears in `body` unchanged, so a site writes the served route path in its source.

## 9. Summary

m6-md is one command, one directory of Markdown and one JSON file, and a site serves that file through a route.
The command line has two required arguments and three optional flags, the frontmatter has two keys, and the output has five fixed fields.
Table 5 gives the sources for the subjects around it.

| source | what it owns |
|---|---|
| `m6-md/src/main.rs` | m6-md's command line, input and output |
| `Cargo.toml`, `m6-md/Cargo.toml` | the workspace members list, and the crate's dependencies |
| `m6-core/src/site.rs`, [`m6-core-reference.md`](m6-core-reference.md) §4.6 | the renderer's static params, read at start and on reload |
| [`m6-user-guide.md`](m6-user-guide.md) | Example 02, the blog that serves this output, with both templates |
| [`m6-site-toml.md`](m6-site-toml.md) | `[[route]]` and the renderer `params` key |
| [`m6-http.md`](m6-http.md) | `/genid`, the cache, and when a change clears it |

**Table 5: where the code and the surrounding subjects are written down.**
