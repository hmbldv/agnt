//! Per-persona cumulative-cost tracking with daily rollover.
//!
//! State is held in memory and persisted at
//! `~/.local/state/cc-bridge/<bridge_name>.json` after every recorded
//! dispatch. On startup the bridge reads the file and silently zeros the
//! cumulative if the persisted `date` is older than today (local time).
//!
//! The shape is intentionally tiny and human-readable so an operator can
//! `cat` the file to debug.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// On-disk shape — one record per persona. The `date` is the local-time
/// date the cumulative was last reset; reads at midnight + 1 see a stale
/// date and zero the running total.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostState {
    /// Map keyed by persona name → per-persona record.
    #[serde(default)]
    pub personas: HashMap<String, PersonaCost>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersonaCost {
    pub date: String, // ISO YYYY-MM-DD in local TZ
    pub cumulative_today_usd: f64,
    pub last_call_usd: f64,
    pub last_call_at: Option<DateTime<Local>>,
    pub total_calls_today: u32,
}

/// In-memory cost tracker plus persistence path.
pub struct CostTracker {
    state: CostState,
    state_path: PathBuf,
}

impl CostTracker {
    /// Default path: `~/.local/state/cc-bridge/<bridge_name>.json`.
    pub fn default_path(bridge_name: &str) -> PathBuf {
        let base = dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("cc-bridge").join(format!("{bridge_name}.json"))
    }

