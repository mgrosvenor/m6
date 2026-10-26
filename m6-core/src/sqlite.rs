//! One SQLite file with the house settings, versioned migrations and
//! append-only tables.
//!
//! Each service that keeps data in SQLite used to open it with its own pragmas
//! and write its own upgrade code. This module is the one place for both. It is
//! behind the off-by-default `sqlite` feature, so a service that does not enable
//! it links nothing new. [`rusqlite`] is re-exported, so a service links m6-core
//! and nothing else.
//!
//! # The house settings
//!
//! [`open`] applies the settings in Table 1. Each one has a reason, and each is
//! set explicitly so that a change of default in a later rusqlite or SQLite
//! cannot change behaviour silently.
//!
//! | # | setting | value | reason |
//! |---|---|---|---|
//! | 1 | `journal_mode` | `DELETE` (rollback journal) | The journal file is removed at every commit, so at rest the database is exactly one file. Copying that file copies everything. WAL keeps `-wal` and `-shm` files beside it, and a copy of the main file alone can miss committed work. [`Journal::Wal`] is available for services that want it |
//! | 2 | `busy_timeout` | 5 s ([`BUSY_TIMEOUT`]) | A second connection waits for a lock instead of failing at once with `SQLITE_BUSY` |
//! | 3 | `foreign_keys` | on | SQLite has them off in every new connection, so `ON DELETE CASCADE` does nothing without it. The value is read back after it is set |
//! | 4 | `recursive_triggers` | on | SQLite fires DELETE triggers for rows removed by `INSERT OR REPLACE` only with this on. Without it the append-only rule can be bypassed |
//!
//! *Table 1: the settings [`open`] applies. The journal mode is read back, and
//! a mode that did not take effect is an error.*
//!
//! # Migrations
//!
//! A migration list is a slice of [`Migration`], numbered 1, 2, 3 in order. The
//! schema version lives in `PRAGMA user_version`, which is four bytes in the
//! file header, needs no extra table and changes in the same transaction as the
//! migration. [`migrate`] checks the list, then runs each migration newer than
//! the file in its own `BEGIN IMMEDIATE` transaction, and re-reads the version
//! inside that transaction. Two processes, or two worker threads, that open the
//! same file at once therefore apply each migration exactly once.
//!
//! - A migration is never edited once released. A change is a new migration.
//! - A failing migration rolls back, and the file stays at the previous version.
//! - A file newer than the list is refused with [`SqliteError::NewerThanCode`],
//!   so an older binary never writes into a schema it does not know.
//! - A migration may not contain statements that SQLite refuses or ignores
//!   inside a transaction (`PRAGMA journal_mode`, `PRAGMA foreign_keys`,
//!   `VACUUM`).
//!
//! # Append-only tables
//!
//! [`append_only`] creates two triggers, `<table>_append_only_update` and
//! `<table>_append_only_delete`, that abort UPDATE and DELETE on the table.
//! Call it after [`migrate`] on every open. It puts the triggers back if a
//! migration or a person dropped them, and a migration that must rewrite rows
//! drops them by name first. The triggers guard against mistakes in code. They
//! give no protection against anyone who can write to the file, because that
//! person can drop them. A table that is ever pruned must not be append-only,
//! since the rule refuses [`prune_oldest`] as well.
//!
//! # Size caps
//!
//! [`used_bytes`] reports the bytes of the file that hold data, and
//! [`prune_oldest`] deletes the oldest rows of a table in short transactions
//! until the file is at or under a target. Deleted pages go to SQLite's free
//! list and are reused, so the file does not shrink, and `used_bytes` is the
//! number to compare with a cap.
//!
//! # Startup
//!
//! ```
//! use m6_core::sqlite::{self, Migration};
//!
//! const MIGRATIONS: &[Migration] = &[Migration {
//!     version: 1,
//!     sql: "CREATE TABLE events (id INTEGER PRIMARY KEY, at TEXT NOT NULL) STRICT;",
//! }];
//!
//! # fn main() -> Result<(), sqlite::SqliteError> {
//! let dir = tempfile::tempdir().unwrap();
//! let path = dir.path().join("events.db");
//! let mut conn = sqlite::open(&path)?;
//! sqlite::migrate(&mut conn, MIGRATIONS)?;
//! sqlite::append_only(&conn, "events")?;
//! assert_eq!(sqlite::schema_version(&conn)?, 1);
//! # Ok(())
//! # }
//! ```
//!
//! A service keeps the connection in one `Arc<Mutex<Connection>>`, or opens one
//! per worker through `App::with_thread_state`. Both are safe with the busy
//! timeout and the migration rules above.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use rusqlite;
pub use rusqlite::{params, Connection, OptionalExtension, Transaction};

