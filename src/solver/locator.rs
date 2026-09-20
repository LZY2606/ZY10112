use std::collections::{BTreeMap, BTreeSet};

use super::linalg::{diagonalize, invert_symmetric, solve_symmetric};
use super::solver_types::*;
use super::{phase_name, velocity, RESIDUAL_HARD_S, RESIDUAL_SOFT_S, TOLERANCE_S};
use crate::model::{unproject_xy, LayerRecord, SolverInput};

#[derive(Clone)]
struct Seed {
    x: f64,
    y: f64,
    depth: f64,
}

#[derive(Clone)]
struct Fit {
    x: f64,
    y: f64,
    depth: f64,
    origin_time: f64,
    biases: BTreeMap<String, f64>,
    covariance: CovarianceSummary,
    warnings: BTreeSet<String>,
    assignments: Vec<PickAssignment>,
    score: f64,
    rms: f64,
}

fn reject(pick: &crate::model::Pick, reason: &str) -> RejectedPick {
    RejectedPick {
        observation_id: pick.id.clone(),
        station_id: pick.station_id.clone(),
        phase: phase_name(pick.phase).to_string(),
        reason: reason.to_string(),
    }
}

fn gather(input: &SolverInput, window: &EventWindow) -> (Vec<Observation>, Vec<RejectedPick>) {
    let mut observations = Vec::new();
    let mut rejected = Vec::new();
    for id in &window.pick_ids {
        let pick = input.picks[id].clone();
        let Some(station) = input.stations.get(&pick.station_id).cloned() else {
            rejected.push(reject(&pick, "station record is missing"));
            continue;
        };
        if !input.models.contains_key(&pick.model_version) {
            rejected.push(reject(&pick, "velocity model version is missing"));
            continue;
        }
        observations.push(Observation {
            pick,
            station,
            locked: input.locked.contains_key(id.as_str()),
        });
    }
    observations.sort_by(|a, b| a.pick.id.cmp(&b.pick.id));
    (observations, rejected)
}

fn station_list(observations: &[Observation]) -> Vec<String> {
    let stations: BTreeSet<String> = observations
        .iter()
        .map(|o| o.pick.station_id.clone())
        .collect();
    stations.into_iter().collect()
}

fn distance(x: f64, y: f64, depth: f64, station: &crate::model::Station) -> f64 {
    ((x - station.x).powi(2) + (y - station.y).powi(2) + depth.powi(2)).sqrt()
}

fn model_layers<'a>(input: &'a SolverInput, observations: &[Observation]) -> &'a [LayerRecord] {
    let version = observations
        .first()
        .map(|o| o.pick.model_version.as_str())
        .unwrap_or_default();
    input
        .models
        .get(version)
        .map(|layers| layers.as_slice())
        .unwrap_or(&[])
}

fn predicted(
    observation: &Observation,
    layers: &[LayerRecord],
    x: f64,
    y: f64,
    depth: f64,
    origin_time: f64,
    biases: &BTreeMap<String, f64>,
) -> Result<(f64, f64), String> {
    let range = distance(x, y, depth, &observation.station);
    let speed = velocity(layers, depth, observation.pick.phase)?;
    let bias = *biases.get(&observation.pick.station_id).unwrap_or(&0.0);
    Ok((origin_time + range / speed + bias, range))
}

fn estimate_origin(input: &SolverInput, observations: &[Observation], seed: &Seed) -> f64 {
    let layers = model_layers(input, observations);
    let mut estimates = Vec::new();
    for observation in observations {
        if let Ok(speed) = velocity(layers, seed.depth, observation.pick.phase) {
            let range = distance(seed.x, seed.y, seed.depth, &observation.station);
            estimates.push(observation.pick.corrected_time - range / speed);
        }
    }
    estimates.sort_by(|a, b| a.total_cmp(b));
    estimates[estimates.len() / 2]
}

