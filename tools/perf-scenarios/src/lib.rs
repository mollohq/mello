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
