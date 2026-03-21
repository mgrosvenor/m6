# m6 — Testing Plan

---

## Infrastructure

```bash
cargo nextest run
cargo nextest run --test-threads 1        # socket-binding tests
RUSTFLAGS="-Z sanitizer=address,leak" \
  cargo +nightly nextest run --target x86_64-unknown-linux-gnu
```

### ProcessGuard

Required for every test that binds a socket:

```rust
struct ProcessGuard(Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}
```

### Fixture Site

```
tests/fixtures/site/
├── site.toml
├── configs/
│   ├── m6-html.conf    ← routes: /, /blog, /blog/{stem}, /error, /admin/{page}
│   ├── m6-file.conf    ← routes: /assets/{relpath}
│   └── m6-auth.conf
├── templates/
├── assets/
├── content/
│   └── posts/
│       ├── _index.json
│       └── hello-world.json
└── data/
    └── site.json
```

---

## m6-html

### L1 — Start / Stop

| Test | Expected |
|---|---|
| Valid config, no secrets_file | Starts using config values |
| Valid config, secrets_file present | Starts using merged values — secrets file wins on conflict |
| secrets_file declared but file absent | Silently ignored, starts with config values |
| secrets_file declared, file absent | Silently ignored, starts with config values |
| Local key overrides global | Starts, warning in stdout |
| SIGTERM | Finish in-flight, exit 0 |
| SIGTERM twice | Immediate exit |
| SIGINT | Same as SIGTERM |

### L2 — Route Matching

| Test | Expected |
|---|---|
| `/blog` with `/blog` and `/blog/{stem}` | Exact `/blog` matched |
| `/blog/hello-world` | Parameterised, stem = `hello-world` |
| No matching route | 404, empty body |
| Equal specificity tie | First declaration wins, warning logged |

### L3 — Params Merge

| Test | Expected |
|---|---|
| `global_params` + route `params`, conflicting key | Route params win |
| Three files, left-to-right | Last file wins |
| Missing params file | 500, error in stdout |
| Built-in key in params file (`site_name`) | Built-in overwrites (injected last) |

### L4 — Path Parameter Expansion

| Test | Expected |
|---|---|
| `/blog/hello-world` → `content/posts/{stem}.json` | Reads `hello-world.json` |
| `{stem}` containing `..` | 400 |
| `{stem}` containing `/` | 400 |
| `{relpath}` with subdirectory | Allowed |

### L5 — Built-in Keys

| Test | Expected |
|---|---|
| `site_name` in template | From `[site] name` |
| `request_path` | Matches request path |
| `query.foo` for `?foo=bar` | `"bar"` |
| `/error?status=404&from=/x` | Template receives `error_status` and `error_from` |

### L6 — Status and Cache

| Test | Expected |
|---|---|
| Route `status = 404` | Response status 404 |
| `cache = "public"` | `Cache-Control: public` |
| `cache = "no-store"` | `Cache-Control: no-store` |

### L7 — Compression

| Test | Expected |
|---|---|
| `Accept-Encoding: br` | `Content-Encoding: br`, decompresses to correct HTML |
| `Accept-Encoding: gzip` | `Content-Encoding: gzip` |
| No `Accept-Encoding` | Identity |

### L8 — Integration

100 concurrent requests, all routes, no errors. Modify `data/site.json`, restart (inotify not in renderer — just verifying content update). Sanitiser: 1000 requests, SIGTERM, no leaks.

---

## m6-file

### L1 — Start / Stop

Same as m6-html L1.

### L2 — Path Resolution

| Test | Expected |
|---|---|
| Existing file | Correct bytes, correct Content-Type |
| Nonexistent file | 404 |
| `../` traversal in URL | 404 |
| `{relpath}` with subdirectory | Correct file |
| Symlink outside root | 404 |

### L3 — Compression

| Test | Expected |
|---|---|
| `text/css` requested | Compressed (brotli or gzip per Accept-Encoding) |
| `image/jpeg` requested | Not compressed |
| `font/woff2` requested | Not compressed |

### L4 — Cache-Control

Always `Cache-Control: public`.

### L5 — Integration

Sanitiser: 1000 requests, SIGTERM, no leaks.

---

## m6-http

### L1 — Start / Stop

