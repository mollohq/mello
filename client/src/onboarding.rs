//! The onboarding state machine.
//!
//! Onboarding used to be an `int` mirrored in three places — the Slint
//! `onboarding-step` property, `Settings::onboarding_step`, and the persisted
//! TOML — written from sixteen sites across seven files. Several of those sites
//! updated the UI without persisting, so the screen and the disk disagreed and
//! the next launch went somewhere unexpected.
//!
//! Worse, nothing enforced that a step was *reachable* or that it rendered
//! anything: `step 0` with `logged-in == false` matches no branch in
//! `main.slint`, which is the blank window that took signup down.
//!
//! Here the states are named, the transitions are a pure function, and
//! [`apply`] is the single writer — so the UI and the persisted value cannot
//! drift apart.

use std::cell::RefCell;
use std::rc::Rc;

use crate::app_context::AppContext;
use slint::ComponentHandle;

/// Where the user is in first-run onboarding.
///
/// The numeric encoding is preserved for the Slint property and for settings
/// written by older builds; see [`OnboardingState::from_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnboardingState {
    /// Waiting for the crew list. Renders **nothing** — the app is only
    /// legitimately here for the moment between launch and the first
    /// `DiscoverCrews` response.
    Loading,
    /// Step 1: choose a crew to join, or create one.
    PickCrew,
    /// Step 2: nickname and avatar.
    PickAvatar,
    /// Step 3: link an identity, or skip.
    LinkIdentity,
    /// Onboarding finished; the main app is shown.
    Done,
}

/// Everything that can move onboarding along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Crew discovery returned (successfully or not — either way the user must
    /// leave `Loading`, or they are staring at an empty window).
    DiscoverySettled,
    /// A crew was chosen, or creation of a new one was started.
    CrewChosen,
    /// The account now exists (`OnboardingReady`).
    AccountReady,
    /// An identity was linked, or the user chose to skip.
    IdentitySettled,
    /// A previous session was restored (`LoggedIn`).
    SessionRestored,
    /// Session restore failed; fall back to signing up again.
    RestoreFailed,
    /// The user logged out.
    LoggedOut,
    /// The step indicator was used to jump backwards.
    GoBackTo(OnboardingState),
}

impl OnboardingState {
    /// Decode the numeric step used by Slint and by persisted settings.
    ///
    /// Total by construction: any unexpected value means a build that wrote a
    /// step we no longer understand, and treating that as `Done` is the safe
    /// reading — the alternative strands the user in onboarding forever.
    pub fn from_step(step: i32) -> Self {
        match step {
            0 => Self::Loading,
            1 => Self::PickCrew,
            2 => Self::PickAvatar,
            3 => Self::LinkIdentity,
            _ => Self::Done,
        }
    }

    pub fn to_step(self) -> i32 {
        match self {
            Self::Loading => 0,
            Self::PickCrew => 1,
            Self::PickAvatar => 2,
            Self::LinkIdentity => 3,
            Self::Done => 4,
        }
    }

    /// Does this state render anything on its own?
    ///
    /// `Loading` does not: `main.slint` shows onboarding only for steps 1..=3
    /// and the app only when logged in. Any path that can rest here without a
    /// session shows the user an empty window.
    pub fn renders_without_session(self) -> bool {
        !matches!(self, Self::Loading | Self::Done)
    }

    /// The pure transition function.
    ///
    /// Unknown combinations deliberately stay put rather than guessing: a
    /// stray event should never teleport the user mid-signup.
    #[must_use]
    pub fn next(self, input: Input) -> Self {
        use Input::*;
        use OnboardingState::*;

        match (self, input) {
            // Leaving Loading is unconditional. Even a *failed* discovery must
            // move on, because Loading renders nothing — this is the transition
            // whose absence caused the blank-window outage.
            (Loading, DiscoverySettled) => PickCrew,

            (PickCrew, CrewChosen) => PickAvatar,

            // The account exists but onboarding is not finished: the user is
            // still offered identity linking, and may skip it.
            //
            // Unconditional on the previous state. Signup having succeeded is
            // decisive regardless of where the UI thought the user was, and
            // making it conditional would strand them if the two ever
            // disagreed — the exact failure this module exists to prevent.
            (_, AccountReady) => LinkIdentity,
            (LinkIdentity, IdentitySettled) => Done,

            // A restored session skips onboarding entirely, from anywhere.
            (_, SessionRestored) => Done,
            // ...and losing one drops back to the start rather than to Loading,
            // which would render nothing.
            (_, RestoreFailed | LoggedOut) => PickCrew,

            // The step indicator only goes backwards, never forwards past work
            // the user has not done.
            (current, GoBackTo(target)) if target.to_step() < current.to_step() => target,

            (current, _) => current,
        }
    }

