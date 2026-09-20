use crate::linalg::{inverse_symmetric, solve_normal};
use crate::model::{parse_epoch, Phase, Pick, PickObservation, PickStatus, Station, VelocityModel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const GRID_STEP_KM: f64 = 0.5;
const MAX_ITERATIONS: usize = 10;
const ACCEPTANCE_SIGMA: f64 = 4.0;
const SEED_GAP_SECONDS: f64 = 30.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidatePick {
    pub pick_id: String,
    pub station_id: String,
    pub phase: Phase,
    pub observed_time: String,
    pub predicted_time: String,
    pub residual_seconds: f64,
    pub sigma_seconds: f64,
    pub confidence: f64,
    pub contribution: f64,
    pub locked: bool,
    pub accepted: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockEstimate {
    pub station_id: String,
    pub estimated_residual_seconds: f64,
    pub sigma_seconds: Option<f64>,
    pub reference: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub candidate_id: String,
    pub event_key: String,
    pub rank: usize,
    pub score: f64,
    pub status: String,
    pub x_km: Option<f64>,
    pub y_km: Option<f64>,
    pub depth_km: f64,
    pub origin_time: Option<String>,
    pub station_count: usize,
    pub p_count: usize,
    pub s_count: usize,
    pub azimuth_gap_deg: Option<f64>,
    pub rms_residual_seconds: Option<f64>,
    pub warnings: Vec<String>,
    pub covariance_labels: Vec<String>,
    pub covariance: Vec<Vec<f64>>,
    pub clock_estimates: Vec<ClockEstimate>,
    pub accepted_picks: Vec<CandidatePick>,
    pub rejected_picks: Vec<CandidatePick>,
}

#[derive(Debug, Clone)]
pub struct SolveContext<'a> {
    pub stations: &'a [Station],
    pub model: &'a VelocityModel,
    pub picks: &'a [Pick],
    pub segments: &'a [(String, f64, f64, f64)],
    pub locks: &'a BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct Blueprint {
    event_key: String,
    seed_time: f64,
    omitted_station: Option<String>,
}

#[derive(Debug, Clone)]
struct Solution {
    x: f64,
    y: f64,
    origin: f64,
    clocks: Vec<f64>,
    chi2: f64,
    covariance: Vec<f64>,
    variance_factor: f64,
    independent_chi2: f64,
}

#[derive(Debug, Clone)]
struct Fitted {
    blueprint: Blueprint,
    observations: Vec<PickObservation>,
    solution: Option<Solution>,
    warnings: Vec<String>,
    accepted: Vec<CandidatePick>,
    rejected: Vec<CandidatePick>,
    score: f64,
}

fn station_index(stations: &[Station], station_id: &str) -> Option<usize> {
    stations
        .iter()
        .position(|station| station.station_id == station_id)
}

fn corrected_epoch(pick: &Pick, context: &SolveContext<'_>, station_index: usize) -> f64 {
    let observed = parse_epoch(&pick.time).unwrap_or(f64::NAN);
    let station_id = &context.stations[station_index].station_id;
    let mut bias = 0.0;
    for (candidate_station, start, end, segment_bias) in context.segments {
        if candidate_station != station_id || observed < *start {
            continue;
        }
        if end.is_finite() && observed >= *end {
            continue;
        }
        bias = *segment_bias;
    }
    observed - bias
}

fn station_bias(station_id: &str, observed: f64, context: &SolveContext<'_>) -> f64 {
    let mut bias = 0.0;
    for (candidate_station, start, end, segment_bias) in context.segments {
        if candidate_station != station_id || observed < *start {
            continue;
        }
        if end.is_finite() && observed >= *end {
            continue;
        }
        bias = *segment_bias;
    }
    bias
}

fn make_observation(
    pick: &Pick,
    index: usize,
    context: &SolveContext<'_>,
) -> Option<PickObservation> {
    let station = station_index(context.stations, &pick.station_id)?;
    Some(PickObservation {
        pick_index: index,
        station_index: station,
        phase: pick.phase,
        observed_epoch: corrected_epoch(pick, context, station),
        sigma_seconds: pick.sigma_seconds.max(0.001),
        confidence: pick.confidence,
    })
}

fn phase_order_bad(picks: &[Pick]) -> BTreeMap<String, bool> {
    let mut p_times: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for pick in picks {
        if pick.status != PickStatus::Noise && pick.phase == Phase::P {
            if let Ok(time) = parse_epoch(&pick.time) {
                p_times
                    .entry(pick.station_id.clone())
                    .or_default()
                    .push(time);
            }
        }
    }
    for times in p_times.values_mut() {
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }
    picks
        .iter()
        .filter(|pick| pick.phase == Phase::S && pick.status != PickStatus::Noise)
        .filter_map(|pick| {
            let time = parse_epoch(&pick.time).ok()?;
            let nearby_p = p_times
                .get(&pick.station_id)?
                .iter()
                .any(|p_time| (time - p_time).abs() <= 15.0 && time < *p_time);
            Some((pick.pick_id.clone(), nearby_p))
        })
        .collect()
}

fn cluster_centers(observations: &[PickObservation], picks: &[Pick]) -> Vec<f64> {
    let mut p: Vec<(f64, usize)> = observations
        .iter()
        .filter(|obs| {
            picks[obs.pick_index].phase == Phase::P
                && picks[obs.pick_index].status != PickStatus::Noise
        })
        .map(|obs| (obs.observed_epoch, obs.station_index))
        .collect();
    p.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut groups: Vec<Vec<(f64, usize)>> = Vec::new();
    for item in p {
        if let Some(group) = groups.last_mut() {
            if item.0 - group.last().unwrap().0 <= SEED_GAP_SECONDS {
                group.push(item);
                continue;
            }
        }
        groups.push(vec![item]);
    }
    groups
        .into_iter()
        .filter(|group| {
            let mut stations = group
                .iter()
                .map(|(_, station_index)| *station_index)
                .collect::<Vec<_>>();
            stations.sort_unstable();
            stations.dedup();
            stations.len() >= 2
        })
        .filter_map(|group| group.first().map(|(time, _)| *time))
        .collect()
}

pub fn solve_network(context: SolveContext<'_>) -> Vec<Candidate> {
    let bad_order = phase_order_bad(context.picks);
    let observations: Vec<PickObservation> = context
        .picks
        .iter()
        .enumerate()
        .filter_map(|(index, pick)| make_observation(pick, index, &context))
        .collect();
    let centers = cluster_centers(&observations, context.picks);
    let mut blueprints = Vec::new();
    for (cluster, seed_time) in centers.iter().enumerate() {
        let stations_in_cluster = station_set_for_cluster(&observations, context.picks, *seed_time);
        let base = Blueprint {
            event_key: format!("event-{:03}", cluster + 1),
            seed_time: *seed_time,
            omitted_station: None,
        };
        blueprints.push(base);
        for station_id in stations_in_cluster {
            blueprints.push(Blueprint {
                event_key: format!("event-{:03}", cluster + 1),
                seed_time: *seed_time,
                omitted_station: Some(station_id),
            });
        }
    }
    let mut fitted: Vec<Fitted> = blueprints
        .iter()
        .map(|blueprint| fit_independent(blueprint, &observations, &bad_order, &context))
        .collect();
    refine_shared_clocks(&mut fitted, &context);
    let mut candidates: Vec<Candidate> = fitted
        .into_iter()
        .map(|fitted| into_candidate(fitted, &context, &bad_order))
        .collect();
    finalize_ranking(&mut candidates);
    candidates
}

fn station_set_for_cluster(
    observations: &[PickObservation],
    picks: &[Pick],
    seed_time: f64,
) -> Vec<String> {
    let mut ids: Vec<String> = observations
        .iter()
        .filter(|obs| {
            let pick = &picks[obs.pick_index];
            pick.status != PickStatus::Noise
                && pick.phase == Phase::P
                && (obs.observed_epoch - seed_time).abs() <= SEED_GAP_SECONDS
        })
        .map(|obs| picks[obs.pick_index].station_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn assigned_to_blueprint(
    obs: &PickObservation,
    blueprint: &Blueprint,
    context: &SolveContext<'_>,
) -> bool {
    let pick = &context.picks[obs.pick_index];
    if pick.status == PickStatus::Noise {
        return false;
    }
    if let Some(event_key) = context.locks.get(&pick.pick_id) {
        return event_key == &blueprint.event_key;
    }
    if blueprint
        .omitted_station
        .as_ref()
        .is_some_and(|station_id| station_id == &pick.station_id)
    {
        return false;
    }
    let dt = obs.observed_epoch - blueprint.seed_time;
    match pick.phase {
        Phase::P => dt >= -SEED_GAP_SECONDS && dt <= SEED_GAP_SECONDS,
        Phase::S => dt >= 0.0 && dt <= 15.0,
    }
}

fn grid_initial(
    obs: &[PickObservation],
    context: &SolveContext<'_>,
    station_ids: &[usize],
) -> (f64, f64, f64) {
    let model = context.model;
    let mut best = (f64::INFINITY, model.x_min_km, model.y_min_km, f64::INFINITY);
    let mut x = model.x_min_km;
    while x <= model.x_max_km + 1e-9 {
        let mut y = model.y_min_km;
        while y <= model.y_max_km + 1e-9 {
            let origins: Vec<f64> = obs
                .iter()
                .map(|obs| {
                    let station = &context.stations[obs.station_index];
                    let distance = ((x - station.x_km).powi(2) + (y - station.y_km).powi(2)).sqrt();
                    obs.observed_epoch - distance / obs.phase.velocity(model)
                })
                .collect();
            let mut sorted_origins = origins;
            sorted_origins.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let origin = sorted_origins[sorted_origins.len() / 2];
            let chi2 = obs
                .iter()
                .map(|obs| {
                    let station = &context.stations[obs.station_index];
                    let distance = ((x - station.x_km).powi(2) + (y - station.y_km).powi(2)).sqrt();
                    let residual =
                        obs.observed_epoch - origin - distance / obs.phase.velocity(model);
                    (residual / obs.sigma_seconds).powi(2)
                })
                .sum::<f64>();
            if chi2 < best.0 {
                best = (chi2, x, y, origin);
            }
            y += GRID_STEP_KM;
        }
        x += GRID_STEP_KM;
    }
    let _ = station_ids;
    (best.1, best.2, best.3)
}

fn travel_residual(
    obs: &PickObservation,
    x: f64,
    y: f64,
    origin: f64,
    clock: f64,
    model: &VelocityModel,
    stations: &[Station],
) -> f64 {
    let station = &stations[obs.station_index];
    let distance = ((x - station.x_km).powi(2) + (y - station.y_km).powi(2)).sqrt();
    obs.observed_epoch - origin - distance / obs.phase.velocity(model) - clock
}

fn solve_event(
    obs: &[PickObservation],
    initial: (f64, f64, f64),
    fixed_clocks: Option<&[f64]>,
    context: &SolveContext<'_>,
) -> Option<Solution> {
    let mut station_ids: Vec<usize> = obs.iter().map(|obs| obs.station_index).collect();
    station_ids.sort_unstable();
    station_ids.dedup();
    let clock_count = if fixed_clocks.is_some() {
        0
    } else {
        station_ids.len().saturating_sub(1)
    };
    let n = 3 + clock_count;
    let mut params = vec![initial.0, initial.1, initial.2];
    params.extend(vec![0.0; clock_count]);
    let mut last_chi2 = f64::INFINITY;
    for _ in 0..MAX_ITERATIONS {
        let mut normal = vec![0.0; n * n];
        let mut rhs = vec![0.0; n];
        let mut chi2 = 0.0;
        for obs in obs {
            let clock_slot = station_ids
                .iter()
                .position(|id| *id == obs.station_index)
                .unwrap();
            let clock_value = match fixed_clocks {
                Some(values) => values[clock_slot],
                None if clock_slot == 0 => 0.0,
                None => params[3 + clock_slot - 1],
            };
            let station = &context.stations[obs.station_index];
            let dx = params[0] - station.x_km;
            let dy = params[1] - station.y_km;
            let distance = (dx * dx + dy * dy).sqrt().max(0.01);
            let velocity = obs.phase.velocity(context.model);
            let residual = travel_residual(
                obs,
                params[0],
                params[1],
                params[2],
                clock_value,
                context.model,
                context.stations,
            );
            let weight = obs.sigma_seconds.powi(-2);
            let mut derivatives = vec![
                -dx / (velocity * distance),
                -dy / (velocity * distance),
                -1.0,
            ];
            if fixed_clocks.is_none() {
                let mut clock_derivatives = vec![0.0; clock_count];
                if clock_slot > 0 {
                    clock_derivatives[clock_slot - 1] = -1.0;
                }
                derivatives.extend(clock_derivatives);
            }
            for i in 0..n {
                rhs[i] -= weight * derivatives[i] * residual;
                for j in 0..n {
                    normal[i * n + j] += weight * derivatives[i] * derivatives[j];
                }
            }
            chi2 += weight * residual * residual;
        }
        let delta = solve_normal(&normal, &rhs, n)?;
        let mut trial = params.clone();
        let mut accepted_step = false;
        for scale in [1.0_f64, 0.5, 0.25, 0.125] {
            for index in 0..n {
                trial[index] = params[index] + scale * delta[index];
            }
            let trial_chi2: f64 = obs
                .iter()
                .map(|obs| {
                    let slot = station_ids
                        .iter()
                        .position(|id| *id == obs.station_index)
                        .unwrap();
                    let clock_value = match fixed_clocks {
                        Some(values) => values[slot],
                        None if slot == 0 => 0.0,
                        None => trial[3 + slot - 1],
                    };
                    let residual = travel_residual(
                        obs,
                        trial[0],
                        trial[1],
                        trial[2],
                        clock_value,
                        context.model,
                        context.stations,
                    );
                    (residual / obs.sigma_seconds).powi(2)
                })
                .sum();
            if trial_chi2 < chi2 || trial_chi2 < last_chi2 {
                params = trial;
                last_chi2 = trial_chi2;
                accepted_step = true;
                break;
            }
        }
        if !accepted_step {
            break;
        }
    }
    let covariance = inverse_symmetric(
        &normal_from_obs(obs, &params, &station_ids, fixed_clocks, context)?,
        n,
    )?;
    let variance_factor = if obs.len() > n {
        last_chi2 / (obs.len() - n) as f64
    } else {
        1.0
    };
    let mut clocks = vec![0.0; station_ids.len()];
    if let Some(values) = fixed_clocks {
        clocks.copy_from_slice(values);
    } else {
        for (slot, value) in clocks.iter_mut().enumerate().skip(1) {
            *value = params[2 + slot];
        }
    }
    Some(Solution {
        x: params[0],
        y: params[1],
        origin: params[2],
        clocks,
        chi2: last_chi2,
        covariance,
        variance_factor,
        independent_chi2: last_chi2,
    })
}

fn normal_from_obs(
    obs: &[PickObservation],
    params: &[f64],
    station_ids: &[usize],
    fixed_clocks: Option<&[f64]>,
    context: &SolveContext<'_>,
) -> Option<Vec<f64>> {
    let n = 3 + if fixed_clocks.is_some() {
        0
    } else {
        station_ids.len().saturating_sub(1)
    };
    let mut normal = vec![0.0; n * n];
    for obs in obs {
        let slot = station_ids.iter().position(|id| *id == obs.station_index)?;
        let station = &context.stations[obs.station_index];
        let dx = params[0] - station.x_km;
        let dy = params[1] - station.y_km;
        let distance = (dx * dx + dy * dy).sqrt().max(0.01);
        let velocity = obs.phase.velocity(context.model);
        let weight = obs.sigma_seconds.powi(-2);
        let mut derivatives = vec![
            -dx / (velocity * distance),
            -dy / (velocity * distance),
            -1.0,
        ];
        if fixed_clocks.is_none() {
            derivatives.extend(vec![0.0; station_ids.len().saturating_sub(1)]);
            if slot > 0 {
                derivatives[2 + slot] = -1.0;
            }
        }
        for i in 0..n {
            for j in 0..n {
                normal[i * n + j] += weight * derivatives[i] * derivatives[j];
            }
        }
    }
    Some(normal)
}

fn make_candidate_pick(
    obs: &PickObservation,
    accepted: bool,
    reason: Option<String>,
    solution: &Solution,
    context: &SolveContext<'_>,
    slot: usize,
) -> CandidatePick {
    let pick = &context.picks[obs.pick_index];
    let station = &context.stations[obs.station_index];
    let distance =
        ((solution.x - station.x_km).powi(2) + (solution.y - station.y_km).powi(2)).sqrt();
    let corrected_prediction =
        solution.origin + distance / obs.phase.velocity(context.model) + solution.clocks[slot];
    let residual = obs.observed_epoch - corrected_prediction;
    let bias = station_bias(&pick.station_id, obs.observed_epoch, context);
    CandidatePick {
        pick_id: pick.pick_id.clone(),
        station_id: pick.station_id.clone(),
        phase: pick.phase,
        observed_time: pick.time.clone(),
        predicted_time: crate::model::format_epoch(corrected_prediction + bias),
        residual_seconds: round_milli(residual),
        sigma_seconds: obs.sigma_seconds,
        confidence: obs.confidence,
        contribution: round_milli((residual / obs.sigma_seconds).powi(2)),
        locked: pick.status == PickStatus::Locked,
        accepted,
        reason,
    }
}

fn round_milli(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn fit_independent(
    blueprint: &Blueprint,
    observations: &[PickObservation],
    bad_order: &BTreeMap<String, bool>,
    context: &SolveContext<'_>,
) -> Fitted {
    let mut warnings = Vec::new();
    let mut assigned = Vec::new();
    let mut rejected: Vec<CandidatePick> = Vec::new();
    for obs in observations {
        let pick = &context.picks[obs.pick_index];
        if pick.status == PickStatus::Noise {
            rejected.push(empty_pick(
                *obs,
                Some("marked_as_noise".to_string()),
                context,
            ));
        } else if bad_order.get(&pick.pick_id).copied().unwrap_or(false) {
            rejected.push(empty_pick(
                *obs,
                Some("s_arrival_before_station_p".to_string()),
                context,
            ));
        } else if !assigned_to_blueprint(obs, blueprint, context) {
            let reason = if context
                .locks
                .get(&pick.pick_id)
                .is_some_and(|event| event != &blueprint.event_key)
            {
                "locked_to_other_candidate"
            } else if blueprint.omitted_station.as_deref() == Some(pick.station_id.as_str()) {
                "omitted_by_alternative_hypothesis"
            } else {
                "outside_association_window"
            };
            rejected.push(empty_pick(*obs, Some(reason.to_string()), context));
        } else {
            assigned.push(*obs);
        }
    }
    let mut station_ids: Vec<usize> = assigned.iter().map(|obs| obs.station_index).collect();
    station_ids.sort_unstable();
    station_ids.dedup();
    let p_count = assigned
        .iter()
        .filter(|obs| context.picks[obs.pick_index].phase == Phase::P)
        .count();
    if station_ids.len() < 3 {
        warnings.push("fewer_than_three_stations".to_string());
    }
    if p_count < 3 {
        warnings.push("fewer_than_three_p_arrivals".to_string());
    }
    if station_ids.len() < 3 || p_count < 3 {
        return Fitted {
            blueprint: blueprint.clone(),
            observations: assigned,
            solution: None,
            warnings,
            accepted: Vec::new(),
            rejected,
            score: 1e300,
        };
    }
    let initial = grid_initial(&assigned, context, &station_ids);
    let mut solution = match solve_event(&assigned, initial, None, context) {
        Some(solution) => solution,
        None => {
            warnings.push("rank_deficient_or_non_positive_definite_hessian".to_string());
            return Fitted {
                blueprint: blueprint.clone(),
                observations: assigned,
                solution: None,
                warnings,
                accepted: Vec::new(),
                rejected,
                score: 1e300,
            };
        }
    };
    let mut accepted_ids = classify_accepted(&assigned, &solution, &station_ids, context);
    if accepted_ids.len() != assigned.len() {
        let trimmed: Vec<PickObservation> = assigned
            .iter()
            .copied()
            .filter(|obs| {
                accepted_ids.contains(&obs.pick_index)
                    || context.picks[obs.pick_index].status == PickStatus::Locked
            })
            .collect();
        let mut trimmed_stations: Vec<usize> =
            trimmed.iter().map(|obs| obs.station_index).collect();
        trimmed_stations.sort_unstable();
        trimmed_stations.dedup();
        if trimmed_stations.len() >= 3 && trimmed.len() >= 4 {
            if let Some(refit) = solve_event(
                &trimmed,
                (solution.x, solution.y, solution.origin),
                None,
                context,
            ) {
                solution = refit;
            }
        }
        accepted_ids = classify_accepted(&trimmed, &solution, &trimmed_stations, context);
        for obs in &assigned {
            if !accepted_ids.contains(&obs.pick_index) {
                rejected.push(empty_pick(
                    *obs,
                    Some("arrival_residual_gt_4sigma".to_string()),
                    context,
                ));
            }
        }
        assigned = trimmed;
        station_ids = trimmed_stations;
    }
    if assigned.len() <= 3 + station_ids.len().saturating_sub(1) {
        warnings.push("no_positive_degrees_of_freedom".to_string());
    }
    if !context.model.contains(solution.x, solution.y) {
        warnings.push("solution_outside_velocity_model".to_string());
    }
    let accepted = build_accepted(&assigned, &solution, &station_ids, context);
    let rms = rms_residual(&accepted);
    let residual_rejections = rejected
        .iter()
        .filter(|pick| pick.reason.as_deref() == Some("arrival_residual_gt_4sigma"))
        .count();
    let score = score_candidate(
        &solution,
        accepted.len(),
        residual_rejections,
        blueprint,
        rms,
    );
    Fitted {
        blueprint: blueprint.clone(),
        observations: assigned,
        solution: Some(solution),
        warnings,
        accepted,
        rejected,
        score,
    }
}

fn empty_pick(
    obs: PickObservation,
    reason: Option<String>,
    context: &SolveContext<'_>,
) -> CandidatePick {
    let pick = &context.picks[obs.pick_index];
    CandidatePick {
        pick_id: pick.pick_id.clone(),
        station_id: pick.station_id.clone(),
        phase: pick.phase,
        observed_time: pick.time.clone(),
        predicted_time: "not_estimated".to_string(),
        residual_seconds: f64::NAN,
        sigma_seconds: obs.sigma_seconds,
        confidence: obs.confidence,
        contribution: f64::NAN,
        locked: pick.status == PickStatus::Locked,
        accepted: false,
        reason,
    }
}

fn classify_accepted(
    obs: &[PickObservation],
    solution: &Solution,
    station_ids: &[usize],
    context: &SolveContext<'_>,
) -> Vec<usize> {
    obs.iter()
        .copied()
        .filter(|obs| {
            let pick = &context.picks[obs.pick_index];
            let slot = station_ids
                .iter()
                .position(|id| *id == obs.station_index)
                .unwrap();
            let residual = travel_residual(
                obs,
                solution.x,
                solution.y,
                solution.origin,
                solution.clocks[slot],
                context.model,
                context.stations,
            );
            residual.abs() <= ACCEPTANCE_SIGMA * obs.sigma_seconds
                || pick.status == PickStatus::Locked
        })
        .map(|obs| obs.pick_index)
        .collect()
}

fn build_accepted(
    obs: &[PickObservation],
    solution: &Solution,
    station_ids: &[usize],
    context: &SolveContext<'_>,
) -> Vec<CandidatePick> {
    let mut result: Vec<CandidatePick> = obs
        .iter()
        .map(|obs| {
            let slot = station_ids
                .iter()
                .position(|id| *id == obs.station_index)
                .unwrap();
            make_candidate_pick(obs, true, None, solution, context, slot)
        })
        .collect();
    result.sort_by(|a, b| a.pick_id.cmp(&b.pick_id));
    result
}

fn rms_residual(picks: &[CandidatePick]) -> f64 {
    if picks.is_empty() {
        return f64::NAN;
    }
    (picks
        .iter()
        .map(|pick| pick.residual_seconds.powi(2))
        .sum::<f64>()
        / picks.len() as f64)
        .sqrt()
}

fn score_candidate(
    solution: &Solution,
    accepted: usize,
    rejected: usize,
    blueprint: &Blueprint,
    rms: f64,
) -> f64 {
    let omission_penalty = if blueprint.omitted_station.is_some() {
        100.0
    } else {
        0.0
    };
    solution.chi2 + rejected as f64 * 50.0 + omission_penalty + accepted as f64 * rms.abs() * 0.001
}

fn refine_shared_clocks(fitted: &mut [Fitted], context: &SolveContext<'_>) {
    let event_count = fitted
        .iter()
        .map(|item| item.blueprint.event_key.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if event_count < 2 {
        for item in fitted.iter_mut() {
            item.warnings
                .push("shared_clock_bias_not_identifiable_single_event".to_string());
        }
        return;
    }
    let mut primary = BTreeMap::new();
    for (index, item) in fitted.iter().enumerate() {
        if item.solution.is_some() {
            primary
                .entry(item.blueprint.event_key.clone())
                .and_modify(|existing: &mut usize| {
                    if item.score < fitted[*existing].score {
                        *existing = index;
                    }
                })
                .or_insert(index);
        }
    }
    if primary.len() < 2 {
        for item in fitted.iter_mut() {
            item.warnings
                .push("shared_clock_bias_not_identifiable".to_string());
        }
        return;
    }
    let primary_indices: Vec<usize> = primary.values().copied().collect();
    let mut global_stations: Vec<String> = context
        .stations
        .iter()
        .map(|s| s.station_id.clone())
        .collect();
    let mut occurrence: BTreeMap<String, usize> = BTreeMap::new();
    for index in &primary_indices {
        let mut ids: Vec<String> = fitted[*index]
            .observations
            .iter()
            .map(|obs| context.picks[obs.pick_index].station_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        for id in ids {
            *occurrence.entry(id).or_default() += 1;
        }
    }
    global_stations.retain(|id| occurrence.get(id).copied().unwrap_or(0) >= 2);
    global_stations.sort();
    if global_stations.len() < 2 {
        for item in fitted.iter_mut() {
            item.warnings
                .push("shared_clock_bias_not_identifiable".to_string());
        }
        return;
    }
    let reference = global_stations[0].clone();
    let clock_count = global_stations.len() - 1;
    let n = primary_indices.len() * 3 + clock_count;
    let mut params = vec![0.0; n];
    for (event_slot, &fitted_index) in primary_indices.iter().enumerate() {
        let solution = fitted[fitted_index].solution.as_ref().unwrap();
        params[event_slot * 3] = solution.x;
        params[event_slot * 3 + 1] = solution.y;
        params[event_slot * 3 + 2] = solution.origin;
    }
    let independent_total_chi2: f64 = primary_indices
        .iter()
        .map(|index| fitted[*index].solution.as_ref().unwrap().independent_chi2)
        .sum();
    let mut last_chi2 = f64::INFINITY;
    let mut covariance = Vec::new();
    for _ in 0..MAX_ITERATIONS {
        let mut normal = vec![0.0; n * n];
        let mut rhs = vec![0.0; n];
        let mut chi2 = 0.0;
        for (event_slot, &fitted_index) in primary_indices.iter().enumerate() {
            for obs in &fitted[fitted_index].observations {
                let station_id = context.picks[obs.pick_index].station_id.clone();
                let Some(clock_slot) = global_stations.iter().position(|id| id == &station_id)
                else {
                    continue;
                };
                let station = &context.stations[obs.station_index];
                let dx = params[event_slot * 3] - station.x_km;
                let dy = params[event_slot * 3 + 1] - station.y_km;
                let distance = (dx * dx + dy * dy).sqrt().max(0.01);
                let velocity = obs.phase.velocity(context.model);
                let clock_value = if station_id == reference {
                    0.0
                } else {
                    params[primary_indices.len() * 3 + clock_slot - 1]
                };
                let residual = obs.observed_epoch
                    - params[event_slot * 3 + 2]
                    - distance / velocity
                    - clock_value;
                let weight = obs.sigma_seconds.powi(-2);
                let mut derivatives = vec![0.0; n];
                derivatives[event_slot * 3] = -dx / (velocity * distance);
                derivatives[event_slot * 3 + 1] = -dy / (velocity * distance);
                derivatives[event_slot * 3 + 2] = -1.0;
                if station_id != reference {
                    derivatives[primary_indices.len() * 3 + clock_slot - 1] = -1.0;
                }
                for i in 0..n {
                    rhs[i] -= weight * derivatives[i] * residual;
                    for j in 0..n {
                        normal[i * n + j] += weight * derivatives[i] * derivatives[j];
                    }
                }
                chi2 += weight * residual * residual;
            }
        }
        let Some(delta) = solve_normal(&normal, &rhs, n) else {
            for item in fitted.iter_mut() {
                item.warnings
                    .push("shared_clock_joint_hessian_rank_deficient".to_string());
            }
            return;
        };
        covariance = inverse_symmetric(&normal, n).unwrap_or_default();
        if (last_chi2 - chi2).abs() < 1e-9 {
            last_chi2 = chi2;
            break;
        }
        for (parameter, delta) in params.iter_mut().zip(delta) {
            *parameter += delta;
        }
        last_chi2 = chi2;
    }
    let clock_values: Vec<f64> = std::iter::once(0.0)
        .chain(params[primary_indices.len() * 3..].iter().copied())
        .collect();
    if last_chi2 >= independent_total_chi2 {
        for item in fitted.iter_mut() {
            item.warnings
                .push("shared_clock_correction_rejected_by_chi_square".to_string());
        }
        return;
    }
    for (event_slot, &fitted_index) in primary_indices.iter().enumerate() {
        let event_key = fitted[fitted_index].blueprint.event_key.clone();
        for item in fitted.iter_mut() {
            if item.blueprint.event_key != event_key || item.solution.is_none() {
                continue;
            }
            let mut station_ids: Vec<usize> = item
                .observations
                .iter()
                .map(|obs| obs.station_index)
                .collect();
            station_ids.sort_unstable();
            station_ids.dedup();
            let clocks = station_ids
                .iter()
                .map(|station_index| {
                    let station_id = &context.stations[*station_index].station_id;
                    global_stations
                        .iter()
                        .position(|id| id == station_id)
                        .map(|slot| clock_values[slot])
                        .unwrap_or(0.0)
                })
                .collect();
            let solution = item.solution.as_mut().unwrap();
            solution.x = params[event_slot * 3];
            solution.y = params[event_slot * 3 + 1];
            solution.origin = params[event_slot * 3 + 2];
            solution.clocks = clocks;
            solution.chi2 = last_chi2;
            if !covariance.is_empty() {
                let start = event_slot * 3;
                let mut event_covariance = Vec::new();
                for row in 0..3 {
                    for col in 0..3 {
                        event_covariance.push(covariance[(start + row) * n + start + col]);
                    }
                }
                solution.covariance = event_covariance;
                solution.variance_factor = 1.0;
            }
            item.warnings
                .push(format!("shared_clock_reference:{reference}"));
        }
    }
}

fn azimuth_gap(
    stations: &[Station],
    observations: &[PickObservation],
    x: f64,
    y: f64,
) -> Option<f64> {
    let mut ids: Vec<usize> = observations.iter().map(|obs| obs.station_index).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() < 2 {
        return None;
    }
    let mut angles: Vec<f64> = ids
        .iter()
        .map(|index| {
            let station = &stations[*index];
            (station.y_km - y)
                .atan2(station.x_km - x)
                .to_degrees()
                .rem_euclid(360.0)
        })
        .collect();
    angles.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut largest = 360.0 - angles[angles.len() - 1] + angles[0];
    for pair in angles.windows(2) {
        largest = largest.max(pair[1] - pair[0]);
    }
    Some((largest * 1000.0).round() / 1000.0)
}

fn into_candidate(
    fitted: Fitted,
    context: &SolveContext<'_>,
    _bad_order: &BTreeMap<String, bool>,
) -> Candidate {
    let Some(solution) = fitted.solution else {
        return Candidate {
            candidate_id: crate::db::stable_id(
                "candidate",
                &serde_json::json!({"event":fitted.blueprint.event_key,"rank":0,"status":"underdetermined","seed":fitted.blueprint.seed_time,"omitted":fitted.blueprint.omitted_station}).to_string(),
            ),
            event_key: fitted.blueprint.event_key,
            rank: 0,
            score: round_milli(fitted.score),
            status: "underdetermined".to_string(),
            x_km: None,
            y_km: None,
            depth_km: 0.0,
            origin_time: None,
            station_count: 0,
            p_count: 0,
            s_count: 0,
            azimuth_gap_deg: None,
            rms_residual_seconds: None,
            warnings: fitted.warnings,
            covariance_labels: Vec::new(),
            covariance: Vec::new(),
            clock_estimates: Vec::new(),
            accepted_picks: fitted.accepted,
            rejected_picks: fitted.rejected,
        };
    };
    let mut station_ids: Vec<usize> = fitted
        .observations
        .iter()
        .map(|obs| obs.station_index)
        .collect();
    station_ids.sort_unstable();
    station_ids.dedup();
    let p_count = fitted
        .accepted
        .iter()
        .filter(|pick| pick.phase == Phase::P)
        .count();
    let s_count = fitted.accepted.len() - p_count;
    let mut warnings = fitted.warnings;
    let gap = azimuth_gap(
        context.stations,
        &fitted.observations,
        solution.x,
        solution.y,
    );
    if gap.unwrap_or(0.0) > 180.0 {
        warnings.push("azimuthal_gap_greater_than_180_degrees".to_string());
    }
    let boundary = (solution.x - context.model.x_min_km).abs() < 0.30
        || (solution.x - context.model.x_max_km).abs() < 0.30
        || (solution.y - context.model.y_min_km).abs() < 0.30
        || (solution.y - context.model.y_max_km).abs() < 0.30;
    if boundary {
        warnings.push("solution_on_velocity_model_boundary".to_string());
    }
    let rms = round_milli(rms_residual(&fitted.accepted));
    let covariance_labels = vec![
        "x_km".to_string(),
        "y_km".to_string(),
        "origin_seconds".to_string(),
    ];
    let full_dimension = (solution.covariance.len() as f64).sqrt() as usize;
    let covariance: Vec<Vec<f64>> = (0..3)
        .map(|row| {
            (0..3)
                .map(|column| {
                    let value = solution.covariance[row * full_dimension + column];
                    (value * 1.0e9).round() / 1.0e9
                })
                .collect()
        })
        .collect();
    let status = if warnings.iter().any(|warning| {
        warning.contains("out_of")
            || warning.contains("boundary")
            || warning.contains("rank")
            || warning.contains("fewer")
            || warning.contains("not_identifiable")
            || warning.contains("azimuthal_gap")
            || warning.contains("no_positive_degrees")
    }) || covariance.len() != 3
    {
        "conditional"
    } else {
        "reliable"
    };
    let clock_estimates = station_ids
        .iter()
        .enumerate()
        .map(|(slot, index)| {
            let station_id = context.stations[*index].station_id.clone();
            let full_dimension = (solution.covariance.len() as f64).sqrt() as usize;
            let clock_parameter = 3 + slot.saturating_sub(1);
            let sigma_seconds = if slot > 0 && full_dimension > clock_parameter {
                let variance =
                    solution.covariance[clock_parameter * full_dimension + clock_parameter];
                (variance > 0.0).then(|| (variance.sqrt() * 1000.0).round() / 1000.0)
            } else {
                None
            };
            ClockEstimate {
                station_id,
                estimated_residual_seconds: round_milli(solution.clocks[slot]),
                sigma_seconds,
                reference: slot == 0,
            }
        })
        .collect();
    Candidate {
        candidate_id: crate::db::stable_id(
            "candidate",
            &serde_json::json!({
                "event": fitted.blueprint.event_key,
                "seed": fitted.blueprint.seed_time,
                "omitted": fitted.blueprint.omitted_station,
                "x": solution.x,
                "y": solution.y,
                "origin": solution.origin,
                "clocks": solution.clocks
            })
            .to_string(),
        ),
        event_key: fitted.blueprint.event_key,
        rank: 0,
        score: round_milli(fitted.score),
        status: status.to_string(),
        x_km: Some((solution.x * 1000.0).round() / 1000.0),
        y_km: Some((solution.y * 1000.0).round() / 1000.0),
        depth_km: 0.0,
        origin_time: Some(crate::model::format_epoch(solution.origin)),
        station_count: station_ids.len(),
        p_count,
        s_count,
        azimuth_gap_deg: gap,
        rms_residual_seconds: Some(rms),
        warnings,
        covariance_labels,
        covariance,
        clock_estimates,
        accepted_picks: fitted.accepted,
        rejected_picks: fitted.rejected,
    }
}

fn finalize_ranking(candidates: &mut Vec<Candidate>) {
    candidates.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.event_key.cmp(&b.event_key))
            .then_with(|| {
                a.x_km
                    .unwrap_or(0.0)
                    .partial_cmp(&b.x_km.unwrap_or(0.0))
                    .unwrap()
            })
            .then_with(|| {
                a.y_km
                    .unwrap_or(0.0)
                    .partial_cmp(&b.y_km.unwrap_or(0.0))
                    .unwrap()
            })
    });
    let mut ranks: BTreeMap<String, usize> = BTreeMap::new();
    for candidate in candidates.iter_mut() {
        let rank = ranks.entry(candidate.event_key.clone()).or_insert(1);
        candidate.rank = *rank;
        *rank += 1;
    }
    candidates.retain(|candidate| candidate.rank <= 3);
}
