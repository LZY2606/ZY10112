mod linalg;
mod solver_types;

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{LayerRecord, Phase, SolverInput};
pub use solver_types::*;

pub const TOLERANCE_S: f64 = 1e-9;
const SP_TRAVEL_GAP_MIN_S: f64 = 0.5;
const RESIDUAL_HARD_S: f64 = 4.0;
const RESIDUAL_SOFT_S: f64 = 2.0;

fn velocity(layers: &[LayerRecord], depth: f64, phase: Phase) -> Result<f64, String> {
    let top = layers.first().map(|l| l.depth_top_m).unwrap_or(0.0);
    let bottom = layers.last().map(|l| l.depth_bottom_m).unwrap_or(0.0);
    if depth + 1.0 < top || depth - 1.0 > bottom {
        return Err(format!(
            "source depth {depth:.0} m is outside velocity model [{top:.0}, {bottom:.0}] m"
        ));
    }
    let layer = layers
        .iter()
        .find(|layer| depth <= layer.depth_bottom_m + 1.0)
        .unwrap_or_else(|| layers.last().expect("checked model"));
    let span = (layer.depth_bottom_m - layer.depth_top_m).max(1.0);
    let fraction = ((depth - layer.depth_top_m) / span).clamp(0.0, 1.0);
    Ok(match phase {
        Phase::P => layer.vp_m_s + fraction * 0.0,
        Phase::S => layer.pub_vs_m_s + fraction * 0.0,
    })
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::P => "P",
        Phase::S => "S",
    }
}

fn effective_pick_ids(input: &SolverInput) -> Vec<String> {
    let mut superseded: BTreeSet<String> = input
        .picks
        .values()
        .filter(|pick| pick.source == "manual")
        .filter_map(|pick| pick.related_id.clone())
        .collect();
    let mut ids: Vec<String> = input
        .picks
        .keys()
        .filter(|id| !superseded.remove(*id))
        .cloned()
        .collect();
    ids.sort();
    ids
}

fn connected_time_same_station(earlier: f64, later: f64) -> bool {
    later > earlier + SP_TRAVEL_GAP_MIN_S - TOLERANCE_S && later < earlier + 60.0 + TOLERANCE_S
}

fn cluster_picks(input: &SolverInput, ids: &[String]) -> Vec<Vec<String>> {
    let mut parent: BTreeMap<String, String> =
        ids.iter().map(|id| (id.clone(), id.clone())).collect();
    fn find(parent: &mut BTreeMap<String, String>, id: &str) -> String {
        let mut current = id.to_string();
        while parent[&current] != current {
            let next = parent[&current].clone();
            parent.insert(current.clone(), parent[&next].clone());
            current = next;
        }
        current
    }
    for (i, id_a) in ids.iter().enumerate() {
        let a = &input.picks[id_a];
        for id_b in &ids[i + 1..] {
            let b = &input.picks[id_b];
            let same_station = a.station_id == b.station_id;
            let station_pair_allowed = !same_station
                || (a.phase != b.phase
                    && connected_time_same_station(
                        a.corrected_time.min(b.corrected_time),
                        a.corrected_time.max(b.corrected_time),
                    ));
            let global_gap = (a.corrected_time - b.corrected_time).abs();
            if station_pair_allowed && global_gap <= 12.0 + TOLERANCE_S {
                let root_a = find(&mut parent, id_a);
                let root_b = find(&mut parent, id_b);
                if root_a != root_b {
                    let (smaller, larger) = if root_a <= root_b {
                        (root_a.clone(), root_b.clone())
                    } else {
                        (root_b.clone(), root_a.clone())
                    };
                    parent.insert(larger, smaller);
                }
            }
        }
    }
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in ids {
        let root = find(&mut parent, id);
        groups.entry(root).or_default().push(id.clone());
    }
    let mut result: Vec<Vec<String>> = groups
        .into_values()
        .map(|mut group| {
            group.sort();
            group
        })
        .collect();
    result.sort_by(|a, b| {
        input
            .picks
            .get(&a[0])
            .unwrap()
            .corrected_time
            .total_cmp(&input.picks.get(&b[0]).unwrap().corrected_time)
            .then(a[0].cmp(&b[0]))
    });
    result
}

fn station_phase_precheck(input: &SolverInput, ids: &[String]) -> (Vec<String>, Vec<RejectedPick>) {
    let mut by_station: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for id in ids {
        by_station
            .entry(input.picks[id].station_id.clone())
            .or_default()
            .push(id);
    }
    let mut kept: BTreeSet<String> = ids.iter().cloned().collect();
    let mut rejected = Vec::new();
    for (station_id, station_ids) in by_station {
        let mut ps: Vec<(f64, Phase, String)> = station_ids
            .iter()
            .map(|id| {
                let pick = &input.picks[*id];
                (pick.corrected_time, pick.phase, (*id).clone())
            })
            .collect();
        ps.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)));
        for pair in ps.windows(2) {
            let (time_a, phase_a, id_a) = &pair[0];
            let (time_b, phase_b, id_b) = &pair[1];
            let impossible_order = matches!((phase_a, phase_b), (Phase::S, Phase::P))
                && (time_b - time_a).abs() < SP_TRAVEL_GAP_MIN_S;
            let impossible_gap =
                phase_a == phase_b && (time_b - time_a).abs() < SP_TRAVEL_GAP_MIN_S;
            let reversed_gap = matches!((phase_a, phase_b), (Phase::P, Phase::S))
                && *time_b < *time_a + SP_TRAVEL_GAP_MIN_S - TOLERANCE_S;
            if impossible_order || impossible_gap || reversed_gap {
                for id in [id_a, id_b] {
                    if kept.remove(id) {
                        let pick = &input.picks[id.as_str()];
                        rejected.push(RejectedPick {
                            observation_id: id.clone(),
                            station_id: station_id.clone(),
                            phase: phase_name(pick.phase).to_string(),
                            reason: "impossible P/S ordering or duplicate-phase interval at this station".to_string(),
                        });
                    }
                }
            }
        }
    }
    let mut result: Vec<String> = ids
        .iter()
        .filter(|id| kept.contains(*id))
        .cloned()
        .collect();
    result.sort();
    (result, rejected)
}

