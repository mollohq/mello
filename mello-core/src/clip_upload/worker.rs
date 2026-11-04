//! The upload worker (CLIPS.md 15.7 and 15.8): one thread at low
//! priority takes each queued clip through `post_clip`, the poster, the MP4
//! parts and the complete, one request at a time. The queue file is saved
//! after each step, so a restart goes on from the last step.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::crew_events::{
    ClipUploadCompleteResponse, CompletedPart, GameClipUploadCompleteRequest,
    GameClipUploadUrlRequest, GameClipUploadUrlResponse, PostClipRequest, PostClipResponse,
    UploadPartUrl,
};
use crate::events::Event;
use crate::nakama::SharedToken;

use super::http::{HttpBackend, HttpStorage};
use super::pace::{self, RateSource, POSTER_FLOOR_BPS};
use super::queue::{PendingClip, UploadQueue};

/// Why a request did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Try again later: no network, a server error, no session yet.
    Retry(String),
    /// The server refuses for good: the clip drops.
    Refused(String),
    /// A presigned URL expired (403): ask for new URLs.
    Expired,
    /// The multipart upload is gone (404): the backend aborts one after 24 h.
    NoSuchUpload,
    /// The pace paused the upload during a part.
    Paused,
    /// The client shuts down.
    Stopped,
}

/// The three RPCs of CLIPS.md 15.8.
pub(crate) trait Backend {
    fn post_clip(&self, req: &PostClipRequest) -> Result<PostClipResponse, Failure>;
    fn upload_urls(
        &self,
        req: &GameClipUploadUrlRequest,
    ) -> Result<GameClipUploadUrlResponse, Failure>;
    fn complete(
        &self,
        req: &GameClipUploadCompleteRequest,
    ) -> Result<ClipUploadCompleteResponse, Failure>;
}

/// One PUT of a file range to a presigned URL.
pub(crate) struct Put<'a> {
    pub url: &'a str,
    pub path: &'a Path,
    pub offset: u64,
    pub len: u64,
    /// The poster signs its type and cache headers. A part signs its length only.
    pub content_type: Option<&'a str>,
    pub cache_control: Option<&'a str>,
    /// The poster never pauses (G1).
    pub floor_bps: u64,
}

pub(crate) trait Storage {
    /// Sends the range and returns the ETag.
    fn put(&self, put: &Put<'_>) -> Result<String, Failure>;
}

/// The result of one step of a clip.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The clip moved on: save it and take the next step.
    Progress,
    /// The MP4 is up, at this URL.
    Finished(String),
    /// The clip cannot go up. It leaves the queue; its local file stays.
    Drop(String),
    /// Try again after a wait.
    Retry(String),
    /// The pace is a pause: wait for it to end.
    Paused,
    Stopped,
}

impl From<Failure> for Step {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Retry(why) => Step::Retry(why),
            Failure::Refused(why) => Step::Drop(why),
            Failure::Expired => Step::Drop("the upload URL expired".into()),
            Failure::NoSuchUpload => Step::Drop("the upload is gone".into()),
            Failure::Paused => Step::Paused,
            Failure::Stopped => Step::Stopped,
        }
    }
}

/// The steps of one clip, over a backend and a storage.
pub(crate) struct Uploader<B, S> {
    backend: B,
    storage: S,
    rate: RateSource,
    events: mpsc::Sender<Event>,
    /// The URLs of each clip in this run. They expire after an hour, so the
    /// queue file does not keep them.
    urls: HashMap<String, GameClipUploadUrlResponse>,
    /// Clips whose URLs were renewed after a 403 or a 404 and not used since.
    /// A second refusal drops the clip, so a bad signature cannot loop.
    renewed: HashSet<String>,
}

impl<B: Backend, S: Storage> Uploader<B, S> {
    pub(crate) fn new(
        backend: B,
        storage: S,
        rate: RateSource,
        events: mpsc::Sender<Event>,
    ) -> Self {
        Self {
            backend,
            storage,
            rate,
            events,
            urls: HashMap::new(),
            renewed: HashSet::new(),
        }
    }

