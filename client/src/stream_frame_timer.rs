//! 16ms stream frame presenter — timer runs only while watching a stream.

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
    // Windows presents via native_frame_slot + DComp below; the RGBA copy
    // path (and this slot) is the non-Windows fallback.
    #[cfg(not(target_os = "windows"))]
    frame_slot: FrameSlot,
    frame_consumed: Arc<AtomicBool>,
    frame_lifecycle: FrameLifecycleSlot,
    frame_timer_ticks: Cell<u64>,
    frame_timer_last_log: Cell<Instant>,
    frame_timer_presented: Cell<u64>,
    #[cfg(target_os = "windows")]
    native_frame_slot: mello_core::NativeFrameSlot,
    #[cfg(target_os = "windows")]
    last_surface_sequence: Cell<u64>,
    #[cfg(target_os = "windows")]
    dcomp_presenter: Rc<std::cell::RefCell<Option<crate::dcomp_presenter::DCompPresenter>>>,
}

fn on_frame_tick(ctx: &TickCtx) {
    let app_for_tick = ctx.app_weak.upgrade();
    ctx.frame_timer_ticks
        .set(ctx.frame_timer_ticks.get().saturating_add(1));

    #[cfg(target_os = "windows")]
    if let Ok(slot) = ctx.native_frame_slot.lock() {
        if let Some(frame) = *slot {
            if frame.sequence != ctx.last_surface_sequence.get() {
                ctx.last_surface_sequence.set(frame.sequence);
                if let Some(ref mut presenter) = *ctx.dcomp_presenter.borrow_mut() {
                    if presenter.present_shared_texture(
                        frame.shared_handle,
                        frame.width,
                        frame.height,
                    ) {
                        ctx.frame_timer_presented
                            .set(ctx.frame_timer_presented.get().saturating_add(1));
                    }
                }
            }
        }
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
            "DComp stream: present_fps={:.1} tick_hz={:.1}",
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
        #[cfg(target_os = "windows")] native_frame_slot: mello_core::NativeFrameSlot,
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
                native_frame_slot,
                #[cfg(target_os = "windows")]
                last_surface_sequence: Cell::new(0),
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
