//! Add-ons: the account's installed addons (uninstall, configure), community addons to install,
//! and install from a manifest URL. A child of `ui` (see the `#[path]` there).
//!
//! The collection is the user's real account, shared with the official apps: every write reads
//! a fresh `addonCollectionGet`, applies one change to it (`apply`, pure and tested) and writes
//! the whole list back, untouched entries as they came.

use super::*;

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Tab { #[default] Installed, Discover }

#[derive(Default)]
pub struct Store {
    pub tab: Tab,
    /// Community addons (`{transportUrl, transportName, manifest}`), from Cinemeta's addon catalog.
    pub discover: Vec<Value>,
    pub url: TextEdit,
    /// The addon at the pasted URL, fetched, ready to install.
    pub preview: Option<Value>,
    /// A write is in flight; others wait for it.
    pub busy: bool,
    /// The installed addon (transport URL) whose Uninstall was pressed once.
    pub confirm: Option<String>,
}

pub enum Change { Install(Value), Uninstall(String) }

/// Official and protected addons stay (Cinemeta, Local Files, OpenSubtitles…).
pub fn removable(a: &Value) -> bool { a["flags"]["protected"] != true && a["flags"]["official"] != true }

/// The list with one change applied, or why not. Never empty; untouched entries are cloned
/// as they are, in order.
pub fn apply(list: &[Value], change: &Change) -> Result<Vec<Value>, String> {
    if list.is_empty() { return Err("Your addon list came back empty; nothing changed".into()) }
    let at = |url: &Value| list.iter().enumerate().filter(|(_, a)| a["transportUrl"] == *url).map(|(i, _)| i).collect::<Vec<_>>();
    let mut out = list.to_vec();
    match change {
        Change::Install(d) => {
            if !d["transportUrl"].is_string() || !d["manifest"]["id"].is_string() { return Err("That isn't an addon".into()) }
            if !at(&d["transportUrl"]).is_empty() { return Err(format!("{} is already installed", s_(&d["manifest"]["name"]))) }
            out.push(d.clone());
        }
        Change::Uninstall(url) => match at(&Value::String(url.clone()))[..] {
            [i] if removable(&list[i]) => { out.remove(i); }
            [_] => return Err("Official addons can't be uninstalled".into()),
            [] => return Err("That addon isn't installed any more".into()),
            _ => return Err("That addon is listed twice; leaving it alone".into()),
        },
    }
    Ok(out)
}

/// What installing a fetched manifest adds to the collection.
pub fn descriptor(url: &str, manifest: Value) -> Value {
    json!({ "transportUrl": url, "transportName": "", "manifest": manifest, "flags": { "official": false, "protected": false } })
}

/// Read fresh, apply `change`, write the whole list; then reload addons and catalogs.
fn write(s: &mut State, change: Change, done: String) {
    if s.store.busy { return }
    let Some(key) = s.auth.clone() else { s.status = "Sign in to change your addons".into(); return };
    if cfg!(test) { s.status = format!("would write: {done}"); return } // tests never touch the account
    s.store.busy = true;
    s.status = "Saving your addons…".into();
    spawn(s, move || {
        let r = (|| {
            let fresh = api("addonCollectionGet", json!({ "type": "AddonCollectionGet", "authKey": key, "update": true }))
                .map_err(|e| format!("Couldn't read your addons, nothing changed ({e})"))?;
            let list = fresh["addons"].as_array().ok_or("Couldn't read your addons; nothing changed")?;
            let next = apply(list, &change)?;
            api("addonCollectionSet", json!({ "type": "AddonCollectionSet", "authKey": key, "addons": next }))?;
            Ok(done)
        })();
        Ok(Msg::Written(r))
    });
}

pub fn written(s: &mut State, r: Result<String, String>) {
    s.store.busy = false;
    s.store.confirm = None;
    match r {
        Ok(done) => { load_account(s); s.store.preview = None; s.store.url.clear(); s.status = done }
        Err(e) => s.status = e,
    }
}

/// Load community addons the first time the page opens.
pub fn open(s: &mut State) {
    s.store.confirm = None;
    if !s.store.discover.is_empty() { return }
    // Cinemeta serves Stremio's addon catalogs; any installed addon offering the community one will do.
    let base = s.addons.iter().find(|a| a["manifest"]["addonCatalogs"].as_array().into_iter().flatten()
        .any(|c| c["type"] == "all" && c["id"] == "community")).map(base).unwrap_or("https://v3-cinemeta.strem.io".into());
    spawn(s, move || Ok(Msg::Discover(get(&format!("{base}/addon_catalog/all/community.json"))?["addons"].as_array().cloned().unwrap_or_default())));
}

/// Fetch the pasted manifest for a preview.
fn fetch(s: &mut State, _: &mut Tx) {
    let mut url = s.store.url.text.trim().to_string();
    if let Some(rest) = url.strip_prefix("stremio://") { url = format!("https://{rest}") }
    if !url.starts_with("http://") && !url.starts_with("https://") { s.status = "Paste an addon's manifest URL (https://…/manifest.json)".into(); return }
    s.store.preview = None;
    s.status = "Fetching addon…".into();
    spawn(s, move || {
        let m = get(&url).map_err(|_| "Couldn't load an addon from that URL".to_string())?;
        if !["id", "name", "version"].iter().all(|k| m[*k].is_string()) { return Err("That URL isn't an addon manifest".into()) }
        Ok(Msg::Preview(descriptor(&url, m)))
    });
}

/// Open an addon's settings page in the browser.
fn configure(url: String) {
    if !url.starts_with("http") { return }
    #[cfg(windows)]
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", &url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
}

// ---- view ----

fn button(key: String, words: &str, strong: bool) -> El {
    tap(row().key(key).h(34.0).px(14.0).radius(17.0).center()
        .fill(if strong { Color::WHITE } else { ink(0.08) }).hover(if strong { ink(0.85) } else { ink(0.14) })
        .child(label(words.to_string(), 14.0, 600.0, if strong { Color::BLACK } else { ink(0.9) })))
}

/// Logo, name and version, description, types; `actions` on the right.
fn addon_card(s: &State, a: &Value, key: String, actions: El) -> El {
    let m = &a["manifest"];
    let logo = m["logo"].as_str().filter(|u| u.starts_with("http")).map(|u| sized(u, 56.0));
    let types: Vec<&str> = m["types"].as_array().into_iter().flatten().map(|t| type_label(s_(t))).collect();
    glass_flat(row().key(key).w_full().padding(16.0).gap(16.0).radius(18.0))
        .child(picture(s, logo.as_deref()).size(56.0, 56.0).radius(12.0).clip_rounded(12.0))
        .child(col().grow(1.0).min_w(0.0).gap(6.0)
            .child(row().gap(8.0).center_y()
                .child(label(s_(&m["name"]).to_string(), 17.0, 600.0, Color::WHITE).truncate().shrink().min_w(0.0))
                .child(label(format!("v{}", s_(&m["version"])), 13.0, 500.0, ink(0.45))))
            .child(label(s_(&m["description"]).to_string(), 14.0, 400.0, ink(0.62)).font(TextStyle::new(14.0, 400.0).lh(20.0)).wrap().max_lines(2))
            .child(label(types.join(" · "), 13.0, 500.0, ink(0.45)).truncate()))
        .child(actions.self_start())
}

fn grid(cards: Vec<El>) -> El {
    let mut g = row().w_full().equal_columns(2).gap(16.0);
    for c in cards { g = g.child(c) }
    g
}

fn installed(s: &State) -> El {
    grid(s.addons.iter().enumerate().map(|(i, a)| {
        let url = s_(&a["transportUrl"]).to_string();
        let mut actions = col().gap(8.0).items_end();
        if a["manifest"]["behaviorHints"]["configurable"] == true {
            let page = format!("{}/configure", base(a));
            actions = actions.child(button(format!("ad-config-{i}"), "Configure", false).on_press(move |_: &mut State| configure(page.clone())));
        }
        if !removable(a) {
            actions = actions.child(label("Official", 13.0, 600.0, ink(0.45)).py(8.0));
        } else if s.store.confirm.as_ref() == Some(&url) {
            let name = s_(&a["manifest"]["name"]).to_string();
            actions = actions.child(tap(row().key(format!("ad-uninstall-{i}")).h(34.0).px(14.0).radius(17.0).center()
                .fill(oklch(0.6, 0.18, 25.0)).hover(oklch(0.66, 0.18, 25.0)).child(label("Confirm", 14.0, 600.0, Color::WHITE)))
                .on_press(move |s: &mut State| write(s, Change::Uninstall(url.clone()), format!("Uninstalled {name}"))));
        } else {
            actions = actions.child(button(format!("ad-uninstall-{i}"), "Uninstall", false)
                .on_press(move |s: &mut State| s.store.confirm = Some(url.clone())));
        }
        addon_card(s, a, format!("ad-{i}"), actions)
    }).collect())
}

fn is_installed(s: &State, d: &Value) -> bool { s.addons.iter().any(|a| a["transportUrl"] == d["transportUrl"]) }

fn install_button(s: &State, d: &Value, key: String) -> El {
    if is_installed(s, d) { return label("Installed", 13.0, 600.0, ink(0.45)).py(8.0) }
    let (d, name) = (d.clone(), s_(&d["manifest"]["name"]).to_string());
    let d = if d["flags"].is_object() { d } else { descriptor(s_(&d["transportUrl"]), d["manifest"].clone()) };
    button(key, "Install", true).on_press(move |s: &mut State| write(s, Change::Install(d.clone()), format!("Installed {name}")))
}

fn discover(s: &State) -> El {
    if s.store.discover.is_empty() { return label("Loading community addons…", 15.0, 500.0, ink(0.5)) }
    grid(s.store.discover.iter().enumerate()
        .map(|(i, d)| addon_card(s, d, format!("dc-{i}"), install_button(s, d, format!("dc-install-{i}")))).collect())
}

fn from_url(s: &State) -> El {
    let field = glass_flat(text_input(&s.store.url, TextStyle::new(16.0, 400.0)).key("addon-url").single_line()
        .grow(1.0).min_w(0.0).px(16.0).py(12.0).radius(12.0)).color(Color::WHITE)
        .placeholder("Install from URL: paste an addon's manifest.json link".to_string(), ink(0.4))
        .on_edit(|s: &mut State| &mut s.store.url).on_submit(fetch);
    let mut c = col().w_full().gap(12.0)
        .child(row().w_full().gap(10.0).center_y().child(field).child(button("addon-fetch".into(), "Preview", false).on_press_tx(fetch)));
    if let Some(d) = &s.store.preview {
        let actions = row().gap(8.0).child(install_button(s, d, "preview-install".into()))
            .child(button("preview-cancel".into(), "Cancel", false).on_press(|s: &mut State| s.store.preview = None));
        c = c.child(addon_card(s, d, "preview".into(), actions));
    }
    c
}

pub fn view(s: &State) -> El {
    let tab = |key: &str, words: String, t: Tab| chip(key.to_string(), words, s.store.tab == t).on_press(move |s: &mut State| s.store.tab = t);
    col().w_full().pt(104.0).px(PAD).pb(56.0).gap(24.0)
        .child(label("Add-ons", 40.0, 700.0, Color::WHITE).font(TextStyle::new(40.0, 700.0).tracking(-0.8)))
        .child(from_url(s))
        .child(row().gap(10.0)
            .child(tab("tab-installed", format!("Installed · {}", s.addons.len()), Tab::Installed))
            .child(tab("tab-discover", "Discover".into(), Tab::Discover)))
        .child(match s.store.tab { Tab::Installed => installed(s), Tab::Discover => discover(s) })
}
