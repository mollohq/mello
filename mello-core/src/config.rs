use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub nakama_host: String,
    pub nakama_port: u16,
    pub nakama_key: String,
    pub nakama_ssl: bool,
}

impl Config {
    pub fn production() -> Self {
        Self {
            nakama_host: "mello-api-1iiv.onrender.com".into(),
            nakama_port: 443,
            nakama_key: option_env!("NAKAMA_SERVER_KEY").unwrap_or("defaultkey").into(),
            nakama_ssl: true,
        }
    }

    pub fn development() -> Self {
        Self::default()
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
            nakama_ssl: false,
        }
    }
}
