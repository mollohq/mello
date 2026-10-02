//! The build stamp: which commit and which features made this binary.
//!
//! `build.rs` records the commit, the dirty flag and the build time. A QA
//! harness runs `mello --build-info` to check that the binary matches the
//! commit it wants to test.

use serde::Serialize;

/// The command-line flag that prints the stamp.
pub const FLAG: &str = "--build-info";

/// The values in the stamp. `built_at` is RFC 3339 in UTC.
#[derive(Serialize)]
pub struct BuildStamp<'a> {
    pub commit: &'a str,
    pub dirty: bool,
    pub built_at: &'a str,
    pub version: &'a str,
    pub features: Vec<&'a str>,
}

impl BuildStamp<'_> {
    /// One line of JSON. The field order is part of the contract.
    pub fn to_json(&self) -> String {
        // A struct of strings, a bool and a list of strings always serializes.
        serde_json::to_string(self).expect("build stamp serializes")
    }
}

/// The Cargo features of `mello-client` that this binary was built with.
pub fn enabled_features() -> Vec<&'static str> {
    [
        ("development", cfg!(feature = "development")),
        ("e2e", cfg!(feature = "e2e")),
        ("mcp", cfg!(feature = "mcp")),
        ("production", cfg!(feature = "production")),
        ("testkit", cfg!(feature = "testkit")),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect()
}

/// The stamp of this binary, as one line of JSON.
pub fn current_json() -> String {
    BuildStamp {
        commit: build_stamp::COMMIT,
        dirty: build_stamp::DIRTY,
        built_at: build_stamp::BUILT_AT,
        version: env!("CARGO_PKG_VERSION"),
        features: enabled_features(),
    }
    .to_json()
}

/// True when the flag is anywhere in the arguments after the program name.
///
/// `args` must not include argv[0].
pub fn requested<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| a.as_ref() == FLAG)
}

/// Print the stamp to stdout. This runs before logging and the UI start, so
/// it must have no other effect. A closed pipe is not an error here.
pub fn print_current() {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{}", current_json());
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn json_has_the_documented_shape_and_field_order() {
        let stamp = BuildStamp {
            commit: COMMIT,
            dirty: false,
            built_at: "2026-10-02T09:12:00Z",
            version: "0.0.0-DEV",
            features: vec!["development", "e2e"],
        };
        assert_eq!(
            stamp.to_json(),
            format!(
                r#"{{"commit":"{COMMIT}","dirty":false,"built_at":"2026-10-02T09:12:00Z","version":"0.0.0-DEV","features":["development","e2e"]}}"#
            )
        );
    }

    #[test]
    fn json_is_one_line_and_parses_back() {
        let stamp = BuildStamp {
            commit: "unknown",
            dirty: true,
            built_at: "2026-10-02T09:12:00Z",
            version: "1.2.3",
            features: vec![],
        };
        let json = stamp.to_json();
        assert!(!json.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["commit"], "unknown");
        assert_eq!(v["dirty"], true);
        assert_eq!(v["features"], serde_json::json!([]));
    }

    #[test]
    fn current_stamp_matches_this_build() {
        let v: serde_json::Value = serde_json::from_str(&current_json()).unwrap();
        let commit = v["commit"].as_str().unwrap();
        assert!(
            commit == "unknown"
                || (commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())),
            "bad commit: {commit}"
        );
        assert!(v["dirty"].is_boolean());
        let at = v["built_at"].as_str().unwrap();
        // 2026-10-02T09:12:00Z
        assert_eq!(at.len(), 20, "bad timestamp: {at}");
        assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T');
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        let features: Vec<&str> = v["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert_eq!(features.contains(&"e2e"), cfg!(feature = "e2e"));
        assert_eq!(
            features.contains(&"production"),
            cfg!(feature = "production")
        );
    }

    #[test]
    fn flag_is_found_anywhere_in_the_arguments() {
        assert!(requested(["--build-info"]));
        assert!(requested(["--instance", "a", "--build-info"]));
        assert!(requested(["--build-info", "mello://join/ABCD-1234"]));
    }

    #[test]
    fn other_arguments_do_not_request_the_stamp() {
        assert!(!requested(Vec::<&str>::new()));
        assert!(!requested(["--reset", "--loopback"]));
        assert!(!requested(["mello://join/ABCD-1234"]));
        assert!(!requested([
            "--build-information",
            "build-info",
            "--BUILD-INFO"
        ]));
    }

    #[test]
    fn the_flag_is_not_a_deep_link() {
        use crate::deep_link::deep_link_in;
        assert_eq!(deep_link_in(["--build-info"]), None);
        assert_eq!(deep_link_in(["--build-info", "--reset"]), None);
    }
}
