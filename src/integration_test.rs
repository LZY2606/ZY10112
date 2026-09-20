use crate::db::{normalize_lines, parse_records};
use crate::model::{InputRecord, Phase, PickStatus};
use crate::solver::{solve_network, SolveContext};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn demo_data_forms_three_deterministic_events() {
    let content = std::fs::read_to_string("data/demo.jsonl").unwrap();
    let records = parse_records(&normalize_lines(&content)).unwrap();
    let mut stations = Vec::new();
    let mut model = None;
    let mut segments = Vec::new();
    let mut picks = Vec::new();
    for record in records {
        match record {
            InputRecord::Station(station) => stations.push(station),
            InputRecord::VelocityModel(value) => model = Some(value),
            InputRecord::ClockCorrection(value) => {
                let start = crate::model::parse_epoch(&value.start_time).unwrap();
                let end = value
                    .end_time
                    .as_ref()
                    .map(|v| crate::model::parse_epoch(v).unwrap())
                    .unwrap_or(f64::INFINITY);
                segments.push((value.station_id, start, end, value.bias_seconds));
            }
            InputRecord::Pick(mut pick) => {
                pick.status = if pick.confidence < 0.4 {
                    PickStatus::Noise
                } else {
                    PickStatus::Active
                };
                picks.push(pick);
            }
        }
    }
    let model = model.unwrap();
    let locks = BTreeMap::new();
    let context = SolveContext {
        stations: &stations,
        model: &model,
        picks: &picks,
        segments: &segments,
        locks: &locks,
    };
    let candidates = solve_network(context);
    let events: BTreeSet<String> = candidates.iter().map(|c| c.event_key.clone()).collect();
    assert_eq!(
        events,
        BTreeSet::from([
            "event-001".to_string(),
            "event-002".to_string(),
            "event-003".to_string()
        ])
    );
    for event in events {
        let ranks: Vec<usize> = candidates
            .iter()
            .filter(|c| c.event_key == event)
            .map(|c| c.rank)
            .collect();
        assert_eq!(ranks, vec![1, 2, 3]);
    }
    let first = candidates
        .iter()
        .find(|c| c.event_key == "event-001" && c.rank == 1)
        .unwrap();
    assert!((first.x_km.unwrap() - 2.0).abs() < 0.1);
    assert!(first.y_km.unwrap() < -4.0);
    let second = candidates
        .iter()
        .find(|c| c.event_key == "event-002" && c.rank == 1)
        .unwrap();
    assert!((second.x_km.unwrap() + 6.0).abs() < 0.1);
    assert!((second.y_km.unwrap() + 4.0).abs() < 0.1);
    let third = candidates
        .iter()
        .find(|c| c.event_key == "event-003" && c.rank == 1)
        .unwrap();
    assert!((third.x_km.unwrap() - 5.0).abs() < 0.1);
    assert!((third.y_km.unwrap() + 7.0).abs() < 0.1);
    for index in 0..3 {
        assert!(first.covariance[index][index] > 0.0);
        for other in 0..3 {
            assert!((first.covariance[index][other] - first.covariance[other][index]).abs() < 1e-9);
        }
    }
    let rerun = solve_network(SolveContext {
        stations: &stations,
        model: &model,
        picks: &picks,
        segments: &segments,
        locks: &locks,
    });
    assert_eq!(
        serde_json::to_string(&candidates).unwrap(),
        serde_json::to_string(&rerun).unwrap()
    );
}

#[test]
fn two_station_network_is_reported_without_precise_coordinates() {
    let stations = vec![
        crate::model::Station {
            station_id: "S1".into(),
            x_km: 0.0,
            y_km: 0.0,
            elevation_km: 0.0,
        },
        crate::model::Station {
            station_id: "S2".into(),
            x_km: 10.0,
            y_km: 0.0,
            elevation_km: 0.0,
        },
    ];
    let model = crate::model::VelocityModel {
        model_id: "m".into(),
        vp_kms: 6.0,
        vs_kms: 3.5,
        x_min_km: -20.0,
        x_max_km: 20.0,
        y_min_km: -20.0,
        y_max_km: 20.0,
    };
    let picks = vec![
        pick("S1-P", "S1", Phase::P, 1.0),
        pick("S2-P", "S2", Phase::P, 2.0),
        pick("S1-S", "S1", Phase::S, 2.0),
    ];
    let segments = vec![];
    let locks = BTreeMap::new();
    let candidates = solve_network(SolveContext {
        stations: &stations,
        model: &model,
        picks: &picks,
        segments: &segments,
        locks: &locks,
    });
    assert!(candidates.iter().all(|candidate| candidate.x_km.is_none()));
    assert!(candidates
        .iter()
        .flat_map(|candidate| candidate.warnings.clone())
        .any(|warning| warning == "fewer_than_three_stations"));
}

