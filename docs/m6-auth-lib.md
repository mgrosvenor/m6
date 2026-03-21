# m6-auth (library crate)

Shared library for user and group management. Used by `m6-auth-server`, `m6-auth-cli`, and any custom renderer that needs to create or modify users (e.g. a signup renderer).

Owns the database schema, migrations, password hashing, and all user/group operations. Nothing in this crate knows about HTTP, JWTs, or request handling.

---

## Workspace

```
m6/
├── Cargo.toml              ← workspace root
├── m6-auth/                ← this crate (library)
│   └── src/lib.rs
├── m6-auth-server/         ← HTTP server binary
│   └── src/main.rs
└── m6-auth-cli/            ← CLI binary
    └── src/main.rs
```

`workspace Cargo.toml`:

```toml
[workspace]
members = ["m6-auth", "m6-auth-server", "m6-auth-cli"]
resolver = "2"
```

`m6-auth/Cargo.toml`:

```toml
[package]
name    = "m6-auth"
version = "0.1.0"
edition = "2021"

[lib]
name = "m6_auth"

[dependencies]
rusqlite    = { version = "0.31", features = ["bundled"] }
bcrypt      = "0.15"
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
uuid        = { version = "1", features = ["v4"] }
thiserror   = "1"
```

---

## Public API

### Connection

```rust
pub struct Db(Connection);

impl Db {
    /// Open database at path. Creates file if absent.
    /// Enables WAL mode. Runs pending migrations.
    pub fn open(path: &Path) -> Result<Self>;
}
```

`Db::open` is the single entry point. Schema creation and all migrations run on open — callers never need to think about database version.

---

### Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id:         String,     // UUID
    pub username:   String,
    pub roles:      Vec<String>,
    pub groups:     Vec<String>, // populated by queries that join memberships
    pub created_at: i64,         // Unix timestamp
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id:      String,
    pub name:    String,
    pub members: Vec<String>,   // usernames, populated by queries that join memberships
}
```

---

### User operations

```rust
impl Db {
    pub fn user_create(
        &self,
        username: &str,
        password: &str,
        roles: &[&str],
    ) -> Result<User>;

    pub fn user_get(&self, username: &str) -> Result<Option<User>>;

    pub fn user_get_by_id(&self, id: &str) -> Result<Option<User>>;

    pub fn user_list(&self) -> Result<Vec<User>>;

    pub fn user_delete(&self, username: &str) -> Result<()>;

    pub fn user_set_password(&self, username: &str, password: &str) -> Result<()>;

    pub fn user_set_roles(&self, username: &str, roles: &[&str]) -> Result<()>;

    /// Verify a plaintext password against the stored hash.
    pub fn user_verify_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<User>>;  // Some(user) if valid, None if invalid or not found
}
```

`user_delete` also removes all group memberships and revokes all refresh tokens for that user.

`user_verify_password` is the login primitive. m6-auth-server calls this on `POST /auth/login`.

---

### Group operations

```rust
impl Db {
    pub fn group_create(&self, name: &str) -> Result<Group>;

    pub fn group_get(&self, name: &str) -> Result<Option<Group>>;

    pub fn group_list(&self) -> Result<Vec<Group>>;

    pub fn group_delete(&self, name: &str) -> Result<()>;

    pub fn group_member_add(&self, group: &str, username: &str) -> Result<()>;

    pub fn group_member_remove(&self, group: &str, username: &str) -> Result<()>;

    pub fn group_members(&self, group: &str) -> Result<Vec<User>>;

