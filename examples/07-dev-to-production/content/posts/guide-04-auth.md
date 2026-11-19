+++
title   = "Guide 04: Login and Protected Pages"
date    = "2025-01-04"
summary = "Add m6-auth: JWT-based authentication, HttpOnly cookies, transparent token refresh, and route-level access control."
tags    = ["guide", "tutorial", "auth", "security"]
cover   = "https://picsum.photos/seed/guide04/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
[Example 02](/blog/guide-02-blog) →
[Example 03](/blog/guide-03-contact-form) →
**Example 04 — Auth** →
[Example 05](/blog/guide-05-cms) →
[Example 06](/blog/guide-06-systemd) →
[Example 07](/blog/guide-07-dev-to-production)

---

`examples/04-auth/` adds m6-auth. Members-only pages are protected behind a login form. The JWT is stored in an HttpOnly cookie — the browser sends it automatically on every request, JavaScript cannot read it.

```bash
cd examples/04-auth
./setup.sh   # generates keys, creates first admin user
./dev.sh
# login at https://localhost:8443/login
```

## Key generation and bootstrap (`setup.sh`)

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# Create first admin user — database created automatically if absent
m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add admin --role admin

echo "Setup complete. Start the server with ./dev.sh"
```

Run `setup.sh` once per environment. The private key stays on the server and signs the tokens. m6-auth-server verifies them when m6-http asks.

## `configs/m6-auth.conf`

```toml
[storage]
path = "@M6_DEV_HOME@/04-auth/auth.db"

[tokens]
access_ttl  = 900       # 15 minutes
refresh_ttl = 2592000   # 30 days
issuer      = "localhost"

[keys]
private_key = "@M6_DEV_HOME@/dev/auth.pem"
public_key  = "@M6_DEV_HOME@/dev/auth.pub"
```

m6-auth manages its own SQLite database at `storage.path`. The access token is short-lived (15 min); the refresh token is long-lived (30 days).

## New entries in the system config

`[auth]` names the backend m6-http asks who a caller is, and it belongs in the system config (`configs/system-dev.toml`) and not in `site.toml`, because it describes this node. The backend is the check socket of m6-auth-server, declared in `site.toml` below.

```toml
[auth]
backend = "m6-auth-check"
```

## New entries in `site.toml`

```toml
[[backend]]
name    = "m6-auth"
sockets = "/run/m6/m6-auth*.sock"

# The check socket carries POST /check. Its name is the login socket's with a
# `check-` prefix, which keeps it out of the login glob above.
[[backend]]
name    = "m6-auth-check"
sockets = "/run/m6/check-m6-auth*.sock"

[[route]]
path    = "/login"
backend = "m6-html"

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
path    = "/members"
backend = "m6-html"
require = "group:members"

[[route]]
path    = "/members/{page}"
backend = "m6-html"
require = "group:members"
```

The `[auth]` block tells m6-http which backend to ask who a caller is: the check socket of m6-auth-server. Routes with `require` are enforced before m6-html is ever contacted.

## The cookie scheme

<pre class="mermaid">
sequenceDiagram
    participant Browser
    participant mhttp as m6-http
    participant mauth as m6-auth
    participant mhtml as m6-html

    Browser->>mhttp: GET /members (no cookie)
    mhttp->>Browser: 302 /login?next=/members
    Browser->>mhttp: POST /auth/login (credentials)
    mhttp->>mauth: forward
    mauth->>Browser: 302 /members + Set-Cookie: session, refresh
    Browser->>mhttp: GET /members (session cookie)
    mhttp->>mhttp: verify JWT locally (no network)
    mhttp->>mhtml: forward
    mhtml->>Browser: members page
</pre>

On successful login, m6-auth sets two HttpOnly cookies:

```
Set-Cookie: session=<access_token>;  HttpOnly; Secure; SameSite=Strict; Path=/;             Max-Age=900
Set-Cookie: refresh=<refresh_token>; HttpOnly; Secure; SameSite=Strict; Path=/auth/refresh; Max-Age=2592000
```

The `session` cookie is sent on every request — m6-http sends it to the check socket, which verifies it and answers with the caller's current roles and groups. The `refresh` cookie is sent **only** to `POST /auth/refresh` due to its restricted `Path`. JavaScript can read neither cookie. No XSS exposure of tokens.

## Session expiry and transparent refresh

When the `session` cookie expires:

1. Next request to a protected route → m6-http finds no valid access token
2. Browser request (Accept: text/html)? Check for `refresh` cookie
3. Refresh cookie present and valid? → redirect to `POST /auth/refresh`
4. m6-auth issues new cookies → redirect back to original path
5. User never sees a login page

If the refresh token is also expired, or absent: redirect to `/login?next=<original-path>`. API clients (non-text/html Accept) receive 401 directly throughout.

## `templates/login.html`

```html
{% extends "base.html" %}
{% block title %}Login · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Login</h1>
  {% if query.error %}
    <p class="error">Invalid username or password.</p>
  {% endif %}
  <form method="post" action="/auth/login">
    <input type="hidden" name="next" value="{{ query.next }}">
    <label>Username <input type="text"     name="username" required autofocus></label>
    <label>Password <input type="password" name="password" required></label>
    <button type="submit">Login</button>
  </form>
{% endblock %}
```

The hidden `next` field carries the original destination through the form POST. m6-auth validates that `next` is a relative path before using it — no open redirect.

m6-auth handles `POST /auth/login` (form-encoded) and either:
- **Success** → sets cookies, `302` to `next`
- **Failure** → `302` to `/login?error=invalid&next=<next>`

## `templates/members.html`

```html
{% extends "base.html" %}
{% block title %}Members · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Members Area</h1>
  <p>Welcome. This page is only visible after login.</p>
  <form method="post" action="/auth/logout">
    <button type="submit">Logout</button>
  </form>
{% endblock %}
```

Logout is a plain form POST — no JavaScript. m6-auth clears both cookies and redirects to `/`.

## Managing users with m6-auth-cli

```bash
# Add a user
m6-auth-cli configs/m6-auth.conf user add alice --group members

# Set a password
m6-auth-cli configs/m6-auth.conf user passwd alice

# Add to a group
m6-auth-cli configs/m6-auth.conf group add-member members alice

# List users
m6-auth-cli configs/m6-auth.conf user ls
```

m6-auth-cli reads the same conf as m6-auth-server. It writes directly to the SQLite database, and a change takes effect on the next check, because `POST /check` reads the database's current roles and groups.

## How m6-http enforces `require`

For each request to a route with `require`:

1. Extract JWT from `Authorization: Bearer` header, or the `session` cookie (header wins)
2. Send it to the check backend (`POST /check` on m6-auth-server's check socket), which verifies the signature, kind, issuer, audience and expiry
3. Check the `require` declaration, `group:<name>` or `role:<name>`, against the roles and groups the check answered with
4. Reject with 401 (no or invalid token) or 403 (insufficient claims): JSON for an API client, a redirect to `/login` for a browser
5. Valid? Forward request with token intact

Renderers receive the verified identity as keys in the request context (`auth_username`, `auth_roles`, `auth_groups`) and can perform additional fine-grained checks.

---

**← [Example 03 — Contact Form](/blog/guide-03-contact-form)** | **[Example 05 — CMS Blog](/blog/guide-05-cms) →**