#[test]
fn locked_pick_survives_recomputation_and_noise_can_be_excluded() {
    let content = std::fs::read_to_string("data/demo.jsonl").unwrap();
    let (stations, model, segments, mut picks) = parse_demo(content);
    let mut locks = BTreeMap::new();
    locks.insert("NOISE-1".to_string(), "event-001".to_string());
    for pick in &mut picks {
        if pick.pick_id == "NOISE-1" {
            pick.status = PickStatus::Locked;
        }
    }
    let candidates = solve_network(SolveContext {
        stations: &stations,
        model: &model,
        picks: &picks,
        segments: &segments,
        locks: &locks,
    });
    let event = candidates
        .iter()
        .find(|c| c.event_key == "event-001" && c.rank == 1)
        .unwrap();
    assert!(event
        .accepted_picks
        .iter()
        .any(|pick| pick.pick_id == "NOISE-1" && pick.locked));
    for pick in &mut picks {
        if pick.pick_id == "NOISE-1" {
            pick.status = PickStatus::Noise;
        }
    }
    let candidates = solve_network(SolveContext {
        stations: &stations,
        model: &model,
        picks: &picks,
        segments: &segments,
        locks: &BTreeMap::new(),
    });
    let event = candidates
        .iter()
        .find(|c| c.event_key == "event-001" && c.rank == 1)
        .unwrap();
    assert!(
        event
            .rejected_picks
            .iter()
            .any(|pick| pick.pick_id == "NOISE-1"
                && pick.reason.as_deref() == Some("marked_as_noise"))
    );
}

