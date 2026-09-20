CREATE TABLE IF NOT EXISTS schema_version (
  version INTEGER PRIMARY KEY
);
INSERT OR IGNORE INTO schema_version(version) VALUES (1);

CREATE TABLE IF NOT EXISTS import_batches (
  batch_id TEXT PRIMARY KEY,
  source_name TEXT NOT NULL,
  content_hash TEXT NOT NULL UNIQUE,
  record_count INTEGER NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('committed', 'superseded')),
  imported_at_epoch_seconds REAL NOT NULL DEFAULT (julianday('now') * 86400 - 2440587.5 * 86400)
);

CREATE TABLE IF NOT EXISTS import_records (
  batch_id TEXT NOT NULL REFERENCES import_batches(batch_id),
  line_number INTEGER NOT NULL,
  record_type TEXT NOT NULL,
  canonical_json TEXT NOT NULL,
  PRIMARY KEY (batch_id, line_number)
);

CREATE TABLE IF NOT EXISTS stations (
  station_id TEXT PRIMARY KEY,
  x_km REAL NOT NULL,
  y_km REAL NOT NULL,
  elevation_km REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS velocity_models (
  model_id TEXT PRIMARY KEY,
  vp_kms REAL NOT NULL CHECK (vp_kms > 0),
  vs_kms REAL NOT NULL CHECK (vs_kms > 0 AND vs_kms < vp_kms),
  x_min_km REAL NOT NULL,
  x_max_km REAL NOT NULL,
  y_min_km REAL NOT NULL,
  y_max_km REAL NOT NULL,
  CHECK (x_min_km < x_max_km AND y_min_km < y_max_km)
);

CREATE TABLE IF NOT EXISTS clock_segments (
  version TEXT NOT NULL,
  station_id TEXT NOT NULL REFERENCES stations(station_id),
  start_time TEXT NOT NULL,
  end_time TEXT,
  bias_seconds REAL NOT NULL,
  rate_s_per_s REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (version, station_id, start_time, end_time)
);

CREATE TABLE IF NOT EXISTS picks (
  pick_id TEXT PRIMARY KEY,
  station_id TEXT NOT NULL REFERENCES stations(station_id),
  phase TEXT NOT NULL CHECK (phase IN ('P', 'S')),
  time TEXT NOT NULL,
  sigma_seconds REAL NOT NULL CHECK (sigma_seconds > 0),
  confidence REAL NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
  source TEXT NOT NULL CHECK (source IN ('auto', 'manual')),
  model_id TEXT NOT NULL REFERENCES velocity_models(model_id),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'locked', 'noise')),
  locked_event TEXT
);

CREATE TABLE IF NOT EXISTS events (
  event_key TEXT PRIMARY KEY,
  created_by_run_id TEXT NOT NULL,
  selected_candidate_id TEXT
);

CREATE TABLE IF NOT EXISTS solver_runs (
  run_id TEXT PRIMARY KEY,
  clock_version TEXT NOT NULL,
  scope_json TEXT NOT NULL,
  rationale TEXT NOT NULL,
  created_by TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS candidate_snapshots (
  candidate_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES solver_runs(run_id),
  event_key TEXT NOT NULL,
  rank INTEGER NOT NULL,
  score REAL NOT NULL,
  status TEXT NOT NULL,
  solution_json TEXT NOT NULL,
  UNIQUE (run_id, event_key, rank)
);

CREATE TABLE IF NOT EXISTS candidate_pick_snapshots (
  candidate_id TEXT NOT NULL REFERENCES candidate_snapshots(candidate_id),
  pick_id TEXT NOT NULL,
  accepted INTEGER NOT NULL,
  locked INTEGER NOT NULL,
  residual_seconds REAL,
  contribution REAL,
  reason TEXT,
  PRIMARY KEY (candidate_id, pick_id)
);

CREATE TABLE IF NOT EXISTS event_hypotheses (
  hypothesis_id TEXT PRIMARY KEY,
  event_key TEXT NOT NULL REFERENCES events(event_key),
  parent_candidate_id TEXT REFERENCES candidate_snapshots(candidate_id),
  frozen_candidate_id TEXT REFERENCES candidate_snapshots(candidate_id),
  label TEXT NOT NULL,
  active_clock_version TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS operation_log (
  log_id TEXT PRIMARY KEY,
  action TEXT NOT NULL,
  detail_json TEXT NOT NULL
);
