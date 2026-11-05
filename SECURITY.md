# Security

This is m6's security policy.
It names the reporting route, the scope, the supported versions, and the defences m6 already runs.
Sections 1 and 2 are the ones a reporter reads first.

1. [Reporting a vulnerability](#1-reporting-a-vulnerability)
2. [What is in scope](#2-what-is-in-scope)
3. [What is out of scope](#3-what-is-out-of-scope)
4. [Supported versions](#4-supported-versions)
5. [How m6 defends itself](#5-how-m6-defends-itself)
6. [Summary](#6-summary)

## 1. Reporting a vulnerability

This section is the reporting route.
Report a vulnerability through GitHub private vulnerability reporting.
The route is `https://github.com/mgrosvenor/m6/security/advisories/new`.
Do not file a vulnerability in the public issue tracker.
No email address is published.

Include what you did, what you observed, and anything that reproduces it.
A request, a packet capture, or a log line with its timestamp is worth more than a description.
If you have a proof of concept, say so and hold the details until the maintainer asks.

GitHub private vulnerability reporting is the only route.

## 2. What is in scope

Everything in the m6 workspace is in scope.
Table 1 lists the crates and what each one does.

| crate | role |
|---|---|
| `m6-http` | the edge: *TLS* (Transport Layer Security) termination, HTTP/1.1, HTTP/2 and HTTP/3, the cache, proxying |
| `m6-core` | the runtime library every service links |
| `m6-auth` | the authentication library |
| `m6-auth-server` | the authentication service |
| `m6-auth-cli` | the command-line tool for authentication |
| `m6-file` | the static file service |
| `m6-html` | the renderer service |
| `m6-md` | the Markdown processor |
| `m6-monitor` | the monitor |

**Table 1: the m6 workspace crates and their roles.**
Take from it that the edge and the authentication service sit on the request path, while the rest are services built on m6-core.

Five areas are of particular interest:

- request smuggling and response splitting across the three protocols
- cache poisoning, and any way to send one client's response to another
- path traversal out of a site directory
- any way an unauthenticated request reaches authenticated state
- a denial of service that a single peer can cause cheaply

A report outside these five areas is still read.

## 3. What is out of scope

Three things are out of scope:

- Volumetric denial of service.
  A flood from a botnet is not a finding.
- Missing hardening headers on a page with no secrets.
- A scanner finding with no working demonstration.

These three are excluded because each is cheap to produce and rarely actionable.
A reproducible defect outside them is in scope, whatever it is.

## 4. Supported versions

The newest release on `main` is supported.
m6 does not backport fixes to older releases.

## 5. How m6 defends itself

m6 already runs defences, so a reporter knows what is covered.
Table 2 lists them.

| defence | where it runs |
|---|---|
| conformance testing against h1spec, h2spec and h3spec, with recorded floors that may not fall | *CI* (continuous integration) |
| one path-parameter validator in `m6-core`, which refuses traversal in one place | inside every service |
| one implementation each for conditional requests, content negotiation and cookies, in `m6-core` | inside every service |
| `cargo deny`, checking known advisories, licence problems, duplicated crates and dependency sources | *CI* |

**Table 2: the defences m6 runs, and where each one runs.**
Take from it that the protocol and dependency checks run in CI, while the request-path validations run inside every service.

The two CI checks run on every pull request and every push to `main` or `develop`, and on every release tag.
The conformance floors are recorded in `tools/conformance-scores.txt`, and a run below a floor fails.

## 6. Summary

Report a vulnerability through GitHub private vulnerability reporting, and do not use the public issue tracker.
The m6 workspace, listed in Table 1, is in scope, and the newest release on `main` is the supported version.
Table 2 lists the defences already running, so a report that defeats one of them is a new finding.
