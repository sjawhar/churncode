//! The SQLite database: processed commits, aggregated deletions, every tracked file's line
//! origins at the newest processed commit, and the settings the history was read with.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::Error;
use crate::track::{Applied, Origin, State};

const SCHEMA_VERSION: &str = "1";

const SCHEMA: &str = "
    CREATE TABLE meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE commits (
        idx INTEGER PRIMARY KEY,
        sha TEXT NOT NULL UNIQUE,
        committer_time INTEGER NOT NULL,
        subject TEXT NOT NULL,
        lines_added INTEGER NOT NULL
    );
    CREATE TABLE deletions (
        origin_idx INTEGER NOT NULL,
        deleter_idx INTEGER NOT NULL,
        n INTEGER NOT NULL,
        PRIMARY KEY (origin_idx, deleter_idx)
    ) WITHOUT ROWID;
    CREATE TABLE file_state (
        path BLOB PRIMARY KEY,
        origins BLOB NOT NULL
    ) WITHOUT ROWID;
";

/// The options that decide which lines exist at all. A database answers for one choice of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Sorted and without duplicates.
    pub excludes: Vec<String>,
    pub base: Option<String>,
}

/// How far the database has read.
#[derive(Debug, Clone)]
pub(crate) struct Progress {
    pub settings: Settings,
    /// The newest processed commit, or the base commit when none has been processed after it.
    pub tip: String,
    /// The origin of the newest processed commit; 0 when none has been processed.
    pub tip_idx: Origin,
}

/// One processed commit, as the database stores it.
pub(crate) struct CommitRow<'a> {
    pub idx: Origin,
    pub sha: &'a str,
    pub committer_time: i64,
    pub subject: &'a str,
    pub applied: &'a Applied,
}

/// Opens a database for an update, creating it (and its tables) when it does not exist.
pub(crate) fn open_for_update(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(path)?;
    let has_tables: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table')",
        [],
        |row| row.get(0),
    )?;
    if has_tables {
        check_schema(&conn, path)?;
    } else {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA} INSERT INTO meta (key, value) VALUES ('schema', '{SCHEMA_VERSION}'); \
             COMMIT;"
        ))?;
    }
    Ok(conn)
}

/// Opens an existing database without writing to it.
pub(crate) fn open_read_only(path: &Path) -> Result<Connection, Error> {
    if !path.exists() {
        return Err(Error::Refused(format!(
            "{} does not exist; build it with churncode update",
            path.display()
        )));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    check_schema(&conn, path)?;
    Ok(conn)
}

fn check_schema(conn: &Connection, path: &Path) -> Result<(), Error> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'meta')",
        [],
        |row| row.get(0),
    )?;
    let version = if has_meta {
        meta(conn, "schema")?
    } else {
        None
    };
    match version.as_deref() {
        Some(SCHEMA_VERSION) => Ok(()),
        Some(other) => Err(Error::Refused(format!(
            "{} has database schema {other}; this churncode reads schema {SCHEMA_VERSION}",
            path.display()
        ))),
        None => Err(Error::Refused(format!(
            "{} is not a churncode database",
            path.display()
        ))),
    }
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn set_meta(tx: &Transaction, key: &str, value: &str) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// What the database has read so far; `None` for a database no update has written yet.
pub(crate) fn progress(conn: &Connection) -> Result<Option<Progress>, Error> {
    let Some(tip) = meta(conn, "tip")? else {
        return Ok(None);
    };
    let corrupt = |key: &str| Error::Refused(format!("the database's {key} entry is unreadable"));
    let excludes = meta(conn, "excludes")?.ok_or_else(|| corrupt("excludes"))?;
    let excludes: Vec<String> = serde_json::from_str(&excludes).map_err(|_| corrupt("excludes"))?;
    let base = meta(conn, "base")?.ok_or_else(|| corrupt("base"))?;
    let tip_idx = meta(conn, "tip_idx")?
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| corrupt("tip_idx"))?;
    Ok(Some(Progress {
        settings: Settings {
            excludes,
            base: (!base.is_empty()).then_some(base),
        },
        tip,
        tip_idx,
    }))
}

