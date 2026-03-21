# m6-auth

Auth service for m6 deployments. Issues and verifies JWTs, manages users and groups, enforces ACLs. Optional — sites with no protected routes do not need it.

---

## Design

Auth is optional and absent from the hot path. A static site with no `require` declarations in `site.toml` has zero auth overhead — m6-http never loads the public key, never executes auth code, and m6-auth does not need to be installed or running.

When auth is used, verification is local: m6-http holds m6-auth's public key and verifies JWT signatures without a network call. m6-auth is only contacted for credential operations — login, token refresh, logout, user and group management.

---

## CLI

```
m6-auth <site-dir> <config-path>
```

Listens on a Unix socket derived from config filename: `configs/m6-auth.conf` → `/run/m6/m6-auth.sock`.

Optional: `--log-level debug`

Logs to stdout as structured JSON.

---

## Config

```toml
# configs/m6-auth.conf

[storage]
path = "data/auth.db"    # SQLite database, relative to site directory

[tokens]
access_ttl  = 900        # 15 minutes, in seconds
refresh_ttl = 2592000    # 30 days, in seconds
issuer      = "example.com"

[keys]
private_key = "/etc/m6/auth.pem"   # RSA or EC private key for signing
public_key  = "/etc/m6/auth.pub"   # Corresponding public key
```

### Config Keys

**`[storage]`**

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | SQLite database path, relative to site directory |

**`[tokens]`**

| Key | Type | Default | Notes |
|---|---|---|---|
| `access_ttl` | integer | `900` | Access token lifetime in seconds |
| `refresh_ttl` | integer | `2592000` | Refresh token lifetime in seconds |
| `issuer` | string | `[site] domain` | JWT `iss` claim |

**`[keys]`**

| Key | Type | Required | Notes |
|---|---|---|---|
| `private_key` | string | yes | Absolute path to PEM private key |
| `public_key` | string | yes | Absolute path to PEM public key |

Keys watched via inotify — reloaded on rotation without restart. On rotation, m6-auth begins signing with the new key immediately. Previously issued tokens with valid signatures from the old key continue to be accepted by m6-http until they expire naturally, provided m6-http is updated with the new public key (also via inotify reload).

---

## HTTP API

Four endpoints. All accept and return JSON. User and group management is handled by `m6-auth-cli`, which operates directly on the database.

### Login

Accepts two content types, detected via `Content-Type` header.

**Browser (form POST):**

```
POST /auth/login
Content-Type: application/x-www-form-urlencoded

username=alice&password=...&next=/members
```

```
→ 302 Found
Set-Cookie: session=<access_token>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=900
Set-Cookie: refresh=<refresh_token>; HttpOnly; Secure; SameSite=Strict; Path=/auth/refresh; Max-Age=2592000
Location: /members

→ 302 Found   (invalid credentials)
Location: /login?error=invalid&next=/members

→ 429 Too Many Requests  (rate limited)
```

On success, redirects to `next`. On failure, redirects back to the login page with `?error=invalid` so the template can display a message. No 401 is ever returned to a browser login form — the redirect loop keeps the browser in control.

`next` must be a relative path beginning with `/`. m6-auth validates this server-side and falls back to `/` if `next` is absent, external, or malformed. No client-side validation needed.

**API client (JSON):**

```
POST /auth/login
Content-Type: application/json

{"username": "alice", "password": "..."}

→ 200 OK
Content-Type: application/json

{
  "access_token":  "<jwt>",
  "refresh_token": "<jwt>",
  "expires_in":    900
}

→ 401 Unauthorized   (invalid credentials)
→ 429 Too Many Requests  (rate limited)
```

JSON clients receive tokens in the response body and manage them themselves. No cookies are set for JSON requests.

m6-http accepts the JWT from either `Authorization: Bearer <token>` or the `session` cookie, whichever is present.

### Token Refresh

**Browser (cookie):**

```
POST /auth/refresh
Cookie: refresh=<refresh_token>

→ 302 Found
Set-Cookie: session=<access_token>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=900
Location: <Referer or />

→ 302 Found   (expired or invalid refresh token)
Location: /login
```

The browser sends the `refresh` cookie automatically because its `Path` is `/auth/refresh`. m6-http can redirect an expired-session browser request directly to `POST /auth/refresh` to attempt silent renewal before falling back to `/login`. If the refresh token is still valid the user never sees a login page.

**API client (JSON):**

```
POST /auth/refresh
Content-Type: application/json

{"refresh_token": "<jwt>"}

→ 200 OK
{"access_token": "<jwt>", "expires_in": 900}

→ 401 Unauthorized   (expired or invalid refresh token)
```

