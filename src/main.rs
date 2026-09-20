use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use pairwise_gsb::db::Store;

mod web;

fn parse_listen(args: &[String]) -> Result<String, String> {
    let mut listen = "127.0.0.1:5312".to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--listen" => {
                index += 1;
                listen = args
                    .get(index)
                    .cloned()
                    .ok_or_else(|| "missing --listen address".to_string())?;
            }
            "--db" => {
                index += 1;
                let _ = args
                    .get(index)
                    .ok_or_else(|| "missing --db path".to_string())?;
            }
            "--help" | "-h" => {
                println!("usage: pair-wise-gsb --listen 127.0.0.1:5312 [--db data/local.sqlite]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
        index += 1;
    }
    Ok(listen)
}

fn parse_db(args: &[String]) -> String {
    args.windows(2)
        .find_map(|pair| (pair[0] == "--db").then(|| pair[1].clone()))
        .unwrap_or_else(|| "data/local.sqlite".to_string())
}

fn bootstrap(store: &Store) -> Result<(), String> {
    let state = store.state()?;
    if state.stations.is_empty() && Path::new("data/sample.jsonl").exists() {
        let bytes = std::fs::read("data/sample.jsonl").map_err(|e| e.to_string())?;
        store.import_jsonl(&bytes)?;
    }
    store.ensure_correction_v1()?;
    store.ensure_hypotheses()?;
    let state = store.state()?;
    for hypothesis in state.hypotheses {
        if hypothesis.current_set_id.is_none() {
            store.recompute(hypothesis.hypothesis_id.as_str(), Some("v1"), None)?;
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, store: &Store) {
    let mut buffer = [0u8; 1_048_576];
    let Ok(read) = stream.read(&mut buffer) else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
    let response = web::route(&request, store);
    let _ = stream.write_all(response.as_slice());
    let _ = stream.flush();
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen = parse_listen(&args)?;
    let db_path = parse_db(&args);
    if let Some(parent) = Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let store = Store::open(&db_path)?;
    bootstrap(&store)?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("cannot bind {listen}: {e}"))?;
    eprintln!("listening on http://{listen}");
    for connection in listener.incoming() {
        if let Ok(stream) = connection {
            handle_connection(stream, &store);
        }
    }
    Ok(())
}