| Test | Expected |
|---|---|
| No arguments | Exit 2 |
| Site dir only, no system config | Exit 2 — second argument required |
| Both args, valid system config | Starts, `[server]` from system config |
| Both args, system config has non-`[server]` key | Warning logged, key ignored, starts |
| `[server]` absent from site.toml, present in system config | Starts — validation after merge |
| `[server]` absent from both | Exit 2 |
| System config missing or unparseable | Exit 2 |
| `[auth]` declared, public key not found | Exit 2 |
| `require` on route, no `[auth]` | Exit 2 |
| `--dump-config` | Effective merged config to stdout, exit 0 |
| SIGTERM | Drain in-flight, exit 0 |
| SIGTERM twice | Immediate |
| SIGTERM during active request | In-flight completes, then exit |

### L2 — Routing

| Test | Expected |
|---|---|
| Exact path | Correct backend |
| Parameterised path | Correct backend |
| No match | 404 per `[errors] mode` |

### L3 — Auth — Public Routes (Hot Path)

| Test | Expected |
|---|---|
| Request to public route, no JWT | Forwarded — no auth check |
| Request to public route, any JWT | Forwarded — no auth check |
| Cached public route | Served from cache — zero auth code executed |

**Critical:** instrument the auth verification function. Assert call count = 0 for all public route requests.

### L4 — Auth — Protected Routes

| Test | Expected |
|---|---|
| No token, API client | 401 |
| No token, no refresh cookie, browser | 302 → `/login?next=<path>` |
| No session cookie, valid refresh cookie, browser | 302 → `POST /auth/refresh` → new cookies → original path |
| No session cookie, expired refresh cookie, browser | 302 → `/login?next=<path>` |
| Invalid JWT (bad signature) | 401 / 302 |
| Expired JWT, no refresh cookie | 401 / 302 → login |
| Valid JWT in Authorization header | Forwarded |
| Valid JWT in session cookie | Forwarded |
| Both header and cookie present | Header takes precedence |
| Valid JWT, wrong group | 403 |
| Valid JWT, correct group | Forwarded with `X-Auth-Claims` header |
| `X-Auth-Claims` header content | Base64-decoded JSON matches token claims |

### L4 — Auth — Login Endpoint

| Test | Expected |
|---|---|
| `POST /auth/login` form, valid credentials | 302 → `next`, two HttpOnly cookies set |
| `POST /auth/login` form, invalid credentials | 302 → `/login?error=invalid&next=<next>` |
| `POST /auth/login` form, `next` is external URL | 302 → `/` (next ignored) |
| `POST /auth/login` form, `next` absent | 302 → `/` |
| `POST /auth/login` JSON, valid credentials | 200, JSON tokens, no cookies |
| `POST /auth/login` JSON, invalid credentials | 401 |
| `POST /auth/login`, rate limited | 429 with `Retry-After` |
| `session` cookie `Path` | Sent on all requests |
| `refresh` cookie `Path` | Sent only to `/auth/refresh` |

### L4 — Auth — Refresh and Logout

| Test | Expected |
|---|---|
| `POST /auth/refresh` with valid refresh cookie | 302 → Referer, new session cookie |
| `POST /auth/refresh` with expired refresh cookie | 302 → `/login` |
| `POST /auth/refresh` JSON, valid token | 200, new access token |
| `POST /auth/refresh` JSON, expired token | 401 |
| `POST /auth/logout` form | 302 → `/`, both cookies cleared (Max-Age=0) |
| `POST /auth/logout` API | 204, refresh token revoked |

### L5 — Pool Management

| Test | Expected |
|---|---|
| Socket appears matching glob | Added to pool, requests routed to it |
| Socket disappears | Removed from pool |
| All sockets gone | 503 per `[errors] mode` |
| One socket fails, others healthy | Traffic shifts to healthy sockets |
| Failed socket retried after backoff | Rejoins pool when available |

### L6 — Caching

| Test | Expected |
|---|---|
| `Cache-Control: public` | Cached |
| `Cache-Control: no-store` | Not cached |
| Cache key is `(path, encoding)` | `br` and `gzip` are separate entries |
| Query strings stripped | `?a=1` and `?a=2` same cache key |
| No eager pre-fetch | One request → one cache entry |

### L7 — Error Handling

