//! First-party SQLite store for the event index. The JSONL log remains the
//! append-only authority; this index supports task/kind/seq queries and
//! survives restarts.

use std::path::Path;
use std::sync::Mutex;

use msk_events::Event;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct EventQuery {
    pub task: Option<String>,
    pub kind: Option<String>,
    pub from_seq: Option<u64>,
    /// Clamped to 1..=10000; defaults to 1000.
    pub limit: usize,
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            task: None,
            kind: None,
            from_seq: None,
            limit: 1000,
        }
    }
}

pub struct SqliteStore {
    connection: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS events (
                 id TEXT PRIMARY KEY,
                 seq INTEGER NOT NULL,
                 ts TEXT NOT NULL,
                 task TEXT,
                 kind TEXT NOT NULL,
                 actor TEXT NOT NULL,
                 parents TEXT NOT NULL,
                 schema_version INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_task_seq ON events(task, seq);
             CREATE INDEX IF NOT EXISTS idx_events_kind_seq ON events(kind, seq);",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Inserts an event. Returns false when the event id was already indexed,
    /// which is normal when a task log is replayed after a daemon restart.
    pub fn append(&self, event: &Event) -> Result<bool, StoreError> {
        let parents = serde_json::to_string(&event.parents)?;
        let data = serde_json::to_string(&event.data)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let changed = connection.execute(
            "INSERT OR IGNORE INTO events
             (id, seq, ts, task, kind, actor, parents, schema_version, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                event.id,
                event.seq as i64,
                event.ts,
                event.task,
                event.kind,
                event.actor,
                parents,
                event.schema_version as i64,
                data,
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn query(&self, query: &EventQuery) -> Result<Vec<Event>, StoreError> {
        let mut sql = String::from(
            "SELECT id, seq, ts, task, kind, actor, parents, schema_version, data
             FROM events WHERE 1=1",
        );
        let mut parameters: Vec<SqlValue> = Vec::new();
        if let Some(task) = &query.task {
            sql.push_str(" AND task = ?");
            parameters.push(SqlValue::Text(task.clone()));
        }
        if let Some(kind) = &query.kind {
            sql.push_str(" AND kind = ?");
            parameters.push(SqlValue::Text(kind.clone()));
        }
        if let Some(from_seq) = query.from_seq {
            sql.push_str(" AND seq >= ?");
            parameters.push(SqlValue::Integer(from_seq as i64));
        }
        let limit = if query.limit == 0 {
            1000
        } else {
            query.limit.clamp(1, 10_000)
        };
        sql.push_str(" ORDER BY rowid ASC LIMIT ?");
        parameters.push(SqlValue::Integer(limit as i64));

        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), |row| {
            let parents: String = row.get(6)?;
            let data: String = row.get(8)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                parents,
                row.get::<_, i64>(7)?,
                data,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (id, seq, ts, task, kind, actor, parents, schema_version, data) = row?;
            events.push(Event {
                schema_version: schema_version as u32,
                seq: seq as u64,
                id,
                ts,
                task,
                kind,
                actor,
                parents: serde_json::from_str(&parents)?,
                data: serde_json::from_str::<Value>(&data)?,
            });
        }
        Ok(events)
    }

    pub fn count(&self) -> Result<u64, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn latest_seq(&self, task: Option<&str>) -> Result<u64, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let seq: i64 = match task {
            Some(task) => connection.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE task = ?1",
                [task],
                |row| row.get(0),
            )?,
            None => {
                connection.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
                    row.get(0)
                })?
            }
        };
        Ok(seq as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_db() -> (std::path::PathBuf, SqliteStore) {
        let path = std::env::temp_dir().join(format!(
            "msk-store-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let store = SqliteStore::open(&path).unwrap();
        (path, store)
    }

    #[test]
    fn indexes_and_queries_events() {
        let (path, store) = temp_db();
        let mut first =
            Event::new("tool.call", "agent", json!({"tool": "exec"})).with_task("task-1");
        first.seq = 1;
        let mut second = Event::new("turn.end", "agent", json!({"status": "awaiting_input"}))
            .with_task("task-1");
        second.seq = 2;
        let mut third =
            Event::new("tool.call", "agent", json!({"tool": "echo"})).with_task("task-2");
        third.seq = 3;
        assert!(store.append(&first).unwrap());
        assert!(store.append(&second).unwrap());
        assert!(store.append(&third).unwrap());
        assert!(
            !store.append(&first).unwrap(),
            "duplicate id must be ignored"
        );
        assert_eq!(store.count().unwrap(), 3);
        assert_eq!(store.latest_seq(Some("task-1")).unwrap(), 2);

        let task_events = store
            .query(&EventQuery {
                task: Some("task-1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(task_events.len(), 2);
        assert_eq!(task_events[0].kind, "tool.call");
        assert_eq!(task_events[1].kind, "turn.end");

        let tool_events = store
            .query(&EventQuery {
                kind: Some("tool.call".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(tool_events.len(), 2);

        let from_two = store
            .query(&EventQuery {
                from_seq: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(from_two.len(), 2);
        assert_eq!(store.count().unwrap(), 3);
        assert_eq!(store.latest_seq(Some("task-1")).unwrap(), 2);
        drop(store);

        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.count().unwrap(), 3);
        let _ = std::fs::remove_file(&path);
    }
}