fn candidate_seeds(
    input: &SolverInput,
    observations: &[Observation],
    layers: &[LayerRecord],
) -> Vec<Seed> {
    let stations = station_list(observations);
    let max_bottom = layers.last().map(|l| l.depth_bottom_m).unwrap_or(1000.0);
    let min_bottom = layers
        .iter()
        .map(|l| l.depth_bottom_m)
        .min_by(|a, b| a.total_cmp(b))
        .unwrap_or(1000.0);
    let mut seeds = Vec::new();
    for station_id in &stations {
        let station = &input.stations[station_id];
        let depths = if min_bottom >= 5000.0 {
            vec![max_bottom * 0.25, max_bottom * 0.75]
        } else {
            vec![500.0_f64.max(min_bottom * 0.5), max_bottom * 0.75]
        };
        for depth in depths {
            seeds.push(Seed {
                x: station.x,
                y: station.y,
                depth,
            });
        }
    }
    let centroid_x =
        observations.iter().map(|o| o.station.x).sum::<f64>() / observations.len() as f64;
    let centroid_y =
        observations.iter().map(|o| o.station.y).sum::<f64>() / observations.len() as f64;
    let spread_x = observations
        .iter()
        .map(|o| (o.station.x - centroid_x).abs())
        .fold(1000.0, f64::max);
    let spread_y = observations
        .iter()
        .map(|o| (o.station.y - centroid_y).abs())
        .fold(1000.0, f64::max);
    let offsets = [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)];
    for (index, (sx, sy)) in offsets.into_iter().enumerate() {
        let depth = if index % 2 == 0 {
            max_bottom * 0.35
        } else {
            max_bottom * 0.85
        };
        seeds.push(Seed {
            x: centroid_x + sx * spread_x * 0.25,
            y: centroid_y + sy * spread_y * 0.25,
            depth,
        });
    }
    let mut chosen = Vec::new();
    for seed in seeds {
        if !seed.x.is_finite()
            || !seed.y.is_finite()
            || seed.depth < 0.0
            || seed.depth > max_bottom + 1.0
        {
            continue;
        }
        let distinct = chosen.iter().all(|existing: &Seed| {
            let dx = existing.x - seed.x;
            let dy = existing.y - seed.y;
            let dz = existing.depth - seed.depth;
            (dx * dx + dy * dy).sqrt() > 800.0 || dz.abs() > 800.0
        });
        if distinct {
            chosen.push(seed);
        }
        if chosen.len() >= 8 {
            break;
        }
    }
    chosen
}