    /// The clip left the queue.
    pub(crate) fn forget(&mut self, clip_id: &str) {
        self.urls.remove(clip_id);
        self.renewed.remove(clip_id);
    }

    /// One request of `c`.
    pub(crate) fn step(&mut self, c: &mut PendingClip) -> Step {
        let (Ok(mp4), Ok(poster)) = (
            std::fs::metadata(&c.mp4_path),
            std::fs::metadata(&c.poster_path),
        ) else {
            return Step::Drop("the clip files are gone".into());
        };
        if !c.posted {
            return self.post(c);
        }
        let Some(urls) = self.urls.get(&c.clip_id).cloned() else {
            return self.request_urls(c, poster.len(), mp4.len());
        };
        if !c.poster_done {
            return self.send_poster(c, &urls, poster.len());
        }
        match next_part(c, &urls) {
            Some(part) => self.send_part(c, &urls, &part),
            None => self.complete_video(c),
        }
    }

    fn post(&mut self, c: &mut PendingClip) -> Step {
        let req = PostClipRequest {
            crew_id: c.crew_id.clone(),
            clip_id: c.clip_id.clone(),
            clip_type: "game".into(),
            duration_seconds: c.duration_seconds,
            participants: Vec::new(),
            game: c.game.clone(),
            local_path: c.mp4_path.clone(),
            waveform: String::new(),
            width: c.width,
            height: c.height,
            has_voice: c.has_voice,
        };
        match self.backend.post_clip(&req) {
            Ok(resp) => {
                c.posted = true;
                log::info!(
                    "[clip-upload] {} posted to crew {} (event {})",
                    c.clip_id,
                    c.crew_id,
                    resp.event_id
                );
                let _ = self.events.send(Event::ClipPosted {
                    clip_id: c.clip_id.clone(),
                    event_id: resp.event_id,
                });
                Step::Progress
            }
            Err(f) => f.into(),
        }
    }

    fn request_urls(&mut self, c: &mut PendingClip, poster_bytes: u64, video_bytes: u64) -> Step {
        let req = GameClipUploadUrlRequest {
            clip_id: c.clip_id.clone(),
            crew_id: c.crew_id.clone(),
            clip_type: "game".into(),
            poster_bytes,
            video_bytes,
            upload_id: c.upload_id.clone(),
        };
        match self.backend.upload_urls(&req) {
            Ok(urls) if urls.upload_id.is_empty() || urls.part_size == 0 => {
                Step::Drop("the server has no clip storage".into())
            }
            Ok(urls) => {
                if urls.upload_id != c.upload_id {
                    // A new multipart upload: the parts of an old one are gone.
                    log::info!(
                        "[clip-upload] {}: upload {} in {} part(s)",
                        c.clip_id,
                        urls.upload_id,
                        urls.parts.len()
                    );
                    c.upload_id = urls.upload_id.clone();
                    c.done_parts.clear();
                }
                self.urls.insert(c.clip_id.clone(), urls);
                Step::Progress
            }
            Err(f) => f.into(),
        }
    }

    fn send_poster(
        &mut self,
        c: &mut PendingClip,
        urls: &GameClipUploadUrlResponse,
        poster_bytes: u64,
    ) -> Step {
        let put = Put {
            url: &urls.poster_upload_url,
            path: Path::new(&c.poster_path),
            offset: 0,
            len: poster_bytes,
            content_type: Some("image/jpeg"),
            cache_control: Some(&urls.cache_control),
            floor_bps: POSTER_FLOOR_BPS,
        };
        match self.storage.put(&put) {
            Ok(_) => {}
            Err(Failure::Expired) => return self.renew(c, false),
            Err(f) => return f.into(),
        }
        let req = complete_request(c, "poster", Vec::new());
        match self.backend.complete(&req) {
            Ok(resp) => {
                c.poster_done = true;
                self.renewed.remove(&c.clip_id);
                log::info!(
                    "[clip-upload] {}: poster up at {}",
                    c.clip_id,
                    resp.poster_url
                );
                Step::Progress
            }
            Err(f) => f.into(),
        }
    }

