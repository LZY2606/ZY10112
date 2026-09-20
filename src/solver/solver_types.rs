use serde::Serialize;
use std::collections::BTreeMap;

use crate::model::{Phase, Pick, Station};

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SolverResultSet {
    pub events: Vec<EventResult>,
    pub rejected: Vec<RejectedPick>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct EventResult {
    pub event_key: String,
    pub label: String,
    pub station_count: usize,
    pub pick_count: usize,
    pub phase_counts: BTreeMap<String, usize>,
    pub azimuthal_gap_deg: f64,
    pub candidates: Vec<CandidateResult>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct CandidateResult {
    pub rank: usize,
    pub status: String,
    pub score: f64,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub x_m: Option<f64>,
    pub y_m: Option<f64>,
    pub depth_m: Option<f64>,
    pub origin_time: Option<f64>,
    pub rms_residual_s: f64,
    pub station_bias_s: BTreeMap<String, f64>,
    pub covariance: CovarianceSummary,
    pub warnings: Vec<String>,
    pub assignments: Vec<PickAssignment>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct CovarianceSummary {
    pub positive_definite: bool,
    pub rank: usize,
    pub parameter_count: usize,
    pub min_eigenvalue: f64,
    pub max_eigenvalue: f64,
    pub condition_number: Option<f64>,
    pub source_sigma: SourceSigma,
    pub matrix: Vec<f64>,
    pub parameter_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct SourceSigma {
    pub x_m: f64,
    pub y_m: f64,
    pub depth_m: f64,
    pub origin_time_s: f64,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PickAssignment {
    pub observation_id: String,
    pub station_id: String,
    pub phase: String,
    pub corrected_time: f64,
    pub predicted_time: Option<f64>,
    pub residual_s: Option<f64>,
    pub uncertainty_s: f64,
    pub confidence: f64,
    pub weight: f64,
    pub score_contribution: f64,
    pub locked: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RejectedPick {
    pub observation_id: String,
    pub station_id: String,
    pub phase: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct EventWindow {
    pub key: String,
    pub label: String,
    pub pick_ids: Vec<String>,
    pub min_time: f64,
    pub max_time: f64,
    pub centroid_time: f64,
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub pick: Pick,
    pub station: Station,
    pub locked: bool,
}

impl Observation {
    pub fn phase_name(&self) -> &'static str {
        match self.pick.phase {
            Phase::P => "P",
            Phase::S => "S",
        }
    }
}