    /// Load from disk if present; otherwise start with an empty map.
    /// A corrupt or unreadable file is reported via warn but doesn't fail
    /// startup — we'd rather lose yesterday's history than refuse to come
    /// up.
    pub fn load_or_default(state_path: PathBuf) -> Self {
        let state = match std::fs::read_to_string(&state_path) {
            Ok(s) => match serde_json::from_str::<CostState>(&s) {
                Ok(state) => state,
                Err(e) => {
                    warn!(
                        path = %state_path.display(),
                        error = %e,
                        "failed to parse cost state file; starting empty"
                    );
                    CostState::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CostState::default(),
            Err(e) => {
                warn!(
                    path = %state_path.display(),
                    error = %e,
                    "failed to read cost state file; starting empty"
                );
                CostState::default()
            }
        };
        Self { state, state_path }
    }

    /// Read-only view of a persona's record after applying the midnight
    /// rollover for `now`. Returns a defaulted record if the persona has
    /// never been seen.
    pub fn snapshot(&self, persona: &str, now: DateTime<Local>) -> PersonaCost {
        let today = now.date_naive().to_string();
        match self.state.personas.get(persona) {
            Some(rec) if rec.date == today => rec.clone(),
            _ => PersonaCost {
                date: today,
                ..PersonaCost::default()
            },
        }
    }

    /// Apply a dispatch's cost. Returns the post-update snapshot. Persists
    /// to disk.
    pub fn record(&mut self, persona: &str, cost_usd: f64, now: DateTime<Local>) -> PersonaCost {
        let today = now.date_naive().to_string();
        let entry = self.state.personas.entry(persona.to_string()).or_default();
        if entry.date != today {
            // Midnight rollover.
            entry.date = today;
            entry.cumulative_today_usd = 0.0;
            entry.total_calls_today = 0;
        }
        entry.cumulative_today_usd += cost_usd;
        entry.last_call_usd = cost_usd;
        entry.last_call_at = Some(now);
        entry.total_calls_today = entry.total_calls_today.saturating_add(1);
        let snap = entry.clone();
        if let Err(e) = self.persist() {
            warn!(
                path = %self.state_path.display(),
                error = %e,
                "failed to persist cost state; in-memory record retained"
            );
        }
        snap
    }

    /// Pre-dispatch quota check. Returns `Ok(())` if the persona is
    /// allowed to dispatch, or `Err(reason)` if the persisted cumulative
    /// has reached the configured limit. Note the limit is checked
    /// _before_ the call — a single dispatch may still cause the
    /// cumulative to overshoot the limit by its own cost. That's fine: the
    /// limit is a soft ceiling, not a hard budget.
    pub fn check_quota(
        &self,
        persona: &str,
        limit: Option<f64>,
        now: DateTime<Local>,
    ) -> Result<(), String> {
        let Some(limit) = limit else {
            return Ok(());
        };
        let snap = self.snapshot(persona, now);
        if snap.cumulative_today_usd >= limit {
            return Err(format!(
                "daily cost limit reached for {persona} (${:.4} >= ${:.4})",
                snap.cumulative_today_usd, limit
            ));
        }
        Ok(())
    }

    fn persist(&self) -> std::io::Result<()> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a sibling tempfile then rename — keeps the on-disk
        // record consistent under crash / concurrent reader.
        let tmp = with_extension(&self.state_path, "json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.state).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.state_path)?;
        debug!(path = %self.state_path.display(), "cost state persisted");
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn raw_state(&self) -> &CostState {
        &self.state
    }

    #[cfg(test)]
    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }
}

fn with_extension(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.to_path_buf().into_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Local-tz date for "today". Centralised so tests can monkey-patch.
pub fn now_local() -> DateTime<Local> {
    Local::now()
}

/// Helper for tests: build a `DateTime<Local>` from an ISO date.
#[cfg(test)]
pub(crate) fn local_date(y: i32, m: u32, d: u32) -> DateTime<Local> {
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
    let nd = NaiveDate::from_ymd_opt(y, m, d).unwrap();
    let dt = NaiveDateTime::new(nd, NaiveTime::from_hms_opt(12, 0, 0).unwrap());
    Local.from_local_datetime(&dt).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tracker_in(tmp: &TempDir) -> CostTracker {
        let path = tmp.path().join("codex.json");
        CostTracker::load_or_default(path)
    }

    #[test]
    fn fresh_tracker_has_no_personas() {
        let tmp = TempDir::new().unwrap();
        let t = tracker_in(&tmp);
        assert!(t.raw_state().personas.is_empty());
    }

    #[test]
    fn record_accumulates_within_day() {
        let tmp = TempDir::new().unwrap();
        let mut t = tracker_in(&tmp);
        let day = local_date(2026, 5, 3);
        t.record("archon", 0.27, day);
        t.record("archon", 0.43, day);
        let snap = t.snapshot("archon", day);
        assert!((snap.cumulative_today_usd - 0.70).abs() < 1e-9);
        assert_eq!(snap.last_call_usd, 0.43);
        assert_eq!(snap.total_calls_today, 2);
    }

    #[test]
    fn midnight_rollover_zeroes_cumulative() {
        let tmp = TempDir::new().unwrap();
        let mut t = tracker_in(&tmp);
        let day1 = local_date(2026, 5, 3);
        let day2 = local_date(2026, 5, 4);
        t.record("archon", 1.50, day1);
        let snap_day1 = t.snapshot("archon", day1);
        assert_eq!(snap_day1.cumulative_today_usd, 1.50);
        // Snapshot taken on a fresh day: cumulative should be zero.
        let snap_day2 = t.snapshot("archon", day2);
        assert_eq!(snap_day2.cumulative_today_usd, 0.0);
        assert_eq!(snap_day2.total_calls_today, 0);
        // First record on day2 starts fresh.
        let after = t.record("archon", 0.10, day2);
        assert_eq!(after.cumulative_today_usd, 0.10);
        assert_eq!(after.total_calls_today, 1);
    }

    #[test]
    fn quota_check_passes_under_limit() {
        let tmp = TempDir::new().unwrap();
        let mut t = tracker_in(&tmp);
        let day = local_date(2026, 5, 3);
        t.record("archon", 0.40, day);
        assert!(t.check_quota("archon", Some(1.00), day).is_ok());
    }

    #[test]
    fn quota_check_rejects_at_limit() {
        let tmp = TempDir::new().unwrap();
        let mut t = tracker_in(&tmp);
        let day = local_date(2026, 5, 3);
        t.record("archon", 0.011, day);
        let err = t.check_quota("archon", Some(0.01), day).unwrap_err();
        assert!(err.contains("daily cost limit reached"), "{err}");
    }

    #[test]
    fn quota_none_means_unlimited() {
        let tmp = TempDir::new().unwrap();
        let mut t = tracker_in(&tmp);
        let day = local_date(2026, 5, 3);
        t.record("archon", 9_999.99, day);
        assert!(t.check_quota("archon", None, day).is_ok());
    }

    #[test]
    fn persist_and_reload_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("codex.json");
        {
            let mut t = CostTracker::load_or_default(path.clone());
            let day = local_date(2026, 5, 3);
            t.record("archon", 0.27, day);
            t.record("scalpel", 0.05, day);
        }
        // Reload from the same path.
        let t2 = CostTracker::load_or_default(path);
        assert_eq!(t2.raw_state().personas.len(), 2);
        let archon = &t2.raw_state().personas["archon"];
        assert!((archon.cumulative_today_usd - 0.27).abs() < 1e-9);
    }

    #[test]
    fn corrupt_file_does_not_panic_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("codex.json");
        std::fs::write(&path, b"this is not json").unwrap();
        let t = CostTracker::load_or_default(path.clone());
        assert!(t.raw_state().personas.is_empty());
        assert_eq!(t.state_path(), path);
    }
}
