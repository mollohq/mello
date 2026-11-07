use serde::{Deserialize, Serialize};

const APP_NAME: &str = "mello";

fn default_share_game_activity() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub capture_device_id: Option<String>,
    pub playback_device_id: Option<String>,
    pub dark_theme: bool,
    pub device_id: Option<String>,
    pub onboarding_step: u8,
    pub last_crew_id: Option<String>,
    pub pending_crew_id: Option<String>,
    pub pending_crew_name: Option<String>,
    pub pending_crew_description: Option<String>,
    pub pending_crew_open: Option<bool>,
    /// The invite code of the link that opened a fresh install (#68).
    /// Finalize joins its crew. Survives a restart on step 2.
    pub pending_invite_code: Option<String>,
    /// The name of the invited crew, shown on step 2.
    pub pending_invite_crew_name: Option<String>,
    /// The resolved invite, shown on the welcome screen. Stored so that a
    /// restart on the welcome screen shows it again with no network call.
    pub pending_invite: Option<mello_core::crew::ResolvedInvite>,
    /// Onboarding started from an invite link and skipped step 1. The step
    /// indicator then counts two steps, not three. Survives a restart.
    pub onboarding_via_invite: bool,
    pub start_on_boot: bool,
    pub ptt_key: Option<String>,
    /// The voice clip key, as `ptt_key` stores a key ("F7", "Ctrl+F7").
    /// `None` turns the key off.
    pub clip_key: Option<String>,
    /// Game clips (CLIPS.md 15.10): the replay of the game while the
    /// user is in voice. On by default.
    pub game_clips_enabled: bool,
    /// "1080p60", "720p60" or "720p30" (`GameClipQuality`).
    pub game_clip_quality: String,
    /// The game clip key, in the format of `clip_key` ("F8").
    pub game_clip_key: Option<String>,
    /// The stream key, in the format of `clip_key` ("F9"). A press starts
    /// a stream of the detected game, or ends the hosted stream. `None`
    /// turns the key off.
    pub stream_key: Option<String>,
    // General tab
    pub start_minimized: bool,
    pub close_to_tray: bool,
    /// The OS showed the notice that a closed window went to the tray and
    /// m3llo still runs. The notice shows on the first close only.
    pub tray_notice_shown: bool,
    pub auto_connect: bool,
    pub minimize_on_join: bool,
    pub hardware_acceleration: bool,
    /// Linux: use Cisco's OpenH264 binary, which the client downloads at the
    /// first start (`platform/linux_openh264.rs`). Cisco's licence asks for
    /// a way to turn it off. Read at startup only.
    pub openh264_enabled: bool,
    // Audio tab
    pub input_volume: f32,
    pub output_volume: f32,
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub echo_suppression: bool,
    pub agc: bool,
    pub input_mode: String, // "voice_activity" or "push_to_talk"
    /// Manual input sensitivity in dBFS (the slider). Applies when
    /// `vad_auto` is off.
    pub vad_threshold: f32,
    /// Automatic input sensitivity: the speech gate tracks the noise floor.
    /// On by default; the slider applies only when it is off.
    pub vad_auto: bool,
    // HUD tab
    pub hud_enabled: bool,
    pub hud_show_overlay_in_game: bool,
    pub hud_overlay_opacity: f32,
    pub hud_show_clip_toasts: bool,
    pub hud_overlay_x: Option<i32>,
    pub hud_overlay_y: Option<i32>,
    pub hud_miniplayer_x: Option<i32>,
    pub hud_miniplayer_y: Option<i32>,
    pub seen_session_ids: Vec<String>,
    /// When the user last opened each crew's feed (ms, by crew id). A head2head
    /// belt that moved after this is unread.
    pub feed_seen_ms: std::collections::HashMap<String, i64>,
    /// The sealed belts and recaps the user opened (`sealed.rs`), by crew id.
    /// A key is `title_id:moved_at_ms` or `recap:<week_start_ms>`.
    pub sealed_opened: std::collections::HashMap<String, Vec<String>>,
    /// The members this user said hi to from a joined card, by crew id. The
    /// card then reads "Said hi" and stays off, once for each member. Local.
    pub said_hi: std::collections::HashMap<String, Vec<String>>,
    /// When the viewer first saw the own chips of a crew sealed (ms, by crew
    /// id). The day 0 reveal lasts 7 days from this time (`sealed.rs`).
    pub own_seal_since: std::collections::HashMap<String, i64>,
    /// The `feed_seen_ms` of a crew as it was when the feed opened. Unread is
    /// judged against this for the whole visit. Not stored.
    #[serde(skip)]
    pub feed_baseline_ms: std::collections::HashMap<String, i64>,
    // Games tab: integrations the user switched off (consent is default-on;
    // absence from this list means enabled).
    pub disabled_game_integrations: Vec<String>,
    /// Installed games the user hides from the crew (game ids). Left out of
    /// the library report. Games hidden in Steam are hidden without an entry.
    pub hidden_games: Vec<String>,
    /// User dismissed the post-game "connect Riot account" CTA; don't re-ask.
    pub riot_prompt_dismissed: bool,
    /// When false, sensed play is kept local — no crew presence or session-end RPCs.
    #[serde(default = "default_share_game_activity")]
    pub share_game_activity: bool,
    // Notification sounds (specs/features/NOTIFICATION-SOUNDS.md §7).
    /// The volume of the notification sounds, 0..1.
    pub sound_volume: f32,
    pub sound_voice: bool,
    pub sound_streams: bool,
    pub sound_chat: bool,
    pub sound_clips: bool,
    pub sound_crew: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            capture_device_id: None,
            playback_device_id: None,
            dark_theme: true,
            device_id: None,
            onboarding_step: 0,
            last_crew_id: None,
            pending_crew_id: None,
            pending_crew_name: None,
            pending_crew_description: None,
            pending_crew_open: None,
            pending_invite_code: None,
            pending_invite_crew_name: None,
            pending_invite: None,
            onboarding_via_invite: false,
            start_on_boot: true,
            ptt_key: None,
            clip_key: Some(crate::platform::hotkeys::DEFAULT_CLIP_KEY.to_string()),
            game_clips_enabled: true,
            game_clip_quality: mello_core::events::GameClipQuality::default()
                .as_str()
                .to_string(),
            game_clip_key: Some(crate::platform::hotkeys::DEFAULT_GAME_CLIP_KEY.to_string()),
            stream_key: Some(crate::platform::hotkeys::DEFAULT_STREAM_KEY.to_string()),
            start_minimized: false,
            close_to_tray: true,
            tray_notice_shown: false,
            auto_connect: false,
            minimize_on_join: false,
            hardware_acceleration: true,
            openh264_enabled: true,
            input_volume: 1.0,
            output_volume: 1.0,
            noise_suppression: true,
            echo_cancellation: true,
            echo_suppression: false,
            agc: true,
            input_mode: "voice_activity".into(),
            vad_threshold: -40.0,
            vad_auto: true,
            hud_enabled: true,
            hud_show_overlay_in_game: true,
            hud_overlay_opacity: 0.8,
            hud_show_clip_toasts: true,
            hud_overlay_x: None,
            hud_overlay_y: None,
            hud_miniplayer_x: None,
            hud_miniplayer_y: None,
            seen_session_ids: Vec::new(),
            feed_seen_ms: Default::default(),
            sealed_opened: Default::default(),
            said_hi: Default::default(),
            own_seal_since: Default::default(),
            feed_baseline_ms: Default::default(),
            disabled_game_integrations: Vec::new(),
            hidden_games: Vec::new(),
            riot_prompt_dismissed: false,
            share_game_activity: default_share_game_activity(),
            sound_volume: 0.8,
            sound_voice: true,
            sound_streams: true,
            sound_chat: true,
            sound_clips: true,
            sound_crew: true,
        }
    }
}