    /// What must be loaded for this state to be *usable*.
    ///
    /// Attached to the state, not to the button that happened to lead here.
    /// `PickAvatar` has three entry paths — the crew-selected callback, a
    /// restart resuming the persisted step, and the step indicator jumping
    /// back — and only the first used to load anything. The other two rendered
    /// a fully drawn step 2 with an empty avatar grid and empty device lists,
    /// and its Continue button requires a selected avatar, so it could never
    /// be pressed. The screen looked fine and was a dead end.
    ///
    /// Declaring the requirement here makes every path equivalent by
    /// construction, including paths added later.
    pub fn entry_effects(self) -> &'static [Effect] {
        match self {
            // Loading exists precisely to wait for this response; without it
            // the state never advances and the window stays empty.
            Self::Loading => &[Effect::DiscoverCrews],
            // Nothing to pick without the crew list.
            Self::PickCrew => &[Effect::DiscoverCrews],
            // Continue is gated on a chosen avatar, and the step shows mic and
            // speaker pickers.
            Self::PickAvatar => &[Effect::LoadAvatarGrid, Effect::ListAudioDevices],
            Self::LinkIdentity | Self::Done => &[],
        }
    }

    /// Every state, for exhaustive tests.
    #[cfg(any(test, feature = "testkit"))]
    pub const ALL: [Self; 5] = [
        Self::Loading,
        Self::PickCrew,
        Self::PickAvatar,
        Self::LinkIdentity,
        Self::Done,
    ];
}

/// Data a state needs before the user can act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Fetch the discoverable crew list.
    DiscoverCrews,
    /// Generate the avatar grid.
    LoadAvatarGrid,
    /// Enumerate microphones and speakers.
    ListAudioDevices,
}

/// Handles needed to run [`Effect`]s.
///
/// Every field is already a cheap handle, so Slint callbacks can capture a
/// clone. Bundling them is what lets the single writer own effect dispatch
/// rather than leaving it to whichever callback triggered the transition —
/// which is precisely how step 2 ended up loading its data on one path out of
/// three.
#[derive(Clone)]
pub struct EffectCtx {
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<mello_core::Command>,
    pub avatar_state: std::sync::Arc<std::sync::Mutex<crate::avatar::AvatarGridState>>,
    pub avatar_shuffle_timer: Rc<RefCell<Option<slint::Timer>>>,
    pub rt: tokio::runtime::Handle,
}

impl EffectCtx {
    pub fn from_ctx(ctx: &AppContext) -> Self {
        Self {
            cmd_tx: ctx.cmd_tx.clone(),
            avatar_state: ctx.avatar_state.clone(),
            avatar_shuffle_timer: ctx.avatar_shuffle_timer.clone(),
            rt: ctx.rt.clone(),
        }
    }
}

/// Run the effects a state needs to be usable.
///
/// Idempotent per entry: callers gate on the state actually changing.
pub fn run_entry_effects(app: &crate::MainWindow, fx: &EffectCtx, state: OnboardingState) {
    for effect in state.entry_effects() {
        match effect {
            Effect::DiscoverCrews => {
                let _ = fx
                    .cmd_tx
                    .send(mello_core::Command::DiscoverCrews { cursor: None });
            }
            Effect::ListAudioDevices => {
                let _ = fx.cmd_tx.send(mello_core::Command::ListAudioDevices);
            }
            Effect::LoadAvatarGrid => {
                *fx.avatar_state.lock().unwrap() = crate::avatar::AvatarGridState::new();
                crate::callbacks::onboarding::load_avatar_grid(
                    app.as_weak(),
                    &fx.avatar_state,
                    &fx.rt,
                );
                crate::callbacks::onboarding::start_ambient_shuffle_after_load(
                    app.as_weak(),
                    &fx.avatar_state,
                    &fx.avatar_shuffle_timer,
                    &fx.rt,
                );
            }
        }
    }
}

