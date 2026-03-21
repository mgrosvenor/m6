pub mod jwt;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id:         String,
    pub username:   String,
    pub roles:      Vec<String>,
    pub groups:     Vec<String>, // populated by queries that join memberships
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id:      String,
    pub name:    String,
    pub members: Vec<String>, // usernames, populated by queries that join memberships
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id:         String,
    pub user_id:    String,
    pub username:   String,
    pub name:       String,
    pub created_at: i64,
    pub expires_at: i64,
}

// ── Error type ───────────────────────────────────────────────────────────────

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

pub type Result<T> = std::result::Result<T, AuthError>;

// ── Schema ───────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
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
";

const BCRYPT_COST: u32 = 12;

// ── Db ───────────────────────────────────────────────────────────────────────

pub struct Db(Connection);

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA busy_timeout=5000;
            PRAGMA foreign_keys=ON;
        ")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db(conn))
    }

    pub fn close(self) -> std::result::Result<(), (Connection, rusqlite::Error)> {
        self.0.close()
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    fn user_groups_by_id(&self, user_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.0.prepare(
            "SELECT g.name FROM groups g
             JOIN memberships m ON m.group_id = g.id
             WHERE m.user_id = ?1",
        )?;
        let groups: std::result::Result<Vec<String>, rusqlite::Error> = stmt
            .query_map(params![user_id], |row| row.get(0))?
            .collect();
        Ok(groups?)
    }

    fn row_to_user(
        &self,
        id: String,
        username: String,
        roles_json: String,
        created_at: i64,
    ) -> Result<User> {
        let roles: Vec<String> = serde_json::from_str(&roles_json).unwrap_or_default();
        let groups = self.user_groups_by_id(&id)?;
        Ok(User { id, username, roles, groups, created_at })
    }

    // ── User ops ─────────────────────────────────────────────────────────────

    pub fn user_create(&self, username: &str, password: &str, roles: &[&str]) -> Result<User> {
        // Check for duplicate username first to give a good error
        let exists: bool = self.0.query_row(
            "SELECT COUNT(*) FROM users WHERE username = ?1",
            params![username],
            |row| row.get::<_, i64>(0),
        )? > 0;
        if exists {
            return Err(AuthError::UserExists(username.to_string()));
        }

        let id = Uuid::new_v4().to_string();
        let hash = bcrypt::hash(password, BCRYPT_COST)?;
        let roles_vec: Vec<&str> = roles.to_vec();
        let roles_json = serde_json::to_string(&roles_vec).unwrap_or_else(|_| "[]".to_string());
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        self.0.execute(
            "INSERT INTO users (id, username, password, roles, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, username, hash, roles_json, created_at],
        )?;

        Ok(User {
            id,
            username: username.to_string(),
            roles: roles.iter().map(|s| s.to_string()).collect(),
            groups: vec![],
            created_at,
        })
    }

    pub fn user_get(&self, username: &str) -> Result<Option<User>> {
        let result = self.0.query_row(
            "SELECT id, username, roles, created_at FROM users WHERE username = ?1",
            params![username],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        );

        match result {
            Ok((id, uname, roles_json, created_at)) => {
                Ok(Some(self.row_to_user(id, uname, roles_json, created_at)?))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    pub fn user_get_by_id(&self, id: &str) -> Result<Option<User>> {
        let result = self.0.query_row(
            "SELECT id, username, roles, created_at FROM users WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        );

        match result {
            Ok((id, uname, roles_json, created_at)) => {
                Ok(Some(self.row_to_user(id, uname, roles_json, created_at)?))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    pub fn user_list(&self) -> Result<Vec<User>> {
        let mut stmt = self.0.prepare(
            "SELECT id, username, roles, created_at FROM users ORDER BY username",
        )?;
        let rows: std::result::Result<Vec<(String, String, String, i64)>, rusqlite::Error> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect();

        let mut users = Vec::new();
        for (id, uname, roles_json, created_at) in rows? {
            users.push(self.row_to_user(id, uname, roles_json, created_at)?);
        }
        Ok(users)
    }

    pub fn user_delete(&self, username: &str) -> Result<()> {
        let n = self.0.execute(
            "DELETE FROM users WHERE username = ?1",
            params![username],
        )?;
        if n == 0 {
            return Err(AuthError::UserNotFound(username.to_string()));
        }
        Ok(())
    }

    pub fn user_set_password(&self, username: &str, password: &str) -> Result<()> {
        let hash = bcrypt::hash(password, BCRYPT_COST)?;
        let n = self.0.execute(
            "UPDATE users SET password = ?1 WHERE username = ?2",
            params![hash, username],
        )?;
        if n == 0 {
            return Err(AuthError::UserNotFound(username.to_string()));
        }
        Ok(())
    }

    pub fn user_set_roles(&self, username: &str, roles: &[&str]) -> Result<()> {
        let roles_vec: Vec<&str> = roles.to_vec();
        let roles_json = serde_json::to_string(&roles_vec).unwrap_or_else(|_| "[]".to_string());
        let n = self.0.execute(
            "UPDATE users SET roles = ?1 WHERE username = ?2",
            params![roles_json, username],
        )?;
        if n == 0 {
            return Err(AuthError::UserNotFound(username.to_string()));
        }
        Ok(())
    }

    /// Returns `None` for wrong password OR non-existent user (no user enumeration).
    pub fn user_verify_password(&self, username: &str, password: &str) -> Result<Option<User>> {
        let result = self.0.query_row(
            "SELECT id, username, password, roles, created_at FROM users WHERE username = ?1",
            params![username],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        );

        match result {
            Ok((id, uname, hash, roles_json, created_at)) => {
                let valid = bcrypt::verify(password, &hash)?;
                if valid {
                    Ok(Some(self.row_to_user(id, uname, roles_json, created_at)?))
                } else {
                    Ok(None)
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    // ── Group ops ────────────────────────────────────────────────────────────

    pub fn group_create(&self, name: &str) -> Result<Group> {
        let exists: bool = self.0.query_row(
            "SELECT COUNT(*) FROM groups WHERE name = ?1",
            params![name],
            |row| row.get::<_, i64>(0),
        )? > 0;
        if exists {
            return Err(AuthError::GroupExists(name.to_string()));
        }

        let id = Uuid::new_v4().to_string();
        self.0.execute(
            "INSERT INTO groups (id, name) VALUES (?1, ?2)",
            params![id, name],
        )?;
        Ok(Group { id, name: name.to_string(), members: vec![] })
    }

    pub fn group_get(&self, name: &str) -> Result<Option<Group>> {
        let result = self.0.query_row(
            "SELECT id, name FROM groups WHERE name = ?1",
            params![name],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );

        match result {
            Ok((id, gname)) => {
                let members = self.group_member_names_by_id(&id)?;
                Ok(Some(Group { id, name: gname, members }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    pub fn group_list(&self) -> Result<Vec<Group>> {
        let mut stmt = self.0.prepare(
            "SELECT id, name FROM groups ORDER BY name",
        )?;
        let rows: std::result::Result<Vec<(String, String)>, rusqlite::Error> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect();

        let mut groups = Vec::new();
        for (id, name) in rows? {
            let members = self.group_member_names_by_id(&id)?;
            groups.push(Group { id, name, members });
        }
        Ok(groups)
    }

    pub fn group_delete(&self, name: &str) -> Result<()> {
        let n = self.0.execute(
            "DELETE FROM groups WHERE name = ?1",
            params![name],
        )?;
        if n == 0 {
            return Err(AuthError::GroupNotFound(name.to_string()));
        }
        Ok(())
    }

    pub fn group_member_add(&self, group: &str, username: &str) -> Result<()> {
        // Resolve group id
        let group_id = self.group_id_by_name(group)?;
        let user_id = self.user_id_by_name(username)?;

        self.0.execute(
            "INSERT OR IGNORE INTO memberships (user_id, group_id) VALUES (?1, ?2)",
            params![user_id, group_id],
        )?;
        Ok(())
    }

    pub fn group_member_remove(&self, group: &str, username: &str) -> Result<()> {
        let group_id = self.group_id_by_name(group)?;
        let user_id = self.user_id_by_name(username)?;

        self.0.execute(
            "DELETE FROM memberships WHERE user_id = ?1 AND group_id = ?2",
            params![user_id, group_id],
        )?;
        Ok(())
    }

    pub fn group_members(&self, group: &str) -> Result<Vec<User>> {
        let group_id = self.group_id_by_name(group)?;

        let mut stmt = self.0.prepare(
            "SELECT u.id, u.username, u.roles, u.created_at
             FROM users u
             JOIN memberships m ON m.user_id = u.id
             WHERE m.group_id = ?1
             ORDER BY u.username",
        )?;
        let rows: std::result::Result<Vec<(String, String, String, i64)>, rusqlite::Error> = stmt
            .query_map(params![group_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect();

        let mut users = Vec::new();
        for (id, uname, roles_json, created_at) in rows? {
            users.push(self.row_to_user(id, uname, roles_json, created_at)?);
        }
        Ok(users)
    }

    pub fn user_groups(&self, username: &str) -> Result<Vec<String>> {
        let user_id = self.user_id_by_name(username)?;
        self.user_groups_by_id(&user_id)
    }

    // ── Refresh token ops ────────────────────────────────────────────────────

    pub fn refresh_token_store(&self, user_id: &str, token_hash: &str, expires_at: i64) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO refresh_tokens (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
            params![token_hash, user_id, expires_at],
        )?;
        Ok(())
    }

    /// Returns the user_id if the token exists and has not expired.
    pub fn refresh_token_verify(&self, token_hash: &str) -> Result<Option<String>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let result = self.0.query_row(
            "SELECT user_id FROM refresh_tokens WHERE token_hash = ?1 AND expires_at > ?2",
            params![token_hash, now],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(user_id) => Ok(Some(user_id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    pub fn refresh_token_revoke(&self, token_hash: &str) -> Result<()> {
        self.0.execute(
            "DELETE FROM refresh_tokens WHERE token_hash = ?1",
            params![token_hash],
        )?;
        Ok(())
    }

    pub fn refresh_tokens_revoke_all(&self, user_id: &str) -> Result<()> {
        self.0.execute(
            "DELETE FROM refresh_tokens WHERE user_id = ?1",
            params![user_id],
        )?;
        Ok(())
    }

    // ── API token ops ────────────────────────────────────────────────────────

    pub fn api_token_create(
        &self,
        user_id: &str,
        username: &str,
        name: &str,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<ApiToken> {
        let id = Uuid::new_v4().to_string();
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.0.execute(
            "INSERT INTO api_tokens (id, user_id, name, token_hash, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, user_id, name, token_hash, created_at, expires_at],
        )?;
        Ok(ApiToken { id, user_id: user_id.to_string(), username: username.to_string(), name: name.to_string(), created_at, expires_at })
    }

    pub fn api_token_list(&self, username: &str) -> Result<Vec<ApiToken>> {
        let user_id = self.user_id_by_name(username)?;
        let mut stmt = self.0.prepare(
            "SELECT id, user_id, name, created_at, expires_at
             FROM api_tokens WHERE user_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows: std::result::Result<Vec<(String, String, String, i64, i64)>, rusqlite::Error> = stmt
            .query_map(params![user_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect();
        Ok(rows?.into_iter().map(|(id, uid, name, ca, ea)| ApiToken {
            id, user_id: uid, username: username.to_string(), name, created_at: ca, expires_at: ea,
        }).collect())
    }

    pub fn api_token_revoke(&self, token_id: &str) -> Result<()> {
        let n = self.0.execute(
            "DELETE FROM api_tokens WHERE id = ?1",
            params![token_id],
        )?;
        if n == 0 {
            return Err(AuthError::ApiTokenNotFound(token_id.to_string()));
        }
        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    fn group_id_by_name(&self, name: &str) -> Result<String> {
        let result = self.0.query_row(
            "SELECT id FROM groups WHERE name = ?1",
            params![name],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(AuthError::GroupNotFound(name.to_string())),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    fn user_id_by_name(&self, username: &str) -> Result<String> {
        let result = self.0.query_row(
            "SELECT id FROM users WHERE username = ?1",
            params![username],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(AuthError::UserNotFound(username.to_string())),
            Err(e) => Err(AuthError::Db(e)),
        }
    }

    fn group_member_names_by_id(&self, group_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.0.prepare(
            "SELECT u.username FROM users u
             JOIN memberships m ON m.user_id = u.id
             WHERE m.group_id = ?1
             ORDER BY u.username",
        )?;
        let names: std::result::Result<Vec<String>, rusqlite::Error> = stmt
            .query_map(params![group_id], |row| row.get(0))?
            .collect();
        Ok(names?)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn tmp_db() -> (Db, NamedTempFile) {
        let f = NamedTempFile::new().expect("tempfile");
        let db = Db::open(f.path()).expect("open db");
        (db, f)
    }

    // ── WAL mode ─────────────────────────────────────────────────────────────

    #[test]
    fn test_wal_mode() {
        let (db, _f) = tmp_db();
        let mode: String = db.0.query_row(
            "PRAGMA journal_mode",
            [],
            |row| row.get(0),
        ).expect("pragma");
        assert_eq!(mode, "wal");
    }

    // ── User create / get ─────────────────────────────────────────────────────

    #[test]
    fn test_user_create_and_get() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "hunter2", &["admin", "user"]).expect("create");
        assert_eq!(u.username, "alice");
        assert!(u.roles.contains(&"admin".to_string()));
        assert!(u.roles.contains(&"user".to_string()));

        let got = db.user_get("alice").expect("get").expect("some");
        assert_eq!(got.id, u.id);
        assert_eq!(got.username, "alice");
    }

    #[test]
    fn test_user_create_duplicate() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "pass1", &[]).expect("first create");
        let err = db.user_create("alice", "pass2", &[]).expect_err("should fail");
        assert!(matches!(err, AuthError::UserExists(_)));
    }

    #[test]
    fn test_user_get_nonexistent() {
        let (db, _f) = tmp_db();
        let r = db.user_get("nobody").expect("query ok");
        assert!(r.is_none());
    }

    #[test]
    fn test_user_get_by_id() {
        let (db, _f) = tmp_db();
        let u = db.user_create("bob", "pass", &["user"]).expect("create");
        let got = db.user_get_by_id(&u.id).expect("get").expect("some");
        assert_eq!(got.username, "bob");
    }

    #[test]
    fn test_user_list() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "p1", &[]).expect("c1");
        db.user_create("bob", "p2", &[]).expect("c2");
        let list = db.user_list().expect("list");
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|u| u.username == "alice"));
        assert!(list.iter().any(|u| u.username == "bob"));
    }

    // ── user_delete cascade ───────────────────────────────────────────────────

    #[test]
    fn test_user_delete_cascades() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "pass", &[]).expect("create");
        let _g = db.group_create("admins").expect("group");
        db.group_member_add("admins", "alice").expect("add member");
        db.refresh_token_store(&u.id, "tok_hash_1", i64::MAX).expect("store token");

        // Confirm membership and token exist
        let members = db.group_members("admins").expect("members");
        assert_eq!(members.len(), 1);

        db.user_delete("alice").expect("delete");

        // Group still exists
        let g = db.group_get("admins").expect("get").expect("some");
        assert_eq!(g.members.len(), 0);

        // Token gone — refresh_token_verify should return None
        let tv = db.refresh_token_verify("tok_hash_1").expect("verify");
        assert!(tv.is_none());
    }

    #[test]
    fn test_user_delete_not_found() {
        let (db, _f) = tmp_db();
        let err = db.user_delete("ghost").expect_err("should fail");
        assert!(matches!(err, AuthError::UserNotFound(_)));
    }

    // ── user_set_password / user_set_roles ────────────────────────────────────

    #[test]
    fn test_user_set_password() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "old_pass", &[]).expect("create");
        db.user_set_password("alice", "new_pass").expect("set pw");

        assert!(db.user_verify_password("alice", "old_pass").expect("verify").is_none());
        assert!(db.user_verify_password("alice", "new_pass").expect("verify").is_some());
    }

    #[test]
    fn test_user_set_roles() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "pass", &["user"]).expect("create");
        db.user_set_roles("alice", &["admin", "mod"]).expect("set roles");
        let u = db.user_get("alice").expect("get").expect("some");
        assert!(u.roles.contains(&"admin".to_string()));
        assert!(u.roles.contains(&"mod".to_string()));
        assert!(!u.roles.contains(&"user".to_string()));
    }

    // ── user_verify_password ─────────────────────────────────────────────────

    #[test]
    fn test_verify_correct_password() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "correct", &["user"]).expect("create");
        let u = db.user_verify_password("alice", "correct").expect("verify").expect("some");
        assert_eq!(u.username, "alice");
    }

    #[test]
    fn test_verify_wrong_password_returns_none() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "correct", &[]).expect("create");
        let r = db.user_verify_password("alice", "wrong").expect("verify");
        assert!(r.is_none());
    }

    #[test]
    fn test_verify_nonexistent_user_returns_none() {
        let (db, _f) = tmp_db();
        let r = db.user_verify_password("nobody", "pass").expect("verify");
        assert!(r.is_none());
    }

    // ── Group ops ─────────────────────────────────────────────────────────────

    #[test]
    fn test_group_create_and_get() {
        let (db, _f) = tmp_db();
        let g = db.group_create("editors").expect("create");
        assert_eq!(g.name, "editors");
        assert!(g.members.is_empty());

        let got = db.group_get("editors").expect("get").expect("some");
        assert_eq!(got.id, g.id);
    }

    #[test]
    fn test_group_create_duplicate() {
        let (db, _f) = tmp_db();
        db.group_create("editors").expect("first");
        let err = db.group_create("editors").expect_err("should fail");
        assert!(matches!(err, AuthError::GroupExists(_)));
    }

    #[test]
    fn test_group_get_nonexistent() {
        let (db, _f) = tmp_db();
        let r = db.group_get("missing").expect("query ok");
        assert!(r.is_none());
    }

    #[test]
    fn test_group_list() {
        let (db, _f) = tmp_db();
        db.group_create("a").expect("c1");
        db.group_create("b").expect("c2");
        let list = db.group_list().expect("list");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_group_delete() {
        let (db, _f) = tmp_db();
        db.group_create("tmp").expect("create");
        db.group_delete("tmp").expect("delete");
        let r = db.group_get("tmp").expect("query");
        assert!(r.is_none());
    }

    #[test]
    fn test_group_delete_not_found() {
        let (db, _f) = tmp_db();
        let err = db.group_delete("ghost").expect_err("should fail");
        assert!(matches!(err, AuthError::GroupNotFound(_)));
    }

    // ── Membership ops ────────────────────────────────────────────────────────

    #[test]
    fn test_group_member_add_remove() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "p", &[]).expect("user");
        db.group_create("team").expect("group");
        db.group_member_add("team", "alice").expect("add");

        let members = db.group_members("team").expect("members");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].username, "alice");

        let groups = db.user_groups("alice").expect("groups");
        assert!(groups.contains(&"team".to_string()));

        db.group_member_remove("team", "alice").expect("remove");
        let members = db.group_members("team").expect("after remove");
        assert!(members.is_empty());
    }

    #[test]
    fn test_group_member_add_idempotent() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "p", &[]).expect("user");
        db.group_create("team").expect("group");
        db.group_member_add("team", "alice").expect("first add");
        db.group_member_add("team", "alice").expect("second add idempotent");
        let members = db.group_members("team").expect("members");
        assert_eq!(members.len(), 1);
    }

    #[test]
    fn test_group_member_add_unknown_group() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "p", &[]).expect("user");
        let err = db.group_member_add("nonexistent", "alice").expect_err("should fail");
        assert!(matches!(err, AuthError::GroupNotFound(_)));
    }

    #[test]
    fn test_group_member_add_unknown_user() {
        let (db, _f) = tmp_db();
        db.group_create("team").expect("group");
        let err = db.group_member_add("team", "ghost").expect_err("should fail");
        assert!(matches!(err, AuthError::UserNotFound(_)));
    }

    #[test]
    fn test_user_groups_populated_on_get() {
        let (db, _f) = tmp_db();
        db.user_create("alice", "p", &[]).expect("user");
        db.group_create("alpha").expect("g1");
        db.group_create("beta").expect("g2");
        db.group_member_add("alpha", "alice").expect("add1");
        db.group_member_add("beta", "alice").expect("add2");
        let u = db.user_get("alice").expect("get").expect("some");
        assert!(u.groups.contains(&"alpha".to_string()));
        assert!(u.groups.contains(&"beta".to_string()));
    }

    // ── Refresh token ops ─────────────────────────────────────────────────────

    #[test]
    fn test_refresh_token_store_and_verify() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "p", &[]).expect("user");
        db.refresh_token_store(&u.id, "hash1", i64::MAX).expect("store");
        let uid = db.refresh_token_verify("hash1").expect("verify").expect("some");
        assert_eq!(uid, u.id);
    }

    #[test]
    fn test_refresh_token_expired() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "p", &[]).expect("user");
        // expires_at = 1 (far in the past)
        db.refresh_token_store(&u.id, "hash_exp", 1).expect("store");
        let r = db.refresh_token_verify("hash_exp").expect("verify");
        assert!(r.is_none());
    }

    #[test]
    fn test_refresh_token_revoke() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "p", &[]).expect("user");
        db.refresh_token_store(&u.id, "tok", i64::MAX).expect("store");
        db.refresh_token_revoke("tok").expect("revoke");
        let r = db.refresh_token_verify("tok").expect("verify");
        assert!(r.is_none());
    }

    #[test]
    fn test_refresh_tokens_revoke_all() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "p", &[]).expect("user");
        db.refresh_token_store(&u.id, "tok1", i64::MAX).expect("s1");
        db.refresh_token_store(&u.id, "tok2", i64::MAX).expect("s2");
        db.refresh_tokens_revoke_all(&u.id).expect("revoke all");
        assert!(db.refresh_token_verify("tok1").expect("v1").is_none());
        assert!(db.refresh_token_verify("tok2").expect("v2").is_none());
    }

    // ── Concurrent open (WAL allows multiple readers + one writer) ───────────

    #[test]
    fn test_concurrent_open_no_corruption() {
        let f = NamedTempFile::new().expect("tempfile");
        let path = f.path().to_path_buf();

        let db1 = Db::open(&path).expect("db1");
        let db2 = Db::open(&path).expect("db2");

        // Interleave writes
        db1.user_create("alice", "pass_a", &["admin"]).expect("alice");
        db2.user_create("bob", "pass_b", &["user"]).expect("bob");
        db1.user_create("carol", "pass_c", &[]).expect("carol");

        // Both connections see all rows
        let list1 = db1.user_list().expect("list1");
        let list2 = db2.user_list().expect("list2");
        assert_eq!(list1.len(), 3);
        assert_eq!(list2.len(), 3);
    }

    // ── API token ops ─────────────────────────────────────────────────────────

    #[test]
    fn test_api_token_create_and_list() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "pass", &[]).expect("user");
        let tok = db.api_token_create(&u.id, "alice", "ci-deploy", "hash1", i64::MAX)
            .expect("create");
        assert_eq!(tok.user_id, u.id);
        assert_eq!(tok.username, "alice");
        assert_eq!(tok.name, "ci-deploy");

        let list = db.api_token_list("alice").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, tok.id);
        assert_eq!(list[0].name, "ci-deploy");
    }

    #[test]
    fn test_api_token_list_multiple() {
        let (db, _f) = tmp_db();
        let u = db.user_create("bob", "pass", &[]).expect("user");
        db.api_token_create(&u.id, "bob", "token-a", "hash_a", i64::MAX).expect("a");
        db.api_token_create(&u.id, "bob", "token-b", "hash_b", i64::MAX).expect("b");
        let list = db.api_token_list("bob").expect("list");
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|t| t.name == "token-a"));
        assert!(list.iter().any(|t| t.name == "token-b"));
    }

    #[test]
    fn test_api_token_list_empty() {
        let (db, _f) = tmp_db();
        db.user_create("carol", "pass", &[]).expect("user");
        let list = db.api_token_list("carol").expect("list");
        assert!(list.is_empty());
    }

    #[test]
    fn test_api_token_list_unknown_user() {
        let (db, _f) = tmp_db();
        let err = db.api_token_list("ghost").expect_err("should fail");
        assert!(matches!(err, AuthError::UserNotFound(_)));
    }

    #[test]
    fn test_api_token_revoke() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "pass", &[]).expect("user");
        let tok = db.api_token_create(&u.id, "alice", "my-token", "hashX", i64::MAX)
            .expect("create");
        db.api_token_revoke(&tok.id).expect("revoke");
        let list = db.api_token_list("alice").expect("list");
        assert!(list.is_empty());
    }

    #[test]
    fn test_api_token_revoke_not_found() {
        let (db, _f) = tmp_db();
        let err = db.api_token_revoke("nonexistent-id").expect_err("should fail");
        assert!(matches!(err, AuthError::ApiTokenNotFound(_)));
    }

    #[test]
    fn test_api_token_cascade_on_user_delete() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "pass", &[]).expect("user");
        db.api_token_create(&u.id, "alice", "tok", "hashY", i64::MAX).expect("create");
        db.user_delete("alice").expect("delete");
        // After user deletion, api_token_list errors with UserNotFound (user is gone)
        let err = db.api_token_list("alice").expect_err("user gone");
        assert!(matches!(err, AuthError::UserNotFound(_)));
    }

    #[test]
    fn test_api_token_hash_unique() {
        let (db, _f) = tmp_db();
        let u = db.user_create("alice", "pass", &[]).expect("user");
        db.api_token_create(&u.id, "alice", "tok-1", "same_hash", i64::MAX).expect("first");
        // Same hash must fail (UNIQUE constraint on token_hash)
        let err = db.api_token_create(&u.id, "alice", "tok-2", "same_hash", i64::MAX)
            .expect_err("duplicate hash");
        assert!(matches!(err, AuthError::Db(_)));
    }
}
