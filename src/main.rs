//! Native Stremio client on Rune. Logs in to api.strem.io, loads your addon collection and
//! library, browses catalogs, lists streams from your addons, and plays them through Stremio's
//! local streaming server (127.0.0.1:11470, started by the Stremio app) with the libmpv Stremio
//! ships. Views live in `ui.rs`, playback in `player.rs`.

// Release builds are a windowed app: no console window next to it. Debug builds keep the console
// for logs and the self-tests.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cast;
mod dlna;
mod library;
mod player;
mod preview;
mod server;
mod ui;
mod update;
#[cfg(windows)]
mod video;
#[cfg(target_os = "macos")]
#[path = "video_mac.rs"]
mod video;
#[cfg(any(windows, target_os = "macos"))]
mod videotest;
mod window;

use player::{Mpv, Playback};
use ui::{addons, search, settings};
use rune::{AppSpec, Image, ImageCache, Key, KeyEvent, TextEdit, Tween, Tx, Waker};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Height of the control bar, and width of the player's side panels, in points.
const BAR: f32 = 64.0;
const SUBS_W: f32 = 380.0;
/// Catalogs shown as rows on Home, and cards per row (Rune has no horizontal scroll yet).
const HOME_ROWS: usize = 6;
const ROW_CARDS: usize = 5;
/// Titles the Home hero cycles through, and how long each stays.
const HERO_PICKS: usize = 5;
/// The Home row of `row_*` maps that pages through the library (others are catalog indexes).
const UP_NEXT: usize = usize::MAX;
/// The Watchlist's Home row (the library proper, `State::saved`).
const WATCHLIST: usize = usize::MAX - 1;
const HERO_EVERY: Duration = Duration::from_secs(4);
/// Width of the hero's current dot, which fills over `HERO_EVERY`.
const DOT_W: f32 = 28.0;
/// Its fill is this many segments, redrawn this often (see `ui::dot_fill`).

/// Decoded image pixels kept; least recently used go first past this. Rune keeps a mip chain of
/// each on the GPU too (~4/3 of this).
const IMAGE_BUDGET: usize = 160 << 20;
/// Image fetch threads: enough to hide latency, few enough that the first images aren't starved.
const IMAGE_WORKERS: usize = 6;

/// Image bytes fetched so far, for `measure_home_images`.
static DOWNLOADED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const API: &str = "https://api.strem.io/api/";
/// The API's answer to an authKey it no longer knows (signed out or expired).
const NO_SESSION: &str = "Session does not exist";
const SERVER: &str = "http://127.0.0.1:11470";

enum Msg {
    Error(String),
    Auth(String),
    /// The API no longer knows the session (signed out elsewhere or expired).
    SignedOut,
    User(String),
    Addons(Vec<Value>),
    Library(Vec<Value>),
    Catalog(usize, Vec<Value>),
    /// A further page of a catalog (empty when it has no more).
    More(usize, Vec<Value>),
    Image(String, image::RgbaImage),
    Meta(String, Value),
    Streams(String, Vec<Stream>),
    /// Addon subtitles for a video: (language code, url).
    Subs(String, Vec<(String, String)>),
    /// The account's library item for a title being watched (none yet), with its meta.
    LibraryItem(String, Option<Value>, Value),
    /// Watch progress was written to the account.
    Saved,
    /// A Watchlist change the account took (see `library::toggle`).
    Listed(Value),
    /// Devices the streaming server found to cast to, and a TV's playback status.
    CastDevices(Vec<Value>),
    CastStatus(String, Value),
    /// A title's cast with photos, from one of the addons asked (see `open_meta`).
    Cast(String, Vec<Value>),
    /// Search answers: (query, index into `search.cats`, metas).
    Search(String, usize, Vec<Value>),
    /// The streaming server's `/settings`, or why not (it's down).
    Server(Result<Value, String>),
    /// Community addons to install, an addon fetched from a pasted URL, and how a write went.
    Discover(Vec<Value>),
    Preview(Value),
    Written(Result<String, String>),
    /// A newer release on GitHub, if there is one.
    Update(Option<update::Release>),
    /// The update is installed (macOS) or its Setup is running (Windows; it closes the app).
    Updating,
    UpdateProgress(update::Progress),
    /// The update didn't install: the offer comes back.
    UpdateFailed(String),
}

#[derive(Clone, PartialEq)]
struct Stream { name: String, title: String, url: String }

/// `name`: the catalog's name with its type, for Home rows; `title`: without the type or a
/// repeat of the addon's name, for the Browse chips under the addon's tab (`addon`).
/// `skip`: the catalog pages with the `skip` extra.
#[derive(Default)]
struct Catalog { base: String, ty: String, id: String, name: String, addon: String, title: String, skip: bool }

#[derive(Clone, Copy, PartialEq)]
enum Page { Login, Home, Browse(&'static str), Detail, Player, Search, Settings, Addons }

struct Icons {
    play: Image, pause: Image, back: Image, close: Image, expand: Image, shrink: Image, subs: Image, search: Image, next: Image, cast: Image,
    // The caption buttons' glyphs, thin as Windows draws them.
    minimize: Image, maximize: Image, restore: Image, quit: Image,
    volume: Image, muted: Image,
}

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
    /// The Watchlist: the account's library proper (`library::listed`).
    saved: Vec<Value>,
    /// The open title's cast with photos and roles (an addon's `app_extras.cast`), when an
    /// addon built on TMDB gives them; Cinemeta's are names only.
    people: Vec<Value>,
    /// Home rows (catalog index, or `UP_NEXT`): the first card shown, which way the last page
    /// turned (±1), catalogs with a further page on the way or none left, the row under the
    /// mouse (arrow keys page it), and sideways wheel travel since the last turn.
    row_at: HashMap<usize, usize>,
    row_dir: f32,
    more_loading: HashSet<usize>,
    more_done: HashSet<usize>,
    row_hover: Option<usize>,
    wheel: (f32, Instant),
    hero: usize,
    /// When the hero last changed; it advances `HERO_EVERY` after.
    hero_at: Instant,
    /// Counts hero changes; keys the current dot's fill so each title's starts over.
    /// The current dot's fill: [x, y, w, h] from empty to full over `HERO_EVERY`.
    hero_fill: Tween<[f32; 4]>,
    hero_round: u64,
    /// The app window has focus (`on_focus`; a window Windows never activated hears nothing).
    /// Out of focus the hero holds still, so an idle app in the background draws nothing.
    focused: bool,
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
    /// The page a detail page goes back to (Home, Browse or Search).
    back_to: Page,
    meta: Option<Value>,
    season: i64,
    stream_for: String,
    streams: Vec<Stream>,
    /// (video id, seconds) to start at when that video plays, from Continue Watching.
    resume: Option<(String, f64)>,
    status: String,
    /// A newer release to offer, and the one being installed with how far it has got.
    update: Option<update::Release>,
    updating: Option<(update::Release, update::Progress)>,
    /// The streams side sheet on the detail page.
    sheet: bool,
    /// The profile menu under the avatar.
    menu: bool,
    fullscreen: bool,
    /// The app's title strip, where the system's title bar was.
    caption: window::Caption,
    /// Playback controls: shown while the mouse moves (see `poll`).
    controls: bool,
    /// Since when mpv's volume differs from the saved one (saved once it settles).
    volume_moved: Option<Instant>,
    cursor: (i32, i32),
    last_move: Instant,
    bar_hover: bool,
    subs_open: bool,
    quality_open: bool,
    /// The TV list in the player (`cast`), what the streaming server found, and the TV being
    /// played to.
    cast_open: bool,
    cast_devices: Vec<Value>,
    cast: Option<cast::Session>,
    /// The stream URL playing.
    playing: String,
    /// Seek-bar previews of the stream playing.
    preview: Option<preview::Preview>,
    /// Where the pointer is over the seek bar (a fraction of the video), and the bar's width.
    seek_hover: Option<f32>,
    seek_w: f32,
    /// Where mpv's frames land (`video.rs`), drawn by the player page, and the size they're
    /// rendered at (the video element's, in device pixels).
    video: rune::NativeSurface,
    video_size: (u32, u32),
    /// Seek-bar drag in progress, as a fraction of the duration; volume-slider drag, percent.
    scrub: Option<f32>,
    vol_drag: Option<f64>,
    subs_for: String,
    addon_subs: Vec<(String, String)>,
    search: search::Search,
    settings: settings::Settings,
    /// The streaming server's settings: none yet, or it's down.
    server: Option<Result<Value, String>>,
    /// The settings picker that's unfolded.
    choice: Option<&'static str>,
    /// The Add-ons page.
    store: addons::Store,
    player: Option<Mpv>,
    /// Watch progress for the account's library (see `library.rs`).
    watch: Option<library::Watch>,
    playback: Playback,
    icons: Option<Icons>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    waker: Option<Waker>,
    cache: Option<ImageCache>,
}

/// `%APPDATA%\stremio-rune\authKey`; on macOS in `~/Library/Application Support/stremio-rune`.
fn auth_file() -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("APPDATA").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
    };
    dir.unwrap_or_default().join("stremio-rune").join("authKey")
}

/// The sessions found in the installed Stremio desktop app (stremio-web's `profile` in WebView2
/// localStorage), likeliest first, so a signed-in Stremio needs no second login. Old keys linger
/// in the storage files, so `setup` asks the API which one still works.
#[cfg(not(target_os = "macos"))]
fn stremio_desktop_auth() -> Vec<String> {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else { return vec![] };
    let dir = PathBuf::from(local)
        .join(r"Programs\Stremio\stremio-shell-ng.exe.WebView2\EBWebView\Default\Local Storage\leveldb");
    let mut files: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    // byte scan of LevelDB, newest file first; misses a key only in a snappy-compressed table
    let mut keys = vec![];
    for b in files.iter().rev().filter_map(|p| std::fs::read(p).ok()) { add_keys(&mut keys, &b) }
    keys
}

/// The same on macOS, where Stremio's web UI runs in WebKit: its localStorage is SQLite under
/// `~/Library/WebKit/<bundle id>` (Stremio 6 keeps 5's), values in UTF-16, so the scan also
/// drops the zero bytes. SQLite leaves replaced values in free pages: the last key in a file
/// isn't always the current one.
#[cfg(target_os = "macos")]
fn stremio_desktop_auth() -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { walk(&p, out) } else if p.to_string_lossy().to_lowercase().contains("localstorage") { out.push(p) }
        }
    }
    let mut files = Vec::new();
    let Some(home) = std::env::var_os("HOME") else { return vec![] };
    walk(&PathBuf::from(home).join("Library/WebKit/com.westbridge.stremio5-mac"), &mut files);
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let mut keys = vec![];
    for b in files.iter().rev().filter_map(|p| std::fs::read(p).ok()) {
        add_keys(&mut keys, &b);
        add_keys(&mut keys, &b.into_iter().filter(|&c| c != 0).collect::<Vec<_>>());
    }
    keys
}

/// Every `"auth":{"key":"…"` in a blob not yet in `keys`, the last one first.
fn add_keys(keys: &mut Vec<String>, bytes: &[u8]) {
    let needle = br#""auth":{"key":""#;
    let mut end = bytes.len();
    while let Some(at) = bytes[..end].windows(needle.len()).rposition(|w| w == needle) {
        let key: String = bytes[at + needle.len()..].iter().take_while(|b| b.is_ascii_alphanumeric()).map(|&b| b as char).collect();
        if key.len() >= 16 && !keys.contains(&key) { keys.push(key) }
        end = at;
    }
}

