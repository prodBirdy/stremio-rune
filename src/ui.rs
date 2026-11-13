//! Views: a dark, cinematic layout with a floating glass nav, a full-bleed hero, and rows of
//! landscape cards. Playback controls float over the video in a window of their own.

use crate::*;
use rune::tok::{TextStyle, motion};
use rune::{Align, Color, Cursor, Cx, Cycle, Drag, El, GrowthSpec, Phase, ScrollSpec, Side, SurfaceFit, Transition, col, el, oklch, row, text, text_input};
use std::f32::consts::PI;

#[path = "search.rs"]
pub mod search;
#[path = "controls.rs"]
mod controls;
pub use controls::language;
#[cfg(test)]
pub use controls::{VOL_W, qualities, resolution};
use controls::player;
#[path = "settings.rs"]
pub mod settings;
#[path = "addons.rs"]
pub mod addons;

const PAD: f32 = 56.0;
const HERO_H: f32 = 560.0;
/// Space between cards.
const GAP: f32 = 22.0;
/// People in a detail page's Cast & Crew row.
const PEOPLE: usize = 8;
/// A synopsis longer than this (in bytes) is cut off in the hero, so Details shows it whole.
const LONG_SYNOPSIS: usize = 200;

fn bg() -> Color { oklch(0.15, 0.012, 265.0) }
fn ink(a: f32) -> Color { Color::WHITE.alpha(a) }

fn label(s: impl Into<String>, size: f32, weight: f32, color: Color) -> El {
    text(s).font(TextStyle::new(size, weight)).color(color)
}

/// Frosted panel: a faint white wash and hairline over whatever is behind it.
fn glass(e: El) -> El {
    e.fill(ink(0.07)).border(1.0, ink(0.12)).backdrop_blur(24.0)
}

/// `glass`'s look without its blur, for what always sits on the page's flat background, where
/// blurring shows the same colour: it costs no blur passes, and leaves Rune's few blurs a frame
/// (four) to glass over content, such as the nav.
fn glass_flat(e: El) -> El {
    e.fill(ink(0.07)).border(1.0, ink(0.12))
}

fn tap(e: El) -> El { e.cursor(Cursor::Pointer).press_scale(0.97) }

fn all(e: El) -> El { e.pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)) }

/// An image by key (see `sized`), cover-filled, or a quiet placeholder while it loads. It asks
/// for its image when laid out within a screen of the viewport, and each move there (a scroll)
/// marks it used, so the cache keeps what's on screen.
fn picture(s: &State, url: Option<&str>) -> El {
    let mut e = el().fill(ink(0.05));
    let Some(u) = url else { return e };
    if let Some(img) = s.images.get(u) { e = e.pixels_fit(img.pixels(), SurfaceFit::Fill) }
    let key = u.to_string();
    e.on_layout_tx(move |s: &mut State, [_, y, _, h], tx| {
        tx.live_only();
        if y < s.view_h * 2.0 && y + h > -s.view_h { want(s, [key.clone()]) }
    })
}

/// Image keys for where they show, at the widest the window gets (the screen's width).
pub fn hero_img(url: Option<String>) -> Option<String> { url.map(|u| sized(&u, window::screen_w())) }
pub fn card_img(url: Option<String>) -> Option<String> {
    url.map(|u| sized(&u, (window::screen_w() - 2.0 * PAD - (ROW_CARDS - 1) as f32 * GAP) / ROW_CARDS as f32))
}
pub fn poster_img(url: Option<&str>) -> Option<String> {
    url.map(|u| sized(u, (window::screen_w() - 2.0 * PAD - 6.0 * GAP) / 7.0))
}

fn icon(s: &State, pick: impl Fn(&Icons) -> &Image, size: f32) -> El {
    let e = el().size(size, size);
    match &s.icons { Some(i) => e.pixels(pick(i).pixels()), None => e }
}

/// A thin bar; the caller sets its width (`w_full`, or pinned edges).
fn progress(frac: f32, h: f32) -> El {
    row().h(h).radius(h / 2.0).fill(ink(0.28))
        .child(el().h(h).radius(h / 2.0).fill(Color::WHITE).w_frac(frac.clamp(0.0, 1.0)))
}

fn badge(s: impl Into<String>) -> El {
    row().h(24.0).px(7.0).radius(5.0).center().border(1.0, ink(0.45)).child(label(s, 13.0, 600.0, ink(0.9)))
}

pub fn s_(v: &Value) -> &str { v.as_str().unwrap_or("") }

