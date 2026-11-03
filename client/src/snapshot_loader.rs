use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use slint::{Model, Weak};

use crate::snapshot_cache;
use crate::FeedCardData;
use crate::MainWindow;

const MAX_CONCURRENT: usize = 3;

/// Loads session snapshot JPEGs to disk and delivers thumb-sized images via feed row updates.
///
/// A poster is decoded only while its card is near the visible part of the
/// feed (`FeedView.poster-wanted` in types.slint), at the size the card draws
/// it, and dropped when the card moves away. The JPEGs stay on disk, so a card
/// that comes back decodes its poster again.
pub struct SnapshotLoader {
    client: reqwest::Client,
    generation: Arc<AtomicU32>,
    rt: tokio::runtime::Handle,
    semaphore: Arc<tokio::sync::Semaphore>,
    /// Cards that want their poster now. A decode that ends after its card
    /// moved away is not applied.
    wanted: Arc<Mutex<HashSet<String>>>,
    /// Cards with a poster decode under way, so a card asks only once.
    inflight: Arc<Mutex<HashSet<String>>>,
}

/// Takes the card off `inflight` when its poster task ends, on every path.
struct Inflight(Arc<Mutex<HashSet<String>>>, String);

impl Drop for Inflight {
    fn drop(&mut self) {
        self.0.lock().unwrap().remove(&self.1);
    }
}

impl SnapshotLoader {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self {
            client: reqwest::Client::new(),
            generation: Arc::new(AtomicU32::new(1)),
            rt,
            semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            wanted: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Invalidate in-flight work after crew switch or timeline reload.
    pub fn bump_generation(&self) -> u32 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current_generation(&self) -> u32 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Fetch frame-0 JPEG to disk only (no decode, no UI update).
    pub fn prefetch_disk(&self, url: String, gen: u32) {
        let client = self.client.clone();
        let sem = self.semaphore.clone();
        let generation = self.generation.clone();
        self.rt.spawn(async move {
            let _permit = sem.acquire().await.ok();
            if generation.load(Ordering::SeqCst) != gen {
                return;
            }
            let _ = snapshot_cache::prefetch_raw(&client, &url).await;
        });
    }

    /// A card came near the visible part of the feed (`wanted`), or moved
    /// away from it. `max_w` is the poster width it draws, in physical pixels.
    pub fn poster_wanted(
        &self,
        app_weak: Weak<MainWindow>,
        card_id: String,
        wanted: bool,
        max_w: u32,
    ) {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        // A feed rebuild gives a card new rows and a new instance, which
        // reports again: look at the row, not at what was asked before.
        let row = card_models(&app)
            .iter()
            .find_map(|cards| find_card_row(cards, &card_id).and_then(|i| cards.row_data(i)));
        let Some(row) = row else {
            return;
        };
        if !wanted {
            self.wanted.lock().unwrap().remove(&card_id);
            let holds = row.snapshot_poster_ready
                || row.snapshot_poster.size().width > 0
                || row.snapshot_playback_frame.size().width > 0;
            if holds {
                update_card(&app, &card_id, |card| {
                    card.snapshot_poster = slint::Image::default();
                    card.snapshot_playback_frame = slint::Image::default();
                    card.snapshot_poster_ready = false;
                    card.snapshot_loading = !card.snapshot_error;
                });
            }
            return;
        }
        self.wanted.lock().unwrap().insert(card_id.clone());
        if row.snapshot_poster_ready || !self.inflight.lock().unwrap().insert(card_id.clone()) {
            return;
        }
        let Some(url) = first_snapshot_url(&row) else {
            self.inflight.lock().unwrap().remove(&card_id);
            return;
        };
        let gen = self.current_generation();
        self.request_poster(app_weak, card_id, url, gen, max_w);
    }

    /// Load poster (frame 0), at most `max_w` wide, and update the feed card row.
    fn request_poster(
        &self,
        app_weak: Weak<MainWindow>,
        card_id: String,
        url: String,
        gen: u32,
        max_w: u32,
    ) {
        let client = self.client.clone();
        let sem = self.semaphore.clone();
        let generation = self.generation.clone();
        let wanted = self.wanted.clone();
        let inflight = Inflight(self.inflight.clone(), card_id.clone());
        self.rt.spawn(async move {
            let _inflight = inflight;
            let _permit = sem.acquire().await.ok();
            if generation.load(Ordering::SeqCst) != gen {
                return;
            }

            let raw = match snapshot_cache::ensure_raw_on_disk(&client, &url).await {
                Some(p) => p,
                None => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak.upgrade() {
                            set_card_error(&app, &card_id);
                        }
                    });
                    return;
                }
            };

            if generation.load(Ordering::SeqCst) != gen {
                return;
            }

