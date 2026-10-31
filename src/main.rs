//! Native Stremio client on Rune. Logs in to api.strem.io, loads your addon collection and
//! library, browses catalogs, lists streams from your addons, and plays them through Stremio's
//! local streaming server (127.0.0.1:11470, started by the Stremio app) with the libmpv Stremio
//! ships. Views live in `ui.rs`, playback in `player.rs`.

mod player;
mod ui;
mod window;

use player::{Mpv, Playback};
use rune::{AppSpec, Image, ImageCache, Key, KeyEvent, TextEdit, Tx, Waker};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Height of the control bar under the video, and width of the subtitles panel, in points.
const BAR: f32 = 64.0;
const SUBS_W: f32 = 380.0;
/// Mouse stillness after which the playback controls hide.
const IDLE: Duration = Duration::from_millis(2500);
/// Catalogs shown as rows on Home, and cards per row (Rune has no horizontal scroll yet).
const HOME_ROWS: usize = 6;
const ROW_CARDS: usize = 5;
/// Titles the Home hero cycles through.
const HERO_PICKS: usize = 5;

/// Decoded image pixels kept; least recently used go first past this. Rune keeps a mip chain of
/// each on the GPU too (~4/3 of this).
const IMAGE_BUDGET: usize = 160 << 20;
/// Image fetch threads: enough to hide latency, few enough that the first images aren't starved.
const IMAGE_WORKERS: usize = 6;

/// Image bytes fetched so far, for `measure_home_images`.
static DOWNLOADED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const API: &str = "https://api.strem.io/api/";
const SERVER: &str = "http://127.0.0.1:11470";

enum Msg {
    Error(String),
    Auth(String),
    User(String),
    Addons(Vec<Value>),
    Library(Vec<Value>),
    Catalog(usize, Vec<Value>),
    Image(String, image::RgbaImage),
    Meta(String, Value),
    Streams(String, Vec<Stream>),
    /// Addon subtitles for a video: (language code, url).
    Subs(String, Vec<(String, String)>),
}

struct Stream { name: String, title: String, url: String }

struct Catalog { base: String, ty: String, id: String, name: String }

