# m6-auth-server

`m6-auth-server` is the authentication service in m6.
It checks a username and password, issues *JWT* (JSON Web Token) access and refresh tokens, renews an access token, revokes a refresh token, and publishes the signing public key.
m6-http asks this service for the identity on a protected route, so login, refresh, logout and check reach it.

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

`m6-auth-server` owns five routes on two Unix sockets: login, refresh, logout and the public key on the primary socket, and check on the check socket.
Table 1 gives them.
Take from it that every route is literal, so the service has no route configuration of its own.

| method | path | listener | what it does | success |
|---|---|---|---|---|
| `POST` | `/auth/login` | primary | checks a password and issues an access token and a refresh token | 302 with cookies for a form client, 200 with JSON for a JSON client |
| `POST` | `/auth/refresh` | primary | issues a new access token from a refresh token | 302 with a session cookie for a form client, 200 with JSON for a JSON client |
| `POST` | `/auth/logout` | primary | clears the browser's cookies, and revokes refresh tokens on the bearer path | 302 for the browser path, 204 for the bearer path |
| `GET` | `/auth/public-key` | primary | returns the current public key | 200 with a PEM body |
| `POST` | `/check` | check | reads the current identity for an access token from the database | 200 with identity JSON |

**Table 1: the five routes, the listener each answers on, what each does, and what success returns.**

The method column is part of the contract.
A `GET` on login, refresh or logout does not match the route.

### 2.1. The two listeners

The primary socket is the one the service has always bound, and it serves the four login routes.
The check socket serves `/check` and nothing else.

The check socket path is derived from the primary path: the same directory, the same file name, and the prefix `check-`.
`/run/m6/m6-auth.sock` gives `/run/m6/check-m6-auth.sock`, so a deployment's existing login glob `/run/m6/m6-auth*.sock` matches the primary and never the check socket.
The upgrade therefore adds a socket and moves none that already dials the primary.
`M6_CHECK_SOCKET_OVERRIDE` sets the path outright, as `M6_SOCKET_OVERRIDE` does for the primary, because a test cannot write to `/run/m6`.

The two listeners are selected by two flags.
Neither flag binds both sockets, which is the upgrade path and keeps one process serving exactly what it serves today.
`--login-only` binds the primary socket and starts no check listener.
`--check-only` binds the check socket and starts no login listener.
Both flags together stop the start with exit 2, naming both.

A second socket is the mechanism because m6 has no shared Unix listener: `[server] reuse_port` is a TCP and QUIC feature of m6-http, and `m6_core::net::listen_unix` has no reuse option.
The two names are separate binds with separate accept queues.

**Two listeners in one process do not isolate `/check` from a login's bcrypt.**
The two sockets feed one event loop and one connection table, so a `/check` that arrives during a login waits for that login's password verify, which is 273 ms at cost 12 in a release build.
The second process is what removes that tail: with `/auth/login` absent from the check process's listener, the check loop runs no bcrypt, and a check process and a login process then have separate loops.
`docs/m6-user-guide.md` section 7.4 gives the deployment that runs them as two units.

The two processes do not load the same material.
A process that binds the primary socket signs tokens and writes users, so it reads `[keys] private_key`, applies the auth schema and opens the database read-write.
A process started with `--check-only` decodes a token and reads users, so it reads only `[keys] public_key`, applies no schema and opens the database through `m6_core::sqlite::open_read_only`, whose `PRAGMA query_only` refuses every write.
The check side therefore needs a database that the login side created once, and a missing file or schema stops it at start with exit 2.
It never creates the database.
Deliver `auth.pub` to the check unit and both key files to the login unit.

The service owns the signing keys, the token lifetimes, the rate limiter and one database connection, all built at start.
It does not manage users or groups.
`m6-auth-cli` (the *CLI*, command-line interface) writes those rows directly, and the service reads them at login and refresh.

