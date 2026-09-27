//! Browser sign-in and link flows, run off the command loop.
//!
//! A browser flow waits for the user in the browser, for up to 120 s. The
//! command loop must not wait with it (#88): voice ticks and every other
//! command stop while the loop waits. [`BrowserFlows::start`] runs the flow on
//! a blocking thread and returns at once. The loop polls
//! [`BrowserFlows::finished`] in its `select!` and then completes the sign-in
//! or link with the result.
//!
//! The callback server has one fixed port, so one flow can wait at a time. A
//! start request while a flow waits is ignored with a log line. The waiting
//! flow continues. A cancelled flow cannot give its port to a new flow at
//! once: the old listener closes on a thread that the app cannot join.

use tokio::task::{JoinError, JoinHandle};

use crate::auth_discord::DiscordAuth;
use crate::auth_google::GoogleAuth;
use crate::auth_steam::SteamAuth;
use crate::auth_twitch::TwitchAuth;
use crate::config::Config;
use crate::oauth::{FlowCancel, OAuthError};

/// A provider that signs in through the browser on desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocialProvider {
    Google,
    Discord,
    Twitch,
    Steam,
}

impl SocialProvider {
    /// The name that the user reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Discord => "Discord",
            Self::Twitch => "Twitch",
            Self::Steam => "Steam",
        }
    }

    /// The provider id for Nakama's custom authentication. Google uses its
    /// own endpoint and has none.
    pub fn custom_id(self) -> Option<&'static str> {
        match self {
            Self::Google => None,
            Self::Discord => Some("discord"),
            Self::Twitch => Some("twitch"),
            Self::Steam => Some("steam"),
        }
    }

    /// The OAuth client id, or `None` when this build has none configured.
    /// OpenID 2.0 has no client id, so Steam gives an empty value.
    pub fn client_id(self, config: &Config) -> Option<String> {
        match self {
            Self::Google => config.google_client_id.clone(),
            Self::Discord => config.discord_client_id.clone(),
            Self::Twitch => config.twitch_client_id.clone(),
            Self::Steam => Some(String::new()),
        }
    }

    /// Run the provider's browser flow. Blocking.
    pub fn run(self, client_id: &str, cancel: &FlowCancel) -> FlowResult {
        match self {
            Self::Google => GoogleAuth::authenticate(client_id, cancel)
                .map(|(code, verifier)| Credential::GoogleCode { code, verifier }),
            Self::Discord => DiscordAuth::authenticate(client_id, cancel).map(Credential::Token),
            Self::Twitch => TwitchAuth::authenticate(client_id, cancel).map(Credential::Token),
            Self::Steam => SteamAuth::authenticate(cancel).map(Credential::Token),
        }
    }
}

/// What the app does with the identity when the flow ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowIntent {
    /// Sign in to (or create) the account of this identity.
    SignIn,
    /// Attach this identity to the current account.
    Link,
}

impl FlowIntent {
    fn describe(self) -> &'static str {
        match self {
            Self::SignIn => "sign-in",
            Self::Link => "link",
        }
    }
}

/// What a successful browser flow returns.
#[derive(Debug, PartialEq, Eq)]
pub enum Credential {
    /// Google: the authorization code and the PKCE verifier to exchange it.
    GoogleCode { code: String, verifier: String },
    /// A Discord or Twitch access token, or the Steam OpenID response.
    Token(String),
}

/// What the flow thread returns.
pub type FlowResult = Result<Credential, OAuthError>;

/// A flow that ended. `result` is `Err` when the flow thread panicked.
#[derive(Debug)]
pub struct FlowOutcome {
    pub provider: SocialProvider,
    pub intent: FlowIntent,
    pub result: Result<FlowResult, JoinError>,
}

struct Pending {
    provider: SocialProvider,
    intent: FlowIntent,
    cancel: FlowCancel,
    task: JoinHandle<FlowResult>,
}

/// The browser flow that waits now, if any. Owned by the command loop.
#[derive(Default)]
pub struct BrowserFlows {
    pending: Option<Pending>,
}