/// Environment variable redirecting settings storage to a specific directory.
///
/// Onboarding calls [`Settings::save`] on nearly every step transition, so any
/// test that drives onboarding would otherwise rewrite the developer's (or CI
/// runner's) real config. Tests point this at a temp dir.
pub const CONFIG_DIR_ENV: &str = "MELLO_CONFIG_DIR";

impl Settings {
    /// Settings file path for a given config-dir override value.
    ///
    /// Split from the environment lookup so the path composition is testable
    /// without mutating process-global env state.
    fn override_path(dir: Option<String>) -> Option<std::path::PathBuf> {
        dir.filter(|v| !v.is_empty())
            .map(|dir| std::path::PathBuf::from(dir).join(format!("{APP_NAME}.toml")))
    }

    /// `None` means "use confy's platform default location".
    fn configured_path() -> Option<std::path::PathBuf> {
        Self::override_path(std::env::var(CONFIG_DIR_ENV).ok())
    }

    pub fn load() -> Self {
        let loaded = match Self::configured_path() {
            Some(path) => confy::load_path::<Settings>(&path),
            None => confy::load::<Settings>(APP_NAME, None),
        };

        match loaded {
            Ok(s) => {
                log::info!("Settings loaded");
                s
            }
            Err(e) => {
                log::warn!("Failed to load settings, using defaults: {}", e);
                Self::default()
            }
        }
    }