In a deployment, m6-http forwards a login, refresh or logout request to the service over the primary Unix socket.
The kernel supplies no client address on that socket.
Section 10 explains what it uses instead, and section 3 gives the two socket paths.

## 3. Command line

`m6-auth-server` takes a site directory and a config path.
Table 2 gives the arguments and flags.
Take from it that every flag is optional and the two positional arguments are required.

| argument or flag | meaning |
|---|---|
| `<site-dir>` | the site root, used to find the default `site.toml` |
| `<config-path>` | the service config, normally `configs/m6-auth.conf` |
| `--log-level <level>` | overrides the configured log level for this run |
| `--dump-config` | prints the core configuration and any routes declared in the config, validates the auth table and the key files, then exits |
| `--version` or `-V` | prints the tag form, `v1.16.0-rc.1`, then exits |
| `--version-full` | prints one `<crate> - <full SemVer>` line per m6 crate in the binary, starting with `m6-auth-server`, then exits |
| `--login-only` | binds the primary socket and serves the four login routes |
| `--check-only` | binds the check socket and serves `/check` |

**Table 2: the two positional arguments and the six flags.**

For this binary the dump covers the site directory, the config paths, the socket, the server settings and any routes declared in the config.
The output does not list the `[storage]`, `[tokens]`, `[keys]` or `[rate_limit]` sections, or the five routes the service registers.
The dump reads those sections with the same code the start reads, including the key material, so a config the start refuses exits 2 here too.

A missing positional argument prints the usage line and exits 2.
`--login-only` and `--check-only` together print one line naming both and exit 2.

The socket path comes from the config file's stem, so `configs/m6-auth.conf` becomes `/run/m6/m6-auth.sock`.
The check socket is derived beside it with the `check-` prefix, so the same config gives `/run/m6/check-m6-auth.sock`.
The directory `/run/m6` is created at startup, and a stale socket file at either path is removed before the bind.
`M6_SOCKET_OVERRIDE` and `M6_CHECK_SOCKET_OVERRIDE` set the two paths outright for a test that cannot write to `/run/m6`.

The service logs to stdout as *JSON* (JavaScript Object Notation) lines, and `[log] level` in `site.toml` sets how much it writes.
Logging is core's, so every m6 service writes the same lifecycle lines.
Section 11.3 lists the lines this service adds.

The config path decides the socket name.

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
A relative path stops the service at startup with exit 2, and the error names the setting.
The check is `secret_path` in `m6-core/src/config.rs`.

The `issuer` falls back to `[site] domain` in the `site.toml` that the optional `site_toml` key names.
When neither the config nor that file names one, the start stops with exit 2 and the error names `[tokens] issuer`.
The `site_toml` key is core's, and it defaults to `<site-dir>/site.toml`.

The audience is the deployment's name, and it is the issuer when `[tokens] audience` is unset.
A token minted for one deployment is refused at another even when the two share the signing key.

The service reads the database path, the token lifetimes, the issuer, the audience and the rate limit once at startup.
A change to any of them after startup is logged with the text "restart m6-auth-server to apply it", and the running value stays.

The two-listener split adds no config key.
The flags in Table 2 select the listeners, and the socket paths come from the config path and the two environment overrides.

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

The keys come from the unit's `LoadCredential` lines and the database from `StateDirectory=m6-auth`, as `m6-user-guide.md` section 7.2 sets out.
The two-unit layout there gives the check unit its own config at `/etc/m6/check/m6-auth.conf`, which is this file with `private_key` removed and the login unit name in `public_key` replaced by the check unit name.

### 4.1. Keys and paths

The private key signs every token and the public key verifies them.
The service reads both files at startup and keeps them in memory.
Both paths are absolute.

The pair is checked at load.
The service signs a probe token with the private key and verifies it with the public key.
A pair that does not match stops startup with exit 2.
Section 4.2 covers what a rotation does to tokens already issued.

### 4.2. Key rotation

