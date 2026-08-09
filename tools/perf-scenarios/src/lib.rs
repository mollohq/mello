use mello_core::Event;
use serde::Deserialize;

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    20_000
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Step {
    DeviceAuth {
        device_id: String,
    },
    Login {
        email: String,
        password: String,
    },
    /// Guest crew discovery — the first call a new user makes, authenticated
    /// with the compile-time http_key rather than a session.
    DiscoverCrews,
    /// The whole signup transaction: create the account, then either join the
    /// named crew or create one.
    ///
    /// `crew_name` without `crew_id` creates a fresh crew, which is what the
    /// release smoke test uses so it never touches a real user's crew.
    FinalizeOnboarding {
        #[serde(default)]
        crew_id: Option<String>,
        #[serde(default)]
        crew_name: Option<String>,
        display_name: String,
    },
    /// Delete a crew. Omit `crew_id` to delete the one this run created.
    ///
    /// The smoke test cannot name the id up front — it does not exist until
    /// `FinalizeOnboarding` succeeds — so the runner remembers it from the
    /// `CrewCreated` event.
    DeleteCrew {
        #[serde(default)]
        crew_id: Option<String>,
    },
    /// Permanently delete the signed-in account. Irreversible.
    ///
    /// Exists so the release smoke test cleans up the throwaway account it
    /// creates against production; without it every release leaves one behind
    /// and skews `admin_dashboard_stats`. Delete the crew first — the account
    /// going away does not take its crews with it.
    DeleteAccount,
    SelectCrew {
        crew_id: String,
    },
    JoinVoice {
        channel_id: String,
    },
    LeaveVoice,
    SetMute {
        muted: bool,
    },
    InjectWav {
        path: String,
        #[serde(default = "default_true")]
        loop_source: bool,
    },
    StopInject,
    Sleep {
        ms: u64,
    },
    ExpectEvent {
        event: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    Sample {
        duration_s: u64,
        #[serde(default)]
        label: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub steps: Vec<Step>,
}

pub fn expand_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn event_type(ev: &Event) -> String {
    serde_json::to_value(ev)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "Unknown".to_string())
}

pub fn load_scenario(path: &str) -> Result<Scenario, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let raw = expand_env(&raw);
    Ok(serde_json::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario_path(name: &str) -> String {
        format!(
            "{}/../perf-harness/scenarios/{}.json",
            env!("CARGO_MANIFEST_DIR"),
            name
        )
    }

    /// The release smoke test signs up against **production**. If it stops
    /// deleting what it created, every release leaves a real account and crew
    /// behind and `admin_dashboard_stats` drifts — silently, because the run
    /// still passes. Pin the cleanup so removing it fails here instead.
    #[test]
    fn signup_smoke_cleans_up_after_itself() {
        let scenario = load_scenario(&scenario_path("signup_smoke")).expect("scenario must parse");

        assert!(
            scenario
                .steps
                .iter()
                .any(|s| matches!(s, Step::DeleteCrew { .. })),
            "signup_smoke must delete the crew it creates"
        );
        assert!(
            scenario
                .steps
                .iter()
                .any(|s| matches!(s, Step::DeleteAccount)),
            "signup_smoke must delete the account it creates"
        );

        // Order matters: deleting the account first revokes the session that
        // the crew deletion needs, orphaning the crew as an empty group.
        let crew_at = scenario
            .steps
            .iter()
            .position(|s| matches!(s, Step::DeleteCrew { .. }))
            .expect("checked above");
        let account_at = scenario
            .steps
            .iter()
            .position(|s| matches!(s, Step::DeleteAccount))
            .expect("checked above");
        assert!(
            crew_at < account_at,
            "delete_crew must come before delete_account"
        );
    }

    /// Both deletions must be awaited. Without the `expect_event` the client
    /// quits as soon as the command is queued and the process can exit before
    /// the request is even sent.
    #[test]
    fn signup_smoke_waits_for_both_deletions() {
        let scenario = load_scenario(&scenario_path("signup_smoke")).expect("scenario must parse");
        let awaited: Vec<&str> = scenario
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::ExpectEvent { event, .. } => Some(event.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            awaited.contains(&"CrewDeleted"),
            "expected a wait for CrewDeleted, got {awaited:?}"
        );
        assert!(
            awaited.contains(&"AccountDeleted"),
            "expected a wait for AccountDeleted, got {awaited:?}"
        );
    }

    #[test]
    fn delete_crew_defaults_to_no_explicit_id() {
        let step: Step = serde_json::from_str(r#"{"action":"delete_crew"}"#).unwrap();
        assert!(matches!(step, Step::DeleteCrew { crew_id: None }));

        let step: Step =
            serde_json::from_str(r#"{"action":"delete_crew","crew_id":"abc"}"#).unwrap();
        assert!(matches!(step, Step::DeleteCrew { crew_id: Some(id) } if id == "abc"));
    }

    #[test]
    fn delete_account_parses() {
        let step: Step = serde_json::from_str(r#"{"action":"delete_account"}"#).unwrap();
        assert!(matches!(step, Step::DeleteAccount));
    }
}