impl BrowserFlows {
    /// Start `run` on a blocking thread and return at once. `run` gets the
    /// flow's cancel handle.
    ///
    /// While a flow waits, a new request is ignored and this returns `false`.
    /// Must be called inside a Tokio runtime.
    pub fn start<F>(&mut self, provider: SocialProvider, intent: FlowIntent, run: F) -> bool
    where
        F: FnOnce(&FlowCancel) -> FlowResult + Send + 'static,
    {
        if let Some(waiting) = &self.pending {
            log::info!(
                "[auth] {} {} ignored: the {} {} still waits for the browser",
                provider.label(),
                intent.describe(),
                waiting.provider.label(),
                waiting.intent.describe(),
            );
            return false;
        }

        let cancel = FlowCancel::default();
        let flow_cancel = cancel.clone();
        let task = tokio::task::spawn_blocking(move || run(&flow_cancel));
        log::info!(
            "[auth] {} {} waits for the browser, off the command loop",
            provider.label(),
            intent.describe(),
        );
        self.pending = Some(Pending {
            provider,
            intent,
            cancel,
            task,
        });
        true
    }

    /// True while a flow waits.
    #[cfg(test)]
    pub fn is_waiting(&self) -> bool {
        self.pending.is_some()
    }

    /// Stop the waiting flow, if any. Its outcome still arrives through
    /// [`BrowserFlows::finished`], always as [`OAuthError::Aborted`]. Until
    /// then, the flow holds the port, and new start requests are ignored.
    pub fn cancel(&self) {
        if let Some(waiting) = &self.pending {
            log::info!(
                "[auth] stopping the {} {}",
                waiting.provider.label(),
                waiting.intent.describe(),
            );
            waiting.cancel.cancel();
        }
    }

    /// Wait until the waiting flow ends. With no flow, this never completes,
    /// so it is safe as a `select!` branch.
    ///
    /// Cancel safe: if the `select!` takes another branch, the flow continues
    /// and a later call returns its outcome.
    pub async fn finished(&mut self) -> FlowOutcome {
        let Some(waiting) = self.pending.as_mut() else {
            return std::future::pending().await;
        };
        let mut result = (&mut waiting.task).await;
        let Pending {
            provider,
            intent,
            cancel,
            ..
        } = self
            .pending
            .take()
            .expect("the pending flow is present until its outcome is taken");
        // The flow can end with an identity just before the cancel. The app
        // stopped it, so the identity must not be used.
        if cancel.is_cancelled() && !matches!(result, Ok(Err(OAuthError::Aborted))) {
            log::info!(
                "[auth] discarding the result of the stopped {} {}",
                provider.label(),
                intent.describe(),
            );
            result = Ok(Err(OAuthError::Aborted));
        }
        FlowOutcome {
            provider,
            intent,
            result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const LIMIT: Duration = Duration::from_secs(10);

    /// A flow that waits until the test sends it a result, like a user who
    /// has not finished in the browser yet.
    fn gated_flow() -> (
        mpsc::Sender<FlowResult>,
        impl FnOnce(&FlowCancel) -> FlowResult + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        (tx, move |_: &FlowCancel| {
            rx.recv_timeout(LIMIT).expect("the test sends a result")
        })
    }

    #[tokio::test]
    async fn start_returns_while_the_flow_still_waits() {
        // #88: the command handler returns while the user is in the browser,
        // so the loop can handle the next command.
        let mut flows = BrowserFlows::default();
        let (release, flow) = gated_flow();

        assert!(flows.start(SocialProvider::Discord, FlowIntent::Link, flow));
        assert!(flows.is_waiting());

        // The flow has not ended: `finished` does not complete yet.
        let early = tokio::time::timeout(Duration::from_millis(50), flows.finished()).await;
        assert!(early.is_err(), "the flow ended before the browser answered");
        assert!(flows.is_waiting(), "a timed-out poll must keep the flow");

        release
            .send(Ok(Credential::Token("tok".into())))
            .expect("flow thread listens");
        let outcome = tokio::time::timeout(LIMIT, flows.finished())
            .await
            .expect("the outcome arrives");
        assert_eq!(outcome.provider, SocialProvider::Discord);
        assert_eq!(outcome.intent, FlowIntent::Link);
        assert_eq!(
            outcome.result.expect("no panic").expect("flow ok"),
            Credential::Token("tok".into())
        );
        assert!(!flows.is_waiting());
    }

    #[tokio::test]
    async fn a_second_start_while_one_waits_is_ignored() {
        let mut flows = BrowserFlows::default();
        let (release, first) = gated_flow();
        assert!(flows.start(SocialProvider::Google, FlowIntent::SignIn, first));

        let second_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&second_ran);
        let started = flows.start(SocialProvider::Twitch, FlowIntent::Link, move |_| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Credential::Token("second".into()))
        });
        assert!(!started, "the second request must be ignored");

