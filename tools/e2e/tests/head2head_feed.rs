//! Head2head on the crew feed, end to end against a live local Nakama.
//!
//! Two fresh accounts share one crew. Each reports a library with
//! `library_report`, and the test reads `crew_feed` as account A and checks the
//! head2head entry and the recent games card. It then moves a belt, and sends
//! `game_session_end` with `tz_offset_min` to bring in night_owl.
//!
//! A second test, `session_titles_and_recap_belts_follow_the_activity_stats`,
//! drives the six session titles and the recap belts. It needs the dev_fault
//! RPC (`MELLO_ENABLE_DEV_FAULT=1`, which the script sets on its own stack) to
//! backdate voice joins and to generate the recap.
//!
//! A third test, `belt_and_join_pushes_reach_the_push_worker`, checks the belt
//! moved push and the friend landed push against a stub push Worker (spec 23).
//! It needs `MELLO_E2E_PUSH_PORT`, which the script sets. It takes a few seconds.
//!
//! The stack must serve THIS checkout's Nakama module, because the main
//! checkout may not have `library_report`. Run it with:
//!
//! ```text
//! ./scripts/e2e-head2head.sh              # own stack on port 17350, torn down after
//! ./scripts/e2e-head2head.sh --existing   # the stack already on 7350
//! ```
//!
//! The tests skip when `MELLO_E2E` is unset, like `backend.rs`. They delete
//! their accounts at the end. Each takes about 70 seconds: the session trigger
//! of the holders doc has a 60 second debounce (spec HEAD2HEAD section 4), and
//! the tests wait it out rather than forcing a recompute. They run in
//! parallel, so the total stays near 70 seconds.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{drain_sends, next_send_of_type, stub_push_worker};

use mello_core::nakama::client::NakamaClient;
use mello_core::Config;
use serde_json::{json, Value};

fn e2e_config() -> Config {
    Config::development().with_env_overrides()
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn random_device_id() -> String {
    format!("e2e-{:032x}", rand::random::<u128>())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis() as i64
}

async fn rpc_json(client: &NakamaClient, id: &str, payload: Value) -> Value {
    let raw = client
        .rpc(id, &payload)
        .await
        .unwrap_or_else(|e| panic!("rpc {id} failed: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("rpc {id} returned bad JSON ({e}): {raw}"))
}

fn game(id: &str, name: &str, playtime_min: i64) -> Value {
    json!({"game_id": id, "name": name, "launcher": "steam", "playtime_min": playtime_min})
}

/// The `this_week` entry of a type, or `None`.
fn this_week_entry(feed: &Value, entry_type: &str) -> Option<Value> {
    feed["sections"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == "this_week")?["entries"]
        .as_array()?
        .iter()
        .find(|e| e["type"] == entry_type)
        .cloned()
}

async fn feed_as(client: &NakamaClient, crew_id: &str) -> Value {
    rpc_json(client, "crew_feed", json!({"crew_id": crew_id})).await
}

async fn head2head_of(client: &NakamaClient, crew_id: &str) -> Value {
    let feed = feed_as(client, crew_id).await;
    this_week_entry(&feed, "head2head")
        .unwrap_or_else(|| panic!("the feed has no head2head entry: {feed}"))["data"]
        .clone()
}

fn title<'a>(h2h: &'a Value, id: &str) -> Option<&'a Value> {
    h2h["titles"]
        .as_array()?
        .iter()
        .find(|t| t["title_id"] == id)
}

fn pair_value<'a>(pair: &'a Value, id: &str) -> Option<&'a Value> {
    pair["values"]
        .as_array()?
        .iter()
        .find(|v| v["title_id"] == id)
}

/// A UTC offset in minutes that puts the local clock at `target_min` after
/// midnight right now. In -720..=719, so inside the server's -840..=840.
fn tz_for_local_minute(target_min: i64) -> i64 {
    let utc_min = (now_ms() / 60_000).rem_euclid(1440);
    (target_min - utc_min + 720).rem_euclid(1440) - 720
}