    /// The command that applies the saved input sensitivity to libmello.
    /// Sent at startup, on every change and on reset (spec 10 section 8).
    pub fn input_sensitivity_command(&self) -> mello_core::Command {
        mello_core::Command::SetInputSensitivity {
            auto: self.vad_auto,
            db: self.vad_threshold,
        }
    }

    /// The command that applies the "Game clips" section to core. Sent at
    /// startup, on every change and on reset. An unknown quality in the file
    /// falls back to the default.
    pub fn game_clips_command(&self) -> mello_core::Command {
        mello_core::Command::SetGameClips {
            enabled: self.game_clips_enabled,
            quality: mello_core::events::GameClipQuality::parse(&self.game_clip_quality)
                .unwrap_or_default(),
        }
    }

    pub fn save(&self) {
        let stored = match Self::configured_path() {
            Some(path) => confy::store_path(&path, self),
            None => confy::store(APP_NAME, None, self),
        };

        if let Err(e) = stored {
            log::warn!("Failed to save settings: {}", e);
        }
    }

    /// The stable device identity for Nakama device auth, creating and
    /// persisting one on first use.
    ///
    /// This must be stable across attempts and across restarts. Onboarding used
    /// to mint a fresh random id inside every `FinalizeOnboarding`, and
    /// `device_id` was never written, so each attempt authenticated as a *new*
    /// device and Nakama duly created a *new* account (`create=true`). Any
    /// retry — or any restart before onboarding was marked complete — left
    /// another orphan user behind. Production users accumulated five or six
    /// accounts each this way.
    ///
    /// Reusing the id makes finalize idempotent: a second attempt authenticates
    /// back into the same account instead of creating another.
    /// Does this machine have a device account?
    ///
    /// The signal is a persisted `device_id`. Onboarding writes it when the
    /// user continues from step 2, just before the first device auth creates
    /// the account. A fresh install, or a user who only ever signed in with
    /// an existing account, has none.
    pub fn has_device_account(&self) -> bool {
        self.device_id.as_deref().is_some_and(|id| !id.is_empty())
    }

