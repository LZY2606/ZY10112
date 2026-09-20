mod db;
#[cfg(test)]
mod integration_test;
mod linalg;
mod model;
mod service;
mod solver;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut listen = "127.0.0.1:5312".to_string();
    let mut database = "pairwise_gsb.sqlite".to_string();
    let mut demo = "data/demo.jsonl".to_string();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--listen" => {
                index += 1;
                listen = args.get(index).cloned().expect("--listen requires address");
            }
            "--database" => {
                index += 1;
                database = args.get(index).cloned().expect("--database requires path");
            }
            "--demo" => {
                index += 1;
                demo = args.get(index).cloned().expect("--demo requires path");
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        index += 1;
    }
    if let Err(error) = service::run(&listen, &database, &demo) {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}
