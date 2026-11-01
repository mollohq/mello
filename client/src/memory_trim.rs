//! Working-set trim at quiet moments (spec 12-NATIVE-PLATFORM section 3.3).
//!
//! Startup, a crew load and a voice join allocate a lot that the client then
//! does not touch again: decode buffers, JSON, first-frame caches, audio set-up.
//! Windows keeps those pages in the working set until memory gets tight or the
//! window is minimized, so Task Manager's "Memory" shows them for minutes. A
//! client measured 83 MB idle in a call right after the join and 48 MB twenty
//! minutes later, with the same committed memory.
//!
//! [`settle`] asks for a trim [`QUIET`] after the last settle point. The trim
//! (`SetProcessWorkingSetSize(-1, -1)`, what Windows does on minimize) moves the
//! pages out of the working set; they stay committed, and the ones the client
//! touches again come back as soft faults from the standby list. Nothing is
//! freed and nothing changes for the code.

use std::time::Duration;

use mello_core::Event;

/// The wait after the last settle point. Long enough that a burst of events
/// (a crew load, a join) ends first, short enough that the number in Task
/// Manager drops while the user still looks.
pub const QUIET: Duration = Duration::from_secs(10);

thread_local! {
    static TIMER: slint::Timer = slint::Timer::default();
}

/// True for the events after which the client settles: a login, a crew load,
/// a voice join, a voice state change (a leave among them).
pub fn settles(event: &Event) -> bool {
    matches!(
        event,
        Event::LoggedIn { .. }
            | Event::CrewStateLoaded { .. }
            | Event::VoiceJoined { .. }
            | Event::VoiceStateChanged { .. }
    )
}

/// A settle point: trim the working set [`QUIET`] from now. A later call moves
/// the trim again, so a burst of settle points gives one trim.
pub fn settle() {
    TIMER.with(|t| t.start(slint::TimerMode::SingleShot, QUIET, trim_now));
}

/// Moves the pages of this process out of its working set.
pub fn trim_now() {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
        // Both sizes at usize::MAX ((SIZE_T)-1) empties the working set.
        let result =
            unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
        match result {
            Ok(()) => log::info!("[memory] working set trimmed"),
            Err(e) => log::warn!("[memory] working set trim failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_join_and_a_crew_load_are_settle_points() {
        assert!(settles(&Event::VoiceJoined {
            crew_id: "k".into(),
            channel_id: "c".into(),
            members: vec![],
        }));
    }
}
