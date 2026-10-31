//! Settings: the app's own (player defaults, kept in `%APPDATA%\stremio-rune\settings.json`) and
//! Stremio's streaming server's (read from and written to its `/settings`, as stremio-web does).
//! A child of `ui` (see the `#[path]` there) so the page can use its private helpers.

use super::*;
use serde::{Deserialize, Serialize};

/// Choices, as stremio-web offers them where it has the setting.
pub const SUB_SIZES: [u32; 7] = [75, 100, 125, 150, 175, 200, 250];
pub const SEEK_STEPS: [u32; 6] = [3000, 5000, 10000, 15000, 20000, 30000];
pub const HIDE_DELAYS: [u32; 5] = [1500, 2500, 4000, 6000, 10000];
pub const COLORS: [(&str, &str); 4] = [("White", "#FFFFFF"), ("Yellow", "#FFE14D"), ("Cyan", "#7FE0FF"), ("Gray", "#C8C8C8")];
pub const STYLES: [(&str, &str); 3] = [("outline", "Outline"), ("box", "Dark box"), ("none", "None")];
/// Stremio's language codes, and the tags mpv should match for each (files use either form).
pub const LANGS: [(&str, &str); 26] = [
    ("eng", "eng,en"), ("ger", "ger,deu,de"), ("fre", "fre,fra,fr"), ("spa", "spa,es"), ("ita", "ita,it"),
    ("por", "por,pt"), ("dut", "dut,nld,nl"), ("rus", "rus,ru"), ("pol", "pol,pl"), ("tur", "tur,tr"),
    ("ara", "ara,ar"), ("chi", "chi,zho,zh"), ("jpn", "jpn,ja"), ("kor", "kor,ko"), ("swe", "swe,sv"),
    ("nor", "nor,no,nob,nb"), ("dan", "dan,da"), ("fin", "fin,fi"), ("gre", "gre,ell,el"), ("heb", "heb,he"),
    ("hun", "hun,hu"), ("cze", "cze,ces,cs"), ("rum", "rum,ron,ro"), ("hin", "hin,hi"), ("ukr", "ukr,uk"), ("vie", "vie,vi"),
];
/// The streaming server's cache sizes (bytes; null = unlimited) and torrent profiles
/// (stremio-web's `useStreamingOptions`).
fn cache_sizes() -> [(Value, &'static str); 5] {
    [(json!(0), "No caching"), (json!(2147483648u64), "2 GB"), (json!(5368709120u64), "5 GB"), (json!(10737418240u64), "10 GB"), (Value::Null, "Unlimited")]
}
const PROFILE_KEYS: [&str; 6] = ["btDownloadSpeedHardLimit", "btDownloadSpeedSoftLimit", "btHandshakeTimeout", "btMaxConnections", "btMinPeersForStable", "btRequestTimeout"];
fn profiles() -> [(&'static str, Value); 4] {
    [
        ("Default", json!([3670016, 2621440, 20000, 55, 5, 4000])),
        ("Soft", json!([1677721.6, 1677721.6, 20000, 35, 5, 4000])),
        ("Fast", json!([39321600, 4194304, 20000, 200, 10, 4000])),
        ("Ultra fast", json!([78643200, 8388608, 25000, 400, 10, 6000])),
    ]
}
/// The fields stremio-core POSTs back to `/settings` (its `UpdateSettings` body).
const SERVER_FIELDS: [&str; 11] = ["cacheSize", "cacheRoot", "btMaxConnections", "btHandshakeTimeout", "btRequestTimeout",
    "btDownloadSpeedSoftLimit", "btDownloadSpeedHardLimit", "btMinPeersForStable", "remoteHttps", "proxyStreamsEnabled", "transcodeProfile"];

/// The app's own settings. Defaults are how the app behaved before it had settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Stremio language codes; empty = the file's default track / no subtitles.
    pub audio_lang: String,
    pub sub_lang: String,
    /// Percent.
    pub sub_size: u32,
    pub sub_color: String,
    /// One of `STYLES`.
    pub sub_style: String,
    /// Arrow-key seek, and stillness before the playback controls hide, in ms.
    pub seek_ms: u32,
    pub hide_ms: u32,
    pub hwdec: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { audio_lang: String::new(), sub_lang: String::new(), sub_size: 100, sub_color: "#FFFFFF".into(),
            sub_style: "outline".into(), seek_ms: 5000, hide_ms: 2500, hwdec: true }
    }
}

pub fn file() -> PathBuf { auth_file().with_file_name("settings.json") }

/// Missing or unreadable file: defaults; missing fields: their defaults.
pub fn load(path: &std::path::Path) -> Settings {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_to(path: &std::path::Path, s: &Settings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)? }
    std::fs::write(path, serde_json::to_vec_pretty(s)?)
}

