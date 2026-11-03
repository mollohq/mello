//! A live run of the upload against the local stack (CLIPS.md
//! 15.8): `post_clip`, the poster, the MP4 parts and both completes through the
//! real HTTP code, then the public objects with a range request. Ignored: it
//! needs the local Docker stack with this branch's backend and the seed users.
//!
//! ```text
//! cargo test -p mello-core --lib clip_upload::live_test -- --ignored --nocapture
//! ```
//!
//! `MELLO_LIVE_CLIP` is the path of an F8 clip (`.mp4`, with its `.jpg` next
//! to it); without it the test writes 7 MB of bytes. `MELLO_LIVE_RATE_BPS`
//! sets the pace cap.

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Instant;

use super::http::{HttpBackend, HttpStorage};
use super::pace::RateSource;
use super::queue::PendingClip;
use super::worker::{Step, Uploader};
use crate::config::Config;
use crate::nakama::NakamaClient;

const EMAIL: &str = "bob@test.com";
const PASSWORD: &str = "password123";

fn sign_in(config: &Config) -> (NakamaClient, tokio::runtime::Runtime, String, String) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut nk = NakamaClient::new(config.clone());
    let (user_id, crew_id) = rt.block_on(async {
        let user = nk.login_email(EMAIL, PASSWORD).await.expect("sign in");
        let crews = nk.list_user_groups().await.expect("crews");
        let crew = crews.first().expect("the seed user has a crew").id.clone();
        (user.id, crew)
    });
    (nk, rt, user_id, crew_id)
}

/// The clip files: an F8 clip, or bytes in a temp folder.
fn clip_files() -> (String, String) {
    if let Ok(mp4) = std::env::var("MELLO_LIVE_CLIP") {
        let poster = mp4.replace(".mp4", ".jpg");
        return (mp4, poster);
    }
    let dir = std::env::temp_dir().join("mello-clip-live-test");
    std::fs::create_dir_all(&dir).expect("dir");
    let mp4 = dir.join("clip.mp4");
    let poster = dir.join("clip.jpg");
    let bytes: Vec<u8> = (0..7_000_000u32).map(|i| (i * 7) as u8).collect();
    std::fs::write(&mp4, bytes).expect("mp4");
    std::fs::write(&poster, vec![0xFFu8; 50_000]).expect("jpg");
    (
        mp4.to_string_lossy().into_owned(),
        poster.to_string_lossy().into_owned(),
    )
}

#[test]
#[ignore = "needs the local Docker stack with this branch's backend"]
fn a_game_clip_goes_up_to_the_local_stack() {
    let config = Config::default();
    assert_eq!(config.nakama_host, "127.0.0.1", "the local stack only");
    let (nk, rt, user_id, crew_id) = sign_in(&config);

    let (mp4_path, poster_path) = clip_files();
    let mp4_bytes = std::fs::read(&mp4_path).expect("read the mp4");
    let poster_bytes = std::fs::read(&poster_path).expect("read the poster");
    let clip_id = format!("game_clip_live_{}", chrono::Utc::now().timestamp_millis());
    let mut clip = PendingClip {
        user_id,
        crew_id: crew_id.clone(),
        clip_id: clip_id.clone(),
        game: "Hearthstone".into(),
        mp4_path,
        poster_path,
        duration_seconds: 30.0,
        width: 1280,
        height: 720,
        has_voice: true,
        created_ms: chrono::Utc::now().timestamp_millis(),
        posted: false,
        poster_done: false,
        upload_id: String::new(),
        done_parts: Vec::new(),
    };

    let rate_bps: Option<u64> = std::env::var("MELLO_LIVE_RATE_BPS")
        .ok()
        .and_then(|v| v.parse().ok());
    let rate: RateSource = Arc::new(move || rate_bps);
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, _events) = mpsc::channel();
    let mut up = Uploader::new(
        HttpBackend::new(config.http_base(), nk.shared_token()),
        HttpStorage::new(rate.clone(), stop),
        rate,
        tx,
    );

    let started = Instant::now();
    let media_url = loop {
        match up.step(&mut clip) {
            Step::Progress => continue,
            Step::Finished(url) => break url,
            other => panic!("the upload stopped: {other:?}"),
        }
    };
    let secs = started.elapsed().as_secs_f64();
    let total = (mp4_bytes.len() + poster_bytes.len()) as f64;
    println!(
        "{clip_id}: {} part(s), {:.1} MB in {:.1} s ({:.1} Mbps), cap {:?}",
        clip.done_parts.len(),
        total / 1e6,
        secs,
        total * 8.0 / secs / 1e6,
        rate_bps
    );
    if let Some(bps) = rate_bps {
        let floor = total * 8.0 / bps as f64 * 0.9;
        assert!(secs >= floor, "{secs:.1} s is faster than the cap allows");
    }

    // The objects are public, immutable and serve byte ranges (CLIPS.md 15.9).
    let http = reqwest::blocking::Client::new();
    let resp = http.get(&media_url).send().expect("GET the mp4");
    assert!(
        resp.status().is_success(),
        "GET {media_url}: {}",
        resp.status()
    );
    let header = |r: &reqwest::blocking::Response, name: &str| {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(header(&resp, "content-type"), "video/mp4");
    assert!(header(&resp, "cache-control").contains("immutable"));
    assert!(resp.bytes().expect("body") == mp4_bytes, "the MP4 differs");

    let resp = http
        .get(&media_url)
        .header(reqwest::header::RANGE, "bytes=0-1048575")
        .send()
        .expect("range GET");
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(resp.bytes().expect("body").len(), 1_048_576);

    let poster_url = media_url.replace(".mp4", ".jpg");
    let resp = http.get(&poster_url).send().expect("GET the poster");
    assert_eq!(header(&resp, "content-type"), "image/jpeg");
    assert!(
        resp.bytes().expect("body") == poster_bytes,
        "the poster differs"
    );

    // The crew's clip carries both URLs and the game fields.
    let page = rt
        .block_on(nk.rpc("crew_clips", &serde_json::json!({ "crew_id": crew_id })))
        .expect("crew_clips");
    let page: serde_json::Value = serde_json::from_str(&page).expect("json");
    let stored = page["clips"]
        .as_array()
        .and_then(|clips| clips.iter().find(|c| c["clip_id"] == clip_id.as_str()))
        .unwrap_or_else(|| panic!("{clip_id} is not in the crew's clips"))
        .clone();
    println!("stored: {stored}");
    assert_eq!(stored["clip_type"], "game");
    assert_eq!(stored["media_url"], media_url.as_str());
    assert_eq!(stored["poster_url"], poster_url.as_str());
    assert_eq!(stored["width"], 1280);
    assert_eq!(stored["has_voice"], true);
}
