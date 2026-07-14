//! Shared HTTP clients.
//!
//! `reqwest::Client::new()` builds a fresh connection pool, DNS resolver and TLS
//! config every time, and a per-request client can't reuse keep-alive
//! connections — so avatar/GIF/snapshot fetches pay a new TLS handshake (and a
//! `spawn_blocking` DNS lookup) on every call. Sharing one client fixes all of
//! that: `reqwest::Client` is internally `Arc`, so cloning is cheap.

use std::sync::OnceLock;

/// Process-wide async HTTP client. Cheap to clone (shares the pool).
pub fn shared() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(concat!("mello/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        })
        .clone()
}
