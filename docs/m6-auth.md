# m6-auth-server

`m6-auth-server` is the authentication service in m6.
It checks a username and password, issues *JWT* (JSON Web Token) access and refresh tokens, renews an access token, revokes a refresh token, and publishes the signing public key.
m6-http asks this service for the identity on a protected route.

This page is the reference for the service binary.
The library crate has its own page in [`m6-auth-lib.md`](m6-auth-lib.md), the administrative tool in [`m6-auth-cli.md`](m6-auth-cli.md), and the route and `[auth]` configuration in [`m6-site-toml.md`](m6-site-toml.md).
The crate manifest is `m6-auth-server/Cargo.toml`.

Auth is optional.
A site whose routes declare no `require` and whose configuration declares no `[auth]` runs no token check and needs neither this service nor a signing key.
Section 2 states what the service owns, and section 13 lists the documents that cover the rest.

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

`m6-auth-server` owns five routes on two Unix sockets: login, refresh, logout and the public key on the primary socket, and check on the check socket.
Table 1 gives them.
Take from it that every route is a literal path, so the service has no route configuration.

| method | path | listener | what it does | success |
|---|---|---|---|---|
| `POST` | `/auth/login` | primary | checks a password and issues an access token and a refresh token | 302 with cookies for a form client, 200 with JSON for a JSON client |
| `POST` | `/auth/refresh` | primary | issues a new access token from a refresh token | 302 with a session cookie for a form client, 200 with JSON for a JSON client |
| `POST` | `/auth/logout` | primary | clears the browser's cookies, and revokes refresh tokens on the bearer path | 302 for the browser path, 204 for the bearer path |
| `GET` | `/auth/public-key` | primary | returns the current public key | 200 with a PEM body |
| `POST` | `/check` | check | reads the current identity for an access token from the database | 200 with identity JSON |

**Table 1: the five routes, the listener each answers on, what each does, and what success returns.**

A `GET` on login, refresh or logout matches no route.

### 2.1. The two listeners

The primary socket serves the four login routes.
The check socket serves `/check`.
A route requested on the other socket answers 404, and `POST /genid` on the check socket answers 404.

The check socket path is derived from the primary path: the same directory, the same file name, and the prefix `check-`.
`/run/m6/m6-auth.sock` gives `/run/m6/check-m6-auth.sock`, so the login glob `/run/m6/m6-auth*.sock` matches the primary socket only.
`M6_CHECK_SOCKET_OVERRIDE` sets the check path outright, and `M6_SOCKET_OVERRIDE` sets the primary path, for a test that cannot write to `/run/m6`.

Two flags select the listeners.
With neither flag, one process binds both sockets.
`--login-only` binds the primary socket and starts no check listener.
`--check-only` binds the check socket and starts no login listener.
Both flags together stop the start with exit 2, naming both.

Both sockets of a one-process start feed one event loop and one connection table.
A `/check` that arrives during a login waits for that login's bcrypt verify (cost 12).
Two processes, one per flag, have two loops, and the check loop runs no bcrypt because `/auth/login` is absent from its routes.
`m6-user-guide.md` section 7.4 gives the deployment that runs them as two units.

The two processes load different material.
A process that binds the primary socket signs tokens and writes users, so it reads `[keys] private_key`, applies the auth schema and opens the database read-write.
A process started with `--check-only` decodes a token and reads users, so it reads only `[keys] public_key`, applies no schema and opens the database through `m6_auth::Db::open_read_only`, which sets `PRAGMA query_only` so every write fails.
The login side creates the database and schema once, and a check process with a missing file or schema stops at start with exit 2.
Deliver `auth.pub` to the check unit and both key files to the login unit.

The service owns the signing keys, the token lifetimes, the rate limiter and one database connection, all built at start.
`m6-auth-cli` (the *CLI*, command-line interface) writes the users and groups, and the service reads them at login, refresh and check.

