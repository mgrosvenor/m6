# Security

## Reporting

Report security issues privately, **not** through the public issue tracker.

- **Preferred:** GitHub [private vulnerability reporting](https://github.com/mgrosvenor/m6/security/advisories/new).
  It is private, it threads, and it takes attachments.
- Otherwise: the [contact form](https://mgrosvenor.com/contact).

No address is published here deliberately. An address in a file GitHub renders on
the repository's front page is an address scrapers read, and the two channels
above both reach the maintainer without one.

Please include what you did, what you observed, and anything that helps
reproduce it: a request, a packet capture, a log line with its timestamp. If you
have a proof of concept, say so and hold it until we have talked -- use GitHub
for anything you need to attach, since the contact form takes text only.

You will get an acknowledgement. This is a one-person project, so please allow
a reasonable window before disclosing publicly.

## What is in scope

The m6 workspace: `m6-http` (the edge: TLS, HTTP/1.1, HTTP/2, HTTP/3, the
cache, proxying), `m6-core`, and the services built on it (`m6-file`,
`m6-html`, `m6-auth-server`, `m6-md`, `m6-monitor`).

Particularly interesting:

- request smuggling and response splitting across the three protocols
- cache poisoning, and any way to get one client's response to another
- path traversal out of a site directory
- anything that lets an unauthenticated request reach authenticated state
- denial of service that a single peer can cause cheaply

## What is not

- Volumetric denial of service. A flood from a botnet is not a finding.
- Missing hardening headers on a page with no secrets on it.
- Findings from an automated scanner with no working demonstration.

## Supported versions

The newest release on `main`. This project does not currently backport fixes.

## How the project tries to prevent these

Stated so a reporter knows what has already been looked at:

- Differential testing against independent implementations: h1spec, h2spec and
  h3spec run on every push, with recorded minimum scores that may not fall.
- Path parameters go through one validator, and traversal is refused in one
  place rather than in each service.
- Conditional requests, content negotiation and cookie handling each have a
  single implementation in `m6-core`, because the defects that reached
  production were consistently the second copy of something.
- `cargo deny` checks advisories, licences and duplicate dependencies on every
  push.
