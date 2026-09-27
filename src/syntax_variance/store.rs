//! Persists syntax variances to `syntax_variances` table in `ContextForensicsStore`.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use crate::context_forensics::store::ContextForensicsStore;
use super::inspector::SyntaxVariance;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxVarianceRecord {
    pub variance_type: String,
    pub key_name: String,
    pub sample_json: String,
    pub count: u64,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

pub fn record_variances(store: &ContextForensicsStore, variances: &[SyntaxVariance]) -> Result<()> {
    if variances.is_empty() {
        return Ok(());
    }

    let conn = store.lock()?;
    let now = chrono::Utc::now().to_rfc3339();

    for v in variances {
        conn.execute(
            "INSERT INTO syntax_variances (variance_type, key_name, sample_json, count, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?4)
             ON CONFLICT (variance_type, key_name) DO UPDATE SET
                count = count + 1,
                sample_json = excluded.sample_json,
                last_seen_at = excluded.last_seen_at",
            params![v.variance_type, v.key_name, v.sample_json, now],
        )?;
    }

    Ok(())
}

pub fn get_variances(store: &ContextForensicsStore) -> Result<Vec<SyntaxVarianceRecord>> {
    let conn = store.lock()?;

    let mut stmt = conn.prepare(
        "SELECT variance_type, key_name, sample_json, count, first_seen_at, last_seen_at
         FROM syntax_variances
         ORDER BY count DESC, last_seen_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        let count_i64: i64 = row.get(3)?;
        let count = u64::try_from(count_i64).unwrap_or(0);
        Ok(SyntaxVarianceRecord {
            variance_type: row.get(0)?,
            key_name: row.get(1)?,
            sample_json: row.get(2)?,
            count,
            first_seen_at: row.get(4)?,
            last_seen_at: row.get(5)?,
        })
    })?;

    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_get_variances_should_persist_and_deduplicate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context-forensics.sqlite");
        let store = ContextForensicsStore::open(&path).unwrap();

        let v1 = SyntaxVariance {
            variance_type: "header".to_string(),
            key_name: "anthropic-beta:prompt-caching".to_string(),
            sample_json: "{}".to_string(),
        };

        record_variances(&store, &[v1.clone()]).unwrap();
        record_variances(&store, &[v1.clone()]).unwrap();

        let records = get_variances(&store).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key_name, "anthropic-beta:prompt-caching");
        assert_eq!(records[0].count, 2);
    }
}
