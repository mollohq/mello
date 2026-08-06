//! Test-only helpers for driving the UI headlessly.
//!
//! Compiled under `cfg(test)` for unit tests in this crate, and under the
//! `testkit` feature so integration tests in `client/tests/` can use it too.
//!
//! This module is the single test-facing surface of the crate: the internal
//! modules it draws from stay private, so production code keeps its normal
//! dead-code analysis.

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