m6-http forwards a login, refresh or logout request to the service over the primary Unix socket.
A Unix socket supplies no client address, so section 10 gives the address the rate limiter uses.

## 3. Command line

`m6-auth-server` takes a site directory and a config path.
Table 2 gives the arguments and flags.
Take from it that the two positional arguments are required and every flag is optional.

| argument or flag | meaning |
|---|---|
| `<site-dir>` | the site root, used to find the default `site.toml` |
| `<config-path>` | the service config, normally `configs/m6-auth.conf` |
| `--log-level <level>` | overrides the configured log level for this run |
| `--dump-config` | prints the site directory, config paths, socket, server settings and routes, validates the auth tables and the key files, then exits |
| `--version` or `-V` | prints the tag form, `v1.16.0-rc.1`, then exits |
| `--version-full` | prints one `<crate> - <full SemVer>` line per m6 crate in the binary, starting with `m6-auth-server`, then exits |
| `--login-only` | binds the primary socket and serves the four login routes |
| `--check-only` | binds the check socket and serves `/check` |

**Table 2: the two positional arguments and the flags.**

The dump prints the settings core owns, and it validates `[storage]`, `[tokens]`, `[keys]` and `[rate_limit]` with the code the start runs, including the key files, so a config the start refuses exits 2 here too.

A missing positional argument prints the usage line and exits 2.

The primary socket path comes from the config file's stem, so `configs/m6-auth.conf` gives `/run/m6/m6-auth.sock`.
The check socket takes the `check-` prefix beside it, so the same config gives `/run/m6/check-m6-auth.sock`.
The bind creates a missing parent directory and replaces a stale socket file at the path.

The service writes *JSON* (JavaScript Object Notation) lines to stdout, or to journald's native socket under systemd, and `[log] level` in `site.toml` sets how much it writes.
Section 11.3 lists the lines this service adds to core's lifecycle lines.

## 4. Configuration

The service's own configuration is one *TOML* (Tom's Obvious, Minimal Language) file.
Table 3 gives its keys, including the two *PEM* (Privacy-Enhanced Mail) key paths.
Take from it that `[storage]` and `[keys]` are the required sections, that the issuer must come from the config or from `[site] domain`, and that the lifetimes, audience and rate limit have defaults.

| section | key | type | default | notes |
|---|---|---|---|---|
| `[storage]` | `path` | string | required | SQLite database path, absolute |
| `[tokens]` | `access_ttl` | integer | `900` | access token lifetime in seconds |
| `[tokens]` | `refresh_ttl` | integer | `2592000` | refresh token lifetime in seconds |
| `[tokens]` | `issuer` | string | `[site] domain` | the `iss` claim. The start stops with exit 2 when neither names one |
| `[tokens]` | `audience` | string | the issuer | the `aud` claim |
| `[keys]` | `private_key` | string | required on the login side | path to the PEM private key, absolute. A `--check-only` process does not read it and may leave it out |
| `[keys]` | `public_key` | string | required | path to the PEM public key, absolute |
| `[rate_limit]` | `max_attempts` | integer | `5` | failed logins allowed per address per window |
| `[rate_limit]` | `window_secs` | integer | `900` | length of the failure window in seconds |

**Table 3: the config keys and their defaults.**

Every path in this file is absolute.
A relative path stops the service at startup with exit 2, and the error names the setting (`secret_path`, `m6-core/src/config.rs:553`).

The `issuer` falls back to `[site] domain` in the `site.toml` that the optional `site_toml` key names.
When neither the config nor that file names one, the start stops with exit 2 and the error names `[tokens] issuer`.
The `site_toml` key is core's, and it defaults to `<site-dir>/site.toml`.

The audience is the issuer when `[tokens] audience` is unset.
A token carries its `aud`, so a deployment with another audience refuses it even when the two share the signing key.

The service reads this file once at startup, and a change takes a restart.