| Test | Expected |
|---|---|
| Backend returns 404 | m6-http fetches `/_errors?status=404&from=/original-path`, returns 404 + HTML |
| Backend returns 500 | m6-http fetches `/_errors?status=500&from=/original-path`, returns 500 + HTML |
| No `[errors] path` configured | Returns status per `[errors] mode` |
| Error page fetch itself fails | Falls back to `[errors] mode`, no loop |
| Request already to error path | Returns status per `[errors] mode`, no recursion |
| Pool unreachable | 503 per `[errors] mode` |

### L8 — `[errors] mode`

| Mode | Unreachable pool | Response |
|---|---|---|
| `"status"` | | 503, empty body |
| `"internal"` | | 503, m6-http minimal HTML |
| `"custom"` | | 503, error page fetched from `[errors] path` |

### L9 — Hot Reload

| Change | Expected |
|---|---|
| `site.toml` modified | Route table updated, no restart |
| TLS cert modified | Context reloaded |
| Data file modified | Affected cache entries evicted |
| New socket appears | Added to pool without restart |

### L10 — Sanitiser + Load

1000 concurrent requests, mixed routes, mixed auth, SIGTERM mid-load. No leaks. No deadlocks. Pool correctly reflects socket state throughout.

---

## m6-auth

### L1 — Start / Stop

| Test | Expected |
|---|---|
| Valid config | Starts, socket appears |
| Missing private key | Exit 2 |
| Missing database directory | Exit 2 or create |
| SIGTERM | Exit 0 |

### L2 — Login

| Test | Expected |
|---|---|
| Correct credentials | 200, access + refresh tokens in JSON body |
| Correct credentials | `Set-Cookie: session=<jwt>; HttpOnly; Secure; SameSite=Strict` present |
| Wrong password | 401, no cookie set |
| Unknown user | 401 (same response as wrong password) |
| 6th attempt within 15 minutes | 429 with `Retry-After` |
| After rate limit window | Login succeeds again |

### L3 — Token Refresh

| Test | Expected |
|---|---|
| Valid refresh token | 200, new access token |
| Expired refresh token | 401 |
| Invalid token | 401 |
| After logout | 401 (revoked) |

### L4 — JWT Verification (m6-http side)

| Test | Expected |
|---|---|
| Token signed with correct key | Verified locally |
| Token signed with wrong key | 401 |
| Token `exp` in past | 401 |
| Token `iss` mismatch | 401 |
| Token groups match `require` | Forwarded |
| Token groups do not match | 403 |

### L5 — User and Group Management

All endpoints require `role:admin` JWT. Verify 403 without it.

Create user → user exists. Add to group → group membership reflected in next login token. Delete user → login fails. Delete group → membership removed.

### L6 — Key Rotation

Issue token with key A. Rotate to key B (replace key files, inotify triggers reload). Token from key A still accepted until expiry. Token from key B accepted immediately. Token with neither key rejected.

---

## End-to-End

```bash
# Start all processes
m6-html fixture/site/ fixture/site/configs/m6-html.conf &
m6-file fixture/site/ fixture/site/configs/m6-file.conf &
m6-auth fixture/site/ fixture/site/configs/m6-auth.conf &
m6-http fixture/site/ &

# All public routes return 200
for path in / /blog /blog/hello-world /assets/style.css; do
  code=$(curl -sk -o /dev/null -w "%{http_code}" https://localhost:8443$path)
  echo "$code $path"
done

# Protected route without JWT → 401
curl -sk -o /dev/null -w "%{http_code}" https://localhost:8443/admin/dashboard

# Login and access protected route
TOKEN=$(curl -sk -X POST https://localhost:8443/auth/login \
  -d '{"username":"admin","password":"..."}' | jq -r .access_token)
curl -sk -o /dev/null -w "%{http_code}" \
  -H "Authorization: Bearer $TOKEN" \
  https://localhost:8443/admin/dashboard

# Error page
curl -sk -o /dev/null -w "%{http_code}" https://localhost:8443/does-not-exist
# Expected: 404, HTML body from /error route

# Verify all log lines are valid JSON
journalctl -u m6-http -o cat | jq . > /dev/null
```

---

## CI

```yaml
jobs:
  test:
    - cargo nextest run
    - cargo clippy -- -D warnings
    - cargo fmt --check
  sanitiser:
    - RUSTFLAGS="-Z sanitizer=address,leak"
      cargo +nightly nextest run --target x86_64-unknown-linux-gnu
  load:
    - wrk -t4 -c100 -d30s https://localhost:8443/
    # fail on p99 > 10ms for cached public routes
```
