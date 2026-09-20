use crate::db::Store;
use crate::model::{Pick, Station, VelocityModel};
use crate::solver::{solve_network, SolveContext};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

pub fn run(listen: &str, database_path: &str, demo_path: &str) -> Result<(), String> {
    let store = Store::open(database_path)?;
    if store.stations()?.is_empty() {
        let demo = std::fs::read_to_string(demo_path).map_err(|error| error.to_string())?;
        store.import_jsonl(demo_path, &demo)?;
    }
    if store.latest_solutions()?.is_empty() {
        let initial_scope = ["event-001", "event-002", "event-003"].map(str::to_string);
        let run_id = compute_and_save(
            &store,
            "clock-v1",
            &initial_scope,
            "initial_deterministic_baseline",
            &BTreeMap::new(),
        )?;
        println!("initial solver run {run_id}");
    }
    let listener = TcpListener::bind(listen).map_err(|error| error.to_string())?;
    println!("listening on http://{listen}");
    for stream in listener.incoming() {
        let stream = stream.map_err(|error| error.to_string())?;
        if let Err(error) = handle(stream, &store) {
            eprintln!("request failed: {error}");
        }
    }
    Ok(())
}

pub(crate) fn compute_and_save(
    store: &Store,
    clock_version: &str,
    scope: &[String],
    rationale: &str,
    forced_locks: &BTreeMap<String, String>,
) -> Result<String, String> {
    let stations = store.stations()?;
    let stored_model = store.model()?;
    let picks = store.picks()?;
    let segments = store.segments(clock_version)?;
    let station_models: Vec<Station> = stations
        .iter()
        .map(|station| Station {
            station_id: station.station_id.clone(),
            x_km: station.x_km,
            y_km: station.y_km,
            elevation_km: station.elevation_km,
        })
        .collect();
    let model = VelocityModel {
        model_id: stored_model.model_id,
        vp_kms: stored_model.vp_kms,
        vs_kms: stored_model.vs_kms,
        x_min_km: stored_model.x_min_km,
        x_max_km: stored_model.x_max_km,
        y_min_km: stored_model.y_min_km,
        y_max_km: stored_model.y_max_km,
    };
    let pick_models: Vec<Pick> = picks
        .iter()
        .map(|pick| Pick {
            pick_id: pick.pick_id.clone(),
            station_id: pick.station_id.clone(),
            phase: pick.phase,
            time: pick.time.clone(),
            sigma_seconds: pick.sigma_seconds,
            confidence: pick.confidence,
            source: pick.source,
            model_id: pick.model_id.clone(),
            status: pick.status,
        })
        .collect();
    let segment_tuples: Vec<(String, f64, f64, f64)> = segments
        .iter()
        .map(|segment| {
            Ok((
                segment.station_id.clone(),
                crate::model::parse_epoch(&segment.start_time)?,
                segment
                    .end_time
                    .as_ref()
                    .map(|value| crate::model::parse_epoch(value))
                    .transpose()?
                    .unwrap_or(f64::INFINITY),
                segment.bias_seconds,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut locks = BTreeMap::new();
    for (pick_id, event_key) in forced_locks {
        locks.insert(pick_id.clone(), event_key.clone());
    }
    for pick in &picks {
        if let Some(event_key) = &pick.locked_event {
            locks.insert(pick.pick_id.clone(), event_key.clone());
        }
    }
    let context = SolveContext {
        stations: &station_models,
        model: &model,
        picks: &pick_models,
        segments: &segment_tuples,
        locks: &locks,
    };
    let mut candidates = solve_network(context);
    candidates.retain(|candidate| scope.is_empty() || scope.contains(&candidate.event_key));
    let values = candidates
        .iter()
        .map(|candidate| serde_json::to_value(candidate).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    store.save_solution(clock_version, scope, rationale, &values)
}

fn handle(mut stream: TcpStream, store: &Store) -> Result<(), String> {
    let mut request = vec![0; 1024 * 1024];
    let read = stream
        .read(&mut request)
        .map_err(|error| error.to_string())?;
    let request = String::from_utf8_lossy(&request[..read]).to_string();
    let mut lines = request.lines();
    let request_line = lines.next().unwrap_or("GET / HTTP/1.1");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");
    let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
    let (status, content_type, body_out) = route(method, target, path, body, store)?;
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_out.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|e| e.to_string())?;
    stream
        .write_all(body_out.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn route(
    method: &str,
    target: &str,
    path: &str,
    body: &str,
    store: &Store,
) -> Result<(String, &'static str, String), String> {
    match (method, path) {
        ("GET", "/") => Ok((
            "200 OK".to_string(),
            "text/html; charset=utf-8",
            std::fs::read_to_string("static/index.html").map_err(|e| e.to_string())?,
        )),
        ("GET", "/api/state") => state(store),
        ("POST", "/api/import") => import(body, store),
        ("POST", "/api/picks/status") => pick_status(body, store),
        ("POST", "/api/solve") => solve(body, store),
        ("POST", "/api/freeze") => freeze(body, store),
        ("POST", "/api/hypotheses/duplicate") => duplicate(body, store),
        ("POST", "/api/hypotheses/recompute") => recompute_hypothesis(body, store),
        ("GET", "/api/runs") => runs(store),
        ("GET", "/api/run") => run_detail(target, store),
        ("GET", "/api/hypotheses") => hypotheses(store),
        ("GET", "/api/batches") => batches(store),
        ("GET", "/api/logs") => logs(store),
        _ => Ok((
            "404 Not Found".to_string(),
            "application/json",
            json!({"error":"not_found"}).to_string(),
        )),
    }
}

fn json_response(value: serde_json::Value) -> Result<(String, &'static str, String), String> {
    Ok((
        "200 OK".to_string(),
        "application/json; charset=utf-8",
        value.to_string(),
    ))
}

fn state(store: &Store) -> Result<(String, &'static str, String), String> {
    let mut segments = serde_json::Map::new();
    for version in ["clock-v1", "clock-v2"] {
        segments.insert(version.to_string(), json!(store.segments(version)?));
    }
    json_response(json!({
        "stations": store.stations()?,
        "model": store.model()?,
        "picks": store.picks()?,
        "clock_versions": ["clock-v1", "clock-v2"],
        "segments": segments,
        "candidates": store.latest_solutions()?,
        "runs": store.runs()?,
        "hypotheses": store.hypotheses()?,
    }))
}

fn runs(store: &Store) -> Result<(String, &'static str, String), String> {
    json_response(json!(store.runs()?))
}

fn run_detail(target: &str, store: &Store) -> Result<(String, &'static str, String), String> {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    let run_id = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("id="))
        .ok_or("missing id")?;
    json_response(json!(store.run_solutions(run_id)?))
}

fn hypotheses(store: &Store) -> Result<(String, &'static str, String), String> {
    json_response(json!(store.hypotheses()?))
}

fn import(body: &str, store: &Store) -> Result<(String, &'static str, String), String> {
    let request: serde_json::Value =
        serde_json::from_str(body).map_err(|error| error.to_string())?;
    let content = request["content"].as_str().ok_or("missing content")?;
    let source = request["source_name"].as_str().unwrap_or("api-import");
    let outcome = store.import_jsonl(source, content)?;
    json_response(json!(outcome))
}

fn pick_status(body: &str, store: &Store) -> Result<(String, &'static str, String), String> {
    let request: serde_json::Value =
        serde_json::from_str(body).map_err(|error| error.to_string())?;
    let pick_id = request["pick_id"].as_str().ok_or("missing pick_id")?;
    let status = request["status"].as_str().ok_or("missing status")?;
    let locked_event = request["locked_event"].as_str();
    if !["active", "locked", "noise"].contains(&status) {
        return Err("status must be active, locked or noise".to_string());
    }
    store.set_pick_status(pick_id, status, locked_event, "web")?;
    json_response(json!({"ok":true}))
}

fn solve(body: &str, store: &Store) -> Result<(String, &'static str, String), String> {
    let request: serde_json::Value =
        serde_json::from_str(body).map_err(|error| error.to_string())?;
    let clock_version = request["clock_version"].as_str().unwrap_or("clock-v1");
    let rationale = request["rationale"]
        .as_str()
        .unwrap_or("user_requested_recompute");
    let scope: Vec<String> = request["scope"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    let run_id = compute_and_save(store, clock_version, &scope, rationale, &BTreeMap::new())?;
    json_response(json!({"run_id":run_id}))
}

fn recompute_hypothesis(
    body: &str,
    store: &Store,
) -> Result<(String, &'static str, String), String> {
    let request: serde_json::Value =
        serde_json::from_str(body).map_err(|error| error.to_string())?;
    let hypothesis_id = request["hypothesis_id"]
        .as_str()
        .ok_or("missing hypothesis_id")?;
    let (event_key, locked_pick_ids, stored_clock_version) =
        store.recompute_hypothesis_context(hypothesis_id)?;
    let clock_version = request["clock_version"]
        .as_str()
        .unwrap_or(stored_clock_version.as_str());
    let locks = locked_pick_ids
        .into_iter()
        .map(|pick_id| (pick_id, event_key.clone()))
        .collect();
    let scope = vec![event_key.clone()];
    let run_id = compute_and_save(
        store,
        clock_version,
        &scope,
        "hypothesis_working_recompute",
        &locks,
    )?;
    let candidate = store
        .run_solutions(&run_id)?
        .into_iter()
        .find(|candidate| candidate["event_key"] == event_key && candidate["rank"] == 1)
        .ok_or("recomputed event candidate missing")?;
    let candidate_id = candidate["candidate_id"]
        .as_str()
        .ok_or("candidate missing candidate_id")?;
    store.attach_working_candidate(hypothesis_id, candidate_id, clock_version)?;
    store.log_action(
        "recompute_hypothesis",
        &json!({"hypothesis_id":hypothesis_id,"run_id":run_id,"candidate_id":candidate_id,"clock_version":clock_version}),
    )?;
    json_response(json!({"run_id":run_id,"candidate_id":candidate_id}))
}

fn freeze(body: &str, store: &Store) -> Result<(String, &'static str, String), String> {
    let request: serde_json::Value =
        serde_json::from_str(body).map_err(|error| error.to_string())?;
    let candidate_id = request["candidate_id"]
        .as_str()
        .ok_or("missing candidate_id")?;
    let label = request["label"].as_str().unwrap_or("frozen");
    let hypothesis_id = store.freeze_candidate(candidate_id, label, "web")?;
    json_response(json!({"hypothesis_id":hypothesis_id}))
}

fn duplicate(body: &str, store: &Store) -> Result<(String, &'static str, String), String> {
    let request: serde_json::Value =
        serde_json::from_str(body).map_err(|error| error.to_string())?;
    let hypothesis_id = request["hypothesis_id"]
        .as_str()
        .ok_or("missing hypothesis_id")?;
    let label = request["label"].as_str().unwrap_or("parallel");
    let new_id = store.duplicate_hypothesis(hypothesis_id, label, "web")?;
    json_response(json!({"hypothesis_id":new_id}))
}

fn batches(store: &Store) -> Result<(String, &'static str, String), String> {
    let mut stmt = store.connection.prepare(
        "SELECT batch_id, source_name, content_hash, record_count, status FROM import_batches ORDER BY rowid"
    ).map_err(|e| e.to_string())?;
    let values = stmt
        .query_map([], |row| {
            Ok(json!({
                "batch_id": row.get::<_, String>(0)?,
                "source_name": row.get::<_, String>(1)?,
                "content_hash": row.get::<_, String>(2)?,
                "record_count": row.get::<_, i64>(3)?,
                "status": row.get::<_, String>(4)?
            }))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    json_response(json!(values))
}

fn logs(store: &Store) -> Result<(String, &'static str, String), String> {
    let mut stmt = store
        .connection
        .prepare("SELECT log_id, action, detail_json FROM operation_log ORDER BY rowid")
        .map_err(|e| e.to_string())?;
    let values = stmt.query_map([], |row| {
        let detail: String = row.get(2)?;
        Ok(json!({
            "log_id": row.get::<_, String>(0)?,
            "action": row.get::<_, String>(1)?,
            "detail": serde_json::from_str::<serde_json::Value>(&detail).unwrap_or(json!({"raw":detail}))
        }))
    }).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    json_response(json!(values))
}