/// Extra mpv options for `Mpv::play`; set after its own, so these win.
pub fn mpv_options(s: &Settings) -> Vec<(&'static str, String)> {
    let mut o = vec![("hwdec", if s.hwdec { "auto-safe" } else { "no" }.to_string()),
        ("sub-scale", format!("{}", s.sub_size as f32 / 100.0)), ("sub-color", s.sub_color.clone()),
        ("cursor-autohide", s.hide_ms.to_string())];
    let tags = |code: &str| LANGS.iter().find(|l| l.0 == code).map_or(code.to_string(), |l| l.1.to_string());
    if !s.audio_lang.is_empty() { o.push(("alang", tags(&s.audio_lang))) }
    if !s.sub_lang.is_empty() { o.push(("slang", tags(&s.sub_lang))) }
    match s.sub_style.as_str() {
        "box" => o.extend([("sub-border-style", "background-box".into()), ("sub-back-color", "#B0000000".into())]),
        "none" => o.extend([("sub-border-style", "outline-and-shadow".into()), ("sub-outline-size", "0".into()), ("sub-shadow-offset", "0".into())]),
        _ => o.extend([("sub-border-style", "outline-and-shadow".into()), ("sub-outline-color", "#000000".into())]),
    }
    o
}

/// Change a setting and keep it.
fn set(s: &mut State, f: impl FnOnce(&mut Settings)) {
    f(&mut s.settings);
    if !cfg!(test) && let Err(e) = save_to(&file(), &s.settings) { s.status = format!("Couldn't save settings ({e})") }
}

// ---- streaming server ----

/// Read the server's settings (`options` + `values`) into `s.server`.
pub fn fetch_server(s: &mut State) {
    spawn(s, || Ok(Msg::Server(get(&format!("{SERVER}/settings")).map_err(|_| "not running".to_string()))));
}

/// What stremio-core sends to change server settings: its fields from `values`, then `patch`.
pub fn server_body(values: &Value, patch: &Value) -> Value {
    let mut body: serde_json::Map<String, Value> = SERVER_FIELDS.iter().map(|k| (k.to_string(), values[*k].clone())).collect();
    for (k, v) in patch.as_object().into_iter().flatten() { body.insert(k.clone(), v.clone()); }
    Value::Object(body)
}

fn set_server(s: &mut State, patch: Value) {
    let Some(Ok(cur)) = &mut s.server else { return };
    let body = server_body(&cur["values"], &patch);
    for (k, v) in patch.as_object().into_iter().flatten() { cur["values"][k] = v.clone() } // shown at once
    spawn(s, move || {
        ureq::post(format!("{SERVER}/settings")).send_json(body).map_err(|e| format!("Streaming server: {e}"))?;
        Ok(Msg::Server(get(&format!("{SERVER}/settings")).map_err(|_| "not running".to_string())))
    });
}

// ---- view ----

fn card(title: &str, body: El) -> El {
    col().w_full().gap(12.0)
        .child(label(title.to_string(), 13.0, 600.0, ink(0.5)).px(4.0))
        .child(glass(body.w_full().padding(8.0).gap(2.0).radius(18.0)))
}

/// A setting: its name on the left, the control on the right.
fn line(name: &str, control: El) -> El {
    row().w_full().min_h(56.0).px(14.0).py(8.0).gap(20.0).center_y()
        .child(label(name.to_string(), 16.0, 500.0, Color::WHITE).grow(1.0).min_w(0.0))
        .child(control)
}

/// Chips for a short list, `on` highlighted; picking calls `pick` with the index.
fn chips(key: &str, names: &[String], on: Option<usize>, pick: fn(&mut State, usize)) -> El {
    let mut r = row().gap(6.0).flex_wrap().justify_end().max_w(640.0);
    for (i, n) in names.iter().enumerate() {
        r = r.child(chip(format!("{key}-{i}"), n.clone(), on == Some(i)).on_press(move |s: &mut State| pick(s, i)));
    }
    r
}

/// A long list: the current value; a press unfolds the choices under the line.
fn picker(s: &State, key: &'static str, name: &str, names: &[String], on: Option<usize>, pick: fn(&mut State, usize)) -> El {
    let open = s.choice == Some(key);
    let current = on.map_or("—".to_string(), |i| names[i].clone());
    let head = line(name, tap(row().key(key).h(36.0).px(16.0).gap(8.0).radius(18.0).center_y()
        .fill(if open { ink(0.16) } else { ink(0.08) }).hover(ink(0.14))
        .child(label(current, 15.0, 600.0, Color::WHITE)).child(label(if open { "▴" } else { "▾" }, 12.0, 500.0, ink(0.6))))
        .on_press(move |s: &mut State| s.choice = if s.choice == Some(key) { None } else { Some(key) }));
    if !open { return head }
    let mut grid = row().w_full().gap(6.0).flex_wrap().px(14.0).pb(12.0);
    for (i, n) in names.iter().enumerate() {
        grid = grid.child(chip(format!("{key}-{i}"), n.clone(), on == Some(i))
            .on_press(move |s: &mut State| { pick(s, i); s.choice = None }));
    }
    col().w_full().child(head).child(grid)
}

