//! Keeps `qa/flows.yaml` honest (plans/E2E-QA.md §9).
//!
//! The catalogue maps each user flow to the tests that cover it. A renamed or
//! deleted test would leave the catalogue claiming coverage that no longer
//! exists, so this checks every name it lists. The file uses a flat YAML
//! subset (one key per line, lists in `[ ]`), which is parsed here by line to
//! avoid a YAML dependency.

use std::collections::HashSet;
use std::path::Path;

#[derive(Default, Debug)]
struct Flow {
    id: String,
    line: usize,
    priority: String,
    title: String,
    harness: Vec<String>,
    journeys: Vec<String>,
}

fn list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse(src: &str) -> Vec<Flow> {
    let mut flows: Vec<Flow> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(id) = line.strip_prefix("- id:") {
            flows.push(Flow {
                id: id.trim().to_string(),
                line: i + 1,
                ..Flow::default()
            });
            continue;
        }
        let Some(flow) = flows.last_mut() else {
            continue;
        };
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "priority" => flow.priority = value.trim().to_string(),
                "title" => flow.title = value.trim().to_string(),
                "harness" => flow.harness = list(value),
                "journeys" => flow.journeys = list(value),
                _ => {}
            }
        }
    }
    flows
}

/// Every `fn name(` in the client crate's sources.
fn test_functions(dir: &Path, out: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            test_functions(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            let src = std::fs::read_to_string(&p).unwrap_or_default();
            for part in src.split("fn ").skip(1) {
                let name: String = part
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if part[name.len()..].starts_with('(') {
                    out.insert(name);
                }
            }
        }
    }
}

#[test]
fn the_flow_catalogue_names_only_tests_and_journeys_that_exist() {
    let client = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = client.parent().expect("client/ has a parent");
    let path = repo.join("qa/flows.yaml");
    let src = std::fs::read_to_string(&path).expect("read qa/flows.yaml");
    let flows = parse(&src);
    assert!(
        flows.len() > 10,
        "qa/flows.yaml parsed to {} flows",
        flows.len()
    );

    let mut fns = HashSet::new();
    test_functions(&client.join("src"), &mut fns);

    let mut problems = Vec::new();
    let mut ids = HashSet::new();
    for f in &flows {
        let at = format!("qa/flows.yaml:{} {}", f.line, f.id);
        if !ids.insert(f.id.clone()) {
            problems.push(format!("{at}: duplicate id"));
        }
        if !["P0", "P1", "P2", "P3"].contains(&f.priority.as_str()) {
            problems.push(format!("{at}: priority {:?} is not P0..P3", f.priority));
        }
        if f.title.is_empty() {
            problems.push(format!("{at}: no title"));
        }
        for h in &f.harness {
            if !fns.contains(h) {
                problems.push(format!(
                    "{at}: harness test `{h}` does not exist in client/src"
                ));
            }
        }
        for j in &f.journeys {
            if !repo.join(j).is_file() {
                problems.push(format!("{at}: journey `{j}` does not exist"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_flat_format() {
        let src = "flows:\n  # comment\n  - id: A-1\n    title: One: two\n    priority: P0\n    harness: [a, b]\n    journeys: []\n  - id: A-2\n    priority: P3\n";
        let flows = parse(src);
        assert_eq!(flows.len(), 2);
        assert_eq!(flows[0].id, "A-1");
        assert_eq!(flows[0].title, "One: two");
        assert_eq!(flows[0].harness, vec!["a", "b"]);
        assert!(flows[0].journeys.is_empty());
        assert_eq!(flows[1].priority, "P3");
    }
}