A complete config:

```toml
# configs/m6-auth.conf, the login unit's copy

[storage]
path = "/var/lib/m6-auth/auth.db"

[tokens]
access_ttl  = 900                 # 15 minutes
refresh_ttl = 2592000             # 30 days
issuer      = "example.com"

[keys]
private_key = "/run/credentials/m6-auth-login.service/auth.pem"
public_key  = "/run/credentials/m6-auth-login.service/auth.pub"

[rate_limit]
max_attempts = 5                  # failed logins per address
window_secs  = 900                # 15 minutes
```

`m6-user-guide.md` section 7.2 sets out the `LoadCredential` and `StateDirectory` lines that supply the keys and the database directory.
The check unit's config at `/etc/m6/check/m6-auth.conf` is this file with `private_key` removed and the login unit name in `public_key` replaced by the check unit name.

### 4.1. Keys and paths

The private key signs every token and the public key verifies them.
The service reads both files at startup and keeps them in memory.
The login side signs a probe token with the private key and verifies it with the public key, and a pair that does not match stops startup with exit 2.
Section 4.2 covers what a rotation does to tokens already issued.

### 4.2. Key rotation

A rotation has three steps: write the new key pair in `/etc/m6`, run `m6-auth-cli <config> sessions revoke-all`, and restart m6-auth-server and m6-http.
systemd copies a credential when the unit starts, so each restart delivers the new files.

The service signs with the new key from the restart.
An access token signed with the old key fails its signature check, so `/check` answers 401 at the next request.

`sessions revoke-all` ends every refresh token by deleting every `refresh_tokens` row.
The refresh route matches the SHA-256 hash of the token to a row, so a deleted row refuses the next refresh.
A key change alone leaves a refresh token usable until its `exp`, because the refresh route never checks a signature (section 5.2).
The command is therefore the step that ends a stolen refresh token.

Section 4.3 gives the key formats and the commands that generate them.

### 4.3. Key generation

An *EC* (elliptic curve) P-256 key produces ES256 tokens, and an *RSA* (Rivest-Shamir-Adleman) key produces RS256 tokens.
The service picks the algorithm from the private key at load.

An EC private key is PKCS#8, whose PEM tag is `PRIVATE KEY`.
`openssl ecparam` writes a SEC1 key tagged `EC PRIVATE KEY`, so the block below converts it with `openssl pkcs8`.
Choose one of the two blocks.

```sh
umask 077   # every file below is created readable by the current user only

# EC (ES256), recommended: smaller tokens and faster verification
openssl ecparam -name prime256v1 -genkey -noout -out auth-sec1.pem
openssl pkcs8 -topk8 -nocrypt -in auth-sec1.pem -out auth.pem
openssl ec -in auth-sec1.pem -pubout -out auth.pub
rm auth-sec1.pem

# RSA (RS256), wider compatibility
openssl genrsa -out auth.pem 2048
openssl rsa -in auth.pem -pubout -out auth.pub

chmod 644 auth.pub   # optional: m6-http may run as another user
```

The private key file is readable by the user the service runs as and by no one else, and the `umask 077` gives that from the moment of creation.
`m6-auth-cli/tests/integration.rs:30` uses the same SEC1 route because a PKCS#8 key that LibreSSL's `genpkey` writes directly can omit the embedded public key and fail at signing.
The config's `[keys]` paths name the two files the block produces.

Section 5 turns to the HTTP API, where these keys produce tokens.

## 5. HTTP API

The service's *API* (application programming interface) is the five routes of Table 1 over *HTTP* (Hypertext Transfer Protocol).
The content type of a login or refresh request decides the response shape.
This section gives each route's request and response, beginning with login.

### 5.1. Login

`POST /auth/login` accepts a form body or a JSON body, chosen by `Content-Type`.
A `Content-Type` containing `application/json` takes the JSON path.
Any other content type takes the form path.