fn fit_seed(input: &SolverInput, observations: &[Observation], seed: Seed) -> Result<Fit, String> {
    let layers = model_layers(input, observations);
    let max_bottom = layers.last().map(|l| l.depth_bottom_m).unwrap_or(1000.0);
    let stations = station_list(observations);
    let anchor = stations
        .first()
        .cloned()
        .ok_or_else(|| "empty event".to_string())?;
    let parameter_names: Vec<String> = {
        let mut names = vec![
            "x_m".to_string(),
            "y_m".to_string(),
            "depth_m".to_string(),
            "origin_time".to_string(),
        ];
        names.extend(
            stations
                .iter()
                .filter(|id| **id != anchor)
                .map(|id| format!("bias:{id}")),
        );
        names
    };
    let n = parameter_names.len();
    let mut params = vec![
        seed.x,
        seed.y,
        seed.depth,
        estimate_origin(input, observations, &seed),
    ];
    params.extend(std::iter::repeat(0.0).take(n - 4));

    let mut x = params[0];
    let mut y = params[1];
    let mut depth = params[2];
    let mut origin_time = params[3];
    let mut biases: BTreeMap<String, f64> = BTreeMap::new();
    biases.insert(anchor.clone(), 0.0);
    for (station_id, value) in stations
        .iter()
        .filter(|id| **id != anchor)
        .zip(params.iter().skip(4).copied())
    {
        biases.insert(station_id.clone(), value);
    }

    let mut objective = f64::INFINITY;
    for _ in 0..80 {
        let mut rows: Vec<(Vec<f64>, f64, f64)> = Vec::new();
        for observation in observations {
            let obs_layers = input
                .models
                .get(&observation.pick.model_version)
                .map(|v| v.as_slice())
                .unwrap_or(layers);
            let (predicted_time, range) =
                predicted(observation, obs_layers, x, y, depth, origin_time, &biases)?;
            let residual = observation.pick.corrected_time - predicted_time;
            let mut jacobian = vec![0.0; n];
            let speed = velocity(obs_layers, depth, observation.pick.phase)?;
            jacobian[0] = (observation.station.x - x) / (range * speed);
            jacobian[1] = (observation.station.y - y) / (range * speed);
            jacobian[2] = -depth / range / speed;
            jacobian[3] = -1.0;
            if observation.pick.station_id != anchor {
                let index = 4 + stations
                    .iter()
                    .filter(|id| **id != anchor)
                    .position(|id| *id == observation.pick.station_id)
                    .unwrap();
                jacobian[index] = -1.0;
            }
            let sigma = (observation.pick.uncertainty_s.powi(2) + 0.02_f64.powi(2)).sqrt();
            let weight = (0.05 + 0.95 * observation.pick.confidence) / sigma.powi(2);
            rows.push((jacobian, residual, weight));
        }
        rows.push((
            {
                let mut j = vec![0.0; n];
                j[2] = 1.0;
                j
            },
            8000.0 - depth,
            1.0 / 30000.0_f64.powi(2),
        ));
        for index in 4..n {
            let value = params.get(index).copied().unwrap_or(0.0);
            let mut j = vec![0.0; n];
            j[index] = 1.0;
            rows.push((j, -value, 1.0 / 2.0_f64.powi(2)));
        }
        let calculate = |px: f64,
                         py: f64,
                         pz: f64,
                         pt: f64,
                         pb: &BTreeMap<String, f64>|
         -> Result<f64, String> {
            let mut total = 0.0;
            for observation in observations {
                let obs_layers = input
                    .models
                    .get(&observation.pick.model_version)
                    .map(|v| v.as_slice())
                    .unwrap_or(layers);
                let (predicted_time, _) = predicted(observation, obs_layers, px, py, pz, pt, pb)?;
                let residual = observation.pick.corrected_time - predicted_time;
                let sigma = (observation.pick.uncertainty_s.powi(2) + 0.02_f64.powi(2)).sqrt();
                total += (0.05 + 0.95 * observation.pick.confidence) * (residual / sigma).powi(2);
            }
            Ok(total
                + ((pz - 8000.0) / 30000.0).powi(2)
                + pb.values().map(|v| (v / 2.0).powi(2)).sum::<f64>())
        };
        let mut best = (objective, params.clone());
        for damping in [1e-7, 1e-6, 1e-5, 1e-4, 0.001, 0.01, 0.1, 1.0, 10.0, 100.0] {
            let mut normal = vec![0.0; n * n];
            let mut rhs = vec![0.0; n];
            for (jacobian, residual, weight) in &rows {
                for i in 0..n {
                    rhs[i] -= weight * jacobian[i] * residual;
                    for j in 0..n {
                        normal[i * n + j] += weight * jacobian[i] * jacobian[j];
                    }
                }
            }
            for i in 0..n {
                normal[i * n + i] += damping * normal[i * n + i].abs().max(1e-12);
            }
            let Some(delta) = solve_symmetric(&normal, &rhs, n) else {
                continue;
            };
            let mut trial_params: Vec<f64> = params
                .iter()
                .zip(delta)
                .map(|(value, step)| value + step)
                .collect();
            trial_params[2] = trial_params[2].clamp(0.0, max_bottom);
            let max_horizontal_move = 200_000.0;
            trial_params[0] =
                x + (trial_params[0] - x).clamp(-max_horizontal_move, max_horizontal_move);
            trial_params[1] =
                y + (trial_params[1] - y).clamp(-max_horizontal_move, max_horizontal_move);
            let mut trial_biases = biases.clone();
            for (station_id, value) in stations
                .iter()
                .filter(|id| **id != anchor)
                .zip(trial_params.iter().skip(4))
            {
                trial_biases.insert(station_id.clone(), *value);
            }
            let Ok(value) = calculate(
                trial_params[0],
                trial_params[1],
                trial_params[2],
                trial_params[3],
                &trial_biases,
            ) else {
                continue;
            };
            if value < best.0 {
                best = (value, trial_params);
            }
        }
        if (best.0 - objective).abs() < 1e-11 {
            break;
        }
        objective = best.0;
        params = best.1;
        x = params[0];
        y = params[1];
        depth = params[2];
        origin_time = params[3];
        for (station_id, value) in stations
            .iter()
            .filter(|id| **id != anchor)
            .zip(params.iter().skip(4))
        {
            biases.insert(station_id.clone(), *value);
        }
    }

    let covariance = covariance_summary(
        input,
        observations,
        layers,
        &stations,
        &anchor,
        x,
        y,
        depth,
        origin_time,
        &biases,
        &parameter_names,
        n,
    )?;
    let mut assignments = Vec::new();
    let mut warnings = covariance_warnings(&covariance, &stations, &anchor);
    let mut supported = 0usize;
    let mut squared = 0.0;
    let mut contributions = Vec::new();
    for observation in observations {
        let obs_layers = input
            .models
            .get(&observation.pick.model_version)
            .map(|v| v.as_slice())
            .unwrap_or(layers);
        let (predicted_time, _) =
            predicted(observation, obs_layers, x, y, depth, origin_time, &biases)?;
        let residual = observation.pick.corrected_time - predicted_time;
        let sigma = (observation.pick.uncertainty_s.powi(2) + 0.02_f64.powi(2)).sqrt();
        let normalized = (residual / sigma).abs();
        let within_soft = residual.abs() <= RESIDUAL_SOFT_S + TOLERANCE_S;
        let supported_here = observation.locked || within_soft;
        if supported_here {
            supported += 1;
        }
        if residual.abs() > RESIDUAL_HARD_S && !observation.locked {
            warnings.insert(format!(
                "{} has a {:.2} s rejected residual",
                observation.pick.id,
                residual.abs()
            ));
        }
        let assignment_weight = if supported_here {
            (0.05 + 0.95 * observation.pick.confidence) * (-normalized.powi(2) / 8.0).exp()
        } else {
            0.0
        };
        let contribution = assignment_weight * 2.0;
        contributions.push(contribution);
        squared += residual.powi(2);
        assignments.push(PickAssignment {
            observation_id: observation.pick.id.clone(),
            station_id: observation.pick.station_id.clone(),
            phase: phase_name(observation.pick.phase).to_string(),
            corrected_time: observation.pick.corrected_time,
            predicted_time: Some(predicted_time),
            residual_s: Some(residual),
            uncertainty_s: observation.pick.uncertainty_s,
            confidence: observation.pick.confidence,
            weight: assignment_weight,
            score_contribution: contribution,
            locked: observation.locked,
            source: observation.pick.source.clone(),
        });
    }
    if supported < 2 {
        warnings.insert("fewer than two picks support this candidate".to_string());
    }
    let score = if contributions.is_empty() {
        0.0
    } else {
        contributions.iter().sum::<f64>() / contributions.len() as f64
    };
    Ok(Fit {
        x,
        y,
        depth,
        origin_time,
        biases,
        covariance,
        warnings,
        assignments,
        score,
        rms: (squared / observations.len().max(1) as f64).sqrt(),
    })
}

