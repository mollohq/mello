use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::image_cache::GifFrameData;

/// Frames of one decoded GIF, plus how many entries were inserted before it
/// (`seq`), so the oldest can be evicted when the cache is full.
struct AnimEntry {
    frames: Vec<slint::Image>,
    delays: Vec<u32>,
    current: usize,
    elapsed: u32,
    loops_done: u32,
    paused: bool,
    seq: u64,
}

/// What the animator tells the UI about a URL: a frame to show, or that the
/// fetch failed and no frame will come.
pub enum GifFrame<'a> {
    Image(&'a slint::Image),
    Failed,
}

type FrameHandler = Rc<dyn Fn(&str, GifFrame<'_>)>;

/// One fetch result, pushed from the tokio runtime and drained on the UI
/// thread by the timer. `None` means the fetch or the decode failed.
type FetchResult = (String, Option<GifFrameData>);

/// One fetch the animator waits for. Dropping it without `deliver` would
/// leave the timer running, so every path delivers, with `None` on failure.
pub struct FetchTicket {
    url: String,
    inbox: Arc<Mutex<Vec<FetchResult>>>,
    in_flight: Arc<AtomicUsize>,
}

impl FetchTicket {
    /// Hand the result to the UI thread. Safe from any thread.
    pub fn deliver(self, data: Option<GifFrameData>) {
        self.inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((self.url, data));
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Drives GIF frame animation via a single shared Slint Timer.
/// Keyed by URL so the same GIF isn't decoded twice.
///
/// The timer runs while a fetch is in flight, a result waits in the inbox, or
/// a frame is animating. Otherwise it stops, so an idle client costs no wakeups.
#[derive(Clone)]
pub struct GifAnimator {
    entries: Rc<RefCell<HashMap<String, AnimEntry>>>,
    /// URLs asked for in this cache's lifetime: in flight, decoded, or failed.
    requested: Rc<RefCell<HashSet<String>>>,
    failed: Rc<RefCell<HashSet<String>>>,
    inbox: Arc<Mutex<Vec<FetchResult>>>,
    in_flight: Arc<AtomicUsize>,
    timer: Rc<slint::Timer>,
    tick_ms: u32,
    max_loops: Option<u32>,
    max_entries: usize,
    next_seq: Rc<Cell<u64>>,
    timer_running: Rc<Cell<bool>>,
    on_frame: Rc<RefCell<Option<FrameHandler>>>,
}

impl GifAnimator {
    /// `max_entries` bounds decoded frames kept in memory. Past it, the oldest
    /// GIF is dropped and fetched again on hover.
    pub fn new(tick_ms: u32, max_loops: Option<u32>, max_entries: usize) -> Self {
        Self {
            entries: Rc::new(RefCell::new(HashMap::new())),
            requested: Rc::new(RefCell::new(HashSet::new())),
            failed: Rc::new(RefCell::new(HashSet::new())),
            inbox: Arc::new(Mutex::new(Vec::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            timer: Rc::new(slint::Timer::default()),
            tick_ms,
            max_loops,
            max_entries,
            next_seq: Rc::new(Cell::new(0)),
            timer_running: Rc::new(Cell::new(false)),
            on_frame: Rc::new(RefCell::new(None)),
        }
    }

    /// Register the frame callback. The timer starts lazily on first fetch.
    pub fn start(&self, on_frame: impl Fn(&str, GifFrame<'_>) + 'static) {
        *self.on_frame.borrow_mut() = Some(Rc::new(on_frame));
    }

    /// Fetch and decode `url` once. A URL already requested (in flight,
    /// decoded, or failed) is ignored. Keeps the timer alive until the result
    /// lands in the inbox and is drained.
    pub fn fetch(&self, url: &str, rt: &tokio::runtime::Handle) {
        let Some(ticket) = self.begin_fetch(url) else {
            return;
        };
        let url = url.to_string();
        rt.spawn(async move {
            let data = crate::image_cache::fetch_gif_frames(&url).await;
            if data.is_none() {
                log::debug!("[gif] failed to fetch frames from {}", url);
            }
            ticket.deliver(data);
        });
    }

    /// Count a fetch for `url` as in flight and keep the timer running until
    /// its [`FetchTicket`] delivers. `None` when the URL was already requested.
    fn begin_fetch(&self, url: &str) -> Option<FetchTicket> {
        if !self.requested.borrow_mut().insert(url.to_string()) {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.ensure_timer_running();
        Some(FetchTicket {
            url: url.to_string(),
            inbox: self.inbox.clone(),
            in_flight: self.in_flight.clone(),
        })
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
        let failed = self.failed.clone();
        let inbox = self.inbox.clone();
        let in_flight = self.in_flight.clone();
        let tick = self.tick_ms;
        let max_loops = self.max_loops;
        let max_entries = self.max_entries;
        let next_seq = self.next_seq.clone();
        // Weak (not Rc) to avoid a timer -> closure -> timer reference cycle.
        let timer_weak = Rc::downgrade(&self.timer);
        let running = self.timer_running.clone();

        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(tick as u64),
            move || {
                let pending: Vec<FetchResult> = {
                    let mut inbox = inbox.lock().unwrap_or_else(|e| e.into_inner());
                    inbox.drain(..).collect()
                };
                for (url, data) in pending {
                    let Some(data) = data else {
                        failed.borrow_mut().insert(url.clone());
                        handler(&url, GifFrame::Failed);
                        continue;
                    };
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
                        handler(&url, GifFrame::Image(first));
                    }

                    let mut map = entries.borrow_mut();
                    if map.len() >= max_entries {
                        if let Some(oldest) = map
                            .iter()
                            .min_by_key(|(_, e)| e.seq)
                            .map(|(k, _)| k.clone())
                        {
                            map.remove(&oldest);
                        }
                    }
                    let seq = next_seq.get();
                    next_seq.set(seq + 1);
                    map.insert(
                        url,
                        AnimEntry {
                            delays: data.delays,
                            frames: images,
                            current: 0,
                            elapsed: 0,
                            loops_done: 0,
                            paused: false,
                            seq,
                        },
                    );
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

                            handler(url, GifFrame::Image(&entry.frames[entry.current]));
                        }
                    }
                }

                // Stop only when nothing animates, nothing waits in the inbox,
                // and no fetch can still deliver. `fetch()` / `resume()` restart.
                let animating = {
                    let map = entries.borrow();
                    map.values().any(|e| !e.paused && e.frames.len() > 1)
                };
                let idle = !animating
                    && in_flight.load(Ordering::SeqCst) == 0
                    && inbox.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
                if idle {
                    running.set(false);
                    if let Some(timer) = timer_weak.upgrade() {
                        timer.stop();
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
                self.ensure_timer_running();
            }
        }
    }

    /// The frame a row should show right now, if the GIF is decoded.
    pub fn current_frame(&self, url: &str) -> Option<slint::Image> {
        self.entries
            .borrow()
            .get(url)
            .map(|e| e.frames[e.current].clone())
    }

    /// True when a fetch for `url` was tried in this cache's lifetime and failed.
    pub fn has_failed(&self, url: &str) -> bool {
        self.failed.borrow().contains(url)
    }

    /// True when `url` was asked for: in flight, decoded, or failed.
    pub fn is_requested(&self, url: &str) -> bool {
        self.requested.borrow().contains(url)
    }

    /// Drop all frame data, forget every request, and stop the timer.
    /// A fetch still in flight delivers to the inbox and is discarded on the
    /// next clear or shown if the URL is asked for again.
    pub fn stop_and_clear(&self) {
        self.timer.stop();
        self.timer_running.set(false);
        self.entries.borrow_mut().clear();
        self.requested.borrow_mut().clear();
        self.failed.borrow_mut().clear();
        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ Regression: the timer used to stop itself on its first tick when the
    /// inbox was empty, 50 ms after a load and before any fetch had returned.
    /// Decoded frames then sat in the inbox with nothing to drain them, so
    /// every picker tile stayed blank and every chat GIF stayed on "Loading".
    #[test]
    fn the_timer_waits_for_a_fetch_in_flight() {
        crate::testkit::init_test_backend();
        let anim = GifAnimator::new(50, None, 8);
        let seen: Rc<RefCell<Vec<(String, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        anim.start(move |url, frame| {
            log.borrow_mut()
                .push((url.to_string(), matches!(frame, GifFrame::Image(_))));
        });

        let ticket = anim.begin_fetch("http://gif/a").expect("first request");
        assert!(
            anim.begin_fetch("http://gif/a").is_none(),
            "one fetch per URL"
        );

        // Three ticks with nothing to show. The timer must stay alive.
        for _ in 0..3 {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        }
        assert!(seen.borrow().is_empty());

        ticket.deliver(Some(GifFrameData {
            frames: vec![(vec![0, 0, 0, 255], 1, 1)],
            delays: vec![100],
        }));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));

        assert_eq!(
            *seen.borrow(),
            vec![("http://gif/a".to_string(), true)],
            "the frame reaches the handler on the tick after delivery"
        );
        assert!(anim.current_frame("http://gif/a").is_some());
        assert!(anim.is_requested("http://gif/a"));
    }

    #[test]
    fn a_failed_fetch_is_reported_once_and_remembered() {
        crate::testkit::init_test_backend();
        let anim = GifAnimator::new(50, None, 8);
        let seen: Rc<RefCell<Vec<(String, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        anim.start(move |url, frame| {
            log.borrow_mut()
                .push((url.to_string(), matches!(frame, GifFrame::Image(_))));
        });
        let ticket = anim.begin_fetch("http://gif/b").expect("request");
        ticket.deliver(None);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        assert_eq!(*seen.borrow(), vec![("http://gif/b".to_string(), false)]);
        assert!(anim.has_failed("http://gif/b"));
        assert!(anim.current_frame("http://gif/b").is_none());
    }

    /// The cache holds `max_entries` GIFs. Past that the oldest goes, and it
    /// can be asked for again (a hover fetches it).
    #[test]
    fn the_cache_evicts_the_oldest_gif() {
        crate::testkit::init_test_backend();
        let anim = GifAnimator::new(50, None, 2);
        anim.start(|_, _| {});
        for url in ["http://gif/1", "http://gif/2", "http://gif/3"] {
            let ticket = anim.begin_fetch(url).expect("request");
            ticket.deliver(Some(GifFrameData {
                frames: vec![(vec![0, 0, 0, 255], 1, 1)],
                delays: vec![100],
            }));
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        }
        assert!(anim.current_frame("http://gif/1").is_none(), "evicted");
        assert!(anim.current_frame("http://gif/2").is_some());
        assert!(anim.current_frame("http://gif/3").is_some());
    }
}