A form login sends `username`, `password` and an optional `next`.

Table 4 gives the form responses.
Take from it that both credential outcomes are redirects and a blocked address gets a 429.

| condition | status | body or headers |
|---|---|---|
| valid credentials | 302 | `Location: <next>`, `Set-Cookie: session=...`, `Set-Cookie: refresh=...` |
| wrong username or password | 302 | `Location: /login?error=invalid&next=<encoded next>` |
| blocked address | 429 | `Retry-After: <seconds left in the window>`, body `Too Many Requests` |

**Table 4: the three form login outcomes.**

Both cookies carry `HttpOnly`, `Secure` and `SameSite=Lax`.
The session cookie has `Path=/` and `Max-Age=<access_ttl>`.
The refresh cookie has `Path=/auth/refresh` and `Max-Age=<refresh_ttl>`.
A `next` value passes when it starts with `/` and its second byte is neither `/` nor `\` (`is_same_origin_path`, `m6-core/src/http.rs:249`).
A passing value is the `Location`, and `/` is the `Location` when `next` is absent or fails.

A *JSON* (JavaScript Object Notation) login sends `{"username": "...", "password": "..."}`.
On success it answers 200 with the tokens in the body and sets no cookies.
Table 5 gives the JSON responses.
Take from it that each outcome has its own status and error code.

| condition | status | body |
|---|---|---|
| valid credentials | 200 | `{"access_token": "...", "refresh_token": "...", "expires_in": 900}` |
| malformed body | 400 | `{"error":"bad_request"}` |
| wrong username or password | 401 | `{"error":"invalid_credentials"}` |
| blocked address | 429 | `{"error":"rate_limited"}` |

**Table 5: the four JSON login outcomes.**

`expires_in` is `access_ttl` in seconds.
A correct password clears the address's failure count and stores the refresh token's SHA-256 hash with an expiry of `now + refresh_ttl`.
The access token carries the user's groups and roles as they are at login.

### 5.2. Refresh

`POST /auth/refresh` issues a new access token from a refresh token and leaves the refresh token valid.
A browser sends the `refresh` cookie, and a JSON client sends `{"refresh_token": "..."}`.

A browser refresh answers 302.
On success the `Location` is the `Referer` when that value passes the same two-byte check as `next`, and `/` otherwise.
The response sets a new session cookie.
A missing, invalid or expired refresh token answers 302 to `/login`.

Table 6 gives the JSON refresh responses.
Take from it that a failed refresh is a 401 and a malformed body is a 400.

| condition | status | body |
|---|---|---|
| valid refresh token | 200 | `{"access_token": "...", "expires_in": 900}` |
| malformed body | 400 | `{"error":"bad_request"}` |
| invalid or expired token | 401 | `{"error":"invalid_token"}` |

**Table 6: the three JSON refresh outcomes.**

The service matches the SHA-256 hash of the token to the `refresh_tokens` row, which carries the expiry, and loads the user for current groups and roles.
The route verifies no signature, so a key change alone leaves a refresh token usable.
The refresh token stays the same across refreshes and works until its expiry or a revoke.
Section 6 gives both token shapes.

### 5.3. Logout

`POST /auth/logout` clears the browser's cookies, and a bearer request revokes refresh tokens.
The browser path needs no valid session and answers 302 to `/` with both cookies cleared.
A browser sends no `refresh` cookie to this path because that cookie's `Path` is `/auth/refresh`, so a browser logout revokes no refresh row and the refresh token stays valid until its `exp`.
A JSON client sends `Authorization: Bearer <access token>`.
The service decodes the access token, deletes every refresh token of that user and answers 204.
An invalid bearer token also gets 204 and revokes nothing.

An access token stays valid at the service until its `exp`, and the service keeps no access-token revocation list.

### 5.4. Public key

`GET /auth/public-key` returns the current public key as a PEM body with `Content-Type: application/x-pem-file`.
The route is unauthenticated and serves the loaded key, so a rotation changes the body after the restart.
Any client that needs the key without filesystem access reads it here.

### 5.5. Check

`POST /check` answers the current identity for an access token.
It takes `Authorization: Bearer <access token>` and returns `sub`, `username`, `roles` and `groups` read from the database.
The 200 carries `Cache-Control: no-store`, and nothing caches the answer, so a role change or a revocation counts from the next request.
A route with `require` calls it, and the edge applies that route's requirement to the identity in the answer.

A missing, invalid or expired token is 401 `{"error":"unauthorised"}`.
A user who is absent from the database, including a deleted user, is the same 401.
A locked database is 503 `{"error":"unavailable"}` at once, because m6-http's 50 ms deadline is shorter than this service's one-second deferral.
The 401 refuses this request, and the 503 reports a database the service could not read.

Section 6 gives the token contents these routes produce, and section 7 gives the m6-http checks that consume them.

## 6. Tokens

The service issues two kinds of *JWT* (JSON Web Token): an access token for a request and a refresh token for a new access token.
`m6-auth-cli token create` mints a third kind, an API token.
Every token names its kind in the header `typ`: `at+jwt` for an access token, `m6-api+jwt` for an API token and `m6-refresh+jwt` for a refresh token.
A token with no `typ`, or any other value including the library's default `JWT`, is refused.
Table 7 gives the claims.
Take from it that the access token carries the identity, groups and roles, and the refresh token carries the user id and name.

| claim | access token | refresh token | meaning |
|---|---|---|---|
| `iss` | yes | yes | issuer, from `[tokens] issuer` |
| `aud` | yes | yes | audience, from `[tokens] audience`, the issuer when unset |
| `sub` | yes | yes | user id |
| `exp` | yes | yes | expiry, in seconds since the Unix epoch |
| `iat` | yes | yes | issue time, in seconds since the Unix epoch |
| `jti` | yes | yes | a fresh identifier for this mint |
| `username` | yes | yes | login name |
| `groups` | yes | no | group names at issue time |
| `roles` | yes | no | role names at issue time |
| `type` | no | yes | the string `refresh` |

**Table 7: the claims in each token.**

The `type` claim marks a refresh token for a reader that ignores the header.
`POST /check` checks the `typ` header, accepts `at+jwt` and `m6-api+jwt`, and refuses `m6-refresh+jwt` before it reads the claims.
The `jti` is new at every mint, and the service stores nothing by it.

The `alg` header follows the key: an EC P-256 key gives ES256, and an RSA key gives RS256.
`POST /check` and the JSON logout path check the signature, `typ`, `iss`, `aud` and `exp`, and require `iss`, `aud`, `sub`, `exp`, `iat` and `jti` to be present.
The leeway is zero, so a token is refused at the second `exp` names.

The groups and roles in an access token are a snapshot at issue time.
`POST /check` answers the database's current roles and groups.

Section 7 gives the m6-http configuration, and section 8 gives the identity a backend receives.

## 7. m6-http integration

m6-http asks this service for the identity and enforces `require` and the genid claims on the answer.
This section gives the one configuration that points m6-http at this service, and where the rest is specified.

```toml
[auth]
backend = "m6-auth-check"

