//! Views: a dark, cinematic layout with a floating glass nav, a full-bleed hero, and rows of
//! landscape cards. Playback controls sit under the video (mpv's window covers the top).

use crate::*;
use rune::tok::{TextStyle, motion};
use rune::{Color, Cursor, Cx, Cycle, Drag, El, Phase, ScrollSpec, SurfaceFit, Transition, col, el, oklch, row, text, text_input};
use std::f32::consts::PI;

#[path = "search.rs"]
pub mod search;

const PAD: f32 = 56.0;
const HERO_H: f32 = 560.0;
/// Space between cards.
const GAP: f32 = 22.0;

fn bg() -> Color { oklch(0.15, 0.012, 265.0) }
fn ink(a: f32) -> Color { Color::WHITE.alpha(a) }

fn label(s: impl Into<String>, size: f32, weight: f32, color: Color) -> El {
    text(s).font(TextStyle::new(size, weight)).color(color)
}

/// Frosted panel: a faint white wash and hairline over whatever is behind it.
fn glass(e: El) -> El {
    e.fill(ink(0.07)).border(1.0, ink(0.12)).backdrop_blur(24.0)
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

fn s_(v: &Value) -> &str { v.as_str().unwrap_or("") }

/// `Drama · Mystery · 2026 · 107 min`
fn meta_line(m: &Value) -> String {
    let genres = m["genres"].as_array().or(m["genre"].as_array()).into_iter().flatten().take(2).map(s_);
    genres.chain([s_(&m["releaseInfo"]), s_(&m["runtime"])]).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
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
    let pill = glass(row().key("nav").h(50.0).px(6.0).gap(4.0).radius(25.0).center_y())
        .shadow(10.0, 30.0, Color::BLACK.alpha(0.35))
        .child(find).child(item("Home", Page::Home)).child(item("Movies", Page::Browse("movie"))).child(item("Shows", Page::Browse("series")))
        .child(el().w(4.0)).child(avatar);
    // The menu hangs off a plain wrapper so the pill's blur layer doesn't clip it.
    let mut anchor = el().child(pill);
    if s.menu { anchor = anchor.child(menu(s).pin(None, Some(60.0), Some(0.0), None)) }
    row().pin(Some(0.0), Some(18.0), Some(0.0), None).justify_center().child(anchor)
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
        .child(tap(row().key("sign-out").h(40.0).px(12.0).mt(4.0).radius(10.0).center_y().hover(ink(0.08))
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
    let mut list = col().w_full().gap(10.0).pb(24.0);
    if s.streams.is_empty() { list = list.child(label("Finding streams…", 15.0, 500.0, ink(0.5))) }
    for (i, st) in s.streams.iter().enumerate() {
        let url = st.url.clone();
        list = list.child(tap(col().key(format!("st-{i}")).w_full().px(16.0).py(14.0).gap(6.0).radius(14.0)
            .fill(ink(0.06)).border(1.0, ink(0.08)).hover(ink(0.12))
            .child(row().gap(10.0).center_y().child(icon(s, |i| &i.play, 14.0)).child(label(st.name.clone(), 15.0, 600.0, Color::WHITE).truncate()))
            .child(label(st.title.clone(), 13.0, 400.0, ink(0.62)).font(TextStyle::new(13.0, 400.0).lh(19.0)).wrap().max_lines(4)))
            .on_press(move |s: &mut State| play(s, &url)));
    }
    let panel = col().key("sheet").w(500.0).pin(None, Some(0.0), Some(0.0), Some(0.0)).pt(28.0).px(24.0).gap(20.0)
        .fill(bg().alpha(0.9)).backdrop_blur(30.0).border(1.0, ink(0.08)).shadow(0.0, 40.0, Color::BLACK.alpha(0.5))
        .transition(Transition::offset(500.0, 0.0).combined(Transition::opacity()))
        .child(head)
        .child(list.scroll(ScrollSpec { stick_bottom: false }).grow(1.0).shrink().min_h(0.0));
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

/// Full-bleed backdrop with the scrims that keep text readable over any image.
fn backdrop_layer(s: &State, url: Option<&str>) -> [El; 3] {
    [
        all(picture(s, url)),
        all(el()).linear_gradient(PI / 2.0, [(0.0, Color::BLACK.alpha(0.8)), (0.6, Color::BLACK.alpha(0.0))]),
        all(el()).linear_gradient(PI, [(0.35, bg().alpha(0.0)), (1.0, bg())]),
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
    let picks: Vec<&Value> = s.items.get(&0).into_iter().flatten().take(HERO_PICKS).collect();
    let Some(&m) = picks.get(s.hero.min(picks.len().saturating_sub(1))) else { return el().w_full().h(HERO_H) };
    let (ty, id) = (s_(&m["type"]).to_string(), s_(&m["id"]).to_string());
    let kicker = s.catalogs.first().map(|c| c.name.clone());
    let info = headline(m, kicker).child(el().h(8.0))
        .child(primary(s, "watch", "Watch now").self_start().on_press(move |s: &mut State| open_meta(s, ty.clone(), id.clone())));
    // The current dot stretches into a track that fills while its title stays up: a render-time
    // tween, so the fill moves without rebuilding the page.
    let mut dots = row().gap(8.0).center_y();
    for i in 0..picks.len() {
        let on = i == s.hero.min(picks.len() - 1);
        let mut dot = el().key(format!("dot-{i}")).h(8.0).w(if on { DOT_W } else { 8.0 }).radius(4.0)
            .fill(ink(0.35)).clip_rounded(4.0).motion(motion::SNAPPY);
        if on { dot = dot.child(el().key(format!("fill-{}", s.hero_round)).h(8.0).w(DOT_W).fill(Color::WHITE).live_rect(&s.hero_fill)) }
        dots = dots.child(tap(dot).on_press(move |s: &mut State| show_hero(s, i)));
    }
    let [image, side, fade] = backdrop_layer(s, hero_img(backdrop(m)).as_deref());
    // Keyed by title so a change cross-fades: the old image fades out as the new one fades in.
    let image = image.key(format!("hero-{}", s_(&m["id"])));
    el().key("hero").w_full().h(HERO_H)
        .child(image).child(side).child(fade)
        .child(info.pin(Some(PAD), None, None, Some(40.0)))
        .child(dots.pin(None, None, Some(PAD), Some(56.0)))
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

fn section(title: &str, body: El) -> El {
    col().w_full().px(PAD).gap(16.0).child(label(title, 24.0, 600.0, Color::WHITE)).child(body)
}

fn up_next(s: &State) -> El {
    let mut cards = row().w_full().equal_columns(ROW_CARDS).gap(GAP);
    for item in s.library.iter().take(ROW_CARDS) {
        let st = &item["state"];
        let (at, len) = (st["timeOffset"].as_f64().unwrap_or(0.0), st["duration"].as_f64().unwrap_or(0.0));
        let left = ((len - at) / 60_000.0).max(0.0).round();
        let mut sub = if len > 0.0 { format!("{left} min left") } else { "Continue".into() };
        if let Some((se, ep)) = episode_of(s_(&st["video_id"])) { sub = format!("S{se}, E{ep} · {sub}") }
        let done = (len > 0.0).then(|| (at / len) as f32);
        let it = item.clone();
        cards = cards.child(landscape(s, format!("cw-{}", s_(&item["_id"])), backdrop(item), s_(&item["name"]).to_string(), sub, done)
            .on_press(move |s: &mut State| resume(s, &it)));
    }
    section("Up Next", cards)
}

fn catalog_row(s: &State, i: usize, items: &[Value]) -> El {
    let mut cards = row().w_full().equal_columns(ROW_CARDS).gap(GAP);
    for m in items.iter().take(ROW_CARDS) {
        let (ty, id) = (s_(&m["type"]).to_string(), s_(&m["id"]).to_string());
        let sub = meta_line(m);
        cards = cards.child(landscape(s, format!("c{i}-{id}"), backdrop(m), s_(&m["name"]).to_string(), sub, None)
            .on_press(move |s: &mut State| open_meta(s, ty.clone(), id.clone())));
    }
    section(&s.catalogs[i].name, cards)
}

fn home(s: &State) -> El {
    let mut page = col().w_full().gap(36.0).pb(56.0).child(hero(s));
    if !s.library.is_empty() { page = page.child(up_next(s)) }
    for i in 0..s.catalogs.len().min(HOME_ROWS) {
        // The first catalog feeds the hero; show it as a row too, from its sixth title on.
        let Some(items) = s.items.get(&i) else { continue };
        let items = if i == 0 && items.len() > HERO_PICKS { &items[HERO_PICKS..] } else { &items[..] };
        page = page.child(catalog_row(s, i, items));
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

fn browse(s: &State, ty: &str) -> El {
    let mut chips = row().w_full().gap(10.0).flex_wrap();
    for (i, c) in s.catalogs.iter().enumerate().filter(|(_, c)| c.ty == ty) {
        chips = chips.child(chip(format!("cat-{i}"), c.name.clone(), i == s.selected).on_press(move |s: &mut State| open_catalog(s, i)));
    }
    let mut grid = row().w_full().equal_columns(7).gap(GAP);
    for m in s.items.get(&s.selected).into_iter().flatten() {
        grid = grid.child(poster_card(s, m, format!("p-{}", s_(&m["id"]))));
    }
    col().w_full().pt(104.0).px(PAD).pb(56.0).gap(28.0)
        .child(label(type_label(ty), 40.0, 700.0, Color::WHITE).font(TextStyle::new(40.0, 700.0).tracking(-0.8)))
        .child(chips).child(grid)
}

fn detail(s: &State) -> El {
    let back = back_button(s).on_press(|s: &mut State| s.page = s.back_to);
    let Some(m) = &s.meta else {
        return el().w_full().h(HERO_H).child(back.pin(Some(PAD), Some(24.0), None, None))
    };
    let ty = s_(&m["type"]).to_string();
    let videos = m["videos"].as_array().cloned().unwrap_or_default();
    let episodic = ty != "movie" && !videos.is_empty();
    let mut eps: Vec<&Value> = videos.iter().filter(|v| v["season"].as_i64() == Some(s.season)).collect();
    eps.sort_by_key(|v| v["episode"].as_i64());
    // Play: where you left off, else the movie, else this season's first episode.
    let target = s.resume.as_ref().map(|(v, _)| v.clone())
        .or_else(|| (!episodic).then(|| s.meta_id.clone()))
        .or_else(|| eps.first().map(|v| s_(&v["id"]).to_string()));
    let words = match (s.resume.as_ref().and_then(|(v, _)| episode_of(v)), target.as_deref().and_then(episode_of)) {
        (Some((se, ep)), _) => format!("Resume S{se}, E{ep}"),
        (None, Some((se, ep))) if episodic => format!("Play S{se}, E{ep}"),
        _ => "Play".to_string(),
    };
    let mut info = headline(m, None).child(el().h(8.0));
    if let Some(vid) = target {
        let ty = ty.clone();
        info = info.child(primary(s, "play", words).self_start().on_press(move |s: &mut State| pick(s, ty.clone(), vid.clone())));
    }
    let [image, side, fade] = backdrop_layer(s, hero_img(backdrop(m)).as_deref());
    let head = el().key("detail-hero").w_full().h(HERO_H)
        .child(image).child(side).child(fade)
        .child(info.pin(Some(PAD), None, None, Some(28.0)))
        .child(back.pin(Some(PAD), Some(22.0), None, None));
    let mut page = col().w_full().gap(28.0).pb(56.0).child(head);
    if episodic {
        let mut tabs = row().gap(8.0).flex_wrap();
        for n in seasons(m) {
            let name = if n == 0 { "Specials".to_string() } else { format!("Season {n}") };
            tabs = tabs.child(chip(format!("season-{n}"), name, n == s.season).on_press(move |s: &mut State| s.season = n));
        }
        let mut list = col().gap(4.0);
        for v in eps {
            let vid = s_(&v["id"]).to_string();
            let on = vid == s.stream_for;
            let ty = ty.clone();
            list = list.child(tap(row().key(format!("ep-{vid}")).h(52.0).px(14.0).gap(14.0).radius(12.0).center_y()
                .fill(if on { ink(0.12) } else { Color::TRANSPARENT }).hover(ink(0.08))
                .child(label(format!("{}", v["episode"].as_i64().unwrap_or(0)), 15.0, 600.0, ink(0.45)).w(28.0))
                .child(label(v["title"].as_str().or(v["name"].as_str()).unwrap_or("").to_string(), 16.0, 500.0, Color::WHITE).truncate().grow(1.0).min_w(0.0)))
                .on_press(move |s: &mut State| pick(s, ty.clone(), vid.clone())));
        }
        page = page.child(col().w_full().max_w(980.0).px(PAD).gap(16.0)
            .child(label("Episodes", 22.0, 600.0, Color::WHITE)).child(tabs).child(list));
    }
    page
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
    col().w_full().h_full().center()
        .linear_gradient(PI * 0.8, [(0.0, oklch(0.32, 0.09, 260.0)), (0.55, bg()), (1.0, oklch(0.1, 0.01, 265.0))])
        .child(card)
}

// ---- player ----

fn seek(s: &mut State, d: &mut Drag, _: &mut Tx) {
    let f = (d.x / d.w.max(1.0)).clamp(0.0, 1.0);
    if d.phase != Phase::Up { s.playback.scrub = Some(f); return }
    s.playback.scrub = None;
    if d.cancelled() || s.playback.dur <= 0.0 { return }
    s.playback.pos = f as f64 * s.playback.dur;
    mpv(s, &["seek", &s.playback.pos.to_string(), "absolute"]);
}

/// Stremio's loader breathing: 0.4 → 1 → 0.4 opacity over two seconds.
const PULSE: Cycle = Cycle { period: 2.0, phase: 0.0, stops: &[(0.0, 0.4), (0.5, 1.0), (1.0, 0.4)], still: 1.0, fps: 30.0 };

/// A plain spinner: eight dots around a circle, each fading a beat after the last.
fn spinner() -> El {
    const FADE: &[(f32, f32)] = &[(0.0, 1.0), (0.8, 0.15), (1.0, 1.0)];
    let mut ring = el().size(44.0, 44.0);
    for i in 0..8 {
        let a = i as f32 / 8.0 * 2.0 * PI;
        ring = ring.child(el().size(7.0, 7.0).radius(3.5).fill(Color::WHITE)
            .cycle(Cycle { period: 0.9, phase: -(i as f32) / 8.0, stops: FADE, still: 1.0, fps: 30.0 })
            .absolute(18.5 + 17.0 * a.sin(), 18.5 - 17.0 * a.cos()));
    }
    ring
}

/// What shows while the stream loads (mpv's window stays hidden until the first frame): the
/// title's logo pulsing, its bright copy uncovered left to right as the cache fills over a dim
/// one, as Stremio does. No logo: a spinner.
fn loader(s: &State) -> El {
    let img = s.meta.as_ref().and_then(logo).and_then(|u| s.images.get(&u)).filter(|i| i.is_ready());
    let Some(img) = img else { return all(el()).center().child(spinner()) };
    let [iw, ih] = img.size().unwrap_or([400, 160]);
    let k = (420.0 / iw as f32).min(180.0 / ih as f32);
    let (w, h) = (iw as f32 * k, ih as f32 * k);
    let done = if s.playback.started { 1.0 } else { (s.playback.cache_pct / 100.0).clamp(0.0, 1.0) as f32 };
    let art = || el().size(w, h).pixels(img.pixels());
    all(el()).center().child(
        el().size(w, h).cycle(PULSE)
            .child(art().opacity(0.2).absolute(0.0, 0.0))
            .child(el().w(w * done).h(h).clip_rounded(0.0).absolute(0.0, 0.0).child(art())),
    )
}

/// ISO 639-2 codes as addons send them, for the common languages.
pub fn language(code: &str) -> &str {
    match code {
        "eng" | "en" => "English", "ger" | "deu" | "de" => "German", "fre" | "fra" | "fr" => "French",
        "spa" | "es" => "Spanish", "ita" | "it" => "Italian", "por" | "pt" => "Portuguese", "pob" => "Portuguese (BR)",
        "dut" | "nld" | "nl" => "Dutch", "rus" | "ru" => "Russian", "pol" | "pl" => "Polish", "tur" | "tr" => "Turkish",
        "ara" | "ar" => "Arabic", "chi" | "zho" | "zh" => "Chinese", "jpn" | "ja" => "Japanese", "kor" | "ko" => "Korean",
        "swe" | "sv" => "Swedish", "nor" | "no" => "Norwegian", "dan" | "da" => "Danish", "fin" | "fi" => "Finnish",
        "gre" | "ell" | "el" => "Greek", "heb" | "he" => "Hebrew", "hun" | "hu" => "Hungarian", "cze" | "ces" | "cs" => "Czech",
        "rum" | "ron" | "ro" => "Romanian", "hin" | "hi" => "Hindi", "ukr" | "uk" => "Ukrainian", "vie" | "vi" => "Vietnamese",
        other => other,
    }
}

/// Audio & subtitle choices, beside the video: the file's audio tracks, then subtitles (off,
/// the file's own tracks, then the addons').
fn subtitles(s: &State) -> El {
    let p = &s.playback;
    let option = |key: String, words: String, on: bool| tap(row().key(key).w_full().h(44.0).px(14.0).gap(12.0).radius(10.0).center_y()
        .fill(if on { ink(0.12) } else { Color::TRANSPARENT }).hover(ink(0.08))
        .child(el().size(8.0, 8.0).radius(4.0).fill(if on { Color::WHITE } else { Color::TRANSPARENT }))
        .child(label(words, 15.0, if on { 600.0 } else { 500.0 }, if on { Color::WHITE } else { ink(0.8) }).truncate().grow(1.0).min_w(0.0)));
    let heading = |t: &str| label(t, 12.0, 600.0, ink(0.45)).pt(12.0).px(14.0);
    let section = |t: &str| label(t, 16.0, 700.0, ink(0.9)).pt(16.0).pb(4.0).px(14.0);
    let mut list = col().w_full().gap(2.0).pb(24.0);
    if !p.audio.is_empty() { list = list.child(section("Audio")) }
    for t in &p.audio {
        let id = t.id.clone();
        list = list.child(option(format!("aud-{id}"), t.label.clone(), p.aid == t.id).on_press(move |s: &mut State| mpv(s, &["set", "aid", &id])));
    }
    list = list.child(section("Subtitles"))
        .child(option("sub-off".into(), "Off".into(), p.sid.is_empty() || p.sid == "no").on_press(|s: &mut State| mpv(s, &["set", "sid", "no"])));
    let embedded: Vec<_> = p.tracks.iter().filter(|t| t.external.is_empty()).collect();
    if !embedded.is_empty() { list = list.child(heading("IN THIS VIDEO")) }
    for t in embedded {
        let id = t.id.clone();
        list = list.child(option(format!("sub-{id}"), t.label.clone(), p.sid == t.id).on_press(move |s: &mut State| mpv(s, &["set", "sid", &id])));
    }
    if !s.addon_subs.is_empty() { list = list.child(heading("FROM YOUR ADDONS")) }
    let mut subs: Vec<&(String, String)> = s.addon_subs.iter().collect();
    subs.sort_by_key(|(lang, _)| language(lang));
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (lang, url) in subs {
        let n = seen.entry(language(lang)).and_modify(|n| *n += 1).or_insert(1);
        let on = p.tracks.iter().any(|t| t.external == *url && t.id == p.sid);
        let (l, u) = (lang.clone(), url.clone());
        list = list.child(option(format!("asub-{url}"), format!("{} {n}", language(lang)), on).on_press(move |s: &mut State| add_sub(s, &l, &u)));
    }
    col().key("subtitles").w(SUBS_W).h_full().pt(22.0).px(14.0).gap(12.0).fill(bg()).border(1.0, ink(0.06))
        .transition(Transition::offset(SUBS_W, 0.0).combined(Transition::opacity()))
        .child(row().w_full().px(6.0).center_y()
            .child(label("Audio & Subtitles", 22.0, 700.0, Color::WHITE).grow(1.0))
            .child(tap(row().key("subs-close").size(36.0, 36.0).radius(18.0).center().hover(ink(0.12))
                .child(icon(s, |i| &i.close, 16.0))).on_press(|s: &mut State| s.subs_open = false)))
        .child(list.scroll(ScrollSpec { stick_bottom: false }).grow(1.0).shrink().min_h(0.0))
}

/// The video (mpv's child window covers that area once playing), the audio & subtitles panel when
/// open, and the controls under them while the mouse is active.
fn player(s: &State) -> El {
    let p = &s.playback;
    let mut video = el().grow(1.0).min_w(0.0).fill(Color::BLACK);
    if p.loading() { video = video.child(loader(s)) }
    let mut stage = row().w_full().grow(1.0).min_h(0.0).child(video);
    if s.subs_open { stage = stage.child(subtitles(s)) }
    let mut root = col().w_full().h_full().fill(Color::BLACK).child(stage);
    if s.controls { root = root.child(bar(s)) }
    root
}

fn bar(s: &State) -> El {
    let p = &s.playback;
    let at = p.scrub.map_or(p.pos, |f| f as f64 * p.dur);
    let frac = if p.dur > 0.0 { (at / p.dur) as f32 } else { 0.0 };
    let round = |key: &str, on: bool| tap(row().key(key.to_string()).size(40.0, 40.0).radius(20.0).center()
        .fill(if on { ink(0.14) } else { Color::TRANSPARENT }).hover(ink(0.12)));
    let back = round("back", false).child(icon(s, |i| &i.back, 20.0)).on_press(stop);
    let toggle = round("toggle", false).child(icon(s, |i| if p.paused { &i.play } else { &i.pause }, 20.0))
        .on_press(|s: &mut State| mpv(s, &["cycle", "pause"]));
    let time = |secs: f64| label(clock(secs), 14.0, 500.0, ink(0.8)).font(TextStyle::new(14.0, 500.0).features("'tnum' 1"));
    let title = s.meta.as_ref().map(|m| s_(&m["name"]).to_string()).unwrap_or_default();
    // The whole bar height is the drag target; the thin track is just its picture.
    let track = row().key("seek").grow(1.0).min_w(0.0).h(BAR).center_y().cursor(Cursor::Pointer).on_drag(seek)
        .child(progress(frac, if p.scrub.is_some() { 6.0 } else { 4.0 }).w_full());
    let mut bar = row().key("bar").w_full().h(BAR).px(14.0).gap(10.0).center_y().fill(Color::BLACK)
        .on_hover_change(|s: &mut State, inside, _| s.bar_hover = inside)
        .child(back).child(toggle)
        .child(label(title, 15.0, 600.0, Color::WHITE).truncate().max_w(260.0)).child(el().w(6.0));
    if p.started && p.buffering { bar = bar.child(label(format!("Buffering {}%", p.cache_pct as u32), 13.0, 500.0, ink(0.6))) }
    bar.child(time(at)).child(track).child(time(p.dur))
        .child(round("subs", s.subs_open).child(icon(s, |i| &i.subs, 20.0)).on_press(|s: &mut State| s.subs_open = !s.subs_open))
        .child(round("fullscreen", false).child(icon(s, |i| if s.fullscreen { &i.shrink } else { &i.expand }, 18.0)).on_press(toggle_fullscreen))
}

// ---- root ----

pub fn view(s: &State, _: &mut Cx<State>) -> El {
    let body = match s.page {
        Page::Player => return player(s),
        Page::Login => return login(s).child(if s.status.is_empty() { el() } else { toast(s) }),
        Page::Home => home(s),
        Page::Browse(ty) => browse(s, ty),
        Page::Detail => detail(s),
        Page::Search => search::view(s),
    };
    let mut root = col().w_full().h_full().fill(bg())
        .on_layout_tx(|s: &mut State, r, tx| { tx.live_only(); s.view_h = r[3] })
        .child(body.scroll(ScrollSpec { stick_bottom: false }).w_full().grow(1.0).shrink().min_h(0.0));
    // A click anywhere outside the open menu closes it.
    if s.menu { root = root.child(all(el()).key("menu-scrim").on_press(|s: &mut State| s.menu = false)) }
    root = root.child(nav(s));
    if s.sheet && s.page == Page::Detail { for e in sheet(s) { root = root.child(e) } }
    if !s.status.is_empty() { root = root.child(toast(s)) }
    root
}