    fn send_part(
        &mut self,
        c: &mut PendingClip,
        urls: &GameClipUploadUrlResponse,
        part: &UploadPartUrl,
    ) -> Step {
        // A part starts only when the pace lets it run (G3).
        if (self.rate)() == Some(0) {
            return Step::Paused;
        }
        let put = Put {
            url: &part.url,
            path: Path::new(&c.mp4_path),
            offset: u64::from(part.part_number.saturating_sub(1)) * urls.part_size,
            len: part.bytes,
            content_type: None,
            cache_control: None,
            floor_bps: 0,
        };
        match self.storage.put(&put) {
            Ok(etag) => {
                self.renewed.remove(&c.clip_id);
                c.done_parts.push(CompletedPart {
                    part_number: part.part_number,
                    etag,
                });
                log::info!(
                    "[clip-upload] {}: part {} of {} up",
                    c.clip_id,
                    part.part_number,
                    urls.parts.len()
                );
                Step::Progress
            }
            Err(Failure::Expired) => self.renew(c, false),
            Err(Failure::NoSuchUpload) => self.renew(c, true),
            Err(f) => f.into(),
        }
    }

    /// New URLs after a 403, or a new upload after a 404 (`restart`). A
    /// second refusal in a row drops the clip.
    fn renew(&mut self, c: &mut PendingClip, restart: bool) -> Step {
        if !self.renewed.insert(c.clip_id.clone()) {
            return Step::Drop("the bucket refuses the upload".into());
        }
        log::info!(
            "[clip-upload] {}: {}",
            c.clip_id,
            if restart {
                "the upload is gone, starting a new one"
            } else {
                "the URLs expired, asking for new ones"
            }
        );
        if restart {
            c.upload_id.clear();
            c.done_parts.clear();
        }
        self.urls.remove(&c.clip_id);
        Step::Progress
    }

    fn complete_video(&mut self, c: &mut PendingClip) -> Step {
        let mut parts = c.done_parts.clone();
        parts.sort_by_key(|p| p.part_number);
        let req = complete_request(c, "video", parts);
        match self.backend.complete(&req) {
            Ok(resp) => Step::Finished(resp.media_url),
            Err(f) => f.into(),
        }
    }
}

fn complete_request(
    c: &PendingClip,
    part: &str,
    parts: Vec<CompletedPart>,
) -> GameClipUploadCompleteRequest {
    GameClipUploadCompleteRequest {
        clip_id: c.clip_id.clone(),
        crew_id: c.crew_id.clone(),
        clip_type: "game".into(),
        part: part.into(),
        upload_id: if part == "video" {
            c.upload_id.clone()
        } else {
            String::new()
        },
        parts,
    }
}

/// The first part of the plan that is not up.
fn next_part(c: &PendingClip, urls: &GameClipUploadUrlResponse) -> Option<UploadPartUrl> {
    urls.parts
        .iter()
        .find(|p| !c.done_parts.iter().any(|d| d.part_number == p.part_number))
        .cloned()
}

/// The wait after `failures` failed steps in a row: 5 s, then double, at
/// most 5 min.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    let doubling = failures.saturating_sub(1).min(6);
    Duration::from_secs((5u64 << doubling).min(300))
}

/// How often a paused upload looks at the pace again.
const PAUSE_POLL: Duration = Duration::from_secs(1);

enum Msg {
    Enqueue(Box<PendingClip>),
    User(Option<String>),
}

/// The handle of the worker thread. Dropping it stops the thread at its next
/// write or request; it does not wait for it.
pub(crate) struct UploadWorker {
    tx: mpsc::Sender<Msg>,
    stop: Arc<AtomicBool>,
}

