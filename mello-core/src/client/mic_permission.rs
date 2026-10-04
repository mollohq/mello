//! Microphone permission for `CheckMicPermission` and `RequestMicPermission`.
//!
//! The core asks the OS through libmello. A test build (`e2e-mic` feature)
//! can fix the answer with `MELLO_E2E_MIC_PERMISSION`, so that an e2e journey
//! does not depend on the decision that macOS keeps for the app that started
//! it, and never opens the OS dialog (plans/E2E-QA.md §16.7). Without the
//! variable, a test build asks the OS too.

use crate::events::Event;

/// The variable that fixes the permission in a test build.
#[cfg(any(feature = "e2e-mic", test))]
pub(crate) const ENV: &str = "MELLO_E2E_MIC_PERMISSION";

/// The microphone permission, as the OS reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MicPermission {
    Granted,
    Denied,
    /// The user was not asked yet.
    Undetermined,
}

impl MicPermission {
    /// The event that tells the UI about this permission.
    pub(crate) fn event(self) -> Event {
        Event::MicPermissionChanged {
            granted: self == Self::Granted,
            denied: self == Self::Denied,
        }
    }

    /// Parse a value of `MELLO_E2E_MIC_PERMISSION`.
    #[cfg(any(feature = "e2e-mic", test))]
    fn parse(value: &str) -> Option<Self> {
        match value {
            "granted" => Some(Self::Granted),
            "denied" => Some(Self::Denied),
            "undetermined" => Some(Self::Undetermined),
            _ => None,
        }
    }

    /// The permission that libmello reads from the OS.
    pub(crate) fn from_os() -> Self {
        let status = unsafe { mello_sys::mello_mic_permission_status() };
        if status == mello_sys::MelloMicPermission_MELLO_MIC_GRANTED {
            Self::Granted
        } else if status == mello_sys::MelloMicPermission_MELLO_MIC_DENIED {
            Self::Denied
        } else {
            Self::Undetermined
        }
    }
}

/// Where the core gets the permission from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MicPermissionSource {
    /// Ask the OS. The only source in a release build.
    Os,
    /// Test builds only: a fixed permission from `MELLO_E2E_MIC_PERMISSION`.
    /// A request changes it as a user who answers the OS dialog does.
    #[cfg(any(feature = "e2e-mic", test))]
    Fixed(MicPermission),
}

impl MicPermissionSource {
    /// The source for this process: `Os`, or in a test build the value of
    /// `MELLO_E2E_MIC_PERMISSION` when it is set.
    pub(crate) fn from_env() -> Self {
        #[cfg(feature = "e2e-mic")]
        {
            let source = Self::from_value(std::env::var(ENV).ok().as_deref());
            if let Self::Fixed(p) = source {
                log::info!("[e2e] mic permission fixed to {p:?} by {ENV}; the OS is not asked");
            }
            source
        }
        #[cfg(not(feature = "e2e-mic"))]
        Self::Os
    }

    /// The source for a value of `MELLO_E2E_MIC_PERMISSION`. No value or an
    /// empty value keeps the OS. An unknown value keeps the OS and logs an
    /// error, so a typo in a journey shows in the app log.
    #[cfg(any(feature = "e2e-mic", test))]
    pub(crate) fn from_value(value: Option<&str>) -> Self {
        match value.filter(|v| !v.is_empty()) {
            None => Self::Os,
            Some(v) => match MicPermission::parse(v) {
                Some(p) => Self::Fixed(p),
                None => {
                    log::error!(
                        "[e2e] {ENV}={v:?} is not granted, denied or undetermined; asking the OS"
                    );
                    Self::Os
                }
            },
        }
    }

    /// `CheckMicPermission`: the current permission. `os` reads it from the
    /// OS, and only the `Os` source calls it.
    pub(crate) fn check(&self, os: impl FnOnce() -> MicPermission) -> MicPermission {
        match self {
            Self::Os => os(),
            #[cfg(any(feature = "e2e-mic", test))]
            Self::Fixed(p) => *p,
        }
    }

