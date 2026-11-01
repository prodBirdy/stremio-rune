//! Watch progress, written back to the Stremio account's library the way Stremio's own clients do
//! (`datastorePut` on the `libraryItem` collection), so Continue Watching, watch time and the
//! watched flag stay in sync with the official apps.
//!
//! The library is shared with those apps, so items are written whole and well-formed: the
//! account's copy is fetched first and only its watch state changes; a title watched for the
//! first time gets a complete new item, flagged `temp` as Stremio does for titles watched
//! without being added to the library.
//!
//! The library proper (added, not `temp`) is the Watchlist: titles go on and off it here too.

use super::*;
use std::time::SystemTime;

/// How often progress is saved while playing.
const SAVE_EVERY: Duration = Duration::from_secs(30);
/// Stremio's rule: watched once more than this share of the video has played.
const WATCHED_SHARE: f64 = 0.7;
/// Stopping past this share counts as finished: the title leaves Continue Watching.
const FINISHED_SHARE: f64 = 0.95;

/// The title being watched and the play time not yet saved.
pub struct Watch {
    id: String,
    video_id: String,
    /// The library item to write; `None` until the account's copy (or its absence) is known.
    item: Option<Value>,
    /// Played milliseconds since the last save.
    pending_ms: f64,
    last_tick: Instant,
    last_save: Instant,
}

/// Playback started: look up the title in the account's library.
pub fn start(s: &mut State) {
    let (Some(m), Some(key)) = (s.meta.clone(), s.auth.clone()) else { return };
    let id = m["id"].as_str().unwrap_or(&s.meta_id).to_string();
    let video_id = s.stream_for.clone();
    s.watch = Some(Watch { id: id.clone(), video_id, item: None, pending_ms: 0.0, last_tick: Instant::now(), last_save: Instant::now() });
    spawn(s, move || {
        let r = api("datastoreGet", json!({ "authKey": key, "collection": "libraryItem", "ids": [id.clone()], "all": false }))?;
        Ok(Msg::LibraryItem(id, r.as_array().and_then(|a| a.first().cloned()), m))
    });
}

/// The account's copy arrived (or there is none): settle the item this session writes.
pub fn landed(s: &mut State, id: String, existing: Option<Value>, meta: Value) {
    let Some(w) = s.watch.as_mut().filter(|w| w.id == id) else { return };
    let now = iso_now();
    let mut item = existing.unwrap_or_else(|| new_item(&meta, &now));
    // A different episode than last time starts its own watch count, as in Stremio.
    if item["state"]["video_id"].as_str() != Some(&w.video_id) {
        let st = &mut item["state"];
        st["video_id"] = w.video_id.clone().into();
        st["timeWatched"] = 0.into();
        st["timeOffset"] = 0.into();
        st["flaggedWatched"] = 0.into();
    }
    w.item = Some(item);
}

/// Called every poll while playing: count played time, save every `SAVE_EVERY`.
pub fn tick(s: &mut State) {
    let p = &s.playback;
    let playing = p.started && !p.paused && !p.loading();
    let Some(w) = s.watch.as_mut() else { return };
    let now = Instant::now();
    if playing { w.pending_ms += now.duration_since(w.last_tick).as_secs_f64() * 1000.0 }
    w.last_tick = now;
    if w.last_save.elapsed() >= SAVE_EVERY { save(s, false) }
}

/// Playback stopped: save where it stopped.
pub fn finish(s: &mut State) {
    tick(s);
    save(s, true);
    s.watch = None;
}

fn save(s: &mut State, stopping: bool) {
    let (pos, dur) = (s.playback.pos, s.playback.dur);
    let Some(w) = s.watch.as_mut() else { return };
    w.last_save = Instant::now();
    let Some(item) = w.item.as_mut() else { return }; // the account's copy hasn't arrived yet
    if dur <= 0.0 { return }
    apply(item, pos, dur, std::mem::take(&mut w.pending_ms), stopping, &iso_now());
    let item = item.clone();
    // Up Next reflects it at once; the account follows.
    let mut all: Vec<Value> = s.library.iter().filter(|i| i["_id"] != item["_id"]).cloned().collect();
    all.push(item.clone());
    s.library = continue_watching(all);
    if let Some(on) = s.saved.iter_mut().find(|i| i["_id"] == item["_id"]) { *on = item.clone() }
    let Some(key) = s.auth.clone() else { return };
    spawn(s, move || {
        api("datastorePut", json!({ "authKey": key, "collection": "libraryItem", "changes": [item] }))?;
        Ok(Msg::Saved)
    });
}

