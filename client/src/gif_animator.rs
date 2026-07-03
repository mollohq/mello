use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::image_cache::GifFrameData;

struct AnimEntry {
    frames: Vec<slint::Image>,
    delays: Vec<u32>,
    current: usize,
    elapsed: u32,
    loops_done: u32,
    paused: bool,
}

type FrameHandler = Rc<dyn Fn(&str, &slint::Image)>;

/// Drives GIF frame animation via a single shared Slint Timer.
/// Keyed by URL so the same GIF isn't decoded twice.
///
/// The timer starts only when at least one GIF is active (lazy).
#[derive(Clone)]
pub struct GifAnimator {
    entries: Rc<RefCell<HashMap<String, AnimEntry>>>,
    inbox: Arc<Mutex<Vec<(String, GifFrameData)>>>,
    timer: Rc<slint::Timer>,
    tick_ms: u32,
    max_loops: Option<u32>,
    timer_running: Rc<Cell<bool>>,
    on_frame: Rc<RefCell<Option<FrameHandler>>>,
}

impl GifAnimator {
    pub fn new(tick_ms: u32, max_loops: Option<u32>) -> Self {
        Self {
            entries: Rc::new(RefCell::new(HashMap::new())),
            inbox: Arc::new(Mutex::new(Vec::new())),
            timer: Rc::new(slint::Timer::default()),
            tick_ms,
            max_loops,
            timer_running: Rc::new(Cell::new(false)),
            on_frame: Rc::new(RefCell::new(None)),
        }
    }

    /// Send-safe handle to the inbox. Give this to async tasks.
    pub fn inbox(&self) -> Arc<Mutex<Vec<(String, GifFrameData)>>> {
        self.inbox.clone()
    }

    /// Register the frame callback. The 50ms timer starts lazily on first GIF activity.
    pub fn start(&self, on_frame: impl Fn(&str, &slint::Image) + 'static) {
        *self.on_frame.borrow_mut() = Some(Rc::new(on_frame));
    }

    fn ensure_timer_running(&self) {
        if self.timer_running.get() {
            return;
        }
        let Some(handler) = self.on_frame.borrow().clone() else {
            return;
        };
        self.timer_running.set(true);

        let entries = self.entries.clone();
        let inbox = self.inbox.clone();
        let tick = self.tick_ms;
        let max_loops = self.max_loops;
        // Weak (not Rc) to avoid a timer -> closure -> timer reference cycle.
        let timer_weak = Rc::downgrade(&self.timer);
        let running = self.timer_running.clone();

        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(tick as u64),
            move || {
                {
                    let mut pending = inbox.lock().unwrap();
                    for (url, data) in pending.drain(..) {
                        let images: Vec<slint::Image> = data
                            .frames
                            .iter()
                            .map(|(rgba, w, h)| {
                                let buf =
                                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        rgba, *w, *h,
                                    );
                                slint::Image::from_rgba8(buf)
                            })
                            .collect();

                        if let Some(first) = images.first() {
                            handler(&url, first);
                        }

                        entries.borrow_mut().insert(
                            url,
                            AnimEntry {
                                delays: data.delays,
                                frames: images,
                                current: 0,
                                elapsed: 0,
                                loops_done: 0,
                                paused: false,
                            },
                        );
                    }
                }

                {
                    let mut map = entries.borrow_mut();
                    for (url, entry) in map.iter_mut() {
                        if entry.paused || entry.frames.len() <= 1 {
                            continue;
                        }
                        entry.elapsed += tick;
                        let delay = entry.delays[entry.current];
                        if entry.elapsed >= delay {
                            entry.elapsed -= delay;
                            let prev = entry.current;
                            entry.current = (entry.current + 1) % entry.frames.len();

                            if entry.current < prev {
                                entry.loops_done += 1;
                                if let Some(limit) = max_loops {
                                    if entry.loops_done >= limit {
                                        entry.paused = true;
                                        continue;
                                    }
                                }
                            }

                            handler(url, &entry.frames[entry.current]);
                        }
                    }
                }

                // Nothing left to animate and nothing pending: stop the timer so
                // an idle client with a finished/paused GIF in view costs zero
                // wakeups. `note_activity()` / `resume()` restart it.
                let animating = {
                    let map = entries.borrow();
                    map.values().any(|e| !e.paused && e.frames.len() > 1)
                };
                if !animating && inbox.lock().unwrap().is_empty() {
                    running.set(false);
                    if let Some(timer) = timer_weak.upgrade() {
                        timer.stop();
                    }
                }
            },
        );
    }

    /// Call when a new GIF URL is queued for decode.
    pub fn note_activity(&self) {
        self.ensure_timer_running();
    }

    /// Unpause a GIF and reset its loop counter (e.g. on hover).
    pub fn resume(&self, url: &str) {
        if let Some(entry) = self.entries.borrow_mut().get_mut(url) {
            if entry.paused {
                entry.paused = false;
                entry.loops_done = 0;
                entry.elapsed = 0;
                self.ensure_timer_running();
            }
        }
    }

    /// Drop all frame data and stop the timer.
    pub fn stop_and_clear(&self) {
        self.timer.stop();
        self.timer_running.set(false);
        self.entries.borrow_mut().clear();
        self.inbox.lock().unwrap().clear();
    }

    pub fn has_url(&self, url: &str) -> bool {
        self.entries.borrow().contains_key(url)
    }
}