use rusqlite::{DatabaseName, TransactionBehavior};

/// The busy timeout [`open`] sets: 5 seconds.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The journal mode of a database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Journal {
    /// `journal_mode = DELETE`. The database is one file at rest.
    Rollback,
    /// `journal_mode = WAL`. `-wal` and `-shm` files sit beside the database.
    Wal,
}

/// Settings for [`OpenOptions::open`]. The default is Table 1 in the module
/// documentation.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    busy_timeout: Duration,
    journal: Journal,
    foreign_keys: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            busy_timeout: BUSY_TIMEOUT,
            journal: Journal::Rollback,
            foreign_keys: true,
        }
    }
}

impl OpenOptions {
    /// How long a connection waits for a lock before failing with `SQLITE_BUSY`.
    pub fn busy_timeout(mut self, d: Duration) -> Self {
        self.busy_timeout = d;
        self
    }

    /// The journal mode to put the file in.
    pub fn journal(mut self, j: Journal) -> Self {
        self.journal = j;
        self
    }

    /// Whether foreign key constraints are enforced.
    pub fn foreign_keys(mut self, on: bool) -> Self {
        self.foreign_keys = on;
        self
    }

    /// Open the file, creating it if it is missing, and apply the settings.
    pub fn open(&self, path: &Path) -> Result<Connection, SqliteError> {
        let conn = Connection::open(path).map_err(|source| SqliteError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        if conn.is_readonly(DatabaseName::Main)? {
            return Err(SqliteError::ReadOnly(path.to_path_buf()));
        }
        // First, so that changing the journal mode waits for a lock instead of
        // failing at once.
        conn.busy_timeout(self.busy_timeout)?;

        let wanted = match self.journal {
            Journal::Rollback => "delete",
            Journal::Wal => "wal",
        };
        let got: String =
            conn.pragma_update_and_check(None, "journal_mode", wanted, |r| r.get(0))?;
        if !got.eq_ignore_ascii_case(wanted) {
            return Err(SqliteError::JournalMode { wanted, got });
        }

        conn.pragma_update(None, "foreign_keys", self.foreign_keys)?;
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if fk != self.foreign_keys {
            return Err(SqliteError::ForeignKeys);
        }

        conn.pragma_update(None, "recursive_triggers", true)?;
        Ok(conn)
    }
}

/// Open a database file with the house settings: [`OpenOptions::default`].
pub fn open(path: &Path) -> Result<Connection, SqliteError> {
    OpenOptions::default().open(path)
}

/// One step of a schema. `version` counts 1, 2, 3 down the list.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

/// The schema version stored in the file (`PRAGMA user_version`).
pub fn schema_version(conn: &Connection) -> Result<u32, SqliteError> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))?)
}

/// Bring the file up to the last migration in the list. Returns the version
/// now in the file. See the module documentation for the rules.
pub fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<u32, SqliteError> {
    for (index, m) in migrations.iter().enumerate() {
        if u64::from(m.version) != index as u64 + 1 {
            return Err(SqliteError::MigrationList {
                index,
                version: m.version,
            });
        }
    }
    let code = migrations.len() as u32;
    let cur = schema_version(conn)?;
    if cur > code {
        return Err(SqliteError::NewerThanCode { db: cur, code });
    }
    for m in migrations.iter().filter(|m| m.version > cur) {
        let fail = |source| SqliteError::Migration {
            version: m.version,
            source,
        };
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(fail)?;
        let now: u32 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(fail)?;
        if now >= m.version {
            continue;
        }
        tx.execute_batch(m.sql).map_err(fail)?;
        tx.pragma_update(None, "user_version", m.version)
            .map_err(fail)?;
        tx.commit().map_err(fail)?;
    }
    schema_version(conn)
}