fn covariance_summary(
    input: &SolverInput,
    observations: &[Observation],
    fallback_layers: &[LayerRecord],
    stations: &[String],
    anchor: &str,
    x: f64,
    y: f64,
    depth: f64,
    origin_time: f64,
    biases: &BTreeMap<String, f64>,
    names: &[String],
    n: usize,
) -> Result<CovarianceSummary, String> {
    let mut data = vec![0.0; n * n];
    let mut posterior = vec![0.0; n * n];
    let add_matrix = |matrix: &mut [f64], jacobian: &[f64], weight: f64| {
        for i in 0..n {
            for j in 0..n {
                matrix[i * n + j] += weight * jacobian[i] * jacobian[j];
            }
        }
    };
    for observation in observations {
        let layers = input
            .models
            .get(&observation.pick.model_version)
            .map(|v| v.as_slice())
            .unwrap_or(fallback_layers);
        let (_, range) = predicted(observation, layers, x, y, depth, origin_time, biases)?;
        let speed = velocity(layers, depth, observation.pick.phase)?;
        let mut jacobian = vec![0.0; n];
        jacobian[0] = (observation.station.x - x) / (range * speed);
        jacobian[1] = (observation.station.y - y) / (range * speed);
        jacobian[2] = -depth / range / speed;
        jacobian[3] = -1.0;
        if observation.pick.station_id != anchor {
            let index = 4 + stations
                .iter()
                .filter(|id| **id != anchor)
                .position(|id| *id == observation.pick.station_id)
                .unwrap();
            jacobian[index] = -1.0;
        }
        let sigma = (observation.pick.uncertainty_s.powi(2) + 0.02_f64.powi(2)).sqrt();
        let weight = (0.05 + 0.95 * observation.pick.confidence) / sigma.powi(2);
        add_matrix(&mut data, &jacobian, weight);
        add_matrix(&mut posterior, &jacobian, weight);
    }
    posterior[2 * n + 2] += 1.0 / 30000.0_f64.powi(2);
    for index in 4..n {
        posterior[index * n + index] += 1.0 / 2.0_f64.powi(2);
    }

    let (data_values, _) = diagonalize(data.clone(), n);
    let data_max = data_values
        .iter()
        .copied()
        .fold(0.0_f64, f64::max)
        .max(1e-12);
    let rank = data_values
        .iter()
        .filter(|value| **value > data_max * 1e-9 && **value > 0.0)
        .count();

    let mut regularized = posterior.clone();
    let mut jitter = 1e-10;
    let matrix = loop {
        let (values, _) = diagonalize(regularized.clone(), n);
        let max = values.iter().copied().fold(0.0_f64, f64::max).max(1.0);
        if values.iter().all(|value| *value > max * 1e-12) {
            break regularized;
        }
        if jitter > 1.0 {
            return Err("covariance normal matrix cannot be made positive definite".to_string());
        }
        regularized = posterior.clone();
        for i in 0..n {
            regularized[i * n + i] += jitter * max;
        }
        jitter *= 4.0;
    };

    let inverse = invert_symmetric(&matrix, n)
        .ok_or_else(|| "covariance matrix inversion failed".to_string())?;
    let (mut values, _) = diagonalize(inverse.clone(), n);
    values.sort_by(|a, b| a.total_cmp(b));
    let positive_definite = values.iter().all(|value| *value > 0.0 && value.is_finite());
    let min = values.first().copied().unwrap_or(0.0);
    let max = values.last().copied().unwrap_or(0.0);
    let condition_number = if min > 0.0 && min.is_finite() {
        Some(max / min)
    } else {
        None
    };
    Ok(CovarianceSummary {
        positive_definite,
        rank,
        parameter_count: n,
        min_eigenvalue: min,
        max_eigenvalue: max,
        condition_number,
        source_sigma: SourceSigma {
            x_m: inverse[0].max(0.0).sqrt(),
            y_m: inverse[1 * n + 1].max(0.0).sqrt(),
            depth_m: inverse[2 * n + 2].max(0.0).sqrt(),
            origin_time_s: inverse[3 * n + 3].max(0.0).sqrt(),
        },
        matrix: inverse,
        parameter_names: names.to_vec(),
    })
}

