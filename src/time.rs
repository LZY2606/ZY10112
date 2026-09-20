pub(crate) fn parse_time(value: &serde_json::Value) -> Result<f64, String> {
    if let Some(number) = value.as_f64() {
        return finite(number, "time");
    }
    let text = value
        .as_str()
        .ok_or_else(|| "time must be a number or RFC3339 string".to_string())?;
    parse_rfc3339_utc(text)
}

pub(crate) fn finite(value: f64, name: &str) -> Result<f64, String> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{name} must be finite"))
    }
}

fn parse_rfc3339_utc(input: &str) -> Result<f64, String> {
    let text = input.strip_suffix('Z').unwrap_or(input);
    let (date, clock) = text
        .split_once('T')
        .ok_or_else(|| "expected YYYY-MM-DDTHH:MM:SSZ".to_string())?;
    let mut date_parts = date.split('-');
    let year: i64 = parse_part(date_parts.next(), "year")?;
    let month: i64 = parse_part(date_parts.next(), "month")?;
    let day: i64 = parse_part(date_parts.next(), "day")?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err("invalid calendar date".to_string());
    }
    let mut time_parts = clock.split(':');
    let hour: i64 = parse_part(time_parts.next(), "hour")?;
    let minute: i64 = parse_part(time_parts.next(), "minute")?;
    let second_text = time_parts
        .next()
        .ok_or_else(|| "missing seconds".to_string())?;
    let whole_seconds_before_fraction: i64 = second_text
        .split('.')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(-1);
    if time_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&whole_seconds_before_fraction)
    {
        return Err("invalid clock time".to_string());
    }
    let (whole_seconds, fraction) = if let Some((whole, frac)) = second_text.split_once('.') {
        let whole: i64 = whole.parse().map_err(|_| "invalid seconds".to_string())?;
        let mut normalized = format!("{frac:0<3}");
        normalized.truncate(3);
        let millis: i64 = normalized
            .parse()
            .map_err(|_| "invalid fractional seconds".to_string())?;
        (whole, millis as f64 / 1000.0)
    } else {
        (
            second_text
                .parse()
                .map_err(|_| "invalid seconds".to_string())?,
            0.0,
        )
    };
    let days = days_from_civil(year, month, day);
    Ok(days as f64 * 86400.0
        + hour as f64 * 3600.0
        + minute as f64 * 60.0
        + whole_seconds as f64
        + fraction)
}

fn parse_part<T: std::str::FromStr>(part: Option<&str>, name: &str) -> Result<T, String> {
    part.ok_or_else(|| format!("missing {name}"))?
        .parse()
        .map_err(|_| format!("invalid {name}"))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let adjusted_month = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * adjusted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_epoch_and_utc() {
        assert_eq!(parse_time(&serde_json::json!(12.5)).unwrap(), 12.5);
        assert_eq!(
            parse_time(&serde_json::json!("1970-01-01T00:00:01.500Z")).unwrap(),
            1.5
        );
        assert!(parse_time(&serde_json::json!("1970-01-01T25:00:00Z")).is_err());
    }
}