            let rgba = match tokio::task::spawn_blocking(move || {
                let (bytes, w, h) = snapshot_cache::decode_thumb_rgba(&raw, &url)?;
                snapshot_cache::fit_width(bytes, w, h, max_w)
            })
            .await
            {
                Ok(Some(buf)) => buf,
                _ => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak.upgrade() {
                            set_card_error(&app, &card_id);
                        }
                    });
                    return;
                }
            };

            if generation.load(Ordering::SeqCst) != gen {
                return;
            }

            let _ = slint::invoke_from_event_loop(move || {
                if !wanted.lock().unwrap().contains(&card_id) {
                    return;
                }
                if let Some(app) = app_weak.upgrade() {
                    let (bytes, w, h) = rgba;
                    let img = snapshot_cache::rgba_bytes_to_image(bytes, w, h);
                    apply_poster(&app, &card_id, img);
                }
            });
        });
    }

    /// Load a playback frame and push it to the card row (manual slideshow).
    pub fn request_playback_frame(
        &self,
        app_weak: Weak<MainWindow>,
        card_id: String,
        url: String,
        index: i32,
        gen: u32,
    ) {
        let client = self.client.clone();
        let sem = self.semaphore.clone();
        let generation = self.generation.clone();
        self.rt.spawn(async move {
            let _permit = sem.acquire().await.ok();
            if generation.load(Ordering::SeqCst) != gen {
                return;
            }

            let raw = match snapshot_cache::ensure_raw_on_disk(&client, &url).await {
                Some(p) => p,
                None => return,
            };

            if generation.load(Ordering::SeqCst) != gen {
                return;
            }

            let rgba = match tokio::task::spawn_blocking(move || {
                snapshot_cache::decode_thumb_rgba(&raw, &url)
            })
            .await
            {
                Ok(Some(buf)) => buf,
                _ => return,
            };

            if generation.load(Ordering::SeqCst) != gen {
                return;
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    let (bytes, w, h) = rgba;
                    let img = snapshot_cache::rgba_bytes_to_image(bytes, w, h);
                    apply_playback_frame(&app, &card_id, index, img);
                }
            });
        });
    }

    /// After the feed is set: disk-prefetch frame 0 for session-preview cards
    /// in both the this_week grid and the memory band. The posters decode
    /// when the cards ask for them (`poster_wanted`).
    pub fn load_session_preview_cards(&self, app_weak: Weak<MainWindow>, gen: u32) {
        self.wanted.lock().unwrap().clear();
        if let Some(app) = app_weak.upgrade() {
            for cards in card_models(&app) {
                for i in 0..cards.row_count() {
                    let Some(mut card) = cards.row_data(i) else {
                        continue;
                    };
                    if card.card_type.as_str() != "session-preview" {
                        continue;
                    }
                    let url = first_snapshot_url(&card);
                    let Some(url) = url else {
                        continue;
                    };

                    card.snapshot_loading = true;
                    card.snapshot_poster_ready = false;
                    card.snapshot_error = false;
                    cards.set_row_data(i, card.clone());

                    self.prefetch_disk(url, gen);
                }
            }
        }
    }
}

/// Session-previews live in both the this_week grid (feed_cards) and the memory
/// band (memory_cards); snapshot work must reach either.
fn card_models(app: &MainWindow) -> [slint::ModelRc<FeedCardData>; 2] {
    [app.get_feed_cards(), app.get_memory_cards()]
}

/// Find the card by id across both models and mutate it in place.
fn update_card<F: FnOnce(&mut FeedCardData)>(app: &MainWindow, card_id: &str, f: F) {
    for cards in card_models(app) {
        if let Some(i) = find_card_row(&cards, card_id) {
            if let Some(mut card) = cards.row_data(i) {
                f(&mut card);
                cards.set_row_data(i, card);
            }
            return;
        }
    }
}

fn first_snapshot_url(card: &FeedCardData) -> Option<String> {
    let urls = &card.snapshot_urls;
    if urls.row_count() == 0 {
        return None;
    }
    urls.row_data(0).map(|u| u.to_string())
}

fn find_card_row(cards: &slint::ModelRc<FeedCardData>, card_id: &str) -> Option<usize> {
    (0..cards.row_count()).find(|&i| cards.row_data(i).is_some_and(|c| c.id.as_str() == card_id))
}

fn apply_poster(app: &MainWindow, card_id: &str, img: slint::Image) {
    update_card(app, card_id, |card| {
        card.snapshot_loading = false;
        card.snapshot_poster = img;
        card.snapshot_poster_ready = true;
        card.snapshot_error = false;
    });
    log::debug!("[snapshot] poster ready for {}", card_id);
}

fn set_card_error(app: &MainWindow, card_id: &str) {
    update_card(app, card_id, |card| {
        card.snapshot_loading = false;
        card.snapshot_poster_ready = false;
        card.snapshot_error = true;
    });
    log::warn!("[snapshot] poster failed for {}", card_id);
}

fn apply_playback_frame(app: &MainWindow, card_id: &str, index: i32, img: slint::Image) {
    update_card(app, card_id, |card| {
        card.snapshot_playback_frame = img;
        card.snapshot_playback_index = index;
        card.snapshot_playback_revision += 1;
    });
    log::debug!("[snapshot] playback frame {} ready for {}", index, card_id);
}

pub fn snapshot_url_for_card(app: &MainWindow, card_id: &str, index: usize) -> Option<String> {
    for cards in card_models(app) {
        let Some(i) = find_card_row(&cards, card_id) else {
            continue;
        };
        let card = cards.row_data(i)?;
        let urls = &card.snapshot_urls;
        if index >= urls.row_count() {
            return None;
        }
        return urls.row_data(index).map(|u| u.to_string());
    }
    None
}
