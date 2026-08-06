use serde::{Deserialize, Serialize};

/// Parse the permissive boolean spellings accepted in env vars.
fn parse_env_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub nakama_host: String,
    pub nakama_port: u16,
    pub nakama_key: String,
    pub nakama_http_key: String,
    pub nakama_ssl: bool,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub discord_client_id: Option<String>,
    pub twitch_client_id: Option<String>,
}

impl Config {
    pub fn production() -> Self {
        Self {
            nakama_host: "mello-api-1iiv.onrender.com".into(),
            nakama_port: 443,
            nakama_key: option_env!("NAKAMA_SERVER_KEY")
                .unwrap_or("defaultkey")
                .into(),
            nakama_http_key: option_env!("NAKAMA_HTTP_KEY")
                .unwrap_or("defaulthttpkey")
                .into(),
            nakama_ssl: true,
            google_client_id: option_env!("GOOGLE_CLIENT_ID").map(Into::into),
            google_client_secret: option_env!("GOOGLE_CLIENT_SECRET").map(Into::into),
            discord_client_id: option_env!("DISCORD_CLIENT_ID").map(Into::into),
            twitch_client_id: option_env!("TWITCH_CLIENT_ID").map(Into::into),
        }
    }

    pub fn development() -> Self {
        Self::default()
    }

    /// Apply `NAKAMA_*` environment overrides on top of this config.
    ///
    /// Keys, host and port are otherwise baked in at compile time via
    /// `option_env!`, which makes it impossible to point a *built* binary at a
    /// different server. Integration tests, the e2e harness and the production
    /// canary all need exactly that, so they call this.
    ///
    /// Recognised: `NAKAMA_HOST`, `NAKAMA_PORT`, `NAKAMA_SSL`,
    /// `NAKAMA_SERVER_KEY`, `NAKAMA_HTTP_KEY`. Empty or unparseable values are
    /// ignored rather than clobbering a good default.
    #[must_use]
    pub fn with_env_overrides(self) -> Self {
        self.with_overrides_from(|key| std::env::var(key).ok())
    }

    /// Override source as a closure so the precedence rules are testable
    /// without mutating process-global environment state (which races under
    /// parallel test execution).
    #[must_use]
    fn with_overrides_from(mut self, get: impl Fn(&str) -> Option<String>) -> Self {
        let non_empty = |key: &str| get(key).filter(|v| !v.is_empty());

        if let Some(v) = non_empty("NAKAMA_HOST") {
            self.nakama_host = v;
        }
        if let Some(port) = non_empty("NAKAMA_PORT").and_then(|v| v.parse::<u16>().ok()) {
            self.nakama_port = port;
        }
        if let Some(ssl) = non_empty("NAKAMA_SSL").and_then(|v| parse_env_bool(&v)) {
            self.nakama_ssl = ssl;
        }
        if let Some(v) = non_empty("NAKAMA_SERVER_KEY") {
            self.nakama_key = v;
        }
        if let Some(v) = non_empty("NAKAMA_HTTP_KEY") {
            self.nakama_http_key = v;
        }
        self
    }

    pub fn http_base(&self) -> String {
        let scheme = if self.nakama_ssl { "https" } else { "http" };
        format!("{}://{}:{}", scheme, self.nakama_host, self.nakama_port)
    }

    pub fn ws_url(&self, token: &str) -> String {
        let scheme = if self.nakama_ssl { "wss" } else { "ws" };
        format!(
            "{}://{}:{}/ws?lang=en&status=true&token={}",
            scheme, self.nakama_host, self.nakama_port, token
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nakama_host: "127.0.0.1".into(),
            nakama_port: 7350,
            nakama_key: "mello_dev_key".into(),
            nakama_http_key: option_env!("NAKAMA_HTTP_KEY")
                .unwrap_or("mello_http_key_dev")
                .into(),
            nakama_ssl: false,
            google_client_id: option_env!("GOOGLE_CLIENT_ID").map(Into::into),
            google_client_secret: option_env!("GOOGLE_CLIENT_SECRET").map(Into::into),
            discord_client_id: option_env!("DISCORD_CLIENT_ID").map(Into::into),
            twitch_client_id: option_env!("TWITCH_CLIENT_ID").map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn overrides(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn env_bool_accepts_common_spellings() {
        for truthy in ["1", "true", "TRUE", "yes", "On"] {
            assert_eq!(parse_env_bool(truthy), Some(true), "{truthy}");
        }
        for falsy in ["0", "false", "NO", "off"] {
            assert_eq!(parse_env_bool(falsy), Some(false), "{falsy}");
        }
        assert_eq!(parse_env_bool("maybe"), None);
    }

    #[test]
    fn overrides_replace_baked_in_values() {
        let cfg = Config::production().with_overrides_from(overrides(&[
            ("NAKAMA_HOST", "127.0.0.1"),
            ("NAKAMA_PORT", "7350"),
            ("NAKAMA_SSL", "false"),
            ("NAKAMA_HTTP_KEY", "test_http_key"),
            ("NAKAMA_SERVER_KEY", "test_server_key"),
        ]));

        assert_eq!(cfg.nakama_host, "127.0.0.1");
        assert_eq!(cfg.nakama_port, 7350);
        assert!(!cfg.nakama_ssl);
        assert_eq!(cfg.nakama_http_key, "test_http_key");
        assert_eq!(cfg.nakama_key, "test_server_key");
        assert_eq!(cfg.http_base(), "http://127.0.0.1:7350");
    }

    #[test]
    fn absent_overrides_leave_config_untouched() {
        let base = Config::production();
        let cfg = base.clone().with_overrides_from(overrides(&[]));

        assert_eq!(cfg.nakama_host, base.nakama_host);
        assert_eq!(cfg.nakama_port, base.nakama_port);
        assert_eq!(cfg.nakama_ssl, base.nakama_ssl);
        assert_eq!(cfg.nakama_http_key, base.nakama_http_key);
    }

    /// An exported-but-empty variable is the common shape of a misconfigured CI
    /// secret. It must not silently blank out a working key — that is the
    /// failure mode that took production signup down.
    #[test]
    fn empty_overrides_are_ignored() {
        let base = Config::production();
        let cfg = base
            .clone()
            .with_overrides_from(overrides(&[("NAKAMA_HOST", ""), ("NAKAMA_HTTP_KEY", "")]));

        assert_eq!(cfg.nakama_host, base.nakama_host);
        assert_eq!(cfg.nakama_http_key, base.nakama_http_key);
    }

    #[test]
    fn unparseable_port_is_ignored() {
        let cfg = Config::production().with_overrides_from(overrides(&[("NAKAMA_PORT", "banana")]));
        assert_eq!(cfg.nakama_port, Config::production().nakama_port);
    }
}
