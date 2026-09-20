use crate::model::{InputRecord, Phase, PickSource, PickStatus};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;
use sha2::{Digest, Sha256};

pub struct Store {
    pub connection: Connection,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredPick {
    pub pick_id: String,
    pub station_id: String,
    pub phase: Phase,
    pub time: String,
    pub sigma_seconds: f64,
    pub confidence: f64,
    pub source: PickSource,
    pub model_id: String,
    pub status: PickStatus,
    pub locked_event: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredStation {
    pub station_id: String,
    pub x_km: f64,
    pub y_km: f64,
    pub elevation_km: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredModel {
    pub model_id: String,
    pub vp_kms: f64,
    pub vs_kms: f64,
    pub x_min_km: f64,
    pub x_max_km: f64,
    pub y_min_km: f64,
    pub y_max_km: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredSegment {
    pub version: String,
    pub station_id: String,
    pub start_time: String,
    pub end_time: Option<String>,
    pub bias_seconds: f64,
    pub rate_s_per_s: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct ImportOutcome {
    pub batch_id: String,
    pub content_hash: String,
    pub duplicated: bool,
    pub records: usize,
}

impl Store {
    pub fn open(path: &str) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;",
            )
            .map_err(|error| error.to_string())?;
        let store = Store { connection };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), String> {
        self.connection
            .execute_batch(include_str!("../sql/schema.sql"))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn log_action(&self, action: &str, detail: &serde_json::Value) -> Result<(), String> {
        let id = stable_id(action, &detail.to_string());
        self.connection
            .execute(
                "INSERT OR IGNORE INTO operation_log(log_id, action, detail_json) VALUES (?1, ?2, ?3)",
                params![id, action, detail.to_string()],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn import_jsonl(&self, source_name: &str, content: &str) -> Result<ImportOutcome, String> {
        let canonical = normalize_lines(content);
        let hash = hash_text(&canonical);
        if let Some((batch_id, records)) = self
            .connection
            .query_row(
                "SELECT batch_id, record_count FROM import_batches WHERE content_hash = ?1",
                params![hash],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?
        {
            return Ok(ImportOutcome {
                batch_id,
                content_hash: hash,
                duplicated: true,
                records: records as usize,
            });
        }
        let records = parse_records(&canonical)?;
        let batch_id = stable_id("import", &format!("{source_name}:{hash}"));
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO import_batches(batch_id, source_name, content_hash, record_count, status)
             VALUES (?1, ?2, ?3, ?4, 'committed')",
            params![batch_id, source_name, hash, records.len() as i64],
        )
        .map_err(|e| e.to_string())?;
        for (line, record) in records.iter().enumerate() {
            insert_record(&tx, &batch_id, line as i64, record)?;
        }
        tx.execute(
            "INSERT INTO operation_log(log_id, action, detail_json) VALUES (?1, 'import_batch', ?2)",
            params![stable_id(&format!("log:{batch_id}"), &hash), serde_json::json!({"batch_id":batch_id,"hash":hash,"duplicated":false}).to_string()],
        ).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(ImportOutcome {
            batch_id,
            content_hash: hash,
            duplicated: false,
            records: records.len(),
        })
    }

    pub fn stations(&self) -> Result<Vec<StoredStation>, String> {
        let mut stmt = self
            .connection
            .prepare(
                "SELECT station_id, x_km, y_km, elevation_km FROM stations ORDER BY station_id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredStation {
                    station_id: row.get(0)?,
                    x_km: row.get(1)?,
                    y_km: row.get(2)?,
                    elevation_km: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn model(&self) -> Result<StoredModel, String> {
        self.connection
            .query_row(
                "SELECT model_id, vp_kms, vs_kms, x_min_km, x_max_km, y_min_km, y_max_km
             FROM velocity_models ORDER BY model_id LIMIT 1",
                [],
                |row| {
                    Ok(StoredModel {
                        model_id: row.get(0)?,
                        vp_kms: row.get(1)?,
                        vs_kms: row.get(2)?,
                        x_min_km: row.get(3)?,
                        x_max_km: row.get(4)?,
                        y_min_km: row.get(5)?,
                        y_max_km: row.get(6)?,
                    })
                },
            )
            .map_err(|e| e.to_string())
    }

    pub fn segments(&self, version: &str) -> Result<Vec<StoredSegment>, String> {
        let mut stmt = self
            .connection
            .prepare(
                "SELECT version, station_id, start_time, end_time, bias_seconds, rate_s_per_s
             FROM clock_segments WHERE version = ?1
             ORDER BY station_id, start_time, end_time IS NOT NULL",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![version], |row| {
                Ok(StoredSegment {
                    version: row.get(0)?,
                    station_id: row.get(1)?,
                    start_time: row.get(2)?,
                    end_time: row.get(3)?,
                    bias_seconds: row.get(4)?,
                    rate_s_per_s: row.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn picks(&self) -> Result<Vec<StoredPick>, String> {
        let mut stmt = self.connection.prepare(
            "SELECT pick_id, station_id, phase, time, sigma_seconds, confidence, source, model_id, status, locked_event
             FROM picks ORDER BY pick_id"
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], row_to_pick).map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

fn row_to_pick(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPick> {
    let phase: String = row.get(2)?;
    let source: String = row.get(6)?;
    let status: String = row.get(8)?;
    Ok(StoredPick {
        pick_id: row.get(0)?,
        station_id: row.get(1)?,
        phase: if phase == "S" { Phase::S } else { Phase::P },
        time: row.get(3)?,
        sigma_seconds: row.get(4)?,
        confidence: row.get(5)?,
        source: if source == "manual" {
            PickSource::Manual
        } else {
            PickSource::Auto
        },
        model_id: row.get(7)?,
        status: match status.as_str() {
            "locked" => PickStatus::Locked,
            "noise" => PickStatus::Noise,
            _ => PickStatus::Active,
        },
        locked_event: row.get(9)?,
    })
}

pub fn parse_records(content: &str) -> Result<Vec<InputRecord>, String> {
    let mut records = Vec::new();
    for (line_number, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<InputRecord>(line)
            .map_err(|error| format!("invalid JSON on line {}: {error}", line_number + 1))?;
        validate_record(&record)?;
        records.push(record);
    }
    Ok(records)
}

fn validate_record(record: &InputRecord) -> Result<(), String> {
    match record {
        InputRecord::Pick(pick) => {
            crate::model::parse_epoch(&pick.time)?;
            if !(0.0..=1.0).contains(&pick.confidence) || pick.sigma_seconds <= 0.0 {
                return Err(format!(
                    "invalid uncertainty/confidence for {}",
                    pick.pick_id
                ));
            }
        }
        InputRecord::ClockCorrection(segment) => {
            crate::model::parse_epoch(&segment.start_time)?;
            if let Some(end) = &segment.end_time {
                let start = crate::model::parse_epoch(&segment.start_time)?;
                let end = crate::model::parse_epoch(end)?;
                if end <= start {
                    return Err("clock segment end must be after start".to_string());
                }
            }
        }
        InputRecord::Station(station) => {
            if !station.station_id.chars().all(valid_id_char) {
                return Err("station id contains unsupported characters".to_string());
            }
        }
        InputRecord::VelocityModel(model) => {
            if model.vs_kms <= 0.0 || model.vp_kms <= model.vs_kms {
                return Err("require 0 < Vs < Vp".to_string());
            }
            if model.x_min_km >= model.x_max_km || model.y_min_km >= model.y_max_km {
                return Err("velocity model bounds are inverted".to_string());
            }
        }
    }
    Ok(())
}

fn valid_id_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
}

fn insert_record(
    tx: &rusqlite::Transaction<'_>,
    batch_id: &str,
    line: i64,
    record: &InputRecord,
) -> Result<(), String> {
    let canonical = serde_json::to_string(record).map_err(|e| e.to_string())?;
    let record_type = match record {
        InputRecord::Station(_) => "station",
        InputRecord::VelocityModel(_) => "velocity_model",
        InputRecord::ClockCorrection(_) => "clock_correction",
        InputRecord::Pick(_) => "pick",
    };
    tx.execute(
        "INSERT INTO import_records(batch_id, line_number, record_type, canonical_json)
         VALUES (?1, ?2, ?3, ?4)",
        params![batch_id, line, record_type, canonical],
    )
    .map_err(|e| e.to_string())?;
    match record {
        InputRecord::Station(station) => {
            tx.execute(
                "INSERT INTO stations(station_id, x_km, y_km, elevation_km)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    station.station_id,
                    station.x_km,
                    station.y_km,
                    station.elevation_km
                ],
            )
            .map_err(|e| e.to_string())?;
            if tx.changes() == 0 {
                let same: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM stations
                         WHERE station_id=?1 AND x_km=?2 AND y_km=?3 AND elevation_km=?4",
                        params![
                            station.station_id,
                            station.x_km,
                            station.y_km,
                            station.elevation_km
                        ],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                if same == 0 {
                    return Err(format!(
                        "conflicting immutable station {}",
                        station.station_id
                    ));
                }
            }
        }
        InputRecord::VelocityModel(model) => {
            tx.execute(
                "INSERT INTO velocity_models(model_id, vp_kms, vs_kms, x_min_km, x_max_km, y_min_km, y_max_km)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![model.model_id, model.vp_kms, model.vs_kms, model.x_min_km, model.x_max_km, model.y_min_km, model.y_max_km],
            ).map_err(|e| e.to_string())?;
            if tx.changes() == 0 {
                let same: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM velocity_models
                         WHERE model_id=?1 AND vp_kms=?2 AND vs_kms=?3
                           AND x_min_km=?4 AND x_max_km=?5 AND y_min_km=?6 AND y_max_km=?7",
                        params![
                            model.model_id,
                            model.vp_kms,
                            model.vs_kms,
                            model.x_min_km,
                            model.x_max_km,
                            model.y_min_km,
                            model.y_max_km
                        ],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                if same == 0 {
                    return Err(format!(
                        "conflicting immutable velocity model {}",
                        model.model_id
                    ));
                }
            }
        }
        InputRecord::ClockCorrection(segment) => {
            tx.execute(
                "INSERT OR IGNORE INTO clock_segments(version, station_id, start_time, end_time, bias_seconds, rate_s_per_s)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![segment.version, segment.station_id, segment.start_time, segment.end_time, segment.bias_seconds, segment.rate_s_per_s],
            ).map_err(|e| e.to_string())?;
            if tx.changes() == 0 {
                let existing: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM clock_segments
                     WHERE version=?1 AND station_id=?2 AND start_time=?3
                       AND IFNULL(end_time,'')=IFNULL(?4,'')
                       AND bias_seconds=?5 AND rate_s_per_s=?6",
                        params![
                            segment.version,
                            segment.station_id,
                            segment.start_time,
                            segment.end_time,
                            segment.bias_seconds,
                            segment.rate_s_per_s
                        ],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                if existing == 0 {
                    return Err(format!(
                        "conflicting immutable clock segment for {} version {}",
                        segment.station_id, segment.version
                    ));
                }
            }
        }
        InputRecord::Pick(pick) => {
            tx.execute(
                "INSERT INTO picks(pick_id, station_id, phase, time, sigma_seconds, confidence, source, model_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![pick.pick_id, pick.station_id, phase_text(pick.phase), pick.time, pick.sigma_seconds, pick.confidence, source_text(pick.source), pick.model_id],
            ).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn phase_text(phase: Phase) -> &'static str {
    match phase {
        Phase::P => "P",
        Phase::S => "S",
    }
}

fn source_text(source: PickSource) -> &'static str {
    match source {
        PickSource::Auto => "auto",
        PickSource::Manual => "manual",
    }
}

pub fn normalize_lines(content: &str) -> String {
    let mut canonical = String::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => {
                canonical.push_str(&value.to_string());
                canonical.push('\n');
            }
            Err(_) => {
                canonical.push_str(line.trim_end());
                canonical.push('\n');
            }
        }
    }
    canonical
}

pub fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn stable_id(kind: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b":");
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl Store {
    pub fn set_pick_status(
        &self,
        pick_id: &str,
        status: &str,
        locked_event: Option<&str>,
        actor: &str,
    ) -> Result<(), String> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        let changed = tx
            .execute(
                "UPDATE picks SET status = ?2, locked_event = ?3 WHERE pick_id = ?1",
                params![pick_id, status, locked_event],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err(format!("unknown pick {pick_id}"));
        }
        let detail = serde_json::json!({"pick_id":pick_id,"status":status,"locked_event":locked_event,"actor":actor});
        tx.execute(
            "INSERT OR IGNORE INTO operation_log(log_id, action, detail_json) VALUES (?1, ?2, ?3)",
            params![
                stable_id("pick-status", &detail.to_string()),
                "pick_status",
                detail.to_string()
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn save_solution(
        &self,
        clock_version: &str,
        scope: &[String],
        rationale: &str,
        candidates: &[serde_json::Value],
    ) -> Result<String, String> {
        let scope_json = serde_json::to_string(scope).map_err(|e| e.to_string())?;
        let identity = serde_json::json!({
            "clock_version": clock_version,
            "scope": scope_json,
            "rationale": rationale,
            "candidates": candidates
        })
        .to_string();
        let run_id = stable_id("run", &identity);
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR IGNORE INTO solver_runs(run_id, clock_version, scope_json, rationale, created_by)
             VALUES (?1, ?2, ?3, ?4, 'local-user')",
            params![run_id, clock_version, scope_json, rationale],
        ).map_err(|e| e.to_string())?;
        for candidate in candidates {
            let candidate_id = candidate["candidate_id"]
                .as_str()
                .ok_or("candidate missing candidate_id")?
                .to_string();
            let event_key = candidate["event_key"]
                .as_str()
                .ok_or("candidate missing event_key")?;
            let rank = candidate["rank"].as_i64().ok_or("candidate missing rank")?;
            tx.execute(
                "INSERT OR IGNORE INTO candidate_snapshots(candidate_id, run_id, event_key, rank, score, status, solution_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    candidate_id,
                    run_id,
                    event_key,
                    rank,
                    candidate["score"].as_f64().unwrap_or(f64::INFINITY),
                    candidate["status"].as_str().unwrap_or("underdetermined"),
                    candidate.to_string()
                ],
            ).map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR IGNORE INTO events(event_key, created_by_run_id) VALUES (?1, ?2)
                 ON CONFLICT(event_key) DO NOTHING",
                params![event_key, run_id],
            )
            .map_err(|e| e.to_string())?;
            for pick in candidate["accepted_picks"].as_array().into_iter().flatten() {
                insert_candidate_pick(&tx, &candidate_id, pick, true)?;
            }
            for pick in candidate["rejected_picks"].as_array().into_iter().flatten() {
                insert_candidate_pick(&tx, &candidate_id, pick, false)?;
            }
        }
        let detail =
            serde_json::json!({"run_id":run_id,"clock_version":clock_version,"scope":scope});
        tx.execute(
            "INSERT OR IGNORE INTO operation_log(log_id, action, detail_json) VALUES (?1, 'solver_run', ?2)",
            params![stable_id("log-run", &run_id), detail.to_string()],
        ).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(run_id)
    }

    pub fn freeze_candidate(
        &self,
        candidate_id: &str,
        label: &str,
        actor: &str,
    ) -> Result<String, String> {
        let event_key: String = self
            .connection
            .query_row(
                "SELECT event_key FROM candidate_snapshots WHERE candidate_id = ?1",
                params![candidate_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let hypothesis_id = stable_id("hypothesis", &format!("{candidate_id}:{label}"));
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO event_hypotheses(hypothesis_id, event_key, parent_candidate_id, frozen_candidate_id, label, active_clock_version)
             VALUES (?1, ?2, ?3, ?3, ?4,
                     (SELECT clock_version FROM solver_runs WHERE run_id =
                       (SELECT run_id FROM candidate_snapshots WHERE candidate_id = ?3)))
             ON CONFLICT(hypothesis_id) DO NOTHING",
            params![hypothesis_id, event_key, candidate_id, label],
        ).map_err(|e| e.to_string())?;
        let detail = serde_json::json!({"hypothesis_id":hypothesis_id,"candidate_id":candidate_id,"event_key":event_key,"label":label,"actor":actor});
        tx.execute(
            "INSERT OR IGNORE INTO operation_log(log_id, action, detail_json) VALUES (?1, 'freeze_candidate', ?2)",
            params![stable_id("freeze", &hypothesis_id), detail.to_string()],
        ).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(hypothesis_id)
    }

    pub fn duplicate_hypothesis(
        &self,
        hypothesis_id: &str,
        label: &str,
        actor: &str,
    ) -> Result<String, String> {
        let (event_key, candidate_id, clock_version): (String, String, String) = self
            .connection
            .query_row(
                "SELECT h.event_key, h.frozen_candidate_id, h.active_clock_version
             FROM event_hypotheses h WHERE h.hypothesis_id = ?1",
                params![hypothesis_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        let new_id = stable_id("hypothesis-duplicate", &format!("{hypothesis_id}:{label}"));
        self.connection.execute(
            "INSERT INTO event_hypotheses(hypothesis_id, event_key, parent_candidate_id, frozen_candidate_id, label, active_clock_version)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            params![new_id, event_key, candidate_id, label, clock_version],
        ).map_err(|e| e.to_string())?;
        self.log_action("duplicate_hypothesis", &serde_json::json!({"new_hypothesis_id":new_id,"parent_hypothesis_id":hypothesis_id,"actor":actor}))?;
        Ok(new_id)
    }

    pub fn recompute_hypothesis_context(
        &self,
        hypothesis_id: &str,
    ) -> Result<(String, Vec<String>, String), String> {
        let (event_key, parent_candidate_id, clock_version): (String, String, String) = self
            .connection
            .query_row(
                "SELECT event_key, parent_candidate_id, active_clock_version
                 FROM event_hypotheses WHERE hypothesis_id = ?1",
                params![hypothesis_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        let picks = self.locked_pick_ids(&parent_candidate_id)?;
        Ok((event_key, picks, clock_version))
    }

    pub fn attach_working_candidate(
        &self,
        hypothesis_id: &str,
        candidate_id: &str,
        clock_version: &str,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE event_hypotheses
                 SET parent_candidate_id = ?2, active_clock_version = ?3
                 WHERE hypothesis_id = ?1",
                params![hypothesis_id, candidate_id, clock_version],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn locked_pick_ids(&self, candidate_id: &str) -> Result<Vec<String>, String> {
        let mut stmt = self
            .connection
            .prepare(
                "SELECT pick_id FROM candidate_pick_snapshots
                 WHERE candidate_id = ?1 AND accepted = 1 ORDER BY pick_id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![candidate_id], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn latest_solutions(&self) -> Result<Vec<serde_json::Value>, String> {
        let mut stmt = self
            .connection
            .prepare(
                "SELECT cs.solution_json
                 FROM candidate_snapshots cs
                 JOIN solver_runs sr ON sr.run_id = cs.run_id
                 WHERE sr.rowid = (
                     SELECT MAX(sr2.rowid)
                     FROM candidate_snapshots cs2
                     JOIN solver_runs sr2 ON sr2.run_id = cs2.run_id
                     WHERE cs2.event_key = cs.event_key
                 )
                 ORDER BY cs.event_key, cs.rank",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let json: String = row.get(0)?;
                serde_json::from_str(&json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    )
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn run_solutions(&self, run_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let mut stmt = self
            .connection
            .prepare("SELECT solution_json FROM candidate_snapshots WHERE run_id = ?1 ORDER BY event_key, rank")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![run_id], |row| {
                let json: String = row.get(0)?;
                serde_json::from_str(&json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    )
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn runs(&self) -> Result<Vec<serde_json::Value>, String> {
        let mut stmt = self
            .connection
            .prepare("SELECT run_id, clock_version, scope_json, rationale, created_by FROM solver_runs ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let scope_json: String = row.get(2)?;
                Ok(json!({
                    "run_id": row.get::<_, String>(0)?,
                    "clock_version": row.get::<_, String>(1)?,
                    "scope": serde_json::from_str::<serde_json::Value>(&scope_json).unwrap_or(json!([])),
                    "rationale": row.get::<_, String>(3)?,
                    "created_by": row.get::<_, String>(4)?
                }))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn hypotheses(&self) -> Result<Vec<serde_json::Value>, String> {
        let mut stmt = self
            .connection
            .prepare("SELECT hypothesis_id, event_key, parent_candidate_id, frozen_candidate_id, label, active_clock_version FROM event_hypotheses ORDER BY event_key, rowid")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(json!({
                    "hypothesis_id": row.get::<_, String>(0)?,
                    "event_key": row.get::<_, String>(1)?,
                    "parent_candidate_id": row.get::<_, Option<String>>(2)?,
                    "frozen_candidate_id": row.get::<_, Option<String>>(3)?,
                    "label": row.get::<_, String>(4)?,
                    "active_clock_version": row.get::<_, String>(5)?
                }))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

fn insert_candidate_pick(
    tx: &rusqlite::Transaction<'_>,
    candidate_id: &str,
    pick: &serde_json::Value,
    accepted: bool,
) -> Result<(), String> {
    tx.execute(
        "INSERT OR IGNORE INTO candidate_pick_snapshots(candidate_id, pick_id, accepted, locked, residual_seconds, contribution, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            candidate_id,
            pick["pick_id"].as_str().unwrap_or("unknown"),
            accepted as i64,
            pick["locked"].as_bool().unwrap_or(false) as i64,
            pick["residual_seconds"].as_f64(),
            pick["contribution"].as_f64(),
            pick["reason"].as_str()
        ],
    ).map_err(|e| e.to_string())?;
    Ok(())
}