/// Replace the two user ids with A and B so the printed JSON is readable.
fn anonymise(v: &Value, a: &str, b: &str) -> String {
    serde_json::to_string_pretty(v)
        .expect("serialise")
        .replace(a, "A")
        .replace(b, "B")
}

#[test]
fn head2head_entry_follows_two_members_libraries_belts_and_sessions() {
    if std::env::var("MELLO_E2E").is_err() {
        println!(
            "SKIP head2head_entry_follows_two_members_libraries_belts_and_sessions: \
             set MELLO_E2E=1 and run ./scripts/e2e-head2head.sh"
        );
        return;
    }

    rt().block_on(async {
        // ---- two accounts, one crew ----
        let mut a = NakamaClient::new(e2e_config());
        let (user_a, _) = a
            .authenticate_device(&random_device_id())
            .await
            .expect("auth A");
        let mut b = NakamaClient::new(e2e_config());
        let (user_b, _) = b
            .authenticate_device(&random_device_id())
            .await
            .expect("auth B");
        let (ida, idb) = (user_a.id.clone(), user_b.id.clone());

        let (crew, _) = a
            .create_crew(
                &format!("E2E H2H {:x}", rand::random::<u32>()),
                "",
                true,
                None,
                &[],
            )
            .await
            .expect("create_crew");
        let code = a.create_invite_code(&crew.id).await.expect("invite code");
        b.join_by_invite_code(&code)
            .await
            .expect("B joins by invite code");

        // ---- crew of two, no data yet ----
        let h2h = head2head_of(&a, &crew.id).await;
        assert_eq!(h2h["member_count"], 2);
        assert_eq!(h2h["open_seats"], 3, "five seats, two taken");
        assert_eq!(h2h["titles"], json!([]), "no data, no contested title");
        assert!(h2h.get("belt_moved_recent").is_none());

        // ---- library reports ----
        // A: three games. Shared game steam-1000 at 100 h.
        // B: four games. The same shared game at 20 h.
        //
        // Title rules (spec HEAD2HEAD section 2), both members contend:
        //   the_carry      shared game minutes: A 6000 against B 1200, A holds.
        //   the_collector  installed games: A 3 against B 4, B holds.
        //   one_trick      top game share: A 6000/6360 = 94 %, B 1200/2400 = 50 %, A holds.
        // The week and night titles need session stats, so they are not there yet.
        let a_games = json!({"games": [
            game("steam-100", "Alpha", 300),
            game("steam-101", "Gamma", 60),
            game("steam-1000", "Shared Game", 6000),
        ]});
        let b_games = |shared_min: i64| {
            json!({"games": [
                game("steam-200", "Beta", 600),
                game("steam-201", "Delta", 300),
                game("steam-202", "Epsilon", 300),
                game("steam-1000", "Shared Game", shared_min),
            ]})
        };
        assert_eq!(rpc_json(&a, "library_report", a_games).await["count"], 3);
        assert_eq!(
            rpc_json(&b, "library_report", b_games(1200)).await["count"],
            4
        );

        let h2h = head2head_of(&a, &crew.id).await;
        let ids: Vec<&str> = h2h["titles"]
            .as_array()
            .expect("titles")
            .iter()
            .map(|t| t["title_id"].as_str().expect("title id"))
            .collect();
        assert_eq!(
            ids.len(),
            3,
            "the_carry, the_collector and one_trick are contested: {ids:?}"
        );
        assert_eq!(
            title(&h2h, "the_carry").expect("carry")["holder_user_id"],
            ida.as_str()
        );
        assert_eq!(title(&h2h, "the_carry").expect("carry")["value"], 100.0);
        assert_eq!(
            title(&h2h, "the_collector").expect("collector")["holder_user_id"],
            idb.as_str()
        );
        assert_eq!(title(&h2h, "the_collector").expect("collector")["value"], 4);
        assert_eq!(
            title(&h2h, "one_trick").expect("one trick")["holder_user_id"],
            ida.as_str()
        );
        assert_eq!(h2h["member_count"], 2);
        assert_eq!(h2h["open_seats"], 3);
        let pairs = h2h["pairs"].as_array().expect("pairs");
        assert_eq!(pairs.len(), 1, "one pair, and the viewer is A");
        assert_eq!(pairs[0]["a"], ida.as_str());
        assert_eq!(pairs[0]["b"], idb.as_str());
        assert_eq!(
            (pairs[0]["a_count"].clone(), pairs[0]["b_count"].clone()),
            (json!(2), json!(1))
        );
        let values = pairs[0]["values"].as_array().expect("pair values");
        assert_eq!(values.len(), 3, "one value for each counted title");
        let carry = pair_value(&pairs[0], "the_carry").expect("carry values");
        assert_eq!(
            (carry["a_value"].as_f64(), carry["b_value"].as_f64()),
            (Some(100.0), Some(20.0))
        );
        let collector = pair_value(&pairs[0], "the_collector").expect("collector values");
        assert_eq!(
            (collector["a_value"].clone(), collector["b_value"].clone()),
            (json!(3), json!(4))
        );
        // The first time a title is contested is not a move.
        assert!(
            h2h.get("belt_moved_recent").is_none(),
            "a title that becomes contested has not changed hands: {h2h}"
        );

        // B sees the same pair swapped.
        let h2h_b = head2head_of(&b, &crew.id).await;
        let pb = &h2h_b["pairs"][0];
        assert_eq!(pb["a"], idb.as_str());
        assert_eq!(
            (pb["a_count"].clone(), pb["b_count"].clone()),
            (json!(1), json!(2))
        );
        let carry_b = pair_value(pb, "the_carry").expect("carry values for B");
        assert_eq!(
            (carry_b["a_value"].as_f64(), carry_b["b_value"].as_f64()),
            (Some(20.0), Some(100.0))
        );

        // ---- the recent games card shows the union of both libraries ----
        let feed = feed_as(&a, &crew.id).await;
        let recent = this_week_entry(&feed, "recent-games").expect("recent games card");
        let mut got: Vec<String> = recent["data"]["games"]
            .as_array()
            .expect("games")
            .iter()
            .map(|g| g["game_id"].as_str().expect("game id").to_string())
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "steam-100",
                "steam-1000",
                "steam-101",
                "steam-200",
                "steam-201",
                "steam-202"
            ],
            "the union of the two libraries, the shared game once"
        );
        let shared = recent["data"]["games"]
            .as_array()
            .expect("games")
            .iter()
            .find(|g| g["game_id"] == "steam-1000")
            .expect("shared tile");
        assert_eq!(shared["member_ids"].as_array().expect("members").len(), 2);

        // ---- a belt moves: B takes the_carry ----
        // B reports 120 h in the shared game. The recompute sees the holder change.
        rpc_json(&b, "library_report", b_games(7200)).await;
        let h2h = head2head_of(&a, &crew.id).await;
        let carry_title = title(&h2h, "the_carry").expect("carry");
        assert_eq!(carry_title["holder_user_id"], idb.as_str());
        assert_eq!(carry_title["previous_holder_user_id"], ida.as_str());
        let moved = h2h.get("belt_moved_recent").unwrap_or_else(|| {
            panic!("belt_moved_recent must be set after a title changes hands: {h2h}")
        });
        assert_eq!(moved["title_id"], "the_carry");
        assert_eq!(moved["holder_user_id"], idb.as_str());
        assert!(now_ms() - moved["moved_at_ms"].as_i64().expect("moved_at_ms") < 60_000);
        // Carry now goes to B. one_trick: B 7200/8400 = 86 % against A 94 %, A holds.
        let pair = &h2h["pairs"][0];
        assert_eq!(
            (pair["a_count"].clone(), pair["b_count"].clone()),
            (json!(1), json!(2))
        );

        // ---- night_owl from game_session_end with tz_offset_min ----
        // A ended a session at about 18:05 local, B at about 03:00 local. The
        // title ranks the clock from 18:00, so B is later.
        let tz_a = tz_for_local_minute(18 * 60 + 5);
        let tz_b = tz_for_local_minute(3 * 60);
        let session = |tz: i64| {
            json!({
                "crew_id": crew.id, "game_name": "Shared Game", "game_id": "steam-1000",
                "duration_min": 30, "active_min": 30,
                "session_id": format!("e2e-{:x}", rand::random::<u64>()),
                "tz_offset_min": tz,
            })
        };
        rpc_json(&a, "game_session_end", session(tz_a)).await;
        // The session trigger skips while the doc is under 60 s old. Wait until
        // the last library report is older than that, so B's session end is
        // the recompute under test and not a skipped one.
        let doc_age_ms = now_ms() - h2h["updated_at"].as_i64().expect("updated_at");
        let wait_ms = (61_000 - doc_age_ms).max(0) as u64;
        println!("waiting {wait_ms} ms for the 60 s session debounce");
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        rpc_json(&b, "game_session_end", session(tz_b)).await;

        let h2h = head2head_of(&a, &crew.id).await;
        let owl = title(&h2h, "night_owl").unwrap_or_else(|| {
            panic!("night_owl must appear once both members have a clock: {h2h}")
        });
        assert_eq!(owl["holder_user_id"], idb.as_str());
        assert_eq!(owl["unit"], "");
        let clock = owl["value"].as_str().expect("night_owl value is HH:MM");
        assert!(
            clock.starts_with("03:0"),
            "B's clock is 03:00 or 03:01, got {clock}"
        );
        let pair = &h2h["pairs"][0];
        let owl_pair = pair_value(pair, "night_owl").expect("night_owl in the pair values");
        assert!(owl_pair["b_value"]
            .as_str()
            .expect("b clock")
            .starts_with("03:0"));
        assert!(owl_pair["a_value"]
            .as_str()
            .expect("a clock")
            .starts_with("18:"));
        let counted =
            pair["a_count"].as_i64().expect("a_count") + pair["b_count"].as_i64().expect("b_count");
        assert_eq!(
            counted,
            pair["values"].as_array().expect("values").len() as i64,
            "one value for each counted title"
        );

        // ---- print the entry, ids replaced ----
        println!(
            "HEAD2HEAD ENTRY (A = the viewer)\n{}",
            anonymise(&h2h, &ida, &idb)
        );

        let _ = b.delete_account().await;
        let _ = a.delete_account().await;
    });
}

