//! Frame times for the lab's benches (`FX_D2D_STATS=1`): the renderer has no
//! rendering notifier, so it reports itself. One line per second on stderr:
//! frames drawn, walk time (Slint's tree walk and the Direct2D calls, until
//! `EndDraw` is called), flush time (`EndDraw`: Direct2D's tessellation and
//! command submission), present time (the vsync wait) and the interval
//! between frames, p50 / p95 / max in milliseconds.

use std::time::{Duration, Instant};

use crate::itemrenderer::Categories;

pub struct FrameStats {
    start: Instant,
    last_frame: Option<Instant>,
    last_report: Instant,
    draw: Vec<f32>,
    flush: Vec<f32>,
    present: Vec<f32>,
    interval: Vec<f32>,
    /// Per-frame time by item kind, summed over the report period.
    categories: Categories,
    total: u32,
}

fn pct(v: &mut [f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f32 * p).round() as usize]
}

impl FrameStats {
    pub fn from_env() -> Option<Self> {
        std::env::var_os("FX_D2D_STATS").map(|_| {
            let now = Instant::now();
            Self {
                start: now,
                last_frame: None,
                last_report: now,
                draw: Vec::new(),
                flush: Vec::new(),
                present: Vec::new(),
                interval: Vec::new(),
                categories: Categories::default(),
                total: 0,
            }
        })
    }

    pub fn frame(
        &mut self,
        frame_start: Instant,
        flush: Duration,
        present: Duration,
        categories: Option<Categories>,
    ) {
        let end = Instant::now();
        if let Some(c) = categories {
            self.categories.rect += c.rect;
            self.categories.image += c.image;
            self.categories.text += c.text;
            self.categories.path += c.path;
            self.categories.shadow += c.shadow;
            self.categories.pixmap += c.pixmap;
        }
        let total = end.duration_since(frame_start).as_secs_f32() * 1000.0;
        let present_ms = present.as_secs_f32() * 1000.0;
        let flush_ms = flush.as_secs_f32() * 1000.0;
        self.draw.push((total - present_ms - flush_ms).max(0.0));
        self.flush.push(flush_ms);
        self.present.push(present_ms);
        if let Some(last) = self.last_frame {
            self.interval
                .push(end.duration_since(last).as_secs_f32() * 1000.0);
        }
        self.last_frame = Some(end);
        self.total += 1;
        if end.duration_since(self.last_report) >= Duration::from_secs(1) {
            self.report(end);
        }
    }

    fn report(&mut self, now: Instant) {
        let t = now.duration_since(self.start).as_secs_f32();
        eprintln!(
            "d2d stats +{t:5.1}s: {:3} frames | walk p50 {:5.2} p95 {:5.2} max {:5.2} | flush p50 {:5.2} p95 {:5.2} max {:5.2} | present p50 {:5.2} p95 {:5.2} max {:5.2} | interval p50 {:5.1} p95 {:5.1} max {:5.1} ms",
            self.draw.len(),
            pct(&mut self.draw, 0.5),
            pct(&mut self.draw, 0.95),
            pct(&mut self.draw, 1.0),
            pct(&mut self.flush, 0.5),
            pct(&mut self.flush, 0.95),
            pct(&mut self.flush, 1.0),
            pct(&mut self.present, 0.5),
            pct(&mut self.present, 0.95),
            pct(&mut self.present, 1.0),
            pct(&mut self.interval, 0.5),
            pct(&mut self.interval, 0.95),
            pct(&mut self.interval, 1.0),
        );
        let n = self.draw.len().max(1) as f32 * 1000.0;
        let c = self.categories;
        eprintln!(
            "           per frame: rect {:.2} image {:.2} text {:.2} path {:.2} shadow {:.2} pixmap {:.2} ms",
            c.rect as f32 / n,
            c.image as f32 / n,
            c.text as f32 / n,
            c.path as f32 / n,
            c.shadow as f32 / n,
            c.pixmap as f32 / n,
        );
        self.categories = Categories::default();
        self.draw.clear();
        self.flush.clear();
        self.present.clear();
        self.interval.clear();
        self.last_report = now;
    }
}

impl Drop for FrameStats {
    fn drop(&mut self) {
        if !self.draw.is_empty() {
            self.report(Instant::now());
        }
        eprintln!("d2d stats: {} frames in total", self.total);
    }
}
