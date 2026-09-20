use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum InputRecord {
    Station(StationRecord),
    VelocityModel(ModelRecord),
    Pick(PickRecord),
}

#[derive(Debug, Clone, Deserialize)]
pub struct StationRecord {
    #[serde(rename = "record_type", default)]
    pub record_type: Option<String>,
    pub station_id: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub elevation_m: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelRecord {
    #[serde(rename = "record_type", default)]
    pub record_type: Option<String>,
    pub version: String,
    pub layers: Vec<LayerRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerRecord {
    pub depth_top_m: f64,
    pub depth_bottom_m: f64,
    pub vp_m_s: f64,
    #[serde(rename = "vs_m_s")]
    pub pub_vs_m_s: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PickRecord {
    #[serde(rename = "record_type", default)]
    pub record_type: Option<String>,
    pub observation_id: String,
    #[serde(default)]
    pub related_observation_id: Option<String>,
    pub station_id: String,
    #[serde(rename = "phase")]
    pub phase_text: String,
    pub time: serde_json::Value,
    #[serde(default = "default_uncertainty")]
    pub time_uncertainty_s: f64,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(rename = "velocity_model_version")]
    pub velocity_model_version: String,
}

fn default_uncertainty() -> f64 {
    0.10
}
fn default_confidence() -> f64 {
    1.0
}
fn default_source() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Phase {
    P,
    S,
}

impl Phase {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_uppercase().as_str() {
            "P" => Ok(Phase::P),
            "S" => Ok(Phase::S),
            other => Err(format!("unsupported phase {other:?}; expected P or S")),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Station {
    pub id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Pick {
    pub id: String,
    pub related_id: Option<String>,
    pub station_id: String,
    pub phase: Phase,
    pub raw_time: f64,
    pub corrected_time: f64,
    pub uncertainty_s: f64,
    pub confidence: f64,
    pub source: String,
    pub model_version: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClockSegment {
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub offset_s: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SolverInput {
    pub stations: BTreeMap<String, Station>,
    pub models: BTreeMap<String, Vec<LayerRecord>>,
    pub picks: BTreeMap<String, Pick>,
    pub corrections: BTreeMap<String, Vec<ClockSegment>>,
    pub locked: BTreeMap<String, String>,
}

impl StationRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.station_id.trim().is_empty() {
            return Err("station_id is required".into());
        }
        if !(-90.0..=90.0).contains(&self.latitude) {
            return Err("latitude must be in [-90, 90]".into());
        }
        if !(-180.0..=180.0).contains(&self.longitude) {
            return Err("longitude must be in [-180, 180]".into());
        }
        if !self.elevation_m.is_finite() || self.elevation_m.abs() > 12_000.0 {
            return Err("elevation_m is outside [-12 km, 12 km]".into());
        }
        Ok(())
    }
}

impl ModelRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.version.trim().is_empty() {
            return Err("velocity model version is required".into());
        }
        if self.layers.is_empty() {
            return Err("velocity model has no layers".into());
        }
        let mut last_bottom = f64::NAN;
        for (index, layer) in self.layers.iter().enumerate() {
            if !layer.depth_top_m.is_finite()
                || !layer.depth_bottom_m.is_finite()
                || layer.depth_top_m < 0.0
                || layer.depth_bottom_m <= layer.depth_top_m
            {
                return Err(format!("layer {index} has invalid depth interval"));
            }
            if index > 0 && (layer.depth_top_m - last_bottom).abs() > 1.0 {
                return Err(format!("layer {index} is not contiguous"));
            }
            if layer.vp_m_s <= 0.0 || layer.pub_vs_m_s <= 0.0 || layer.pub_vs_m_s >= layer.vp_m_s {
                return Err(format!("layer {index} requires 0 < Vs < Vp"));
            }
            last_bottom = layer.depth_bottom_m;
        }
        Ok(())
    }
}

impl PickRecord {
    pub fn validated_time(&self) -> Result<f64, String> {
        crate::time::parse_time(&self.time)
    }

    pub fn validate(&self, at: f64) -> Result<(), String> {
        if self.observation_id.trim().is_empty() {
            return Err("observation_id is required".into());
        }
        if self.station_id.trim().is_empty() {
            return Err("station_id is required".into());
        }
        Phase::parse(&self.phase_text)?;
        if !at.is_finite() {
            return Err("pick time is not finite".into());
        }
        if !(0.001..=60.0).contains(&self.time_uncertainty_s) {
            return Err("time_uncertainty_s must be in [0.001, 60] seconds".into());
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err("confidence must be in [0, 1]".into());
        }
        if self.source != "auto" && self.source != "manual" {
            return Err("source must be auto or manual".into());
        }
        if self.velocity_model_version.trim().is_empty() {
            return Err("velocity_model_version is required".into());
        }
        Ok(())
    }
}

pub fn local_origin(stations: &BTreeMap<String, Station>) -> (f64, f64) {
    let lat = stations.values().map(|s| s.latitude).sum::<f64>() / stations.len().max(1) as f64;
    let lon = stations.values().map(|s| s.longitude).sum::<f64>() / stations.len().max(1) as f64;
    (lat, lon)
}

pub fn project_xy(latitude: f64, longitude: f64, origin_lat: f64, origin_lon: f64) -> (f64, f64) {
    const EARTH_M: f64 = 6_371_008.8;
    let lat0 = origin_lat.to_radians();
    let x = EARTH_M * (longitude.to_radians() - origin_lon.to_radians()) * lat0.cos();
    let y = EARTH_M * (latitude.to_radians() - lat0);
    (x, y)
}

pub fn unproject_xy(x: f64, y: f64, origin_lat: f64, origin_lon: f64) -> (f64, f64) {
    const EARTH_M: f64 = 6_371_008.8;
    let lat = origin_lat + (y / EARTH_M).to_degrees();
    let lon = origin_lon + (x / (EARTH_M * origin_lat.to_radians().cos())).to_degrees();
    (lat, lon)
}

#[cfg(test)]
mod parsing_tests {
    use super::*;
    #[test]
    fn parses_sample_pick() {
        let line = serde_json::json!({"record_type":"pick","observation_id":"x","station_id":"s","phase":"P","time":1.0,"velocity_model_version":"v"});
        let parsed: InputRecord = serde_json::from_value(line).unwrap();
        match parsed {
            InputRecord::Pick(pick) => assert_eq!(pick.velocity_model_version, "v"),
            _ => panic!(),
        }
    }
}

#[test]
fn parses_entire_sample_dataset() {
    let text = include_str!("../data/sample.jsonl");
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let _record: InputRecord = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("line {}: {error}", line_number + 1));
    }
}
