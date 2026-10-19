//! Head2head on the crew feed, end to end against a live local Nakama.
//!
//! Two fresh accounts share one crew. Each reports a library with
//! `library_report`, and the test reads `crew_feed` as account A and checks the
//! head2head entry and the recent games card. It then moves a belt, and sends
//! `game_session_end` with `tz_offset_min` to bring in night_owl.
//!
//! The stack must serve THIS checkout's Nakama module, because the main
//! checkout may not have `library_report`. Run it with:
//!
//! ```text
//! ./scripts/e2e-head2head.sh              # own stack on port 17350, torn down after
//! ./scripts/e2e-head2head.sh --existing   # the stack already on 7350
//! ```
//!
//! The test skips when `MELLO_E2E` is unset, like `backend.rs`. It deletes both
//! accounts at the end. It takes about 70 seconds: the session trigger of the
//! holders doc has a 60 second debounce (spec HEAD2HEAD section 4), and the
//! test waits it out rather than forcing a recompute.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
