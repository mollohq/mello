//! Test-only helpers for driving the UI headlessly.
//!
//! Compiled under `cfg(test)` for unit tests in this crate, and under the
//! `testkit` feature so integration tests in `client/tests/` can use it too.
//!
//! This module is the single test-facing surface of the crate: the internal
//! modules it draws from stay private, so production code keeps its normal
//! dead-code analysis.

use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use mello_core::{Command, Event};
use slint::ComponentHandle;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// Re-exported so integration tests can build a headless context via
/// [`AppContext::for_test`] without `app_context` becoming public API.
pub use crate::app_context::AppContext;
/// The generated Slint root component, for asserting on UI state.
pub use crate::MainWindow;

/// Install the headless Slint backend for the current thread, once.
///
/// `init_no_event_loop` installs a *per-thread* backend and panics if that
/// thread already has one. The libtest harness normally gives each test its own
/// thread, but that is an implementation detail rather than a guarantee, so
/// guard rather than rely on it.
///
/// Note this backend does not run `slint::Timer`s — which is why the poll loop
/// is driven through [`crate::poll_loop::PollState::tick`] rather than by
/// waiting on its 100 ms timer.
pub fn init_test_backend() {
    thread_local! {
        static INIT: std::cell::OnceCell<()> = const { std::cell::OnceCell::new() };
    }
    INIT.with(|once| {
        once.get_or_init(i_slint_backend_testing::init_no_event_loop);
    });
}

/// Redirect settings writes to a process-wide temp dir, once.
///
/// Callbacks call `Settings::save()` on almost every onboarding step, so
/// without this a test run would rewrite the developer's real config file.
/// Set once for the whole process rather than per-`Harness`: the variable is
/// process-global, and rewriting it per test would race under parallel
/// execution. Sharing one directory is harmless because each `Harness` holds
/// its own in-memory `Settings` and never reads back from disk.
fn redirect_settings_to_temp_dir() {
    static TEMP_CONFIG_DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = TEMP_CONFIG_DIR
        .get_or_init(|| tempfile::tempdir().expect("failed to create temp config dir for tests"));
    std::env::set_var(crate::settings::CONFIG_DIR_ENV, dir.path());
}

/// Fail loudly if Slint element metadata is missing.
///
/// Without debug info every `ElementHandle` query returns an *empty iterator*
/// rather than an error, so element-based assertions would silently pass while
/// testing nothing. That is the worst possible failure mode for a harness, so
/// check it explicitly, once per process.
///
/// Uses a throwaway window so it cannot disturb the caller's UI state. Note
/// that `MainWindow`'s default state renders nothing at all — neither the
/// onboarding branch (`step 1..=3`) nor the app branch matches — so the probe
/// has to put it into a state that actually instantiates something.
fn assert_element_queries_work() {
    static CHECKED: std::sync::Once = std::sync::Once::new();
    CHECKED.call_once(|| {
        let probe = MainWindow::new().expect("failed to create probe window");
        probe.set_onboarding_step(1);
        let found = ElementHandle::find_by_element_type_name(&probe, "Onboarding").count()
            + i_slint_backend_testing::ElementQuery::from_root(&probe)
                .match_descendants()
                .find_all()
                .len();
        assert!(
            found > 0,
            "Slint element queries returned nothing. Debug info is missing, so every \
             ElementHandle lookup would silently no-op and element-based assertions \
             would pass without testing anything. Check that \
             CompilerConfiguration::with_debug_info(true) is applied in client/build.rs."
        );
    });
}

/// A headless Mello UI, wired exactly like the real app.
///
/// Drives the *production* [`crate::callbacks::wire_all`] and
/// [`crate::handlers::handle_event`] rather than reimplementing them, so a
/// change to either is visible to these tests.
///
/// Two directions of travel:
/// - **Core → UI**: [`Harness::emit`] pushes an [`Event`] and pumps the poll
///   loop; assert with [`Harness::app`].
/// - **UI → core**: invoke a Slint callback (or [`Harness::click`]), then read
///   the resulting [`Command`]s with [`Harness::commands`].
pub struct Harness {
    ctx: AppContext,
    poll: crate::poll_loop::PollState,
    cmd_rx: UnboundedReceiver<Command>,
    event_tx: std::sync::mpsc::Sender<Event>,
    #[allow(dead_code)]
    update_tx: std::sync::mpsc::Sender<crate::updater::UpdateEvent>,
    // Kept alive: components hold `rt.handle()` clones.
    _rt: tokio::runtime::Runtime,
}