A rotation has three steps: write the new key pair in `/etc/m6`, run `m6-auth-cli <config> sessions revoke-all`, and restart m6-auth-server and m6-http.
systemd copies a credential when the unit starts, so each restart delivers the new files.

A successful rotation changes the signing key immediately after the restart.
From then on, m6-http refuses an access token signed with the old key at the next request with 401, and a browser sees the error response.

`sessions revoke-all` ends every refresh token.
It deletes every `refresh_tokens` row in one transaction.
The refresh route matches an opaque token by its stored SHA-256 hash and reads the row, so a deleted row refuses the next refresh even though the token itself is unchanged.
A key change alone leaves a refresh token usable until its `exp`, so the command is the step that ends a stolen one.
Run the command and the restart together with the key change, as one rotation.

The command is the interim form of the `sessions revoke-all` step in the auth API design.
That design adds an auth generation, a new epoch and list key, and an acknowledgement from every cache.
This version holds no decision list on any node, so deleting the rows is the whole effect of the command and it waits for nothing.

Section 4.3 gives the key formats and the commands that generate them.

### 4.3. Key generation

An *EC* (elliptic curve) P-256 key produces ES256 tokens, and an *RSA* (Rivest-Shamir-Adleman) key produces RS256 tokens.
The service picks the algorithm from the private key at load, so the key type is the only choice.

An EC key must be PKCS#8, whose PEM tag is `PRIVATE KEY`.
`openssl ecparam` writes a SEC1 key tagged `EC PRIVATE KEY`, and the signing library refuses that tag at startup.
Convert the SEC1 key to PKCS#8 before use, and keep the embedded public key that the signing library needs.
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

The private key file must be readable by the user the service runs as and by no one else.
The umask at creation gives that without a window in which the key is world-readable.
The same recipe is in `m6-auth-cli/tests/integration.rs`, which generates a key pair for the token tests.
The config's `[keys]` paths name the two files it produces.
The conversion matters: a PKCS#8 key written directly by LibreSSL's `genpkey` can omit the embedded public key and fail at signing, which is why the test goes through SEC1.

Section 4 gave the config surface, the path rules, the rotation steps and the key formats.
Section 5 turns to the HTTP API, where those keys produce tokens.

## 5. HTTP API

The service exposes five routes over *HTTP* (Hypertext Transfer Protocol), and the content type decides the response shape.
The five routes form the service's *API* (application programming interface).
This section gives each route's request and response, beginning with login.

### 5.1. Login

`POST /auth/login` accepts a form body or a JSON body, chosen by `Content-Type`.
A request with `application/json` takes the JSON path.
A request with `application/x-www-form-urlencoded`, or any other content type, takes the form path.

A form login sends `username`, `password` and an optional `next`.

Table 4 gives the form responses.
Take from it that the two credential outcomes are redirects, and a blocked address gets a 429 that the browser displays.

| condition | status | body or headers |
|---|---|---|
| valid credentials | 302 | `Location: <next>`, `Set-Cookie: session=...`, `Set-Cookie: refresh=...` |
| wrong username or password | 302 | `Location: /login?error=invalid&next=<encoded next>` |
| blocked address | 429 | `Retry-After: <seconds left in the window>`, body `Too Many Requests` |

**Table 4: the three form login outcomes.**

The two cookies carry `HttpOnly`, `Secure`, `SameSite=Lax` and a `Max-Age` from the token lifetime.

