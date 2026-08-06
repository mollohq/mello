//! Cross-language contract test between the Rust client and the Go backend.
//!
//! Nothing else in the build connects these two sides: renaming an RPC in
//! `main.go` compiles fine, `go test` passes, `cargo test` passes, and the
//! break only appears at runtime as a failed call against production.
//!
//! This test reads both sources and asserts every RPC the client invokes is
//! actually registered by the server. It needs no Docker, no backend and no
//! network — it is pure text analysis, so it belongs in the fast PR lane.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/mello-core
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("mello-core should have a parent directory")
        .to_path_buf()
}

/// Collect the string literal following each occurrence of `marker`.
///
/// Hand-rolled rather than pulling in a regex dependency for two patterns.
fn literals_after(haystack: &str, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = haystack;
    while let Some(idx) = rest.find(marker) {
        rest = &rest[idx + marker.len()..];
        if let Some(end) = rest.find('"') {
            out.push(rest[..end].to_string());
        }
    }
    out
}

/// Collect the identifier following each occurrence of `marker`, stopping at
/// the first character that cannot appear in an RPC name.
fn idents_after(haystack: &str, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = haystack;
    while let Some(idx) = rest.find(marker) {
        rest = &rest[idx + marker.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end > 0 {
            out.push(rest[..end].to_string());
        }
    }
    out
}

/// Every RPC id registered by the Nakama modules.
fn server_rpcs() -> BTreeSet<String> {
    let main_go = repo_root().join("backend/nakama/data/modules/main.go");
    let src = std::fs::read_to_string(&main_go).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}). This test compares the client's RPC calls \
             against the server's registrations and cannot run without it.",
            main_go.display()
        )
    });
    literals_after(&src, "RegisterRpc(\"").into_iter().collect()
}

/// Every RPC id the client invokes.
///
/// Two call shapes exist: the `rpc("name", ..)` helper, and a handful of
/// hand-built `/v2/rpc/<name>` URLs used for calls that authenticate with the
/// http_key instead of a session (guest discovery, crew avatars).
fn client_rpcs() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let src_dir = repo_root().join("mello-core/src");

    let mut stack = vec![src_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable source dir") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable source file");
            found.extend(literals_after(&text, "rpc(\""));
            found.extend(idents_after(&text, "/v2/rpc/"));
        }
    }
    found
}

/// Client RPCs known to have no backend registration.
///
/// Each entry is a latent runtime failure kept visible rather than hidden.
/// Removing the dead client method is a public-API change, so it is flagged
/// here instead of being made silently.
///
/// - `channel_list`: `NakamaClient::channel_list` (nakama/client.rs) has no
///   callers anywhere in the workspace. The backend registers
///   `channel_create`/`rename`/`delete`/`reorder` but never `channel_list`, so
///   wiring this method up would fail against a real server.
const KNOWN_UNREGISTERED: &[&str] = &["channel_list"];

/// Every RPC the client calls must exist on the server.
#[test]
fn every_client_rpc_is_registered_by_the_backend() {
    let server = server_rpcs();
    let client = client_rpcs();

    assert!(
        server.len() > 20,
        "only found {} server RPCs — the parser is probably broken rather than \
         the backend being nearly empty",
        server.len()
    );
    assert!(
        client.len() > 20,
        "only found {} client RPC calls — the parser is probably broken",
        client.len()
    );

    let missing: Vec<&String> = client
        .difference(&server)
        .filter(|name| !KNOWN_UNREGISTERED.contains(&name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "the client calls RPCs the backend does not register: {missing:?}\n\
         Either the RPC was renamed or removed in \
         backend/nakama/data/modules/main.go without updating mello-core, or \
         it was never registered. Every one of these fails at runtime against \
         a real server."
    );

    // Keep the allowlist honest: once an entry is registered (or the dead
    // client method is deleted) it must be removed from KNOWN_UNREGISTERED.
    let stale: Vec<&&str> = KNOWN_UNREGISTERED
        .iter()
        .filter(|name| server.contains(**name) || !client.contains(**name))
        .collect();
    assert!(
        stale.is_empty(),
        "KNOWN_UNREGISTERED is out of date: {stale:?} no longer needs an \
         exception (it is now registered, or the client no longer calls it). \
         Remove it so the list keeps reflecting real gaps."
    );
}

/// Informational: RPCs the backend registers but no client code calls.
///
/// Not a failure — some are admin- or ops-only (`admin_dashboard_stats` is
/// http_key gated, `dev_*` are local tooling). Printed so dead surface is
/// visible rather than accumulating silently.
#[test]
fn report_backend_rpcs_the_client_never_calls() {
    let server = server_rpcs();
    let client = client_rpcs();

    let unused: Vec<&String> = server.difference(&client).collect();
    println!("backend RPCs not called by mello-core ({}):", unused.len());
    for name in &unused {
        println!("  {name}");
    }
}