#[derive(Clone, Copy, PartialEq)]
enum Page { Login, Home, Browse(&'static str), Detail, Player }

struct Icons { play: Image, pause: Image, back: Image, close: Image, expand: Image, shrink: Image, subs: Image }

struct State {
    page: Page,
    email: TextEdit,
    password: TextEdit,
    auth: Option<String>,
    user: String,
    addons: Vec<Value>,
    catalogs: Vec<Catalog>,
    /// Catalog shown on a Browse page.
    selected: usize,
    items: HashMap<usize, Vec<Value>>,
    /// Continue watching, newest first.
    library: Vec<Value>,
    hero: usize,
    images: HashMap<String, Image>,
    /// Decoded bytes and last use of each image in `images`, for `evict`.
    lru: HashMap<String, (usize, u64)>,
    tick: u64,
    /// Images loaded, loading or queued.
    requested: HashSet<String>,
    /// Images waiting for a worker, best first, and the page they were wanted for.
    queue: Arc<(Mutex<VecDeque<String>>, Condvar)>,
    queue_page: Page,
    /// Height of the window in points, for what counts as near the viewport.
    view_h: f32,
    meta_id: String,
    meta: Option<Value>,
    season: i64,
    stream_for: String,
    streams: Vec<Stream>,
    /// (video id, seconds) to start at when that video plays, from Continue Watching.
    resume: Option<(String, f64)>,
    status: String,
    /// The streams side sheet on the detail page.
    sheet: bool,
    /// The profile menu under the avatar.
    menu: bool,
    fullscreen: bool,
    /// Playback controls: shown while the mouse moves (see `poll`).
    controls: bool,
    cursor: (i32, i32),
    last_move: Instant,
    bar_hover: bool,
    subs_open: bool,
    subs_for: String,
    addon_subs: Vec<(String, String)>,
    player: Option<Mpv>,
    playback: Playback,
    icons: Option<Icons>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    waker: Option<Waker>,
    cache: Option<ImageCache>,
}

fn auth_file() -> PathBuf {
    let dir = std::env::var_os("APPDATA").or_else(|| std::env::var_os("HOME")).unwrap_or_default();
    PathBuf::from(dir).join("stremio-rune").join("authKey")
}

/// The session of the installed Stremio desktop app (stremio-web's `profile` in WebView2
/// localStorage), so a signed-in Stremio needs no second login.
fn stremio_desktop_auth() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join(r"Programs\Stremio\stremio-shell-ng.exe.WebView2\EBWebView\Default\Local Storage\leveldb");
    let mut files: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    // byte scan of LevelDB, newest record wins; misses a key only in a snappy-compressed table
    files.iter().rev().filter_map(|p| std::fs::read(p).ok()).find_map(|b| last_auth_key(&b))
}

/// The last `"auth":{"key":"…"` in a blob, unless a later `"auth":null` says it signed out.
fn last_auth_key(bytes: &[u8]) -> Option<String> {
    let find = |n: &[u8]| bytes.windows(n.len()).rposition(|w| w == n);
    let needle = br#""auth":{"key":""#;
    let at = find(needle)?;
    if find(br#""auth":null"#).is_some_and(|out| out > at) { return None }
    let key: String = bytes[at + needle.len()..].iter().take_while(|b| b.is_ascii_alphanumeric()).map(|&b| b as char).collect();
    (key.len() >= 16).then_some(key)
}

fn state() -> State {
    let (tx, rx) = channel();
    let auth = std::fs::read_to_string(auth_file()).ok().filter(|k| !k.is_empty()).or_else(stremio_desktop_auth);
    State {
        page: if auth.is_some() { Page::Home } else { Page::Login },
        email: TextEdit::new(""),
        password: TextEdit::new(""),
        auth,
        user: String::new(),
        addons: vec![],
        catalogs: vec![],
        selected: 0,
        items: HashMap::new(),
        library: vec![],
        hero: 0,
        images: HashMap::new(),
        lru: HashMap::new(),
        tick: 0,
        requested: HashSet::new(),
        queue: Default::default(),
        queue_page: Page::Login,
        view_h: 800.0,
        meta_id: String::new(),
        meta: None,
        season: 1,
        stream_for: String::new(),
        streams: vec![],
        resume: None,
        status: String::new(),
        sheet: false,
        menu: false,
        fullscreen: false,
        controls: true,
        cursor: (0, 0),
        last_move: Instant::now(),
        bar_hover: false,
        subs_open: false,
        subs_for: String::new(),
        addon_subs: vec![],
        player: None,
        playback: Playback::default(),
        icons: None,
        tx,
        rx,
        waker: None,
        cache: None,
    }
}

// ---- data ----

fn get(url: &str) -> Result<Value, String> {
    ureq::get(url).call().map_err(|e| e.to_string())?.body_mut().read_json().map_err(|e| e.to_string())
}

fn api(method: &str, body: Value) -> Result<Value, String> {
    let v: Value = ureq::post(format!("{API}{method}")).send_json(body).map_err(|e| e.to_string())?
        .body_mut().read_json().map_err(|e| e.to_string())?;
    match v.get("error") {
        Some(e) if !e.is_null() => Err(e["message"].as_str().unwrap_or("Stremio API error").into()),
        _ => Ok(v["result"].clone()),
    }
}

/// Run `job` off the main thread; its message lands in `poll`.
fn spawn(s: &State, job: impl FnOnce() -> Result<Msg, String> + Send + 'static) {
    let (tx, wake) = (s.tx.clone(), s.waker.clone());
    std::thread::spawn(move || {
        let _ = tx.send(job().unwrap_or_else(Msg::Error));
        if let Some(w) = wake { w() }
    });
}

fn base(addon: &Value) -> String {
    addon["transportUrl"].as_str().unwrap_or("").trim_end_matches("/manifest.json").to_string()
}

/// Whether an addon serves `resource` for this type and id (Stremio's manifest rules).
fn supports(addon: &Value, resource: &str, ty: &str, id: &str) -> bool {
    let m = &addon["manifest"];
    let or_m = |r: &Value, k: &str| if r[k].is_null() { m[k].clone() } else { r[k].clone() };
    m["resources"].as_array().into_iter().flatten().any(|r| {
        let (name, types, prefixes) = match r.as_str() {
            Some(n) => (n.to_string(), m["types"].clone(), m["idPrefixes"].clone()),
            None => (r["name"].as_str().unwrap_or("").to_string(), or_m(r, "types"), or_m(r, "idPrefixes")),
        };
        name == resource
            && types.as_array().is_some_and(|t| t.iter().any(|t| t.as_str() == Some(ty)))
            && prefixes.as_array().is_none_or(|p| p.iter().any(|p| p.as_str().is_some_and(|p| id.starts_with(p))))
    })
}

/// Direct URL, or a torrent through the local streaming server (-1 = largest file).
fn stream_url(st: &Value) -> Option<String> {
    if let Some(u) = st["url"].as_str() { return Some(u.to_string()) }
    let hash = st["infoHash"].as_str()?;
    Some(format!("{SERVER}/{hash}/{}", st["fileIdx"].as_i64().unwrap_or(-1)))
}

fn type_label(ty: &str) -> &str {
    match ty { "movie" => "Movies", "series" => "Shows", "channel" => "Channels", "tv" => "TV", other => other }
}

fn catalogs(addons: &[Value]) -> Vec<Catalog> {
    addons.iter().flat_map(|a| {
        let b = base(a);
        a["manifest"]["catalogs"].as_array().cloned().unwrap_or_default().into_iter()
            .filter(|c| !c["extra"].as_array().into_iter().flatten().any(|e| e["isRequired"] == true)
                && c["extraRequired"].as_array().is_none_or(|r| r.is_empty()))
            .map(move |c| {
                let ty = c["type"].as_str().unwrap_or("").to_string();
                let name = format!("{} {}", c["name"].as_str().unwrap_or("?"), type_label(&ty));
                Catalog { base: b.clone(), id: c["id"].as_str().unwrap_or("").into(), ty, name }
            })
    }).collect()
}

/// Continue watching as Stremio defines it: started, not removed (or watched without adding),
/// newest first.
fn continue_watching(items: Vec<Value>) -> Vec<Value> {
    let mut v: Vec<Value> = items.into_iter()
        .filter(|i| i["type"] != "other" && (i["removed"] != true || i["temp"] == true)
            && i["state"]["timeOffset"].as_f64().unwrap_or(0.0) > 0.0)
        .collect();
    v.sort_by(|a, b| b["state"]["lastWatched"].as_str().cmp(&a["state"]["lastWatched"].as_str()));
    v
}

/// A landscape image for a title: its background, metahub's by IMDb id, or the poster.
fn backdrop(m: &Value) -> Option<String> {
    let id = m["id"].as_str().or(m["_id"].as_str()).unwrap_or("");
    let field = |k: &str| m[k].as_str().filter(|u| !u.is_empty()).map(String::from);
    field("background")
        .or_else(|| id.starts_with("tt").then(|| format!("https://images.metahub.space/background/medium/{}/img", id.split(':').next().unwrap_or(id))))
        .or_else(|| field("poster"))
}

/// The title's logo artwork, for the loader.
fn logo(m: &Value) -> Option<String> {
    let id = m["id"].as_str().or(m["_id"].as_str()).unwrap_or("");
    m["logo"].as_str().filter(|u| !u.is_empty()).map(String::from)
        .or_else(|| id.starts_with("tt").then(|| format!("https://images.metahub.space/logo/medium/{}/img", id.split(':').next().unwrap_or(id))))
}

/// Where to look when an image URL has nothing: a missing metahub backdrop falls back to the
/// poster.
fn fallbacks(url: &str) -> Vec<String> {
    let mut v = vec![url.to_string()];
    if url.contains("metahub.space/background/") { v.push(url.replace("/background/", "/poster/")) }
    v
}

/// `(season, episode)` from a video id like `tt123:2:5`.
fn episode_of(video_id: &str) -> Option<(i64, i64)> {
    let mut parts = video_id.rsplitn(3, ':');
    let ep = parts.next()?.parse().ok()?;
    let season = parts.next()?.parse().ok()?;
    parts.next()?;
    Some((season, ep))
}

/// The image key for `url` shown `pt` points wide: metahub's smallest variant that covers it at
/// the display scale (2× at most), and the width to decode to after `#w`.
fn sized(url: &str, pt: f32) -> String {
    sized_px(url, (pt * window::scale().min(2.0)).round() as u32)
}

fn sized_px(url: &str, px: u32) -> String {
    // Measured: background small 480 wide, medium = large 1920; poster 300, 500, 780. A tenth
    // short of the width still looks sharp.
    let fit = |sizes: &[(&'static str, u32)]| sizes.iter().find(|s| s.1 as f32 >= px as f32 * 0.9).unwrap_or(sizes.last().unwrap()).0;
    let mut parts: Vec<&str> = url.split('/').collect(); // https://images.metahub.space/{kind}/{size}/{id}/img
    if parts.len() > 4 && parts[2].ends_with("metahub.space") {
        match parts[3] {
            "background" => parts[4] = fit(&[("small", 480), ("medium", 1920)]),
            "poster" => parts[4] = fit(&[("small", 300), ("medium", 500), ("large", 780)]),
            _ => {}
        }
    }
    format!("{}#w{px}", parts.join("/"))
}

/// Fetch and decode an image key (a URL, sized by `sized` or else capped at 1280 px wide).
/// Decoding here also covers WebP, which metahub serves and Rune's ImageCache can't decode on
/// desktop.
fn load_image(key: &str) -> Option<image::RgbaImage> {
    let (url, w) = key.rsplit_once("#w").and_then(|(u, w)| Some((u, w.parse().ok()?))).unwrap_or((key, 1280));
    let fetch = |u: &String| {
        let bytes = ureq::get(u).call().ok()?.body_mut().read_to_vec().ok()?;
        DOWNLOADED.fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
        image::load_from_memory(&bytes).ok()
    };
    let img = fallbacks(url).iter().find_map(fetch)?;
    Some(if img.width() > w { img.thumbnail(w, w * 4) } else { img }.to_rgba8())
}

/// The fetch pool: workers take the queue's front, so the first wanted loads first.
fn start_image_workers(s: &State) {
    for _ in 0..IMAGE_WORKERS {
        let (queue, tx, wake) = (s.queue.clone(), s.tx.clone(), s.waker.clone());
        std::thread::spawn(move || loop {
            let key = {
                let (q, ready) = &*queue;
                ready.wait_while(q.lock().unwrap(), |q| q.is_empty()).unwrap().pop_front().unwrap()
            };
            // A codec panic on a bad file fails that image, not the worker.
            let Some(img) = std::panic::catch_unwind(|| load_image(&key)).ok().flatten() else { continue };
            let _ = tx.send(Msg::Image(key, img));
            if let Some(w) = &wake { w() }
        });
    }
}

/// Queue images not yet asked for, in order, and mark them all just used. Work still queued
/// for a page we left is dropped (it's asked for again on return).
fn want(s: &mut State, urls: impl IntoIterator<Item = String>) {
    let queue = s.queue.clone();
    let (q, ready) = &*queue;
    let mut q = q.lock().unwrap();
    if s.queue_page != s.page {
        for k in q.drain(..) { s.requested.remove(&k); }
        s.queue_page = s.page;
    }
    s.tick += 1;
    for u in urls {
        if let Some(e) = s.lru.get_mut(&u) { e.1 = s.tick }
        if s.requested.insert(u.clone()) { q.push_back(u) }
    }
    ready.notify_all();
}

/// Drop least recently used images until the decoded pixels fit `budget`.
// an image on screen that hasn't moved is "used" when it was laid out; a page bigger
// than the budget would evict its own images (they reload when they move again).
fn evict(s: &mut State, budget: usize) {
    let mut total: usize = s.lru.values().map(|e| e.0).sum();
    while total > budget {
        let Some(k) = s.lru.iter().min_by_key(|e| e.1.1).map(|e| e.0.clone()) else { break };
        total -= s.lru.remove(&k).map_or(0, |e| e.0);
        s.images.remove(&k);
        s.requested.remove(&k);
    }
}

// ---- actions ----

fn sign_in(s: &mut State, _: &mut Tx) {
    let (email, password) = (s.email.text.clone(), s.password.text.clone());
    s.status = "Signing in…".into();
    spawn(s, move || {
        let r = api("login", json!({ "type": "Login", "email": email, "password": password, "facebook": false }))?;
        Ok(Msg::Auth(r["authKey"].as_str().ok_or("no authKey in response")?.into()))
    });
}

fn sign_out(s: &mut State) {
    if !cfg!(test) { let _ = std::fs::remove_file(auth_file()); }
    s.menu = false;
    s.auth = None;
    s.page = Page::Login;
    s.status.clear();
}

/// Addons, library and the account's email, in parallel.
fn load_account(s: &mut State) {
    let Some(key) = s.auth.clone() else { return };
    s.status = "Loading your addons…".into();
    let k = key.clone();
    spawn(s, move || {
        let r = api("addonCollectionGet", json!({ "type": "AddonCollectionGet", "authKey": k, "update": true }))?;
        Ok(Msg::Addons(r["addons"].as_array().cloned().unwrap_or_default()))
    });
    let k = key.clone();
    spawn(s, move || {
        let r = api("datastoreGet", json!({ "authKey": k, "collection": "libraryItem", "ids": [], "all": true }))?;
        Ok(Msg::Library(r.as_array().cloned().unwrap_or_default()))
    });
    spawn(s, move || {
        let email = api("getUser", json!({ "type": "GetUser", "authKey": key })).ok()
            .and_then(|u| u["email"].as_str().map(String::from)).unwrap_or_default();
        Ok(Msg::User(email))
    });
}

fn fetch_catalog(s: &mut State, i: usize) {
    let Some(c) = s.catalogs.get(i) else { return };
    if s.items.contains_key(&i) { return }
    let url = format!("{}/catalog/{}/{}.json", c.base, c.ty, c.id);
    spawn(s, move || Ok(Msg::Catalog(i, get(&url)?["metas"].as_array().cloned().unwrap_or_default())));
}

fn open_catalog(s: &mut State, i: usize) {
    s.selected = i;
    fetch_catalog(s, i);
}

fn go(s: &mut State, page: Page) {
    s.page = page;
    if let Page::Browse(ty) = page {
        if let Some(i) = s.catalogs.iter().position(|c| c.ty == ty) { open_catalog(s, i) }
    }
}

fn open_meta(s: &mut State, ty: String, id: String) {
    s.page = Page::Detail;
    s.sheet = false;
    s.meta = None;
    s.streams.clear();
    s.stream_for.clear();
    s.resume = None;
    s.meta_id = id.clone();
    let Some(addon) = s.addons.iter().find(|a| supports(a, "meta", &ty, &id)) else {
        s.status = "No addon provides details for this title".into();
        return;
    };
    let url = format!("{}/meta/{ty}/{id}.json", base(addon));
    spawn(s, move || Ok(Msg::Meta(id, get(&url)?["meta"].clone())));
}

/// Continue Watching: open the title with the last video's streams, starting where you left off.
fn resume(s: &mut State, item: &Value) {
    let (ty, id) = (item["type"].as_str().unwrap_or("movie").to_string(), item["_id"].as_str().unwrap_or("").to_string());
    let video = item["state"]["video_id"].as_str().filter(|v| !v.is_empty()).unwrap_or(&id).to_string();
    let at = item["state"]["timeOffset"].as_f64().unwrap_or(0.0) / 1000.0;
    open_meta(s, ty, id);
    s.resume = Some((video, at));
}

fn load_streams(s: &mut State, ty: String, vid: String) {
    s.streams.clear();
    s.stream_for = vid.clone();
    s.status = "Finding streams…".into();
    for addon in s.addons.iter().filter(|a| supports(a, "stream", &ty, &vid)) {
        let (url, vid, from) = (format!("{}/stream/{ty}/{vid}.json", base(addon)), vid.clone(),
            addon["manifest"]["name"].as_str().unwrap_or("").to_string());
        spawn(s, move || {
            let list = get(&url)?["streams"].as_array().cloned().unwrap_or_default().iter()
                .filter_map(|st| Some(Stream {
                    name: st["name"].as_str().unwrap_or(&from).replace('\n', " "),
                    title: st["title"].as_str().or(st["description"].as_str()).unwrap_or("").to_string(),
                    url: stream_url(st)?,
                })).collect();
            Ok(Msg::Streams(vid, list))
        });
    }
}

fn play(s: &mut State, url: &str) {
    stop(s); // close the previous player first
    let title = s.meta.as_ref().and_then(|m| m["name"].as_str()).unwrap_or("Stremio").to_string();
    let start = s.resume.as_ref().filter(|(v, _)| *v == s.stream_for).map(|(_, at)| *at);
    match Mpv::play(url, &title, s.waker.clone(), start) {
        Ok(p) => {
            s.player = Some(p);
            s.page = Page::Player;
            s.status.clear();
            s.controls = true;
            s.last_move = Instant::now();
            let ty = s.meta.as_ref().and_then(|m| m["type"].as_str()).unwrap_or("movie").to_string();
            let vid = s.stream_for.clone();
            load_subs(s, ty, vid);
            let l = s.meta.as_ref().and_then(logo);
            want(s, l);
        }
        Err(e) => s.status = e,
    }
}

/// Subtitles the addons offer for this video. Quiet on failure: they're optional.
fn load_subs(s: &mut State, ty: String, vid: String) {
    if s.subs_for == vid { return }
    s.subs_for = vid.clone();
    s.addon_subs.clear();
    for addon in s.addons.iter().filter(|a| supports(a, "subtitles", &ty, &vid)) {
        let (url, vid) = (format!("{}/subtitles/{ty}/{vid}.json", base(addon)), vid.clone());
        spawn(s, move || {
            let list = get(&url).map(|v| v["subtitles"].as_array().cloned().unwrap_or_default()).unwrap_or_default();
            Ok(Msg::Subs(vid, list.iter().filter_map(|x| Some((x["lang"].as_str()?.to_string(), x["url"].as_str()?.to_string()))).collect()))
        });
    }
}

/// Show an addon subtitle: select it if mpv has it already, else add and select it.
fn add_sub(s: &mut State, lang: &str, url: &str) {
    match s.playback.tracks.iter().find(|t| t.external == url) {
        Some(t) => { let id = t.id.clone(); mpv(s, &["set", "sid", &id]) }
        None => mpv(s, &["sub-add", url, "select", ui::language(lang), lang]),
    }
}

fn stop(s: &mut State) {
    if s.fullscreen { toggle_fullscreen(s) }
    s.subs_open = false;
    s.controls = true;
    s.player = None; // destroys mpv and its child window, uncovering the app
    s.playback = Playback::default();
    s.page = Page::Detail;
}

fn toggle_fullscreen(s: &mut State) {
    s.fullscreen = window::toggle_fullscreen();
}

/// Episode click or Play: fetch that video's streams into the side sheet.
fn pick(s: &mut State, ty: String, vid: String) {
    if vid != s.stream_for || s.streams.is_empty() { load_streams(s, ty, vid) }
    s.sheet = true;
}

/// App-wide keys. Keys pressed while the video has focus come back from mpv instead
/// (see `player.rs`), as `fullscreen` and `escape` messages.
fn on_key(s: &mut State, ev: &KeyEvent, _: &mut Tx) -> bool {
    let playing = s.page == Page::Player;
    match &ev.key {
        Key::Function(11) => toggle_fullscreen(s),
        Key::Text(t) if playing && t == "f" => toggle_fullscreen(s),
        Key::Text(t) if playing && t == " " => mpv(s, &["cycle", "pause"]),
        Key::Escape => escape(s),
        _ => return false,
    }
    true
}

/// Esc peels one layer: menu, sheet, fullscreen, playback, then the detail page.
fn escape(s: &mut State) {
    if s.menu { s.menu = false }
    else if s.sheet && s.page == Page::Detail { s.sheet = false }
    else if s.fullscreen { toggle_fullscreen(s) }
    else if s.page == Page::Player { stop(s) }
    else if s.page == Page::Detail { s.page = Page::Home }
}

fn mpv(s: &State, args: &[&str]) {
    if let Some(p) = &s.player { let _ = p.command(args); }
}

// ---- runtime hooks ----

fn setup(s: &mut State, waker: Waker) {
    let cache = ImageCache::new(waker.clone());
    let icon = |path: &str| cache.svg(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="none" stroke="#fff" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" d="{path}"/></svg>"##
    ).into_bytes(), 22.0, 3.0);
    s.icons = Some(Icons {
        play: icon("M7 5v14l12-7z"),
        pause: icon("M8 5v14M16 5v14"),
        back: icon("M15 18l-6-6 6-6"),
        close: icon("M6 6l12 12M18 6L6 18"),
        expand: icon("M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"),
        shrink: icon("M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5"),
        subs: icon("M4 6h16v12H4zM7 14h3M12 14h5M7 10.5h6M15 10.5h2"),
    });
    s.cache = Some(cache);
    s.waker = Some(waker);
    start_image_workers(s);
    load_account(s);
}

fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if let Some(player) = &s.player {
        // Redraw when a shown value changes: whole seconds, not every video frame.
        let shown = |p: &Playback| (p.pos as u64, p.dur as u64, p.paused, p.loading(), p.cache_pct as u64, p.sid.clone(), p.tracks.clone());
        let before = shown(&s.playback);
        let ended = player.pump(&mut s.playback);
        changed |= before != shown(&s.playback);
        // Controls show while the mouse moves, and stay while paused, loading, scrubbing,
        // hovered or picking subtitles. The mouse is read from the system because over the
        // video it's in mpv's window, which Rune never hears about.
        let cursor = window::cursor();
        if cursor != s.cursor { s.cursor = cursor; s.last_move = Instant::now() }
        let p = &s.playback;
        let controls = p.paused || p.loading() || p.scrub.is_some() || s.bar_hover || s.subs_open || s.last_move.elapsed() < IDLE;
        changed |= controls != s.controls;
        s.controls = controls;
        player.fit(if controls { BAR } else { 0.0 }, if s.subs_open { SUBS_W } else { 0.0 }, !p.loading());
        tx.wake_after(Duration::from_millis(200));
        for m in std::mem::take(&mut s.playback.messages) {
            match m.as_str() { "fullscreen" => toggle_fullscreen(s), "escape" => escape(s), _ => {} }
            changed = true;
        }
        if let Some(e) = ended {
            stop(s);
            s.status = e.unwrap_or_default();
            changed = true;
        }
    }
    while let Ok(msg) = s.rx.try_recv() {
        changed = true;
        match msg {
            Msg::Error(e) => s.status = e,
            Msg::Auth(key) => {
                let f = auth_file();
                let _ = std::fs::create_dir_all(f.parent().unwrap());
                let _ = std::fs::write(f, &key);
                s.auth = Some(key);
                s.page = Page::Home;
                s.password.clear();
                load_account(s);
            }
            Msg::User(email) => s.user = email,
            Msg::Addons(addons) => {
                s.addons = addons;
                s.catalogs = catalogs(&s.addons);
                s.items.clear();
                s.status.clear();
                for i in 0..s.catalogs.len().min(HOME_ROWS) { fetch_catalog(s, i) }
            }
            Msg::Library(items) => {
                s.library = continue_watching(items);
            }
            // Pictures ask for their images when laid out near the viewport (`ui::picture`).
            Msg::Catalog(i, items) => { s.items.insert(i, items); }
            Msg::Image(url, img) => if let Some(c) = &s.cache {
                s.tick += 1;
                s.lru.insert(url.clone(), (img.as_raw().len(), s.tick));
                s.images.insert(url, c.rgba(img.width(), img.height(), img.into_raw()));
                evict(s, IMAGE_BUDGET);
            },
            Msg::Meta(id, meta) if id == s.meta_id => {
                let ty = meta["type"].as_str().unwrap_or("movie").to_string();
                let episodic = ty != "movie" && meta["videos"].as_array().is_some_and(|v| !v.is_empty());
                s.season = s.resume.as_ref().and_then(|(v, _)| episode_of(v)).map(|(se, _)| se)
                    .or_else(|| ui::seasons(&meta).into_iter().find(|&n| n > 0))
                    .unwrap_or(0);
                s.meta = Some(meta);
                s.status.clear();
                // Movies preload their streams for Play; Continue Watching opens them at once.
                if let Some((video, _)) = s.resume.clone() { pick(s, ty, video) }
                else if !episodic { load_streams(s, ty, id) }
            }
            Msg::Subs(vid, list) if vid == s.subs_for => s.addon_subs.extend(list),
            Msg::Streams(vid, list) if vid == s.stream_for => {
                s.streams.extend(list);
                s.status.clear();
            }
            _ => {} // a stale answer for a page we left
        }
    }
    changed
}

/// Fake data for headless `--snap … --scene <name>` renders.
fn demo(s: &mut State) {
    s.auth = None; // keep the real account from loading over the fake data
    s.catalogs = vec![
        Catalog { base: String::new(), ty: "movie".into(), id: "top".into(), name: "Popular Movies".into() },
        Catalog { base: String::new(), ty: "series".into(), id: "top".into(), name: "Popular Shows".into() },
    ];
    let item = |n: usize| json!({ "id": format!("tt{n}"), "type": "movie", "name": format!("The Long Quiet {n}"), "releaseInfo": "2026",
        "genres": ["Drama", "Mystery"], "runtime": "107 min", "imdbRating": "7.5",
        "description": "A cartographer crossing the last unmapped desert finds a signal that shouldn't exist, and a town that remembers her name." });
    s.items.insert(0, (1..9).map(item).collect());
    s.items.insert(1, (9..17).map(item).collect());
    s.library = (1..4).map(|n| json!({ "_id": format!("tt{n}"), "type": "series", "name": format!("Show {n}"),
        "state": { "timeOffset": 600000 * n, "duration": 2700000, "video_id": format!("tt{n}:2:{n}") } })).collect();
    s.user = "jonas@example.com".into();
}

fn main() {
    rune::run(AppSpec {
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: &[
            ("home", |s| { demo(s); s.page = Page::Home }),
            ("browse", |s| { demo(s); s.page = Page::Browse("movie") }),
            ("detail", |s| {
                demo(s);
                s.page = Page::Detail;
                let mut m = s.items[&0][0].clone();
                m["type"] = "series".into();
                m["videos"] = (1..9).map(|e| json!({ "id": format!("tt1:1:{e}"), "season": 1, "episode": e, "title": format!("Episode {e}") })).collect();
                s.meta = Some(m);
                s.stream_for = "tt1:1:2".into();
                s.sheet = true;
                s.streams = (1..5).map(|n| Stream { name: format!("Torrentio {n}080p"), title: "The.Long.Quiet.S01E02.1080p.WEB.h264\n👤 120 💾 2.1 GB".into(), url: String::new() }).collect();
            }),
            ("menu", |s| { demo(s); s.page = Page::Home; s.menu = true }),
            ("player", |s| { demo(s); s.page = Page::Player; s.playback = Playback { pos: 1234.0, dur: 5400.0, started: true, ..Default::default() } }),
            ("loading", |s| { demo(s); s.page = Page::Player; s.playback = Playback { cache_pct: 40.0, ..Default::default() } }),
            ("subtitles", |s| {
                demo(s);
                s.page = Page::Player;
                s.subs_open = true;
                s.playback = Playback { pos: 1234.0, dur: 5400.0, started: true, sid: "1".into(), ..Default::default() };
                s.playback.tracks = vec![player::Track { id: "1".into(), label: "ENG · SDH".into(), external: String::new() }];
                s.addon_subs = vec![("ger".into(), "u1".into()), ("ger".into(), "u2".into()), ("fre".into(), "u3".into())];
            }),
        ],
        ..AppSpec::new("Stremio", (1280.0, 800.0), state, ui::view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "plays a test torrent through Stremio's server; opens a window briefly"]
    fn playback_reports_time_pauses_seeks_and_quits_cleanly() {
        // TEST_STREAM: any 10+ minute video; the default needs the Stremio app running.
        let src = std::env::var("TEST_STREAM").unwrap_or_else(|_| "http://127.0.0.1:11470/dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c/-1".into());
        let p = Mpv::play(&src, "test", None, Some(60.0)).unwrap();
        let mut pb = Playback::default();
        let wait = |pb: &mut Playback, done: &dyn Fn(&Playback) -> bool| {
            let t = std::time::Instant::now();
            while !done(pb) {
                assert_eq!(p.pump(pb), None, "ended early");
                assert!(t.elapsed().as_secs() < 20, "timed out");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        wait(&mut pb, &|pb| pb.started && pb.pos >= 60.0 && pb.dur > 500.0); // started at the resume point
        p.command(&["cycle", "pause"]).unwrap();
        wait(&mut pb, &|pb| pb.paused);
        p.command(&["seek", "300", "absolute"]).unwrap();
        wait(&mut pb, &|pb| (pb.pos - 300.0).abs() < 2.0);
        p.command(&["quit"]).unwrap();
        let t = std::time::Instant::now();
        while p.pump(&mut pb).is_none() { assert!(t.elapsed().as_secs() < 10, "no shutdown"); std::thread::sleep(std::time::Duration::from_millis(50)) }
        drop(p);
    }

    fn runtime(scene: fn(&mut State)) -> rune::Runtime<State> {
        let mut s = state();
        scene(&mut s);
        let mut rt = rune::Runtime::new(s, ui::view, (1280.0, 800.0), rune::Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        rt
    }

    fn click(rt: &mut rune::Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
    }

    #[test]
    fn avatar_opens_a_menu_instead_of_signing_out() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Home });
        click(&mut rt, "avatar");
        assert!(rt.state.menu && rt.state.page == Page::Home, "menu open, still signed in");
        click(&mut rt, "menu-scrim");
        assert!(!rt.state.menu, "a click outside closes it");
        click(&mut rt, "avatar");
        click(&mut rt, "sign-out");
        assert!(rt.state.page == Page::Login && !rt.state.menu);
    }

    #[test]
    fn episodes_open_the_streams_sheet_and_the_scrim_closes_it() {
        let mut rt = runtime(|s| {
            demo(s);
            let mut m = s.items[&0][0].clone();
            m["type"] = "series".into();
            m["videos"] = (1..4).map(|e| json!({ "id": format!("tt1:1:{e}"), "season": 1, "episode": e, "title": format!("Episode {e}") })).collect();
            s.meta_id = "tt1".into();
            s.meta = Some(m);
            s.page = Page::Detail;
        });
        assert!(!rt.state.sheet);
        click(&mut rt, "ep-tt1:1:2");
        assert!(rt.state.sheet && rt.state.stream_for == "tt1:1:2");
        assert!(rt.find("sheet").is_some());
        click(&mut rt, "sheet-scrim");
        assert!(!rt.state.sheet);
    }

    /// Home as Cinemeta alone fills it: 5 hero picks, then rows of 5 from four catalogs.
    #[test]
    #[ignore = "network: times Home's images from Cinemeta and metahub"]
    fn measure_home_images() {
        let cat = |p: &str| get(&format!("https://v3-cinemeta.strem.io/catalog/{p}.json")).unwrap()["metas"].as_array().cloned().unwrap();
        let rows = ["movie/top", "series/top", "movie/imdbRating", "series/imdbRating"].map(cat);
        // In Home's order: the hero, the first row (above the fold), the rest, other hero picks.
        let mut keys: Vec<String> = ui::hero_img(backdrop(&rows[0][0])).into_iter().collect();
        keys.extend(rows[0][HERO_PICKS..HERO_PICKS + ROW_CARDS].iter().filter_map(|m| ui::card_img(backdrop(m))));
        for r in &rows[1..] { keys.extend(r[..ROW_CARDS].iter().filter_map(|m| ui::card_img(backdrop(m)))) }
        keys.extend(rows[0][1..HERO_PICKS].iter().filter_map(|m| ui::hero_img(backdrop(m))));
        let mut s = state();
        start_image_workers(&s);
        let t = Instant::now();
        want(&mut s, keys.clone());
        let fold: HashSet<&String> = keys[..1 + ROW_CARDS].iter().collect();
        let (mut first, mut above, mut last, mut px, mut n) = (None, 0, t.elapsed(), 0usize, 0);
        let mut fold_at = Duration::ZERO;
        while let Ok(Msg::Image(key, img)) = s.rx.recv_timeout(Duration::from_secs(10)) {
            last = t.elapsed();
            first.get_or_insert((last, key.clone()));
            if fold.contains(&key) { above += 1; fold_at = last }
            px += (img.width() * img.height() * 4) as usize;
            n += 1;
            if n == keys.len() { break }
        }
        let (first, first_key) = first.unwrap();
        println!("scale {} screen {}pt hero {} card {}", window::scale(), window::screen_w(), keys[0], keys[1]);
        println!("images {n}/{} first {first:?} (hero: {}) above the fold {above}/{} {fold_at:?} all {:?} downloaded {} KB decoded {} KB",
            keys.len(), first_key == keys[0], fold.len(), last, DOWNLOADED.load(std::sync::atomic::Ordering::Relaxed) / 1024, px / 1024);
    }

    #[test]
    fn images_take_the_smallest_sharp_variant_and_the_cache_drops_the_least_recently_used() {
        let mh = |kind: &str, size: &str| format!("https://images.metahub.space/{kind}/{size}/tt1/img");
        assert_eq!(sized_px(&mh("background", "medium"), 472), mh("background", "small") + "#w472"); // a card at 1×
        assert_eq!(sized_px(&mh("background", "small"), 944), mh("background", "medium") + "#w944"); // at 2×
        assert_eq!(sized_px(&mh("background", "medium"), 2560), mh("background", "medium") + "#w2560"); // a hero: the largest
        assert_eq!(sized_px(&mh("poster", "small"), 320), mh("poster", "small") + "#w320");
        assert_eq!(sized_px(&mh("poster", "small"), 600), mh("poster", "large") + "#w600");
        assert_eq!(sized_px(&mh("logo", "medium"), 600), mh("logo", "medium") + "#w600");
        assert_eq!(sized_px("https://x.org/a.jpg", 300), "https://x.org/a.jpg#w300");

        let mut s = state();
        for (k, used) in [("a", 3), ("b", 1), ("c", 2)] { s.lru.insert(k.into(), (40, used)); s.requested.insert(k.into()); }
        evict(&mut s, 120);
        assert_eq!(s.lru.len(), 3, "within budget");
        evict(&mut s, 100);
        assert!(!s.lru.contains_key("b") && !s.requested.contains("b"), "the oldest goes, and may be fetched again");
        evict(&mut s, 40);
        assert_eq!(s.lru.keys().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn home_queues_the_hero_first_then_rows_near_the_viewport_and_drops_them_on_leaving() {
        let mut rt = runtime(|s| {
            demo(s);
            for i in 2..HOME_ROWS {
                s.catalogs.push(Catalog { base: String::new(), ty: "movie".into(), id: format!("c{i}"), name: format!("Row {i}") });
                s.items.insert(i, (i * 10..i * 10 + 5).map(|n| json!({ "id": format!("tt{n}"), "name": "x",
                    "poster": format!("https://images.metahub.space/poster/small/tt{n}/img") })).collect());
            }
            for m in s.items.get_mut(&0).unwrap() { m["poster"] = format!("https://images.metahub.space/poster/small/{}/img", m["id"].as_str().unwrap()).into() }
            s.page = Page::Home;
        });
        for _ in 0..3 { rt.frame(1.0 / 60.0); }
        let queued: Vec<String> = rt.state.queue.0.lock().unwrap().iter().cloned().collect();
        assert_eq!(Some(&queued[0]), ui::hero_img(backdrop(&rt.state.items[&0][0])).as_ref(), "hero first");
        let card = |n: usize| ui::card_img(backdrop(&json!({ "id": format!("tt{n}") }))).unwrap();
        let at = |n: usize| queued.iter().position(|k| *k == card(n));
        assert!(at(6) < at(9) && at(9) < at(20), "rows top-down");
        assert_eq!(at(50), None, "the last row is over two screens down: not yet");
        click(&mut rt, "nav-Movies");
        let queued: Vec<String> = rt.state.queue.0.lock().unwrap().iter().cloned().collect();
        assert!(!queued.is_empty() && queued.iter().all(|k| k.contains("/poster/")), "Home's queue dropped for Browse's posters");
        assert!(!rt.state.requested.contains(&card(6)), "and asked for again on return");
    }

    #[test]
    fn library_and_ids() {
        let lib = vec![
            json!({ "_id": "a", "type": "movie", "removed": false, "state": { "timeOffset": 5, "lastWatched": "2026-01-01" } }),
            json!({ "_id": "b", "type": "movie", "removed": false, "state": { "timeOffset": 5, "lastWatched": "2026-03-01" } }),
            json!({ "_id": "c", "type": "movie", "removed": true, "temp": true, "state": { "timeOffset": 5, "lastWatched": "2026-02-01" } }),
            json!({ "_id": "d", "type": "movie", "removed": true, "state": { "timeOffset": 5 } }),
            json!({ "_id": "e", "type": "movie", "removed": false, "state": { "timeOffset": 0 } }),
            json!({ "_id": "f", "type": "other", "removed": false, "state": { "timeOffset": 5 } }),
        ];
        let ids: Vec<_> = continue_watching(lib).iter().map(|i| i["_id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        assert_eq!(episode_of("tt123:2:5"), Some((2, 5)));
        assert_eq!(episode_of("kitsu:44:3:7"), Some((3, 7)));
        assert_eq!(episode_of("tt123"), None);
        assert_eq!(backdrop(&json!({ "id": "tt9:1:2" })).unwrap(), "https://images.metahub.space/background/medium/tt9/img");
        assert_eq!(backdrop(&json!({ "id": "x", "poster": "p" })).unwrap(), "p");
        // Library items can carry empty image fields; they mustn't win over a real image.
        assert_eq!(backdrop(&json!({ "_id": "tt5", "background": "", "poster": "" })).unwrap(), "https://images.metahub.space/background/medium/tt5/img");
        assert_eq!(fallbacks("https://images.metahub.space/background/medium/tt5/img")[1], "https://images.metahub.space/poster/medium/tt5/img");
        assert_eq!(logo(&json!({ "id": "tt5", "logo": "" })).unwrap(), "https://images.metahub.space/logo/medium/tt5/img");
    }

    #[test]
    fn manifest_rules_and_stream_urls() {
        let addon = json!({ "manifest": { "types": ["movie", "series"], "idPrefixes": ["tt"],
            "resources": ["catalog", { "name": "stream", "types": ["series"], "idPrefixes": ["tt", "kitsu"] }] } });
        assert!(supports(&addon, "catalog", "movie", "tt1"));
        assert!(!supports(&addon, "catalog", "movie", "yt1"));
        assert!(supports(&addon, "stream", "series", "kitsu:1:2"));
        assert!(!supports(&addon, "stream", "movie", "tt1"));
        assert!(!supports(&addon, "meta", "movie", "tt1"));
        assert_eq!(stream_url(&json!({ "infoHash": "abc", "fileIdx": 2 })).unwrap(), format!("{SERVER}/abc/2"));
        assert_eq!(stream_url(&json!({ "infoHash": "abc" })).unwrap(), format!("{SERVER}/abc/-1"));
        assert_eq!(stream_url(&json!({ "url": "https://x/y.mp4" })).unwrap(), "https://x/y.mp4");
        assert!(stream_url(&json!({ "ytId": "abc" })).is_none());

        let k = "0123456789abcdef0123";
        let old = format!(r#"x"auth":{{"key":"old0123456789abcdef","user":{{}}}}y"auth":{{"key":"{k}","user":{{}}}}"#);
        assert_eq!(last_auth_key(old.as_bytes()).as_deref(), Some(k));
        assert_eq!(last_auth_key(format!(r#"{old}"auth":null"#).as_bytes()), None);
        assert_eq!(last_auth_key(b"nothing here"), None);
        assert_eq!(ui::clock(3723.0), "1:02:03");
        assert_eq!(ui::clock(65.9), "1:05");
    }
}