/// The single writer for onboarding state.
///
/// Updates the Slint property and the persisted setting together, then runs the
/// new state's entry effects. Every previous site did the first two by hand and
/// several forgot the persistence half, leaving the screen and the disk
/// disagreeing about where the user was; the effects were forgotten on two
/// entry paths out of three.
///
/// Takes the window and settings separately rather than an [`AppContext`]
/// because most callers are Slint callbacks that captured only those two.
///
/// Must not be called while `settings` is already borrowed.
pub fn apply_to(
    app: &crate::MainWindow,
    settings: &Rc<RefCell<crate::Settings>>,
    fx: &EffectCtx,
    state: OnboardingState,
) {
    let previous = OnboardingState::from_step(app.get_onboarding_step());
    // A logged-out user parked in a non-rendering state sees an empty window.
    // Loading is legitimate only as a transient at startup, so anything that
    // persists it is worth shouting about.
    if !state.renders_without_session() && !app.get_logged_in() && state != OnboardingState::Done {
        log::warn!(
            "[onboarding] persisting {state:?} while logged out — this state \
             renders no screen; the user would see an empty window"
        );
    }

    write_state(app, settings, state);

    // Only on a real change: `next` returns the current state for unrelated
    // inputs, and re-running effects there would re-fetch on every stray event.
    if state != previous {
        run_entry_effects(app, fx, state);
    }
}

/// Persist the state to the UI property and to settings, together.
fn write_state(
    app: &crate::MainWindow,
    settings: &Rc<RefCell<crate::Settings>>,
    state: OnboardingState,
) {
    app.set_onboarding_step(state.to_step());
    let mut s = settings.borrow_mut();
    s.onboarding_step = state.to_step() as u8;
    s.save();
}

/// Apply an input to the current state and persist the result.
pub fn advance_with(
    app: &crate::MainWindow,
    settings: &Rc<RefCell<crate::Settings>>,
    fx: &EffectCtx,
    input: Input,
) {
    let next = OnboardingState::from_step(app.get_onboarding_step()).next(input);
    apply_to(app, settings, fx, next);
}

/// [`advance_with`] for callers that already hold an [`AppContext`].
pub fn advance(ctx: &AppContext, input: Input) {
    advance_with(&ctx.app, &ctx.settings, &EffectCtx::from_ctx(ctx), input);
}