The session cookie has `Path=/` and `Max-Age=<access_ttl>`.
The refresh cookie has `Path=/auth/refresh` and `Max-Age=<refresh_ttl>`.
The `next` value starts with `/`, and its second byte is neither `/` nor `\`, which is the whole check.
A value that passes is used as the `Location` header, and the service falls back to `/` when `next` is absent or fails that check.

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
On success the `Location` is the `Referer` when that value passes the same two-byte check as `next`, and `/` otherwise.
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

The service hashes the opaque token and matches the SHA-256 hash to the `refresh_tokens` row.
It loads the user for current groups and roles.
This path never verifies the token's signature, so a key change alone does not refuse a refresh token.

A refresh token is a JWT with `type` set to `refresh`.
The service does not rotate it on refresh, so the same refresh token works until it expires.
Section 6 gives both token shapes.

### 5.3. Logout

`POST /auth/logout` clears the browser's cookies, and a bearer request revokes refresh tokens.
A browser sends no `refresh` cookie to this path, because that cookie's `Path` is `/auth/refresh`.
The service clears both cookies and answers 302 to `/`, and a browser logout therefore revokes no refresh row, so the refresh token stays valid until its `exp`.
A JSON client sends `Authorization: Bearer <access token>`, and the service deletes every refresh token for that user and answers 204.

The browser path does not require a valid session.
The JSON path decodes the access token to find the user.
An invalid bearer token still gets 204, and the service revokes nothing.

Access tokens stay valid until their `exp`.
The service keeps no access-token revocation list.
The bearer path stops the next refresh by deleting the refresh rows, and the browser path leaves them in place.

### 5.4. Public key

`GET /auth/public-key` returns the current public key as a PEM body with `Content-Type: application/x-pem-file`.
The route is unauthenticated, and it serves the key that is loaded, so a rotation changes the body after the service restarts.

m6-http holds no key and does not call this route.
The route exists for any other client that needs the key without filesystem access.

### 5.5. Check

`POST /check` answers the current identity for an access token.
It takes `Authorization: Bearer <access token>` and returns `sub`, `username`, `roles` and `groups` read from the database.
The answer is the decision for this request only, with no cache hint: nothing caches a decision, so a revocation counts from the next request.
A route with `require` calls it, and the edge applies that route's requirement to the identity in the answer.

A missing, invalid or expired token is 401 `{"error":"unauthorised"}`.
A user who is not in the database, including a deleted user, is the same 401 rather than an empty role list.
A locked database is 503 `{"error":"unavailable"}` at once, because the caller's deadline is shorter than this service's one-second deferral.
The 401 and 503 are distinct: the 401 refuses this request, and the 503 reports a database the service could not read.

Login, refresh, logout, the public key and check are the whole HTTP surface.
Section 6 gives the token contents those routes produce, and section 7 gives the m6-http checks that consume them.

## 6. Tokens

The service issues two kinds of *JWT* (JSON Web Token): an access token for a request and a refresh token for a new access token.
`m6-auth-cli token create` mints a third kind, an API token.
Every token names its kind in the header `typ`: `at+jwt` for an access token, `m6-api+jwt` for an API token and `m6-refresh+jwt` for a refresh token.
A token with no `typ`, or any other value including the library's default `JWT`, is refused.
Table 7 gives the claims.
Take from it that the access token carries the identity, groups and roles, and the refresh token carries enough to find the user.

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

The `type` claim marks a refresh token for a reader that does not look at the header.
The `typ` header is what the service checks, and `POST /check` accepts `at+jwt` and `m6-api+jwt` and refuses `m6-refresh+jwt` before it reads the claims.
The `jti` is new at every mint and unique per mint. At this tip it names no row: the table that can end a token by its `jti` is part of the shelved token-rules work.
`sid` is absent for the same reason, because it names a session row this schema does not have.

The algorithm follows the key: an EC P-256 key gives ES256, and an RSA key gives RS256.
The `alg` header is set from the key at startup.
The service checks the signature, `typ`, `iss`, `aud`, `exp` and the presence of `iss`, `aud`, `sub`, `exp`, `iat` and `jti` in `POST /check`, and on refresh and the JSON logout path.
The leeway is zero, so a token is refused at the second `exp` names.
m6-http no longer decodes the token, so the algorithm and the key are this service's business.

The access token's groups and roles are a snapshot at issue time.
`POST /check` answers the database's current roles and groups, so a role change or a revocation counts from the next request.

The two token shapes are the contract between this service and its clients.
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

The login glob matches the primary socket and never the check socket, because the check socket's name begins with `check-`.
A deployment can keep the login backend unchanged while it splits the service into two processes.

`[auth] backend` names the check backend.
m6-http probes each member once at start: 401 or 503 passes, 404 stops the start, and a refused connection warns.
A route with `require` sends one `POST /check` per protected request, under a 50 ms deadline, and applies the route's requirement to the identity in the answer.
No credential is 401 with no call, a 401 from this service is 401, an identity without the requirement is 403, and a check that cannot be made is 503 with `Retry-After: 1`.
Nothing caches the answer, so a role change or a revocation counts from the next request.

The same check covers a source node's `/genid`, so its claims see a role change or a revocation from the next request too.
The token sources, the `require` forms and the outcomes of a route with `require` are in [`m6-http.md`](m6-http.md) and [`m6-site-toml.md`](m6-site-toml.md).
The header the edge sets carries the identity this service answered, and section 8 shows what a backend does with it.

## 8. Renderer integration

A backend receives the identity in the `X-Auth-Claims` header, and m6 turns it into the `auth_*` request-dictionary keys.
The keys exist only on a route with `require` and an identity that passed the check, so a backend that needs a claim on every request declares `require` on the route.
The key names are in [`m6-core-reference.md`](m6-core-reference.md) §4.6.

The authority's answer, the edge's `require` match and the backend keys are one chain.
Section 9 covers the database that holds the users the chain begins with.

## 9. Storage

The service stores users, groups, memberships and refresh tokens in *SQLite*.
The `m6-auth` library owns the schema, the password hashes and every query.
The service opens one connection through `m6_auth::Db::open` at start and keeps it for the life of the process.
A database that cannot be opened stops the service with exit 2, and the error names the path.

`Db::open` sets *WAL* (write-ahead logging), a 5-second busy timeout and foreign keys.
WAL lets readers and one writer proceed together.
The service sets the busy timeout to zero after that open, because a wait in the one loop holds every connection behind it.
A statement that meets another process's lock returns `SQLITE_BUSY` at once.
The loop defers that request and tries it again every 5 ms while it serves the other connections.
A request that is still locked after one second is answered with 503 and `Retry-After: 1`.
A retried login does not verify the password twice, because the retry keeps the user the first attempt verified.
`m6-auth-cli` keeps the 5-second wait, because it serves one command at a time.
Each process has one connection and one loop.
A login holds its process for its bcrypt verify, so every connection in that process waits.
A check-only process has no login route, so that wait never reaches `/check`.

The database path is the `[storage] path` key, an absolute path.
The service creates the parent directory when it is missing.
`m6-auth-cli` and a custom renderer use the same library and the same file, so a user created by the CLI is visible at the next login.

The database is the shared state.
Section 10 covers the one piece of state the service keeps in memory.

## 10. Rate limiting

The login route limits failed passwords by client address.
Table 3 gives the two `[rate_limit]` keys, and the defaults are 5 failures in 900 seconds.
Take from it that the throttle is configuration, and a restart clears it.

`window_secs = 0` makes every count expire at once, which turns the throttle off.

Only a failed credential check counts.
A correct password clears the address's count.
A blocked address gets 429 with `Retry-After: <seconds left in the window>` before the password is checked.
The value is the whole seconds until the window ends, rounded up, so it is never below the true wait and never above `window_secs`.
The window is measured from the most recent failure, so five failures each inside the window use up the budget however long the series takes.
A block lifts after `window_secs` with no further failure.

The login listener is a Unix socket, so the service takes the value of `X-Forwarded-For`, then `X-Real-IP`.
When neither is present, the address is the string `unix`.
m6-http sets the forwarded header and strips any client-supplied copy.
m6-http sends exactly one address in it, on HTTP/1.1 and HTTP/2, so behind m6-http the `unix` bucket is never used.
A direct caller can set the header, so the service's socket must be reachable only by m6-http.

The counts live in memory.
A restart forgets them, and section 4 gives the restart rule for the two keys.
The service runs one login process per site, so one count covers every login.
A check-only process never touches the limiter, because `/auth/login` is not on its listener.
The source comment says that brute force slow enough to survive restarts is left to the firewall.
The tests `forwarded_addresses_select_the_rate_limit_bucket`, `requests_without_a_forwarded_address_share_one_bucket` and `a_restart_forgets_the_failure_counts` in `m6-auth-server/tests/integration.rs` pin these limits.
The unit test `failures_spread_across_more_than_one_window_exhaust_the_budget` in `m6-auth-server/src/rate_limit.rs` pins the window's start and its rollover.

The throttle bounds one route and one address at a time.
Section 11 covers the process around it.

## 11. Lifecycle

The service runs one process and one event loop, with one or two Unix sockets and a signal handler.
This section gives the signals, the exit codes and the log lines, beginning with the signals.

### 11.1. Signals

Table 8 gives the two signals.
Take from it that both shutdown signals exit 0 and remove the sockets the process bound.

| signal | first receipt | later receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | stop accepting, remove the sockets, exit 0 | the same, and the second signal ends the process at once if the first has not |

**Table 8: the signals the service handles.**

### 11.2. Exit codes

Table 9 gives the exit codes.
Take from it that a configuration fault fails the start, and a request fault becomes an HTTP status.

| code | meaning |
|---|---|
| `0` | clean shutdown, `--version`, `--version-full`, or a successful `--dump-config` |
| `1` | runtime error, including a failure to start logging |
| `2` | configuration or invocation error: a missing argument, a config that does not parse, a relative path, a key that does not load, a key pair that does not match, a database that cannot be opened, or a socket that cannot be bound |

**Table 9: the process exit codes.**

A request that fails after startup returns a 4xx or 5xx response, and the process keeps running.

### 11.3. Logging

The service writes one line per event to stdout, in JSON by default.
It logs the route outcomes and the core lifecycle lines.
Table 10 gives the events.
Take from it that every login attempt is recorded with the address and every failure with a reason.

| event | level | fields |
|---|---|---|
| login success | info | `username`, `ip` |
| login failure | warn | `ip`, `reason` |
| rate limit exceeded | warn | `ip` |
| token refresh | info | `user_id` |
| logout | info | `user_id` for the bearer path |

**Table 10: the log events this service adds to the core lifecycle lines.**

The service never logs a password, a token or key material.

A change to a startup-only value is logged at warn level, and section 4 gives the restart rule.

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

`m6-auth-server` turns a password into a short-lived access token and a refresh token, and answers the current identity on `POST /check` for a protected route.
The service has five routes, one or two listeners, one config file and one SQLite database.
The rotation steps and the two path rules are in section 4.

Table 11 gives the documents that own what this page leaves out.
Take from it that every number and every contract has a file responsible for it.

| document | covers |
|---|---|
| [`m6-auth-lib.md`](m6-auth-lib.md) | the `m6-auth` library: schema, password hashing, user and group operations |
| [`m6-auth-cli.md`](m6-auth-cli.md) | the administrative CLI: users, groups, API tokens and the session revoke-all step |
| [`m6-site-toml.md`](m6-site-toml.md) | `[auth]`, `[[backend]]`, `[[route]]` and `require` |
| [`m6-http.md`](m6-http.md) | m6-http internals, including the edge check and the cache |
| [`m6-core-reference.md`](m6-core-reference.md) | the request dictionary and the renderer API |
| [`m6-architecture.md`](m6-architecture.md) | why the edge verifies tokens and why the header is stripped |
| `m6-auth-server/Cargo.toml` | the crate's dependencies and features |

**Table 11: further reading, and what each document covers.**

The first three are the pages a deployment reads beside this one.

A change to those routes or claims changes this page.
The rest of the page is the detail behind them.