        // The first flow is the one that reports.
        release
            .send(Ok(Credential::Token("first".into())))
            .expect("flow thread listens");
        let outcome = tokio::time::timeout(LIMIT, flows.finished())
            .await
            .expect("the outcome arrives");
        assert_eq!(outcome.provider, SocialProvider::Google);
        assert_eq!(outcome.intent, FlowIntent::SignIn);
        assert_eq!(
            outcome.result.expect("no panic").expect("flow ok"),
            Credential::Token("first".into())
        );
        assert!(!second_ran.load(std::sync::atomic::Ordering::SeqCst));

        // With no flow waiting, a new request starts.
        assert!(flows.start(SocialProvider::Twitch, FlowIntent::Link, |_| {
            Ok(Credential::Token("third".into()))
        }));
    }

    #[tokio::test]
    async fn cancel_reaches_the_flow_and_its_outcome_still_arrives() {
        let mut flows = BrowserFlows::default();
        flows.start(SocialProvider::Steam, FlowIntent::Link, |cancel| {
            // Stands in for the callback server's wait, which `cancel` wakes.
            let deadline = std::time::Instant::now() + LIMIT;
            while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(OAuthError::Aborted)
        });

        flows.cancel();
        let outcome = tokio::time::timeout(LIMIT, flows.finished())
            .await
            .expect("the outcome arrives");
        assert!(matches!(outcome.result, Ok(Err(OAuthError::Aborted))));
        assert!(!flows.is_waiting());
    }

    #[tokio::test]
    async fn an_identity_that_arrives_before_the_cancel_is_discarded() {
        // Logout cancels the flow. A callback that arrived just before must
        // not sign in or link after the logout.
        let mut flows = BrowserFlows::default();
        let (done_tx, done_rx) = mpsc::channel();
        flows.start(SocialProvider::Twitch, FlowIntent::Link, move |_| {
            let _ = done_tx.send(());
            Ok(Credential::Token("tok".into()))
        });
        done_rx.recv_timeout(LIMIT).expect("the flow ran");

        flows.cancel();
        let outcome = tokio::time::timeout(LIMIT, flows.finished())
            .await
            .expect("the outcome arrives");
        assert!(
            matches!(outcome.result, Ok(Err(OAuthError::Aborted))),
            "{:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn a_panicking_flow_reports_and_frees_the_slot() {
        let mut flows = BrowserFlows::default();
        flows.start(SocialProvider::Google, FlowIntent::Link, |_| {
            panic!("flow thread failed")
        });
        let outcome = tokio::time::timeout(LIMIT, flows.finished())
            .await
            .expect("the outcome arrives");
        assert!(
            outcome.result.is_err(),
            "a panic is reported as a JoinError"
        );
        assert!(!flows.is_waiting());
    }

    #[tokio::test]
    async fn finished_never_completes_without_a_flow() {
        let mut flows = BrowserFlows::default();
        let polled = tokio::time::timeout(Duration::from_millis(20), flows.finished()).await;
        assert!(polled.is_err());
    }

    #[test]
    fn steam_needs_no_client_id_and_the_others_need_theirs() {
        // Builds can set the ids at compile time: clear them for this test.
        let config = Config {
            google_client_id: None,
            discord_client_id: None,
            twitch_client_id: None,
            ..Config::default()
        };
        assert_eq!(
            SocialProvider::Steam.client_id(&config),
            Some(String::new())
        );
        for provider in [
            SocialProvider::Google,
            SocialProvider::Discord,
            SocialProvider::Twitch,
        ] {
            assert_eq!(provider.client_id(&config), None, "{provider:?}");
        }
    }
}