    /// `RequestMicPermission`. The `Os` source calls `os`, which asks the OS
    /// and sends its answer later, and returns `None`. A fixed source answers
    /// at once, as a user who presses Allow: `Undetermined` becomes
    /// `Granted`, and `Denied` stays `Denied` (macOS does not ask again). It
    /// keeps the answer, so a later check agrees.
    pub(crate) fn request(&mut self, os: impl FnOnce()) -> Option<MicPermission> {
        match self {
            Self::Os => {
                os();
                None
            }
            #[cfg(any(feature = "e2e-mic", test))]
            Self::Fixed(p) => {
                if *p == MicPermission::Undetermined {
                    *p = MicPermission::Granted;
                }
                Some(*p)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use MicPermission::{Denied, Granted, Undetermined};

    #[test]
    fn the_variable_selects_a_fixed_permission() {
        assert_eq!(
            MicPermissionSource::from_value(Some("granted")),
            MicPermissionSource::Fixed(Granted)
        );
        assert_eq!(
            MicPermissionSource::from_value(Some("denied")),
            MicPermissionSource::Fixed(Denied)
        );
        assert_eq!(
            MicPermissionSource::from_value(Some("undetermined")),
            MicPermissionSource::Fixed(Undetermined)
        );
    }

    #[test]
    fn no_variable_an_empty_one_or_a_typo_asks_the_os() {
        for value in [None, Some(""), Some("Granted"), Some("allow")] {
            assert_eq!(
                MicPermissionSource::from_value(value),
                MicPermissionSource::Os,
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_os_source_asks_the_os_to_check_and_to_request() {
        let mut source = MicPermissionSource::from_value(None);
        for os_answer in [Granted, Denied, Undetermined] {
            let asked = Cell::new(false);
            let p = source.check(|| {
                asked.set(true);
                os_answer
            });
            assert!(asked.get(), "check must ask the OS");
            assert_eq!(p, os_answer);
        }
        let asked = Cell::new(false);
        assert_eq!(source.request(|| asked.set(true)), None);
        assert!(asked.get(), "request must ask the OS");
        assert_eq!(source, MicPermissionSource::Os);
    }

    /// For each value: what a check reports, what a request answers, and what
    /// a check after the request reports. The OS is never asked.
    #[test]
    fn a_fixed_source_never_asks_the_os_and_a_request_acts_as_allow() {
        for (value, before, after) in [
            ("granted", Granted, Granted),
            ("denied", Denied, Denied),
            ("undetermined", Undetermined, Granted),
        ] {
            let mut source = MicPermissionSource::from_value(Some(value));
            let no_os = || -> MicPermission { panic!("{value}: check asked the OS") };
            assert_eq!(source.check(no_os), before, "{value}: check");
            let answer = source.request(|| panic!("{value}: request asked the OS"));
            assert_eq!(answer, Some(after), "{value}: request");
            assert_eq!(source.check(no_os), after, "{value}: check after request");
        }
    }

    /// The client loop answers both commands from the source. An undetermined
    /// user who presses "ALLOW MICROPHONE" gets the Mute button.
    #[tokio::test]
    async fn the_client_loop_answers_the_commands_from_a_fixed_source() {
        use crate::command::Command;
        use std::sync::{mpsc, Arc};

        let (event_tx, events) = mpsc::channel();
        let mut client = super::super::Client::with_voice(
            crate::config::Config::default(),
            event_tx.clone(),
            crate::voice::VoiceManager::without_audio(event_tx),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
            false,
            false,
        );
        client.mic_permission = MicPermissionSource::from_value(Some("undetermined"));
        async fn answer(
            client: &mut super::super::Client,
            events: &mpsc::Receiver<Event>,
            cmd: Command,
        ) -> Vec<(bool, bool)> {
            client.handle_command(cmd).await;
            events
                .try_iter()
                .filter_map(|e| match e {
                    Event::MicPermissionChanged { granted, denied } => Some((granted, denied)),
                    _ => None,
                })
                .collect()
        }
        assert_eq!(
            answer(&mut client, &events, Command::CheckMicPermission).await,
            [(false, false)]
        );
        assert_eq!(
            answer(&mut client, &events, Command::RequestMicPermission).await,
            [(true, false)]
        );
        assert_eq!(
            answer(&mut client, &events, Command::CheckMicPermission).await,
            [(true, false)]
        );
    }

    /// A build without the `e2e-mic` feature (every shipped build) ignores
    /// the variable. Only this test sets it, and only without the feature,
    /// so no other test sees it.
    #[cfg(not(feature = "e2e-mic"))]
    #[test]
    fn a_build_without_the_feature_ignores_the_variable() {
        std::env::set_var(ENV, "granted");
        let source = MicPermissionSource::from_env();
        std::env::remove_var(ENV);
        assert_eq!(source, MicPermissionSource::Os);
    }

    #[test]
    fn the_event_carries_granted_and_denied() {
        let flags = |p: MicPermission| match p.event() {
            Event::MicPermissionChanged { granted, denied } => (granted, denied),
            other => panic!("unexpected event {other:?}"),
        };
        assert_eq!(flags(Granted), (true, false));
        assert_eq!(flags(Denied), (false, true));
        assert_eq!(flags(Undetermined), (false, false));
    }
}
