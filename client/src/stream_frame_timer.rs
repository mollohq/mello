//! The stream UI timer — runs only while watching a stream.
//!
//! Streaming-quality stage 3: on Windows this timer presents nothing.
//! libmello's present thread shows each frame at its render time through the
//! DComp swap chain (`DCompPresenter::frame_presenter`). The timer only keeps
//! the UI-thread side of the picture current: the host's pointer (a child
//! visual), the geometry after a stream size change, and the debug fps.
//! Off Windows the decoded RGBA still reaches Slint here (the macOS viewer has
//! no compositor path yet).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[cfg(not(target_os = "windows"))]
use mello_core::FrameSlot;
use mello_core::{FrameLifecycleSlot, FRAME_STATE_PRESENTED};

use crate::MainWindow;

struct TickCtx {
    app_weak: slint::Weak<MainWindow>,
    // Off Windows the RGBA copy path (and this slot) is the display path.
    #[cfg(not(target_os = "windows"))]
    frame_slot: FrameSlot,
    frame_consumed: Arc<AtomicBool>,
    frame_lifecycle: FrameLifecycleSlot,
    frame_timer_ticks: Cell<u64>,
    frame_timer_last_log: Cell<Instant>,
    frame_timer_presented: Cell<u64>,
    #[cfg(target_os = "windows")]
    last_presented_total: Cell<u64>,
    #[cfg(target_os = "windows")]
    dcomp_presenter: Rc<std::cell::RefCell<Option<crate::dcomp_presenter::DCompPresenter>>>,
}

fn on_frame_tick(ctx: &TickCtx) {
    let app_for_tick = ctx.app_weak.upgrade();
    ctx.frame_timer_ticks
        .set(ctx.frame_timer_ticks.get().saturating_add(1));

    #[cfg(target_os = "windows")]
    if let Some(ref mut presenter) = *ctx.dcomp_presenter.borrow_mut() {
        // The host's pointer, when the host sends one (the game capture hook).
        presenter.update_cursor(&mello_core::stream::cursor::viewer_cursor());
        // A new stream size from the present thread needs a new scale.
        presenter.refresh_geometry_if_resized();
        let total = presenter.presented_frames();
        ctx.frame_timer_presented.set(
            ctx.frame_timer_presented
                .get()
                .saturating_add(total.saturating_sub(ctx.last_presented_total.get())),
        );
        ctx.last_presented_total.set(total);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let frame_data = ctx.frame_slot.lock().ok().and_then(|mut s| s.take());
        if let Some((w, h, rgba)) = frame_data {
            if let Some(app) = app_for_tick.as_ref() {
                let buf =
                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                app.set_stream_frame(slint::Image::from_rgba8(buf));
                ctx.frame_timer_presented
                    .set(ctx.frame_timer_presented.get().saturating_add(1));
            }
        }
    }

    ctx.frame_consumed.store(true, Ordering::Release);
    ctx.frame_lifecycle
        .store(FRAME_STATE_PRESENTED, Ordering::Release);

    let last_log = ctx.frame_timer_last_log.get();
    if last_log.elapsed().as_secs_f32() >= 1.0 {
        let elapsed = last_log.elapsed().as_secs_f32().max(0.001);
        let present_fps = ctx.frame_timer_presented.get() as f32 / elapsed;

        if let Some(app) = app_for_tick.as_ref() {
            app.set_dbg_stream_ui_render_fps(present_fps);
        }

        #[cfg(target_os = "windows")]
        log::info!(
            "DComp stream: present_fps={:.1} ui_tick_hz={:.1}",
            present_fps,
            ctx.frame_timer_ticks.get() as f32 / elapsed
        );
        #[cfg(not(target_os = "windows"))]
        log::info!(
            "Stream: present_fps={:.1} tick_hz={:.1}",
            present_fps,
            ctx.frame_timer_ticks.get() as f32 / elapsed
        );

        ctx.frame_timer_ticks.set(0);
        ctx.frame_timer_presented.set(0);
        ctx.frame_timer_last_log.set(Instant::now());
    }
}

pub struct StreamFrameTimer {
    running: Cell<bool>,
    timer: slint::Timer,
    tick_ctx: Rc<TickCtx>,
}

impl StreamFrameTimer {
    pub fn new(
        app_weak: slint::Weak<MainWindow>,
        #[cfg(not(target_os = "windows"))] frame_slot: FrameSlot,
        frame_consumed: Arc<AtomicBool>,
        frame_lifecycle: FrameLifecycleSlot,
        #[cfg(target_os = "windows")] dcomp_presenter: Rc<
            std::cell::RefCell<Option<crate::dcomp_presenter::DCompPresenter>>,
        >,
    ) -> Self {
        Self {
            running: Cell::new(false),
            timer: slint::Timer::default(),
            tick_ctx: Rc::new(TickCtx {
                app_weak,
                #[cfg(not(target_os = "windows"))]
                frame_slot,
                frame_consumed,
                frame_lifecycle,
                frame_timer_ticks: Cell::new(0),
                frame_timer_last_log: Cell::new(Instant::now()),
                frame_timer_presented: Cell::new(0),
                #[cfg(target_os = "windows")]
                last_presented_total: Cell::new(0),
                #[cfg(target_os = "windows")]
                dcomp_presenter,
            }),
        }
    }

    pub fn set_watching(&self, watching: bool) {
        if self.running.get() == watching {
            return;
        }
        self.running.set(watching);
        if watching {
            let ctx = self.tick_ctx.clone();
            #[cfg(target_os = "windows")]
            ctx.last_presented_total.set(0);
            self.timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(16),
                move || on_frame_tick(&ctx),
            );
        } else {
            self.timer.stop();
            self.tick_ctx.frame_timer_ticks.set(0);
            self.tick_ctx.frame_timer_presented.set(0);
            self.tick_ctx.frame_timer_last_log.set(Instant::now());
            if let Some(app) = self.tick_ctx.app_weak.upgrade() {
                app.set_dbg_stream_ui_render_fps(0.0);
            }
        }
    }
}
