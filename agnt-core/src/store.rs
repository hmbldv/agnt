use crate::backend::Message;
use rusqlite::{params, Connection};

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
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
        Ok(Self { conn })
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
        self.conn
            .execute(
                "INSERT INTO tool_calls (session, ts, name, args, result, duration_us)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![session, ts, name, args, result, duration_us as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load(&self, session: &str) -> Result<Vec<Message>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM messages WHERE session = ?1 ORDER BY idx")
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

    pub fn append(&self, session: &str, msg: &Message) -> Result<(), String> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(idx), -1) + 1 FROM messages WHERE session = ?1",
                params![session],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let json = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO messages (session, idx, json) VALUES (?1, ?2, ?3)",
                params![session, n, json],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn clear(&self, session: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM messages WHERE session = ?1",
                params![session],
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "DELETE FROM tool_calls WHERE session = ?1",
                params![session],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Per-tool latency stats for a session: (name, count, avg_us, max_us).
    pub fn stats(&self, session: &str) -> Result<Vec<(String, i64, i64, i64)>, String> {
        let mut stmt = self
            .conn
            .prepare(
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
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
}