fn toggle(key: &str, on: bool) -> El {
    tap(row().key(key.to_string()).size(48.0, 28.0).radius(14.0).padding(3.0).center_y()
        .fill(if on { oklch(0.72, 0.15, 150.0) } else { ink(0.18) })
        .child(el().w(if on { 20.0 } else { 0.0 }).motion(motion::SNAPPY))
        .child(el().size(22.0, 22.0).radius(11.0).fill(Color::WHITE)))
}

fn secs(ms: u32) -> String { format!("{} s", ms as f32 / 1000.0) }

fn langs(none: &str) -> Vec<String> {
    std::iter::once(none.to_string()).chain(LANGS.iter().map(|l| language(l.0).to_string())).collect()
}
fn lang_at(code: &str) -> Option<usize> {
    if code.is_empty() { Some(0) } else { LANGS.iter().position(|l| l.0 == code).map(|i| i + 1) }
}
fn lang_code(i: usize) -> String { if i == 0 { String::new() } else { LANGS[i - 1].0.to_string() } }

fn account(s: &State) -> El {
    let who = if s.user.is_empty() { "Stremio account".to_string() } else { s.user.clone() };
    card("ACCOUNT", col().child(line(&who, tap(row().key("settings-sign-out").h(36.0).px(16.0).radius(18.0).center()
        .fill(ink(0.08)).hover(ink(0.14)).child(label("Sign out", 15.0, 600.0, oklch(0.72, 0.15, 25.0))))
        .on_press(sign_out))))
}

fn player_card(s: &State) -> El {
    let p = &s.settings;
    let sizes: Vec<String> = SUB_SIZES.iter().map(|n| format!("{n}%")).collect();
    let colors: Vec<String> = COLORS.iter().map(|c| c.0.to_string()).collect();
    let styles: Vec<String> = STYLES.iter().map(|c| c.1.to_string()).collect();
    let steps: Vec<String> = SEEK_STEPS.iter().map(|&n| secs(n)).collect();
    let delays: Vec<String> = HIDE_DELAYS.iter().map(|&n| secs(n)).collect();
    card("PLAYER", col()
        .child(picker(s, "audio-lang", "Default audio language", &langs("File default"), lang_at(&p.audio_lang),
            |s, i| set(s, |p| p.audio_lang = lang_code(i))))
        .child(picker(s, "sub-lang", "Default subtitle language", &langs("None"), lang_at(&p.sub_lang),
            |s, i| set(s, |p| p.sub_lang = lang_code(i))))
        .child(line("Subtitle size", chips("sub-size", &sizes, SUB_SIZES.iter().position(|&n| n == p.sub_size),
            |s, i| set(s, |p| p.sub_size = SUB_SIZES[i]))))
        .child(line("Subtitle color", chips("sub-color", &colors, COLORS.iter().position(|c| c.1 == p.sub_color),
            |s, i| set(s, |p| p.sub_color = COLORS[i].1.into()))))
        .child(line("Subtitle background", chips("sub-style", &styles, STYLES.iter().position(|c| c.0 == p.sub_style),
            |s, i| set(s, |p| p.sub_style = STYLES[i].0.into()))))
        .child(line("Seek with arrow keys", chips("seek", &steps, SEEK_STEPS.iter().position(|&n| n == p.seek_ms),
            |s, i| set(s, |p| p.seek_ms = SEEK_STEPS[i]))))
        .child(line("Hide controls after", chips("hide", &delays, HIDE_DELAYS.iter().position(|&n| n == p.hide_ms),
            |s, i| set(s, |p| p.hide_ms = HIDE_DELAYS[i]))))
        .child(line("Hardware decoding", toggle("hwdec", p.hwdec).on_press(|s: &mut State| set(s, |p| p.hwdec = !p.hwdec)))))
}

fn quiet(words: &str) -> El { label(words.to_string(), 15.0, 500.0, ink(0.55)) }

