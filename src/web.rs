use pairwise_gsb::db::Store;
use std::io::Write;

fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let _ = write!(output, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n", body.len());
    output.extend_from_slice(body);
    output
}

fn json_response(value: &serde_json::Value, status: &str) -> Vec<u8> {
    response(
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec_pretty(value)
            .unwrap_or_default()
            .as_slice(),
    )
}

fn error(message: &str, code: u16) -> Vec<u8> {
    let status = if code == 404 {
        "404 Not Found"
    } else {
        "400 Bad Request"
    };
    json_response(&serde_json::json!({"error": message}), status)
}

fn body_json(request: &str) -> Result<serde_json::Value, Vec<u8>> {
    let body = request.split("\r\n\r\n").nth(1).unwrap_or("{}");
    serde_json::from_str(body).map_err(|err| error(&err.to_string(), 400))
}

fn query_field(path: &str, name: &str) -> Option<String> {
    path.split_once('?').and_then(|(_, query)| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key == name {
                Some(url_decode(value))
            } else {
                None
            }
        })
    })
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let value = u8::from_str_radix(&input[index + 1..index + 3], 16).unwrap_or(b'?');
                output.push(value);
                index += 3;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).to_string()
}

fn json_string(value: &serde_json::Value, field: &str) -> Result<String, Vec<u8>> {
    value
        .get(field)
        .and_then(|item| item.as_str())
        .map(str::to_string)
        .ok_or_else(|| error(&format!("missing string field {field}"), 400))
}

fn execute_action(
    store: &Store,
    method: &str,
    path: &str,
    request: &str,
) -> Result<Vec<u8>, Vec<u8>> {
    let json = body_json(request)?;
    let result = if path == "/api/import" {
        let text = json_string(&json, "jsonl")?;
        let summary = store
            .import_jsonl(text.as_bytes())
            .map_err(|e| error(e.as_str(), 400))?;
        serde_json::json!(summary)
    } else if path == "/api/lock" {
        let hypothesis = json_string(&json, "hypothesis_id")?;
        let observation = json_string(&json, "observation_id")?;
        let event_key = json
            .get("event_key")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        store
            .set_lock(&hypothesis, &observation, event_key.as_deref(), true)
            .map_err(|e| error(e.as_str(), 400))?;
        store
            .recompute(&hypothesis, None, event_key.as_deref())
            .map_err(|e| error(e.as_str(), 400))?;
        serde_json::json!({"ok": true})
    } else if path == "/api/unlock" {
        let hypothesis = json_string(&json, "hypothesis_id")?;
        let observation = json_string(&json, "observation_id")?;
        store
            .set_lock(&hypothesis, &observation, None, false)
            .map_err(|e| error(e.as_str(), 400))?;
        store
            .recompute(&hypothesis, None, None)
            .map_err(|e| error(e.as_str(), 400))?;
        serde_json::json!({"ok": true})
    } else if path == "/api/noise" {
        let observation = json_string(&json, "observation_id")?;
        let enabled = json.get("noise").and_then(|v| v.as_bool()).unwrap_or(true);
        store
            .set_noise(&observation, enabled)
            .map_err(|e| error(e.as_str(), 400))?;
        for hypothesis in ["H-A", "H-B"] {
            store
                .recompute(hypothesis, None, None)
                .map_err(|e| error(e.as_str(), 400))?;
        }
        serde_json::json!({"ok": true})
    } else if path == "/api/corrections" {
        let version = json_string(&json, "version")?;
        let parent = json
            .get("parent_version")
            .and_then(|v| v.as_str())
            .unwrap_or("v1")
            .to_string();
        let note = json
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap_or("user correction")
            .to_string();
        let mut offsets = std::collections::BTreeMap::new();
        if let Some(object) = json.get("offsets_s").and_then(|v| v.as_object()) {
            for (station, value) in object {
                let number = value
                    .as_f64()
                    .ok_or_else(|| error("offsets_s must contain numbers", 400))?;
                offsets.insert(station.clone(), number);
            }
        }
        store
            .create_correction_version(&version, &parent, &offsets, &note)
            .map_err(|e| error(e.as_str(), 400))?;
        serde_json::json!({"ok": true})
    } else if path == "/api/recompute" {
        let hypothesis = json_string(&json, "hypothesis_id")?;
        let correction = json
            .get("correction_version")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(version) = correction.as_deref() {
            store
                .set_hypothesis_correction(&hypothesis, version)
                .map_err(|e| error(e.as_str(), 400))?;
        }
        let event_key = json
            .get("event_key")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let (set_id, result) = store
            .recompute(&hypothesis, correction.as_deref(), event_key.as_deref())
            .map_err(|e| error(e.as_str(), 400))?;
        serde_json::json!({"set_id": set_id, "result": result})
    } else {
        return Err(error("unknown POST endpoint", 404));
    };
    let _ = method;
    Ok(json_response(&result, "200 OK"))
}

pub(crate) fn route(request: &str, store: &Store) -> Vec<u8> {
    let mut lines = request.lines();
    let request_line = lines.next().unwrap_or("GET / HTTP/1.1");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");

    match (method, path) {
        ("GET", "/") => response(
            "200 OK",
            "text/html; charset=utf-8",
            include_str!("../static/index.html").as_bytes(),
        ),
        ("GET", "/app.js") => response(
            "200 OK",
            "application/javascript; charset=utf-8",
            include_str!("../static/app.js").as_bytes(),
        ),
        ("GET", "/api/state") => match store.state() {
            Ok(state) => json_response(&serde_json::json!(state), "200 OK"),
            Err(err) => error(&err, 400),
        },
        ("GET", "/api/sets") => match store.candidate_sets() {
            Ok(sets) => json_response(&serde_json::json!({"sets": sets}), "200 OK"),
            Err(err) => error(&err, 400),
        },
        ("GET", "/api/set") => {
            let Some(set_id) = query_field(target, "id").and_then(|v| v.parse::<i64>().ok()) else {
                return error("missing numeric id", 400);
            };
            match store.candidate_set(set_id) {
                Ok(result) => json_response(
                    &serde_json::json!({"set_id": set_id, "result": result}),
                    "200 OK",
                ),
                Err(err) => error(&err, 404),
            }
        }
        ("POST", _) => {
            execute_action(store, method, path, request).unwrap_or_else(|response| response)
        }
        _ => error("not found", 404),
    }
}
