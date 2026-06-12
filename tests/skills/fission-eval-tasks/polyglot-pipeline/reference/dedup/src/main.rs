//! Reference solution for the dedup component. See dedup/SPEC.md.
//! Excluded from agent visibility by the SKILL runner.
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use serde_json::{Map, Value};

struct Group {
    win_date: String,
    win_pos: usize,
    winner: Map<String, Value>,
    tags: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: dedup FILE1.jsonl [FILE2.jsonl ...]");
        std::process::exit(2);
    }

    // id -> group; BTreeMap keeps output sorted by id ascending (byte order).
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut pos = 0usize;

    for path in &args {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cannot read {}: {}", path, e);
                std::process::exit(1);
            }
        };
        for line in io::BufReader::new(file).lines() {
            let line = line.unwrap_or_default();
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("bad json: {}", e);
                    std::process::exit(1);
                }
            };
            let obj = match v.as_object() {
                Some(o) => o.clone(),
                None => {
                    eprintln!("line is not a JSON object");
                    std::process::exit(1);
                }
            };
            let id = obj.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let date = obj.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let row_tags: Vec<String> = obj
                .get("tags")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                .unwrap_or_default();

            let cur_pos = pos;
            pos += 1;

            match groups.get_mut(&id) {
                Some(g) => {
                    // newest date wins; tie -> larger position.
                    if (date.as_str(), cur_pos) > (g.win_date.as_str(), g.win_pos) {
                        g.win_date = date;
                        g.win_pos = cur_pos;
                        g.winner = obj;
                    }
                    g.tags.extend(row_tags);
                }
                None => {
                    groups.insert(
                        id,
                        Group { win_date: date, win_pos: cur_pos, winner: obj, tags: row_tags },
                    );
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for (_id, g) in groups {
        let mut rec = g.winner;
        let mut tags = g.tags;
        tags.sort();
        tags.dedup();
        rec.insert("tags".into(), Value::Array(tags.into_iter().map(Value::String).collect()));
        let line = serde_json::to_string(&Value::Object(rec)).unwrap();
        writeln!(out, "{}", line).unwrap();
    }
}