/// Every tracked file's line origins.
pub(crate) fn load_state(conn: &Connection) -> Result<State, Error> {
    let mut stmt = conn.prepare("SELECT path, origins FROM file_state")?;
    let mut rows = stmt.query([])?;
    let mut files = HashMap::new();
    while let Some(row) = rows.next()? {
        let path: Vec<u8> = row.get(0)?;
        let origins = decode_origins(row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?);
        files.insert(path, origins);
    }
    Ok(State {
        files,
        ..State::default()
    })
}

/// One file's line origins, `None` when the database tracks no lines at that path.
pub(crate) fn file_origins(conn: &Connection, path: &[u8]) -> Result<Option<Vec<Origin>>, Error> {
    Ok(conn
        .query_row(
            "SELECT origins FROM file_state WHERE path = ?1",
            [path],
            |row| Ok(decode_origins(row.get_ref(0)?.as_blob()?)),
        )
        .optional()?)
}

/// The commit id of every processed commit, by origin.
pub(crate) fn commit_ids(conn: &Connection) -> Result<HashMap<String, Origin>, Error> {
    let mut stmt = conn.prepare("SELECT sha, idx FROM commits")?;
    let ids = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// Records processed commits, the files they touched and the new progress, in the caller's
/// transaction.
pub(crate) fn append(
    tx: &Transaction,
    settings: &Settings,
    commits: &[CommitRow],
    state: &State,
    tip: &str,
    tip_idx: Origin,
) -> Result<(), Error> {
    {
        let mut insert_commit = tx.prepare(
            "INSERT INTO commits (idx, sha, committer_time, subject, lines_added)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut insert_deletion =
            tx.prepare("INSERT INTO deletions (origin_idx, deleter_idx, n) VALUES (?1, ?2, ?3)")?;
        for commit in commits {
            insert_commit.execute(params![
                commit.idx,
                commit.sha,
                commit.committer_time,
                commit.subject,
                commit.applied.lines_added as i64,
            ])?;
            for (&origin, &n) in &commit.applied.deleted {
                insert_deletion.execute(params![origin, commit.idx, n as i64])?;
            }
        }
        let mut upsert_file =
            tx.prepare("INSERT OR REPLACE INTO file_state (path, origins) VALUES (?1, ?2)")?;
        let mut delete_file = tx.prepare("DELETE FROM file_state WHERE path = ?1")?;
        let mut encoded = Vec::new();
        for path in &state.touched {
            match state.files.get(path) {
                Some(origins) if !origins.is_empty() => {
                    encode_origins(origins, &mut encoded);
                    upsert_file.execute(params![path, encoded])?;
                }
                _ => {
                    delete_file.execute([path])?;
                }
            }
        }
    }
    let excludes = serde_json::to_string(&settings.excludes).expect("strings serialize");
    set_meta(tx, "excludes", &excludes)?;
    set_meta(tx, "base", settings.base.as_deref().unwrap_or(""))?;
    set_meta(tx, "tip", tip)?;
    set_meta(tx, "tip_idx", &tip_idx.to_string())?;
    Ok(())
}

fn encode_origins(origins: &[Origin], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(origins.len() * 4);
    for origin in origins {
        out.extend_from_slice(&origin.to_le_bytes());
    }
}

fn decode_origins(blob: &[u8]) -> Vec<Origin> {
    let (chunks, _) = blob.as_chunks::<4>();
    chunks.iter().map(|&c| Origin::from_le_bytes(c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_round_trip() {
        let origins = vec![0, 1, 1, 70_000, u32::MAX];
        let mut encoded = Vec::new();
        encode_origins(&origins, &mut encoded);
        assert_eq!(decode_origins(&encoded), origins);
    }

    #[test]
    fn refuses_foreign_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        let err = open_for_update(&path).unwrap_err();
        assert!(
            err.to_string().contains("not a churncode database"),
            "{err}"
        );
        let err = open_read_only(&dir.path().join("missing.db")).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }
}
