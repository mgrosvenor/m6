# m6-auth library

`m6-auth` stores the users, groups and tokens that m6's authentication services share.
`m6-auth-server` and `m6-auth-cli` link it, and a renderer can link it to create users itself.
This document gives its public interface: the database it opens, the rows it returns, every operation, and the rules a caller keeps.

**Contents**

1. [What the crate is](#1-what-the-crate-is)
2. [Opening a database](#2-opening-a-database)
3. [Users](#3-users)
4. [Groups](#4-groups)
5. [Tokens](#5-tokens)
6. [Errors](#6-errors)
7. [The schema](#7-the-schema)
8. [A renderer that links it](#8-a-renderer-that-links-it)
9. [Concurrent access](#9-concurrent-access)
10. [Summary](#10-summary)

## 1. What the crate is

**`m6-auth` is the storage library for m6's users, groups and tokens.**
It owns the *SQL* (Structured Query Language) database, the password hashes, the token records and the JWT engine, and it holds no socket and no *HTTP* (Hypertext Transfer Protocol) request handling.
The package is `m6-auth` and the library target is `m6_auth`, so code writes `use m6_auth::Db`.

Table 1 gives the crates that link it.
Take from it that every caller reaches one database file through the same `Db::open`.

| crate | what it uses `m6-auth` for |
|---|---|
| `m6-auth-server` | opens the database, verifies a password on login, and stores and revokes refresh tokens |
| `m6-auth-cli` | opens the same database to manage users, groups and *API* (application programming interface) tokens |
| a custom renderer | can create a user on a signup page, as section 8 shows |

**Table 1: the crates that link `m6-auth`, and what each one uses it for.**
The first two are in this workspace, and the third is any renderer a site writes.

SQLite is bundled through `rusqlite`, passwords are hashed with `bcrypt`, and the full dependency list is in `m6-auth/Cargo.toml`.

A caller adds `m6-auth` and gets the row types, the error type and the JWT engine through it.
Section 2 opens the database those types describe.

## 2. Opening a database

**`Db::open` is the only constructor, and `Db::close` is the only destructor.**

```rust
pub fn open(path: &Path) -> Result<Self>;

pub fn close(self) -> std::result::Result<(), Box<(Connection, rusqlite::Error)>>;
```

`Db` is a newtype over one `rusqlite::Connection`, and `open` creates the file when it is absent.
The parent directory has to exist, because SQLite creates the file and leaves the directory to the caller.
`open` sets three pragmas, applies the schema from section 7, and returns the handle.
Table 2 gives the pragmas.
Take from it that the journal mode and the busy timeout make concurrent access work, and the foreign-key setting is what makes a delete cascade.

| pragma | value | why it is set |
|---|---|---|
| `journal_mode` | `WAL` | readers and one writer proceed together, so a login can read while the CLI writes |
| `busy_timeout` | `5000` | a statement waits 5 s for a lock before it returns `SQLITE_BUSY` |
| `foreign_keys` | `ON` | SQLite starts every connection with the cascade off, and section 7 relies on it |

**Table 2: the three pragmas `Db::open` sets, and why each one matters.**
The busy timeout is in milliseconds, so 5000 is 5 s.

`Db::open` runs `CREATE TABLE IF NOT EXISTS` for each table in the schema.
It creates a missing table and leaves an existing one untouched, and the crate records no schema version and holds no migration list.
A column added to the schema does not appear in a file that already has the table, so that file keeps the columns it had.
Section 7 gives the tables.

`close` consumes the handle and returns the `Connection` and the `rusqlite::Error` on failure, boxed, so a caller can retry with the same connection.
A process that holds one handle for its life can leave `close` to the operating system when it exits.
A renderer whose worker owns a handle calls `close` when the worker is destroyed, as section 8 shows.

The operations in sections 3 to 6 all take `&self`, so one open handle serves every call on its worker.
Section 3 starts with users.

## 3. Users

**A `User` is one row of `users` plus the names of the groups that row belongs to.**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub roles: Vec<String>,
    pub groups: Vec<String>,
    pub created_at: i64,
}
```

The fields map to the `users` row, and `groups` is the one field that comes from a join.
`id` is a v4 *UUID* (universally unique identifier) string that `user_create` generates.
`created_at` is Unix seconds.
`roles` is the user's role list, stored in the row as a *JSON* (JavaScript Object Notation) array.
`groups` is filled on every read, because each read joins `memberships`, and `user_create` returns an empty list because the new user is in no group.

Table 3 gives the eight user operations.
Take from it that the three reads return `None` for an absent name, and the writes return `UserNotFound` or `UserExists`.

| method | signature | what it does |
|---|---|---|
| `user_create` | `(&self, username: &str, password: &str, roles: &[&str]) -> Result<User>` | checks the name first so a duplicate costs no bcrypt work, hashes the password at bcrypt cost 12, stores the roles as JSON, and returns `UserExists` when the name is taken |
| `user_get` | `(&self, username: &str) -> Result<Option<User>>` | reads one user by name, with groups |
| `user_get_by_id` | `(&self, id: &str) -> Result<Option<User>>` | reads one user by id, with groups |
| `user_list` | `(&self) -> Result<Vec<User>>` | reads every user, ordered by username |
| `user_delete` | `(&self, username: &str) -> Result<()>` | deletes the row and returns `UserNotFound` when no row matched, and the foreign keys delete its memberships, refresh-token rows and API-token rows, but not the tokens already issued, which stay valid until `exp` (see section 5.2) |
| `user_set_password` | `(&self, username: &str, password: &str) -> Result<()>` | replaces the hash with a new one at cost 12 |
| `user_set_roles` | `(&self, username: &str, roles: &[&str]) -> Result<()>` | replaces the whole role list, and an empty slice clears every role |
| `user_verify_password` | `(&self, username: &str, password: &str) -> Result<Option<User>>` | returns the user on a matching password, and `None` for a wrong password or an unknown user, and an unknown user returns without running bcrypt, so response time reveals whether a username exists |

**Table 3: the eight user operations, their signatures and what each one does.**
`user_verify_password` is the login primitive, and `m6-auth-server` calls it on `POST /auth/login`.

The membership rows behind `groups` have their own operations, and section 4 gives them.

## 4. Groups

**A `Group` is one row of `groups` plus the usernames in it.**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
}
```

The fields map to the `groups` row, and `members` comes from a join.
`id` is a v4 UUID string that `group_create` generates.
`members` holds usernames, is filled on every read, and is empty for a new group.

Table 4 gives the eight group operations.
Take from it that a read returns `None` for an absent name, and a membership call names a missing group or user with `GroupNotFound` or `UserNotFound`.

| method | signature | what it does |
|---|---|---|
| `group_create` | `(&self, name: &str) -> Result<Group>` | creates the group and returns `GroupExists` when the name is taken |
| `group_get` | `(&self, name: &str) -> Result<Option<Group>>` | reads one group by name, with its members |
| `group_list` | `(&self) -> Result<Vec<Group>>` | reads every group, ordered by name |
| `group_delete` | `(&self, name: &str) -> Result<()>` | deletes the group and its memberships through the foreign keys, and leaves the users |
| `group_member_add` | `(&self, group: &str, username: &str) -> Result<()>` | resolves the group then the user, adds the membership with `INSERT OR IGNORE`, returns `GroupNotFound` or `UserNotFound` when either is missing, and leaves one row after a second identical call |
| `group_member_remove` | `(&self, group: &str, username: &str) -> Result<()>` | removes a membership, and an absent membership is not an error |
| `group_members` | `(&self, group: &str) -> Result<Vec<User>>` | returns the members as users, ordered by username |
| `user_groups` | `(&self, username: &str) -> Result<Vec<String>>` | returns the names of the groups a user belongs to |

**Table 4: the eight group operations, their signatures and what each one does.**
`group_members` returns `User` rows, so each member carries its roles and its own group list.

Tokens are the other rows with operations, and section 5 gives both kinds.

## 5. Tokens

**The crate stores two kinds of token record and ships the *JWT* (JSON Web Token) engine that mints the tokens those records hash.**
Both records store a hash of the token and never the token itself.
Section 5.1 gives refresh tokens, section 5.2 API tokens, and section 5.3 the engine.

### 5.1. Refresh tokens

**A refresh token is a long-lived JWT whose row lets the server revoke it.**

`m6-auth-server` stores the hash when a login succeeds, verifies it on refresh, and revokes it on logout.
Table 5 gives the four operations.
Take from it that verification answers with the user id, and an expired row answers `None`.

| method | signature | what it does |
|---|---|---|
| `refresh_token_store` | `(&self, user_id: &str, token_hash: &str, expires_at: i64) -> Result<()>` | inserts the row, and a repeat hash replaces it |
| `refresh_token_verify` | `(&self, token_hash: &str) -> Result<Option<String>>` | returns the user id when the row exists and `expires_at` is in the future |
| `refresh_token_revoke` | `(&self, token_hash: &str) -> Result<()>` | deletes the row, and an absent hash is not an error |
| `refresh_tokens_revoke_all` | `(&self, user_id: &str) -> Result<()>` | deletes every refresh-token row for one user |

**Table 5: the four refresh-token operations, their signatures and what each one does.**
The hash is the key, so the table never holds a usable token.

`refresh_token_verify` compares `expires_at` with the current Unix time and leaves an expired row in place.
`user_delete` reaches the same rows through the cascade in section 7.

### 5.2. API tokens

**An API token is a long-lived JWT that a CLI creates for a user, names, and lists later, and whose record the CLI can delete.**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub user_id: String,
    pub username: String,
    pub name: String,
    pub created_at: i64,
    pub expires_at: i64,
}
```

The fields map to the `api_tokens` row.
`id` is a v4 UUID string that `api_token_create` generates.
`username` is the name the caller passed, and the row stores `user_id`.
`token_hash` is not part of the type, so a listing never returns it.

Table 6 gives the three operations.
Take from it that create and list work by user, and revoke works by token id.

| method | signature | what it does |
|---|---|---|
| `api_token_create` | `(&self, user_id: &str, username: &str, name: &str, token_hash: &str, expires_at: i64) -> Result<ApiToken>` | inserts the row and returns the new token record |
| `api_token_list` | `(&self, username: &str) -> Result<Vec<ApiToken>>` | returns a user's tokens, newest first, and `UserNotFound` for an unknown name |
| `api_token_revoke` | `(&self, token_id: &str) -> Result<()>` | deletes the row and returns `ApiTokenNotFound` when the id is absent, and the token stays valid until `exp` |

**Table 6: the three API-token operations, their signatures and what each one does.**
`m6-auth-cli` mints the token with the JWT engine, stores its hash through `api_token_create`, and prints the token once.

**Revoking an API token does not make it stop working.**
A request is authorised by the JWT signature and `exp` alone, and nothing reads `api_tokens` to authorise it, so a revoked token, or one whose user `user_delete` removed, stays valid until its `exp`.
`m6-auth-cli token create` sets `exp` 365 days ahead unless `--ttl-days` says otherwise, so a short TTL is the only way to bound a token that leaks.
The same holds for an access token after logout or `user_delete`, because logout revokes refresh tokens only.
Only a refresh token is checked against the database, through `refresh_token_verify`, so revoking it takes effect at once.

### 5.3. The JWT engine

**`m6_auth::jwt` is the one part of the crate that knows about JWTs, the server mints and verifies tokens with it, and the CLI mints them.**
Table 7 gives its public items.
Take from it that the engine picks its algorithm from the key it is given, and the hash function is what the two token tables store.

| item | what it is |
|---|---|
| `JwtEngine::new(private_pem, public_pem, issuer)` | reads a PEM key pair and picks `ES256` (ECDSA on P-256) for an EC private key or `RS256` (RSA with SHA-256) for an RSA one |
| `encode_access`, `encode_refresh` | sign a token from the matching claims type |
| `decode_access`, `decode_refresh` | verify the signature, the issuer and `exp`, and return the claims |
| `AccessClaims` | `iss`, `sub` (the user id), `exp`, `iat`, `username`, `groups`, `roles` |
| `RefreshClaims` | `iss`, `sub`, `exp`, `iat`, `username`, and `type` set to `refresh` |
| `hash_token(token)` | the lowercase hex SHA-256 (*Secure Hash Algorithm*, 256-bit) of the token string |
| `now_secs()` | the current Unix time in seconds |
| `KeyAlgo` | `Rs256` or `Es256`, the algorithm the engine detected |

**Table 7: the public items in `m6_auth::jwt`, and what each one is.**
The claims types are the JSON bodies of the tokens, and `hash_token` is what keeps a token out of the database.

`JwtEngine::new` tries the EC parser first and the RSA parser second, and an error names the parser that failed.
`decode_access` and `decode_refresh` use the engine's algorithm and issuer and the library's default check of `exp`, so a token from another issuer or an expired token fails.

`hash_token` returns a plain SHA-256 digest, which suits a signed token with high entropy.

The crate reports every failure through one error type, and section 6 gives it.

## 6. Errors

**`AuthError` is the crate's only error type, and `Result<T>` is `std::result::Result<T, AuthError>`.**

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

    #[error("api token not found: {0}")]
    ApiTokenNotFound(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("password hashing error: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),
}
```

Table 8 gives the seven variants and the calls that return each.
Take from it that five variants name a row, and the other two wrap a failure from a dependency.

| variant | meaning | returned by |
|---|---|---|
| `UserNotFound` | no user has that name or id | `user_delete`, `user_set_password`, `user_set_roles`, `user_groups`, a membership call naming an unknown user, `api_token_list` |
| `UserExists` | the username is taken | `user_create` |
| `GroupNotFound` | no group has that name | `group_delete`, a membership call naming an unknown group |
| `GroupExists` | the group name is taken | `group_create` |
| `ApiTokenNotFound` | no API token has that id | `api_token_revoke` |
| `Db` | SQLite returned an error | any call that reads or writes |
| `Bcrypt` | the password hash or check failed | `user_create`, `user_set_password`, `user_verify_password` |

**Table 8: the seven `AuthError` variants, what each one means and which calls return it.**
The first five are answers about a row, and the last two carry a dependency's own error through `#[from]`.

`AuthError` implements `std::error::Error` through `thiserror`, so a caller converts it with one `map_err`, as section 8 does.
The `Db` and `Bcrypt` variants convert with `?` inside the crate, so a dependency failure needs no wrapping at each call site.

The rows these errors name live in five tables, and section 7 gives them.

## 7. The schema

**`Db::open` applies a fixed schema of five tables, and the schema is a constant in `lib.rs`.**

```sql
CREATE TABLE IF NOT EXISTS users (
    id         TEXT PRIMARY KEY,
    username   TEXT UNIQUE NOT NULL,
    password   TEXT NOT NULL,
    roles      TEXT NOT NULL,
    created_at INTEGER NOT NULL
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
    token_hash TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS api_tokens (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
```

Table 9 gives what each table holds and what deletes its rows.
Take from it that every foreign key cascades from a user or a group, so a deleted row leaves no orphan.

| table | holds | rows removed by |
|---|---|---|
| `users` | one row per user, with the bcrypt hash, the roles JSON and `created_at` | `user_delete` |
| `groups` | one row per group | `group_delete` |
| `memberships` | one row per user in a group, keyed by `(user_id, group_id)` | `user_delete` or `group_delete` |
| `refresh_tokens` | one row per refresh token, keyed by its hash | `user_delete`, or a revoke call |
| `api_tokens` | one row per API token, keyed by its id, with a unique hash | `user_delete`, or `api_token_revoke` |

**Table 9: the five tables, what each one holds and what removes its rows.**
The cascade depends on the `foreign_keys` pragma in Table 2.

`users.password` holds a bcrypt hash at cost 12, and `users.roles` holds a JSON array of strings.
The two `token_hash` columns hold the lowercase hex SHA-256 that `hash_token` returns, and `api_tokens.token_hash` has a UNIQUE constraint so one token cannot be stored twice.

Section 8 puts these tables and the operations together in a renderer.

## 8. A renderer that links it

**A renderer creates users itself by linking `m6-core` and `m6-auth` and opening the same database file the auth service uses.**
The renderer's manifest links both crates.

```toml
# render-signup/Cargo.toml
[dependencies]
m6-core = { path = "../../../m6/m6-core" }
m6-auth = { path = "../../../m6/m6-auth" }
```

`m6` is not published to a registry, so a path dependency needs a checkout in that position, and a git dependency pinned to a revision is the alternative.
Section 9 of `m6-app-anatomy.md` gives both forms and what each one costs.

A per-thread initialiser receives the configuration map and opens one `Db` per worker.

```rust
use m6_auth::{AuthError, Db};
use m6_core::prelude::*;
use std::path::Path;

struct Worker {
    db: Db,
}

fn open_worker(config: &Map<String, Value>, _global: &()) -> Result<Worker> {
    let path = config
        .get("auth_db")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::BadRequest("missing auth_db".to_string()))?;
    let db = Db::open(Path::new(path)).map_err(|e| Error::Other(e.into()))?;
    Ok(Worker { db })
}

fn show_form(req: &Request, _global: &(), _worker: &mut Worker) -> Result<Response> {
    Response::render("templates/signup.html", req)
}

fn signup(req: &Request, _global: &(), worker: &mut Worker) -> Result<Response> {
    let username = req.field("username")?;
    let password = req.field("password")?;
    match worker.db.user_create(&username, &password, &["user"]) {
        Ok(_) => Ok(Response::redirect("/login?registered=1")),
        Err(AuthError::UserExists(_)) => Response::render_with(
            "templates/signup.html",
            req,
            json!({"error": "Username taken"}),
        ),
        Err(e) => Err(Error::Other(e.into())),
    }
}

fn main() -> Result<()> {
    App::with_thread_state(open_worker)
        .on_destroy_thread(|worker| {
            let _ = worker.db.close();
        })
        .route_get("/signup", show_form)
        .route_post("/signup", signup)
        .run()
}
```

`auth_db` comes from the renderer's configuration map, so the database path stays out of the tracked configuration.
Core merges the file named by `secrets_file` into that map, and the key is read with `config.get("auth_db")`.

```toml
# render-signup.toml
secrets_file = "/etc/m6/render-signup.secrets.toml"

# render-signup.secrets.toml
auth_db = "/srv/example.com/data/auth.db"
```

The key is absolute because a per-thread initialiser receives no site directory to resolve a relative path against.
`m6-auth-server` and `m6-auth-cli` resolve a relative `[storage] path` against the configuration file's directory, so the renderer has to name the same file the server opened.
`m6-app-anatomy.md` section 5.3 gives the two state tiers and what each initialiser receives.

A shared handle is the other option: `m6-auth-server` keeps one `Db` in a `Mutex` under `App::with_global`, because `Db` is `Send` and not `Sync`.
A renderer that writes rarely can use the per-worker shape above, which needs no lock.

The processes above share one file, and section 9 gives the rules that keep that safe.

## 9. Concurrent access

**One database file serves every process that links this crate, and `Db::open` sets the pragmas that make that safe.**
Table 2 gives the pragmas, and section 8 gives how a worker holds its handle.

The crate keeps no copy of user data in memory.
Every call reads or writes SQLite, so a user created by `m6-auth-cli` or a renderer is visible to the server's next query.
That is what makes a signup page and a user tool agree without a restart.

Section 10 collects the document and points at the documents that own what this one leaves out.

## 10. Summary

**A caller of `m6-auth` opens one database, asks for the row it needs through `Db`, and converts an `AuthError` into its own error type.**
The schema is the crate's contract, and `Db::open` applies it without a migration step.
One file serves every process, under the pragmas in Table 2.
Every write is visible to the next query, so a renderer and the auth service agree on a user as soon as it is created.

Table 10 gives the documents that own what this one leaves out.
Take from it that the server and the CLI have their own references, and the lifecycle a renderer follows is in the anatomy document.

| document | what it owns |
|---|---|
| `m6-auth.md` | the auth service: configuration, HTTP endpoints, rate limiting and the token lifetimes |
| `m6-auth-cli.md` | the command line that uses these operations |
| `m6-app-anatomy.md` | the app lifecycle, the state tiers and how a renderer links core from another repository |
| `../README.md` | where this document sits in the documentation table |

**Table 10: further reading, and what each document owns.**
The first two are the references for the two crates in Table 1, and the third is the shape a renderer follows.

The schema in section 7 is the place to check before writing a row of your own.