/// The Watchlist: what Stremio's Library lists (added, not removed, not just watched), the
/// latest change first.
pub fn listed(items: &[Value]) -> Vec<Value> {
    let mut v: Vec<Value> = items.iter().filter(|i| i["removed"] != true && i["temp"] != true && i["type"] != "other").cloned().collect();
    v.sort_by(|a, b| b["_mtime"].as_str().cmp(&a["_mtime"].as_str()));
    v
}

/// The open title's library id (its meta's id, as `start` writes it).
fn title_id(s: &State) -> String {
    s.meta.as_ref().and_then(|m| m["id"].as_str()).unwrap_or(&s.meta_id).to_string()
}

pub fn on_list(s: &State) -> bool {
    let id = title_id(s);
    s.saved.iter().any(|i| i["_id"] == id.as_str())
}

/// Put the open title on the Watchlist or take it off: here at once, then in the account, from
/// its copy there so the watch state is kept.
pub fn toggle(s: &mut State) {
    let Some(m) = s.meta.clone() else { return };
    let (id, add, now) = (title_id(s), !on_list(s), iso_now());
    if add {
        let mut item = new_item(&m, &now);
        mark(&mut item, true, &now);
        s.saved.insert(0, item);
    } else {
        s.saved.retain(|i| i["_id"] != id.as_str());
    }
    let Some(key) = s.auth.clone() else { return };
    spawn(s, move || {
        let r = api("datastoreGet", json!({ "authKey": key, "collection": "libraryItem", "ids": [id], "all": false }))?;
        let mut item = r.as_array().and_then(|a| a.first().cloned()).unwrap_or_else(|| new_item(&m, &now));
        mark(&mut item, add, &now);
        api("datastorePut", json!({ "authKey": key, "collection": "libraryItem", "changes": [item.clone()] }))?;
        Ok(Msg::Listed(item))
    });
}

/// On the list: not removed and no longer `temp`; off it: removed, as Stremio's Remove does.
fn mark(item: &mut Value, add: bool, now: &str) {
    item["removed"] = (!add).into();
    item["temp"] = false.into();
    item["_mtime"] = now.into();
}

/// The account took a Watchlist change: its item replaces the one shown.
pub fn listed_landed(s: &mut State, item: Value) {
    s.saved.retain(|i| i["_id"] != item["_id"]);
    if item["removed"] != true { s.saved.insert(0, item.clone()) }
    let mut all: Vec<Value> = s.library.iter().filter(|i| i["_id"] != item["_id"]).cloned().collect();
    all.push(item);
    s.library = continue_watching(all);
}

/// Fold a save into the item's watch state (times in milliseconds, as Stremio stores them).
fn apply(item: &mut Value, pos: f64, dur: f64, played_ms: f64, stopping: bool, now: &str) {
    let st = &mut item["state"];
    let num = |v: &Value| v.as_f64().unwrap_or(0.0);
    let finished = stopping && pos >= dur * FINISHED_SHARE;
    // Finished titles leave Continue Watching: Stremio resets the offset at the end.
    st["timeOffset"] = (if finished { 0 } else { (pos * 1000.0) as u64 }).into();
    st["duration"] = ((dur * 1000.0) as u64).into();
    st["timeWatched"] = ((num(&st["timeWatched"]) + played_ms) as u64).into();
    st["overallTimeWatched"] = ((num(&st["overallTimeWatched"]) + played_ms) as u64).into();
    st["lastWatched"] = now.into();
    let watched = num(&st["timeWatched"]) > dur * 1000.0 * WATCHED_SHARE || finished;
    if watched && num(&st["flaggedWatched"]) == 0.0 {
        st["flaggedWatched"] = 1.into();
        st["timesWatched"] = ((num(&st["timesWatched"]) + 1.0) as u64).into();
    }
    item["_mtime"] = now.into();
}