impl Harness {
    /// Build a harness with all callbacks wired and no OS resources touched.
    pub fn new() -> Self {
        init_test_backend();
        redirect_settings_to_temp_dir();
        assert_element_queries_work();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        let app = MainWindow::new().expect("failed to create MainWindow");
        let (cmd_tx, cmd_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let (update_tx, update_rx) = std::sync::mpsc::channel::<crate::updater::UpdateEvent>();

        let ctx = AppContext::for_test(app, cmd_tx, rt.handle().clone());

        // The real wiring — not a copy of it.
        crate::callbacks::wire_all(&ctx);

        let poll = crate::poll_loop::PollState::new(&ctx, event_rx, update_rx);

        Self {
            ctx,
            poll,
            cmd_rx,
            event_tx,
            update_tx,
            _rt: rt,
        }
    }

    /// The live `MainWindow`, for reading properties and invoking callbacks.
    pub fn app(&self) -> &MainWindow {
        &self.ctx.app
    }

    /// The shared context, for tests that need to inspect internal state.
    pub fn ctx(&self) -> &AppContext {
        &self.ctx
    }

    /// Deliver a core event to the UI and process it immediately.
    pub fn emit(&mut self, event: Event) {
        self.event_tx
            .send(event)
            .expect("poll loop receiver was dropped");
        self.pump();
    }

    /// Run one poll iteration, synchronously.
    ///
    /// Real builds run this on a 100 ms timer; the headless backend runs no
    /// timers, so tests drive it directly. One tick drains up to 128 events,
    /// which is far more than any single test queues.
    pub fn pump(&mut self) {
        self.poll.tick();
    }

    /// Drain every [`Command`] the UI has emitted since the last call.
    pub fn commands(&mut self) -> Vec<Command> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// All elements currently present in the UI tree.
    ///
    /// Hidden subtrees are absent from traversal, so an empty result means
    /// nothing is on screen — see [`Harness::assert_not_blank`].
    pub fn elements(&self) -> Vec<ElementHandle> {
        i_slint_backend_testing::ElementQuery::from_root(self.app())
            .match_descendants()
            .find_all()
    }

    /// Find elements by their Slint element id, e.g. `"Onboarding::continue-btn"`.
    pub fn find(&self, element_id: &str) -> Vec<ElementHandle> {
        ElementHandle::find_by_element_id(self.app(), element_id).collect()
    }

    /// Click an element by id, then pump.
    ///
    /// Panics when nothing matches: a missing element must fail the test rather
    /// than silently do nothing, which is what a bare query would do.
    pub fn click(&mut self, element_id: &str) {
        let matches = self.find(element_id);
        assert!(
            !matches.is_empty(),
            "no element with id {element_id:?} is currently visible \
             (hidden elements are absent from the tree, so check the screen state too)"
        );
        matches[0].mock_single_click(slint::platform::PointerEventButton::Left);
        self.pump();
    }

    /// Type text into whatever currently has focus.
    ///
    /// Slint 1.17 exposes no public keyboard helper, so this drives
    /// `Window::dispatch_event` directly.
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            let s = slint::SharedString::from(ch.to_string());
            self.app()
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: s.clone() });
            self.app()
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: s });
        }
        self.pump();
    }

    /// Assert the window is showing *something* the user can see.
    ///
    /// The onboarding outage produced a window where neither the onboarding
    /// branch nor the app branch matched, so nothing rendered at all and there
    /// was no error, no retry, and no way forward.
    pub fn assert_not_blank(&self) {
        let count = self.elements().len();
        assert!(
            count > 0,
            "the window is blank: no elements are visible in this state \
             (onboarding-step={}, logged-in={}). A user here would see an empty \
             window with no way forward.",
            self.app().get_onboarding_step(),
            self.app().get_logged_in(),
        );
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience for tests that only need a `Rc`-shared handle.
impl Harness {
    pub fn settings(&self) -> Rc<std::cell::RefCell<crate::Settings>> {
        self.ctx.settings.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UI → core, through the real `callbacks::wire_all`.
    #[test]
    fn ui_callback_reaches_core_as_a_command() {
        let mut h = Harness::new();
        assert!(!h.app().get_mic_muted(), "should start unmuted");

        h.app().invoke_mic_toggle();

        assert!(h.app().get_mic_muted(), "UI state should flip");
        let cmds = h.commands();
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::SetMute { muted: true })),
            "expected a SetMute{{muted:true}} command, got: {cmds:?}"
        );
    }

    /// core → UI, through the real `handlers::handle_event` and poll loop.
    #[test]
    fn core_event_reaches_the_ui() {
        let mut h = Harness::new();
        assert!(!h.app().get_in_voice());

        h.emit(Event::VoiceStateChanged { in_call: true });

        assert!(
            h.app().get_in_voice(),
            "VoiceStateChanged should have updated the UI"
        );
    }

    /// Events queued before a pump must not be lost, and one tick must drain
    /// all of them rather than one per tick.
    #[test]
    fn pump_drains_all_queued_events() {
        let mut h = Harness::new();

        h.event_tx
            .send(Event::VoiceStateChanged { in_call: true })
            .unwrap();
        h.event_tx
            .send(Event::VoiceStateChanged { in_call: false })
            .unwrap();
        h.pump();

        assert!(
            !h.app().get_in_voice(),
            "both events should have been applied in a single tick"
        );
    }

    /// The blank-window bug, as a test.
    ///
    /// `MainWindow`'s default state satisfies neither the onboarding branch
    /// (`step 1..=3`) nor the app branch (`logged-in && (step==0 || step>3)`),
    /// so nothing renders. This is what a user hit when `discover_crews`
    /// failed: an empty window, no error, no retry, no way forward.
    #[test]
    fn default_state_renders_a_blank_window() {
        let h = Harness::new();

        assert_eq!(h.app().get_onboarding_step(), 0);
        assert!(!h.app().get_logged_in());
        assert_eq!(
            h.elements().len(),
            0,
            "documenting current behaviour: the default state is blank. If this \
             starts failing, the dead state has been fixed and \
             assert_not_blank should be asserted here instead."
        );
    }

    /// ...and the same window is fine once a real screen is selected, which is
    /// what makes `assert_not_blank` a meaningful check rather than a tautology.
    #[test]
    fn onboarding_step_one_renders_content() {
        let h = Harness::new();
        h.app().set_onboarding_step(1);

        h.assert_not_blank();
    }

    #[test]
    fn click_panics_when_the_element_is_not_present() {
        let mut h = Harness::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            h.click("Onboarding::definitely-not-a-real-element");
        }));
        assert!(
            result.is_err(),
            "clicking a missing element must fail the test, not silently no-op"
        );
    }
}
