use agnt_core::{Message, MessageStore, StoreError, ToolLog};
use rusqlite::{params, Connection};
use std::sync::Mutex;
use tracing::{debug, info};

fn io_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Io(e.to_string())
}

/// SQLite-backed session store.
///
/// Wraps `rusqlite::Connection` in a `Mutex` so the store is `Send + Sync`
/// and can be shared across threads without the caller needing to add their
/// own interior mutability.
///
/// The connection is opened in WAL mode with `synchronous=NORMAL` for
/// significantly better throughput on the agent hot path. Prepared
/// statements are cached across calls.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;

        // WAL mode + relaxed fsync on the hot path. `journal_mode` is a
        // PRAGMA that returns the new mode as a row, so we use `query_row`.
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", &"NORMAL")
            .map_err(|e| e.to_string())?;
        info!(path = %path, journal_mode = %mode, "agnt-store opened");

        conn.execute(
            "CREATE TABLE IF NOT EXISTS messages (
                session TEXT NOT NULL,
                idx     INTEGER NOT NULL,
                json    TEXT NOT NULL,
                PRIMARY KEY (session, idx)
            )",
            [],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tool_calls (
                session     TEXT NOT NULL,
                ts          INTEGER NOT NULL,
                name        TEXT NOT NULL,
                args        TEXT NOT NULL,
                result      TEXT NOT NULL,
                duration_us INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage (
                session           TEXT NOT NULL,
                message_idx       INTEGER NOT NULL,
                prompt_tokens     INTEGER,
                completion_tokens INTEGER,
                total_tokens      INTEGER,
                PRIMARY KEY (session, message_idx)
            )",
            [],
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|e| format!("store mutex poisoned: {}", e))
    }

    /// Returns the current `journal_mode` PRAGMA value (e.g. "wal").
    /// Primarily used by tests to confirm WAL is active.
    pub fn journal_mode(&self) -> Result<String, String> {
        let conn = self.lock()?;
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())
    }

    pub fn log_tool(
        &self,
        session: &str,
        name: &str,
        args: &str,
        result: &str,
        duration_us: u64,
    ) -> Result<(), String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO tool_calls (session, ts, name, args, result, duration_us)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|e| e.to_string())?;
        stmt.execute(params![session, ts, name, args, result, duration_us as i64])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load(&self, session: &str) -> Result<Vec<Message>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached("SELECT json FROM messages WHERE session = ?1 ORDER BY idx")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            let s = r.map_err(|e| e.to_string())?;
            let m: Message = serde_json::from_str(&s).map_err(|e| e.to_string())?;
            out.push(m);
        }
        Ok(out)
    }

    /// Append a single message. Uses a single INSERT…SELECT to compute the
    /// next `idx` in one roundtrip instead of a separate SELECT + INSERT.
    pub fn append(&self, session: &str, msg: &Message) -> Result<(), String> {
        let json = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO messages (session, idx, json)
                 SELECT ?1, COALESCE(MAX(idx), -1) + 1, ?2
                 FROM messages
                 WHERE session = ?1",
            )
            .map_err(|e| e.to_string())?;
        stmt.execute(params![session, json])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Append many messages in a single transaction. Reduces fsync cost
    /// from 2+N per turn down to 1.
    #[tracing::instrument(skip(self, messages), fields(session = %session, count = messages.len()))]
    pub fn append_many(&self, session: &str, messages: &[Message]) -> Result<(), String> {
        if messages.is_empty() {
            return Ok(());
        }
        // Pre-serialize outside the lock.
        let jsons: Vec<String> = messages
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;

        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        {
            // Find starting idx once.
            let mut next: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(idx), -1) + 1 FROM messages WHERE session = ?1",
                    params![session],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let mut stmt = tx
                .prepare_cached("INSERT INTO messages (session, idx, json) VALUES (?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for json in &jsons {
                stmt.execute(params![session, next, json])
                    .map_err(|e| e.to_string())?;
                next += 1;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Run an arbitrary closure inside a single BEGIN/COMMIT transaction.
    /// The closure receives a borrowed `rusqlite::Transaction` and can issue
    /// multiple writes that will all commit together (or roll back on error).
    pub fn with_transaction<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, String>,
    {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let out = f(&tx)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(out)
    }

    #[tracing::instrument(skip(self), fields(session = %session))]
    pub fn clear(&self, session: &str) -> Result<(), String> {
        debug!("clearing session");
        let conn = self.lock()?;
        {
            let mut stmt = conn
                .prepare_cached("DELETE FROM messages WHERE session = ?1")
                .map_err(|e| e.to_string())?;
            stmt.execute(params![session]).map_err(|e| e.to_string())?;
        }
        {
            let mut stmt = conn
                .prepare_cached("DELETE FROM tool_calls WHERE session = ?1")
                .map_err(|e| e.to_string())?;
            stmt.execute(params![session]).map_err(|e| e.to_string())?;
        }
        {
            let mut stmt = conn
                .prepare_cached("DELETE FROM usage WHERE session = ?1")
                .map_err(|e| e.to_string())?;
            stmt.execute(params![session]).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Per-tool latency stats for a session: (name, count, avg_us, max_us).
    pub fn stats(&self, session: &str) -> Result<Vec<(String, i64, i64, i64)>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT name, COUNT(*), CAST(AVG(duration_us) AS INTEGER), MAX(duration_us)
                 FROM tool_calls
                 WHERE session = ?1
                 GROUP BY name
                 ORDER BY COUNT(*) DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// Record token usage for a single message in a session.
    pub fn log_usage(
        &self,
        session: &str,
        message_idx: i64,
        prompt: u32,
        completion: u32,
    ) -> Result<(), String> {
        let total = prompt as i64 + completion as i64;
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached(
                "INSERT OR REPLACE INTO usage
                    (session, message_idx, prompt_tokens, completion_tokens, total_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| e.to_string())?;
        stmt.execute(params![
            session,
            message_idx,
            prompt as i64,
            completion as i64,
            total
        ])
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Sum token usage across a session: `(prompt_sum, completion_sum, total_sum)`.
    pub fn usage_total(&self, session: &str) -> Result<(i64, i64, i64), String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT
                    COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0)
                 FROM usage
                 WHERE session = ?1",
            )
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![session], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())
    }
}

impl MessageStore for Store {
    fn load(&self, session: &str) -> Result<Vec<Message>, StoreError> {
        Store::load(self, session).map_err(io_err)
    }

    fn append(&self, session: &str, message: &Message) -> Result<(), StoreError> {
        Store::append(self, session, message).map_err(io_err)
    }

    fn log_tool(&self, session: &str, log: &ToolLog<'_>) -> Result<(), StoreError> {
        Store::log_tool(
            self,
            session,
            log.name,
            log.args,
            log.result,
            log.duration_us,
        )
        .map_err(io_err)
    }

    fn clear(&self, session: &str) -> Result<(), StoreError> {
        Store::clear(self, session).map_err(io_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnt_core::Message;

    fn tmp_path(name: &str) -> String {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        dir.join(format!("agnt-store-{}-{}-{}.db", name, pid, nanos))
            .to_string_lossy()
            .into_owned()
    }

    fn user(content: &str) -> Message {
        Message {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            usage: None,
        }
    }

    #[test]
    fn wal_mode_is_active() {
        let path = tmp_path("wal");
        let store = Store::open(&path).unwrap();
        let mode = store.journal_mode().unwrap().to_lowercase();
        assert_eq!(mode, "wal", "expected WAL journal mode, got {}", mode);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_and_load_roundtrip() {
        let path = tmp_path("append");
        let store = Store::open(&path).unwrap();
        store.append("s1", &user("hello")).unwrap();
        store.append("s1", &user("world")).unwrap();
        let msgs = store.load("s1").unwrap();
        assert_eq!(msgs.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_many_batches_in_one_tx() {
        let path = tmp_path("batch");
        let store = Store::open(&path).unwrap();
        let batch = vec![user("a"), user("b"), user("c")];
        store.append_many("s1", &batch).unwrap();
        // Append another single message — idx must continue after the batch.
        store.append("s1", &user("d")).unwrap();
        let msgs = store.load("s1").unwrap();
        assert_eq!(msgs.len(), 4);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_many_empty_is_noop() {
        let path = tmp_path("empty");
        let store = Store::open(&path).unwrap();
        store.append_many("s1", &[]).unwrap();
        assert!(store.load("s1").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn with_transaction_commits() {
        let path = tmp_path("tx");
        let store = Store::open(&path).unwrap();
        store
            .with_transaction(|tx| {
                tx.execute(
                    "INSERT INTO messages (session, idx, json) VALUES (?1, ?2, ?3)",
                    params!["s1", 0i64, "{\"role\":\"user\",\"content\":\"hi\"}"],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load("s1").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn with_transaction_rolls_back_on_err() {
        let path = tmp_path("rollback");
        let store = Store::open(&path).unwrap();
        let res: Result<(), String> = store.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO messages (session, idx, json) VALUES (?1, ?2, ?3)",
                params!["s1", 0i64, "{\"role\":\"user\",\"content\":\"hi\"}"],
            )
            .map_err(|e| e.to_string())?;
            Err("boom".to_string())
        });
        assert!(res.is_err());
        assert!(store.load("s1").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_tool_and_stats() {
        let path = tmp_path("tool");
        let store = Store::open(&path).unwrap();
        store.log_tool("s1", "fs_read", "{}", "ok", 100).unwrap();
        store.log_tool("s1", "fs_read", "{}", "ok", 300).unwrap();
        store.log_tool("s1", "http", "{}", "ok", 500).unwrap();
        let stats = store.stats("s1").unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].0, "fs_read");
        assert_eq!(stats[0].1, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn usage_log_and_total() {
        let path = tmp_path("usage");
        let store = Store::open(&path).unwrap();
        store.log_usage("s1", 0, 100, 50).unwrap();
        store.log_usage("s1", 1, 200, 80).unwrap();
        let (p, c, t) = store.usage_total("s1").unwrap();
        assert_eq!(p, 300);
        assert_eq!(c, 130);
        assert_eq!(t, 430);

        // Different session isolated.
        let (p2, c2, t2) = store.usage_total("s2").unwrap();
        assert_eq!((p2, c2, t2), (0, 0, 0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_wipes_usage_too() {
        let path = tmp_path("clear");
        let store = Store::open(&path).unwrap();
        store.append("s1", &user("a")).unwrap();
        store.log_tool("s1", "t", "{}", "ok", 1).unwrap();
        store.log_usage("s1", 0, 10, 20).unwrap();
        store.clear("s1").unwrap();
        assert!(store.load("s1").unwrap().is_empty());
        assert_eq!(store.usage_total("s1").unwrap(), (0, 0, 0));
        assert!(store.stats("s1").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