impl UploadWorker {
    pub(crate) fn start(
        events: mpsc::Sender<Event>,
        http_base: String,
        token: SharedToken,
    ) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("game-clip-upload".into())
            .spawn(move || {
                set_low_priority();
                let rate: RateSource = Arc::new(|| pace::pace().rate_bps());
                let backend = HttpBackend::new(http_base, token);
                let storage = HttpStorage::new(rate.clone(), thread_stop.clone());
                let uploader = Uploader::new(backend, storage, rate, events);
                let queue = UploadQueue::open_default(now_ms());
                log::info!(
                    "[clip-upload] worker started, {} clip(s) queued",
                    queue.len()
                );
                run(rx, uploader, queue, &thread_stop);
            })
            .map_err(|e| log::error!("[clip-upload] worker thread did not start: {e}"))
            .ok()?;
        Some(Self { tx, stop })
    }

    pub(crate) fn enqueue(&self, clip: PendingClip) {
        let _ = self.tx.send(Msg::Enqueue(Box::new(clip)));
    }

    /// The signed-in user. Only that user's clips go up.
    pub(crate) fn set_user(&self, user_id: Option<String>) {
        let _ = self.tx.send(Msg::User(user_id));
    }
}

impl Drop for UploadWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn apply(msg: Msg, queue: &mut UploadQueue, user: &mut Option<String>) {
    match msg {
        Msg::Enqueue(clip) => {
            log::info!(
                "[clip-upload] {} queued for crew {} ({} queued)",
                clip.clip_id,
                clip.crew_id,
                queue.len() + 1
            );
            queue.push(*clip);
        }
        Msg::User(id) => *user = id,
    }
}

