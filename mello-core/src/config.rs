//! Client configuration

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub nakama_url: String,
    pub nakama_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nakama_url: "http://localhost:7350".into(),
            nakama_key: "defaultkey".into(),
        }
    }
}