/// One game session end. `session_id` is unique for each call.
fn session_payload(crew_id: &str, duration_min: i64, tz: i64) -> Value {
    json!({
        "crew_id": crew_id, "game_name": "Shared Game", "game_id": "steam-1000",
        "duration_min": duration_min, "active_min": duration_min,
        "session_id": format!("e2e-{:x}", rand::random::<u64>()),
        "tz_offset_min": tz,
    })
}

fn clip_payload(crew_id: &str) -> Value {
    json!({
        "crew_id": crew_id, "clip_id": format!("e2e-clip-{:x}", rand::random::<u64>()),
        "clip_type": "voice", "duration_seconds": 10,
    })
}

#[test]
fn session_titles_and_recap_belts_follow_the_activity_stats() {
    if std::env::var("MELLO_E2E").is_err() {
        println!(
            "SKIP session_titles_and_recap_belts_follow_the_activity_stats: \
             set MELLO_E2E=1 and run ./scripts/e2e-head2head.sh"
        );
        return;
    }

    rt().block_on(async {
        let mut a = NakamaClient::new(e2e_config());
        let (user_a, _) = a
            .authenticate_device(&random_device_id())
            .await
            .expect("auth A");
        let mut b = NakamaClient::new(e2e_config());
        let (user_b, _) = b
            .authenticate_device(&random_device_id())
            .await
            .expect("auth B");
        let (ida, idb) = (user_a.id.clone(), user_b.id.clone());
        let (crew, _) = a
            .create_crew(
                &format!("E2E H2H S {:x}", rand::random::<u32>()),
                "",
                true,
                None,
                &[],
            )
            .await
            .expect("create_crew");
        let code = a.create_invite_code(&crew.id).await.expect("invite code");
        b.join_by_invite_code(&code).await.expect("B joins");
        let cid = crew.id.as_str();

        // ---- phase 1: a first doc with a clipper held by A ----
        // A posts 2 clips and B posts 1. The library report is a trigger that
        // the debounce never skips, so the doc is computed now and A holds
        // `clipper`. The clip posts before it only write the counters or are
        // skipped by the debounce.
        rpc_json(&a, "post_clip", clip_payload(cid)).await;
        rpc_json(&a, "post_clip", clip_payload(cid)).await;
        rpc_json(&b, "post_clip", clip_payload(cid)).await;
        rpc_json(
            &a,
            "library_report",
            json!({"games": [game("steam-1000", "Shared Game", 600)]}),
        )
        .await;
        let first = head2head_of(&a, cid).await;
        assert_eq!(
            title(&first, "clipper").expect("clipper is contested")["holder_user_id"],
            ida.as_str()
        );
        let t1 = first["updated_at"].as_i64().expect("updated_at");

        // ---- phase 2: sessions, voice and moments. The debounce skips the recomputes. ----
        // A: two 3 minute sessions and one 90 minute session. B: one 3 minute
        // and one 40 minute session. A ends the later clock (22:xx against 20:xx).
        let tz_a = tz_for_local_minute(22 * 60 + 5);
        let tz_b = tz_for_local_minute(20 * 60 + 5);
        for min in [3, 3, 90] {
            rpc_json(&a, "game_session_end", session_payload(cid, min, tz_a)).await;
        }
        for min in [3, 40] {
            rpc_json(&b, "game_session_end", session_payload(cid, min, tz_b)).await;
        }

        // B's second clip. The recompute of this one is skipped too.
        rpc_json(&b, "post_clip", clip_payload(cid)).await;

        // Win moments: A 2, B 1. A loss moment counts for nobody.
        for _ in 0..2 {
            rpc_json(
                &a,
                "post_moment",
                json!({"crew_id": cid, "sentiment": "win", "text": "gg"}),
            )
            .await;
        }
        rpc_json(
            &b,
            "post_moment",
            json!({"crew_id": cid, "sentiment": "win", "text": "gg"}),
        )
        .await;
        rpc_json(
            &b,
            "post_moment",
            json!({"crew_id": cid, "sentiment": "loss", "text": "gg"}),
        )
        .await;

        // Voice. The call must take the SFU path, as the voice journeys in qa/
        // do: a P2P join counts as a failure, because P2P is being removed.
        // This test has no client, so no media runs. It drives the server voice
        // state with voice_join and voice_leave, and checks the join answers in
        // SFU mode. A is the crew admin and switches the crew to the SFU first.
        // The own stack of scripts/e2e-head2head.sh holds a throwaway signing
        // key, which SFU mode needs.
        //
        // TODO(e2e-sfu-media): no media goes through the SFU here. The media
        // path is covered by the voice journeys (qa/journeys/voice-two-users.ts),
        // which drive real clients against the local SFU. Move this to the
        // harness when it can read the holders doc.
        let enable = a.rpc("dev_enable_sfu_crew", &json!({"crew_id": cid})).await;
        if let Err(e) = enable {
            panic!(
                "dev_enable_sfu_crew failed ({e}). The stack needs SFU_JWT_PRIVATE_KEY. \
                 scripts/e2e-head2head.sh sets a throwaway key on its own stack"
            );
        }
        for (name, client) in [("A", &a), ("B", &b)] {
            let join = rpc_json(client, "voice_join", json!({"crew_id": cid})).await;
            assert_eq!(
                join["mode"], "sfu",
                "{name}: the voice join must use the SFU, not P2P: {join}"
            );
            assert!(
                join["sfu_token"].as_str().is_some_and(|t| !t.is_empty()),
                "{name}: an SFU join carries a token: {join}"
            );
        }
        for (client, user, min) in [(&a, &ida, 80), (&b, &idb, 60)] {
            let raw = client
                .rpc(
                    "dev_fault",
                    &json!({"action": "backdate_voice_join", "user_id": user, "count": min}),
                )
                .await;
            if let Err(e) = raw {
                panic!(
                    "dev_fault failed ({e}). The stack needs MELLO_ENABLE_DEV_FAULT=1. \
                     scripts/e2e-head2head.sh sets it on its own stack"
                );
            }
        }
        rpc_json(&b, "voice_leave", json!({"crew_id": cid})).await;
        rpc_json(&a, "voice_leave", json!({"crew_id": cid})).await;

        // ---- phase 3: wait out the debounce, then B's last clip recomputes ----
        let wait_ms = (61_000 - (now_ms() - t1)).max(0) as u64;
        println!("waiting {wait_ms} ms for the 60 s session debounce");
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        // B's third clip is the one recompute after the wait. B has 3 clips
        // against 2 and takes `clipper` from A. A second clip right after
        // would be skipped by the debounce again.
        rpc_json(&b, "post_clip", clip_payload(cid)).await;

        let h2h = head2head_of(&a, cid).await;
        let holder = |id: &str| -> String {
            title(&h2h, id).unwrap_or_else(|| panic!("{id} must be contested: {h2h}"))
                ["holder_user_id"]
                .as_str()
                .expect("holder")
                .to_string()
        };
        assert_eq!(holder("clipper"), idb, "B has 3 clips against 2");
        assert_eq!(title(&h2h, "clipper").expect("clipper")["value"], 3);
        assert_eq!(
            title(&h2h, "clipper").expect("clipper")["previous_holder_user_id"],
            ida.as_str()
        );
        assert_eq!(holder("rage_quit"), ida, "A has 2 short sessions against 1");
        assert_eq!(title(&h2h, "rage_quit").expect("rage_quit")["value"], 2);
        assert_eq!(holder("marathon"), ida, "A 90 min against B 40 min");
        assert_eq!(title(&h2h, "marathon").expect("marathon")["value"], 90);
        assert_eq!(holder("mouth"), ida, "A 80 voice minutes against B 60");
        let mouth = title(&h2h, "mouth").expect("mouth")["value"]
            .as_i64()
            .expect("mouth value");
        assert!((80..=81).contains(&mouth), "A mouth minutes: {mouth}");
        assert_eq!(holder("winner"), ida, "A 2 win moments against 1");
        assert_eq!(holder("last_one_up"), ida, "A ends at 22:xx, B at 20:xx");
        assert!(title(&h2h, "last_one_up").expect("last_one_up")["value"]
            .as_str()
            .expect("clock")
            .starts_with("22:"));

        // ---- the recap lists the belts ----
        let raw = a
            .rpc(
                "dev_fault",
                &json!({"action": "generate_recap", "crew_id": cid}),
            )
            .await
            .expect("generate_recap");
        assert!(raw.contains("\"success\":true"), "generate_recap: {raw}");
        let recaps = rpc_json(&a, "crew_recaps", json!({"crew_id": cid})).await;
        let recap = recaps["recaps"][0].clone();
        // The crew voice_session event is unchanged: it spans the real wall
        // time of the session, a minute or so here, not the backdated stints.
        assert!(
            recap["total_hangout_min"].as_i64().expect("hangout") >= 1,
            "the voice session gives the recap content: {recap}"
        );
        let moved = recap["belts_moved"].as_array().expect("belts_moved");
        let clipper_move = moved
            .iter()
            .find(|m| m["title_id"] == "clipper")
            .unwrap_or_else(|| panic!("clipper moved this week: {recap}"));
        assert_eq!(clipper_move["holder_user_id"], idb.as_str());
        assert_eq!(clipper_move["previous_holder_user_id"], ida.as_str());
        assert_eq!(clipper_move["value"], 3);
        assert!(
            moved.iter().all(|m| m["holder_name"].is_string()),
            "names resolved: {moved:?}"
        );
        let held = recap["belts_held"].as_array().expect("belts_held");
        let titles_of = |id: &str| -> Vec<String> {
            held.iter()
                .find(|h| h["user_id"] == id)
                .map(|h| {
                    h["title_ids"]
                        .as_array()
                        .expect("title_ids")
                        .iter()
                        .map(|t| t.as_str().expect("title id").to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        let (held_a, held_b) = (titles_of(&ida), titles_of(&idb));
        for id in ["mouth", "marathon", "last_one_up", "rage_quit", "winner"] {
            assert!(held_a.iter().any(|t| t == id), "A holds {id}: {held:?}");
        }
        assert!(
            held_b.iter().any(|t| t == "clipper"),
            "B holds clipper: {held:?}"
        );
        assert_eq!(
            held_a.len() + held_b.len(),
            h2h["titles"].as_array().expect("titles").len(),
            "every contested title has one holder: {held:?}"
        );
        for h in held {
            assert_eq!(
                h["count"].as_u64().expect("count"),
                h["title_ids"].as_array().expect("title_ids").len() as u64
            );
        }
        assert!(
            held[0]["count"].as_u64() >= held[1]["count"].as_u64(),
            "most belts first: {held:?}"
        );
        assert_eq!(recap["leaderboard"], recap["belts_held"]);
        assert!(
            recap["awards"]
                .as_array()
                .expect("awards")
                .iter()
                .any(|w| w["kind"] == "belt" && w["title"] == "Clipper"),
            "a belt award for the clipper move: {recap}"
        );

        println!(
            "RECAP BELTS (A, B replaced)\n{}",
            anonymise(
                &json!({"belts_moved": recap["belts_moved"], "belts_held": recap["belts_held"]}),
                &ida,
                &idb
            )
        );

        let _ = b.delete_account().await;
        let _ = a.delete_account().await;
    });
}

/// The name a push shows for a user: the display name, else the username.
fn push_name(u: &mello_core::events::User) -> String {
    if u.display_name.is_empty() {
        u.username.clone()
    } else {
        u.display_name.clone()
    }
}

/// The belt moved push and the friend landed push (HEAD2HEAD section 8), against
/// a live Nakama and a stub Worker.
///
/// 1. B takes the_carry from A with a second library report. A gets "<B> took
///    The Carry from you. You hold <n>." B caused the move and gets nothing.
/// 2. C joins the crew by invite code. A and B each get "<C> joined <crew>.
///    Say hi." C gets nothing.
///
/// Quiet hours (no push before 09:00 or from 22:00 local) would make this test
/// depend on the time of day. Every account first reports an offset that puts
/// its clock at 12:00.
#[test]
fn belt_and_join_pushes_reach_the_push_worker() {
    if std::env::var("MELLO_E2E").is_err() {
        println!(
            "SKIP belt_and_join_pushes_reach_the_push_worker: \
             set MELLO_E2E=1 and run ./scripts/e2e-head2head.sh"
        );
        return;
    }
    let Some(port) = std::env::var("MELLO_E2E_PUSH_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    else {
        println!(
            "SKIP belt_and_join_pushes_reach_the_push_worker: \
             run ./scripts/e2e-head2head.sh (push stub env unset)"
        );
        return;
    };
    let captured = stub_push_worker(port);

    rt().block_on(async {
        let mut a = NakamaClient::new(e2e_config());
        let (user_a, _) = a
            .authenticate_device(&random_device_id())
            .await
            .expect("auth A");
        let mut b = NakamaClient::new(e2e_config());
        let (user_b, _) = b
            .authenticate_device(&random_device_id())
            .await
            .expect("auth B");
        let mut c = NakamaClient::new(e2e_config());
        let (user_c, _) = c
            .authenticate_device(&random_device_id())
            .await
            .expect("auth C");

        let crew_name = format!("E2E Push {:x}", rand::random::<u32>());
        let (crew, _) = a
            .create_crew(&crew_name, "", true, None, &[])
            .await
            .expect("create_crew");
        let code = a.create_invite_code(&crew.id).await.expect("invite code");
        b.join_by_invite_code(&code).await.expect("B joins");

        // The offset comes from game_session_end. A and B report one so their
        // clocks read 12:00, then register a device.
        let noon = tz_for_local_minute(12 * 60);
        for client in [&a, &b] {
            rpc_json(
                client,
                "game_session_end",
                json!({
                    "crew_id": crew.id, "game_name": "Shared Game", "duration_min": 1,
                    "active_min": 1, "session_id": format!("e2e-{:x}", rand::random::<u64>()),
                    "tz_offset_min": noon,
                }),
            )
            .await;
        }
        a.register_push_token("a1a1a1a1", "ios", "sandbox")
            .await
            .expect("token A");
        b.register_push_token("b2b2b2b2", "ios", "sandbox")
            .await
            .expect("token B");
        c.register_push_token("c3c3c3c3", "ios", "sandbox")
            .await
            .expect("token C");
        // The session trigger of the holders doc may recompute later. No title
        // has a holder to lose yet, so it sends nothing. Drop anything that
        // came before the move under test.
        let _ = drain_sends(&captured, Duration::from_millis(500)).await;

        // ---- belt moved ----
        let games_a = json!({"games": [
            game("steam-100", "Alpha", 300),
            game("steam-1000", "Shared Game", 6000),
        ]});
        let games_b = |shared_min: i64| {
            json!({"games": [
                game("steam-200", "Beta", 600),
                game("steam-1000", "Shared Game", shared_min),
            ]})
        };
        rpc_json(&a, "library_report", games_a).await;
        rpc_json(&b, "library_report", games_b(1200)).await;
        let h2h = head2head_of(&a, &crew.id).await;
        assert_eq!(
            title(&h2h, "the_carry").expect("carry")["holder_user_id"],
            user_a.id.as_str()
        );
        rpc_json(&b, "library_report", games_b(7200)).await;

        let h2h = head2head_of(&a, &crew.id).await;
        assert_eq!(
            title(&h2h, "the_carry").expect("carry")["holder_user_id"],
            user_b.id.as_str(),
            "B took the_carry"
        );
        let held_by_a = h2h["titles"]
            .as_array()
            .expect("titles")
            .iter()
            .filter(|t| t["holder_user_id"] == user_a.id.as_str())
            .count();

        let got = next_send_of_type(&captured, "belt", "belt push to the previous holder").await;
        let n = &got.body["notification"];
        assert_eq!(n["crew_id"], crew.id.as_str());
        assert_eq!(n["title"], crew_name.as_str());
        assert_eq!(
            n["body"],
            format!(
                "{} took The Carry from you. You hold {held_by_a}.",
                push_name(&user_b)
            )
            .as_str()
        );
        assert!(
            n["message_id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("belt-")
                && n["message_id"].as_str().unwrap_or_default().len() <= 64,
            "collapse id: {}",
            n["message_id"]
        );
        assert_eq!(
            got.body["tokens"][0]["token"], "a1a1a1a1",
            "to the previous holder"
        );
        let extra = drain_sends(&captured, Duration::from_millis(1500)).await;
        assert!(
            extra
                .iter()
                .all(|s| s.body["notification"]["type"] != "belt"),
            "B caused the move and gets no belt push: {:?}",
            extra.iter().map(|s| s.body.to_string()).collect::<Vec<_>>()
        );

        // ---- friend landed ----
        c.join_by_invite_code(&code).await.expect("C joins");
        let first = next_send_of_type(&captured, "crew_join", "join push").await;
        let second = next_send_of_type(&captured, "crew_join", "join push").await;
        let want = format!("{} joined {crew_name}. Say hi.", push_name(&user_c));
        let mut tokens = Vec::new();
        for s in [&first, &second] {
            assert_eq!(s.body["notification"]["body"], want.as_str());
            assert_eq!(s.body["notification"]["crew_id"], crew.id.as_str());
            assert!(s.body["notification"]["message_id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("join-"));
            tokens.push(
                s.body["tokens"][0]["token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        tokens.sort();
        assert_eq!(tokens, ["a1a1a1a1", "b2b2b2b2"], "everyone but the joiner");
        let extra = drain_sends(&captured, Duration::from_millis(1000)).await;
        assert!(
            extra
                .iter()
                .all(|s| s.body["tokens"][0]["token"] != "c3c3c3c3"),
            "the joiner gets no push"
        );

        for client in [c, b, a] {
            let _ = client.delete_account().await;
        }
    });
}