/// `[A-Za-z_][A-Za-z0-9_]*`, the only table names placed into SQL here.
fn plain_name(s: &str) -> bool {
    let b = s.as_bytes();
    match b.first() {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {}
        _ => return false,
    }
    b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn checked_name(table: &str) -> Result<&str, SqliteError> {
    if plain_name(table) {
        Ok(table)
    } else {
        Err(SqliteError::BadName(table.to_string()))
    }
}

/// Make `table` append-only: UPDATE and DELETE on it abort. Safe to call on
/// every open.
pub fn append_only(conn: &Connection, table: &str) -> Result<(), SqliteError> {
    let t = checked_name(table)?;
    conn.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS \"{t}_append_only_update\" BEFORE UPDATE ON \"{t}\" \
         BEGIN SELECT RAISE(ABORT, '{t} is append-only: UPDATE refused'); END; \
         CREATE TRIGGER IF NOT EXISTS \"{t}_append_only_delete\" BEFORE DELETE ON \"{t}\" \
         BEGIN SELECT RAISE(ABORT, '{t} is append-only: DELETE refused'); END;"
    ))?;
    Ok(())
}

/// Whether both append-only triggers exist for `table`.
pub fn is_append_only(conn: &Connection, table: &str) -> Result<bool, SqliteError> {
    let t = checked_name(table)?;
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 \
         AND name IN (?2, ?3)",
        params![
            t,
            format!("{t}_append_only_update"),
            format!("{t}_append_only_delete")
        ],
        |r| r.get(0),
    )?;
    Ok(n == 2)
}

/// Bytes of the file that hold data: `(page_count - freelist_count) * page_size`.
pub fn used_bytes(conn: &Connection) -> Result<u64, SqliteError> {
    let pages: u64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let free: u64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let size: u64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
    Ok(pages.saturating_sub(free) * size)
}

/// Delete the oldest rows of `table` (lowest `rowid` first), `batch` rows per
/// transaction, until [`used_bytes`] is at or under `target_bytes` or the table
/// is empty. Returns the number of rows deleted.
///
/// A `batch` of 0 is treated as 1. The table needs a `rowid`, and must not be
/// append-only: the rule refuses the delete and the call returns the error.
pub fn prune_oldest(
    conn: &mut Connection,
    table: &str,
    batch: u32,
    target_bytes: u64,
) -> Result<u64, SqliteError> {
    let t = checked_name(table)?;
    let sql = format!(
        "DELETE FROM \"{t}\" WHERE rowid IN (SELECT rowid FROM \"{t}\" ORDER BY rowid LIMIT ?1)"
    );
    let batch = batch.max(1);
    let mut deleted = 0u64;
    while used_bytes(conn)? > target_bytes {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(&sql, [batch])?;
        tx.commit()?;
        if n == 0 {
            break;
        }
        deleted += n as u64;
    }
    Ok(deleted)
}