    /// All groups a user belongs to. Used by m6-auth-server to populate JWT claims.
    pub fn user_groups(&self, username: &str) -> Result<Vec<String>>;
}
```

---

### Refresh token operations

Used exclusively by m6-auth-server. Exposed in the library so the schema is owned in one place.

```rust
impl Db {
    pub fn refresh_token_store(
        &self,
        user_id: &str,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<()>;

    pub fn refresh_token_verify(&self, token_hash: &str) -> Result<Option<String>>; // user_id

    pub fn refresh_token_revoke(&self, token_hash: &str) -> Result<()>;

    pub fn refresh_tokens_revoke_all(&self, user_id: &str) -> Result<()>; // called on user_delete
}
```

---

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("user not found: {0}")]
    UserNotFound(String),

    #[error("username already exists: {0}")]
    UserExists(String),

    #[error("group not found: {0}")]
    GroupNotFound(String),

    #[error("group already exists: {0}")]
    GroupExists(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("password hashing error: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),
}

pub type Result<T> = std::result::Result<T, AuthError>;
```

---

## Schema

Defined in the library. `Db::open` creates and migrates automatically.

```sql
CREATE TABLE IF NOT EXISTS users (
    id         TEXT PRIMARY KEY,
    username   TEXT UNIQUE NOT NULL,
    password   TEXT NOT NULL,        -- bcrypt hash
    roles      TEXT NOT NULL,        -- JSON array e.g. '["admin","user"]'
    created_at INTEGER NOT NULL      -- Unix timestamp
);

CREATE TABLE IF NOT EXISTS groups (
    id   TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL
);

CREATE TABLE IF NOT EXISTS memberships (
    user_id  TEXT NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, group_id)
);

CREATE TABLE IF NOT EXISTS refresh_tokens (
    token_hash TEXT PRIMARY KEY,     -- SHA-256 of the JWT string
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
```

---

## Custom renderer usage

A signup renderer links against `m6-auth` alongside `m6-render`:

```toml
# render-signup/Cargo.toml
[dependencies]
m6-render = { git = "https://github.com/m6/m6", tag = "v0.1.0" }
m6-auth   = { git = "https://github.com/m6/m6", tag = "v0.1.0" }
```

```rust
use m6_auth::{Db, AuthError};
use m6_render::prelude::*;

// No global state needed — Db is per-thread (not Send+Sync)
struct ThreadLocal {
    db: Db,
}

fn init_thread(config: &Map<String, Value>, _global: &()) -> Result<ThreadLocal> {
    Ok(ThreadLocal {
        db: Db::open(config["auth_db"].as_str()?)?,
    })
}

fn destroy_thread(local: ThreadLocal) {
    local.db.close().ok();
}

fn handle_get(req: &Request, _g: &(), _l: &mut ThreadLocal) -> Result<Response> {
    Response::render("templates/signup.html", req)
}

fn handle_post(req: &Request, _g: &(), local: &mut ThreadLocal) -> Result<Response> {
    let username = req.field("username")?;
    let password = req.field("password")?;

    match local.db.user_create(&username, &password, &["user"]) {
        Ok(_) =>
            Ok(Response::redirect("/login?registered=1")),
        Err(AuthError::UserExists(_)) =>
            Response::render_with("templates/signup.html", req,
                json!({"error": "Username taken"})),
        Err(e) => Err(e.into()),
    }
}

fn main() -> Result<()> {
    App::with_thread_state(init_thread)
        .on_destroy_thread(destroy_thread)
        .route_get("/signup",  handle_get)
        .route_post("/signup", handle_post)
        .run()
}
```

`config.secrets_path("auth_db")` reads the `auth_db` key from the renderer's `secrets_file` — the path to the shared SQLite database. This keeps the database path out of the site directory config.

```toml
# /etc/m6/render-signup.toml  (secrets_file)
auth_db = "/var/www/my-site/data/auth.db"
```

---

## Concurrent access

Multiple processes (m6-auth-server, render-signup, m6-auth-cli) may open the database simultaneously. SQLite WAL mode handles this safely — reads never block writes, writes serialize automatically. `Db::open` sets `PRAGMA journal_mode=WAL` and `PRAGMA busy_timeout=5000`.

m6-auth-server does not cache user data in memory. Every login reads from SQLite. Changes made by m6-auth-cli or a signup renderer take effect immediately on the next login attempt.
