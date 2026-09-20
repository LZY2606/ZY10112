use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::model::{ClockSegment, LayerRecord, Phase, Pick, SolverInput, Station};
use crate::solver::SolverResultSet;

pub struct Store {
    connection: Mutex<Connection>,
}

#[derive(serde::Serialize)]
pub struct ImportSummary {
    pub content_hash: String,
    pub duplicate: bool,
    pub stations: usize,
    pub models: usize,
    pub picks: usize,
}

fn now_ordinal() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Store {
    pub fn open(path: &str) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(|e| e.to_string())?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|e| e.to_string())?;
        let store = Store {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }
}

impl Store {
    fn migrate(&self) -> Result<(), String> {
        let connection = self.connection.lock().map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                r#"
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, applied_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS import_batches(
  content_hash TEXT PRIMARY KEY,
  byte_length INTEGER NOT NULL,
  imported_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS stations(
  station_id TEXT PRIMARY KEY,
  latitude REAL NOT NULL,
  longitude REAL NOT NULL,
  elevation_m REAL NOT NULL,
  first_batch_hash TEXT NOT NULL REFERENCES import_batches(content_hash)
);
CREATE TABLE IF NOT EXISTS velocity_models(
  version TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  depth_top_m REAL NOT NULL,
  depth_bottom_m REAL NOT NULL,
  vp_m_s REAL NOT NULL,
  vs_m_s REAL NOT NULL,
  batch_hash TEXT NOT NULL REFERENCES import_batches(content_hash),
  PRIMARY KEY(version, ordinal)
);
CREATE TABLE IF NOT EXISTS picks(
  observation_id TEXT PRIMARY KEY,
  related_observation_id TEXT,
  station_id TEXT NOT NULL REFERENCES stations(station_id),
  phase TEXT NOT NULL CHECK(phase IN ('P','S')),
  raw_time REAL NOT NULL,
  time_uncertainty_s REAL NOT NULL,
  confidence REAL NOT NULL,
  source TEXT NOT NULL CHECK(source IN ('auto','manual')),
  velocity_model_version TEXT NOT NULL,
  batch_hash TEXT NOT NULL REFERENCES import_batches(content_hash),
  superseded INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS correction_versions(
  version TEXT PRIMARY KEY,
  parent_version TEXT,
  created_at REAL NOT NULL,
  note TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clock_segments(
  version TEXT NOT NULL REFERENCES correction_versions(version),
  station_id TEXT NOT NULL REFERENCES stations(station_id),
  ordinal INTEGER NOT NULL,
  start_time REAL NOT NULL,
  end_time REAL,
  offset_s REAL NOT NULL,
  PRIMARY KEY(version, station_id, ordinal)
);
CREATE TABLE IF NOT EXISTS hypotheses(
  hypothesis_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  correction_version TEXT NOT NULL REFERENCES correction_versions(version),
  current_set_id INTEGER,
  created_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS candidate_sets(
  set_id INTEGER PRIMARY KEY AUTOINCREMENT,
  hypothesis_id TEXT NOT NULL REFERENCES hypotheses(hypothesis_id),
  correction_version TEXT NOT NULL REFERENCES correction_versions(version),
  parent_set_id INTEGER REFERENCES candidate_sets(set_id),
  created_at REAL NOT NULL,
  input_hash TEXT NOT NULL,
  result_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS locked_picks(
  hypothesis_id TEXT NOT NULL REFERENCES hypotheses(hypothesis_id),
  observation_id TEXT NOT NULL REFERENCES picks(observation_id),
  event_key TEXT,
  locked_at REAL NOT NULL,
  PRIMARY KEY(hypothesis_id, observation_id)
);
CREATE TABLE IF NOT EXISTS noise_picks(
  observation_id TEXT PRIMARY KEY REFERENCES picks(observation_id),
  marked_at REAL NOT NULL,
  note TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS operation_log(
  log_id INTEGER PRIMARY KEY AUTOINCREMENT,
  occurred_at REAL NOT NULL,
  actor TEXT NOT NULL,
  operation TEXT NOT NULL,
  target_hypothesis TEXT,
  payload_json TEXT NOT NULL,
  content_hash TEXT NOT NULL
);
"#,
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (1, ?)",
                params![now_ordinal()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Store {
    pub fn import_jsonl(&self, bytes: &[u8]) -> Result<ImportSummary, String> {
        let content_hash = hash_bytes(bytes);
        let mut connection = self.connection.lock().map_err(|error| error.to_string())?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT content_hash FROM import_batches WHERE content_hash = ?",
                params![content_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if existing.is_some() {
            return Ok(ImportSummary {
                content_hash,
                duplicate: true,
                stations: 0,
                models: 0,
                picks: 0,
            });
        }

        let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
        let mut records = Vec::new();
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: crate::model::InputRecord = serde_json::from_str(line)
                .map_err(|error| format!("line {}: {error}", line_number + 1))?;
            records.push(record);
        }

        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO import_batches(content_hash, byte_length, imported_at) VALUES (?, ?, ?)",
            params![content_hash, bytes.len() as i64, now_ordinal()]).map_err(|e| e.to_string())?;

        let mut station_count = 0usize;
        let mut model_count = 0usize;
        let mut pick_count = 0usize;
        for (record_index, record) in records.iter().enumerate() {
            let record_label = match record {
                crate::model::InputRecord::Station(s) => format!("station {}", s.station_id),
                crate::model::InputRecord::VelocityModel(m) => {
                    format!("velocity model {}", m.version)
                }
                crate::model::InputRecord::Pick(p) => format!("pick {}", p.observation_id),
            };
            match record {
                crate::model::InputRecord::Station(station) => {
                    station.validate().map_err(|e| {
                        format!("record {} ({record_label}): {e}", record_index + 1)
                    })?;
                    let exists: Option<String> = transaction
                        .query_row(
                            "SELECT station_id FROM stations WHERE station_id=?",
                            params![station.station_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    if exists.is_none() {
                        transaction.execute("INSERT INTO stations(station_id, latitude, longitude, elevation_m, first_batch_hash) VALUES (?, ?, ?, ?, ?)",
                            params![station.station_id, station.latitude, station.longitude, station.elevation_m, content_hash]).map_err(|e| e.to_string())?;
                        station_count += 1;
                    }
                }
                crate::model::InputRecord::VelocityModel(model) => {
                    model.validate().map_err(|e| {
                        format!("record {} ({record_label}): {e}", record_index + 1)
                    })?;
                    let exists: Option<String> = transaction
                        .query_row(
                            "SELECT version FROM velocity_models WHERE version=?",
                            params![model.version],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    if exists.is_none() {
                        for (ordinal, layer) in model.layers.iter().enumerate() {
                            transaction.execute("INSERT INTO velocity_models(version, ordinal, depth_top_m, depth_bottom_m, vp_m_s, vs_m_s, batch_hash) VALUES (?, ?, ?, ?, ?, ?, ?)",
                                params![model.version, ordinal as i64, layer.depth_top_m, layer.depth_bottom_m, layer.vp_m_s, layer.pub_vs_m_s, content_hash]).map_err(|e| e.to_string())?;
                        }
                        model_count += 1;
                    }
                }
                crate::model::InputRecord::Pick(pick) => {
                    let at = pick.validated_time()?;
                    pick.validate(at).map_err(|e| {
                        format!("record {} ({record_label}): {e}", record_index + 1)
                    })?;
                    if transaction
                        .query_row(
                            "SELECT 1 FROM stations WHERE station_id=?",
                            params![pick.station_id],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?
                        .is_none()
                    {
                        return Err(format!(
                            "pick {} references unknown station {}",
                            pick.observation_id, pick.station_id
                        ));
                    }
                    if transaction
                        .query_row(
                            "SELECT 1 FROM velocity_models WHERE version=?",
                            params![pick.velocity_model_version],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?
                        .is_none()
                    {
                        return Err(format!(
                            "pick {} references unknown velocity model {}",
                            pick.observation_id, pick.velocity_model_version
                        ));
                    }
                    let exists: Option<String> = transaction
                        .query_row(
                            "SELECT observation_id FROM picks WHERE observation_id=?",
                            params![pick.observation_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    if exists.is_none() {
                        let phase = Phase::parse(&pick.phase_text)?;
                        transaction.execute("INSERT INTO picks(observation_id, related_observation_id, station_id, phase, raw_time, time_uncertainty_s, confidence, source, velocity_model_version, batch_hash) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            params![pick.observation_id, pick.related_observation_id, pick.station_id, phase_name_db(phase), at, pick.time_uncertainty_s, pick.confidence, pick.source, pick.velocity_model_version, content_hash]).map_err(|e| e.to_string())?;
                        pick_count += 1;
                    }
                }
            }
        }
        let manual_relations: Vec<(String, String)> = transaction.prepare("SELECT observation_id, related_observation_id FROM picks WHERE source='manual' AND related_observation_id IS NOT NULL").map_err(|e| e.to_string())?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(|e| e.to_string())?
            .collect::<Result<_, _>>().map_err(|e| e.to_string())?;
        for (manual_id, related_id) in manual_relations {
            let updated = transaction
                .execute(
                    "UPDATE picks SET superseded=1 WHERE observation_id=? AND source='auto'",
                    params![related_id],
                )
                .map_err(|e| e.to_string())?;
            if updated == 0 {
                return Err(format!(
                    "manual pick {manual_id} refers to missing auto pick {related_id}"
                ));
            }
        }
        transaction.commit().map_err(|e| e.to_string())?;
        Ok(ImportSummary {
            content_hash,
            duplicate: false,
            stations: station_count,
            models: model_count,
            picks: pick_count,
        })
    }
}

fn phase_name_db(phase: Phase) -> &'static str {
    match phase {
        Phase::P => "P",
        Phase::S => "S",
    }
}

impl Store {
    pub fn ensure_correction_v1(&self) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        transaction.execute("INSERT OR IGNORE INTO correction_versions(version, parent_version, created_at, note) VALUES ('v1', NULL, ?, 'seed zero correction')", params![now_ordinal()]).map_err(|e| e.to_string())?;
        let station_ids: Vec<String> = transaction
            .prepare("SELECT station_id FROM stations ORDER BY station_id")
            .map_err(|e| e.to_string())?
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        for station_id in station_ids {
            transaction.execute("INSERT OR IGNORE INTO clock_segments(version, station_id, ordinal, start_time, end_time, offset_s) VALUES ('v1', ?, 0, -1e18, NULL, 0)", params![station_id]).map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())
    }

    pub fn ensure_hypotheses(&self) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        for (id, name) in [
            ("H-A", "source hypothesis A"),
            ("H-B", "parallel source hypothesis B"),
        ] {
            transaction.execute("INSERT OR IGNORE INTO hypotheses(hypothesis_id, name, correction_version, current_set_id, created_at) VALUES (?, ?, 'v1', NULL, ?)",
                params![id, name, now_ordinal()]).map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())
    }

    fn log_locked(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        operation: &str,
        hypothesis: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String> {
        let encoded = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
        let hash = hash_bytes(&encoded);
        transaction.execute("INSERT INTO operation_log(occurred_at, actor, operation, target_hypothesis, payload_json, content_hash) VALUES (?, 'user', ?, ?, ?, ?)",
            params![now_ordinal(), operation, hypothesis, payload.to_string(), hash]).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_lock(
        &self,
        hypothesis: &str,
        observation_id: &str,
        event_key: Option<&str>,
        locked: bool,
    ) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        if transaction
            .query_row(
                "SELECT 1 FROM hypotheses WHERE hypothesis_id=?",
                params![hypothesis],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("unknown hypothesis".to_string());
        }
        if transaction
            .query_row(
                "SELECT 1 FROM picks WHERE observation_id=? AND COALESCE(superseded,0)=0",
                params![observation_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("unknown or superseded pick".to_string());
        }
        if locked {
            transaction.execute("INSERT INTO locked_picks(hypothesis_id, observation_id, event_key, locked_at) VALUES (?, ?, ?, ?) ON CONFLICT(hypothesis_id, observation_id) DO UPDATE SET event_key=excluded.event_key, locked_at=excluded.locked_at",
                params![hypothesis, observation_id, event_key, now_ordinal()]).map_err(|e| e.to_string())?;
        } else {
            transaction
                .execute(
                    "DELETE FROM locked_picks WHERE hypothesis_id=? AND observation_id=?",
                    params![hypothesis, observation_id],
                )
                .map_err(|e| e.to_string())?;
        }
        self.log_locked(
            &transaction,
            if locked { "lock-pick" } else { "unlock-pick" },
            hypothesis,
            &serde_json::json!({"observation_id": observation_id, "event_key": event_key}),
        )?;
        transaction.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_noise(&self, observation_id: &str, noise: bool) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        if noise {
            if transaction
                .query_row(
                    "SELECT 1 FROM picks WHERE observation_id=? AND COALESCE(superseded,0)=0",
                    params![observation_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|e| e.to_string())?
                .is_none()
            {
                return Err("unknown or superseded pick".to_string());
            }
            let locked_elsewhere: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM locked_picks WHERE observation_id=?",
                    params![observation_id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if locked_elsewhere > 0 {
                return Err(
                    "unlock this pick in every hypothesis before marking it noise".to_string(),
                );
            }
            transaction.execute("INSERT OR IGNORE INTO noise_picks(observation_id, marked_at, note) VALUES (?, ?, '')", params![observation_id, now_ordinal()]).map_err(|e| e.to_string())?;
        } else {
            transaction
                .execute(
                    "DELETE FROM noise_picks WHERE observation_id=?",
                    params![observation_id],
                )
                .map_err(|e| e.to_string())?;
        }
        self.log_locked(
            &transaction,
            if noise { "mark-noise" } else { "unmark-noise" },
            "",
            &serde_json::json!({"observation_id": observation_id, "noise": noise}),
        )?;
        transaction.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn create_correction_version(
        &self,
        version: &str,
        parent_version: &str,
        offsets: &BTreeMap<String, f64>,
        note: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        if transaction
            .query_row(
                "SELECT 1 FROM correction_versions WHERE version=?",
                params![version],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err("correction version already exists".to_string());
        }
        if transaction
            .query_row(
                "SELECT 1 FROM correction_versions WHERE version=?",
                params![parent_version],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("unknown parent correction version".to_string());
        }
        transaction.execute("INSERT INTO correction_versions(version, parent_version, created_at, note) VALUES (?, ?, ?, ?)", params![version, parent_version, now_ordinal(), note]).map_err(|e| e.to_string())?;
        let mut station_ids: Vec<String> = transaction
            .prepare("SELECT station_id FROM stations ORDER BY station_id")
            .map_err(|e| e.to_string())?
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        station_ids.sort();
        for station_id in station_ids {
            let offset = offsets.get(&station_id).copied().unwrap_or(0.0);
            if !offset.is_finite() {
                return Err(format!("offset for {station_id} is not finite"));
            }
            transaction.execute("INSERT INTO clock_segments(version, station_id, ordinal, start_time, end_time, offset_s) VALUES (?, ?, 0, -1e18, NULL, ?)", params![version, station_id, offset]).map_err(|e| e.to_string())?;
        }
        let payload = serde_json::json!({"version": version, "parent_version": parent_version, "offsets": offsets, "note": note});
        self.log_locked(&transaction, "create-correction", "", &payload)?;
        transaction.commit().map_err(|e| e.to_string())
    }
}

impl Store {
    pub fn correction_version_for(&self, hypothesis: &str) -> Result<String, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .query_row(
                "SELECT correction_version FROM hypotheses WHERE hypothesis_id=?",
                params![hypothesis],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())
    }

    pub fn set_hypothesis_correction(&self, hypothesis: &str, version: &str) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        if transaction
            .query_row(
                "SELECT 1 FROM correction_versions WHERE version=?",
                params![version],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("unknown correction version".to_string());
        }
        transaction
            .execute(
                "UPDATE hypotheses SET correction_version=? WHERE hypothesis_id=?",
                params![version, hypothesis],
            )
            .map_err(|e| e.to_string())?;
        self.log_locked(
            &transaction,
            "select-correction",
            hypothesis,
            &serde_json::json!({"version": version}),
        )?;
        transaction.commit().map_err(|e| e.to_string())
    }

    fn station_center(&self, connection: &Connection) -> Result<(f64, f64), String> {
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM stations", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok((0.0, 0.0));
        }
        let (lat, lon): (f64, f64) = connection
            .query_row(
                "SELECT COALESCE(AVG(latitude),0), COALESCE(AVG(longitude),0) FROM stations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        Ok((lat, lon))
    }

    pub fn solver_input(
        &self,
        hypothesis: &str,
        correction_version: &str,
    ) -> Result<SolverInput, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let (origin_lat, origin_lon) = self.station_center(&connection)?;
        let mut input = SolverInput::default();
        {
            let mut statement = connection.prepare("SELECT station_id, latitude, longitude, elevation_m FROM stations ORDER BY station_id").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let latitude: f64 = row.get(1)?;
                    let longitude: f64 = row.get(2)?;
                    let elevation_m: f64 = row.get(3)?;
                    let (x, y) =
                        crate::model::project_xy(latitude, longitude, origin_lat, origin_lon);
                    Ok(Station {
                        id,
                        latitude,
                        longitude,
                        elevation_m,
                        x,
                        y,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let station = row.map_err(|e| e.to_string())?;
                input.stations.insert(station.id.clone(), station);
            }
        }
        {
            let mut statement = connection.prepare("SELECT version, ordinal, depth_top_m, depth_bottom_m, vp_m_s, vs_m_s FROM velocity_models ORDER BY version, ordinal").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        LayerRecord {
                            depth_top_m: row.get(2)?,
                            depth_bottom_m: row.get(3)?,
                            vp_m_s: row.get(4)?,
                            pub_vs_m_s: row.get(5)?,
                        },
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (version, _, layer) = row.map_err(|e| e.to_string())?;
                input.models.entry(version).or_default().push(layer);
            }
        }
        {
            let mut corrections: BTreeMap<String, Vec<ClockSegment>> = BTreeMap::new();
            let mut statement = connection.prepare("SELECT station_id, start_time, end_time, offset_s FROM clock_segments WHERE version=? ORDER BY station_id, ordinal").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map(params![correction_version], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        ClockSegment {
                            start_time: row.get(1)?,
                            end_time: row.get(2)?,
                            offset_s: row.get(3)?,
                        },
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (station, segment) = row.map_err(|e| e.to_string())?;
                corrections.entry(station).or_default().push(segment);
            }
            let mut statement = connection.prepare("SELECT observation_id, related_observation_id, station_id, phase, raw_time, time_uncertainty_s, confidence, source, velocity_model_version, COALESCE(superseded,0) FROM picks WHERE observation_id NOT IN (SELECT observation_id FROM noise_picks) ORDER BY observation_id").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let related_id: Option<String> = row.get(1)?;
                    let station_id: String = row.get(2)?;
                    let phase_text: String = row.get(3)?;
                    let raw_time: f64 = row.get(4)?;
                    let uncertainty: f64 = row.get(5)?;
                    let confidence: f64 = row.get(6)?;
                    let source: String = row.get(7)?;
                    let model_version: String = row.get(8)?;
                    let superseded: i64 = row.get(9)?;
                    Ok((
                        id,
                        related_id,
                        station_id,
                        phase_text,
                        raw_time,
                        uncertainty,
                        confidence,
                        source,
                        model_version,
                        superseded,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (
                    id,
                    related_id,
                    station_id,
                    phase_text,
                    raw_time,
                    uncertainty,
                    confidence,
                    source,
                    model_version,
                    superseded,
                ) = row.map_err(|e| e.to_string())?;
                if superseded != 0 {
                    continue;
                }
                let phase = Phase::parse(&phase_text)?;
                let segments = corrections.get(&station_id);
                let offset = segments
                    .and_then(|segments| {
                        segments
                            .iter()
                            .find(|segment| {
                                raw_time >= segment.start_time
                                    && segment.end_time.map_or(true, |end| raw_time < end)
                            })
                            .map(|segment| segment.offset_s)
                    })
                    .unwrap_or(0.0);
                input.picks.insert(
                    id.clone(),
                    Pick {
                        id,
                        related_id,
                        station_id: station_id.clone(),
                        phase,
                        raw_time,
                        corrected_time: raw_time - offset,
                        uncertainty_s: uncertainty,
                        confidence,
                        source,
                        model_version,
                    },
                );
            }
        }
        {
            let mut statement = connection.prepare("SELECT observation_id, event_key FROM locked_picks WHERE hypothesis_id=? ORDER BY observation_id").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map(params![hypothesis], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (id, event_key) = row.map_err(|e| e.to_string())?;
                if input.picks.contains_key(&id) {
                    input.locked.insert(id, event_key.unwrap_or_default());
                }
            }
        }
        Ok(input)
    }

    pub fn save_result(
        &self,
        hypothesis: &str,
        correction_version: &str,
        result: &SolverResultSet,
        input: &SolverInput,
        event_key: Option<&str>,
    ) -> Result<i64, String> {
        let result_json = serde_json::to_string(result).map_err(|e| e.to_string())?;
        let mut input_snapshot =
            serde_json::to_vec(&(input, hypothesis, correction_version, event_key))
                .map_err(|e| e.to_string())?;
        input_snapshot.extend_from_slice(result_json.as_bytes());
        let input_hash = hash_bytes(&input_snapshot);
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        let parent_set: Option<i64> = transaction
            .query_row(
                "SELECT current_set_id FROM hypotheses WHERE hypothesis_id=?",
                params![hypothesis],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO candidate_sets(hypothesis_id, correction_version, parent_set_id, created_at, input_hash, result_json) VALUES (?, ?, ?, ?, ?, ?)",
            params![hypothesis, correction_version, parent_set, now_ordinal(), input_hash, result_json]).map_err(|e| e.to_string())?;
        let set_id = transaction.last_insert_rowid();
        transaction
            .execute(
                "UPDATE hypotheses SET current_set_id=? WHERE hypothesis_id=?",
                params![set_id, hypothesis],
            )
            .map_err(|e| e.to_string())?;
        self.log_locked(&transaction, "freeze-candidate-set", hypothesis, &serde_json::json!({"set_id": set_id, "correction_version": correction_version, "parent_set_id": parent_set}))?;
        transaction.commit().map_err(|e| e.to_string())?;
        Ok(set_id)
    }

    pub fn recompute(
        &self,
        hypothesis: &str,
        explicit_correction: Option<&str>,
        event_key: Option<&str>,
    ) -> Result<(i64, SolverResultSet), String> {
        let correction_version = match explicit_correction {
            Some(version) => version.to_string(),
            None => self.correction_version_for(hypothesis)?,
        };
        let input = self.solver_input(hypothesis, &correction_version)?;
        let fresh = crate::solver::solve(&input, &correction_version)?;
        let result = if let Some(event_key) = event_key {
            let connection = self.connection.lock().map_err(|e| e.to_string())?;
            let parent_json: Option<String> = connection.query_row(
                "SELECT cs.result_json FROM hypotheses h LEFT JOIN candidate_sets cs ON cs.set_id=h.current_set_id WHERE h.hypothesis_id=?",
                params![hypothesis],
                |row| row.get(0),
            ).optional().map_err(|e| e.to_string())?;
            match parent_json {
                Some(json) => {
                    let mut parent: SolverResultSet =
                        serde_json::from_str(&json).map_err(|e| e.to_string())?;
                    parent.events.retain(|event| event.event_key != event_key);
                    let (mut moved, _untouched) = fresh
                        .events
                        .into_iter()
                        .partition(|event| event.event_key == event_key);
                    parent.events.append(&mut moved);
                    parent.events.sort_by(|a, b| a.event_key.cmp(&b.event_key));
                    let recomputed_ids: std::collections::BTreeSet<String> = fresh
                        .rejected
                        .iter()
                        .map(|item| item.observation_id.clone())
                        .collect();
                    parent
                        .rejected
                        .retain(|item| !recomputed_ids.contains(&item.observation_id));
                    parent.rejected.extend(fresh.rejected);
                    parent.rejected.sort_by(|a, b| {
                        a.observation_id
                            .cmp(&b.observation_id)
                            .then(a.reason.cmp(&b.reason))
                    });
                    parent.rejected.dedup_by(|a, b| {
                        a.observation_id == b.observation_id && a.reason == b.reason
                    });
                    parent
                }
                None => fresh,
            }
        } else {
            fresh
        };
        let set_id =
            self.save_result(hypothesis, &correction_version, &result, &input, event_key)?;
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        self.log_locked(&transaction, "recompute", hypothesis, &serde_json::json!({"set_id": set_id, "event_key": event_key, "correction_version": correction_version}))?;
        transaction.commit().map_err(|e| e.to_string())?;
        Ok((set_id, result))
    }
}

#[derive(serde::Serialize)]
pub struct StateView {
    pub hypotheses: Vec<HypothesisView>,
    pub stations: Vec<StationView>,
    pub velocity_models: BTreeMap<String, Vec<LayerRecord>>,
    pub picks: Vec<PickView>,
    pub correction_versions: BTreeMap<String, CorrectionView>,
    pub current: BTreeMap<String, SolverResultSet>,
    pub rejected_global: Vec<String>,
    pub operation_log: Vec<LogView>,
}

#[derive(serde::Serialize)]
pub struct HypothesisView {
    pub hypothesis_id: String,
    pub name: String,
    pub correction_version: String,
    pub current_set_id: Option<i64>,
    pub locked: BTreeMap<String, Option<String>>,
}

#[derive(serde::Serialize)]
pub struct StationView {
    pub station_id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
}

#[derive(serde::Serialize)]
pub struct PickView {
    pub observation_id: String,
    pub related_observation_id: Option<String>,
    pub station_id: String,
    pub phase: String,
    pub raw_time: f64,
    pub time_uncertainty_s: f64,
    pub confidence: f64,
    pub source: String,
    pub velocity_model_version: String,
    pub superseded: bool,
    pub noise: bool,
}

#[derive(serde::Serialize, Default)]
pub struct CorrectionView {
    pub parent_version: Option<String>,
    pub note: String,
    pub segments: Vec<SegmentView>,
}

#[derive(serde::Serialize)]
pub struct SegmentView {
    pub station_id: String,
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub offset_s: f64,
}

#[derive(serde::Serialize)]
pub struct LogView {
    pub log_id: i64,
    pub occurred_at: f64,
    pub operation: String,
    pub target_hypothesis: String,
    pub payload_json: String,
    pub content_hash: String,
}

impl Store {
    pub fn state(&self) -> Result<StateView, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut stations = Vec::new();
        let mut statement = connection.prepare("SELECT station_id, latitude, longitude, elevation_m FROM stations ORDER BY station_id").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(StationView {
                    station_id: row.get(0)?,
                    latitude: row.get(1)?,
                    longitude: row.get(2)?,
                    elevation_m: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            stations.push(row.map_err(|e| e.to_string())?);
        }

        let mut velocity_models: BTreeMap<String, Vec<LayerRecord>> = BTreeMap::new();
        let mut statement = connection.prepare("SELECT version, depth_top_m, depth_bottom_m, vp_m_s, vs_m_s FROM velocity_models ORDER BY version, ordinal").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    LayerRecord {
                        depth_top_m: row.get(1)?,
                        depth_bottom_m: row.get(2)?,
                        vp_m_s: row.get(3)?,
                        pub_vs_m_s: row.get(4)?,
                    },
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (version, layer) = row.map_err(|e| e.to_string())?;
            velocity_models.entry(version).or_default().push(layer);
        }

        let mut picks = Vec::new();
        let mut statement = connection.prepare("SELECT observation_id, related_observation_id, station_id, phase, raw_time, time_uncertainty_s, confidence, source, velocity_model_version, COALESCE(superseded,0), EXISTS(SELECT 1 FROM noise_picks WHERE noise_picks.observation_id=picks.observation_id) FROM picks ORDER BY observation_id").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(PickView {
                    observation_id: row.get(0)?,
                    related_observation_id: row.get(1)?,
                    station_id: row.get(2)?,
                    phase: row.get(3)?,
                    raw_time: row.get(4)?,
                    time_uncertainty_s: row.get(5)?,
                    confidence: row.get(6)?,
                    source: row.get(7)?,
                    velocity_model_version: row.get(8)?,
                    superseded: row.get::<_, i64>(9)? != 0,
                    noise: row.get::<_, i64>(10)? != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            picks.push(row.map_err(|e| e.to_string())?);
        }

        let mut correction_versions: BTreeMap<String, CorrectionView> = BTreeMap::new();
        let mut statement = connection.prepare("SELECT cv.version, cv.parent_version, cv.note, cs.station_id, cs.start_time, cs.end_time, cs.offset_s FROM correction_versions cv LEFT JOIN clock_segments cs ON cs.version=cv.version ORDER BY cv.version, cs.station_id, cs.ordinal").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                    row.get::<_, Option<Option<f64>>>(5)?,
                    row.get::<_, Option<f64>>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (version, parent, note, station_id, start, end, offset) =
                row.map_err(|e| e.to_string())?;
            let view = correction_versions
                .entry(version)
                .or_insert_with(|| CorrectionView {
                    parent_version: parent,
                    note,
                    segments: Vec::new(),
                });
            if let (Some(station_id), Some(start), Some(end), Some(offset)) =
                (station_id, start, end, offset)
            {
                view.segments.push(SegmentView {
                    station_id,
                    start_time: start,
                    end_time: end,
                    offset_s: offset,
                });
            }
        }

        let mut hypotheses = Vec::new();
        let mut current = BTreeMap::new();
        let mut statement = connection.prepare("SELECT hypothesis_id, name, correction_version, current_set_id FROM hypotheses ORDER BY hypothesis_id").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (id, name, correction_version, current_set_id) = row.map_err(|e| e.to_string())?;
            let mut locked = BTreeMap::new();
            let mut lock_statement = connection.prepare("SELECT observation_id, event_key FROM locked_picks WHERE hypothesis_id=? ORDER BY observation_id").map_err(|e| e.to_string())?;
            let lock_rows = lock_statement
                .query_map(params![id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for lock_row in lock_rows {
                let (pick_id, event_key) = lock_row.map_err(|e| e.to_string())?;
                locked.insert(pick_id, event_key);
            }
            if let Some(set_id) = current_set_id {
                let result_json: String = connection
                    .query_row(
                        "SELECT result_json FROM candidate_sets WHERE set_id=?",
                        params![set_id],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                let result: SolverResultSet =
                    serde_json::from_str(&result_json).map_err(|e| e.to_string())?;
                current.insert(id.clone(), result);
            }
            hypotheses.push(HypothesisView {
                hypothesis_id: id,
                name,
                correction_version,
                current_set_id,
                locked,
            });
        }

        let mut operation_log = Vec::new();
        let mut statement = connection.prepare("SELECT log_id, occurred_at, operation, COALESCE(target_hypothesis,''), payload_json, content_hash FROM operation_log ORDER BY log_id").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(LogView {
                    log_id: row.get(0)?,
                    occurred_at: row.get(1)?,
                    operation: row.get(2)?,
                    target_hypothesis: row.get(3)?,
                    payload_json: row.get(4)?,
                    content_hash: row.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            operation_log.push(row.map_err(|e| e.to_string())?);
        }

        Ok(StateView {
            hypotheses,
            stations,
            velocity_models,
            picks,
            correction_versions,
            current,
            rejected_global: Vec::new(),
            operation_log,
        })
    }

    pub fn candidate_set(&self, set_id: i64) -> Result<SolverResultSet, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let result_json: String = connection
            .query_row(
                "SELECT result_json FROM candidate_sets WHERE set_id=?",
                params![set_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&result_json).map_err(|e| e.to_string())
    }

    pub fn candidate_sets(&self) -> Result<Vec<serde_json::Value>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection.prepare("SELECT set_id, hypothesis_id, correction_version, parent_set_id, created_at FROM candidate_sets ORDER BY set_id").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "set_id": row.get::<_, i64>(0)?,
                    "hypothesis_id": row.get::<_, String>(1)?,
                    "correction_version": row.get::<_, String>(2)?,
                    "parent_set_id": row.get::<_, Option<i64>>(3)?,
                    "created_at": row.get::<_, f64>(4)?,
                }))
            })
            .map_err(|e| e.to_string())?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| e.to_string())?);
        }
        Ok(result)
    }
}
