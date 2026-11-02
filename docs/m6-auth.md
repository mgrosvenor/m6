# m6-auth-server

`m6-auth-server` is the authentication service in m6.
It checks a username and password, issues *JWT* (JSON Web Token) access and refresh tokens, renews an access token, revokes a refresh token, and publishes the signing public key.
m6-http verifies access tokens locally, so only login, refresh and logout reach this service.

This page is the reference for the service binary.
The library crate has its own page in [`m6-auth-lib.md`](m6-auth-lib.md), the administrative tool in [`m6-auth-cli.md`](m6-auth-cli.md), and the route and `[auth]` configuration in [`m6-site-toml.md`](m6-site-toml.md).
The crate manifest is `m6-auth-server/Cargo.toml`.

Auth is optional.
A site whose routes declare no `require` and whose configuration declares no `[auth]` runs no token check and needs neither this service nor a signing key.
Section 2 states what the service owns, and section 13 lists the documents that own the rest.

## 1. Contents

The sections follow the order a deployment meets the service, from the binary to the limits.

2. [The service](#2-the-service)
3. [Command line](#3-command-line)
4. [Configuration](#4-configuration)
5. [HTTP API](#5-http-api)
6. [Tokens](#6-tokens)
7. [m6-http integration](#7-m6-http-integration)
8. [Renderer integration](#8-renderer-integration)
9. [Storage](#9-storage)
10. [Rate limiting](#10-rate-limiting)
11. [Lifecycle](#11-lifecycle)
12. [Limits](#12-limits)
13. [Summary and further reading](#13-summary-and-further-reading)

Section 2 begins with what the service owns.

## 2. The service

`m6-auth-server` owns four routes on a Unix socket: login, refresh, logout and the public key.
Table 1 gives them.
Take from it that every route is literal, so the service has no route configuration of its own.

| method | path | what it does | success |
|---|---|---|---|
| `POST` | `/auth/login` | checks a password and issues an access token and a refresh token | 302 with cookies for a form client, 200 with JSON for a JSON client |
| `POST` | `/auth/refresh` | issues a new access token from a refresh token | 302 with a session cookie for a form client, 200 with JSON for a JSON client |
| `POST` | `/auth/logout` | revokes refresh tokens, and clears cookies on the browser path | 302 for the browser path, 204 for the bearer path |
| `GET` | `/auth/public-key` | returns the current public key | 200 with a PEM body |

**Table 1: the four routes, what each does, and what success returns.**

The method column is part of the contract.
A `GET` on login, refresh or logout does not match the route.

The service holds the database, the signing keys, the token lifetimes and the rate limiter in one shared state.
It does not manage users or groups.
`m6-auth-cli` (the *CLI*, command-line interface) writes those rows directly, and the service reads them at login and refresh.

In a deployment, m6-http forwards a request to the service over the Unix socket.
The kernel supplies no client address on that socket.
Section 10 explains what it uses instead, and section 3 gives the socket path.

## 3. Command line

`m6-auth-server` takes a site directory and a config path.
Table 2 gives the arguments and flags.
Take from it that every flag is optional and the two positional arguments are required.

| argument or flag | meaning |
|---|---|
| `<site-dir>` | the site root, used to resolve the key paths and the default `site.toml` |
| `<config-path>` | the service config, normally `configs/m6-auth.conf` |
| `--log-level <level>` | overrides the configured log level for this run |
| `--dump-config` | prints the parsed configuration and the routes, then exits |
| `--version` or `-V` | prints the binary name and version, then exits |

**Table 2: the two positional arguments and the three flags.**

A missing positional argument prints the usage line and exits 2.

The socket path comes from the config file's stem, so `configs/m6-auth.conf` becomes `/run/m6/m6-auth.sock`.
The directory `/run/m6` is created at startup, and a stale socket file at that path is removed before the bind.

The service logs to stdout.
The default format is *JSON* (JavaScript Object Notation), and `[log] format` in `site.toml` can select text.
Logging is core's, so every m6 service writes the same lifecycle lines.
Section 11.3 lists the lines this service adds.

The config path decides the socket name and the storage base.
The site directory decides the key paths and the default `site.toml`.
Section 4 gives both rules, because the two roots differ.

## 4. Configuration

The service's own configuration is one *TOML* (Tom's Obvious, Minimal Language) file.
Table 3 gives its keys, including the two *PEM* (Privacy-Enhanced Mail) key paths.
Take from it that `[storage]` and `[keys]` are the required sections, and that the token and rate-limit defaults make a minimal config work.

| section | key | type | default | notes |
|---|---|---|---|---|
| `[storage]` | `path` | string | required | SQLite database path, resolved against the directory holding this config file |
| `[tokens]` | `access_ttl` | integer | `900` | access token lifetime in seconds |
| `[tokens]` | `refresh_ttl` | integer | `2592000` | refresh token lifetime in seconds |
| `[tokens]` | `issuer` | string | `[site] domain`, then `localhost` | the `iss` claim |
| `[keys]` | `private_key` | string | required | path to the PEM private key, resolved against the site directory |
| `[keys]` | `public_key` | string | required | path to the PEM public key, resolved against the site directory |
| `[rate_limit]` | `max_attempts` | integer | `5` | failed logins allowed per address per window |
| `[rate_limit]` | `window_secs` | integer | `900` | length of the failure window in seconds |

**Table 3: the config keys, their defaults, and the two different path rules.**

The `[storage]` rule and the `[keys]` rule are the pair to read twice.
One is relative to the config file's directory, and the other is relative to the site directory.

A config at `configs/m6-auth.conf` with `path = "data/auth.db"` opens `<site>/configs/data/auth.db`.
To put the database at `<site>/data/auth.db`, use `path = "../data/auth.db"`.
Both rules are in `m6-auth-server/src/config.rs`.

The `issuer` falls back to `[site] domain` in the `site.toml` that the optional `site_toml` key names.
When that file has no domain, the fallback is `localhost`.
The `site_toml` key is core's, and it defaults to `<site-dir>/site.toml`.

A complete config:

```toml
# configs/m6-auth.conf

[storage]
path = "../data/auth.db"          # resolved against configs/, so this is <site>/data/auth.db

[tokens]
access_ttl  = 900                 # 15 minutes
refresh_ttl = 2592000             # 30 days
issuer      = "example.com"

[keys]
private_key = "/etc/m6/auth.pem"  # resolved against <site-dir>, and an absolute path is used as is
public_key  = "/etc/m6/auth.pub"

[rate_limit]
max_attempts = 5                  # failed logins per address
window_secs  = 900                # 15 minutes
```

### 4.1. Keys and paths

The private key signs every token and the public key verifies them.
The service reads both files at startup and keeps them in memory.
Both paths resolve against the site directory, so `keys/auth.pem` in a config at any path means `<site>/keys/auth.pem`.
An absolute path is used unchanged.

The pair is checked at load.
The service signs a probe token with the private key and verifies it with the public key.
A pair that does not match stops startup with exit 2, and on a reload the running pair stays.
Section 4.2 covers what a rotation does to tokens already issued.

### 4.2. Key rotation

A rotation writes both key files, then touches the service config.
The service reads both files, checks the pair, and swaps the new pair in.
A key that fails to load leaves the running pair serving, and core retries the reload after a delay and again on the next touch.

A successful rotation changes the signing key immediately.
m6-http keeps the public key it loaded, so tokens signed with the new key get 401 until m6-http reloads.
Touch m6-http's system config after the service config, and m6-http reads the new public key.
From then on, access tokens signed with the old key get 401, and they expire naturally within `access_ttl`.

A rotation also invalidates outstanding refresh tokens.
The service verifies a refresh token's signature with the current key before it checks the SHA-256 hash in the database.
A token signed with the old key fails at the next refresh, so a rotation ends every existing session at its next refresh.

The service reads the database path, the token lifetimes, the issuer and the rate limit once at startup.
A change to any of them after startup is logged with the text "restart m6-auth-server to apply it", and the running value stays.

### 4.3. Key generation

An *EC* (elliptic curve) P-256 key produces ES256 tokens, and an *RSA* (Rivest-Shamir-Adleman) key produces RS256 tokens.
The service picks the algorithm from the private key at load, so the key type is the only choice.

An EC key must be PKCS#8, whose PEM tag is `PRIVATE KEY`.
`openssl ecparam` writes a SEC1 key tagged `EC PRIVATE KEY`, and the signing library refuses that tag at startup.
Convert the SEC1 key to PKCS#8 before use, and keep the embedded public key that the signing library needs.
Choose one of the two blocks.

```sh
# EC (ES256), recommended: smaller tokens and faster verification
openssl ecparam -name prime256v1 -genkey -noout -out auth-sec1.pem
openssl pkcs8 -topk8 -nocrypt -in auth-sec1.pem -out auth.pem
openssl ec -in auth-sec1.pem -pubout -out auth.pub
rm auth-sec1.pem

# RSA (RS256), wider compatibility
openssl genrsa -out auth.pem 2048
openssl rsa -in auth.pem -pubout -out auth.pub

chmod 600 auth.pem
```

The same recipe is in `m6-auth-cli/tests/integration.rs`, which generates a key pair for the token tests.
The config's `[keys]` paths name the two files it produces.
The conversion matters: a PKCS#8 key written directly by LibreSSL's `genpkey` can omit the embedded public key and fail at signing, which is why the test goes through SEC1.

Section 4 gave the config surface, the path rules, the rotation steps and the key formats.
Section 5 turns to the HTTP API, where those keys produce tokens.

## 5. HTTP API

The service exposes four routes over *HTTP* (Hypertext Transfer Protocol), and the content type decides the response shape.
The four routes form the service's *API* (application programming interface).
This section gives each route's request and response, beginning with login.

### 5.1. Login

`POST /auth/login` accepts a form body or a JSON body, chosen by `Content-Type`.
A request with `application/json` takes the JSON path.
A request with `application/x-www-form-urlencoded`, or any other content type, takes the form path.

A form login sends `username`, `password` and an optional `next`.
On success the service answers 302 with two cookies and `Location` set to `next`.
On a wrong password it answers 302 to `/login?error=invalid&next=<encoded next>`.
On a blocked address it answers 429 with `Retry-After: 60`.

Table 4 gives the form responses.
Take from it that a browser sees a redirect for every login outcome, so the login page stays in control.

| condition | status | body or headers |
|---|---|---|
| valid credentials | 302 | `Location: <next>`, `Set-Cookie: session=...`, `Set-Cookie: refresh=...` |
| wrong username or password | 302 | `Location: /login?error=invalid&next=<encoded next>` |
| blocked address | 429 | `Retry-After: 60`, body `Too Many Requests` |

**Table 4: the three form login outcomes.**

The two cookies carry `HttpOnly`, `Secure`, `SameSite=Lax` and a `Max-Age` from the token lifetime.

The session cookie has `Path=/` and `Max-Age=<access_ttl>`.
The refresh cookie has `Path=/auth/refresh` and `Max-Age=<refresh_ttl>`.
The `next` value must be a same-origin path: it starts with `/` and does not start with `//` or `/\`.
The service falls back to `/` when `next` is absent, off-site or malformed.

A *JSON* (JavaScript Object Notation) login sends `{"username": "...", "password": "..."}`.
On success it answers 200 with the tokens in the body and sets no cookies.
Table 5 gives the JSON responses.
Take from it that a JSON client owns its tokens and a browser lets the cookies do the work.

| condition | status | body |
|---|---|---|
| valid credentials | 200 | `{"access_token": "...", "refresh_token": "...", "expires_in": 900}` |
| malformed body | 400 | `{"error":"bad_request"}` |
| wrong username or password | 401 | `{"error":"invalid_credentials"}` |
| blocked address | 429 | `{"error":"rate_limited"}` |

**Table 5: the four JSON login outcomes.**

`expires_in` is `access_ttl` in seconds, and the refresh token is the same token the form path puts in its cookie.

The login stores the refresh token's SHA-256 hash with an expiry of `now + refresh_ttl`, and clears the address's failure count after a correct password.
The access token carries the user's groups and roles as they are at login.

### 5.2. Refresh

`POST /auth/refresh` issues a new access token from a refresh token, and it leaves the refresh token in place.
A browser sends the `refresh` cookie, and a JSON client sends `{"refresh_token": "..."}`.

A browser refresh answers 302.
On success the `Location` is the `Referer` when that value is a same-origin path, and `/` otherwise.
The response sets a new session cookie.
A missing, invalid or expired refresh token answers 302 to `/login`.

Table 6 gives the JSON refresh responses.
Take from it that a failed refresh is a 401 and a bad body is a 400, so a JSON client can tell the two apart.

| condition | status | body |
|---|---|---|
| valid refresh token | 200 | `{"access_token": "...", "expires_in": 900}` |
| malformed body | 400 | `{"error":"bad_request"}` |
| invalid or expired token | 401 | `{"error":"invalid_token"}` |

**Table 6: the three JSON refresh outcomes.**

The service checks the token's signature, issuer and expiry.
It then matches the SHA-256 hash to the `refresh_tokens` row and loads the user for current groups and roles.

A refresh token is a JWT with `type` set to `refresh`.
The service does not rotate it on refresh, so the same refresh token works until it expires.
Section 6 gives both token shapes.

### 5.3. Logout

`POST /auth/logout` revokes refresh tokens.
A browser sends the `refresh` cookie, and the service deletes that token's row, clears both cookies and answers 302 to `/`.
A JSON client sends `Authorization: Bearer <access token>`, and the service deletes every refresh token for that user and answers 204.

The browser path does not require a valid session.
The JSON path decodes the access token to find the user.
An invalid bearer token still gets 204, and the service revokes nothing.

Access tokens stay valid until their `exp`.
The service keeps no access-token revocation list, so logout stops the next refresh and leaves the current access token alive for its remaining lifetime.

### 5.4. Public key

`GET /auth/public-key` returns the current public key as a PEM body with `Content-Type: application/x-pem-file`.
The route is unauthenticated, and it serves the key that is loaded, so a rotation changes the body after the service reloads.

m6-http reads the public key from the file named by `[auth] public_key`.
It does not call this route, and the route exists for any other client that needs the key without filesystem access.

Login, refresh, logout and the public key are the whole HTTP surface.
Section 6 gives the token contents those routes produce, and section 7 gives the m6-http checks that consume them.

## 6. Tokens

The service issues two kinds of *JWT* (JSON Web Token): an access token for a request and a refresh token for a new access token.
Table 7 gives the claims.
Take from it that the access token carries the identity, groups and roles, and the refresh token carries enough to find the user.

| claim | access token | refresh token | meaning |
|---|---|---|---|
| `iss` | yes | yes | issuer, from `[tokens] issuer` |
| `sub` | yes | yes | user id |
| `exp` | yes | yes | expiry, in seconds since the Unix epoch |
| `iat` | yes | yes | issue time, in seconds since the Unix epoch |
| `username` | yes | yes | login name |
| `groups` | yes | no | group names at issue time |
| `roles` | yes | no | role names at issue time |
| `type` | no | yes | the string `refresh` |

**Table 7: the claims in each token.**

The `type` claim is the marker that keeps a refresh token out of an access-token check.

The algorithm follows the key: an EC P-256 key gives ES256, and an RSA key gives RS256.
The `alg` header is set from the key at startup.
m6-http accepts either algorithm and picks the decoder from the public key.

m6-http verifies the signature and the `exp` claim.
The service checks `iss` when it decodes its own tokens, on refresh and on the JSON logout path.
The `iss` claim reaches m6-http as data and does not decide whether a request is served.

The access token's groups and roles are a snapshot.
A role change takes effect at the next login or refresh, and an outstanding access token keeps its old claims until it expires.

The default lifetimes are 900 seconds for an access token and 2,592,000 seconds for a refresh token.
The refresh token is stored as a SHA-256 hash, and the raw token is never written to the database.

The two token shapes are the contract between this service and every verifier.
Section 7 gives the verifier in m6-http, and section 8 gives the claims a backend receives.

## 7. m6-http integration

m6-http verifies access tokens and enforces route `require` rules.
This section gives the configuration and the five outcomes, beginning with the `[auth]` section that supplies the verification key.

```toml
[auth]
backend    = "m6-auth"              # optional, checked when given, unused by verification
public_key = "/etc/m6/auth.pub"     # the file m6-http reads at startup

[[backend]]
name    = "m6-auth"
sockets = "/run/m6/m6-auth-*.sock"

[[route]]
path    = "/auth/login"
backend = "m6-auth"

[[route]]
path    = "/auth/logout"
backend = "m6-auth"

[[route]]
path    = "/auth/refresh"
backend = "m6-auth"

[[route]]
path    = "/admin/{page}"
backend = "m6-html"
require = "group:editors"
```

The `[auth] public_key` file is the public key that matches the service's private key.
A relative path resolves against the site directory.
A route with `require` and no `[auth]` stops m6-http at startup.
`[auth]` in both `site.toml` and the system config is also a startup error.

m6-http reads the token from `Authorization: Bearer <token>` or from the `session` cookie, and the header wins when both are present.
The check runs only on a route that declares `require`.
A response on such a route is never cached, because the cache key has no identity.

Table 8 gives the outcomes.
Take from it that a browser without a token gets a redirect and every other refusal is a status code.

| condition | response |
|---|---|
| no token, browser request | 302 to `/auth/refresh` with a `refresh` cookie, otherwise 302 to `/login?next=<path>` |
| no token, other client | 401 with `WWW-Authenticate: Bearer` |
| token present, invalid or expired | 401 with `WWW-Authenticate: Bearer` |
| token valid, claim missing | 403 |
| token valid, claim present | request forwarded with `X-Auth-Claims` |

**Table 8: the five outcomes of a route with `require`.**

The browser row is the only redirect, and it depends on the `Accept` header containing `text/html`.

`require` takes `group:<name>` or `role:<name>`.
m6-http checks the `groups` or `roles` claim for that name.
Any other form denies the request.

The `X-Auth-Claims` header is base64-encoded JSON of the verified claims.
m6-http removes any inbound `X-Auth-Claims` header before routing, so a client cannot supply its own.
Section 8 gives the dictionary keys a backend reads from it.

The edge check is local and the header carries the result.
Section 8 shows what a backend does with that result.

## 8. Renderer integration

A backend receives the verified claims in the `X-Auth-Claims` header, and m6 turns them into request-dictionary keys.
Table 9 gives the keys.
Take from it that a backend reads a name, an id, a group list and a role list from the same request.

| key | value | source claim |
|---|---|---|
| `auth_username` | string | `username`, or `sub` when `username` is absent |
| `auth_sub` | string | `sub` |
| `auth_groups` | array of strings | `groups` |
| `auth_roles` | array of strings | `roles` |

**Table 9: the request-dictionary keys built from verified claims.**

The keys appear only on a route with `require` and a token that passed the check.

A template or a Rust renderer reads the keys like any other dictionary entry:

```rust
let username = req["auth_username"].as_str().unwrap_or("");
let is_editor = req["auth_groups"]
    .as_array()
    .map(|g| g.iter().any(|v| v.as_str() == Some("editors")))
    .unwrap_or(false);
```

The keys are absent on an unauthenticated request and on a route without `require`, even when the request carries a valid token.
A backend that needs a claim on every request should declare `require` on the route.

Token verification, the edge check and the backend keys are one chain.
Section 9 covers the database that holds the users the chain begins with.

## 9. Storage

The service stores users, groups, memberships and refresh tokens in *SQLite*.
The `m6-auth` library owns the schema, the password hashes and every query.
The service opens one connection at startup through `m6_auth::Db::open`.

`Db::open` sets *WAL* (write-ahead logging), a 5-second busy timeout and foreign keys.
WAL lets readers and one writer proceed together.
A second writer waits up to 5 seconds and then fails.
The server holds its one connection behind a mutex, so its own requests take turns.

The database path is the `[storage] path` key, resolved against the config file's directory as section 4 states.
The service creates the parent directory when it is missing.
`m6-auth-cli` and a custom renderer use the same library and the same file, so a user created by the CLI is visible at the next login.

The database is the shared state.
Section 10 covers the one piece of state the service keeps in memory.

## 10. Rate limiting

The login route limits failed passwords by client address.
The default is 5 failures in 900 seconds.
Table 10 gives the two keys and their defaults.
Take from it that the throttle is configuration, and a restart clears it.

| key | default | meaning |
|---|---|---|
| `[rate_limit] max_attempts` | `5` | failed logins allowed per address per window |
| `[rate_limit] window_secs` | `900` | length of the window in seconds |

**Table 10: the rate-limit keys.**

`window_secs = 0` makes every count expire at once, which turns the throttle off.

Only a failed credential check counts.
A correct password clears the address's count.
A blocked address gets 429 with `Retry-After: 60` before the password is checked.
The window starts at the first failure and does not slide on each later failure.

The service listens on a Unix socket, so it takes the value of `X-Forwarded-For`, then `X-Real-IP`.
When neither is present, the address is the string `unix`.
m6-http sets the forwarded header and strips any client-supplied copy.
A direct caller can set the header, so the service's socket must be reachable only by m6-http.

The counts live in memory.
A restart forgets them, and a change to either key takes effect at the next restart.
The source comment says that brute force slow enough to survive restarts is left to the firewall.

The throttle bounds one route and one address at a time.
Section 11 covers the process around it.

## 11. Lifecycle

The service uses the core `App` lifecycle: one process, a bounded worker pool, a Unix socket and a signal handler.
This section gives the signals, the exit codes and the log lines, beginning with the signals.

### 11.1. Signals

The first `SIGTERM` or `SIGINT` stops accepting, drains in-flight requests, removes the socket and exits 0.
A second signal removes the socket and exits 0 at once.
`SIGUSR1` reopens the log file and never counts toward shutdown.

Table 11 gives the three signals.
Take from it that the first two signals differ in how much work they finish, and both exit 0.

| signal | first receipt | later receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | drain in-flight requests, remove the socket, exit 0 | remove the socket and exit 0 at once |
| `SIGUSR1` | reopen the log file | reopen it again |

**Table 11: the signals the service handles.**

`SIGUSR1` is the log-rotation signal and does not start a shutdown.

### 11.2. Exit codes

Table 12 gives the exit codes.
Take from it that a configuration fault fails the start, and a request fault becomes an HTTP status.

| code | meaning |
|---|---|
| `0` | clean shutdown, `--version`, or a successful `--dump-config` |
| `1` | runtime error, including a failure to start logging |
| `2` | configuration or invocation error: a missing argument, a config that does not parse, a key that does not load, a key pair that does not match, a database that cannot be opened, or a socket that cannot be bound |

**Table 12: the process exit codes.**

A request that fails after startup returns a 4xx or 5xx response, and the process keeps running.

### 11.3. Logging

The service writes one line per event to stdout, in JSON by default.
It logs the startup configuration, the route outcomes, key reloads and the core lifecycle lines.
Table 13 gives the events.
Take from it that every login attempt is recorded with the address and every failure with a reason.

| event | level | fields |
|---|---|---|
| config loaded | info | `issuer` |
| signing keys reloaded | info | none |
| login success | info | `username`, `ip` |
| login failure | warn | `ip`, `reason` |
| rate limit exceeded | warn | `ip` |
| token refresh | info | `user_id` |
| logout | info | `user_id` for the bearer path |
| started, shutdown signal received, shutdown complete | info | service name, socket on start |

**Table 13: the log events this service adds to the core lifecycle lines.**

The service never logs a password, a token or key material.

A change to `[storage] path`, a token lifetime, the issuer or a rate-limit key is logged at warn level.
The line says "restart m6-auth-server to apply it", and the running value stays.

Signals, exit codes and logs are the process contract.
Section 12 states what the service does not do.

## 12. Limits

The service covers password login, token refresh, logout and key publication.
It does not implement the features in this list, and each one is a scope boundary:

- OAuth 2.0 or OpenID Connect provider.
- *SAML* (Security Assertion Markup Language).
- Social login.
- Multi-factor authentication.
- Passkeys or WebAuthn.
- Password reset by email, which a custom renderer can add with the *SMTP* (Simple Mail Transfer Protocol) support in core and the `m6-auth` library.

A feature that changes the token contract belongs in this service.
A feature that builds on the existing tokens belongs in a renderer.
The six items above are the scope, and section 13 collects the documents that own the details this page leaves out.

## 13. Summary and further reading

`m6-auth-server` turns a password into a short-lived access token and a refresh token, and m6-http verifies the access token without a network call.
The service has four routes, one config file and one SQLite database.
The rotation steps and the two path rules are in section 4.

Table 14 gives the documents that own what this page leaves out.
Take from it that every number and every contract has a file responsible for it.

| document | covers |
|---|---|
| [`m6-auth-lib.md`](m6-auth-lib.md) | the `m6-auth` library: schema, password hashing, user and group operations |
| [`m6-auth-cli.md`](m6-auth-cli.md) | the administrative CLI: users, groups and API tokens |
| [`m6-site-toml.md`](m6-site-toml.md) | `[auth]`, `[[backend]]`, `[[route]]` and `require` |
| [`m6-http.md`](m6-http.md) | m6-http internals, including the edge check and the cache |
| [`m6-render-lib.md`](m6-render-lib.md) | the request dictionary and the renderer API |
| [`m6-architecture.md`](m6-architecture.md) | why the edge verifies tokens and why the header is stripped |
| `m6-auth-server/Cargo.toml` | the crate's dependencies and features |

**Table 14: further reading, and what each document covers.**

The first three are the pages a deployment reads beside this one.

A change to those routes or claims changes this page.
The rest of the page is the detail behind them.