fn state() -> State {
    let (tx, rx) = channel();
    let auth = std::fs::read_to_string(auth_file()).ok().filter(|k| !k.is_empty());
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
        saved: vec![],
        people: vec![],
        row_at: HashMap::new(),
        row_dir: 1.0,
        more_loading: HashSet::new(),
        more_done: HashSet::new(),
        row_hover: None,
        wheel: (0.0, Instant::now()),
        hero: 0,
        hero_at: Instant::now(),
        hero_fill: dot_fill(),
        hero_round: 0,
        focused: false,
        images: HashMap::new(),
        lru: HashMap::new(),
        tick: 0,
        requested: HashSet::new(),
        queue: Default::default(),
        queue_page: Page::Login,
        view_h: 800.0,
        meta_id: String::new(),
        back_to: Page::Home,
        meta: None,
        season: 1,
        stream_for: String::new(),
        streams: vec![],
        resume: None,
        status: String::new(),
        update: None,
        updating: None,
        sheet: false,
        menu: false,
        fullscreen: false,
        caption: window::caption(),
        controls: true,
        volume_moved: None,
        cursor: (0, 0),
        last_move: Instant::now(),
        bar_hover: false,
        subs_open: false,
        quality_open: false,
        cast_open: false,
        cast_devices: vec![],
        cast: None,
        playing: String::new(),
        preview: None,
        seek_hover: None,
        seek_w: 0.0,
        video: rune::NativeSurface::new(),
        video_size: (0, 0),
        scrub: None,
        vol_drag: None,
        subs_for: String::new(),
        addon_subs: vec![],
        search: search::Search::default(),
        settings: if cfg!(test) { Default::default() } else { settings::load(&settings::file()) },
        server: None,
        choice: None,
        store: addons::Store::default(),
        player: None,
        watch: None,
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

/// A catalog's lasting name (addon, type, id), for `Settings::hero`.
fn catalog_key(c: &Catalog) -> String { format!("{}|{}|{}", c.base, c.ty, c.id) }

/// Where hero titles come from.
#[derive(Clone, Copy, PartialEq, Debug)]
enum HeroSource { Catalog(usize), Watchlist, UpNext }

/// The hero's sources (`Settings::heroes`) in the order picked: those still there, an empty
/// Watchlist or Up Next left out; none left means the first row's.
fn hero_sources(s: &State) -> Vec<HeroSource> {
    let v: Vec<HeroSource> = s.settings.heroes.iter().filter_map(|k| match k.as_str() {
        "watchlist" => (!s.saved.is_empty()).then_some(HeroSource::Watchlist),
        "upnext" => (!s.library.is_empty()).then_some(HeroSource::UpNext),
        k => s.catalogs.iter().position(|c| catalog_key(c) == k).map(HeroSource::Catalog),
    }).collect();
    if v.is_empty() && !s.catalogs.is_empty() { vec![HeroSource::Catalog(0)] } else { v }
}

/// The hero's titles with each one's label and source, taken from its sources in turn: five
/// from one source, two each from several (ten at most).
fn hero_picks(s: &State) -> Vec<(Value, String, HeroSource)> {
    hero_order(s).into_iter().map(|(src, n)| {
        let (items, name) = match src {
            HeroSource::Catalog(i) => (s.items.get(&i).map_or(&[][..], |v| &v[..]), s.catalogs[i].name.as_str()),
            HeroSource::Watchlist => (&s.saved[..], "Watchlist"),
            HeroSource::UpNext => (&s.library[..], "Up Next"),
        };
        let mut m = items[n].clone();
        // library items carry their id as `_id`
        if m["id"].is_null() { m["id"] = m["_id"].clone() }
        (m, name.to_string(), src)
    }).collect()
}

/// Which title of which source each hero turn shows (source, index into it): the sources in
/// turn, five turns for one source, two each for several (ten at most). Copies nothing, so
/// rows can ask often.
fn hero_order(s: &State) -> Vec<(HeroSource, usize)> {
    let sources = hero_sources(s);
    let len = |src: &HeroSource| match src {
        HeroSource::Catalog(i) => s.items.get(i).map_or(0, Vec::len),
        HeroSource::Watchlist => s.saved.len(),
        HeroSource::UpNext => s.library.len(),
    };
    let total = (2 * sources.len()).clamp(HERO_PICKS, 2 * HERO_PICKS);
    let mut order = vec![];
    for round in 0..total {
        let before = order.len();
        for src in &sources {
            if order.len() < total && round < len(src) { order.push((*src, round)) }
        }
        if order.len() == before { break }
    }
    order
}

/// How many of catalog `i`'s first titles the hero shows (its Home row starts after them).
fn hero_taken(s: &State, i: usize) -> usize {
    hero_order(s).iter().filter(|p| p.0 == HeroSource::Catalog(i)).count()
}

/// The hero's sources changed: start it over, with their catalogs loaded.
fn hero_changed(s: &mut State) {
    show_hero(s, 0);
    for src in hero_sources(s) { if let HeroSource::Catalog(i) = src { fetch_catalog(s, i) } }
}

/// An addon's name without what follows a `|` (`Watchly | ElfHosted` is Watchly).
fn addon_name(a: &Value) -> String {
    let n = a["manifest"]["name"].as_str().unwrap_or("");
    n.split('|').next().unwrap_or(n).trim().to_string()
}

/// A catalog's name without a leading repeat of its addon's (`Multiboxd - Watchlist` is Watchlist).
fn catalog_title(name: &str, addon: &str) -> String {
    let rest = name.get(..addon.len()).filter(|p| !addon.is_empty() && p.eq_ignore_ascii_case(addon))
        .map(|_| name[addon.len()..].trim_start_matches([' ', '-', ':', '|', '·', '–']));
    rest.filter(|r| !r.is_empty()).unwrap_or(name).to_string()
}

fn catalogs(addons: &[Value]) -> Vec<Catalog> {
    addons.iter().flat_map(|a| {
        let b = base(a);
        let addon = addon_name(a);
        a["manifest"]["catalogs"].as_array().cloned().unwrap_or_default().into_iter()
            .filter(|c| !c["extra"].as_array().into_iter().flatten().any(|e| e["isRequired"] == true)
                && c["extraRequired"].as_array().is_none_or(|r| r.is_empty()))
            .map(move |c| {
                let ty = c["type"].as_str().unwrap_or("").to_string();
                let raw = c["name"].as_str().unwrap_or("?");
                let (name, title) = (format!("{raw} {}", type_label(&ty)), catalog_title(raw, &addon));
                let skip = c["extra"].as_array().into_iter().flatten().any(|e| e["name"] == "skip")
                    || c["extraSupported"].as_array().is_some_and(|x| x.iter().any(|e| e == "skip"));
                Catalog { base: b.clone(), id: c["id"].as_str().unwrap_or("").into(), ty, name, addon: addon.clone(), title, skip }
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

/// Sign in with the first session from the Stremio app (see `stremio_desktop_auth`) the API
/// still knows; none leaves the login page as it is.
fn reuse_desktop_session(s: &mut State) {
    s.status = "Looking for your Stremio session…".into();
    spawn(s, || {
        let key = stremio_desktop_auth().into_iter()
            .find(|k| api("getUser", json!({ "type": "GetUser", "authKey": k })).is_ok());
        key.map(Msg::Auth).ok_or_else(String::new)
    });
}

/// Addons, library and the account's email, in parallel. A session the API no longer knows
/// signs out.
fn load_account(s: &mut State) {
    let Some(key) = s.auth.clone() else { return };
    s.status = "Loading your addons…".into();
    let k = key.clone();
    spawn(s, move || match api("addonCollectionGet", json!({ "type": "AddonCollectionGet", "authKey": k, "update": true })) {
        Ok(r) => Ok(Msg::Addons(r["addons"].as_array().cloned().unwrap_or_default())),
        Err(e) if e == NO_SESSION => Ok(Msg::SignedOut),
        Err(e) => Err(e),
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

/// The titles a Home row pages through: the library, or a catalog's (the first one's after the
/// hero's picks).
fn row_items(s: &State, r: usize) -> &[Value] {
    if r == UP_NEXT { return &s.library }
    if r == WATCHLIST { return &s.saved }
    let items = s.items.get(&r).map_or(&[][..], |v| &v[..]);
    let taken = if r < s.catalogs.len() { hero_taken(s, r) } else { 0 };
    if taken > 0 && items.len() > taken { &items[taken..] } else { items }
}

fn row_at(s: &State, r: usize) -> usize { s.row_at.get(&r).copied().unwrap_or(0).min(row_items(s, r).len()) }

fn can_page(s: &State, r: usize, dir: i32) -> bool {
    let at = row_at(s, r);
    if dir < 0 { at > 0 } else { at + ROW_CARDS < row_items(s, r).len() }
}

/// Turn a Home row a row's width back or on (`dir` ±1), the last page flush with the end, and
/// fetch the catalog's next page once the row nears the end of what's loaded.
fn page_row(s: &mut State, r: usize, dir: i32) {
    if !can_page(s, r, dir) { return }
    let (at, len) = (row_at(s, r), row_items(s, r).len());
    let to = if dir < 0 { at.saturating_sub(ROW_CARDS) } else { (at + ROW_CARDS).min(len - ROW_CARDS) };
    s.row_at.insert(r, to);
    s.row_dir = dir as f32;
    more(s, r);
}

/// Fetch catalog `r`'s next page (`skip` = titles loaded) when its row shows within two pages
/// of the end, if the catalog pages at all. Up Next has the whole library already.
fn more(s: &mut State, r: usize) {
    let Some(c) = s.catalogs.get(r) else { return };
    if !c.skip || row_at(s, r) + 3 * ROW_CARDS < row_items(s, r).len() || s.more_loading.contains(&r) || s.more_done.contains(&r) { return }
    let url = format!("{}/catalog/{}/{}/skip={}.json", c.base, c.ty, c.id, s.items.get(&r).map_or(0, Vec::len));
    s.more_loading.insert(r);
    // a failed page counts as the end; no retry until the addons reload
    spawn(s, move || Ok(Msg::More(r, get(&url).map(|v| v["metas"].as_array().cloned().unwrap_or_default()).unwrap_or_default())));
}

/// A further page landed: append the titles not shown yet; none new means the catalog ended.
fn more_landed(s: &mut State, r: usize, metas: Vec<Value>) {
    s.more_loading.remove(&r);
    let Some(items) = s.items.get_mut(&r) else { return };
    let mut seen: HashSet<String> = items.iter().map(|m| m["id"].as_str().unwrap_or("").to_string()).collect();
    let before = items.len();
    items.extend(metas.into_iter().filter(|m| seen.insert(m["id"].as_str().unwrap_or("").to_string())));
    if items.len() == before { s.more_done.insert(r); }
}

/// Shift+wheel or a sideways swipe over a Home row turns it, once per 40 pt and at most every
/// 0.35 s (a swipe's coasting is one turn); plain vertical scrolling passes through.
fn wheel_row(s: &mut State, r: usize, e: rune::WheelEvent) -> bool {
    if !e.mods.shift && e.dx.abs() <= e.dy.abs() { return false }
    let d = if e.mods.shift { e.dx + e.dy } else { e.dx };
    if s.wheel.1.elapsed() < Duration::from_millis(350) { s.wheel.0 = 0.0; return true }
    s.wheel.0 += d;
    if s.wheel.0.abs() >= 40.0 {
        page_row(s, r, if s.wheel.0 > 0.0 { -1 } else { 1 });
        s.wheel = (0.0, Instant::now());
    }
    true
}

fn open_catalog(s: &mut State, i: usize) {
    s.selected = i;
    fetch_catalog(s, i);
}

fn go(s: &mut State, page: Page) {
    s.page = page;
    if page == Page::Addons { addons::open(s) }
    if let Page::Browse(ty) = page {
        if let Some(i) = s.catalogs.iter().position(|c| c.ty == ty) { open_catalog(s, i) }
    }
}

fn open_meta(s: &mut State, ty: String, id: String) {
    if matches!(s.page, Page::Home | Page::Browse(_) | Page::Search) { s.back_to = s.page }
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
    let others: Vec<String> = s.addons.iter().filter(|a| a["transportUrl"] != addon["transportUrl"] && supports(a, "meta", &ty, &id))
        .map(|a| format!("{}/meta/{ty}/{id}.json", base(a))).collect();
    let i = id.clone();
    spawn(s, move || Ok(Msg::Meta(i, get(&url)?["meta"].clone())));
    // Photos for the cast: the first other addon with any wins; one that fails stays quiet.
    s.people.clear();
    for url in others {
        let id = id.clone();
        spawn(s, move || Ok(Msg::Cast(id, get(&url).ok().map(|r| cast_of(&r["meta"])).unwrap_or_default())));
    }
}

/// The video a title without episodes plays: the meta's `defaultVideoId` (an IMDb id for one a
/// catalog lists as `tmdb:…`, which stream addons don't know), else its id.
fn movie_video(meta: &Value, id: &str) -> String {
    meta["behaviorHints"]["defaultVideoId"].as_str().or(meta["id"].as_str()).filter(|v| !v.is_empty()).unwrap_or(id).to_string()
}

/// An addon meta's cast with photos (`app_extras.cast`: name, character, photo).
fn cast_of(meta: &Value) -> Vec<Value> {
    meta["app_extras"]["cast"].as_array().into_iter().flatten().filter(|c| c["name"].is_string()).cloned().collect()
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
    match Mpv::play(url, &title, s.waker.clone(), start, settings::mpv_options(&s.settings), Some(s.video.clone())) {
        Ok(p) => {
            // Arrow keys over the video: mpv's own bindings, at the chosen step.
            let step = s.settings.seek_ms as f64 / 1000.0;
            for (key, dir) in [("RIGHT", ""), ("LEFT", "-")] { let _ = p.command(&["keybind", key, &format!("seek {dir}{step}")]); }
            // Up and down: volume, as the volume slider (mpv's own would seek a minute).
            for (key, by) in [("UP", "5"), ("DOWN", "-5")] { let _ = p.command(&["keybind", key, &format!("add volume {by}")]); }
            s.player = Some(p);
            library::start(s);
            s.playing = url.to_string();
            // Seek-bar previews open the stream now, so the first hover needn't wait for it.
            s.preview = Some(preview::Preview::start(url.to_string(), s.tx.clone(), s.waker.clone()));
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

/// A quality pick: the same video from another stream, from the current position.
fn switch_stream(s: &mut State, url: &str) {
    s.quality_open = false;
    if url == s.playing { return }
    let Some(p) = &s.player else { return };
    match p.switch(url, s.playback.pos) {
        Ok(()) => {
            s.playing = url.to_string();
            s.preview = Some(preview::Preview::start(url.to_string(), s.tx.clone(), s.waker.clone()));
            s.playback.started = false; // the loader shows until the new stream's first frame
        }
        Err(e) => s.status = e,
    }
}

fn stop(s: &mut State) {
    cast::stop(s);
    s.cast_open = false;
    if s.player.is_some() { library::finish(s) }
    if s.fullscreen { toggle_fullscreen(s) }
    s.subs_open = false;
    s.quality_open = false;
    s.playing.clear();
    s.preview = None;
    s.seek_hover = None;
    s.controls = true;
    s.player = None; // stops mpv and its renderer
    s.video.clear();
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

/// App-wide keys. While playing, other keys go on to mpv's own bindings (arrows seek, `m`
/// mutes…): the floating controls cover mpv's window, so it never has focus to hear them.
fn on_key(s: &mut State, ev: &KeyEvent, _: &mut Tx) -> bool {
    let playing = s.page == Page::Player;
    match &ev.key {
        Key::Function(11) => toggle_fullscreen(s),
        Key::Text(t) if playing && t == "f" => toggle_fullscreen(s),
        Key::Text(t) if playing && t == " " => mpv(s, &["cycle", "pause"]),
        Key::Left | Key::Right if playing => {
            let step = s.settings.seek_ms as f64 / 1000.0 * if ev.key == Key::Left { -1.0 } else { 1.0 };
            mpv(s, &["seek", &step.to_string()])
        }
        Key::Left | Key::Right if s.page == Page::Home && s.row_hover.is_some() => {
            page_row(s, s.row_hover.unwrap(), if ev.key == Key::Left { -1 } else { 1 })
        }
        Key::Escape => escape(s),
        k if playing => {
            let name = match k { Key::Text(t) => t.as_str(), Key::Left => "LEFT", Key::Right => "RIGHT", Key::Up => "UP", Key::Down => "DOWN", _ => return false };
            mpv(s, &["keypress", name]);
        }
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
    else if s.page == Page::Detail { s.page = s.back_to }
    else if s.choice.is_some() { s.choice = None }
    else if s.store.confirm.is_some() { s.store.confirm = None }
    else if matches!(s.page, Page::Search | Page::Settings | Page::Addons) { s.page = Page::Home }
}

fn mpv(s: &State, args: &[&str]) {
    #[cfg(test)]
    tests::MPV.with(|log| log.borrow_mut().push(args.join(" ")));
    if let Some(p) = &s.player { let _ = p.command(args); }
}

// ---- runtime hooks ----

fn icons(cache: &ImageCache) -> Icons {
    let icon = |path: &str| cache.svg(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="none" stroke="#fff" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" d="{path}"/></svg>"##
    ).into_bytes(), 22.0, 3.0);
    let thin = |path: &str| cache.svg(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12"><path fill="none" stroke="#fff" stroke-width="1" d="{path}"/></svg>"##
    ).into_bytes(), 12.0, 3.0);
    Icons {
        play: icon("M7 5v14l12-7z"),
        pause: icon("M8 5v14M16 5v14"),
        back: icon("M15 18l-6-6 6-6"),
        close: icon("M6 6l12 12M18 6L6 18"),
        expand: icon("M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"),
        shrink: icon("M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5"),
        subs: icon("M4 6h16v12H4zM7 14h3M12 14h5M7 10.5h6M15 10.5h2"),
        search: icon("M11 4a7 7 0 1 0 0 14a7 7 0 1 0 0-14zM20 20l-4.2-4.2"),
        next: icon("M9 6l6 6-6 6"),
        cast: icon("M3 8V6a1 1 0 0 1 1-1h16a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1h-6M3 19h.01M3 15.5a3.5 3.5 0 0 1 3.5 3.5M3 12a7 7 0 0 1 7 7"),
        minimize: thin("M1 6h10"),
        maximize: thin("M1.5 1.5h9v9h-9z"),
        restore: thin("M1.5 3.5h7v7h-7zM3.5 3.5v-2h7v7h-2"),
        quit: thin("M1.5 1.5l9 9M10.5 1.5l-9 9"),
        volume: icon("M4 9.5h3.5L12 6v12l-4.5-3.5H4zM15.5 9.5a3.5 3.5 0 0 1 0 5M18 7a7 7 0 0 1 0 10"),
        muted: icon("M4 9.5h3.5L12 6v12l-4.5-3.5H4zM16 9.5l5 5M21 9.5l-5 5"),
    }
}

fn setup(s: &mut State, waker: Waker) {
    window::chrome::set_waker(waker.clone());
    s.video.set_waker(waker.clone());
    let cache = ImageCache::new(waker.clone());
    s.icons = Some(icons(&cache));
    s.search.debounce.set_waker(waker.clone());
    s.cache = Some(cache);
    s.waker = Some(waker);
    // Torrents need Stremio's streaming server; start it if the Stremio app isn't running it.
    let (tx, wake) = (s.tx.clone(), s.waker.clone());
    std::thread::spawn(move || if let Err(e) = server::ensure() {
        let _ = tx.send(Msg::Error(e));
        if let Some(w) = wake { w() }
    });
    start_image_workers(s);
    if s.auth.is_some() { load_account(s) } else if !cfg!(test) { reuse_desktop_session(s) }
    // A failed check stays quiet: the next start asks again.
    if !cfg!(test) { spawn(s, || Ok(Msg::Update(update::check().unwrap_or(None)))) }
}

/// Install the offered update in place (see `update`); the app restarts into it. A release with
/// nothing for this platform opens its page instead.
fn update_now(s: &mut State, tx: &mut Tx) {
    let Some(r) = s.update.take() else { return };
    if r.asset.is_some() {
        s.updating = Some((r.clone(), update::Progress::Downloading(None)));
        let (tx, wake) = (s.tx.clone(), s.waker.clone());
        spawn(s, move || {
            let report = |p| { let _ = tx.send(Msg::UpdateProgress(p)); if let Some(w) = &wake { w() } };
            Ok(update::install(&r, &report).map_or_else(Msg::UpdateFailed, |()| Msg::Updating))
        });
    } else if let Err(e) = tx.open_url(&r.page) {
        s.status = format!("Couldn't open the release page ({e:?})");
    }
}

fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    changed |= fullscreen_selftest(s, tx);
    cursor_selftest(s, tx);
    if let Some(player) = &s.player {
        // Redraw when a shown value changes: whole seconds, not every position update.
        let shown = |p: &Playback| { let mut p = p.clone(); (p.pos, p.cache_pct) = (p.pos.trunc(), p.cache_pct.trunc()); p.messages.clear(); p };
        let before = shown(&s.playback);
        let ended = player.pump(&mut s.playback);
        changed |= before != shown(&s.playback);
        // Controls show while the mouse moves, and stay while paused, loading, hovered or
        // picking from a panel.
        let cursor = window::cursor();
        if cursor != s.cursor { s.cursor = cursor; s.last_move = Instant::now() }
        let p = &s.playback;
        let controls = p.paused || p.loading() || s.bar_hover || s.subs_open || s.quality_open || s.cast_open || s.last_move.elapsed() < Duration::from_millis(s.settings.hide_ms as u64);
        changed |= controls != s.controls;
        s.controls = controls;
        player.set_video_size(s.video_size.0, s.video_size.1);
        remember_volume(s);
        library::tick(s);
        cast::tick(s);
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
    if let Some(q) = s.search.debounce.take() { search::run(s, q); changed = true }
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
            Msg::SignedOut => {
                sign_out(s);
                s.status = "Your session ended. Sign in again.".into();
            }
            Msg::User(email) => s.user = email,
            Msg::Addons(addons) => {
                s.addons = addons;
                s.catalogs = catalogs(&s.addons);
                s.search.cats = search::catalogs(&s.addons);
                s.items.clear();
                s.row_at.clear();
                s.more_loading.clear();
                s.more_done.clear();
                s.status.clear();
                for i in 0..s.catalogs.len().min(HOME_ROWS) { fetch_catalog(s, i) }
                // Hero sources further down than Home's rows.
                for src in hero_sources(s) { if let HeroSource::Catalog(i) = src && i >= HOME_ROWS { fetch_catalog(s, i) } }
            }
            Msg::Library(items) => {
                s.saved = library::listed(&items);
                s.library = continue_watching(items);
            }
            // Pictures ask for their images when laid out near the viewport (`ui::picture`).
            Msg::Catalog(i, items) => {
                // The hero cycles through its sources' first titles: fetch all their backdrops now,
                // so each turn shows its image at once instead of starting a download.
                let feeds = hero_sources(s).contains(&HeroSource::Catalog(i));
                s.items.insert(i, items);
                if feeds { let heroes: Vec<String> = hero_picks(s).iter().filter_map(|(m, _, _)| ui::hero_img(backdrop(m))).collect(); want(s, heroes) }
            }
            Msg::More(r, metas) => more_landed(s, r, metas),
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
                if s.people.is_empty() { s.people = cast_of(&meta) }
                s.meta = Some(meta);
                s.status.clear();
                // Movies preload their streams for Play; Continue Watching opens them at once.
                if let Some((video, _)) = s.resume.clone() { pick(s, ty, video) }
                else if !episodic { let vid = movie_video(s.meta.as_ref().unwrap(), &id); load_streams(s, ty, vid) }
            }
            Msg::Search(q, i, metas) => search::landed(s, q, i, metas),
            Msg::Subs(vid, list) if vid == s.subs_for => s.addon_subs.extend(list),
            Msg::LibraryItem(id, item, meta) => library::landed(s, id, item, meta),
            Msg::Saved => {}
            Msg::Listed(item) => library::listed_landed(s, item),
            Msg::CastDevices(list) => {
                s.cast_devices = list;
                // Discovery goes on in the server: look again while the list is open.
                if s.cast_open && cast::tvs(&s.cast_devices).is_empty() {
                    let (tx, wake) = (s.tx.clone(), s.waker.clone());
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(2));
                        let _ = tx.send(Msg::CastDevices(cast::discover()));
                        if let Some(w) = wake { w() }
                    });
                }
            }
            Msg::CastStatus(id, status) => cast::landed(s, id, status),
            Msg::Cast(id, cast) => if id == s.meta_id && s.people.is_empty() { s.people = cast },
            Msg::Server(r) => s.server = Some(r),
            Msg::Discover(list) => s.store.discover = list,
            Msg::Preview(d) => { s.store.preview = Some(d); s.status.clear() }
            Msg::Written(r) => addons::written(s, r),
            Msg::Update(Some(r)) => s.update = Some(r),
            // macOS: quit, and the new app opens (`update::install`).
            Msg::Updating => if cfg!(target_os = "macos") { tx.quit() },
            Msg::UpdateProgress(p) => if let Some(u) = s.updating.as_mut() { u.1 = p },
            Msg::UpdateFailed(e) => {
                s.update = s.updating.take().map(|u| u.0);
                s.status = e;
            }
            Msg::Streams(vid, list) if vid == s.stream_for => {
                s.streams.extend(list);
                s.status.clear();
            }
            _ => {} // a stale answer for a page we left
        }
    }
    changed |= rotate_hero(s, tx);
    let caption = window::caption();
    if caption != s.caption { s.caption = caption; changed = true }
    // macOS: the window also leaves fullscreen by its green button or the menu bar's.
    if let Some(on) = window::fullscreen_now() && on != s.fullscreen { s.fullscreen = on; changed = true }
    changed
}

/// Keep the player's volume for next time, once it has stayed put for a second: not on every
/// step of a slider drag. Only after the first frame, when mpv has reported it.
fn remember_volume(s: &mut State) {
    let v = s.playback.volume.round() as u32;
    if !s.playback.started || v == s.settings.volume { s.volume_moved = None; return }
    if s.volume_moved.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(1) {
        settings::set(s, |st| st.volume = v);
        s.volume_moved = None;
    }
}

/// `STREMIO_RUNE_FSTEST=<video file>`: play it, enter and leave fullscreen the way the
/// controls' button does, write the window state at each step to
/// `%TEMP%\stremio-rune-fstest.txt`, and quit. For diagnosing fullscreen on a real desktop.
/// True when it changed what the view shows.
fn fullscreen_selftest(s: &mut State, tx: &mut Tx) -> bool {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU8, Ordering};
    static START: OnceLock<Instant> = OnceLock::new();
    static STEP: AtomicU8 = AtomicU8::new(0);
    let Ok(clip) = std::env::var("STREMIO_RUNE_FSTEST") else { return false };
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f32();
    tx.wake_after(Duration::from_millis(100));
    let log = |what: &str, s: &State| {
        use std::io::Write;
        let path = std::env::temp_dir().join("stremio-rune-fstest.txt");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[{t:.1}s] {what} (fullscreen {}, strip {:?}, video {:?}, mpv dropped {:?} delayed {:?}, rendered {:?}): {}", s.fullscreen, s.caption, s.video_size,
                s.player.as_ref().and_then(|p| p.property("frame-drop-count")), s.player.as_ref().and_then(|p| p.property("vo-delayed-frame-count")),
                s.player.as_ref().and_then(|p| p.video_stats()).map(|v| (v.frames, v.starved)), window::diag());
        }
    };
    let step = STEP.load(Ordering::Relaxed);
    let next = match step {
        0 if t > 2.0 => { window::selftest_front_maximized(); true }
        1 if t > 3.0 => { s.meta = None; play(s, &clip); true }
        2 if t > 6.0 => { log("before", s); toggle_fullscreen(s); log("toggled", s); true }
        3 if t > 6.4 => { window::selftest_jiggle(); log("fullscreen +0.4s", s); true }
        4 if t > 8.0 => { log("fullscreen +2s", s); toggle_fullscreen(s); true }
        5 if t > 9.0 => { log("restored +1s", s); tx.quit(); true }
        _ => false,
    };
    if next { STEP.store(step + 1, Ordering::Relaxed) }
    next
}

/// `STREMIO_RUNE_CURSORTEST=<video file>`: play it with the pointer over the video, write the
/// cursor's state (`window::cursor_diag`) to `%TEMP%\stremio-rune-cursortest.txt` while the
/// controls show, after they hide, and after the pointer moves again, then quit.
fn cursor_selftest(s: &mut State, tx: &mut Tx) {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU8, Ordering};
    static START: OnceLock<Instant> = OnceLock::new();
    static STEP: AtomicU8 = AtomicU8::new(0);
    let Ok(clip) = std::env::var("STREMIO_RUNE_CURSORTEST") else { return };
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f32();
    tx.wake_after(Duration::from_millis(100));
    let log = |what: &str, s: &State| {
        use std::io::Write;
        let path = std::env::temp_dir().join("stremio-rune-cursortest.txt");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[{t:.1}s] {what} (controls {}): {}", s.controls, window::cursor_diag());
        }
    };
    let hide = s.settings.hide_ms as f32 / 1000.0;
    let step = STEP.load(Ordering::Relaxed);
    let next = match step {
        0 if t > 2.0 => { window::selftest_front_maximized(); true }
        1 if t > 3.0 => { s.meta = None; play(s, &clip); true }
        2 if t > 4.5 => { window::selftest_point_at(0.5, 0.45); true }
        3 if t > 5.0 => { log("pointer over the video, just moved", s); true }
        4 if t > 5.0 + hide + 1.5 => { log("still for the idle time + 1.5 s", s); true }
        5 if t > 5.0 + hide + 2.0 => { window::selftest_point_at(0.52, 0.47); true }
        6 if t > 5.0 + hide + 2.4 => { log("moved again", s); true }
        7 if t > 5.0 + hide + 2.6 => { window::selftest_point_at(0.5, 0.97); true }
        8 if t > 5.0 + hide + 3.2 => { log("over the bottom bar", s); true }
        9 if t > 5.0 + 2.0 * hide + 4.5 => { log("still over the bottom bar for the idle time + 1.3 s", s); tx.quit(); true }
        _ => false,
    };
    if next { STEP.store(step + 1, Ordering::Relaxed) }
}

/// Advance the Home hero every `HERO_EVERY`, and schedule the next look. Only on Home.
fn rotate_hero(s: &mut State, tx: &mut Tx) -> bool {
    if s.page != Page::Home || !s.focused { return false }
    let n = hero_order(s).len();
    if n < 2 { return false }
    let left = HERO_EVERY.saturating_sub(s.hero_at.elapsed());
    if !left.is_zero() { tx.wake_after(left); return false }
    show_hero(s, (s.hero + 1) % n);
    tx.wake_after(HERO_EVERY);
    true
}

/// Back in focus, the hero's title gets a full turn again.
fn on_focus(s: &mut State, focused: bool, _: &mut Tx) {
    s.focused = focused;
    if focused { show_hero(s, s.hero) }
}

/// Show hero title `i` for a full `HERO_EVERY`, its dot filling from empty.
fn show_hero(s: &mut State, i: usize) {
    s.hero = i;
    s.hero_at = Instant::now();
    s.hero_fill = dot_fill();
    s.hero_round += 1;
}

fn dot_fill() -> Tween<[f32; 4]> { Tween::linear([0.0, 0.0, 0.0, 8.0], [0.0, 0.0, DOT_W, 8.0], HERO_EVERY) }

/// Fake data for headless `--snap … --scene <name>` renders.
fn demo(s: &mut State) {
    s.auth = None; // keep the real account from loading over the fake data
    s.focused = true;
    s.catalogs = vec![
        Catalog { ty: "movie".into(), id: "top".into(), name: "Popular Movies".into(), addon: "Cinemeta".into(), title: "Popular".into(), ..Default::default() },
        Catalog { ty: "series".into(), id: "top".into(), name: "Popular Shows".into(), addon: "Cinemeta".into(), title: "Popular".into(), ..Default::default() },
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

/// Credits and facts the way Cinemeta gives them.
fn demo_credits() -> Value {
    json!({ "director": ["Mara Ellison"], "writer": ["Mara Ellison", "Tomas Reyes"], "cast": ["Ines Kovač", "Daniel Okafor", "Ruth Amari", "Pavel Lind", "June Hartley"],
        "released": "2026-03-14T00:00:00.000Z", "country": "United States, Morocco", "awards": "2 wins & 9 nominations" })
}

/// A series with credits and a season of episodes.
fn demo_series(s: &State) -> Value {
    let mut m = s.items[&0][0].clone();
    for (k, v) in demo_credits().as_object().unwrap() { m[k] = v.clone() }
    m["type"] = "series".into();
    m["status"] = "Continuing".into();
    m["releaseInfo"] = "2026–".into();
    m["videos"] = (1..9).map(|e| json!({ "id": format!("tt1:1:{e}"), "season": 1, "episode": e, "title": format!("Episode {e}"), "rating": "8.1",
        "released": format!("2026-03-{:02}T00:00:00.000Z", 3 * e), "overview": "The survey team reaches the dry lake and finds tracks that end in the middle of nowhere." })).collect();
    m
}

fn demo_cat(ty: &str, name: &str) -> Catalog {
    Catalog { ty: ty.into(), id: "top".into(), name: name.into(), ..Default::default() }
}

/// Demo playback for the player scenes.
fn demo_player(v: &mut State) {
    demo(v);
    v.page = Page::Player;
    v.controls = true;
    v.meta = Some(json!({ "id": "tt1", "type": "movie", "name": "The Long Quiet" }));
    v.playback = Playback { pos: 1234.0, dur: 5400.0, started: true, sid: "no".into(), aid: "1".into(), ..Default::default() };
    let st = |name: &str, title: &str, url: &str| Stream { name: name.into(), title: title.into(), url: url.into() };
    v.streams = vec![
        st("Torrentio 4k DV | HDR", "The.Long.Quiet.2026.2160p.WEB-DL.DV.HDR.H265\n👤 88 💾 18.2 GB", "a"),
        st("Torrentio 1080p", "The.Long.Quiet.2026.1080p.BluRay.x264\n👤 412 💾 9.8 GB", "b"),
        st("[RD+] Torrentio 1080p", "The.Long.Quiet.2026.1080p.WEB.h264\n👤 120 💾 2.1 GB", "c"),
        st("Torrentio 720p", "The.Long.Quiet.2026.720p.WEBRip\n👤 64 💾 1.1 GB", "d"),
        st("Some Addon", "Direct link", "e"),
    ];
    v.playing = "b".into();
    v.playback.volume = 80.0;
}

fn main() {
    // The video's frames are Direct3D textures (video.rs), which Rune imports on DX12 only.
    #[cfg(windows)]
    if std::env::var_os("WGPU_BACKEND").is_none() { unsafe { std::env::set_var("WGPU_BACKEND", "dx12") } }
    #[cfg(any(windows, target_os = "macos"))]
    if videotest::run() { return }
    let app = rune::App::new(AppSpec {
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: &[
            ("home", |s| { demo(s); s.page = Page::Home }),
            ("update", |s| { demo(s); s.page = Page::Home; s.update = Some(update::Release { version: "0.3.0-alpha".into(), page: String::new(), asset: None }) }),
            ("updating", |s| {
                demo(s);
                s.page = Page::Home;
                s.updating = Some((update::Release { version: "0.3.0-alpha".into(), page: String::new(), asset: None }, update::Progress::Downloading(Some(0.42))));
            }),
            ("browse", |s| {
                demo(s);
                s.page = Page::Browse("movie");
                let cat = |addon: &str, name: &str| Catalog { ty: "movie".into(), name: format!("{name} Movies"), addon: addon.into(),
                    title: catalog_title(name, addon), ..Default::default() };
                s.catalogs.push(cat("Cinemeta", "Featured"));
                for n in ["Netflix", "Disney+", "Prime Video", "Apple TV+", "HBO Max", "Sky Go"] { s.catalogs.push(cat("Streaming Catalogs", n)) }
                for n in ["Trending This Week", "Now Playing", "Sci-Fi Gems", "90s Classics"] { s.catalogs.push(cat("TMDB Discover+", n)) }
                for n in ["Multiboxd - Watchlist", "Multiboxd - Recommended for You"] { s.catalogs.push(cat("Multiboxd", n)) }
            }),
            ("detail", |s| {
                demo(s);
                s.page = Page::Detail;
                s.meta = Some(demo_series(s));
                s.stream_for = "tt1:1:2".into();
                s.sheet = true;
                s.streams = (1..5).map(|n| Stream { name: format!("Torrentio {n}080p"), title: "The.Long.Quiet.S01E02.1080p.WEB.h264\n👤 120 💾 2.1 GB".into(), url: String::new() }).collect();
            }),
            ("detail-movie", |s| {
                demo(s);
                s.page = Page::Detail;
                let mut m = s.items[&0][0].clone();
                for (k, v) in demo_credits().as_object().unwrap() { m[k] = v.clone() }
                s.meta_id = "tt1".into();
                s.meta = Some(m);
            }),
            ("detail-series", |s| {
                demo(s);
                s.page = Page::Detail;
                s.meta = Some(demo_series(s));
                s.stream_for = "tt1:1:2".into();
                s.season = 1;
            }),
            ("search", |s| {
                demo(s);
                search::open(s);
                s.search.query = TextEdit::new("long quiet");
                s.search.cats = vec![demo_cat("movie", "Movies"), demo_cat("series", "Shows")];
                s.search.results = HashMap::from([(0, s.items[&0].clone()), (1, s.items[&1][..3].to_vec())]);
            }),
            ("menu", |s| { demo(s); s.page = Page::Home; s.menu = true }),
            ("settings-hero", |s| { demo(s); settings::open(s); s.choice = Some("hero"); s.settings.heroes = vec!["watchlist".into(), catalog_key(&s.catalogs[1])] }),
            ("watchlist", |s| {
                demo(s);
                s.page = Page::Home;
                s.saved = (20..26).map(|n| json!({ "_id": format!("tt{n}"), "type": if n % 2 == 0 { "movie" } else { "series" }, "name": format!("Saved {n}") })).collect();
            }),
            ("rows", |s| {
                demo(s);
                s.page = Page::Home;
                s.library = (1..13).map(|n| json!({ "_id": format!("tt{n}"), "type": "movie", "name": format!("Title {n}"),
                    "state": { "timeOffset": 600000, "duration": 5400000 } })).collect();
                s.row_at.insert(UP_NEXT, 5);
            }),
            ("addons", |s| {
                demo(s);
                s.page = Page::Addons;
                let a = |name: &str, flags: Value, configurable: bool| json!({ "transportUrl": format!("https://{name}/manifest.json"), "transportName": "", "flags": flags,
                    "manifest": { "id": name, "name": name, "version": "1.2.0", "types": ["movie", "series"], "behaviorHints": { "configurable": configurable },
                        "description": "Streams and metadata for movies and shows, from a source this addon knows well." } });
                s.addons = vec![a("Cinemeta", json!({ "official": true, "protected": true }), false), a("OpenSubtitles", json!({ "official": true, "protected": false }), false),
                    a("Torrentio", json!({ "official": false, "protected": false }), true), a("Streaming Catalogs", json!({ "official": false, "protected": false }), true)];
                s.store.confirm = Some("https://Streaming Catalogs/manifest.json".into());
                s.store.url = TextEdit::new("https://example.org/manifest.json");
                s.store.preview = Some(addons::descriptor("https://example.org/manifest.json", s.addons[2]["manifest"].clone()));
                s.store.preview.as_mut().unwrap()["manifest"]["name"] = "Example Addon".into();
            }),
            ("player", demo_player),
            ("cast-list", |s| {
                demo_player(s);
                s.cast_open = true;
                s.cast_devices = vec![json!({ "id": "Living-Room", "name": "Living Room TV", "type": "chromecast" }),
                    json!({ "id": "uuid:1", "name": "Bedroom [LG] webOS TV", "type": "dlna" }), json!({ "id": "vlc", "name": "VLC", "type": "external" })];
            }),
            ("casting", |s| {
                demo_player(s);
                s.cast = Some(cast::Session::new(json!({ "id": "Living-Room", "name": "Living Room TV", "type": "chromecast" }), 1234.0, 5400.0));
            }),
            ("loading", |s| { demo_player(s); s.playback = Playback { cache_pct: 40.0, ..Default::default() } }),
            ("quality", |s| { demo_player(s); s.quality_open = true }),
            ("seek-preview", |s| { demo_player(s); s.seek_hover = Some(0.4) }),
            ("audio", |s| {
                demo_player(s);
                s.subs_open = true;
                let t = |id: &str, label: String| player::Track { id: id.into(), label, external: String::new() };
                s.playback.audio = vec![t("1", player::audio_label("eng", "", 6, "truehd")), t("2", player::audio_label("ger", "", 2, "aac")),
                    t("3", player::audio_label("eng", "Commentary", 2, "ac3"))];
                s.playback.tracks = vec![t("4", "ENG · SDH".into())];
                s.addon_subs = vec![("ger".into(), "u1".into()), ("ger".into(), "u2".into()), ("fre".into(), "u3".into())];
            }),
            ("settings", |s| {
                demo(s);
                s.page = Page::Settings;
                s.choice = Some("sub-lang");
                s.settings.sub_lang = "eng".into();
                s.server = Some(Ok(json!({ "options": [{ "id": "remoteHttps", "selections": [{ "name": "Disabled", "val": "" }, { "name": "192.168.1.10", "val": "192.168.1.10" }] }],
                    "values": { "serverVersion": "4.22.3", "cacheSize": 2147483648u64, "btMaxConnections": 55, "btHandshakeTimeout": 20000, "btRequestTimeout": 4000,
                        "btDownloadSpeedSoftLimit": 2621440, "btDownloadSpeedHardLimit": 3670016, "btMinPeersForStable": 5, "remoteHttps": "",
                        "transcodeProfile": null, "allTranscodeProfiles": ["nvenc-win"] } })));
            }),
        ],
        ..AppSpec::new("Stremio", (1280.0, 800.0), state, ui::view)
    }).on_window(window::chrome::set_main).on_focus(on_focus);
    let app = match std::env::var_os("STREMIO_RUNE_PACING") { Some(path) => app.on_frame_encoded(pacing(path.into())), None => app };
    app.run();
}

/// `STREMIO_RUNE_PACING=<file>`: every 10 s, append the frame pacing of the last 10 s to the
/// file: gaps between frames, how many missed a vsync (60 Hz), and Rune's update and encode
/// times, to tell a frame the app was busy with from one that waited on the GPU.
fn pacing(path: PathBuf) -> impl FnMut(&rune::FrameEncoded) {
    let (mut last, mut since, mut gaps, mut work) = (None::<Instant>, Instant::now(), Vec::new(), Vec::new());
    move |f| {
        if let Some(l) = last.replace(f.at()) { gaps.push((f.at() - l).as_secs_f64() * 1000.0) }
        work.push(f.update_ms().unwrap_or(0.0) + f.encode_ms().unwrap_or(0.0));
        if since.elapsed() < Duration::from_secs(10) || gaps.is_empty() { return }
        since = Instant::now();
        let sorted = |v: &mut Vec<f64>| { v.sort_by(f64::total_cmp); let v = std::mem::take(v); move |q: f64| v[((v.len() - 1) as f64 * q) as usize] };
        let (n, slow, very) = (gaps.len(), gaps.iter().filter(|&&g| g > 25.0).count(), gaps.iter().filter(|&&g| g > 41.7).count());
        let busy = work.iter().filter(|&&w| w > 12.0).count();
        let (g, w) = (sorted(&mut gaps), sorted(&mut work));
        let line = format!("{:.1} fps | gaps p50 {:.1} p99 {:.1} max {:.1} ms, {slow} over 25 ms, {very} over 41.7 | update+encode p50 {:.1} p99 {:.1} max {:.1} ms, {busy} over 12 ms\n",
            n as f64 / 10.0, g(0.5), g(0.99), g(1.0), w(0.5), w(0.99), w(1.0));
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) { let _ = file.write_all(line.as_bytes()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "plays a test torrent through Stremio's server; opens a window briefly"]
    fn playback_reports_time_pauses_seeks_and_quits_cleanly() {
        // TEST_STREAM: any 10+ minute video; the default needs the Stremio app running.
        let src = std::env::var("TEST_STREAM").unwrap_or_else(|_| "http://127.0.0.1:11470/dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c/-1".into());
        let p = Mpv::play(&src, "test", None, Some(60.0), vec![], None).unwrap();
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

    /// When mpv's waiting-related properties flip, from `Mpv::play` through a seek: what the
    /// loader can follow. `cargo test loader_timeline -- --ignored --nocapture`, with
    /// TEST_STREAM, else a torrent through Stremio's server.
    #[test]
    #[ignore = "plays a stream; opens a window briefly; prints a timeline"]
    fn loader_timeline() {
        let src = std::env::var("TEST_STREAM").unwrap_or_else(|_| "http://127.0.0.1:11470/dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c/-1".into());
        let t = Instant::now();
        let p = Mpv::play(&src, "test", None, None, vec![], None).unwrap();
        println!("{:>6.0} ms  Mpv::play returned", t.elapsed().as_secs_f64() * 1000.0);
        let mut pb = Playback::default();
        let key = |p: &Playback| (p.started, p.buffering, p.seeking, p.idle, p.paused, p.cache_pct as i64, p.loading());
        let mut last = None;
        let mut log = |pb: &Playback, what: &str| if last != Some(key(pb)) {
            last = Some(key(pb));
            println!("{:>6.0} ms  started {} buffering {} seeking {} idle {} paused {} cache {:>3} -> loading {}  {what}",
                t.elapsed().as_secs_f64() * 1000.0, pb.started, pb.buffering, pb.seeking, pb.idle, pb.paused, pb.cache_pct, pb.loading());
        };
        let run = |pb: &mut Playback, log: &mut dyn FnMut(&Playback, &str), secs: f64, what: &str| {
            let until = Instant::now() + Duration::from_secs_f64(secs);
            while Instant::now() < until {
                assert_eq!(p.pump(pb), None, "ended early");
                log(pb, what);
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        log(&pb, "before");
        let wait_start = Instant::now();
        while !pb.started && wait_start.elapsed().as_secs() < 60 { run(&mut pb, &mut log, 0.05, "start") }
        run(&mut pb, &mut log, 1.5, "playing");
        println!("{:>6.0} ms  seek +300", t.elapsed().as_secs_f64() * 1000.0);
        p.command(&["seek", "300", "relative"]).unwrap();
        run(&mut pb, &mut log, 4.0, "after seek");
        p.command(&["quit"]).unwrap();
        while p.pump(&mut pb).is_none() { std::thread::sleep(Duration::from_millis(20)) }
    }

    #[test]
    #[ignore = "plays TEST_STREAM; opens a window briefly"]
    fn switching_streams_keeps_the_position() {
        let src = std::env::var("TEST_STREAM").expect("TEST_STREAM: a 10+ minute video");
        let p = Mpv::play(&src, "test", None, Some(120.0), vec![], None).unwrap();
        let mut pb = Playback::default();
        let wait = |pb: &mut Playback, done: &dyn Fn(&Playback) -> bool| {
            let t = std::time::Instant::now();
            while !done(pb) {
                assert_eq!(p.pump(pb), None, "ended early");
                assert!(t.elapsed().as_secs() < 20, "timed out at {}", pb.pos);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        wait(&mut pb, &|pb| pb.started && pb.pos >= 120.0);
        pb.started = false;
        p.switch(&src, pb.pos).unwrap(); // the replaced file's end isn't the end of playback
        wait(&mut pb, &|pb| pb.started && pb.pos >= 120.0);
        assert!(pb.pos < 130.0, "picked up where it was, not later: {}", pb.pos);
    }

    #[test]
    #[ignore = "plays a clip with two audio tracks; opens a window briefly"]
    fn audio_tracks_are_listed_and_switch() {
        // TEST_AUDIO_STREAM: a clip whose audio tracks are eng then ger (see BRIEF: target	wo-audio.mkv).
        let src = std::env::var("TEST_AUDIO_STREAM").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/target/two-audio.mkv").into());
        let p = Mpv::play(&src, "test", None, None, vec![], None).unwrap();
        let mut pb = Playback::default();
        let wait = |pb: &mut Playback, done: &dyn Fn(&Playback) -> bool| {
            let t = std::time::Instant::now();
            while !done(pb) {
                assert_eq!(p.pump(pb), None, "ended early");
                assert!(t.elapsed().as_secs() < 20, "timed out: {:?} aid={}", pb.audio, pb.aid);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        wait(&mut pb, &|pb| pb.started && pb.audio.len() == 2 && pb.aid == "1");
        assert!(pb.audio[0].label.starts_with("English") && pb.audio[1].label.starts_with("German · Commentary"), "{:?}", pb.audio);
        p.command(&["set", "aid", "2"]).unwrap();
        wait(&mut pb, &|pb| pb.aid == "2");
        // A quality switch keeps the language: track 2 (German) again, chosen by language.
        p.command(&["set", "aid", "2"]).unwrap();
        (pb.started, pb.aid) = (false, String::new());
        p.switch(&src, pb.pos).unwrap();
        wait(&mut pb, &|pb| pb.started && pb.aid == "2");
        p.command(&["quit"]).unwrap();
        let t = std::time::Instant::now();
        while p.pump(&mut pb).is_none() { assert!(t.elapsed().as_secs() < 10, "no shutdown"); std::thread::sleep(std::time::Duration::from_millis(50)) }
    }

    #[test]
    fn loader_shows_from_the_click_through_seeks_and_stalls() {
        let p = |f: fn(&mut Playback)| { let mut p = Playback { started: true, ..Default::default() }; f(&mut p); p.loading() };
        assert!(Playback::default().loading(), "from play(), before mpv says anything");
        assert!(!p(|_| {}), "playing");
        assert!(p(|p| p.seeking = true), "a seek, from its first instant");
        assert!(p(|p| p.idle = true), "the core waiting while not paused: a stall starting");
        assert!(!p(|p| { p.idle = true; p.paused = true }), "paused isn't loading");
        assert!(p(|p| p.buffering = true));
        // The click's own frame shows it: the page switches and the loader is on it.
        let rt = runtime(|s| { demo_player(s); s.playback = Playback::default() });
        assert!(rt.find("loader").is_some());
    }

    #[test]
    fn controls_land_at_once_when_the_window_resizes() {
        // Fullscreen and back: the window jumps to the new size, and the controls with it. Nothing
        // may travel there over frames, with or without a panel open.
        for panel in [false, true] {
            let mut rt = runtime(move |s| { demo_player(s); s.subs_open = panel });
            for _ in 0..60 { rt.frame(1.0 / 60.0); }
            for (w, h) in [(1920.0, 1080.0), (1280.0, 800.0)] {
                rt.set_viewport((w, h), 2.0);
                // As the fullscreen button does: the state change in the same frame, instant.
                rt.update(move |s, tx| { tx.instant(); s.fullscreen = w > 1280.0 });
                let mut seen = vec![];
                for _ in 0..20 {
                    rt.frame(1.0 / 60.0);
                    seen.push((rt.find("toggle").unwrap(), rt.find("fullscreen").unwrap(), rt.find("back").unwrap()));
                }
                assert!(seen.windows(2).all(|p| p[0] == p[1]), "panel {panel}, {w}x{h}: moved over frames: {:?}", &seen[..4]);
                let (toggle, full, _) = seen[0];
                assert!((toggle.1 - (h - BAR / 2.0)).abs() < 1.0 && full.0 > w - 60.0 - if panel { SUBS_W } else { 0.0 }, "at the new edges at once");
            }
            // Showing and hiding the bars still fades.
            rt.update(|s, _| s.controls = false);
            rt.frame(1.0 / 60.0);
            let fading = rt.presented("controls").map(|(_, o)| o);
            assert!(fading.is_some_and(|o| o > 0.05 && o < 1.0), "fading out, not gone at once: {fading:?}");
            for _ in 0..30 { rt.frame(1.0 / 60.0); }
            assert!(rt.find("bar").is_none());
        }
    }

    thread_local! {
        /// What the app sent mpv (`mpv`), for tests without a player.
        pub static MPV: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn sent() -> Vec<String> { MPV.with(|log| std::mem::take(&mut *log.borrow_mut())) }

    #[test]
    fn volume_slider_drags_scrolls_mutes_and_is_remembered() {
        let mut rt = runtime(demo_player);
        let (x, y) = rt.find("volume").expect("slider");
        let w = ui::VOL_W;
        sent();
        rt.pointer_down(x - w / 2.0 + 1.0, y);
        rt.pointer_move(x - w / 4.0, y);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.vol_drag.is_some_and(|v| (v - 25.0).abs() < 3.0), "follows the pointer: {:?}", rt.state.vol_drag);
        rt.pointer_up(x - w / 4.0, y);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.vol_drag.is_none() && (rt.state.playback.volume - 25.0).abs() < 3.0);
        assert!(sent().iter().any(|c| c.starts_with("set volume")), "each step goes to mpv");
        let mut wheel = rune::WheelEvent::default();
        (wheel.x, wheel.y, wheel.dy) = (x, y, 40.0);
        rt.wheel_event(wheel);
        rt.frame(1.0 / 60.0);
        assert_eq!(sent(), ["add volume 5"], "a wheel step over the slider");
        click(&mut rt, "mute");
        assert_eq!(sent(), ["cycle mute"]);
        // Saved once it has settled for a second, and handed to mpv at the next start.
        let mut s = state();
        s.playback = Playback { started: true, volume: 60.0, ..Default::default() };
        remember_volume(&mut s);
        assert_eq!(s.settings.volume, 100, "not on the first step");
        s.volume_moved = Some(Instant::now() - Duration::from_secs(2));
        remember_volume(&mut s);
        assert_eq!(s.settings.volume, 60);
        assert!(settings::mpv_options(&s.settings).contains(&("volume", "60".to_string())));
    }

    #[test]
    #[ignore = "plays TEST_STREAM; opens a window briefly"]
    fn volume_starts_at_the_saved_one_and_is_observed() {
        let src = std::env::var("TEST_STREAM").expect("TEST_STREAM: a video");
        let p = Mpv::play(&src, "test", None, None, vec![("volume", "40".into())], None).unwrap();
        let mut pb = Playback::default();
        let wait = |pb: &mut Playback, done: &dyn Fn(&Playback) -> bool| {
            let t = Instant::now();
            while !done(pb) {
                assert_eq!(p.pump(pb), None, "ended early");
                assert!(t.elapsed().as_secs() < 20, "timed out: volume {} muted {}", pb.volume, pb.muted);
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&mut pb, &|pb| pb.started && pb.volume == 40.0);
        p.command(&["add", "volume", "5"]).unwrap();
        wait(&mut pb, &|pb| pb.volume == 45.0);
        p.command(&["cycle", "mute"]).unwrap();
        wait(&mut pb, &|pb| pb.muted);
        p.command(&["keypress", "UP"]).unwrap(); // no keybind here: mpv's own UP seeks; just mustn't break
        p.command(&["quit"]).unwrap();
        while p.pump(&mut pb).is_none() { std::thread::sleep(Duration::from_millis(20)) }
    }

    #[test]
    fn audio_labels() {
        assert_eq!(player::audio_label("eng", "", 6, "truehd"), "English · 5.1 · TrueHD");
        assert_eq!(player::audio_label("ger", "German", 2, "aac"), "German · Stereo · AAC");
        assert_eq!(player::audio_label("", "Commentary", 0, ""), "Commentary");
    }

    #[test]
    fn panel_lists_audio_above_subtitles() {
        let mut rt = runtime(|s| { demo_player(s); s.subs_open = true;
            s.playback.audio = vec![player::Track { id: "1".into(), label: "English".into(), external: String::new() }];
            s.playback.tracks = vec![player::Track { id: "2".into(), label: "ENG".into(), external: String::new() }] });
        let (a, sub) = (rt.find("aud-1").expect("audio row"), rt.find("sub-2").expect("subtitle row"));
        assert!(a.1 < sub.1, "audio above subtitles");
        sent();
        click(&mut rt, "aud-1");
        assert_eq!(sent(), ["set aid 1"]);
        click(&mut rt, "panel-close");
        assert!(!rt.state.subs_open);
    }

    #[test]
    fn controls_float_only_while_active_and_panels_toggle() {
        let mut rt = runtime(demo_player);
        assert!(rt.find("bar").is_some() && rt.find("video").is_some() && rt.find("loader").is_none());
        let (back, toggle) = (rt.find("back").expect("back"), rt.find("toggle").expect("play/pause"));
        assert!(back.1 < 100.0 && toggle.1 > 700.0, "back and the title at the top, play/pause at the bottom");
        click(&mut rt, "quality");
        assert!(rt.state.quality_open && !rt.state.subs_open);
        click(&mut rt, "subs"); // opening one panel closes the other
        assert!(rt.state.subs_open && !rt.state.quality_open);
        click(&mut rt, "subs");
        assert!(!rt.state.subs_open && !rt.state.quality_open);
        sent();
        click(&mut rt, "toggle");
        assert_eq!(sent(), ["cycle pause"]);
        // The bars go when the mouse rests, and the cursor with them.
        rt.update(|s, _| s.controls = false);
        for _ in 0..60 { rt.frame(1.0 / 60.0); }
        assert!(rt.find("bar").is_none() && rt.find("top-bar").is_none(), "controls hidden, top and bottom");
        let rt = runtime(|s| { demo_player(s); s.playback.buffering = true });
        assert!(rt.find("loader").is_some(), "the loader over the frozen frame");
    }

    #[test]
    fn quality_groups_streams_by_resolution_and_picks_switch() {
        let st = |name: &str, title: &str| Stream { name: name.into(), title: title.into(), url: format!("{name}|{title}") };
        assert_eq!(ui::resolution(&st("Torrentio 4k DV | HDR", "x.1080p.y")), "4K", "the name wins over the title");
        assert_eq!(ui::resolution(&st("Torrentio", "Movie.2026.2160p.WEB-DL")), "4K");
        assert_eq!(ui::resolution(&st("[RD+] Torrentio 1080p", "")), "1080p");
        assert_eq!(ui::resolution(&st("Addon", "Movie 720p\n👤 3")), "720p");
        assert_eq!(ui::resolution(&st("Addon", "Movie.576p.DVDRip")), "480p");
        assert_eq!(ui::resolution(&st("4kids TV", "Direct")), "Other", "whole words only");
        let streams = vec![st("A 720p", ""), st("B", "x.2160p"), st("C", ""), st("D 1080p", ""), st("E 4K", "")];
        let groups: Vec<_> = ui::qualities(&streams).into_iter().map(|(r, g)| (r, g.iter().map(|(i, _)| *i).collect::<Vec<_>>())).collect();
        assert_eq!(groups, [("4K", vec![1, 4]), ("1080p", vec![3]), ("720p", vec![0]), ("Other", vec![2])]);

        let mut rt = runtime(|s| { demo_player(s); s.quality_open = true });
        assert!(rt.find("q-0").unwrap().1 < rt.find("q-1").unwrap().1, "4K first");
        click(&mut rt, "q-1"); // the stream playing: nothing to switch, the panel closes
        assert!(!rt.state.quality_open && rt.state.playing == "b");
    }

    fn runtime(scene: impl FnOnce(&mut State)) -> rune::Runtime<State> {
        let mut s = state();
        scene(&mut s);
        let mut rt = rune::Runtime::new(s, ui::view, (1280.0, 800.0 + window::caption().h), rune::Env { system_dark: true, scale: 2.0 });
        rt.frame(0.0);
        rt
    }

    fn click<S: 'static>(rt: &mut rune::Runtime<S>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
    }

    #[test]
    fn title_strip_sits_above_every_page_except_fullscreen() {
        let caption = window::caption();
        assert!(caption.h > 20.0 && caption.h < 50.0, "the system caption's height: {caption:?}");
        for page in [Page::Home, Page::Settings, Page::Addons, Page::Player] {
            let rt = runtime(move |s| { demo(s); s.page = page });
            assert!(rt.find("title-bar").is_some_and(|t| t.1 < caption.h), "the strip");
            // macOS: the window's own traffic lights; elsewhere the app draws the buttons.
            if cfg!(target_os = "macos") { assert!(rt.find("caption-close").is_none()); continue }
            let (min, max, close) = (rt.find("caption-min").unwrap(), rt.find("caption-max").unwrap(), rt.find("caption-close").unwrap());
            assert!(min.0 < max.0 && max.0 < close.0 && close.0 > 1280.0 - window::CAPTION_BUTTON_W, "buttons at the right");
            assert!(close.1 < caption.h, "in the strip");
        }
        // The strip is clear: backdrops start at the window top and grow under it, and content
        // keeps its place on screen, below where the system title bar was.
        let at = |page: Page, key: &'static str, caption: window::Caption| runtime(move |s| {
            demo(s);
            s.page = page;
            s.caption = caption;
            if page == Page::Detail { s.meta = Some(s.items[&0][0].clone()) }
        }).find(key).unwrap_or_else(|| panic!("no {key}")).1;
        let none = window::Caption { h: 0.0, ..caption };
        for (page, key) in [(Page::Home, "nav-search"), (Page::Home, "watch"), (Page::Detail, "back"), (Page::Detail, "play"),
            (Page::Settings, "settings-sign-out"), (Page::Browse("movie"), "nav-search")] {
            assert!((at(page, key, caption) - at(page, key, none) - caption.h).abs() < 0.5, "{key} moves down by the strip");
        }
        for (page, key) in [(Page::Home, "hero"), (Page::Detail, "detail-hero")] {
            assert!((at(page, key, caption) - at(page, key, none) - caption.h / 2.0).abs() < 0.5, "{key} starts at the top, taller");
        }
        let rt = runtime(|s| { demo(s); s.page = Page::Player; s.fullscreen = true });
        assert!(rt.find("title-bar").is_none(), "fullscreen hides it");
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

    /// ~100 streams as Torrentio sends them: multi-line titles with emoji and flags.
    fn torrentio(n: usize) -> Vec<Stream> {
        (0..n).map(|i| Stream {
            name: format!("Torrentio {}", ["4k DV | HDR", "1080p", "720p", "480p"][i % 4]),
            title: format!("The.Long.Quiet.2026.S01E02.{}p.WEB-DL.DDP5.1.Atmos.H.264-GROUP{i}\n👤 {} 💾 {}.{} GB ⚙️ ThePirateBay\n🇬🇧 / 🇩🇪 / 🇫🇷",
                [2160, 1080, 720, 480][i % 4], 400 - i, 1 + i % 20, i % 10),
            url: format!("http://127.0.0.1:11470/{i:040}/0"),
        }).collect()
    }

    #[test]
    fn streams_sheet_builds_only_the_cards_near_view() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Detail; s.meta = Some(s.items[&0][0].clone());
            s.stream_for = "tt1".into(); s.streams = torrentio(100); s.sheet = true });
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(rt.find("st-0").is_some(), "the first card shows");
        assert!(rt.find("st-99").is_none(), "far cards aren't built");
        // A later addon's answer appends without touching what's built.
        rt.state.tx.send(Msg::Streams("tt1".into(), torrentio(3))).unwrap();
        rt.update(|s, tx| { poll(s, 0.0, tx); });
        for _ in 0..5 { rt.frame(1.0 / 60.0); }
        assert_eq!(rt.state.streams.len(), 103);
        assert!(rt.find("st-0").is_some());
    }

    #[test]
    #[ignore = "timing: cargo test sheet_timing -- --ignored --nocapture"]
    fn sheet_timing() {
        let detail = |s: &mut State| {
            demo(s);
            let mut m = s.items[&0][0].clone();
            m["type"] = "series".into();
            m["videos"] = (1..9).map(|e| json!({ "id": format!("tt1:1:{e}"), "season": 1, "episode": e, "title": format!("Episode {e}") })).collect();
            s.meta_id = "tt1".into();
            s.meta = Some(m);
            s.page = Page::Detail;
        };
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let frames = |rt: &mut rune::Runtime<State>, n: usize| -> (f64, f64) {
            let (mut total, mut worst) = (0.0, 0.0f64);
            for _ in 0..n { let t = Instant::now(); rt.frame(1.0 / 60.0); let d = ms(t); total += d; worst = worst.max(d) }
            (total, worst)
        };
        // Streams already loaded (a movie, or back on the same episode): open the sheet.
        let mut rt = runtime(detail);
        rt.state.stream_for = "tt1:1:2".into();
        rt.state.streams = torrentio(100);
        frames(&mut rt, 5);
        let t = Instant::now();
        rt.update(|s, _| s.season = 1); // any change: the view runs again
        rt.frame(1.0 / 60.0);
        println!("detail page rebuild alone: {:.0} ms", ms(t));
        let t = Instant::now();
        rt.update(|s, _| s.sheet = true);
        let first = { rt.frame(1.0 / 60.0); ms(t) };
        let (rest, worst) = frames(&mut rt, 30);
        println!("open with 100 loaded: first frame {first:.0} ms, next 30 frames {rest:.0} ms (worst {worst:.0} ms)");
        // Episode click: the sheet opens empty, then 5 addons answer with 20 streams each.
        let mut rt = runtime(detail);
        frames(&mut rt, 5);
        let t = Instant::now();
        rt.update(|s, _| { s.stream_for = "tt1:1:2".into(); s.sheet = true });
        let first = { rt.frame(1.0 / 60.0); ms(t) };
        let mut arrivals = vec![];
        for chunk in torrentio(100).chunks(20) {
            rt.state.tx.send(Msg::Streams("tt1:1:2".into(), chunk.to_vec())).unwrap();
            let t = Instant::now();
            rt.update(|s, tx| { poll(s, 0.0, tx); });
            rt.frame(1.0 / 60.0);
            arrivals.push(ms(t));
        }
        let (rest, worst) = frames(&mut rt, 30);
        println!("open empty: first frame {first:.0} ms; each of 5 arrivals (20 streams): {arrivals:.0?} ms; next 30 frames {rest:.0} ms (worst {worst:.0} ms)");
    }

    #[test]
    fn a_movie_listed_by_tmdb_id_asks_for_streams_by_its_imdb_id() {
        let mut rt = runtime(|s| { demo(s); s.meta_id = "tmdb:680".into(); s.page = Page::Detail });
        let meta = json!({ "id": "tmdb:680", "type": "movie", "name": "Pulp Fiction", "behaviorHints": { "defaultVideoId": "tt0110912" } });
        rt.state.tx.send(Msg::Meta("tmdb:680".into(), meta)).unwrap();
        rt.update(|s, tx| { poll(s, 0.0, tx); });
        assert_eq!(rt.state.stream_for, "tt0110912");
        assert_eq!(movie_video(&json!({ "id": "tt1" }), "x"), "tt1");
        assert_eq!(movie_video(&json!({}), "x"), "x");
    }

    #[test]
    fn the_hero_cycles_through_the_chosen_sections() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Home });
        let ids = |s: &State| hero_picks(s).iter().map(|p| p.0["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(&rt.state), ["tt1", "tt2", "tt3", "tt4", "tt5"], "by default the first row's");
        assert_eq!(row_items(&rt.state, 0).len(), 3, "whose row starts after the hero's picks");
        let key = catalog_key(&rt.state.catalogs[1]);
        rt.update(|s, _| { s.settings.heroes = vec![key.clone()]; hero_changed(s) });
        assert_eq!(ids(&rt.state)[0], "tt9");
        assert_eq!((row_items(&rt.state, 0).len(), row_items(&rt.state, 1).len()), (8, 3), "now the chosen row skips them");
        assert_eq!(hero_picks(&rt.state)[0].1, "Popular Shows");
        rt.update(|s, _| { s.settings.heroes = vec!["watchlist".into()]; hero_changed(s) });
        assert_eq!(ids(&rt.state)[0], "tt1", "an empty Watchlist: the first row's");
        // Several: in turn, each title labelled with its section.
        rt.update(|s, _| { s.saved = vec![json!({ "_id": "tt42", "type": "movie", "name": "Saved" })]; s.settings.heroes = vec!["watchlist".into(), key.clone(), "upnext".into()] });
        let picks = hero_picks(&rt.state);
        assert_eq!(ids(&rt.state), ["tt42", "tt9", "tt1", "tt10", "tt2", "tt11"], "round robin, six for three sections");
        assert_eq!(picks.iter().map(|p| p.1.as_str()).collect::<Vec<_>>(), ["Watchlist", "Popular Shows", "Up Next", "Popular Shows", "Up Next", "Popular Shows"]);
        assert_eq!(row_items(&rt.state, 1).len(), 5, "the row skips the three the hero shows");
        rt.update(|s, _| s.settings.heroes = vec!["gone|movie|x".into()]);
        assert_eq!(ids(&rt.state)[0], "tt1", "a catalog no longer installed: the first row's");
    }

    #[test]
    fn a_person_on_a_detail_page_searches_their_name() {
        let mut rt = runtime(|s| {
            demo(s);
            let mut m = s.items[&0][0].clone();
            for (k, v) in demo_credits().as_object().unwrap() { m[k] = v.clone() }
            s.meta_id = "tt1".into();
            s.meta = Some(m);
            s.page = Page::Detail;
            s.search.cats = vec![demo_cat("movie", "Movies")];
        });
        // Cast & Crew sits under the hero: scroll down to it first.
        rt.wheel(640.0, 400.0, -400.0);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        click(&mut rt, "person-1");
        assert!(rt.state.page == Page::Search);
        assert_eq!(rt.state.search.query.text, "Ines Kovač");
        assert!(!rt.state.search.debounce.is_pending(), "asked at once, not after typing");
        rt.state.tx.send(Msg::Search("Ines Kovač".into(), 0, vec![json!({ "id": "tt7", "type": "movie", "name": "Her Film" })])).unwrap();
        rt.update(|s, tx| { poll(s, 0.0, tx); });
        assert_eq!(rt.state.search.results[&0].len(), 1, "the answers for the name land");
    }

    #[test]
    fn cast_photos_come_from_the_first_addon_that_has_them() {
        let mut rt = runtime(|s| {
            demo(s);
            let mut m = s.items[&0][0].clone();
            for (k, v) in demo_credits().as_object().unwrap() { m[k] = v.clone() }
            s.meta_id = "tt1".into();
            s.meta = Some(m);
            s.page = Page::Detail;
        });
        let cast = |who: &str| vec![json!({ "name": "Ines Kovač", "character": who, "photo": "https://image.tmdb.org/t/p/face/a.jpg" })];
        for msg in [Msg::Cast("tt9".into(), cast("Other title")), Msg::Cast("tt1".into(), vec![]),
            Msg::Cast("tt1".into(), cast("Lena Voss")), Msg::Cast("tt1".into(), cast("Second addon"))] { rt.state.tx.send(msg).unwrap() }
        rt.update(|s, tx| { poll(s, 0.0, tx); });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.people.len(), 1);
        assert_eq!(rt.state.people[0]["character"], "Lena Voss");
        let people = ui::credits(&rt.state, rt.state.meta.as_ref().unwrap());
        let ines = people.iter().find(|p| p.0 == "Ines Kovač").unwrap();
        assert_eq!(ines.1, ["Lena Voss"]);
        assert!(ines.2.is_some(), "with the addon's photo");
        assert_eq!(people[0].1, ["Director", "Writer"], "directors first, roles merged");
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
        // The episodes sit under the hero: scroll down to them first.
        rt.wheel(640.0, 400.0, -400.0);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
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
    fn home_rows_page_by_arrows_and_keys_and_stop_at_the_ends() {
        // The first catalog's row, on screen under the hero: 5 hero picks, then 8 titles.
        let mut rt = runtime(|s| {
            demo(s);
            s.library.clear();
            s.items.insert(0, (1..14).map(|n| json!({ "id": format!("tt{n}"), "name": "x" })).collect());
            s.page = Page::Home;
        });
        assert!(!can_page(&rt.state, 0, -1), "back is off at the start");
        click(&mut rt, "prev-0");
        assert_eq!(row_at(&rt.state, 0), 0);
        assert!(rt.find("c0-tt6").is_some() && rt.find("c0-tt11").is_none(), "the row starts after the hero's picks");
        click(&mut rt, "next-0");
        assert_eq!(row_at(&rt.state, 0), 3, "the last page ends flush with the eighth title");
        assert!(rt.find("c0-tt13").is_some() && rt.find("c0-tt6").is_none(), "the window moved");
        assert!(!can_page(&rt.state, 0, 1), "and on is off at the end");
        click(&mut rt, "prev-0");
        assert_eq!(row_at(&rt.state, 0), 0);
        // Up Next pages the whole library; the arrow keys turn the row under the mouse.
        rt.state.library = (1..13).map(|n| json!({ "_id": format!("tt{n}"), "type": "movie", "name": "x", "state": {} })).collect();
        rt.state.row_hover = Some(UP_NEXT);
        rt.on_key = Some(on_key);
        rt.key(KeyEvent { key: Key::Right, mods: Default::default() });
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert_eq!(row_at(&rt.state, UP_NEXT), 5);
        assert!(rt.find("cw-tt6").is_some() && rt.find("cw-tt5").is_none());
    }

    #[test]
    fn a_row_near_its_end_fetches_the_next_page_with_skip_and_drops_titles_it_has() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let mut rt = runtime(|s| {
            demo(s);
            s.catalogs[0].base = base;
            s.catalogs[0].skip = true;
            s.library.clear();
            s.items.insert(0, (1..14).map(|n| json!({ "id": format!("tt{n}"), "name": "x" })).collect()); // 5 hero picks + 8
            s.page = Page::Home;
        });
        click(&mut rt, "next-0");
        assert_eq!(row_at(&rt.state, 0), 3);
        let (mut conn, _) = server.accept().unwrap();
        let mut req = [0u8; 1024];
        let n = conn.read(&mut req).unwrap();
        let line = String::from_utf8_lossy(&req[..n]).lines().next().unwrap().to_string();
        assert_eq!(line, "GET /catalog/movie/top/skip=13.json HTTP/1.1");
        let body = r#"{"metas":[{"id":"tt13"},{"id":"tt14"},{"id":"tt14"},{"id":"tt15"}]}"#;
        write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(conn);
        assert!(rt.state.more_loading.contains(&0), "one page at a time");
        click(&mut rt, "next-0"); // nothing more to show yet, and no second request
        let Ok(Msg::More(r, metas)) = rt.state.rx.recv_timeout(Duration::from_secs(5)) else { panic!("no page") };
        more_landed(&mut rt.state, r, metas);
        let ids: Vec<&str> = rt.state.items[&0].iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 15, "tt13 and the second tt14 dropped");
        assert_eq!(&ids[12..], ["tt13", "tt14", "tt15"]);
        assert!(can_page(&rt.state, 0, 1), "the new titles page on");
        more_landed(&mut rt.state, 0, vec![json!({ "id": "tt15" })]);
        assert!(rt.state.more_done.contains(&0), "a page with nothing new ends the catalog");
    }

    #[test]
    fn home_queues_the_hero_first_then_rows_near_the_viewport_and_drops_them_on_leaving() {
        let mut rt = runtime(|s| {
            demo(s);
            for i in 2..HOME_ROWS {
                s.catalogs.push(Catalog { ty: "movie".into(), id: format!("c{i}"), name: format!("Row {i}"), ..Default::default() });
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
    fn search_opens_types_shows_results_and_opens_a_title() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Home; s.search.cats = vec![demo_cat("movie", "Movies")] });
        click(&mut rt, "nav-search");
        assert!(rt.state.page == Page::Search);
        rt.paste("long quiet"); // lands in the field: it's focused on open
        for _ in 0..5 { rt.frame(1.0 / 60.0); }
        assert_eq!(rt.state.search.query.text, "long quiet");
        assert!(rt.state.search.debounce.is_pending(), "typing schedules a search");
        // Stub the network: drop the real request and answer it by hand, plus a stale answer.
        rt.state.search.debounce.cancel();
        let metas = rt.state.items[&0].clone();
        rt.state.tx.send(Msg::Search("long".into(), 0, vec![json!({ "id": "stale" })])).unwrap();
        rt.state.tx.send(Msg::Search("long quiet".into(), 0, metas)).unwrap();
        rt.update(|s, tx| { poll(s, 0.0, tx); });
        for _ in 0..5 { rt.frame(1.0 / 60.0); }
        assert!(rt.find("s0-stale").is_none(), "answers for older text are dropped");
        click(&mut rt, "s0-tt2");
        assert!(rt.state.page == Page::Detail && rt.state.meta_id == "tt2");
        click(&mut rt, "back");
        assert!(rt.state.page == Page::Search, "back returns to the results");
        assert_eq!(rt.state.search.query.text, "long quiet", "with the query kept");
        click(&mut rt, "nav-search");
        for _ in 0..5 { rt.frame(1.0 / 60.0); }
        rt.key(KeyEvent { key: Key::Escape, mods: Default::default() }); // field focused: one press leaves
        for _ in 0..5 { rt.frame(1.0 / 60.0); }
        assert!(rt.state.page == Page::Home, "Esc leaves search");
    }

    #[test]
    fn search_catalogs_come_from_search_extras() {
        let cat = |ty: &str, id: &str, extra: Value| json!({ "type": ty, "id": id, "name": "Top", "extra": extra });
        let addon = |name: &str, cats: Value| json!({ "transportUrl": format!("https://{name}/manifest.json"), "manifest": { "name": name, "catalogs": cats } });
        let addons = vec![
            addon("Cinemeta", json!([
                cat("movie", "top", json!([{ "name": "genre" }])),
                cat("movie", "top", json!([{ "name": "search", "isRequired": true }])),
                cat("series", "top", json!([{ "name": "search", "isRequired": true }])),
                cat("movie", "year", json!([{ "name": "genre", "isRequired": true }, { "name": "search" }])),
            ])),
            addon("Anime", json!([cat("series", "a", json!([{ "name": "search" }]))])),
        ];
        let names: Vec<_> = search::catalogs(&addons).into_iter().map(|c| (c.base, c.name)).collect();
        assert_eq!(names, [("https://Cinemeta".into(), "Movies".into()), ("https://Cinemeta".into(), "Shows".to_string()), ("https://Anime".into(), "Anime · Shows".into())]);
        assert_eq!(catalogs(&addons).len(), 2, "catalogs that require search stay off Home");
    }

    #[test]
    #[ignore = "starts Stremio's streaming server (ends with the test process)"]
    fn streaming_server_starts_when_down() {
        let was_up = server::running();
        server::ensure().unwrap();
        let t = Instant::now();
        while !server::running() { assert!(t.elapsed().as_secs() < 20, "server never came up"); std::thread::sleep(Duration::from_millis(200)) }
        if !was_up { assert!(get("http://127.0.0.1:11470/settings").is_ok(), "it answers HTTP") }
    }

    #[test]
    fn every_hero_backdrop_is_fetched_when_the_hero_titles_arrive() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Home; s.items.clear() });
        let titles: Vec<Value> = (1..9).map(|n| json!({ "id": format!("tt{n}"), "type": "movie", "name": "x" })).collect();
        rt.state.tx.send(Msg::Catalog(0, titles.clone())).unwrap();
        rt.update(|s, tx| { poll(s, 0.0, tx); });
        let queued: Vec<String> = rt.state.queue.0.lock().unwrap().iter().cloned().collect();
        for m in titles.iter().take(HERO_PICKS) {
            let key = ui::hero_img(backdrop(m)).unwrap();
            assert!(queued.contains(&key) || rt.state.requested.contains(&key), "hero image queued before it shows: {key}");
        }
    }

    #[test]
    fn a_click_on_the_video_closes_an_open_panel_before_it_pauses() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Player; s.controls = true; s.subs_open = true });
        click(&mut rt, "video");
        assert!(!rt.state.subs_open, "the first click closes the panel");
        rt.state.quality_open = true;
        click(&mut rt, "video");
        assert!(!rt.state.quality_open);
    }

    #[test]
    fn hero_advances_every_few_seconds_and_wraps() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Home });
        let tick = |rt: &mut rune::Runtime<State>| rt.update(|s, tx| { rotate_hero(s, tx); });
        tick(&mut rt);
        assert_eq!(rt.state.hero, 0, "not before its time");
        for want in [1, 2, 3, 4, 0] {
            rt.state.hero_at -= HERO_EVERY;
            tick(&mut rt);
            assert_eq!(rt.state.hero, want);
        }
        rt.state.page = Page::Detail;
        rt.state.hero_at -= HERO_EVERY;
        tick(&mut rt);
        assert_eq!(rt.state.hero, 0, "only on Home");
        rt.state.page = Page::Home;
        rt.update(|s, tx| on_focus(s, false, tx));
        rt.frame(1.0 / 60.0);
        rt.state.hero_at -= HERO_EVERY;
        tick(&mut rt);
        assert_eq!(rt.state.hero, 0, "holds while the window is in the background");
        assert!(rt.find(&format!("fill-{}", rt.state.hero_round)).is_none(), "and its dot doesn't fill");
        let round = rt.state.hero_round;
        rt.update(|s, tx| on_focus(s, true, tx));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.hero == 0 && rt.state.hero_round == round + 1, "back in focus: a full turn for the same title");
        assert!(rt.find(&format!("fill-{}", rt.state.hero_round)).is_some());
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
    fn settings_round_trip_with_defaults_for_what_is_missing() {
        let dir = std::env::temp_dir().join(format!("stremio-rune-test-{}", std::process::id()));
        let f = dir.join("settings.json");
        assert_eq!(settings::load(&f), settings::Settings::default(), "no file: defaults");
        let mut p = settings::Settings::default();
        p.sub_lang = "ger".into();
        p.seek_ms = 10000;
        p.hwdec = false;
        settings::save_to(&f, &p).unwrap();
        assert_eq!(settings::load(&f), p);
        std::fs::write(&f, r#"{"sub_size": 150, "unknown": 1}"#).unwrap();
        assert_eq!(settings::load(&f), settings::Settings { sub_size: 150, ..Default::default() }, "missing fields: defaults");
        std::fs::write(&f, "not json").unwrap();
        assert_eq!(settings::load(&f), settings::Settings::default(), "unreadable: defaults");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_become_mpv_options() {
        let opts = |p: &settings::Settings| settings::mpv_options(p).into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>();
        let d = opts(&settings::Settings::default());
        assert_eq!(d["hwdec"], "auto-safe", "today's behaviour");
        assert_eq!(d["sub-scale"], "1");
        assert_eq!(d["cursor-autohide"], "2500");
        assert!(!d.contains_key("alang") && !d.contains_key("slang"), "no language: the file decides");
        let o = opts(&settings::Settings { audio_lang: "ger".into(), sub_lang: "eng".into(), sub_size: 150, sub_color: "#FFE14D".into(),
            sub_style: "box".into(), hwdec: false, ..Default::default() });
        assert_eq!((o["alang"].as_str(), o["slang"].as_str()), ("ger,deu,de", "eng,en"));
        assert_eq!((o["hwdec"].as_str(), o["sub-scale"].as_str(), o["sub-color"].as_str()), ("no", "1.5", "#FFE14D"));
        assert_eq!(o["sub-border-style"], "background-box");
        assert!(o.contains_key("sub-back-color"));
    }

    #[test]
    fn server_settings_post_what_stremio_core_posts() {
        let values = json!({ "serverVersion": "4.22.3", "appPath": "x", "cacheSize": 2147483648u64, "cacheRoot": "C:/c", "btMaxConnections": 55,
            "btHandshakeTimeout": 20000, "btRequestTimeout": 4000, "btDownloadSpeedSoftLimit": 2621440, "btDownloadSpeedHardLimit": 3670016,
            "btMinPeersForStable": 5, "remoteHttps": "", "proxyStreamsEnabled": false, "transcodeProfile": null, "allTranscodeProfiles": ["nvenc-win"] });
        let body = settings::server_body(&values, &json!({ "cacheSize": null }));
        let o = body.as_object().unwrap();
        assert_eq!(o.len(), 11, "only the fields the server takes");
        assert!(o["cacheSize"].is_null() && o["btMaxConnections"] == 55 && o["cacheRoot"] == "C:/c");
        assert!(!o.contains_key("serverVersion") && !o.contains_key("allTranscodeProfiles"));
    }

    #[test]
    fn settings_open_from_the_menu_and_choices_stick() {
        let mut rt = runtime(|s| { demo(s); s.page = Page::Home });
        click(&mut rt, "avatar");
        click(&mut rt, "settings");
        assert!(rt.state.page == Page::Settings && !rt.state.menu);
        click(&mut rt, "sub-size-3");
        assert_eq!(rt.state.settings.sub_size, 150);
        click(&mut rt, "hwdec");
        assert!(!rt.state.settings.hwdec);
        click(&mut rt, "audio-lang");
        click(&mut rt, "audio-lang-2");
        assert_eq!((rt.state.settings.audio_lang.as_str(), rt.state.choice), ("ger", None), "picked and folded");
        click(&mut rt, "settings-sign-out");
        assert!(rt.state.page == Page::Login);
    }

    /// An account's collection as the API sends it: keys in its order, a float, an escape.
    const COLLECTION: &str = r#"[
        {"transportUrl":"https://v3-cinemeta.strem.io/manifest.json","transportName":"","manifest":{"name":"Cinemeta","id":"com.linvo.cinemeta","version":"3.0.14","zeta":1,"alpha":0.5},"flags":{"official":true,"protected":true}},
        {"transportUrl":"https://subs/manifest.json","transportName":"","manifest":{"id":"subs","version":"1.0.0","name":"Subs é"},"flags":{"official":true,"protected":false}},
        {"transportUrl":"https://a/manifest.json","transportName":"","manifest":{"id":"a","version":"1.0.0","name":"A","behaviorHints":{"configurable":true}},"flags":{"official":false,"protected":false}},
        {"transportUrl":"https://b/manifest.json","transportName":"","manifest":{"version":"2.1.0","id":"b","name":"B","weird":[1e3,-0.1]},"flags":{"protected":false,"official":false}}
    ]"#;

    #[test]
    fn addon_edits_change_exactly_one_entry_and_keep_the_rest_as_they_were() {
        use addons::{Change, apply, descriptor};
        let list: Vec<Value> = serde_json::from_str(COLLECTION).unwrap();
        let text = |v: &[Value]| v.iter().map(|a| serde_json::to_string(a).unwrap()).collect::<Vec<_>>();
        // Install appends, everything before it untouched (byte for byte once serialized).
        let d = descriptor("https://new/manifest.json", json!({ "id": "new", "name": "New", "version": "0.1.0" }));
        let out = apply(&list, &Change::Install(d.clone())).unwrap();
        assert_eq!(out.len(), 5);
        assert_eq!(text(&out[..4]), text(&list));
        assert_eq!(out[4], d);
        assert_eq!(serde_json::to_string(&out[0]).unwrap(), r#"{"transportUrl":"https://v3-cinemeta.strem.io/manifest.json","transportName":"","manifest":{"name":"Cinemeta","id":"com.linvo.cinemeta","version":"3.0.14","zeta":1,"alpha":0.5},"flags":{"official":true,"protected":true}}"#,
            "key order and numbers survive a round trip");
        assert!(apply(&list, &Change::Install(list[2].clone())).is_err(), "no duplicates");
        assert!(apply(&list, &Change::Install(json!({ "transportUrl": "x" }))).is_err(), "not an addon");
        // Uninstall removes exactly that one, order kept.
        let out = apply(&list, &Change::Uninstall("https://a/manifest.json".into())).unwrap();
        assert_eq!(text(&out), [&text(&list)[..2], &text(&list)[3..]].concat());
        // Official and protected ones stay; so do unknown or doubled ones.
        assert!(apply(&list, &Change::Uninstall("https://v3-cinemeta.strem.io/manifest.json".into())).is_err());
        assert!(apply(&list, &Change::Uninstall("https://subs/manifest.json".into())).is_err());
        assert!(apply(&list, &Change::Uninstall("https://gone/manifest.json".into())).is_err());
        let doubled = [list.clone(), vec![list[3].clone()]].concat();
        assert!(apply(&doubled, &Change::Uninstall("https://b/manifest.json".into())).is_err());
        // An empty read is never written over the account.
        assert!(apply(&[], &Change::Install(d)).is_err());
    }

    #[test]
    fn addons_page_confirms_uninstall_and_never_writes_from_tests() {
        let mut rt = runtime(|s| {
            demo(s);
            s.page = Page::Home;
            s.auth = Some("test".into());
            s.addons = serde_json::from_str(COLLECTION).unwrap();
            s.store.discover = vec![json!({ "transportUrl": "https://a/manifest.json", "transportName": "", "manifest": { "id": "a", "name": "A", "version": "1" } })];
        });
        click(&mut rt, "nav-Add-ons");
        assert!(rt.state.page == Page::Addons);
        assert!(rt.find("ad-uninstall-0").is_none() && rt.find("ad-uninstall-1").is_none(), "official addons have no Uninstall");
        assert!(rt.find("ad-config-2").is_some() && rt.find("ad-config-3").is_none(), "Configure only when configurable");
        click(&mut rt, "ad-uninstall-3");
        assert_eq!(rt.state.store.confirm.as_deref(), Some("https://b/manifest.json"));
        assert!(rt.state.status.is_empty(), "the first press only asks");
        click(&mut rt, "ad-uninstall-3");
        assert_eq!(rt.state.status, "would write: Uninstalled B");
        click(&mut rt, "tab-discover");
        assert!(rt.find("dc-install-0").is_none(), "installed ones say so instead");
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
        let mut keys = vec![];
        add_keys(&mut keys, old.as_bytes());
        add_keys(&mut keys, old.as_bytes());
        add_keys(&mut keys, b"nothing here");
        assert_eq!(keys, [k, "old0123456789abcdef"]);
        assert_eq!(catalog_title("Multiboxd - Watchlist", "Multiboxd"), "Watchlist");
        assert_eq!(catalog_title("Netflix", "Streaming Catalogs"), "Netflix");
        assert_eq!(catalog_title("Multiboxd", "Multiboxd"), "Multiboxd");
        assert_eq!(addon_name(&json!({ "manifest": { "name": "Watchly | ElfHosted" } })), "Watchly");
        assert_eq!(ui::date("2008-01-28T05:00:00.000Z"), "Jan 28, 2008");
        assert_eq!(ui::date(""), "");
        assert_eq!(ui::clock(3723.0), "1:02:03");
        assert_eq!(ui::clock(65.9), "1:05");
    }
}
