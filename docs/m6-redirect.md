# m6-redirect

m6-redirect is a generic static redirect engine in the Cloudflare Bulk Redirects shape.
It is a member of the m6 workspace at `m6-redirect/`, and it runs as an m6 backend on a Unix socket.
A request that matches a rule answers the rule's status and target.
A request that matches nothing answers 404.
The list of rules lives in one JSON file, and the engine never builds strings for served markup or text in code.

This document describes the list, the matching, the configuration, the list-replace POST, the reload signal and the logging.
Section 2 gives the rules shape, section 3 the matching, section 4 the configuration, section 5 the list-replace POST, section 6 the reload and the response, and section 7 the logging.

## 1. Contents

1. [Contents](#1-contents)
2. [The rules list](#2-the-rules-list)
3. [Matching](#3-matching)
4. [Configuration](#4-configuration)
5. [The list-replace POST](#5-the-list-replace-post)
6. [The reload signal and the response](#6-the-reload-signal-and-the-response)
7. [Logging](#7-logging)
8. [Summary](#8-summary)

Section 2 gives the list the engine reads.

## 2. The rules list

The list is the Cloudflare Bulk Redirects list-items JSON shape, and Table 1 gives the fields the engine reads and what it does with the rest.

| field | type | what m6-redirect does with it |
|---|---|---|
| `source_url` | string, required | The host and path to match exactly, with an optional `http://` or `https://` scheme |
| `target_url` | string, required | The `Location` the redirect answers with |
| `status_code` | integer, optional, default 301 | One of 301, 302, 307 or 308 |
| `subpath_matching` | boolean, optional, default false | Must be false. A true value is refused, because the engine matches exactly |
| everything else | any | Accepted and ignored |

**Table 1: the fields of one list item, and what the engine does with each.**
A body that fails this shape is a malformed body and is refused with 400.

Two validation rules hold beyond Table 1.
Every `source_url` must be unique, and a `source_url` or `target_url` carrying a carriage return or line feed is refused, because either byte would escape the `Location` header the engine writes.
The engine does no regular expressions, no string replacement and no templating, exactly as Cloudflare Bulk Redirects do not.

The list file is read at start and on the reload signal of section 6.
A missing file is an empty table, so a fresh install serves no redirect until the first list is posted.
A malformed file stops the start with exit 2, because a corrupted list serving as silence would break every link.

Section 3 gives the matching.

## 3. Matching

A `GET` or `HEAD` request is matched against the table by host and path, and Table 2 gives the rules.

| part | comparison |
|---|---|
| host | Case-insensitive, with the port when one is present |
| path | Exact, case-sensitive, with no query string |
| query | Ignored |

**Table 2: how one request is matched against one rule.**
The host and the path must both match for a rule to answer.

A request whose host and path match answers the rule's status and `target_url` as `Location`.
A request that matches nothing answers 404 with no `Location`.
The comparison is exact: a trailing slash, an added character or a different letter case on the path all answer 404.
The host is matched case-insensitively, the way DNS names are.

Section 4 gives the configuration.

## 4. Configuration

The engine reads its settings from one `[redirect]` table in its service configuration, and Table 3 gives the keys.

| key | type | default | what it sets |
|---|---|---|---|
| `rules_file` | string | `/var/lib/m6-redirect/rules.json` | The list file. A relative path resolves against the site directory |
| `max_age_secs` | integer | 7776000 | The shared freshness lifetime in seconds, three months |
| `stale_while_revalidate_secs` | integer | 7776000 | How long past freshness a cache may serve the held redirect while it revalidates in the background, RFC 5861 |

**Table 3: the `[redirect]` keys, their types and their defaults.**
Every key is optional, so a config that says nothing serves the defaults.

The default lifetime is three months, the owner's default for a short link.
`stale_while_revalidate_secs` is the same window, so a held redirect keeps serving through an origin outage and until the owner removes it.
A removal is a purge of the path, which evicts the entry at once.
Both values are read at start and again on the reload signal of section 6, so a change takes effect on reload.

Section 5 gives the list-replace POST.

## 5. The list-replace POST

The engine replaces its list when it receives a `POST /m6-redirect/rules` over its socket, and Table 4 gives the answers.

| outcome | status | body |
|---|---|---|
| The body is a valid list | 204 | empty |
| The body is malformed | 400 | a plain-text reason |
| The file cannot be written | 500 | a plain-text reason |

**Table 4: the three answers of the list-replace POST.**
The method and the path are fixed.

The engine validates the body, writes it verbatim to `rules_file`, and swaps the table in one assignment.
The write is atomic: a temp file in the same directory, synced, then renamed over the target.
The directory is created when it is missing.
A malformed body writes nothing and changes nothing, so a failed replace never loses the list that was serving.

The POST is reached over the internal listener only, the same reachability the purge path uses, and the site routes it there.

Section 6 gives the reload signal and the response a redirect carries.

## 6. The reload signal and the response

A `POST /genid` is the backend's reload signal, and m6-redirect answers it by running the start path again: it reads the configuration and the rules file, and only then replaces both, so a failure changes nothing.
A deploy can update the rules file directly and send the reload signal, and the engine serves the new list.

A redirect answer carries the `Cache-Control` field of Table 5.

| directive | value |
|---|---|
| `public` | Marks the response storable, per the backend protocol |
| `s-maxage` | `max_age_secs` |
| `stale-while-revalidate` | `stale_while_revalidate_secs` |

**Table 5: the `Cache-Control` directives on every redirect answer.**
A 404 carries no `Cache-Control`, so the proxy treats it as uncacheable.

The redirect body is empty.
A `HEAD` answers the same status and `Location` with no body, the way the backend protocol requires.

Section 7 gives the logging.

## 7. Logging

m6-redirect writes one access line per request it answers, and one line per list replace.
Table 6 gives the per-request access line's fields.

| field | what it carries |
|---|---|
| `message` | `redirect` |
| `type` | `access` |
| `host` | the request's Host, as received |
| `path` | the request's path, exactly |
| `query` | the request's query string, absent when there was none |
| `status` | the status answered |
| `location` | the redirect's target, absent on a 404 |

**Table 6: the fields of m6-redirect's per-request access line.**

m6-redirect sees only misses: the edge caches the 302 for three months, so this line is the origin's own record of the redirects it expanded, not the whole click record.
A list-replace POST writes one line with the rules count on success, and one with the reason on a refusal or a failed write.

Section 8 collects the whole document.

## 8. Summary

m6-redirect is a static, exact-match redirect engine with one list file and two ways to change it.
The list-replace POST writes and swaps, and the reload signal reads and swaps.
A rule answers the status and target it holds, and everything else answers 404.
The engine knows nothing about codes, pages, campaigns or query parameters, and it serves none of them in a way that ties m6 to one site.

The code is `m6-redirect/src/main.rs` and `m6-redirect/src/rules.rs`.
The lifecycle test is `m6-redirect/tests/lifecycle.rs`, and the behaviour tests are `m6-redirect/tests/behaviour.rs`.