#[test]
fn duplicate_jsonl_import_is_content_addressable_and_does_not_accumulate() {
    let path = std::env::temp_dir().join(format!(
        "pairwise-gsb-idempotent-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = crate::db::Store::open(path.to_str().unwrap()).unwrap();
    let content = std::fs::read_to_string("data/demo.jsonl").unwrap();
    let first = store.import_jsonl("demo", &content).unwrap();
    let second = store.import_jsonl("demo-again", &content).unwrap();
    assert!(!first.duplicated);
    assert!(second.duplicated);
    assert_eq!(first.batch_id, second.batch_id);
    assert_eq!(store.picks().unwrap().len(), 26);
}

fn parse_demo(
    content: String,
) -> (
    Vec<crate::model::Station>,
    crate::model::VelocityModel,
    Vec<(String, f64, f64, f64)>,
    Vec<crate::model::Pick>,
) {
    let mut stations = Vec::new();
    let mut model = None;
    let mut segments = Vec::new();
    let mut picks = Vec::new();
    for record in crate::db::parse_records(&crate::db::normalize_lines(&content)).unwrap() {
        match record {
            InputRecord::Station(value) => stations.push(value),
            InputRecord::VelocityModel(value) => model = Some(value),
            InputRecord::ClockCorrection(value) => {
                let start = crate::model::parse_epoch(&value.start_time).unwrap();
                let end = value
                    .end_time
                    .as_ref()
                    .map(|v| crate::model::parse_epoch(v).unwrap())
                    .unwrap_or(f64::INFINITY);
                segments.push((value.station_id, start, end, value.bias_seconds));
            }
            InputRecord::Pick(value) => picks.push(value),
        }
    }
    (stations, model.unwrap(), segments, picks)
}

fn pick(id: &str, station: &str, phase: Phase, epoch: f64) -> crate::model::Pick {
    crate::model::Pick {
        pick_id: id.into(),
        station_id: station.into(),
        phase,
        time: crate::model::format_epoch(epoch),
        sigma_seconds: 0.04,
        confidence: 0.9,
        source: crate::model::PickSource::Auto,
        model_id: "m".into(),
        status: PickStatus::Active,
    }
}

#[test]
fn normal_matrix_inverse_has_positive_diagonal() {
    let normal = vec![4.0, 1.0, 0.0, 1.0, 3.0, 1.0, 0.0, 1.0, 2.0];
    let inverse = crate::linalg::inverse_symmetric(&normal, 3).unwrap();
    let mut product = vec![0.0; 9];
    for row in 0..3 {
        for col in 0..3 {
            for k in 0..3 {
                product[row * 3 + col] += normal[row * 3 + k] * inverse[k * 3 + col];
            }
        }
    }
    for index in 0..3 {
        assert!(inverse[index * 3 + index] > 0.0);
        assert!((product[index * 3 + index] - 1.0).abs() < 1e-10);
        for off in 0..3 {
            if off != index {
                assert!(product[index * 3 + off].abs() < 1e-10);
            }
        }
    }
}

#[test]
fn timestamp_formatting_never_emits_leap_second_style_minute_overflow() {
    assert_eq!(
        crate::model::format_epoch(-0.0006),
        "1969-12-31T23:59:59.999Z"
    );
    assert_eq!(
        crate::model::format_epoch(86_399.9996),
        "1970-01-02T00:00:00.000Z"
    );
    assert_eq!(
        crate::model::parse_epoch("1970-01-01T00:00:01.500Z").unwrap(),
        1.5
    );
}

#[test]
fn parallel_hypothesis_recompute_keeps_frozen_parent_snapshot() {
    let path = std::env::temp_dir().join(format!(
        "pairwise-gsb-hypothesis-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::db::Store::open(path.to_str().unwrap()).unwrap();
    let content = std::fs::read_to_string("data/demo.jsonl").unwrap();
    store.import_jsonl("demo", &content).unwrap();
    let run_v1 =
        crate::service::compute_and_save(&store, "clock-v1", &[], "test", &BTreeMap::new())
            .unwrap();
    let candidate_v1 = store
        .run_solutions(&run_v1)
        .unwrap()
        .into_iter()
        .find(|c| c["event_key"] == "event-002" && c["rank"] == 1)
        .unwrap();
    let candidate_id_v1 = candidate_v1["candidate_id"].as_str().unwrap();
    let frozen = store
        .freeze_candidate(candidate_id_v1, "baseline", "test")
        .unwrap();
    let branch = store
        .duplicate_hypothesis(&frozen, "parallel", "test")
        .unwrap();
    let (event, picks, version) = store.recompute_hypothesis_context(&branch).unwrap();
    assert_eq!(event, "event-002");
    assert!(picks.contains(&"B-E-P".to_string()));
    assert_eq!(version, "clock-v1");
    let lock_map = picks
        .into_iter()
        .map(|pick_id| (pick_id, event.clone()))
        .collect();
    let run_v2 =
        crate::service::compute_and_save(&store, "clock-v2", &[event.clone()], "test", &lock_map)
            .unwrap();
    let candidate_v2 = store
        .run_solutions(&run_v2)
        .unwrap()
        .into_iter()
        .find(|c| c["event_key"] == event && c["rank"] == 1)
        .unwrap();
    let candidate_id_v2 = candidate_v2["candidate_id"].as_str().unwrap();
    store
        .attach_working_candidate(&branch, candidate_id_v2, "clock-v2")
        .unwrap();
    let frozen_candidate = hypothesis_column(&store, &frozen, "frozen_candidate_id");
    let branch_candidate = hypothesis_column(&store, &branch, "parent_candidate_id");
    let branch_version = hypothesis_column(&store, &branch, "active_clock_version");
    assert_eq!(frozen_candidate, candidate_id_v1);
    assert_eq!(branch_candidate, candidate_id_v2);
    assert_eq!(branch_version, "clock-v2");
}

fn hypothesis_column(store: &crate::db::Store, hypothesis_id: &str, column: &str) -> String {
    let sql = format!("SELECT {column} FROM event_hypotheses WHERE hypothesis_id = ?1");
    store
        .connection
        .query_row(&sql, rusqlite::params![hypothesis_id], |row| row.get(0))
        .unwrap()
}

#[test]
fn conflicting_station_record_rejects_transaction() {
    let path = std::env::temp_dir().join(format!(
        "pairwise-gsb-conflict-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::db::Store::open(path.to_str().unwrap()).unwrap();
    let first = r#"{"type":"station","station_id":"SAME","x_km":0.0,"y_km":0.0,"elevation_km":0.0}
{"type":"velocity_model","model_id":"m","vp_kms":6.0,"vs_kms":3.5,"x_min_km":-20.0,"x_max_km":20.0,"y_min_km":-20.0,"y_max_km":20.0}
"#;
    let conflicting = format!(
        "{first}{}",
        r#"{"type":"station","station_id":"SAME","x_km":1.0,"y_km":0.0,"elevation_km":0.0}
"#
    );
    store.import_jsonl("first", first).unwrap();
    assert!(store.import_jsonl("conflict", &conflicting).is_err());
    let x: f64 = store
        .connection
        .query_row(
            "SELECT x_km FROM stations WHERE station_id='SAME'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(x, 0.0);
}
