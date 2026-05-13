//! SQLite persistence for generated tools, their WASM artifacts, test cases,
//! and runtime telemetry.

use crate::script_tool::{SandboxConfig, ScriptTool};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRecord {
    pub name: String,
    pub version: i64,
    pub description: String,
    pub schema: Value,
    pub source: String,
    pub wasm: Vec<u8>,
    pub sandbox_config: SandboxConfig,
    pub source_sha256: String,
    pub wasm_sha256: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    pub version: i64,
    pub description: String,
    pub wasm_bytes: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolStats {
    pub calls: i64,
    pub failures: i64,
    pub avg_duration_us: f64,
    pub last_called_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    pub input: Value,
    pub expected: Option<Value>,
    pub passed: Option<bool>,
    pub created_at: i64,
}

pub struct ToolStore {
    conn: Mutex<Connection>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ToolStore {
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open db: {e}"))?;
        conn.pragma_update(None, "journal_mode", &"WAL")
            .map_err(|e| format!("pragma: {e}"))?;
        conn.pragma_update(None, "synchronous", &"NORMAL")
            .map_err(|e| format!("pragma: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tools (
                name           TEXT NOT NULL,
                version        INTEGER NOT NULL,
                description    TEXT NOT NULL,
                schema_json    TEXT NOT NULL,
                source         TEXT NOT NULL,
                wasm           BLOB NOT NULL,
                sandbox_json   TEXT NOT NULL,
                source_sha256  TEXT NOT NULL,
                wasm_sha256    TEXT NOT NULL,
                created_at     INTEGER NOT NULL,
                PRIMARY KEY (name, version)
            );
            CREATE INDEX IF NOT EXISTS idx_tools_name ON tools(name);

            CREATE TABLE IF NOT EXISTS tool_calls (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT NOT NULL,
                version     INTEGER NOT NULL,
                ts          INTEGER NOT NULL,
                args        TEXT NOT NULL,
                result      TEXT NOT NULL,
                ok          INTEGER NOT NULL,
                duration_us INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tool_calls_name ON tool_calls(name);

            CREATE TABLE IF NOT EXISTS test_cases (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL,
                version    INTEGER NOT NULL,
                input      TEXT NOT NULL,
                expected   TEXT,
                passed     INTEGER,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_test_cases_name ON test_cases(name);

            CREATE TABLE IF NOT EXISTS tool_features (
                name    TEXT NOT NULL,
                feature TEXT NOT NULL,
                value   TEXT NOT NULL,
                PRIMARY KEY (name, feature)
            );",
        )
        .map_err(|e| format!("create tables: {e}"))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.conn.lock().map_err(|e| format!("mutex: {e}"))
    }

    pub fn save_tool(&self, tool: &ScriptTool) -> Result<i64, String> {
        let conn = self.lock()?;
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM tools WHERE name = ?1",
                params![tool.name],
                |r| r.get(0),
            )
            .map_err(|e| format!("next version: {e}"))?;

        let schema_json = serde_json::to_string(&tool.schema)
            .map_err(|e| format!("encode schema: {e}"))?;
        let sandbox_json = serde_json::to_string(&tool.sandbox_config)
            .map_err(|e| format!("encode sandbox: {e}"))?;
        let source_sha = sha256_hex(tool.source.as_bytes());
        let wasm_sha = sha256_hex(&tool.wasm);
        let now = now_secs();

        conn.execute(
            "INSERT INTO tools (name, version, description, schema_json, source, wasm, sandbox_json, source_sha256, wasm_sha256, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                tool.name,
                next,
                tool.description,
                schema_json,
                tool.source,
                tool.wasm,
                sandbox_json,
                source_sha,
                wasm_sha,
                now,
            ],
        )
        .map_err(|e| format!("insert tool: {e}"))?;

        Ok(next)
    }

    pub fn load_tool(&self, name: &str) -> Result<Option<ToolRecord>, String> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT name, version, description, schema_json, source, wasm, sandbox_json, source_sha256, wasm_sha256, created_at
                 FROM tools WHERE name = ?1 ORDER BY version DESC LIMIT 1",
                params![name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, i64>(9)?,
                    ))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(format!("load tool: {other}")),
            })?;

        let Some((name, version, description, schema_json, source, wasm, sandbox_json, source_sha256, wasm_sha256, created_at)) = row else {
            return Ok(None);
        };
        let schema: Value = serde_json::from_str(&schema_json)
            .map_err(|e| format!("decode schema: {e}"))?;
        let sandbox_config: SandboxConfig = serde_json::from_str(&sandbox_json)
            .map_err(|e| format!("decode sandbox: {e}"))?;
        Ok(Some(ToolRecord {
            name,
            version,
            description,
            schema,
            source,
            wasm,
            sandbox_config,
            source_sha256,
            wasm_sha256,
            created_at,
        }))
    }

    pub fn record_to_tool(rec: ToolRecord) -> ScriptTool {
        ScriptTool {
            name: rec.name,
            description: rec.description,
            schema: rec.schema,
            source: rec.source,
            wasm: rec.wasm,
            sandbox_config: rec.sandbox_config,
        }
    }

    pub fn list(&self) -> Result<Vec<ToolSummary>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT t.name, t.version, t.description, LENGTH(t.wasm), t.created_at
                 FROM tools t
                 JOIN (SELECT name, MAX(version) AS v FROM tools GROUP BY name) m
                   ON t.name = m.name AND t.version = m.v
                 ORDER BY t.name",
            )
            .map_err(|e| format!("prepare: {e}"))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ToolSummary {
                    name: r.get(0)?,
                    version: r.get(1)?,
                    description: r.get(2)?,
                    wasm_bytes: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })
            .map_err(|e| format!("query: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("row: {e}"))?);
        }
        Ok(out)
    }

    pub fn search(&self, query: &str) -> Result<Vec<ToolSummary>, String> {
        let conn = self.lock()?;
        let pat = format!("%{}%", query);
        let mut stmt = conn
            .prepare(
                "SELECT t.name, t.version, t.description, LENGTH(t.wasm), t.created_at
                 FROM tools t
                 JOIN (SELECT name, MAX(version) AS v FROM tools GROUP BY name) m
                   ON t.name = m.name AND t.version = m.v
                 WHERE t.name LIKE ?1 OR t.description LIKE ?1
                 ORDER BY t.name",
            )
            .map_err(|e| format!("prepare: {e}"))?;
        let rows = stmt
            .query_map(params![pat], |r| {
                Ok(ToolSummary {
                    name: r.get(0)?,
                    version: r.get(1)?,
                    description: r.get(2)?,
                    wasm_bytes: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })
            .map_err(|e| format!("query: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("row: {e}"))?);
        }
        Ok(out)
    }

    pub fn record_call(
        &self,
        name: &str,
        version: i64,
        args: &Value,
        result: &str,
        ok: bool,
        duration_us: u64,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        let args_s = serde_json::to_string(args).unwrap_or_else(|_| "null".into());
        conn.execute(
            "INSERT INTO tool_calls (name, version, ts, args, result, ok, duration_us)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                version,
                now_secs(),
                args_s,
                result,
                ok as i64,
                duration_us as i64,
            ],
        )
        .map_err(|e| format!("insert call: {e}"))?;
        Ok(())
    }

    pub fn save_test_case(
        &self,
        name: &str,
        version: i64,
        input: &Value,
        expected: Option<&Value>,
        passed: Option<bool>,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        let input_s = serde_json::to_string(input).unwrap_or_else(|_| "null".into());
        let expected_s = expected.map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into()));
        conn.execute(
            "INSERT INTO test_cases (name, version, input, expected, passed, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                name,
                version,
                input_s,
                expected_s,
                passed.map(|b| b as i64),
                now_secs(),
            ],
        )
        .map_err(|e| format!("insert test case: {e}"))?;
        Ok(())
    }

    pub fn stats(&self, name: Option<&str>) -> Result<ToolStats, String> {
        let conn = self.lock()?;
        let (calls, failures, avg, last): (i64, i64, Option<f64>, Option<i64>) = match name {
            Some(n) => conn
                .query_row(
                    "SELECT
                        COUNT(*),
                        SUM(CASE WHEN ok = 0 THEN 1 ELSE 0 END),
                        AVG(duration_us),
                        MAX(ts)
                     FROM tool_calls WHERE name = ?1",
                    params![n],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                            r.get::<_, Option<f64>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .map_err(|e| format!("stats: {e}"))?,
            None => conn
                .query_row(
                    "SELECT
                        COUNT(*),
                        SUM(CASE WHEN ok = 0 THEN 1 ELSE 0 END),
                        AVG(duration_us),
                        MAX(ts)
                     FROM tool_calls",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                            r.get::<_, Option<f64>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .map_err(|e| format!("stats: {e}"))?,
        };
        Ok(ToolStats {
            calls,
            failures,
            avg_duration_us: avg.unwrap_or(0.0),
            last_called_at: last,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> String {
        let p = std::env::temp_dir().join(format!("toolgnrtr-test-{}.db", uuid::Uuid::new_v4()));
        p.to_str().unwrap().to_string()
    }

    fn dummy_tool(name: &str) -> ScriptTool {
        ScriptTool::new(
            name.into(),
            "test tool".into(),
            serde_json::json!({"type":"object"}),
            "fn main() {}".into(),
            b"\0asm\x01\x00\x00\x00".to_vec(),
        )
    }

    #[test]
    fn save_then_load_roundtrips_wasm_blob() {
        let path = tmp_db();
        let store = ToolStore::open(&path).unwrap();
        let tool = dummy_tool("adder");
        let v = store.save_tool(&tool).unwrap();
        assert_eq!(v, 1);
        let rec = store.load_tool("adder").unwrap().expect("present");
        assert_eq!(rec.name, "adder");
        assert_eq!(rec.version, 1);
        assert_eq!(rec.wasm, tool.wasm);
        assert_eq!(rec.source, tool.source);
        assert_eq!(rec.wasm_sha256, sha256_hex(&tool.wasm));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn versions_increment_per_name() {
        let path = tmp_db();
        let store = ToolStore::open(&path).unwrap();
        let t = dummy_tool("x");
        assert_eq!(store.save_tool(&t).unwrap(), 1);
        assert_eq!(store.save_tool(&t).unwrap(), 2);
        assert_eq!(store.save_tool(&t).unwrap(), 3);
        let rec = store.load_tool("x").unwrap().unwrap();
        assert_eq!(rec.version, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn search_matches_name_and_description() {
        let path = tmp_db();
        let store = ToolStore::open(&path).unwrap();
        let mut a = dummy_tool("json_pretty");
        a.description = "format json".into();
        store.save_tool(&a).unwrap();
        let mut b = dummy_tool("fetch_url");
        b.description = "download something".into();
        store.save_tool(&b).unwrap();
        let hits = store.search("json").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "json_pretty");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_call_and_stats() {
        let path = tmp_db();
        let store = ToolStore::open(&path).unwrap();
        store.save_tool(&dummy_tool("t")).unwrap();
        store
            .record_call("t", 1, &serde_json::json!({}), "ok", true, 1000)
            .unwrap();
        store
            .record_call("t", 1, &serde_json::json!({}), "err", false, 2000)
            .unwrap();
        let s = store.stats(Some("t")).unwrap();
        assert_eq!(s.calls, 2);
        assert_eq!(s.failures, 1);
        assert_eq!(s.avg_duration_us, 1500.0);
        let _ = std::fs::remove_file(&path);
    }
}