fn covariance_warnings(
    covariance: &CovarianceSummary,
    _stations: &[String],
    _anchor: &str,
) -> BTreeSet<String> {
    let mut warnings = BTreeSet::new();
    if !covariance.positive_definite {
        warnings.insert("posterior covariance is not positive definite".to_string());
    }
    if covariance.rank + 1 < covariance.parameter_count {
        warnings.insert(format!(
            "weighted Jacobian rank {} is below parameter count {}",
            covariance.rank, covariance.parameter_count
        ));
    }
    if covariance.condition_number.unwrap_or(0.0) > 1e10 {
        warnings.insert("covariance condition number exceeds 1e10".to_string());
    }
    let matrix = &covariance.matrix;
    let n = covariance.parameter_count;
    for index in 4..n {
        let denominator = (matrix[3 * n + 3] * matrix[index * n + index]).max(f64::MIN_POSITIVE);
        let correlation = (matrix[3 * n + index] / denominator.sqrt()).abs();
        if correlation > 0.99 {
            warnings.insert(format!(
                "origin time is nearly indistinguishable from {}",
                covariance.parameter_names[index]
            ));
        }
    }
    warnings
}

fn azimuthal_gap(observations: &[Observation], x: f64, y: f64) -> f64 {
    let mut angles: Vec<f64> = observations
        .iter()
        .map(|o| {
            ((o.station.y - y).atan2(o.station.x - x))
                .to_degrees()
                .rem_euclid(360.0)
        })
        .collect();
    angles.sort_by(|a, b| a.total_cmp(b));
    if angles.is_empty() {
        return 360.0;
    }
    let mut gap = angles[0] + 360.0 - *angles.last().unwrap();
    for pair in angles.windows(2) {
        gap = gap.max(pair[1] - pair[0]);
    }
    gap
}