fn server_card(s: &State) -> El {
    let v = match &s.server {
        None => return card("STREAMING SERVER", col().child(line("Streaming server", quiet("Checking…")))),
        Some(Err(_)) => return card("STREAMING SERVER", col().child(line("Streaming server", quiet("Not running")))
            .child(label("Torrents need Stremio's streaming server. Start the Stremio app, then come back.", 14.0, 400.0, ink(0.5)).px(14.0).pb(12.0))),
        Some(Ok(v)) => &v["values"],
    };
    let cache: Vec<String> = cache_sizes().iter().map(|c| c.1.to_string()).collect();
    let cache_on = cache_sizes().iter().position(|c| c.0 == v["cacheSize"]);
    let current: Vec<Value> = PROFILE_KEYS.iter().map(|k| v[*k].clone()).collect();
    let mut prof: Vec<String> = profiles().iter().map(|p| p.0.to_string()).collect();
    let prof_on = profiles().iter().position(|p| p.1.as_array().is_some_and(|a| a.iter().zip(&current).all(|(a, b)| a.as_f64() == b.as_f64())));
    if prof_on.is_none() { prof.push("Custom".into()) }
    let prof_on = prof_on.or(Some(prof.len() - 1));
    let trans: Vec<String> = std::iter::once("Disabled".to_string())
        .chain(v["allTranscodeProfiles"].as_array().into_iter().flatten().map(|p| s_(p).to_string())).collect();
    let trans_on = if v["transcodeProfile"].is_null() { Some(0) } else { trans.iter().position(|t| t == s_(&v["transcodeProfile"])) };
    let https: Vec<(String, Value)> = s.server.as_ref().and_then(|r| r.as_ref().ok())
        .and_then(|r| r["options"].as_array()?.iter().find(|o| o["id"] == "remoteHttps")?["selections"].as_array().cloned())
        .unwrap_or_default().into_iter().map(|o| (s_(&o["name"]).to_string(), o["val"].clone())).collect();
    let https_names: Vec<String> = https.iter().map(|h| h.0.clone()).collect();
    let https_on = https.iter().position(|h| h.1 == v["remoteHttps"]);
    let mut body = col()
        .child(line("Cache size", chips("cache", &cache, cache_on, |s, i| set_server(s, json!({ "cacheSize": cache_sizes()[i].0 })))))
        .child(line("Torrent profile", chips("torrent", &prof, prof_on, |s, i| {
            let Some(p) = profiles().get(i).map(|p| p.1.clone()) else { return }; // "Custom" stays as is
            set_server(s, Value::Object(PROFILE_KEYS.iter().zip(p.as_array().unwrap()).map(|(k, v)| (k.to_string(), v.clone())).collect()))
        })))
        .child(line("Transcoding", chips("transcode", &trans, trans_on, |s, i| {
            let name = server_list(s, "allTranscodeProfiles").get(i.wrapping_sub(1)).cloned().unwrap_or(Value::Null);
            set_server(s, json!({ "transcodeProfile": name }))
        })));
    if !https.is_empty() {
        body = body.child(picker(s, "https", "Remote HTTPS connections", &https_names, https_on, |s, i| {
            let val = server_https(s).get(i).cloned().unwrap_or_default();
            set_server(s, json!({ "remoteHttps": val }))
        }));
    }
    card("STREAMING SERVER", body)
}

fn server_list(s: &State, k: &str) -> Vec<Value> {
    s.server.as_ref().and_then(|r| r.as_ref().ok()).and_then(|v| v["values"][k].as_array().cloned()).unwrap_or_default()
}
fn server_https(s: &State) -> Vec<Value> {
    s.server.as_ref().and_then(|r| r.as_ref().ok())
        .and_then(|v| Some(v["options"].as_array()?.iter().find(|o| o["id"] == "remoteHttps")?["selections"].as_array()?.iter().map(|o| o["val"].clone()).collect()))
        .unwrap_or_default()
}

fn about(s: &State) -> El {
    let server = match &s.server { Some(Ok(v)) => s_(&v["values"]["serverVersion"]).to_string(), Some(Err(_)) => "Not running".into(), None => "…".into() };
    card("ABOUT", col()
        .child(line("App version", quiet(env!("CARGO_PKG_VERSION"))))
        .child(line("Streaming server version", quiet(&server))))
}

pub fn open(s: &mut State) {
    s.menu = false;
    s.choice = None;
    s.page = Page::Settings;
    fetch_server(s);
}

pub fn view(s: &State) -> El {
    col().w_full().pt(104.0).pb(56.0).items_center().child(
        col().w_full().max_w(880.0).px(PAD).gap(32.0)
            .child(label("Settings", 40.0, 700.0, Color::WHITE).font(TextStyle::new(40.0, 700.0).tracking(-0.8)))
            .child(account(s)).child(player_card(s)).child(server_card(s)).child(about(s)))
}