/// A complete library item for a title the account hasn't watched or added before.
fn new_item(m: &Value, now: &str) -> Value {
    let hints = &m["behaviorHints"];
    json!({
        "_id": m["id"],
        "name": m["name"].as_str().unwrap_or(""),
        "type": m["type"].as_str().unwrap_or("movie"),
        "poster": m["poster"].as_str(),
        "posterShape": m["posterShape"].as_str().unwrap_or("poster"),
        // Watched without being added: shows in Continue Watching, not in the library list.
        "removed": true,
        "temp": true,
        "_ctime": now,
        "_mtime": now,
        "state": {
            "lastWatched": now, "timeWatched": 0, "timeOffset": 0, "overallTimeWatched": 0,
            "timesWatched": 0, "flaggedWatched": 0, "duration": 0, "video_id": null,
            "watched": null, "noNotif": false,
        },
        "behaviorHints": {
            "defaultVideoId": hints["defaultVideoId"].as_str(),
            "featuredVideoId": hints["featuredVideoId"].as_str(),
            "hasScheduledVideos": hints["hasScheduledVideos"].as_bool().unwrap_or(false),
        },
    })
}

/// Now as RFC 3339 UTC with milliseconds (`2026-10-31T17:29:12.123Z`), as Stremio writes.
fn iso_now() -> String {
    iso(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64))
}

/// Unix milliseconds to RFC 3339 (civil-from-days, Howard Hinnant's algorithm).
fn iso(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso(1_793_467_752_123), "2026-10-31T17:29:12.123Z");
    }

    #[test]
    fn watchlist_is_the_added_items_and_marking_keeps_the_watch_state() {
        let meta = json!({ "id": "tt1", "type": "movie", "name": "X" });
        let mut watched = new_item(&meta, "t0");
        apply(&mut watched, 600.0, 6000.0, 600_000.0, false, "t1");
        assert!(listed(&[watched.clone()]).is_empty(), "watched without adding: not on the list");
        mark(&mut watched, true, "t2");
        assert_eq!((watched["removed"].as_bool(), watched["temp"].as_bool()), (Some(false), Some(false)));
        assert_eq!(watched["state"]["timeOffset"], 600_000, "progress kept");
        let mut older = new_item(&json!({ "id": "tt2", "type": "series", "name": "Y" }), "t0");
        mark(&mut older, true, "t1");
        let ids: Vec<_> = listed(&[older.clone(), watched.clone()]).iter().map(|i| i["_id"].clone()).collect();
        assert_eq!(ids, ["tt1", "tt2"], "latest change first");
        mark(&mut watched, false, "t3");
        assert_eq!(listed(&[watched]).len(), 0);
    }

    #[test]
    fn progress_watch_flag_and_finish() {
        let meta = json!({ "id": "tt1", "type": "movie", "name": "X", "poster": "p" });
        let mut item = new_item(&meta, "t0");
        apply(&mut item, 600.0, 6000.0, 600_000.0, false, "t1");
        let st = &item["state"];
        assert_eq!((st["timeOffset"].as_u64(), st["duration"].as_u64(), st["timeWatched"].as_u64()), (Some(600_000), Some(6_000_000), Some(600_000)));
        assert_eq!(st["flaggedWatched"], 0);
        assert_eq!(item["_mtime"], "t1");
        // Past 70 % of the runtime played: watched, counted once.
        apply(&mut item, 4500.0, 6000.0, 3_700_000.0, false, "t2");
        apply(&mut item, 4600.0, 6000.0, 100_000.0, false, "t3");
        assert_eq!((item["state"]["flaggedWatched"].as_u64(), item["state"]["timesWatched"].as_u64()), (Some(1), Some(1)));
        // Stopping at the credits finishes it: it leaves Continue Watching.
        apply(&mut item, 5900.0, 6000.0, 0.0, true, "t4");
        assert_eq!(item["state"]["timeOffset"], 0);
        assert!(continue_watching(vec![item.clone()]).is_empty());
        // The shape Stremio's clients parse.
        for k in ["_id", "name", "type", "posterShape", "removed", "temp", "_ctime", "_mtime", "state", "behaviorHints"] {
            assert!(item.get(k).is_some(), "{k}");
        }
    }
}