fn underdetermined_candidate(
    rank: usize,
    observations: &[Observation],
    reason: &str,
) -> CandidateResult {
    let names = vec!["origin_time".to_string()];
    CandidateResult {
        rank,
        status: "underdetermined".to_string(),
        score: 0.0,
        latitude: None,
        longitude: None,
        x_m: None,
        y_m: None,
        depth_m: None,
        origin_time: None,
        rms_residual_s: 0.0,
        station_bias_s: observations
            .iter()
            .map(|o| (o.pick.station_id.clone(), 0.0))
            .collect(),
        covariance: CovarianceSummary {
            positive_definite: true,
            rank: 0,
            parameter_count: 1,
            min_eigenvalue: 1.0,
            max_eigenvalue: 1.0,
            condition_number: Some(1.0),
            source_sigma: SourceSigma {
                x_m: 0.0,
                y_m: 0.0,
                depth_m: 0.0,
                origin_time_s: 1.0,
            },
            matrix: vec![1.0],
            parameter_names: names,
        },
        warnings: vec![reason.to_string()],
        assignments: observations
            .iter()
            .map(|o| PickAssignment {
                observation_id: o.pick.id.clone(),
                station_id: o.pick.station_id.clone(),
                phase: phase_name(o.pick.phase).to_string(),
                corrected_time: o.pick.corrected_time,
                predicted_time: None,
                residual_s: None,
                uncertainty_s: o.pick.uncertainty_s,
                confidence: o.pick.confidence,
                weight: 0.0,
                score_contribution: 0.0,
                locked: o.locked,
                source: o.pick.source.clone(),
            })
            .collect(),
    }
}

fn candidate_from_fit(
    input: &SolverInput,
    fit: &Fit,
    rank: usize,
    observations: &[Observation],
) -> CandidateResult {
    let stations: BTreeSet<String> = observations
        .iter()
        .map(|o| o.pick.station_id.clone())
        .collect();
    let station_count = stations.len();
    let support_stations: BTreeSet<String> = fit
        .assignments
        .iter()
        .filter(|a| a.weight > 0.0)
        .map(|a| a.station_id.clone())
        .collect();
    let gap = azimuthal_gap(observations, fit.x, fit.y);
    let mut warnings: Vec<String> = fit.warnings.iter().cloned().collect();
    if gap > 180.0 {
        warnings.push(format!("azimuthal gap {gap:.1}° exceeds 180°"));
    }
    let hard_locked = fit
        .assignments
        .iter()
        .any(|a| a.locked && a.residual_s.unwrap_or(0.0).abs() > RESIDUAL_HARD_S);
    let unsupported = fit
        .assignments
        .iter()
        .any(|a| !a.locked && a.residual_s.unwrap_or(0.0).abs() > RESIDUAL_HARD_S);
    let rank_short = fit.covariance.rank < 6;
    let bias_underidentified = fit.covariance.rank + 1 < fit.covariance.parameter_count;
    let ill_conditioned = fit.covariance.condition_number.unwrap_or(0.0) > 1e10;
    let weak_sigma = fit.covariance.source_sigma.x_m > 10_000.0
        || fit.covariance.source_sigma.y_m > 10_000.0
        || fit.covariance.source_sigma.depth_m > 15_000.0
        || fit.covariance.source_sigma.origin_time_s > 1.0;
    let status = if hard_locked || support_stations.len() < 2 {
        "contradiction"
    } else if station_count < 3 || rank_short || !fit.covariance.positive_definite {
        "underdetermined"
    } else if ill_conditioned || weak_sigma || gap > 180.0 || unsupported || bias_underidentified {
        "degraded"
    } else {
        "located"
    };
    let origin_lat =
        input.stations.values().map(|s| s.latitude).sum::<f64>() / input.stations.len() as f64;
    let origin_lon =
        input.stations.values().map(|s| s.longitude).sum::<f64>() / input.stations.len() as f64;
    let (latitude, longitude) = unproject_xy(fit.x, fit.y, origin_lat, origin_lon);
    CandidateResult {
        rank,
        status: status.to_string(),
        score: fit.score,
        latitude: Some(latitude),
        longitude: Some(longitude),
        x_m: Some(fit.x),
        y_m: Some(fit.y),
        depth_m: Some(fit.depth),
        origin_time: Some(fit.origin_time),
        rms_residual_s: fit.rms,
        station_bias_s: fit.biases.clone(),
        covariance: fit.covariance.clone(),
        warnings,
        assignments: fit.assignments.clone(),
    }
}