### Logout

```
POST /auth/logout
Cookie: session=<access_token>   (browser)
   — or —
Authorization: Bearer <access_token>   (API)

→ 302 Found   (browser — form POST from logout link)
Set-Cookie: session=; Max-Age=0; Path=/
Set-Cookie: refresh=; Max-Age=0; Path=/auth/refresh
Location: /

→ 204 No Content   (API)
```

Revokes the refresh token. Clears both cookies for browser clients. Access tokens expire naturally — they are short-lived and not individually revocable.

### Public Key

```
GET /auth/public-key

→ 200 OK
Content-Type: application/x-pem-file

-----BEGIN PUBLIC KEY-----
...
-----END PUBLIC KEY-----
```

Unauthenticated. Allows m6-http and renderers to fetch the current public key programmatically. m6-http also reads it from the filesystem path configured in `[auth] public_key` — the endpoint is for convenience.

---

## JWT Structure

Access token claims:

```json
{
  "iss": "example.com",
  "sub": "user-id-123",
  "exp": 1700000900,
  "iat": 1700000000,
  "username": "alice",
  "groups": ["editors", "users"],
  "roles":  ["user"]
}
```

Algorithm: RS256 (RSA) or ES256 (EC). Configurable at key generation time. m6-http verifies signature, expiry, and issuer locally without contacting m6-auth.

---

## m6-http Integration

`site.toml`:

```toml
[auth]
backend    = "m6-auth"
public_key = "/etc/m6/auth.pub"

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

m6-http enforcement for a `require` route:

1. Extract JWT from `Authorization: Bearer` header or `session` cookie
2. Verify RS256/ES256 signature using public key — local, no network call
3. Verify `exp` and `iss` claims
4. Check `groups` or `roles` claim against `require` value
5. 401 if token absent or invalid; 403 if claims insufficient
6. Forward to backend if satisfied

---

## Renderer Integration

JWT claims are forwarded to renderers in the `X-Auth-Claims` header and merged into the request dictionary. Renderers access them as ordinary dictionary keys:

```rust
let username = req["auth_username"].as_str().unwrap_or("");
let is_editor = req["auth_groups"]
    .as_array()
    .map(|g| g.iter().any(|v| v.as_str() == Some("editors")))
    .unwrap_or(false);
```

Available keys: `auth_username`, `auth_groups`, `auth_roles`, `auth_sub`. Present on any request where a valid token was found, regardless of whether the route has `require`. Absent on unauthenticated requests.

---

## Storage

SQLite database shared with `m6-auth-cli` and any custom renderer that links against the `m6-auth` library. Schema, migrations, and all database operations are owned by the `m6-auth` library crate. m6-auth-server opens the database via `m6_auth::Db::open` on startup.

Multiple processes may hold the database open simultaneously. WAL mode is set by the library — reads and writes from concurrent processes are safe.

Database path configured in `configs/m6-auth.conf`:

```toml
path = "data/auth.db"
```

---

## Rate Limiting

Login endpoint: 5 attempts per 15 minutes per IP. Response: 429 with `Retry-After` header.

---

## Signal Handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Finish current request, close socket, exit 0 | Immediate exit |

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config error — private key not found, database unreadable, etc. |

---

## Logging

Structured JSON to stdout. Never logs passwords, tokens, or key material.

Events: startup, login success (username, ip), login failure (ip, reason), token refresh, logout, user created, group membership changed, key reloaded.

---

## Key Generation

```bash
# EC key (recommended — smaller tokens, faster verification)
openssl ecparam -name prime256v1 -genkey -noout -out /etc/m6/auth.pem
openssl ec -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub

# RSA key (wider compatibility)
openssl genrsa -out /etc/m6/auth.pem 2048
openssl rsa -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
```

Restrict permissions: `chmod 600 /etc/m6/auth.pem`

---

## Non-Goals (v1)

- OAuth2 / OpenID Connect provider
- SAML
- Social login (Google, GitHub, etc.)
- Multi-factor authentication
- Passkeys / WebAuthn
- Password reset via email (requires SMTP — use a user renderer for this)

---

## Cargo.toml (m6-auth-server)

```toml
[package]
name    = "m6-auth-server"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "m6-auth-server"

[dependencies]
m6-auth      = { path = "../m6-auth" }
m6-render    = { path = "../m6-render" }
jsonwebtoken = "9"
toml         = "0.8"
anyhow       = "1"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
```