fn run<B: Backend, S: Storage>(
    rx: mpsc::Receiver<Msg>,
    mut up: Uploader<B, S>,
    mut queue: UploadQueue,
    stop: &AtomicBool,
) {
    let mut user: Option<String> = None;
    let mut failures = 0u32;
    let mut paused = false;
    loop {
        while let Ok(msg) = rx.try_recv() {
            apply(msg, &mut queue, &mut user);
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Some(mut clip) = user.as_deref().and_then(|u| queue.next_for(u)) else {
            match rx.recv() {
                Ok(msg) => apply(msg, &mut queue, &mut user),
                Err(_) => return,
            }
            continue;
        };
        let step = up.step(&mut clip);
        if paused != (step == Step::Paused) {
            paused = !paused;
            log::info!(
                "[clip-upload] {}: {}",
                clip.clip_id,
                if paused { "paused" } else { "goes on" }
            );
        }
        let wait = match step {
            Step::Progress => {
                failures = 0;
                queue.update(&clip);
                None
            }
            Step::Finished(media_url) => {
                failures = 0;
                queue.remove(&clip.clip_id);
                up.forget(&clip.clip_id);
                log::info!("[clip-upload] {} is up: {}", clip.clip_id, media_url);
                let _ = up.events.send(Event::ClipUploaded {
                    clip_id: clip.clip_id,
                    media_url,
                });
                None
            }
            Step::Drop(reason) => {
                queue.remove(&clip.clip_id);
                up.forget(&clip.clip_id);
                log::warn!("[clip-upload] {} does not go up: {}", clip.clip_id, reason);
                let _ = up.events.send(Event::GameClipUploadFailed {
                    clip_id: clip.clip_id,
                    reason,
                });
                None
            }
            Step::Retry(why) => {
                failures += 1;
                let wait = retry_delay(failures);
                log::warn!(
                    "[clip-upload] {}: {} (try again in {} s)",
                    clip.clip_id,
                    why,
                    wait.as_secs()
                );
                Some(wait)
            }
            Step::Paused => Some(PAUSE_POLL),
            Step::Stopped => return,
        };
        if let Some(wait) = wait {
            match rx.recv_timeout(wait) {
                Ok(msg) => apply(msg, &mut queue, &mut user),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The upload thread runs under the game (CLIPS.md 15.7).
#[cfg(windows)]
fn set_low_priority() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    }
    const THREAD_PRIORITY_LOWEST: i32 = -2;
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) } == 0 {
        log::warn!("[clip-upload] could not lower the thread priority");
    }
}

/// macOS: the utility class, below the game and the voice threads.
#[cfg(target_os = "macos")]
fn set_low_priority() {
    if unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) } != 0
    {
        log::warn!("[clip-upload] could not lower the thread priority");
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn set_low_priority() {}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::clip;
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    const PART: u64 = 5 * 1024 * 1024;

    /// A backend that answers as the server of CLIPS.md 15.8 does.
    #[derive(Default)]
    struct FakeBackend {
        calls: RefCell<Vec<String>>,
        post: RefCell<VecDeque<Result<PostClipResponse, Failure>>>,
        upload_ids: RefCell<VecDeque<String>>,
        parts: u32,
    }

    impl FakeBackend {
        fn with_parts(parts: u32) -> Self {
            Self {
                parts,
                ..Default::default()
            }
        }
    }

    impl Backend for &FakeBackend {
        fn post_clip(&self, req: &PostClipRequest) -> Result<PostClipResponse, Failure> {
            self.calls.borrow_mut().push(format!(
                "post {} {}x{} voice={}",
                req.clip_type, req.width, req.height, req.has_voice
            ));
            self.post
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(PostClipResponse {
                    success: true,
                    event_id: "evt".into(),
                    clip_id: req.clip_id.clone(),
                }))
        }

        fn upload_urls(
            &self,
            req: &GameClipUploadUrlRequest,
        ) -> Result<GameClipUploadUrlResponse, Failure> {
            self.calls
                .borrow_mut()
                .push(format!("urls upload_id={}", req.upload_id));
            let upload_id = if req.upload_id.is_empty() {
                self.upload_ids
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_else(|| "up-1".into())
            } else {
                req.upload_id.clone()
            };
            Ok(GameClipUploadUrlResponse {
                poster_upload_url: "poster-url".into(),
                poster_url: "poster".into(),
                upload_id: upload_id.clone(),
                part_size: PART,
                parts: (1..=self.parts)
                    .map(|n| UploadPartUrl {
                        part_number: n,
                        url: format!("{upload_id}/part-{n}"),
                        bytes: PART,
                    })
                    .collect(),
                media_url: "media".into(),
                cache_control: "public, max-age=31536000, immutable".into(),
            })
        }

        fn complete(
            &self,
            req: &GameClipUploadCompleteRequest,
        ) -> Result<ClipUploadCompleteResponse, Failure> {
            let parts: Vec<String> = req
                .parts
                .iter()
                .map(|p| format!("{}:{}", p.part_number, p.etag))
                .collect();
            self.calls.borrow_mut().push(format!(
                "complete {} {} [{}]",
                req.part,
                req.upload_id,
                parts.join(",")
            ));
            Ok(ClipUploadCompleteResponse {
                success: true,
                media_url: "media".into(),
                poster_url: "poster".into(),
            })
        }
    }

    /// A storage that answers from a script, then with an ETag per URL.
    #[derive(Default)]
    struct FakeStorage {
        puts: RefCell<Vec<String>>,
        script: RefCell<VecDeque<Result<String, Failure>>>,
    }

    impl Storage for &FakeStorage {
        fn put(&self, put: &Put<'_>) -> Result<String, Failure> {
            self.puts.borrow_mut().push(format!(
                "{} @{} +{} type={:?} floor={}",
                put.url, put.offset, put.len, put.content_type, put.floor_bps
            ));
            self.script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(format!("etag-{}", put.url)))
        }
    }

    fn files(name: &str) -> (PathBuf, PendingClip) {
        let dir = std::env::temp_dir().join(format!(
            "mello-clip-worker-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let mut c = clip(name, "bob", 1);
        let mp4 = dir.join("clip.mp4");
        let poster = dir.join("clip.jpg");
        std::fs::write(&mp4, vec![0u8; 1000]).expect("mp4");
        std::fs::write(&poster, vec![0u8; 100]).expect("jpg");
        c.mp4_path = mp4.to_string_lossy().into_owned();
        c.poster_path = poster.to_string_lossy().into_owned();
        (dir, c)
    }

    fn uploader<'a>(
        backend: &'a FakeBackend,
        storage: &'a FakeStorage,
        rate: Option<u64>,
    ) -> (
        Uploader<&'a FakeBackend, &'a FakeStorage>,
        mpsc::Receiver<Event>,
    ) {
        let (tx, rx) = mpsc::channel();
        (
            Uploader::new(backend, storage, Arc::new(move || rate), tx),
            rx,
        )
    }

    /// Steps until the clip finishes or drops.
    fn run_clip(up: &mut Uploader<&FakeBackend, &FakeStorage>, c: &mut PendingClip) -> Step {
        for _ in 0..50 {
            match up.step(c) {
                Step::Progress => continue,
                other => return other,
            }
        }
        panic!("the clip did not finish");
    }

    #[test]
    fn a_clip_goes_up_in_order() {
        let (dir, mut c) = files("order");
        let backend = FakeBackend::with_parts(2);
        let storage = FakeStorage::default();
        let (mut up, events) = uploader(&backend, &storage, Some(3_000_000));

        assert_eq!(run_clip(&mut up, &mut c), Step::Finished("media".into()));
        assert_eq!(
            *backend.calls.borrow(),
            vec![
                "post game 1280x720 voice=true",
                "urls upload_id=",
                "complete poster  []",
                "complete video up-1 [1:etag-up-1/part-1,2:etag-up-1/part-2]",
            ]
        );
        assert_eq!(
            *storage.puts.borrow(),
            vec![
                "poster-url @0 +100 type=Some(\"image/jpeg\") floor=1000000",
                "up-1/part-1 @0 +5242880 type=None floor=0",
                "up-1/part-2 @5242880 +5242880 type=None floor=0",
            ]
        );
        assert!(matches!(
            events.try_recv(),
            Ok(Event::ClipPosted { event_id, .. }) if event_id == "evt"
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_restart_goes_on_from_the_last_part() {
        let (dir, mut c) = files("resume");
        c.posted = true;
        c.poster_done = true;
        c.upload_id = "up-old".into();
        c.done_parts.push(CompletedPart {
            part_number: 1,
            etag: "e1".into(),
        });
        let backend = FakeBackend::with_parts(2);
        let storage = FakeStorage::default();
        let (mut up, _events) = uploader(&backend, &storage, None);

        assert_eq!(run_clip(&mut up, &mut c), Step::Finished("media".into()));
        assert_eq!(
            *backend.calls.borrow(),
            vec![
                "urls upload_id=up-old",
                "complete video up-old [1:e1,2:etag-up-old/part-2]",
            ]
        );
        assert_eq!(
            *storage.puts.borrow(),
            vec!["up-old/part-2 @5242880 +5242880 type=None floor=0"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_urls_are_renewed_once() {
        let (dir, mut c) = files("expired");
        let backend = FakeBackend::with_parts(1);
        let storage = FakeStorage::default();
        storage
            .script
            .borrow_mut()
            .extend([Ok("p".into()), Err(Failure::Expired)]);
        let (mut up, _events) = uploader(&backend, &storage, None);
        assert_eq!(run_clip(&mut up, &mut c), Step::Finished("media".into()));
        let urls_calls = backend
            .calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("urls"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(urls_calls, vec!["urls upload_id=", "urls upload_id=up-1"]);

        // Two refusals in a row: the clip drops.
        let (dir2, mut c) = files("expired-twice");
        let backend = FakeBackend::with_parts(1);
        let storage = FakeStorage::default();
        storage.script.borrow_mut().extend([
            Ok("p".into()),
            Err(Failure::Expired),
            Err(Failure::Expired),
        ]);
        let (mut up, _events) = uploader(&backend, &storage, None);
        assert_eq!(
            run_clip(&mut up, &mut c),
            Step::Drop("the bucket refuses the upload".into())
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn a_gone_upload_starts_again_from_part_one() {
        let (dir, mut c) = files("gone");
        c.posted = true;
        c.poster_done = true;
        c.upload_id = "up-old".into();
        c.done_parts.push(CompletedPart {
            part_number: 1,
            etag: "e1".into(),
        });
        let backend = FakeBackend::with_parts(2);
        backend.upload_ids.borrow_mut().push_back("up-new".into());
        let storage = FakeStorage::default();
        storage
            .script
            .borrow_mut()
            .push_back(Err(Failure::NoSuchUpload));
        let (mut up, _events) = uploader(&backend, &storage, None);
        assert_eq!(run_clip(&mut up, &mut c), Step::Finished("media".into()));
        assert_eq!(
            backend.calls.borrow().last().map(String::as_str),
            Some("complete video up-new [1:etag-up-new/part-1,2:etag-up-new/part-2]")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pause_holds_the_parts_and_not_the_poster() {
        let (dir, mut c) = files("pause");
        let backend = FakeBackend::with_parts(1);
        let storage = FakeStorage::default();
        let (mut up, _events) = uploader(&backend, &storage, Some(0));
        assert_eq!(run_clip(&mut up, &mut c), Step::Paused);
        assert!(c.poster_done, "the poster goes in every state (G1)");
        assert!(c.done_parts.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_post_drops_the_clip_and_a_network_error_retries() {
        let (dir, mut c) = files("refused");
        let backend = FakeBackend::with_parts(1);
        backend.post.borrow_mut().extend([
            Err(Failure::Retry("offline".into())),
            Err(Failure::Refused("clip rate limit exceeded".into())),
        ]);
        let storage = FakeStorage::default();
        let (mut up, _events) = uploader(&backend, &storage, None);
        assert_eq!(up.step(&mut c), Step::Retry("offline".into()));
        assert!(!c.posted);
        assert_eq!(
            up.step(&mut c),
            Step::Drop("clip rate limit exceeded".into())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_files_drop_the_clip() {
        let backend = FakeBackend::with_parts(1);
        let storage = FakeStorage::default();
        let (mut up, _events) = uploader(&backend, &storage, None);
        let mut c = clip("missing", "bob", 1);
        assert_eq!(
            up.step(&mut c),
            Step::Drop("the clip files are gone".into())
        );
        assert!(backend.calls.borrow().is_empty());
    }

    #[test]
    fn the_loop_uploads_only_for_the_signed_in_user() {
        let (dir, mine) = files("loop-mine");
        let (dir2, mut theirs) = files("loop-theirs");
        theirs.user_id = "alice".into();
        let backend = FakeBackend::with_parts(1);
        let storage = FakeStorage::default();
        let (up, events) = uploader(&backend, &storage, None);
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Enqueue(Box::new(theirs))).expect("send");
        tx.send(Msg::Enqueue(Box::new(mine))).expect("send");
        tx.send(Msg::User(Some("bob".into()))).expect("send");
        drop(tx);
        let stop = AtomicBool::new(false);
        run(rx, up, UploadQueue::open(None, 0), &stop);

        let uploaded: Vec<String> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::ClipUploaded { clip_id, .. } => Some(clip_id),
                _ => None,
            })
            .collect();
        assert_eq!(uploaded, vec!["loop-mine".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn retries_back_off_to_five_minutes() {
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(10));
        assert_eq!(retry_delay(7), Duration::from_secs(300));
        assert_eq!(retry_delay(50), Duration::from_secs(300));
    }
}
