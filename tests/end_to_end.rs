use pairwise_gsb::db::Store;
use pairwise_gsb::model::{project_xy, LayerRecord, Phase, Pick, SolverInput, Station};
use pairwise_gsb::solver::solve;
use std::collections::BTreeMap;

fn model() -> Vec<LayerRecord> {
    vec![
        LayerRecord {
            depth_top_m: 0.0,
            depth_bottom_m: 5000.0,
            vp_m_s: 5500.0,
            pub_vs_m_s: 3180.0,
        },
        LayerRecord {
            depth_top_m: 5000.0,
            depth_bottom_m: 15000.0,
            vp_m_s: 6100.0,
            pub_vs_m_s: 3530.0,
        },
    ]
}

fn station(id: &str, lat: f64, lon: f64, lat0: f64, lon0: f64) -> Station {
    let (x, y) = project_xy(lat, lon, lat0, lon0);
    Station {
        id: id.into(),
        latitude: lat,
        longitude: lon,
        elevation_m: 0.0,
        x,
        y,
    }
}

fn pick(id: &str, station_id: &str, phase: Phase, time: f64) -> Pick {
    Pick {
        id: id.into(),
        related_id: None,
        station_id: station_id.into(),
        phase,
        raw_time: time,
        corrected_time: time,
        uncertainty_s: 0.08,
        confidence: 0.95,
        source: "auto".into(),
        model_version: "vm".into(),
    }
}

fn sufficient_input() -> SolverInput {
    let origin_lat = 35.0;
    let origin_lon = 1.0;
    let coords = [
        ("S1", 35.01, 1.00),
        ("S2", 35.00, 1.02),
        ("S3", 34.99, 1.01),
        ("S4", 34.99, 0.99),
        ("S5", 35.01, 0.99),
    ];
    let stations = coords
        .into_iter()
        .map(|(id, lat, lon)| (id.into(), station(id, lat, lon, origin_lat, origin_lon)))
        .collect();
    let mut picks = BTreeMap::new();
    let center = station("center", origin_lat, origin_lon, origin_lat, origin_lon);
    for (id, lat, lon) in coords {
        let st = station(id, lat, lon, origin_lat, origin_lon);
        let horizontal = ((st.x - center.x).powi(2) + (st.y - center.y).powi(2)).sqrt();
        let range = (horizontal.powi(2) + 8000.0_f64.powi(2)).sqrt();
        picks.insert(
            format!("{id}-P"),
            pick(&format!("{id}-P"), id, Phase::P, range / 6100.0),
        );
        picks.insert(
            format!("{id}-S"),
            pick(&format!("{id}-S"), id, Phase::S, range / 3530.0),
        );
    }
    let mut models = BTreeMap::new();
    models.insert("vm".to_string(), model());
    SolverInput {
        stations,
        models,
        picks,
        corrections: BTreeMap::new(),
        locked: BTreeMap::new(),
    }
}

#[test]
fn deterministic_solution_keeps_three_candidates() {
    let input = sufficient_input();
    let first = serde_json::to_string(&solve(&input, "v1").unwrap()).unwrap();
    let second = serde_json::to_string(&solve(&input, "v1").unwrap()).unwrap();
    assert_eq!(first, second);
    let result = solve(&input, "v1").unwrap();
    assert_eq!(result.events.len(), 1);
    assert!(result.events[0].candidates.len() >= 3);
    assert_eq!(result.events[0].candidates[0].rank, 1);
    assert!((result.events[0].candidates[0].depth_m.unwrap() - 8000.0).abs() < 500.0);
}

#[test]
fn rejects_too_few_stations_without_inventing_location() {
    let mut input = sufficient_input();
    for id in ["S3", "S4", "S5"] {
        input.stations.remove(id);
        input.picks.remove(&format!("{id}-P"));
        input.picks.remove(&format!("{id}-S"));
    }
    let error = solve(&input, "v1").unwrap_err();
    assert!(error.contains("at least three stations"));
}

#[test]
fn database_import_is_content_hash_idempotent_and_replays_sets() {
    let path = std::env::temp_dir().join(format!("gsb-e2e-{}.sqlite", std::process::id()));
    let store = Store::open(path.to_str().unwrap()).unwrap();
    let bytes = include_bytes!("../data/sample.jsonl");
    let first = store.import_jsonl(bytes).unwrap();
    assert!(!first.duplicate);
    let second = store.import_jsonl(bytes).unwrap();
    assert!(second.duplicate);
    store.ensure_correction_v1().unwrap();
    store.ensure_hypotheses().unwrap();
    let (_set_one, result_one) = store.recompute("H-A", Some("v1"), None).unwrap();
    let (_set_two, result_two) = store.recompute("H-A", Some("v1"), None).unwrap();
    assert_eq!(
        serde_json::to_string(&result_one).unwrap(),
        serde_json::to_string(&result_two).unwrap()
    );
}