[[backend]]
name = "m6-auth"
sockets = "/run/m6/m6-auth*.sock"

[[backend]]
name = "m6-auth-check"
sockets = "/run/m6/check-m6-auth*.sock"

[[route]]
path = "/auth/login"
backend = "m6-auth"

[[route]]
path = "/auth/logout"
backend = "m6-auth"

[[route]]
path = "/auth/refresh"
backend = "m6-auth"
```

The login glob matches the primary socket only, because the check socket's name begins with `check-`.
The same login backend serves one process or two.

`[auth] backend` names the check backend.
m6-http probes each member once at start: 401 or 503 passes, 404 stops the start, and a refused connection warns (`m6-http.md` startup step 8).
A route with `require` sends one `POST /check` per protected request under a 50 ms deadline.
No credential is 401 with no call, a 401 from this service is 401, an identity without the requirement is 403, and a check that cannot be made is 503 with `Retry-After: 1`.

The token sources, the `require` forms and the outcomes of a route with `require` are in [`m6-http.md`](m6-http.md) and [`m6-site-toml.md`](m6-site-toml.md).
Section 8 shows what a backend receives.

## 8. Renderer integration

A backend receives the identity in the `X-Auth-Claims` header, and m6 turns it into the `auth_*` request-dictionary keys.
The keys exist only on a route with `require` and an identity that passed the check, so a backend that needs a claim on every request declares `require` on the route.
The key names are in [`m6-core-reference.md`](m6-core-reference.md) section 4.6.

Section 9 covers the database that holds the users behind that identity.

## 9. Storage

The service stores users, groups, memberships and refresh tokens in *SQLite*.
The `m6-auth` library owns the schema, the password hashes and every query.
A process that binds the primary socket opens one connection through `m6_auth::Db::open` at start and keeps it for the life of the process.
A database that cannot be opened stops the service with exit 2, and the error names the path.

`Db::open` sets *WAL* (write-ahead logging), a 5-second busy timeout and foreign keys.
WAL lets readers and one writer proceed together.
The service sets the busy timeout to zero after that open, because a wait in the one loop holds every connection behind it.
A statement that meets another process's lock returns `SQLITE_BUSY` at once.
The loop defers that request and retries it every 5 ms while it serves the other connections.
A request still locked after one second is answered with 503 and `Retry-After: 1`.
A retried login keeps the user the first attempt verified, so it runs bcrypt once.
`m6-auth-cli` keeps the 5-second wait, because it serves one command at a time.

A login holds its process for its bcrypt verify, so every connection in that process waits.
A check-only process has no login route, so that wait never reaches `/check`.

The database path is the absolute `[storage] path`.
A missing parent directory stops the start with exit 2, and the error names the setting and the directory.
`m6-auth-cli` and a custom renderer use the same library and the same file, so a user created by the CLI is visible at the next login.

Section 10 covers the one piece of state the service keeps in memory.

## 10. Rate limiting

The login route limits failed passwords by client address.
Table 3 gives the two `[rate_limit]` keys, and the defaults are 5 failures in 900 seconds.
Take from it that both keys are configuration and a restart clears the counts.

`window_secs = 0` makes every count expire at once, which turns the throttle off.

Only a failed credential check counts.
A correct password clears the address's count.
A blocked address gets 429 with `Retry-After: <seconds left in the window>` before the password is checked.
The value is the whole seconds until the window ends, rounded up, so it is never below the true wait and never above `window_secs`.
The window is measured from the most recent failure, so five failures each inside the window use up the budget however long the series takes.
A block lifts after `window_secs` with no further failure.

The login listener is a Unix socket, so the service keys the count on the value of `X-Forwarded-For`, then `X-Real-IP`.
When neither is present, the address is the string `unix`.
m6-http strips any client-supplied `X-Forwarded-For` and sets one address in it, on HTTP/1.1 and HTTP/2, so behind m6-http every request has its own address.
A direct caller can set the header, so only m6-http may reach the service's socket.

The counts live in memory, and a restart forgets them.
Each login process keeps its own counts.
A check-only process never touches the limiter, because `/auth/login` is not on its listener.
Brute force slow enough to survive restarts is a firewall matter.
The tests `forwarded_addresses_select_the_rate_limit_bucket`, `requests_without_a_forwarded_address_share_one_bucket` and `a_restart_forgets_the_failure_counts` in `m6-auth-server/tests/integration.rs` pin these limits.
The unit test `failures_spread_across_more_than_one_window_exhaust_the_budget` in `m6-auth-server/src/rate_limit.rs` pins the window's start.

Section 11 covers the process around the limiter.

## 11. Lifecycle

Each process runs one event loop on one or two Unix sockets.
This section gives the signals, the exit codes and the log lines, beginning with the signals.

### 11.1. Signals

Table 8 gives the shutdown signals.
Take from it that the first signal answers what it already accepted, and a second forces the exit.

| signal | first receipt | second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | stop accepting, answer the accepted requests up to `[server] drain_timeout_s`, remove the sockets, exit 0 | remove the sockets, log `<name> shutdown forced`, exit 0 at once |

**Table 8: the shutdown signals and what each receipt does.**

### 11.2. Exit codes

Table 9 gives the exit codes.
Take from it that a configuration fault exits 2 at start.

| code | meaning |
|---|---|
| `0` | clean shutdown, `--version`, `--version-full`, or a successful `--dump-config` |
| `1` | runtime error, including a failure to start logging |
| `2` | configuration or invocation error: a missing argument, a config that does not parse, a relative path, a key that does not load, a key pair that does not match, a database that cannot be opened, or a socket that cannot be bound |

**Table 9: the process exit codes.**

A request that fails after startup returns a 4xx or 5xx response and the process keeps running.

### 11.3. Logging

The service writes one JSON line per event to its one log stream, which [`m6-log.md`](m6-log.md) describes.
Table 10 gives the events it adds to core's lifecycle lines.
Take from it that every login attempt is recorded with the address and every failure with a reason.

| event | level | fields |
|---|---|---|
| login success | info | `username`, `ip` |
| login failure | warn | `ip`, `reason` |
| rate limit exceeded | warn | `ip` |
| token refresh | info | `user_id` |
| logout | info | `user_id` on the bearer path, none on the browser path |

**Table 10: the log events this service adds to the core lifecycle lines.**

The service logs no password, token or key material.
Its start-up record writes `<redacted>` for `[keys] private_key`, because the record's name rule treats any key holding `private_key` as a credential, even when its value is a path ([`m6-log.md`](m6-log.md) §9).

Section 12 states what the service leaves out.

## 12. Limits

The service covers password login, token refresh, logout, key publication and the identity check.
It does not implement these features:

- OAuth 2.0 or OpenID Connect provider.
- *SAML* (Security Assertion Markup Language).
- Social login.
- Multi-factor authentication.
- Passkeys or WebAuthn.
- Password reset by email, which a custom renderer can add with the *SMTP* (Simple Mail Transfer Protocol) support in core and the `m6-auth` library.

A feature that builds on the existing tokens belongs in a renderer.
Section 13 lists the documents that cover the rest.

## 13. Summary and further reading

`m6-auth-server` turns a password into an access token and a refresh token, and answers the current identity on `POST /check` for a protected route.
It has five routes, one or two listeners, one config file and one SQLite database.

Table 11 gives the documents that own what this page leaves out.
Take from it that each topic this page leaves out has one owning document.

| document | covers |
|---|---|
| [`m6-auth-lib.md`](m6-auth-lib.md) | the `m6-auth` library: schema, password hashing, user and group operations |
| [`m6-auth-cli.md`](m6-auth-cli.md) | the administrative CLI: users, groups, API tokens and the session revoke-all step |
| [`m6-site-toml.md`](m6-site-toml.md) | `[auth]`, `[[backend]]`, `[[route]]` and `require` |
| [`m6-http.md`](m6-http.md) | m6-http internals, including the edge check and the cache |
| [`m6-core-reference.md`](m6-core-reference.md) | the request dictionary and the renderer API |
| [`m6-architecture.md`](m6-architecture.md) | the edge check and why `X-Auth-Claims` and `X-Forwarded-For` are stripped from inbound requests |
| `m6-auth-server/Cargo.toml` | the crate's dependencies and features |

**Table 11: further reading, and what each document covers.**