/// What can go wrong opening, migrating or guarding a database.
#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    #[error("opening {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("{0} is read-only")]
    ReadOnly(PathBuf),
    #[error("journal mode is {got}, wanted {wanted}")]
    JournalMode { wanted: &'static str, got: String },
    #[error("foreign_keys did not take effect")]
    ForeignKeys,
    #[error("migration list entry {index} has version {version}, expected {}", index + 1)]
    MigrationList { index: usize, version: u32 },
    #[error("database is at schema version {db}, this code knows up to {code}")]
    NewerThanCode { db: u32, code: u32 },
    #[error("migration {version} failed: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },
    #[error("not a plain table name: {0:?}")]
    BadName(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl From<SqliteError> for crate::Error {
    fn from(e: SqliteError) -> Self {
        crate::Error::Other(anyhow::Error::new(e))
    }
}

impl From<rusqlite::Error> for crate::Error {
    fn from(e: rusqlite::Error) -> Self {
        crate::Error::Other(anyhow::Error::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ErrorCode;
    use std::sync::{Arc, Barrier};

    fn db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        (dir, path)
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn table_exists(c: &Connection, name: &str) -> bool {
        count(
            c,
            &format!("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{name}'"),
        ) == 1
    }

    fn pragma_s(c: &Connection, name: &str) -> String {
        c.pragma_query_value(None, name, |r| r.get::<_, rusqlite::types::Value>(0))
            .map(|v| match v {
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Text(s) => s,
                other => format!("{other:?}"),
            })
            .unwrap()
    }

    fn violation(e: &rusqlite::Error, needle: &str) -> bool {
        match e {
            rusqlite::Error::SqliteFailure(f, msg) => {
                f.code == ErrorCode::ConstraintViolation
                    && msg.as_deref().unwrap_or("").contains(needle)
            }
            _ => false,
        }
    }

    const M1: Migration = Migration {
        version: 1,
        sql: "CREATE TABLE a(x); INSERT INTO a VALUES (1);",
    };
    const M2: Migration = Migration {
        version: 2,
        sql: "CREATE TABLE b(x);",
    };

    // T1
    #[test]
    fn open_uses_a_rollback_journal_and_leaves_one_file() {
        let (dir, path) = db();
        let c = open(&path).unwrap();
        assert_eq!(pragma_s(&c, "journal_mode"), "delete");
        c.execute_batch("CREATE TABLE a(x); INSERT INTO a VALUES (1);")
            .unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["t.db".to_string()]);
    }

    // T2
    #[test]
    fn open_sets_the_busy_timeout() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        assert_eq!(pragma_s(&c, "busy_timeout"), "5000");
        let (_d2, path2) = db();
        let c = OpenOptions::default()
            .busy_timeout(Duration::from_millis(250))
            .open(&path2)
            .unwrap();
        assert_eq!(pragma_s(&c, "busy_timeout"), "250");
    }

    // T3
    #[test]
    fn open_turns_foreign_keys_on() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        assert_eq!(pragma_s(&c, "foreign_keys"), "1");
        c.execute_batch(
            "CREATE TABLE p(id INTEGER PRIMARY KEY); \
             CREATE TABLE ch(pid INTEGER REFERENCES p(id));",
        )
        .unwrap();
        match c.execute("INSERT INTO ch VALUES (9)", []).unwrap_err() {
            rusqlite::Error::SqliteFailure(f, _) => {
                assert_eq!(f.code, ErrorCode::ConstraintViolation)
            }
            e => panic!("wrong error: {e}"),
        }
        let (_d2, path2) = db();
        let c = OpenOptions::default()
            .foreign_keys(false)
            .open(&path2)
            .unwrap();
        c.execute_batch(
            "CREATE TABLE p(id INTEGER PRIMARY KEY); \
             CREATE TABLE ch(pid INTEGER REFERENCES p(id));",
        )
        .unwrap();
        c.execute("INSERT INTO ch VALUES (9)", []).unwrap();
    }

    // T4
    #[test]
    fn open_turns_recursive_triggers_on() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        assert_eq!(pragma_s(&c, "recursive_triggers"), "1");
    }

    // T5
    #[test]
    fn wal_option_gives_wal() {
        let (_d, path) = db();
        let c = OpenOptions::default()
            .journal(Journal::Wal)
            .open(&path)
            .unwrap();
        assert_eq!(pragma_s(&c, "journal_mode"), "wal");
    }

    // T6
    #[test]
    fn open_converts_a_wal_file_to_rollback() {
        let (dir, path) = db();
        let c = OpenOptions::default()
            .journal(Journal::Wal)
            .open(&path)
            .unwrap();
        c.execute_batch("CREATE TABLE a(x); INSERT INTO a VALUES (1);")
            .unwrap();
        drop(c);
        let c = open(&path).unwrap();
        assert_eq!(pragma_s(&c, "journal_mode"), "delete");
        assert_eq!(count(&c, "SELECT count(*) FROM a"), 1);
        drop(c);
        assert!(!dir.path().join("t.db-wal").exists());
        assert!(!dir.path().join("t.db-shm").exists());
    }

    // T7
    #[test]
    fn open_refuses_a_read_only_file() {
        let (_d, path) = db();
        drop(open(&path).unwrap());
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        // Root can write a read-only file, so this test cannot measure anything
        // there. A check that cannot measure must fail, not pass quietly.
        assert!(
            std::fs::OpenOptions::new().write(true).open(&path).is_err(),
            "this test cannot run as root: the read-only file is still writable"
        );
        match open(&path) {
            Err(SqliteError::ReadOnly(p)) => assert_eq!(p, path),
            other => panic!("expected ReadOnly, got {other:?}"),
        }
    }

    // T8
    #[test]
    fn open_error_names_the_path() {
        let (dir, _p) = db();
        let path = dir.path().join("missing").join("t.db");
        match open(&path) {
            Err(e @ SqliteError::Open { .. }) => {
                assert!(e.to_string().contains(&path.display().to_string()))
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    // T9
    #[test]
    fn migrate_a_fresh_file_applies_every_migration() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        assert_eq!(migrate(&mut c, &[M1, M2]).unwrap(), 2);
        assert_eq!(schema_version(&c).unwrap(), 2);
        assert!(table_exists(&c, "a") && table_exists(&c, "b"));
    }

    // T10
    #[test]
    fn migrate_twice_applies_nothing_the_second_time() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        migrate(&mut c, &[M1]).unwrap();
        migrate(&mut c, &[M1]).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM a"), 1);
    }

    // T11
    #[test]
    fn migrate_applies_only_the_new_ones() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        migrate(&mut c, &[M1]).unwrap();
        assert_eq!(migrate(&mut c, &[M1, M2]).unwrap(), 2);
        assert_eq!(count(&c, "SELECT count(*) FROM a"), 1);
        assert!(table_exists(&c, "b"));
    }

    // T12
    #[test]
    fn a_failing_migration_rolls_back_and_keeps_the_version() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        let bad = Migration {
            version: 2,
            sql: "CREATE TABLE b(x); SELEC 1;",
        };
        match migrate(&mut c, &[M1, bad]) {
            Err(SqliteError::Migration { version: 2, .. }) => {}
            other => panic!("expected Migration 2, got {other:?}"),
        }
        assert_eq!(schema_version(&c).unwrap(), 1);
        assert!(!table_exists(&c, "b"));
        assert_eq!(count(&c, "SELECT count(*) FROM a"), 1);
    }

    // T13
    #[test]
    fn migrate_refuses_a_database_newer_than_the_code() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        c.pragma_update(None, "user_version", 3).unwrap();
        match migrate(&mut c, &[M1, M2]) {
            Err(SqliteError::NewerThanCode { db: 3, code: 2 }) => {}
            other => panic!("expected NewerThanCode, got {other:?}"),
        }
        assert_eq!(schema_version(&c).unwrap(), 3);
        assert!(!table_exists(&c, "a"));
    }

    // T14
    #[test]
    fn migrate_refuses_a_list_out_of_order() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        let v3 = Migration {
            version: 3,
            sql: "CREATE TABLE c(x);",
        };
        match migrate(&mut c, &[M1, v3]) {
            Err(SqliteError::MigrationList {
                index: 1,
                version: 3,
            }) => {}
            other => panic!("expected MigrationList, got {other:?}"),
        }
        match migrate(&mut c, &[M2]) {
            Err(SqliteError::MigrationList { index: 0, .. }) => {}
            other => panic!("expected MigrationList, got {other:?}"),
        }
        assert_eq!(schema_version(&c).unwrap(), 0);
        assert!(!table_exists(&c, "a") && !table_exists(&c, "b"));
    }

    // T15
    #[test]
    fn concurrent_migrate_applies_each_migration_once() {
        let (_d, path) = db();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (b, p) = (barrier.clone(), path.clone());
                std::thread::spawn(move || {
                    let mut c = open(&p).unwrap();
                    b.wait();
                    migrate(&mut c, &[M1, M2]).unwrap()
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), 2);
        }
        let c = open(&path).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM a"), 1);
    }

    // T16
    #[test]
    fn append_only_refuses_update_and_delete_and_allows_insert() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        c.execute_batch("CREATE TABLE t(x INTEGER);").unwrap();
        append_only(&c, "t").unwrap();
        c.execute("INSERT INTO t VALUES (1)", []).unwrap();
        let e = c.execute("UPDATE t SET x = 2", []).unwrap_err();
        assert!(violation(&e, "append-only: UPDATE refused"), "{e}");
        let e = c.execute("DELETE FROM t WHERE x = 1", []).unwrap_err();
        assert!(violation(&e, "append-only: DELETE refused"), "{e}");
        assert_eq!(count(&c, "SELECT x FROM t"), 1);
        assert_eq!(count(&c, "SELECT count(*) FROM t"), 1);
    }

    // T17
    #[test]
    fn append_only_refuses_the_side_doors() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, x INTEGER);")
            .unwrap();
        append_only(&c, "t").unwrap();
        c.execute("INSERT INTO t VALUES (1, 10)", []).unwrap();
        assert!(c
            .execute("INSERT OR REPLACE INTO t VALUES (1, 20)", [])
            .is_err());
        assert!(c
            .execute(
                "INSERT INTO t VALUES (1, 30) ON CONFLICT(id) DO UPDATE SET x = 30",
                []
            )
            .is_err());
        assert!(c.execute("DELETE FROM t", []).is_err());
        assert_eq!(count(&c, "SELECT count(*) FROM t"), 1);
        assert_eq!(count(&c, "SELECT x FROM t"), 10);
    }

    // T18
    #[test]
    fn append_only_is_idempotent_and_reported() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        c.execute_batch("CREATE TABLE t(x); CREATE TABLE u(x);")
            .unwrap();
        append_only(&c, "t").unwrap();
        append_only(&c, "t").unwrap();
        assert!(is_append_only(&c, "t").unwrap());
        assert!(!is_append_only(&c, "u").unwrap());
    }

    // T19
    #[test]
    fn append_only_survives_reopen() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        append_only(&c, "t").unwrap();
        drop(c);
        let c = open(&path).unwrap();
        assert!(c.execute("UPDATE t SET x = 2", []).is_err());
    }

    // T20
    #[test]
    fn append_only_refuses_names_that_are_not_plain() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        for bad in ["", "1a", "a-b", "a b", "t; DROP TABLE x", "\"t\""] {
            match append_only(&c, bad) {
                Err(SqliteError::BadName(n)) => assert_eq!(n, bad),
                other => panic!("{bad:?}: expected BadName, got {other:?}"),
            }
        }
        assert_eq!(
            count(
                &c,
                "SELECT count(*) FROM sqlite_master WHERE type='trigger'"
            ),
            0
        );
    }

    // T21
    #[test]
    fn append_only_on_a_missing_table_is_an_error() {
        let (_d, path) = db();
        let c = open(&path).unwrap();
        assert!(matches!(
            append_only(&c, "nope"),
            Err(SqliteError::Sqlite(_))
        ));
    }

    // T22
    #[test]
    fn errors_convert_into_the_core_error() {
        fn f(p: &Path) -> crate::Result<()> {
            let c = open(p)?;
            c.execute("SELEC", [])?;
            Ok(())
        }
        let (dir, path) = db();
        assert!(matches!(f(&path), Err(crate::Error::Other(_))));
        let bad = dir.path().join("missing").join("t.db");
        assert!(matches!(f(&bad), Err(crate::Error::Other(_))));
    }

    fn filled(c: &mut Connection, rows: i64) {
        c.execute_batch("CREATE TABLE log(id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        let tx = c.transaction().unwrap();
        for i in 1..=rows {
            tx.execute(
                "INSERT INTO log(id, body) VALUES (?1, ?2)",
                params![i, "x".repeat(500)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    // U1
    #[test]
    fn used_bytes_counts_data_pages_and_not_free_ones() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        let empty = used_bytes(&c).unwrap();
        filled(&mut c, 400);
        let full = used_bytes(&c).unwrap();
        assert!(full > empty + 100_000, "{empty} -> {full}");
        c.execute("DELETE FROM log", []).unwrap();
        let after = used_bytes(&c).unwrap();
        assert!(after < full / 2, "{full} -> {after}");
    }

    // U2
    #[test]
    fn prune_oldest_deletes_oldest_first_down_to_the_target() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        filled(&mut c, 400);
        let full = used_bytes(&c).unwrap();
        let target = full / 2;
        let n = prune_oldest(&mut c, "log", 10, target).unwrap();
        assert!(n > 0);
        assert!(used_bytes(&c).unwrap() <= target);
        assert_eq!(count(&c, "SELECT max(id) FROM log"), 400);
        assert_eq!(
            count(&c, "SELECT min(id) FROM log"),
            n as i64 + 1,
            "the oldest rows go first"
        );
        assert_eq!(prune_oldest(&mut c, "log", 10, target).unwrap(), 0);
    }

    // U3
    #[test]
    fn prune_oldest_stops_when_the_table_is_empty() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        filled(&mut c, 20);
        let n = prune_oldest(&mut c, "log", 0, 0).unwrap();
        assert_eq!(n, 20);
        assert_eq!(count(&c, "SELECT count(*) FROM log"), 0);
    }

    // U4
    #[test]
    fn prune_oldest_is_refused_by_an_append_only_table() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        filled(&mut c, 20);
        append_only(&c, "log").unwrap();
        assert!(matches!(
            prune_oldest(&mut c, "log", 5, 0),
            Err(SqliteError::Sqlite(_))
        ));
        assert_eq!(count(&c, "SELECT count(*) FROM log"), 20);
    }

    // U5
    #[test]
    fn prune_oldest_refuses_names_that_are_not_plain() {
        let (_d, path) = db();
        let mut c = open(&path).unwrap();
        assert!(matches!(
            prune_oldest(&mut c, "log; DROP TABLE log", 5, 0),
            Err(SqliteError::BadName(_))
        ));
    }
}
