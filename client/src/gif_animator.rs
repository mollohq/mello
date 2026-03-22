use std::cell::RefCell;
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

/// Drives GIF frame animation via a single shared Slint Timer.
/// Keyed by URL so the same GIF isn't decoded twice.
///
/// `max_loops`: if `Some(n)`, each GIF pauses after `n` full loops.
/// Call `resume` to restart the loop counter (e.g. on hover).
/// Clone is cheap (Rc/Arc internals).
#[derive(Clone)]
pub struct GifAnimator {
    entries: Rc<RefCell<HashMap<String, AnimEntry>>>,
    inbox: Arc<Mutex<Vec<(String, GifFrameData)>>>,
    timer: Rc<slint::Timer>,
    tick_ms: u32,
    max_loops: Option<u32>,
}

impl GifAnimator {
    pub fn new(tick_ms: u32, max_loops: Option<u32>) -> Self {
        Self {
            entries: Rc::new(RefCell::new(HashMap::new())),
            inbox: Arc::new(Mutex::new(Vec::new())),
            timer: Rc::new(slint::Timer::default()),
            tick_ms,
            max_loops,
        }
    }

    /// Send-safe handle to the inbox. Give this to async tasks.
    pub fn inbox(&self) -> Arc<Mutex<Vec<(String, GifFrameData)>>> {
        self.inbox.clone()
    }

    /// Start the animation loop. `on_frame` is called with (url, new_image)
    /// each time a GIF advances to a new frame. Also called once per URL when
    /// frames first arrive from the inbox (with the first frame).
    pub fn start(&self, on_frame: impl Fn(&str, &slint::Image) + 'static) {
        let entries = self.entries.clone();
        let inbox = self.inbox.clone();
        let tick = self.tick_ms;
        let max_loops = self.max_loops;
        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(tick as u64),
            move || {
                // Drain inbox: convert raw bytes → slint::Image on the main thread
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
                            on_frame(&url, first);
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

                // Advance frames
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

                        // Wrapped back to frame 0 → completed a loop
                        if entry.current < prev {
                            entry.loops_done += 1;
                            if let Some(limit) = max_loops {
                                if entry.loops_done >= limit {
                                    entry.paused = true;
                                    continue;
                                }
                            }
                        }

                        on_frame(url, &entry.frames[entry.current]);
                    }
                }
            },
        );
    }

    /// Unpause a GIF and reset its loop counter (e.g. on hover).
    pub fn resume(&self, url: &str) {
        if let Some(entry) = self.entries.borrow_mut().get_mut(url) {
            if entry.paused {
                entry.paused = false;
                entry.loops_done = 0;
                entry.elapsed = 0;
            }
        }
    }

    /// Drop all frame data and stop the timer.
    pub fn stop_and_clear(&self) {
        self.timer.stop();
        self.entries.borrow_mut().clear();
        self.inbox.lock().unwrap().clear();
    }

    pub fn has_url(&self, url: &str) -> bool {
        self.entries.borrow().contains_key(url)
    }
}