    pub fn device_id_or_create(&mut self) -> String {
        if let Some(existing) = self.device_id.as_ref().filter(|id| !id.is_empty()) {
            return existing.clone();
        }

        use rand::Rng;
        let bytes: [u8; 16] = rand::thread_rng().gen();
        let id: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

        self.device_id = Some(id.clone());
        self.save();
        log::info!("[auth] generated device id for this install");
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Input sensitivity starts automatic, as libmello does. A settings file
    /// saved before the auto toggle existed loads with auto on, so an
    /// upgrade keeps today's floor-tracking gate and not a -40 dBFS gate.
    #[test]
    fn input_sensitivity_defaults_to_auto_also_for_old_settings_files() {
        let s = Settings::default();
        assert!(s.vad_auto);
        assert!(matches!(
            s.input_sensitivity_command(),
            mello_core::Command::SetInputSensitivity { auto: true, db } if db == -40.0
        ));

        let old: Settings = toml::from_str(
            "vad_threshold = -25.0
",
        )
        .expect("old file parses");
        assert!(old.vad_auto);
        assert_eq!(old.vad_threshold, -25.0);
    }

    /// A settings file from before game clips loads with them on at 720p60,
    /// on F8. A quality the client does not know falls back to the default.
    #[test]
    fn game_clips_default_on_at_720p60_also_for_old_settings_files() {
        let old: Settings = toml::from_str("dark_theme = false\n").expect("old file parses");
        assert!(old.game_clips_enabled);
        assert_eq!(old.game_clip_key.as_deref(), Some("F8"));
        assert!(matches!(
            old.game_clips_command(),
            mello_core::Command::SetGameClips {
                enabled: true,
                quality: mello_core::events::GameClipQuality::P720Fps60,
            }
        ));

        let odd = Settings {
            game_clips_enabled: false,
            game_clip_quality: "8k".into(),
            ..Default::default()
        };
        assert!(matches!(
            odd.game_clips_command(),
            mello_core::Command::SetGameClips {
                enabled: false,
                quality: mello_core::events::GameClipQuality::P720Fps60,
            }
        ));
    }

    /// A settings file from before the stream key loads with the key on F9.
    #[test]
    fn the_stream_key_defaults_to_f9_also_for_old_settings_files() {
        assert_eq!(Settings::default().stream_key.as_deref(), Some("F9"));
        let old: Settings = toml::from_str("dark_theme = false\n").expect("old file parses");
        assert_eq!(old.stream_key.as_deref(), Some("F9"));
    }

    #[test]
    fn settings_default_values() {
        let s = Settings::default();
        assert!(s.capture_device_id.is_none());
        assert!(s.playback_device_id.is_none());
        assert!(s.dark_theme);
        assert!(s.share_game_activity);
    }

    /// An empty id is no account: `device_id_or_create` treats it the same.
    #[test]
    fn has_device_account_reads_a_non_empty_device_id() {
        let mut s = Settings::default();
        assert!(!s.has_device_account(), "a fresh install has none");
        s.device_id = Some(String::new());
        assert!(!s.has_device_account(), "an empty id is not an account");
        s.device_id = Some("dev-abc".into());
        assert!(s.has_device_account());
    }

    #[test]
    fn settings_missing_share_game_activity_defaults_true() {
        let partial = r#"dark_theme = false"#;
        let decoded: Settings = toml::from_str(partial).expect("deserialize");
        assert!(decoded.share_game_activity);
    }

    /// Cisco's OpenH264 is on unless the user turned it off, also after an
    /// update from a version without the setting.
    #[test]
    fn openh264_is_on_by_default_also_for_old_settings_files() {
        assert!(Settings::default().openh264_enabled);
        let old: Settings = toml::from_str("hardware_acceleration = true").expect("deserialize");
        assert!(old.openh264_enabled);
        let off: Settings = toml::from_str("openh264_enabled = false").expect("deserialize");
        assert!(!off.openh264_enabled);
    }

    #[test]
    fn config_dir_override_composes_settings_path() {
        let path = Settings::override_path(Some("/tmp/mello-test".into()))
            .expect("an override dir should yield a path");
        assert_eq!(path, std::path::PathBuf::from("/tmp/mello-test/mello.toml"));
    }

    /// Unset or empty must fall through to confy's platform default rather than
    /// resolving to a bare relative filename in the process's cwd.
    #[test]
    fn absent_or_empty_config_dir_uses_platform_default() {
        assert!(Settings::override_path(None).is_none());
        assert!(Settings::override_path(Some(String::new())).is_none());
    }

    /// Settings must survive a real write/read cycle through the override path.
    /// Onboarding saves on nearly every step transition, so this is the
    /// mechanism that keeps tests off the developer's real config file.
    #[test]
    fn settings_roundtrip_through_override_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = Settings::override_path(Some(dir.path().to_string_lossy().into_owned()))
            .expect("override path");

        let saved = Settings {
            onboarding_step: 3,
            pending_crew_id: Some("crew-42".into()),
            dark_theme: false,
            ..Default::default()
        };
        confy::store_path(&path, &saved).expect("store");

        assert!(
            path.exists(),
            "settings file should exist at the override path"
        );

        let loaded: Settings = confy::load_path(&path).expect("load");
        assert_eq!(loaded.onboarding_step, 3);
        assert_eq!(loaded.pending_crew_id.as_deref(), Some("crew-42"));
        assert!(!loaded.dark_theme);
    }