fn make_window(input: &SolverInput, index: usize, ids: &[String]) -> EventWindow {
    let mut sum = 0.0;
    let mut min_time = f64::INFINITY;
    let mut max_time = f64::NEG_INFINITY;
    for id in ids {
        let pick = &input.picks[id.as_str()];
        sum += pick.corrected_time;
        min_time = min_time.min(pick.corrected_time);
        max_time = max_time.max(pick.corrected_time);
    }
    EventWindow {
        key: format!("event-{:02}", index + 1),
        label: format!("E{}", index + 1),
        pick_ids: ids.to_vec(),
        min_time,
        max_time,
        centroid_time: sum / ids.len() as f64,
    }
}

pub fn solve(input: &SolverInput, correction_version: &str) -> Result<SolverResultSet, String> {
    if input.stations.len() < 3 {
        return Err(
            "at least three stations are required for a local network solution".to_string(),
        );
    }
    if input.models.is_empty() {
        return Err("at least one velocity model is required".to_string());
    }
    for layers in input.models.values() {
        if layers.is_empty() {
            return Err("velocity model has no layers".to_string());
        }
    }

    let mut rejected = Vec::new();
    let all_ids = effective_pick_ids(input);
    let (checked_ids, mut phase_rejected) = station_phase_precheck(input, &all_ids);
    rejected.append(&mut phase_rejected);
    let rejected_set: BTreeSet<String> = rejected
        .iter()
        .map(|item| item.observation_id.clone())
        .collect();
    let mut clusters = cluster_picks(input, &checked_ids);
    let mut windows = Vec::new();
    for cluster in clusters.drain(..) {
        let stations: BTreeSet<String> = cluster
            .iter()
            .map(|id| input.picks[id].station_id.clone())
            .collect();
        if cluster.len() < 2 || stations.len() < 2 {
            for id in cluster {
                if rejected_set.contains(&id) {
                    continue;
                }
                let pick = &input.picks[id.as_str()];
                rejected.push(RejectedPick {
                    observation_id: id,
                    station_id: pick.station_id.clone(),
                    phase: phase_name(pick.phase).to_string(),
                    reason: "does not form a multi-station event window".to_string(),
                });
            }
        } else {
            windows.push(make_window(input, windows.len(), &cluster));
        }
    }

    windows = assign_locked_picks(input, windows, &mut rejected);
    windows.retain(|window| window.pick_ids.len() >= 2);
    windows.sort_by(|a, b| a.min_time.total_cmp(&b.min_time).then(a.key.cmp(&b.key)));
    for (index, window) in windows.iter_mut().enumerate() {
        window.key = format!("event-{:02}", index + 1);
        window.label = format!("E{}", index + 1);
    }

    let mut events = Vec::new();
    for window in windows {
        events.push(locate_window(
            input,
            &window,
            correction_version,
            &mut rejected,
        )?);
    }
    rejected.sort_by(|a, b| {
        a.observation_id
            .cmp(&b.observation_id)
            .then(a.reason.cmp(&b.reason))
    });
    rejected.dedup_by(|a, b| a.observation_id == b.observation_id && a.reason == b.reason);
    Ok(SolverResultSet { events, rejected })
}
mod locator;
pub use locator::locate_window;

fn assign_locked_picks(
    input: &SolverInput,
    mut windows: Vec<EventWindow>,
    rejected: &mut Vec<RejectedPick>,
) -> Vec<EventWindow> {
    let locked_ids: Vec<String> = input.locked.keys().cloned().collect();
    for pick_id in locked_ids {
        let Some(pick) = input.picks.get(&pick_id) else {
            continue;
        };
        if let Some(window) = windows
            .iter_mut()
            .find(|window| window.pick_ids.contains(&pick_id))
        {
            window.pick_ids.retain(|id| id != &pick_id);
        }
        let anchor = input.locked.get(&pick_id).cloned().unwrap_or_default();
        let target_index = windows
            .iter()
            .position(|window| window.key == anchor)
            .or_else(|| {
                windows
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let distance_a = (a.centroid_time - pick.corrected_time).abs();
                        let distance_b = (b.centroid_time - pick.corrected_time).abs();
                        distance_a.total_cmp(&distance_b).then(a.key.cmp(&b.key))
                    })
                    .map(|(index, _)| index)
            });
        match target_index.and_then(|index| windows.get_mut(index)) {
            Some(window) => {
                if !window.pick_ids.contains(&pick_id) {
                    window.pick_ids.push(pick_id.clone());
                    window.pick_ids.sort();
                    window.min_time = window.min_time.min(pick.corrected_time);
                    window.max_time = window.max_time.max(pick.corrected_time);
                }
            }
            None => rejected.push(RejectedPick {
                observation_id: pick_id.clone(),
                station_id: pick.station_id.clone(),
                phase: phase_name(pick.phase).to_string(),
                reason: format!(
                    "locked anchor event {anchor} does not exist in this candidate set"
                ),
            }),
        }
    }
    windows
}