/// Enter a state directly, running its effects — used at startup to resume the
/// persisted step.
///
/// Startup used to call `set_onboarding_step` raw, which is how a resumed step 2
/// rendered with no avatars and no audio devices.
pub fn resume(ctx: &AppContext, state: OnboardingState) {
    // Deliberately *not* `apply_to`: at startup the Slint property still holds
    // its default of 0, so resuming `Loading` — a fresh install — would compare
    // equal, skip its effects, never fetch crews and never leave `Loading`.
    // That is the blank window the whole module exists to prevent.
    write_state(&ctx.app, &ctx.settings, state);
    run_entry_effects(&ctx.app, &EffectCtx::from_ctx(ctx), state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const INPUTS: &[Input] = &[
        Input::DiscoverySettled,
        Input::CrewChosen,
        Input::AccountReady,
        Input::IdentitySettled,
        Input::SessionRestored,
        Input::RestoreFailed,
        Input::LoggedOut,
    ];

    #[test]
    fn step_encoding_round_trips() {
        for state in OnboardingState::ALL {
            assert_eq!(
                OnboardingState::from_step(state.to_step()),
                state,
                "{state:?} must survive a trip through its numeric encoding"
            );
        }
    }

    /// Settings written by an older or newer build must not strand the user.
    #[test]
    fn unknown_steps_decode_to_done() {
        for step in [4, 5, 9, 127, -1] {
            assert_eq!(
                OnboardingState::from_step(step),
                OnboardingState::Done,
                "step {step} must not leave the user stuck in onboarding"
            );
        }
    }

    /// **The property that matters.** From every state there is a path to
    /// `Done`. A state with no route out is a user who can never finish
    /// signing up — the shape of the original outage.
    #[test]
    fn every_state_can_reach_done() {
        for start in OnboardingState::ALL {
            let mut seen: HashSet<OnboardingState> = HashSet::new();
            let mut queue = vec![start];
            let mut reached_done = false;

            while let Some(state) = queue.pop() {
                if !seen.insert(state) {
                    continue;
                }
                if state == OnboardingState::Done {
                    reached_done = true;
                    break;
                }
                for &input in INPUTS {
                    queue.push(state.next(input));
                }
            }

            assert!(
                reached_done,
                "{start:?} cannot reach Done by any sequence of inputs: a user \
                 there could never finish signing up"
            );
        }
    }

    /// No state may sit indefinitely showing nothing. `Loading` renders no
    /// branch in main.slint, so it must always have an immediate way out.
    #[test]
    fn loading_always_has_an_exit() {
        let escaped = INPUTS
            .iter()
            .any(|&i| OnboardingState::Loading.next(i) != OnboardingState::Loading);
        assert!(
            escaped,
            "Loading renders nothing; without an exit the user sees a blank window"
        );

        assert_eq!(
            OnboardingState::Loading.next(Input::DiscoverySettled),
            OnboardingState::PickCrew,
            "discovery settling must move the user on even when it failed — \
             this exact transition is what the signup outage was missing"
        );
    }

    #[test]
    fn the_happy_path_runs_start_to_finish() {
        let mut state = OnboardingState::Loading;
        for input in [
            Input::DiscoverySettled,
            Input::CrewChosen,
            Input::AccountReady,
            Input::IdentitySettled,
        ] {
            state = state.next(input);
        }
        assert_eq!(state, OnboardingState::Done);
    }

    #[test]
    fn a_restored_session_skips_onboarding_from_anywhere() {
        for state in OnboardingState::ALL {
            assert_eq!(
                state.next(Input::SessionRestored),
                OnboardingState::Done,
                "{state:?} + SessionRestored should go straight to the app"
            );
        }
    }

    /// Losing a session must not land in `Loading`, which renders nothing.
    #[test]
    fn losing_a_session_lands_somewhere_visible() {
        for state in OnboardingState::ALL {
            for input in [Input::RestoreFailed, Input::LoggedOut] {
                let next = state.next(input);
                assert!(
                    next.renders_without_session(),
                    "{state:?} + {input:?} lands in {next:?}, which renders \
                     nothing for a logged-out user"
                );
            }
        }
    }

    #[test]
    fn the_step_indicator_only_goes_backwards() {
        assert_eq!(
            OnboardingState::LinkIdentity.next(Input::GoBackTo(OnboardingState::PickCrew)),
            OnboardingState::PickCrew,
        );
        assert_eq!(
            OnboardingState::PickCrew.next(Input::GoBackTo(OnboardingState::Done)),
            OnboardingState::PickCrew,
            "jumping forward past unfinished work must be refused"
        );
    }

    /// **The invariant we were missing.** A state that renders a screen must
    /// declare everything that screen needs to be *usable*.
    ///
    /// Step 2's Continue button is gated on a chosen avatar, so an empty grid
    /// is a dead end no matter how well the screen renders. The old code loaded
    /// the grid in the crew-selected callback, so two of three entry paths —
    /// resuming the persisted step, and the step indicator jumping back — drew
    /// a perfect, unusable screen. Every invariant we had passed, because they
    /// all asked "does it render?".
    #[test]
    fn pick_avatar_requires_the_data_its_continue_button_needs() {
        let effects = OnboardingState::PickAvatar.entry_effects();
        assert!(
            effects.contains(&Effect::LoadAvatarGrid),
            "Continue is disabled until an avatar is selected, so entering step 2 \
             without loading the grid is a dead end"
        );
        assert!(
            effects.contains(&Effect::ListAudioDevices),
            "step 2 shows mic and speaker pickers; without devices they are empty"
        );
    }

    /// `Loading` renders nothing and only leaves on `DiscoverySettled`, which
    /// arrives as the `DiscoverCrews` response. Without that effect the state
    /// never advances — the blank window that took signup down.
    #[test]
    fn loading_fetches_the_thing_it_is_waiting_for() {
        assert!(
            OnboardingState::Loading
                .entry_effects()
                .contains(&Effect::DiscoverCrews),
            "Loading waits for the crew list; if entering it does not request \
             one, the user sits on an empty window forever"
        );
    }

    /// Any state a logged-out user can rest in must be able to load its own
    /// screen. Otherwise reaching it by a path that isn't the happy one leaves
    /// a screen that renders and cannot be used.
    #[test]
    fn every_rendering_state_declares_its_own_data() {
        for state in OnboardingState::ALL {
            if !state.renders_without_session() {
                continue;
            }
            // LinkIdentity is the exception by design: its buttons are static.
            if state == OnboardingState::LinkIdentity {
                continue;
            }
            assert!(
                !state.entry_effects().is_empty(),
                "{state:?} renders a screen but declares no entry effects; if it \
                 needs data, whichever path forgot to load it is a dead end"
            );
        }
    }

    /// Stray events must not teleport a user mid-signup.
    #[test]
    fn unrelated_inputs_leave_the_state_alone() {
        assert_eq!(
            OnboardingState::PickAvatar.next(Input::DiscoverySettled),
            OnboardingState::PickAvatar,
            "a late discovery response must not yank the user backwards"
        );
    }
}