    #[test]
    fn settings_toml_roundtrip() {
        let s = Settings {
            capture_device_id: Some("mic_123".into()),
            playback_device_id: Some("spk_456".into()),
            dark_theme: false,
            device_id: Some("dev-abc".into()),
            onboarding_step: 4,
            last_crew_id: None,
            ..Default::default()
        };
        let toml_str = toml::to_string(&s).unwrap();
        let decoded: Settings = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.capture_device_id.as_deref(), Some("mic_123"));
        assert_eq!(decoded.playback_device_id.as_deref(), Some("spk_456"));
        assert!(!decoded.dark_theme);
    }

    /// The welcome screen reads the invite from Settings after a restart. It
    /// must survive the TOML file, members and inviter included.
    #[test]
    fn a_pending_invite_survives_the_settings_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = Settings::override_path(Some(dir.path().to_string_lossy().into_owned()))
            .expect("override path");
        let person = |n: &str| mello_core::crew::InvitePerson {
            display_name: n.into(),
            avatar_seed: n.into(),
        };
        let saved = Settings {
            onboarding_step: 5,
            onboarding_via_invite: true,
            pending_invite_code: Some("NITE-0001".into()),
            pending_invite: Some(mello_core::crew::ResolvedInvite {
                crew_name: "Night Owls".into(),
                avatar_seed: "Night Owls".into(),
                crew_id: "c1".into(),
                highlight: "7h hangout".into(),
                member_count: 4,
                members: vec![person("alice"), person("bo")],
                inviter: Some(person("alice")),
            }),
            ..Default::default()
        };
        confy::store_path(&path, &saved).expect("store");
        let loaded: Settings = confy::load_path(&path).expect("load");

        let invite = loaded.pending_invite.expect("the invite is stored");
        assert_eq!(invite.crew_name, "Night Owls");
        assert_eq!(invite.member_count, 4);
        assert_eq!(invite.members, vec![person("alice"), person("bo")]);
        assert_eq!(invite.inviter, Some(person("alice")));
        assert!(loaded.onboarding_via_invite);
    }

    #[test]
    fn settings_missing_fields_use_defaults() {
        let partial = r#"dark_theme = false"#;
        let decoded: Settings = toml::from_str(partial).unwrap();
        assert!(decoded.capture_device_id.is_none());
        assert!(decoded.playback_device_id.is_none());
        assert!(!decoded.dark_theme);
    }

    #[test]
    fn settings_clear_stale_device_roundtrip() {
        let mut s = Settings {
            capture_device_id: Some("{0.0.1.00000000}.{stale-guid}".into()),
            playback_device_id: Some("{0.0.0.00000000}.{stale-guid}".into()),
            ..Default::default()
        };
        assert!(s.capture_device_id.is_some());
        assert!(s.playback_device_id.is_some());

        // Simulate what the AudioDeviceFallback handler does
        s.capture_device_id = None;
        s.playback_device_id = None;

        let toml_str = toml::to_string(&s).unwrap();
        let decoded: Settings = toml::from_str(&toml_str).unwrap();
        assert!(
            decoded.capture_device_id.is_none(),
            "cleared capture device should not persist"
        );
        assert!(
            decoded.playback_device_id.is_none(),
            "cleared playback device should not persist"
        );
    }
}
