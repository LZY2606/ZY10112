use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Phase {
    P,
    S,
}

impl Phase {
    pub fn velocity(self, model: &VelocityModel) -> f64 {
        match self {
            Phase::P => model.vp_kms,
            Phase::S => model.vs_kms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PickSource {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PickStatus {
    #[default]
    Active,
    Locked,
    Noise,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub station_id: String,
    pub x_km: f64,
    pub y_km: f64,
    pub elevation_km: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityModel {
    pub model_id: String,
    pub vp_kms: f64,
    pub vs_kms: f64,
    pub x_min_km: f64,
    pub x_max_km: f64,
    pub y_min_km: f64,
    pub y_max_km: f64,
}

impl VelocityModel {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        (self.x_min_km..=self.x_max_km).contains(&x) && (self.y_min_km..=self.y_max_km).contains(&y)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockSegment {
    pub version: String,
    pub station_id: String,
    pub start_time: String,
    pub end_time: Option<String>,
    pub bias_seconds: f64,
    pub rate_s_per_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pick {
    pub pick_id: String,
    pub station_id: String,
    pub phase: Phase,
    pub time: String,
    pub sigma_seconds: f64,
    pub confidence: f64,
    pub source: PickSource,
    pub model_id: String,
    #[serde(default)]
    pub status: PickStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputRecord {
    Station(Station),
    VelocityModel(VelocityModel),
    ClockCorrection(ClockSegment),
    Pick(Pick),
}

#[derive(Debug, Clone, Copy)]
pub struct PickObservation {
    pub pick_index: usize,
    pub station_index: usize,
    pub phase: Phase,
    pub observed_epoch: f64,
    pub sigma_seconds: f64,
    pub confidence: f64,
}

pub fn parse_epoch(value: &str) -> Result<f64, String> {
    let normalized = value
        .strip_suffix('Z')
        .map(|s| format!("{s}+00:00"))
        .unwrap_or_else(|| value.to_string());
    let (date_part, time_with_offset) = normalized
        .split_once('T')
        .ok_or_else(|| format!("invalid timestamp: {value}"))?;
    let offset_position = time_with_offset
        .rfind(['+', '-'])
        .ok_or_else(|| format!("timestamp without UTC offset: {value}"))?;
    let clock_part = &time_with_offset[..offset_position];
    let offset_part = &time_with_offset[offset_position + 1..];
    let calendar = date_part;
    let mut calendar = calendar.split('-');
    let year: i32 = parse_part(calendar.next(), value)?;
    let month: u32 = parse_part(calendar.next(), value)?;
    let day: u32 = parse_part(calendar.next(), value)?;
    let mut clock = clock_part.split(':');
    let hour: u32 = parse_part(clock.next(), value)?;
    let minute: u32 = parse_part(clock.next(), value)?;
    let second: f64 = parse_part(clock.next(), value)?;
    let offset_sign = if time_with_offset.as_bytes()[offset_position] == b'-' {
        -1.0
    } else {
        1.0
    };
    let mut offset = offset_part.split(':');
    let offset_hour: f64 = parse_part(offset.next(), value)?;
    let offset_minute: f64 = parse_part(offset.next(), value)?;
    let epoch = days_from_civil(year, month, day) as f64 * 86_400.0
        + hour as f64 * 3_600.0
        + minute as f64 * 60.0
        + second
        - offset_sign * (offset_hour * 3_600.0 + offset_minute * 60.0);
    Ok(epoch)
}

fn parse_part<T: std::str::FromStr>(part: Option<&str>, value: &str) -> Result<T, String> {
    part.and_then(|part| part.parse().ok())
        .ok_or_else(|| format!("invalid timestamp: {value}"))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = (if month > 2 { month - 3 } else { month + 9 }) as i64;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era =
        year_of_era as i64 * 365 + year_of_era as i64 / 4 - year_of_era as i64 / 100 + day_of_year;
    era as i64 * 146097 + day_of_era - 719468
}

pub fn format_epoch(epoch: f64) -> String {
    let rounded_total_milliseconds = (epoch * 1000.0).round() as i64;
    let total_days = rounded_total_milliseconds.div_euclid(86_400_000);
    let milliseconds_of_day = rounded_total_milliseconds.rem_euclid(86_400_000);
    let seconds_of_day = milliseconds_of_day as f64 / 1000.0;
    let (year, month, day) = civil_from_days(total_days);
    let hour = (seconds_of_day / 3_600.0) as u32;
    let minute = ((seconds_of_day % 3_600.0) / 60.0) as u32;
    let second = seconds_of_day % 60.0;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:06.3}Z")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y as i32 + if m <= 2 { 1 } else { 0 }, m, d)
}