pub fn locate_window(
    input: &SolverInput,
    window: &EventWindow,
    correction_version: &str,
    rejected: &mut Vec<RejectedPick>,
) -> Result<EventResult, String> {
    let (observations, mut local_rejected) = gather(input, window);
    rejected.append(&mut local_rejected);
    let stations: BTreeSet<String> = observations
        .iter()
        .map(|o| o.pick.station_id.clone())
        .collect();
    let mut phase_counts = BTreeMap::new();
    for observation in &observations {
        *phase_counts
            .entry(observation.phase_name().to_string())
            .or_insert(0usize) += 1;
    }

    let mut candidates = Vec::new();
    if observations.len() < 2 {
        candidates = (1..=3)
            .map(|rank| {
                underdetermined_candidate(rank, &observations, "fewer than two usable observations")
            })
            .collect();
    } else if stations.len() < 3 {
        candidates = (1..=3)
            .map(|rank| {
                underdetermined_candidate(
                    rank,
                    &observations,
                    "fewer than three stations; source and clock terms are not separable",
                )
            })
            .collect();
    } else {
        let layers = model_layers(input, &observations);
        let mut fits: Vec<Fit> = Vec::new();
        for seed in candidate_seeds(input, &observations, layers) {
            match fit_seed(input, &observations, seed) {
                Ok(fit) => fits.push(fit),
                Err(message) => {
                    for observation in &observations {
                        rejected.push(reject(&observation.pick, &message));
                    }
                    return Err(message);
                }
            }
        }
        let status_order = |status: &str| match status {
            "located" => 0,
            "degraded" => 1,
            "underdetermined" => 2,
            "contradiction" => 3,
            _ => 4,
        };
        fits.sort_by(|a, b| {
            let temporary_a = candidate_from_fit(input, a, 0, &observations);
            let temporary_b = candidate_from_fit(input, b, 0, &observations);
            status_order(&temporary_a.status)
                .cmp(&status_order(&temporary_b.status))
                .then(b.score.total_cmp(&a.score))
                .then(a.rms.total_cmp(&b.rms))
                .then(a.x.total_cmp(&b.x))
                .then(a.y.total_cmp(&b.y))
                .then(a.depth.total_cmp(&b.depth))
                .then(a.origin_time.total_cmp(&b.origin_time))
        });
        for (index, fit) in fits.iter().take(3).enumerate() {
            candidates.push(candidate_from_fit(input, fit, index + 1, &observations));
        }
        while candidates.len() < 3 {
            candidates.push(underdetermined_candidate(
                candidates.len() + 1,
                &observations,
                "velocity model bounds produced fewer than three distinct local solutions",
            ));
        }
    }

    for (rank, candidate) in candidates.iter_mut().enumerate() {
        candidate.rank = rank + 1;
        candidate.warnings.sort();
        candidate.warnings.dedup();
        candidate
            .assignments
            .sort_by(|a, b| a.observation_id.cmp(&b.observation_id));
        let _ = correction_version;
    }

    let supported: BTreeSet<String> = candidates
        .iter()
        .flat_map(|candidate| {
            candidate
                .assignments
                .iter()
                .filter(|assignment| assignment.weight > 0.0)
                .map(|assignment| assignment.observation_id.clone())
        })
        .collect();
    for observation in &observations {
        if !supported.contains(&observation.pick.id) {
            rejected.push(reject(
                &observation.pick,
                "arrival residual exceeds every candidate's support threshold",
            ));
        }
    }

    let gap = candidates
        .first()
        .and_then(|c| c.x_m.zip(c.y_m))
        .map(|(x, y)| azimuthal_gap(&observations, x, y))
        .unwrap_or(360.0);
    Ok(EventResult {
        event_key: window.key.clone(),
        label: window.label.clone(),
        station_count: stations.len(),
        pick_count: observations.len(),
        phase_counts,
        azimuthal_gap_deg: gap,
        candidates,
    })
}