/// `Drama · Mystery · 2026 · 107 min`
fn meta_line(m: &Value) -> String {
    let genres = m["genres"].as_array().or(m["genre"].as_array()).into_iter().flatten().take(2).map(s_);
    genres.chain([s_(&m["releaseInfo"]), s_(&m["runtime"])]).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// `Jan 28, 2008` from an ISO date; empty when it isn't one.
pub fn date(iso: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut p = iso.get(..10).unwrap_or("").split('-').map(|x| x.parse::<usize>().ok());
    match (p.next().flatten(), p.next().flatten(), p.next().flatten()) {
        (Some(y), Some(m @ 1..=12), Some(d)) => format!("{} {d}, {y}", MONTHS[m - 1]),
        _ => String::new(),
    }
}

pub fn seasons(m: &Value) -> Vec<i64> {
    let mut v: Vec<i64> = m["videos"].as_array().into_iter().flatten().filter_map(|v| v["season"].as_i64()).collect();
    v.sort();
    v.dedup();
    v
}

/// `1:02:03`, or `4:05` under an hour.
pub fn clock(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

// ---- chrome ----

fn nav(s: &State) -> El {
    let item = |name: &str, page: Page| {
        let on = s.page == page;
        tap(row().key(format!("nav-{name}")).h(38.0).px(18.0).radius(19.0).center()
            .fill(if on { Color::WHITE } else { Color::TRANSPARENT })
            .hover(if on { Color::WHITE } else { ink(0.1) })
            .child(label(name, 16.0, if on { 600.0 } else { 500.0 }, if on { Color::BLACK } else { ink(0.85) })))
            .on_press(move |s: &mut State| go(s, page))
    };
    let initial = s.user.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    let avatar = tap(row().key("avatar").size(38.0, 38.0).radius(19.0).center()
        .linear_gradient(PI * 0.75, [(0.0, ink(0.35)), (1.0, ink(0.12))])
        .child(label(initial, 16.0, 600.0, Color::WHITE)))
        .on_press(|s: &mut State| s.menu = !s.menu);
    let on = s.page == Page::Search;
    let find = tap(row().key("nav-search").size(38.0, 38.0).radius(19.0).center()
        .fill(if on { ink(0.18) } else { Color::TRANSPARENT }).hover(ink(0.1))
        .child(icon(s, |i| &i.search, 18.0)))
        .on_press(search::open);
    let mut pill = glass(row().key("nav").h(50.0).px(6.0).gap(4.0).radius(25.0).center_y())
        .shadow(10.0, 30.0, Color::BLACK.alpha(0.35))
        .child(find).child(item("Home", Page::Home)).child(item("Movies", Page::Browse("movie"))).child(item("Shows", Page::Browse("series"))).child(item("Add-ons", Page::Addons))
        .child(el().w(4.0));
    // A newer release: installs in place and restarts (`update_now`).
    if let Some(u) = &s.update {
        let version = u.version.split('-').next().unwrap_or(&u.version);
        pill = pill.child(tap(row().key("nav-update").h(38.0).pl(14.0).pr(16.0).gap(8.0).radius(19.0).center().fill(ink(0.1)).hover(ink(0.16))
            .child(el().size(8.0, 8.0).radius(4.0).fill(oklch(0.78, 0.17, 150.0)))
            .child(label(format!("Update to {version}"), 15.0, 600.0, Color::WHITE)))
            .on_press_tx(update_now));
    }
    // An update on its way: how far, in place of the offer.
    if let Some((r, p)) = &s.updating {
        let version = r.version.split('-').next().unwrap_or(&r.version);
        let (words, frac) = match p {
            update::Progress::Downloading(Some(f)) => (format!("Updating to {version} · {}%", (f * 100.0) as u32), *f),
            update::Progress::Downloading(None) => (format!("Updating to {version}…"), 0.0),
            update::Progress::Installing => (format!("Installing {version}…"), 1.0),
        };
        pill = pill.child(row().key("nav-updating").h(38.0).pl(14.0).pr(16.0).gap(10.0).radius(19.0).center().fill(ink(0.1))
            .child(label(words, 15.0, 600.0, Color::WHITE).font(TextStyle::new(15.0, 600.0).features("'tnum' 1")))
            .child(progress(frac, 4.0).w(56.0)));
    }
    let pill = pill.child(avatar);
    // The menu hangs off a plain wrapper so the pill's blur layer doesn't clip it.
    let mut anchor = el().child(pill);
    if s.menu { anchor = anchor.child(menu(s).pin(None, Some(60.0), Some(0.0), None)) }
    row().pin(Some(0.0), Some(18.0 + top(s)), Some(0.0), None).justify_center().child(anchor)
}

fn menu(s: &State) -> El {
    let who = if s.user.is_empty() { "Stremio account".to_string() } else { s.user.clone() };
    glass(col().key("menu").w(260.0).padding(8.0).gap(2.0).radius(16.0)).fill(bg().alpha(0.94))
        .shadow(12.0, 32.0, Color::BLACK.alpha(0.45))
        .transition(Transition::offset(0.0, -6.0).combined(Transition::opacity()))
        .child(col().px(12.0).py(10.0).gap(3.0)
            .child(label("Signed in as", 12.0, 500.0, ink(0.5)))
            .child(label(who, 14.0, 600.0, Color::WHITE).truncate()))
        .child(el().h(1.0).w_full().fill(ink(0.1)))
        .child(tap(row().key("settings").h(40.0).px(12.0).mt(4.0).radius(10.0).center_y().hover(ink(0.08))
            .child(label("Settings", 15.0, 500.0, Color::WHITE)))
            .on_press(settings::open))
        .child(tap(row().key("sign-out").h(40.0).px(12.0).radius(10.0).center_y().hover(ink(0.08))
            .child(label("Sign out", 15.0, 500.0, oklch(0.72, 0.15, 25.0))))
            .on_press(sign_out))
}

/// The streams for the picked video, sliding in from the right over a dimmed page.
fn sheet(s: &State) -> [El; 2] {
    let close = |s: &mut State| s.sheet = false;
    let scrim = all(el()).key("sheet-scrim").fill(Color::BLACK.alpha(0.5)).on_press(close);
    let m = s.meta.as_ref();
    let episode = episode_of(&s.stream_for).and_then(|(se, ep)| {
        let v = m?["videos"].as_array()?.iter().find(|v| s_(&v["id"]) == s.stream_for)?;
        Some(format!("S{se}, E{ep} · {}", v["title"].as_str().or(v["name"].as_str()).unwrap_or("")))
    });
    let subtitle = episode.unwrap_or_else(|| m.map(|m| s_(&m["name"]).to_string()).unwrap_or_default());
    let head = row().w_full().gap(12.0).center_y()
        .child(col().grow(1.0).min_w(0.0).gap(4.0)
            .child(label("Streams", 26.0, 700.0, Color::WHITE))
            .child(label(subtitle, 14.0, 500.0, ink(0.6)).truncate()))
        .child(tap(glass(row().key("sheet-close").size(40.0, 40.0).radius(20.0).center()).hover(ink(0.16))
            .child(icon(s, |i| &i.close, 18.0))).on_press(close));
    // Virtual rows: only the cards near the viewport are built, shaped and laid out, so a
    // hundred streams open (and each addon's answer lands) as fast as a handful.
    let list = if s.streams.is_empty() {
        col().w_full().child(label("Finding streams…", 15.0, 500.0, ink(0.5)))
    } else {
        col().key("streams").w_full().growth(GrowthSpec::default()).virtual_rows(s.streams.len(), 128.0,
            |_: &State, i| format!("st-{i}").into(),
            |s: &State, i| {
                let st = &s.streams[i];
                let url = st.url.clone();
                el().w_full().pb(10.0).child(tap(col().w_full().px(16.0).py(14.0).gap(6.0).radius(14.0)
                    .fill(ink(0.06)).border(1.0, ink(0.08)).hover(ink(0.12))
                    .child(row().gap(10.0).center_y().child(icon(s, |i| &i.play, 14.0)).child(label(st.name.clone(), 15.0, 600.0, Color::WHITE).truncate()))
                    .child(label(st.title.clone(), 13.0, 400.0, ink(0.62)).font(TextStyle::new(13.0, 400.0).lh(19.0)).wrap().max_lines(4)))
                    .on_press(move |s: &mut State| play(s, &url)))
            })
    };
    let panel = col().key("sheet").w(500.0).pin(None, Some(0.0), Some(0.0), Some(0.0)).pt(28.0 + top(s)).px(24.0).gap(20.0)
        .fill(bg().alpha(0.9)).backdrop_blur(30.0).border(1.0, ink(0.08)).shadow(0.0, 40.0, Color::BLACK.alpha(0.5))
        .transition(Transition::offset(500.0, 0.0).combined(Transition::opacity()))
        .child(head)
        .child(col().w_full().pb(14.0).scroll(ScrollSpec { stick_bottom: false }).grow(1.0).shrink().min_h(0.0).child(list));
    [scrim, panel]
}

fn toast(s: &State) -> El {
    row().pin(Some(0.0), None, Some(0.0), Some(28.0)).justify_center().child(
        glass(row().key("toast").h(40.0).px(18.0).radius(20.0).center()).fill(Color::BLACK.alpha(0.55))
            .child(label(s.status.clone(), 14.0, 500.0, ink(0.9))),
    )
}

fn back_button(s: &State) -> El {
    tap(glass(row().key("back").size(44.0, 44.0).radius(22.0).center()).hover(ink(0.16))
        .child(icon(s, |i| &i.back, 20.0)))
}

fn primary(s: &State, key: &str, words: impl Into<String>) -> El {
    tap(glass(row().key(key.to_string()).h(54.0).px(26.0).gap(12.0).radius(12.0).center()).fill(ink(0.16)).hover(ink(0.24))
        .child(icon(s, |i| &i.play, 18.0)).child(label(words, 18.0, 600.0, Color::WHITE)))
}

/// Full-bleed backdrop with the scrims that keep text readable over any image, and the caption
/// buttons over its top.
fn backdrop_layer(s: &State, url: Option<&str>) -> [El; 4] {
    [
        all(picture(s, url)),
        all(el()).linear_gradient(PI / 2.0, [(0.0, Color::BLACK.alpha(0.8)), (0.6, Color::BLACK.alpha(0.0))]),
        all(el()).linear_gradient(PI, [(0.35, bg().alpha(0.0)), (1.0, bg())]),
        el().pin(Some(0.0), Some(0.0), Some(0.0), None).h(top(s) + 72.0)
            .linear_gradient(PI, [(0.0, Color::BLACK.alpha(0.45)), (1.0, Color::BLACK.alpha(0.0))]),
    ]
}

/// Title, metadata, synopsis: the shared head of the hero and the detail page.
fn headline(m: &Value, kicker: Option<String>) -> El {
    let mut info = col().w(640.0).gap(14.0);
    if let Some(k) = kicker {
        info = info.child(row().gap(10.0).center_y()
            .child(row().h(24.0).px(8.0).radius(5.0).center().fill(ink(0.85)).child(label(k.to_uppercase(), 12.0, 700.0, Color::BLACK))));
    }
    let mut facts = row().gap(10.0).center_y().child(label(meta_line(m), 15.0, 500.0, ink(0.85)));
    if let Some(r) = m["imdbRating"].as_str().filter(|r| !r.is_empty()) { facts = facts.child(badge(format!("IMDb {r}"))) }
    info.child(label(s_(&m["name"]).to_string(), 64.0, 700.0, Color::WHITE).font(TextStyle::new(64.0, 700.0).tracking(-1.5).lh(70.0)).wrap().max_lines(2))
        .child(facts)
        .child(label(s_(&m["description"]).to_string(), 17.0, 400.0, ink(0.72)).font(TextStyle::new(17.0, 400.0).lh(25.0)).wrap().max_lines(3).w(580.0))
}

// ---- pages ----

fn hero(s: &State) -> El {
    let picks = hero_picks(s);
    let Some((m, kicker, _)) = picks.get(s.hero.min(picks.len().saturating_sub(1))) else { return el().w_full().h(HERO_H + top(s)) };
    let (ty, id) = (s_(&m["type"]).to_string(), s_(&m["id"]).to_string());
    let info = headline(m, Some(kicker.clone())).child(el().h(8.0))
        .child(primary(s, "watch", "Watch now").self_start().on_press(move |s: &mut State| open_meta(s, ty.clone(), id.clone())));
    // The current dot stretches into a track that fills while its title stays up: a render-time
    // tween, so the fill moves without rebuilding the page.
    let mut dots = row().gap(8.0).center_y();
    for i in 0..picks.len() {
        let on = i == s.hero.min(picks.len() - 1);
        let mut dot = el().key(format!("dot-{i}")).h(8.0).w(if on { DOT_W } else { 8.0 }).radius(4.0)
            .fill(ink(0.35)).clip_rounded(4.0).motion(motion::SNAPPY);
        if on && s.focused { dot = dot.child(dot_fill(s)) }
        dots = dots.child(tap(dot).on_press(move |s: &mut State| show_hero(s, i)));
    }
    let [image, side, fade, scrim] = backdrop_layer(s, hero_img(backdrop(m)).as_deref());
    // Keyed by title so a change cross-fades: the old image fades out as the new one fades in.
    let image = image.key(format!("hero-{}", s_(&m["id"])));
    // From the window top, under the title strip; what's in it stays below the strip.
    el().key("hero").w_full().h(HERO_H + top(s))
        .child(image).child(side).child(fade).child(scrim)
        .child(info.pin(Some(PAD), None, None, Some(40.0)))
        .child(dots.pin(None, None, Some(PAD), Some(56.0)))
}

/// The current dot's fill over `HERO_EVERY`: a smooth render-time tween, keyed by round so
/// each title's fill starts empty. It draws every frame while Home is up and focused (the
/// hero pauses unfocused, so a background window draws nothing).
fn dot_fill(s: &State) -> El {
    el().key(format!("fill-{}", s.hero_round)).h(8.0).w(DOT_W).fill(Color::WHITE).live_rect(&s.hero_fill)
}

/// A 16:9 card with an optional progress bar over the image, and two lines under it.
fn landscape(s: &State, key: String, url: Option<String>, title: String, sub: String, done: Option<f32>) -> El {
    let mut image = picture(s, card_img(url).as_deref()).w_full().aspect_ratio(16.0 / 9.0).radius(14.0).clip_rounded(14.0)
        .border(1.0, ink(0.08)).shadow(10.0, 28.0, Color::BLACK.alpha(0.45));
    if let Some(f) = done { image = image.child(progress(f, 4.0).pin(Some(14.0), None, Some(14.0), Some(12.0))) }
    tap(col().key(key).gap(10.0).min_w(0.0))
        .child(image)
        .child(label(title, 17.0, 600.0, Color::WHITE).truncate())
        .child(label(sub, 14.0, 500.0, ink(0.55)).truncate())
}

/// A Home row: its title with back/on chevrons, and a page of cards that slides in from the
/// side it was turned toward while the last one fades. Shift+wheel, a sideways swipe or the
/// arrow keys over it turn it too.
fn home_row(s: &State, r: usize, title: &str, card: impl Fn(&Value) -> El) -> El {
    let name = match r { UP_NEXT => "up".to_string(), WATCHLIST => "wl".to_string(), _ => r.to_string() };
    let at = row_at(s, r);
    let enter = Transition::offset(160.0 * s.row_dir, 0.0).combined(Transition::opacity());
    let mut page = row().key(format!("page-{name}-{at}")).w_full().equal_columns(ROW_CARDS).gap(GAP)
        .transition(Transition::asymmetric(enter, Transition::opacity()));
    for m in row_items(s, r)[at..].iter().take(ROW_CARDS) { page = page.child(card(m)) }
    let arrow = |dir: i32| {
        let on = can_page(s, r, dir);
        let b = glass_flat(row().key(format!("{}-{name}", if dir < 0 { "prev" } else { "next" })).size(36.0, 36.0).radius(18.0).center())
            .opacity(if on { 1.0 } else { 0.35 })
            .child(icon(s, move |i| if dir < 0 { &i.back } else { &i.next }, 16.0));
        if on { tap(b).hover(ink(0.16)).on_press(move |s: &mut State| page_row(s, r, dir)) } else { b }
    };
    let mut head = row().w_full().h(36.0).gap(8.0).center_y()
        .child(label(title, 24.0, 600.0, Color::WHITE).truncate().grow(1.0).min_w(0.0));
    // A row that fits on one page gets no arrows.
    if can_page(s, r, -1) || can_page(s, r, 1) { head = head.child(arrow(-1)).child(arrow(1)) }
    col().key(format!("row-{name}")).w_full().px(PAD).gap(16.0).child(head).child(el().w_full().child(page))
        .on_hover_change(move |s: &mut State, inside, _| {
            if inside { s.row_hover = Some(r) } else if s.row_hover == Some(r) { s.row_hover = None }
        })
        .on_wheel(move |s: &mut State, e, _| wheel_row(s, r, e))
}

fn up_next(s: &State) -> El {
    home_row(s, UP_NEXT, "Up Next", |item| {
        let st = &item["state"];
        let (at, len) = (st["timeOffset"].as_f64().unwrap_or(0.0), st["duration"].as_f64().unwrap_or(0.0));
        let left = ((len - at) / 60_000.0).max(0.0).round();
        let mut sub = if len > 0.0 { format!("{left} min left") } else { "Continue".into() };
        if let Some((se, ep)) = episode_of(s_(&st["video_id"])) { sub = format!("S{se}, E{ep} · {sub}") }
        let done = (len > 0.0).then(|| (at / len) as f32);
        let it = item.clone();
        landscape(s, format!("cw-{}", s_(&item["_id"])), backdrop(item), s_(&item["name"]).to_string(), sub, done)
            .on_press(move |s: &mut State| resume(s, &it))
    })
}

fn watchlist(s: &State) -> El {
    home_row(s, WATCHLIST, "Watchlist", |item| {
        let (ty, id) = (s_(&item["type"]).to_string(), s_(&item["_id"]).to_string());
        let kind = if ty == "movie" { "Movie" } else { "Show" };
        landscape(s, format!("wl-{id}"), backdrop(item), s_(&item["name"]).to_string(), kind.to_string(), None)
            .on_press(move |s: &mut State| open_meta(s, ty.clone(), id.clone()))
    })
}

fn catalog_row(s: &State, i: usize) -> El {
    home_row(s, i, &s.catalogs[i].name, |m| {
        let (ty, id) = (s_(&m["type"]).to_string(), s_(&m["id"]).to_string());
        landscape(s, format!("c{i}-{id}"), backdrop(m), s_(&m["name"]).to_string(), meta_line(m), None)
            .on_press(move |s: &mut State| open_meta(s, ty.clone(), id.clone()))
    })
}

/// A catalog's Home row before its titles: the title over blank cards, which asks for the
/// catalog once it nears the viewport; or, when it failed, the title over why.
fn pending_row(s: &State, i: usize) -> El {
    let head = row().w_full().h(36.0).center_y().child(label(&s.catalogs[i].name, 24.0, 600.0, Color::WHITE).truncate().grow(1.0).min_w(0.0));
    let r = col().key(format!("row-{i}")).w_full().px(PAD).gap(16.0).child(head);
    if let Some(e) = s.catalog_err.get(&i) { return r.child(label(format!("Couldn't load this catalog: {e}"), 15.0, 500.0, ink(0.5)).truncate()) }
    let mut cards = row().w_full().equal_columns(ROW_CARDS).gap(GAP);
    for _ in 0..ROW_CARDS {
        cards = cards.child(col().gap(10.0).min_w(0.0)
            .child(el().w_full().aspect_ratio(16.0 / 9.0).radius(14.0).fill(ink(0.05)))
            .child(el().h(20.0).w_frac(0.6).radius(6.0).fill(ink(0.05)))
            .child(el().h(17.0).w_frac(0.4).radius(6.0).fill(ink(0.05))))
    }
    r.child(cards).on_layout_tx(move |s: &mut State, [_, y, _, h], tx| {
        tx.live_only();
        if y < s.view_h * 2.0 && y + h > -s.view_h { fetch_catalog(s, i) }
    })
}

/// Every catalog's row, as Stremio's board has them: addons in the account's order, catalogs in
/// their manifest's; those that need an extra stay off (`catalogs`), as do those with no titles.
fn home(s: &State) -> El {
    let mut page = col().w_full().gap(36.0).pb(56.0).child(hero(s));
    if !s.library.is_empty() { page = page.child(up_next(s)) }
    if !s.saved.is_empty() { page = page.child(watchlist(s)) }
    // The first catalog feeds the hero; its row starts after the hero's picks (`row_items`).
    for i in 0..s.catalogs.len() {
        if s.catalogs[i].needs_genre { continue }
        match s.items.get(&i) {
            Some(items) if items.is_empty() => {}
            Some(_) => page = page.child(catalog_row(s, i)),
            None => page = page.child(pending_row(s, i)),
        }
    }
    page
}

fn chip(key: String, words: String, on: bool) -> El {
    tap(row().key(key).h(36.0).px(16.0).radius(18.0).center()
        .fill(if on { Color::WHITE } else { ink(0.08) }).hover(if on { Color::WHITE } else { ink(0.14) })
        .child(label(words, 15.0, if on { 600.0 } else { 500.0 }, if on { Color::BLACK } else { ink(0.85) })))
}

/// A 2:3 poster with its title and year; opens the title's detail page.
fn poster_card(s: &State, m: &Value, key: String) -> El {
    let (t, id) = (s_(&m["type"]).to_string(), s_(&m["id"]).to_string());
    let poster = picture(s, poster_img(m["poster"].as_str()).as_deref()).w_full().aspect_ratio(2.0 / 3.0).radius(12.0).clip_rounded(12.0)
        .border(1.0, ink(0.08)).shadow(8.0, 22.0, Color::BLACK.alpha(0.4));
    tap(col().key(key).gap(10.0).min_w(0.0))
        .child(poster)
        .child(label(s_(&m["name"]).to_string(), 15.0, 600.0, Color::WHITE).truncate())
        .child(label(s_(&m["releaseInfo"]).to_string(), 13.0, 500.0, ink(0.5)))
        .on_press(move |s: &mut State| open_meta(s, t.clone(), id.clone()))
}

/// The catalogs of a type: a tab per addon, chips for the open addon's catalogs, and the open
/// catalog's posters.
fn browse(s: &State, ty: &str) -> El {
    let cats: Vec<(usize, &Catalog)> = s.catalogs.iter().enumerate().filter(|(_, c)| c.ty == ty).collect();
    let open = s.catalogs.get(s.selected).filter(|c| c.ty == ty).map(|c| c.addon.as_str());
    let mut addons: Vec<(&str, usize)> = vec![];
    for (i, c) in &cats { if !addons.iter().any(|(a, _)| *a == c.addon) { addons.push((&c.addon, *i)) } }
    let mut page = col().w_full().pt(104.0).px(PAD).pb(56.0).gap(24.0)
        .child(label(type_label(ty), 40.0, 700.0, Color::WHITE).font(TextStyle::new(40.0, 700.0).tracking(-0.8)));
    // One addon needs no tabs.
    if addons.len() > 1 {
        let mut tabs = row().w_full().gap(28.0).flex_wrap();
        for (a, first) in addons {
            let on = open == Some(a);
            tabs = tabs.child(tap(col().key(format!("source-{first}")).gap(8.0)
                .child(label(a, 16.0, if on { 600.0 } else { 500.0 }, if on { Color::WHITE } else { ink(0.5) }))
                .child(el().h(2.0).w_full().radius(1.0).fill(if on { Color::WHITE } else { Color::TRANSPARENT })))
                .on_press(move |s: &mut State| open_catalog(s, first)));
        }
        page = page.child(col().w_full().gap(0.0).child(tabs).child(el().h(1.0).w_full().fill(ink(0.08))));
    }
    let mut chips = row().grow(1.0).min_w(0.0).gap(8.0).flex_wrap();
    for (i, c) in cats.iter().filter(|(_, c)| open.is_none_or(|a| a == c.addon)) {
        let i = *i;
        chips = chips.child(chip(format!("cat-{i}"), c.title.clone(), i == s.selected).on_press(move |s: &mut State| open_catalog(s, i)));
    }
    // The genre filter sits at the chips' end, as a toolbar's would.
    let mut bar = row().w_full().gap(24.0).child(chips);
    if let Some(c) = s.catalogs.get(s.selected).filter(|c| c.ty == ty && !c.genres.is_empty()) { bar = bar.child(genre_button(s, c)) }
    page = page.child(bar);
    match browse_items(s) {
        Some(Err(e)) => page.child(label(format!("Couldn't load this catalog: {e}"), 15.0, 500.0, ink(0.5))),
        Some(Ok([])) => page.child(label("Nothing here yet.", 15.0, 500.0, ink(0.5))),
        Some(Ok(items)) => {
            let mut grid = row().w_full().equal_columns(7).gap(GAP);
            for m in items { grid = grid.child(poster_card(s, m, format!("p-{}", s_(&m["id"])))) }
            page.child(grid)
        }
        None => page,
    }
}

/// The open catalog's genre as a pop-up button: an introductory "Genre", then the current one
/// over up/down chevrons; a press opens `genre_menu` under it.
fn genre_button(s: &State, c: &Catalog) -> El {
    let chevron = |turn: f32| icon(s, |i| &i.next, 9.0).rotate(turn).opacity(0.7);
    let button = tap(glass_flat(row().key("genre").h(36.0).pl(16.0).pr(12.0).gap(10.0).radius(18.0).center_y())
        .fill(if s.genre_open { ink(0.16) } else { ink(0.07) }).hover(ink(0.14))
        .child(label(s.genre.clone().unwrap_or_else(|| "All Genres".into()), 15.0, 600.0, Color::WHITE))
        .child(col().gap(-1.0).child(chevron(-PI / 2.0)).child(chevron(PI / 2.0))))
        .on_press(|s: &mut State| { s.genre_open = !s.genre_open; s.genre_reveal = s.genre_open });
    // Cinemeta's New takes years as its "genre": name what the choices are.
    let years = c.genres.iter().all(|g| g.len() == 4 && g.bytes().all(|b| b.is_ascii_digit()));
    row().gap(10.0).center_y().child(label(if years { "Year" } else { "Genre" }, 15.0, 500.0, ink(0.55))).child(button)
}

/// The genre pop-up's menu, under its button (above it when there's no room below), over a
/// scrim that closes it: All Genres where the catalog allows it, then each genre; the current
/// one checked. A long list scrolls, and opens on the current one.
pub fn genre_menu(s: &State) -> Option<[El; 2]> {
    let c = s.catalogs.get(s.selected).filter(|c| s.genre_open && !c.genres.is_empty())?;
    let item = |key: String, words: String, on: bool, g: Option<String>| row().key(key).h(32.0).px(10.0).gap(6.0).radius(8.0).center_y().hover(ink(0.12))
        .child(label(if on { "✓" } else { "" }, 13.0, 700.0, Color::WHITE).w(16.0))
        .child(label(words, 14.0, 500.0, Color::WHITE).truncate().grow(1.0).min_w(0.0))
        .on_press(move |s: &mut State| pick_genre(s, g.clone()));
    let mut rows = col().w_full().padding(6.0);
    if !c.needs_genre {
        rows = rows.child(item("genre-all".into(), "All Genres".into(), s.genre.is_none(), None))
            .child(el().h(1.0).mx(10.0).my(5.0).fill(ink(0.1)));
    }
    for (n, g) in c.genres.iter().enumerate() {
        rows = rows.child(item(format!("genre-{n}"), g.clone(), s.genre.as_ref() == Some(g), Some(g.clone())));
    }
    let at = s.genre.as_ref().and_then(|g| c.genres.iter().position(|h| h == g)).map_or("genre-all".to_string(), |n| format!("genre-{n}"));
    let list = col().key("genre-scroll").w_full().shrink().min_h(0.0).scroll(ScrollSpec { stick_bottom: false })
        .scroll_edge_fade(16.0).scroll_margin(16.0).child(rows)
        // Once laid out: a request made with the press would come before the list is there.
        .on_layout_tx(move |s: &mut State, _, tx| if s.genre_reveal { s.genre_reveal = false; tx.scroll_center_instant(at.clone()) });
    let menu = glass(col().key("genre-menu").w(220.0).max_h(440.0).radius(14.0).clip_rounded(14.0)).fill(bg().alpha(0.94))
        .shadow(12.0, 32.0, Color::BLACK.alpha(0.45))
        .transition(Transition::offset(0.0, -6.0).combined(Transition::opacity()))
        .child(list)
        .anchored_fit("genre", Side::Bottom, Align::End, 8.0);
    Some([all(el()).key("genre-scrim").on_press(|s: &mut State| s.genre_open = false), menu])
}

fn detail(s: &State) -> El {
    let back = back_button(s).on_press(|s: &mut State| s.page = s.back_to);
    let Some(m) = &s.meta else {
        return el().w_full().h(HERO_H + top(s)).child(back.pin(Some(PAD), Some(24.0 + top(s)), None, None))
    };
    let ty = s_(&m["type"]).to_string();
    let videos = m["videos"].as_array().cloned().unwrap_or_default();
    let episodic = ty != "movie" && !videos.is_empty();
    let mut eps: Vec<&Value> = videos.iter().filter(|v| v["season"].as_i64() == Some(s.season)).collect();
    eps.sort_by_key(|v| v["episode"].as_i64());
    // Play: where you left off, else the movie, else this season's first episode.
    let target = s.resume.as_ref().map(|(v, _)| v.clone())
        .or_else(|| (!episodic).then(|| movie_video(m, &s.meta_id)))
        .or_else(|| eps.first().map(|v| s_(&v["id"]).to_string()));
    let words = match (s.resume.as_ref().and_then(|(v, _)| episode_of(v)), target.as_deref().and_then(episode_of)) {
        (Some((se, ep)), _) => format!("Resume S{se}, E{ep}"),
        (None, Some((se, ep))) if episodic => format!("Play S{se}, E{ep}"),
        _ => "Play".to_string(),
    };
    let mut actions = row().gap(12.0).center_y();
    if let Some(vid) = target {
        let ty = ty.clone();
        actions = actions.child(primary(s, "play", words).on_press(move |s: &mut State| pick(s, ty.clone(), vid.clone())));
    }
    let on = library::on_list(s);
    actions = actions.child(tap(glass(row().key("watchlist").h(54.0).pl(20.0).pr(24.0).gap(10.0).radius(12.0).center()).fill(ink(if on { 0.16 } else { 0.08 })).hover(ink(0.2))
        .child(label(if on { "✓" } else { "+" }, 22.0, 600.0, Color::WHITE))
        .child(label(if on { "On Watchlist" } else { "Watchlist" }, 18.0, 600.0, Color::WHITE)))
        .on_press(library::toggle));
    let info = headline(m, None).child(el().h(8.0)).child(actions);
    let [image, side, fade, scrim] = backdrop_layer(s, hero_img(backdrop(m)).as_deref());
    let head = el().key("detail-hero").w_full().h(HERO_H + top(s))
        .child(image).child(side).child(fade).child(scrim)
        .child(info.pin(Some(PAD), None, None, Some(28.0)))
        .child(back.pin(Some(PAD), Some(22.0 + top(s)), None, None));
    let mut page = col().w_full().gap(48.0).pb(64.0).child(head);
    if episodic { page = page.child(episodes(s, m, &ty, eps)) }
    page.child(about(s, m))
}

fn section_title(words: &str) -> El { label(words, 24.0, 600.0, Color::WHITE) }

/// The season's episodes as cards across the page; the one whose streams are open is outlined.
fn episodes(s: &State, m: &Value, ty: &str, eps: Vec<&Value>) -> El {
    let mut tabs = row().gap(8.0).flex_wrap();
    for n in seasons(m) {
        let name = if n == 0 { "Specials".to_string() } else { format!("Season {n}") };
        tabs = tabs.child(chip(format!("season-{n}"), name, n == s.season).on_press(move |s: &mut State| s.season = n));
    }
    let mut grid = row().w_full().equal_columns(4).gap(GAP);
    for v in eps {
        let vid = s_(&v["id"]).to_string();
        let on = vid == s.stream_for;
        let n = v["episode"].as_i64().or(v["number"].as_i64()).unwrap_or(0);
        let image = picture(s, card_img(v["thumbnail"].as_str().map(String::from)).as_deref()).w_full().aspect_ratio(16.0 / 9.0)
            .radius(12.0).clip_rounded(12.0).border(if on { 2.0 } else { 1.0 }, if on { Color::WHITE } else { ink(0.08) })
            .child(row().h(24.0).px(8.0).radius(6.0).center().fill(Color::BLACK.alpha(0.6)).child(label(format!("E{n}"), 12.0, 700.0, Color::WHITE))
                .pin(Some(10.0), None, None, Some(10.0)));
        let rating = v["rating"].as_str().filter(|r| !r.is_empty() && *r != "0").map(|r| format!("★ {r}"));
        let facts = [date(s_(&v["released"])), rating.unwrap_or_default()].into_iter().filter(|f| !f.is_empty()).collect::<Vec<_>>().join(" · ");
        let overview = v["overview"].as_str().or(v["description"].as_str()).unwrap_or("").to_string();
        let ty = ty.to_string();
        let mut card = tap(col().key(format!("ep-{vid}")).gap(8.0).min_w(0.0)).child(image)
            .child(label(v["title"].as_str().or(v["name"].as_str()).unwrap_or("").to_string(), 16.0, 600.0, Color::WHITE).truncate());
        if !facts.is_empty() { card = card.child(label(facts, 13.0, 500.0, ink(0.5))) }
        if !overview.is_empty() { card = card.child(label(overview, 14.0, 400.0, ink(0.62)).font(TextStyle::new(14.0, 400.0).lh(20.0)).wrap().max_lines(3)) }
        grid = grid.child(card.on_press(move |s: &mut State| pick(s, ty.clone(), vid.clone())));
    }
    col().w_full().px(PAD).gap(20.0).child(section_title("Episodes")).child(tabs).child(grid)
}

/// The people to credit: directors, the cast, then writers, one per person with their roles
/// merged and the photo an addon gave (cast from `State::people` when there is one, else
/// Cinemeta's names).
pub fn credits(s: &State, m: &Value) -> Vec<(String, Vec<String>, Option<String>)> {
    let names = |k: &str| m[k].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>();
    let cast: Vec<(String, String, Option<String>)> = if s.people.is_empty() {
        names("cast").into_iter().map(|n| (n, "Cast".to_string(), None)).collect()
    } else {
        s.people.iter().map(|c| (s_(&c["name"]).to_string(), c["character"].as_str().filter(|r| !r.is_empty()).unwrap_or("Cast").to_string(),
            c["photo"].as_str().filter(|u| !u.is_empty()).map(String::from))).collect()
    };
    let crew = |k: &str, role: &str| names(k).into_iter().map(|n| (n, role.to_string(), None)).collect::<Vec<_>>();
    let mut people: Vec<(String, Vec<String>, Option<String>)> = vec![];
    for (name, role, photo) in crew("director", "Director").into_iter().chain(cast).chain(crew("writer", "Writer")) {
        match people.iter_mut().find(|(p, _, _)| *p == name) {
            Some((_, roles, pic)) => { roles.push(role); if pic.is_none() { *pic = photo } }
            None => people.push((name, vec![role], photo)),
        }
    }
    people
}

/// Credits as a row of people (`credits`), the facts in columns, and the whole synopsis when it
/// runs past the hero's three lines.
fn about(s: &State, m: &Value) -> El {
    let names = |k: &str| m[k].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>();
    let people = credits(s, m);
    let mut page = col().w_full().px(PAD).gap(48.0);
    if !people.is_empty() {
        let mut row_ = row().w_full().equal_columns(PEOPLE).gap(GAP);
        for (i, (name, roles, photo)) in people.into_iter().take(PEOPLE).enumerate() {
            let initials: String = name.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect();
            let face = match photo {
                Some(u) => picture(s, Some(&sized(&u, 88.0))).size(88.0, 88.0).radius(44.0).clip_rounded(44.0),
                None => row().size(88.0, 88.0).radius(44.0).center()
                    .linear_gradient(PI * 0.75, [(0.0, ink(0.22)), (1.0, ink(0.06))])
                    .child(label(initials.to_uppercase(), 26.0, 600.0, ink(0.85))),
            };
            // A press searches their name: their films and shows (`search::find`).
            let who = name.clone();
            row_ = row_.child(tap(col().key(format!("person-{i}")).min_w(0.0).gap(6.0).center())
                .child(face.mb(6.0).border(1.0, ink(0.1)))
                .child(label(name, 15.0, 600.0, Color::WHITE).truncate())
                .child(label(roles.join(", "), 13.0, 500.0, ink(0.5)).truncate())
                .on_press(move |s: &mut State| search::find(s, &who)));
        }
        page = page.child(col().w_full().gap(20.0).child(section_title("Cast & Crew")).child(row_));
    }
    let genres = names("genres").into_iter().chain(names("genre")).take(4).collect::<Vec<_>>();
    let released = Some(date(s_(&m["released"]))).filter(|d| !d.is_empty()).unwrap_or_else(|| s_(&m["releaseInfo"]).to_string());
    let facts = [("Released", released), ("Runtime", s_(&m["runtime"]).to_string()), ("Genres", genres.join(", ")),
        ("Country", s_(&m["country"]).to_string()), ("Status", s_(&m["status"]).to_string()), ("Awards", s_(&m["awards"]).to_string())];
    let facts: Vec<_> = facts.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    let synopsis = s_(&m["description"]);
    if facts.is_empty() && synopsis.len() <= LONG_SYNOPSIS { return page }
    let mut details = col().w_full().gap(20.0).child(section_title("Details"));
    if synopsis.len() > LONG_SYNOPSIS {
        details = details.child(label(synopsis.to_string(), 17.0, 400.0, ink(0.75)).font(TextStyle::new(17.0, 400.0).lh(27.0)).wrap().max_w(860.0));
    }
    let mut grid = row().w_full().equal_columns(4).gap(GAP);
    for (k, v) in facts {
        grid = grid.child(col().min_w(0.0).gap(6.0).pb(10.0)
            .child(label(k, 13.0, 500.0, ink(0.5)))
            .child(label(v, 15.0, 500.0, ink(0.9)).font(TextStyle::new(15.0, 500.0).lh(22.0)).wrap()));
    }
    page.child(details.child(grid))
}

fn login(s: &State) -> El {
    let field = |edit: &TextEdit, key: &str, hint: &str| glass(text_input(edit, TextStyle::new(16.0, 400.0)).key(key.to_string()).single_line()
        .w_full().px(16.0).py(14.0).radius(12.0)).color(Color::WHITE).placeholder(hint.to_string(), ink(0.4));
    let card = glass(col().w(400.0).padding(36.0).gap(14.0).radius(24.0)).shadow(20.0, 60.0, Color::BLACK.alpha(0.5))
        .child(label("Stremio", 36.0, 700.0, Color::WHITE).font(TextStyle::new(36.0, 700.0).tracking(-0.8)))
        .child(label("Sign in with your Stremio account", 15.0, 500.0, ink(0.6)))
        .child(el().h(6.0))
        .child(field(&s.email, "email", "Email").on_edit(|s: &mut State| &mut s.email).on_submit(sign_in))
        .child(field(&s.password, "password", "Password").secure().on_edit(|s: &mut State| &mut s.password).on_submit(sign_in))
        .child(tap(row().key("sign-in").w_full().h(50.0).radius(12.0).center().fill(Color::WHITE).hover(ink(0.85))
            .child(label("Sign in", 16.0, 600.0, Color::BLACK))).on_press_tx(sign_in));
    col().w_full().h_full().pt(top(s)).center()
        .linear_gradient(PI * 0.8, [(0.0, oklch(0.32, 0.09, 260.0)), (0.55, bg()), (1.0, oklch(0.1, 0.01, 265.0))])
        .child(card)
}

// ---- root ----

/// Every page, with the app's title strip over its top where the system's title bar was (gone,
/// see `window::caption`): clear, the system's drag region, and caption buttons at the right.
/// Full-bleed backdrops run under it; everything else starts below it (`top`). Fullscreen has none.
pub fn view(s: &State, _: &mut Cx<State>) -> El {
    let page = page(s);
    if top(s) <= 0.0 { return page }
    page.child(title_bar(s).pin(Some(0.0), Some(0.0), Some(0.0), None))
}

/// Height of the title strip over the page: where content starts.
fn top(s: &State) -> f32 { if s.fullscreen || s.pip { 0.0 } else { s.caption.h } }

fn title_bar(s: &State) -> El {
    let c = &s.caption;
    // macOS: the traffic lights are AppKit's, over the strip's left end; the strip only drags.
    if cfg!(target_os = "macos") { return row().key("title-bar").h(c.h).w_full().window_drag() }
    // Hover only: the system hit-tests the strip, so these act on release like its own.
    let button = |key: &str, glyph: fn(&Icons) -> &Image, hover: Color| row().key(key.to_string())
        .w(window::CAPTION_BUTTON_W).h_full().center().hover(hover).child(icon(s, glyph, 10.0));
    row().key("title-bar").h(c.h).justify_end()
        .child(button("caption-min", |i| &i.minimize, ink(0.08)).on_press(|_: &mut State| window::syscommand(window::SC_MINIMIZE)))
        // Maximize is the system's to hit-test (Windows 11 snap layouts), so its hover comes from there.
        .child(button("caption-max", if c.zoomed { |i| &i.restore } else { |i| &i.maximize }, ink(0.08))
            .fill(if c.max_hover { ink(0.08) } else { Color::TRANSPARENT })
            .on_press(|s: &mut State| window::syscommand(if s.caption.zoomed { window::SC_RESTORE } else { window::SC_MAXIMIZE })))
        .child(button("caption-close", |i| &i.quit, oklch(0.55, 0.2, 27.0)).on_press(|_: &mut State| window::syscommand(window::SC_CLOSE)))
}

fn page(s: &State) -> El {
    let body = match s.page {
        Page::Player => return player(s),
        Page::Login => return login(s).child(if s.status.is_empty() { el() } else { toast(s) }),
        Page::Home => home(s),
        Page::Browse(ty) => browse(s, ty),
        Page::Detail => detail(s),
        Page::Search => search::view(s),
        Page::Settings => settings::view(s),
        Page::Addons => addons::view(s),
    };
    let body = if matches!(s.page, Page::Home | Page::Detail) { body } else { col().w_full().pt(top(s)).child(body) };
    let mut root = col().w_full().h_full().fill(bg())
        .on_layout_tx(|s: &mut State, r, tx| { tx.live_only(); s.view_h = r[3] })
        .child(body.key("page-scroll").scroll(ScrollSpec { stick_bottom: false }).w_full().grow(1.0).shrink().min_h(0.0));
    // A click anywhere outside the open menu closes it.
    if s.menu { root = root.child(all(el()).key("menu-scrim").on_press(|s: &mut State| s.menu = false)) }
    root = root.child(nav(s));
    if matches!(s.page, Page::Browse(_)) && let Some(m) = genre_menu(s) { for e in m { root = root.child(e) } }
    if s.sheet && s.page == Page::Detail { for e in sheet(s) { root = root.child(e) } }
    if !s.status.is_empty() { root = root.child(toast(s)) }
    root
}
