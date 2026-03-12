use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    Online,
    Idle,
    Dnd,
    Offline,
}

impl Default for PresenceStatus {
    fn default() -> Self {
        Self::Offline
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Activity {
    None,
    InVoice {
        #[serde(default)]
        crew_id: String,
    },
    Streaming {
        #[serde(default)]
        crew_id: String,
        #[serde(default)]
        stream_id: String,
        #[serde(default)]
        stream_title: String,
    },
    Watching {
        #[serde(default)]
        crew_id: String,
        #[serde(default)]
        stream_id: String,
        #[serde(default)]
        streamer_id: String,
    },
}

impl Default for Activity {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserPresence {
    pub user_id: String,
    pub status: PresenceStatus,
    #[serde(default)]
    pub last_seen: Option<String>,
    #[serde(default)]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serde_roundtrip() {
        for (status, expected_json) in [
            (PresenceStatus::Online, "\"online\""),
            (PresenceStatus::Idle, "\"idle\""),
            (PresenceStatus::Dnd, "\"dnd\""),
            (PresenceStatus::Offline, "\"offline\""),
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected_json);
            let parsed: PresenceStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn activity_none_serde() {
        let json = serde_json::to_string(&Activity::None).unwrap();
        assert!(json.contains("\"type\":\"none\""));
        let parsed: Activity = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Activity::None));
    }

    #[test]
    fn activity_in_voice_serde() {
        let a = Activity::InVoice {
            crew_id: "crew_xyz".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"type\":\"in_voice\""));
        assert!(json.contains("\"crew_id\":\"crew_xyz\""));
        let parsed: Activity = serde_json::from_str(&json).unwrap();
        match parsed {
            Activity::InVoice { crew_id } => assert_eq!(crew_id, "crew_xyz"),
            _ => panic!("expected InVoice"),
        }
    }

    #[test]
    fn activity_streaming_serde() {
        let a = Activity::Streaming {
            crew_id: "c1".into(),
            stream_id: "s1".into(),
            stream_title: "PROJECT AVALON".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let parsed: Activity = serde_json::from_str(&json).unwrap();
        match parsed {
            Activity::Streaming {
                crew_id,
                stream_id,
                stream_title,
            } => {
                assert_eq!(crew_id, "c1");
                assert_eq!(stream_id, "s1");
                assert_eq!(stream_title, "PROJECT AVALON");
            }
            _ => panic!("expected Streaming"),
        }
    }

    #[test]
    fn user_presence_from_server_json() {
        let json = r#"{
            "user_id": "user_abc",
            "status": "online",
            "last_seen": "2026-03-08T14:15:00Z",
            "activity": {
                "type": "streaming",
                "crew_id": "crew_xyz",
                "stream_id": "stream_123",
                "stream_title": "PROJECT AVALON"
            },
            "updated_at": "2026-03-08T14:16:00Z"
        }"#;

        let p: UserPresence = serde_json::from_str(json).unwrap();
        assert_eq!(p.user_id, "user_abc");
        assert_eq!(p.status, PresenceStatus::Online);
        assert_eq!(p.last_seen.as_deref(), Some("2026-03-08T14:15:00Z"));
        assert!(matches!(p.activity, Some(Activity::Streaming { .. })));
    }

    #[test]
    fn user_presence_minimal_json() {
        // Server may return just user_id + status
        let json = r#"{"user_id": "u1", "status": "offline"}"#;
        let p: UserPresence = serde_json::from_str(json).unwrap();
        assert_eq!(p.user_id, "u1");
        assert_eq!(p.status, PresenceStatus::Offline);
        assert!(p.activity.is_none());
        assert!(p.last_seen.is_none());
    }

    #[test]
    fn status_default_is_offline() {
        assert_eq!(PresenceStatus::default(), PresenceStatus::Offline);
    }

    #[test]
    fn activity_default_is_none() {
        assert!(matches!(Activity::default(), Activity::None));
    }
}
