//! The player page: the video, drawn by Rune from mpv's frames (`video.rs`), filling the window,
//! and over it the loader while it loads and, while the mouse is active, back and the title
//! along the top and the bar along the bottom, each on a scrim and fading in and out, and the
//! Audio & Subtitles, Quality or Cast panel on the right. Double-click toggles fullscreen; the
//! cursor hides with the controls. While casting (`cast`), the TV's controls cover the video.

use super::*;

/// Stremio's loader breathing: 0.4 → 1 → 0.4 opacity over two seconds.
const PULSE: Cycle = Cycle { period: 2.0, phase: 0.0, stops: &[(0.0, 0.4), (0.5, 1.0), (1.0, 0.4)], still: 1.0, fps: 30.0 };

/// Width of the volume slider, in points.
pub const VOL_W: f32 = 96.0;

/// Resolutions in the order the quality list shows them.
const RESOLUTIONS: [&str; 5] = ["4K", "1080p", "720p", "480p", "Other"];

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

/// A stream's resolution, from a word in its name, else its title: `4K`, `1080p`, `720p`,
/// `480p` or `Other`.
pub fn resolution(st: &Stream) -> &'static str {
    let find = |t: &str| t.split(|c: char| !c.is_ascii_alphanumeric()).find_map(|w| match w.to_ascii_lowercase().as_str() {
        "2160p" | "4k" | "uhd" => Some("4K"),
        "1080p" | "1080i" => Some("1080p"),
        "720p" => Some("720p"),
        "480p" | "576p" => Some("480p"),
        _ => None,
    });
    find(&st.name).or_else(|| find(&st.title)).unwrap_or("Other")
}

/// Streams grouped by resolution, best first, with their index in `streams`.
pub fn qualities(streams: &[Stream]) -> Vec<(&'static str, Vec<(usize, &Stream)>)> {
    RESOLUTIONS.iter().map(|&r| (r, streams.iter().enumerate().filter(|(_, st)| resolution(st) == r).collect::<Vec<_>>()))
        .filter(|(_, g)| !g.is_empty()).collect()
}

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

/// What shows over the video while the stream loads or buffers (black before the first frame,
/// the frozen frame after): the title's logo pulsing, its bright copy uncovered left to right as
/// the cache fills over a dim one, as Stremio does. No logo: a spinner.
fn loader(s: &State) -> El {
    let p = &s.playback;
    let logo = s.meta.as_ref().and_then(logo).and_then(|u| s.images.get(&u)).filter(|i| i.is_ready());
    let Some(img) = logo else { return all(el()).key("loader").center().child(spinner()) };
    let [iw, ih] = img.size().unwrap_or([400, 160]);
    let k = (420.0 / iw as f32).min(180.0 / ih as f32);
    let (w, h) = (iw as f32 * k, ih as f32 * k);
    let done = if p.started { 1.0 } else { (p.cache_pct / 100.0).clamp(0.0, 1.0) as f32 };
    let art = || el().size(w, h).pixels(img.pixels());
    all(el()).key("loader").center().child(
        el().size(w, h).cycle(PULSE)
            .child(art().opacity(0.5).absolute(0.0, 0.0)) // plainly there from the first instant, before any cache
            .child(el().w(w * done).h(h).clip_rounded(0.0).absolute(0.0, 0.0).child(art())),
    )
}

/// Fullscreen from a press: the window jumps to its new size in the same frame, so the layout
/// lands there at once instead of springing over from the old size.
fn fullscreen(s: &mut State, tx: &mut Tx) {
    tx.instant();
    toggle_fullscreen(s);
}

/// A click on the video plays or pauses; with a side panel open it closes the panel instead.
/// A double-click toggles pause twice (no change) and switches fullscreen.
fn click_video(s: &mut State) {
    if s.subs_open || s.quality_open || s.cast_open {
        (s.subs_open, s.quality_open, s.cast_open) = (false, false, false);
        return;
    }
    if s.cast.is_some() { return }
    mpv(s, &["cycle", "pause"]);
}

/// Volume drag: follows the pointer as it goes (you hear it), and unmutes.
fn volume_drag(s: &mut State, d: &mut Drag, _: &mut Tx) {
    let v = ((d.x / d.w.max(1.0)).clamp(0.0, 1.0) * 100.0).round() as f64;
    s.vol_drag = (d.phase != Phase::Up).then_some(v);
    if d.cancelled() { return }
    s.playback.volume = v;
    mpv(s, &["set", "volume", &v.to_string()]);
    mpv(s, &["set", "mute", "no"]);
}

fn seek(s: &mut State, d: &mut Drag, _: &mut Tx) {
    let f = (d.x / d.w.max(1.0)).clamp(0.0, 1.0);
    if d.phase != Phase::Up { s.scrub = Some(f); want_preview(s, f); return }
    s.scrub = None;
    if d.cancelled() || s.playback.dur <= 0.0 { return }
    s.playback.pos = f as f64 * s.playback.dur;
    mpv(s, &["seek", &s.playback.pos.to_string(), "absolute"]);
}

/// The seek-bar preview at fraction `f` of the video: asked for unless it's made.
fn want_preview(s: &mut State, f: f32) {
    if s.playback.dur <= 0.0 || s.playing.is_empty() { return }
    let t = preview::bucket(f as f64 * s.playback.dur);
    if s.images.contains_key(&preview::key(&s.playing, t)) { return }
    if let Some(p) = &s.preview { p.want(t) }
}

/// A soft light band sweeping across a `w`×`h` box, left to right: something on its way.
fn shimmer(w: f32, h: f32) -> El {
    let band = w * 0.6;
    el().w(band).h(h).absolute(0.0, 0.0)
        .linear_gradient(PI / 2.0, [(0.0, Color::TRANSPARENT), (0.5, ink(0.16)), (1.0, Color::TRANSPARENT)])
        .looping(rune::LoopSpec::slide(1.3, [-1.0, 0.0], [w / band, 0.0]).easing(rune::Easing::LINEAR))
}

/// Above the seek bar at fraction `f`: the frame there and its time. Until that frame is made,
/// the nearest one made (within a minute) under a shimmer, or just the shimmer.
fn seek_preview(s: &State, f: f32) -> El {
    const W: f32 = 224.0;
    let t = f as f64 * s.playback.dur;
    let b = preview::bucket(t);
    let made = |x: f64| s.images.get(&preview::key(&s.playing, x));
    let exact = made(b);
    let near = exact.or_else(|| (1..=6).flat_map(|k| [b - k as f64 * preview::BUCKET, b + k as f64 * preview::BUCKET]).find_map(made));
    let mut frame = el().w(W).h(W * 9.0 / 16.0).radius(10.0).clip_rounded(10.0).fill(Color::BLACK).border(1.0, ink(0.2));
    if let Some(img) = near { frame = frame.pixels_fit(img.pixels(), SurfaceFit::Fit) }
    if exact.is_none() { frame = frame.child(shimmer(W, W * 9.0 / 16.0)) }
    let x = (f * s.seek_w - W / 2.0).clamp(0.0, (s.seek_w - W).max(0.0));
    col().key("seek-preview").w(W).gap(6.0).items_center().pin(Some(x), None, None, Some(BAR / 2.0 + 14.0))
        .child(frame.shadow(8.0, 24.0, Color::BLACK.alpha(0.5)))
        .child(label(clock(t), 13.0, 600.0, Color::WHITE).font(TextStyle::new(13.0, 600.0).features("'tnum' 1")))
}

pub(super) fn player(s: &State) -> El {
    let p = &s.playback;
    let mut stage = col().grow(1.0).min_w(0.0).h_full()
        .child(el().key("video").w_full().grow(1.0).on_press(click_video).on_double_press_tx(fullscreen));
    if let Some(c) = &s.cast { stage = stage.child(casting(s, c)) } else if p.loading() { stage = stage.child(loader(s)) }
    if s.pip {
        stage = stage.child(pip(s));
    } else if s.controls {
        // Back and the title along the top, below the title strip, on a scrim of their own.
        stage = stage.child(row().key("top-bar").pin(Some(0.0), Some(0.0), Some(0.0), None).h(BAR + 48.0 + top(s))
            .pt(top(s)).px(14.0).pb(48.0).gap(12.0).center_y()
            .linear_gradient(PI, [(0.0, Color::BLACK.alpha(0.75)), (1.0, Color::BLACK.alpha(0.0))])
            .transition(Transition::opacity())
            .on_hover_change(|s: &mut State, inside, _| s.bar_hover = inside)
            .child(tap(row().key("back").size(40.0, 40.0).radius(20.0).center().hover(ink(0.14))
                .child(icon(s, |i| &i.back, 20.0))).on_press(stop))
            .child(label(s.meta.as_ref().map(|m| s_(&m["name"]).to_string()).unwrap_or_default(), 17.0, 600.0, Color::WHITE)
                .truncate().grow(1.0).min_w(0.0)));
        // While casting, the TV's own controls (`casting`) stand in for the bar.
        if s.cast.is_none() {
            stage = stage.child(col().key("controls").w_full().pt(72.0)
                .linear_gradient(0.0, [(0.0, Color::BLACK.alpha(0.82)), (1.0, Color::BLACK.alpha(0.0))])
                .transition(Transition::opacity())
                .child(bar(s)));
        }
    }
    let mut root = row().key("player").w_full().h_full().fill(Color::BLACK)
        .native_surface(&s.video, SurfaceFit::Fit)
        // mpv renders at the video's size in device pixels. Fires on the first layout and on a
        // resize only (a live-only layout handler would keep the window drawing every frame).
        .on_layout(|s: &mut State, r| {
            let k = window::scale();
            s.video_size = ((r[2] * k).round() as u32, (r[3] * k).round() as u32);
        })
        .cursor(if s.controls { Cursor::Default } else { Cursor::Hidden })
        .child(stage);
    if s.subs_open { root = root.child(subtitles(s)) } else if s.quality_open { root = root.child(quality(s)) } else if s.cast_open { root = root.child(tvs(s)) }
    root
}

/// Over the video while a TV plays: which TV, the title, play/pause, a seek bar and Stop.
fn casting(s: &State, c: &cast::Session) -> El {
    let frac = if c.dur > 0.0 { (c.pos / c.dur) as f32 } else { 0.0 };
    let time = |secs: f64| label(clock(secs), 14.0, 500.0, ink(0.85)).font(TextStyle::new(14.0, 500.0).features("'tnum' 1"));
    let track = row().key("cast-seek").grow(1.0).min_w(0.0).h(BAR).center_y().cursor(Cursor::Pointer)
        .on_layout(|s: &mut State, r| s.seek_w = r[2])
        .on_drag(|s: &mut State, d: &mut Drag, _: &mut Tx| {
            let Some(dur) = s.cast.as_ref().map(|c| c.dur).filter(|&d| d > 0.0) else { return };
            if d.phase == Phase::Up && !d.cancelled() { cast::seek(s, (d.x / d.w.max(1.0)).clamp(0.0, 1.0) as f64 * dur) }
        })
        .child(progress(frac, 4.0).w_full());
    let toggle = tap(row().key("cast-toggle").size(64.0, 64.0).radius(32.0).center().fill(ink(0.14)).hover(ink(0.22))
        .child(icon(s, |i| if c.paused { &i.play } else { &i.pause }, 26.0)))
        .on_press(cast::toggle_pause);
    let title = s.meta.as_ref().map(|m| s_(&m["name"]).to_string()).unwrap_or_default();
    all(el()).key("casting").fill(bg()).center().child(col().w(560.0).gap(18.0).items_center()
        .child(icon(s, |i| &i.cast, 44.0).opacity(0.85))
        .child(label(format!("Playing on {}", c.name), 15.0, 600.0, ink(0.6)))
        .child(label(title, 30.0, 700.0, Color::WHITE).truncate())
        .child(el().h(8.0))
        .child(toggle)
        .child(row().w_full().gap(12.0).center_y().child(time(c.pos)).child(track).child(time(c.dur)))
        .child(tap(row().key("cast-stop").h(40.0).px(18.0).mt(6.0).radius(20.0).center().fill(ink(0.08)).hover(ink(0.14))
            .child(label("Stop casting", 15.0, 600.0, Color::WHITE)))
            .on_press(cast::stop)))
}

/// The TVs the streaming server found; a pick plays there from here.
fn tvs(s: &State) -> El {
    let found = cast::tvs(&s.cast_devices);
    let mut list = col();
    if found.is_empty() {
        list = list.child(row().w_full().px(14.0).py(10.0).gap(12.0).center_y().child(spinner().scale(0.5))
            .child(label("Looking for TVs on your network…", 15.0, 500.0, ink(0.7))))
            .child(label("Chromecasts and DLNA TVs on the same Wi-Fi show up here. Stremio's streaming server finds them.", 13.0, 400.0, ink(0.5))
                .font(TextStyle::new(13.0, 400.0).lh(19.0)).wrap().px(14.0));
    }
    for d in found {
        let (id, name) = (s_(&d["id"]).to_string(), d["name"].as_str().unwrap_or(s_(&d["id"])).to_string());
        let kind = if d["type"] == "chromecast" { "Chromecast" } else { "TV (DLNA)" };
        let device = d.clone();
        list = list.child(option(format!("tv-{id}"), name, kind, false).on_press(move |s: &mut State| cast::start(s, device.clone())));
    }
    panel(s, "tvs", "Cast to TV", list)
}

fn bar(s: &State) -> El {
    let p = &s.playback;
    let at = s.scrub.map_or(p.pos, |f| f as f64 * p.dur);
    let frac = if p.dur > 0.0 { (at / p.dur) as f32 } else { 0.0 };
    let round = |key: &str, on: bool| tap(row().key(key.to_string()).size(40.0, 40.0).radius(20.0).center()
        .fill(if on { ink(0.16) } else { Color::TRANSPARENT }).hover(ink(0.14)));
    let toggle = round("toggle", false).child(icon(s, |i| if p.paused { &i.play } else { &i.pause }, 20.0))
        .on_press(|s: &mut State| mpv(s, &["cycle", "pause"]));
    let time = |secs: f64| label(clock(secs), 14.0, 500.0, ink(0.85)).font(TextStyle::new(14.0, 500.0).features("'tnum' 1"));
    // The whole bar height is the drag target; the thin track is just its picture.
    // Hovering it shows the frame at that point (`seek_preview`), as does scrubbing.
    let mut track = row().key("seek").grow(1.0).min_w(0.0).h(BAR).center_y().cursor(Cursor::Pointer).on_drag(seek)
        .on_layout(|s: &mut State, r| s.seek_w = r[2])
        .on_pointer_move_local(|s: &mut State, e, _| {
            let f = (e.x / s.seek_w.max(1.0)).clamp(0.0, 1.0);
            s.seek_hover = Some(f);
            want_preview(s, f);
        })
        .on_hover_change(|s: &mut State, inside, _| if !inside { s.seek_hover = None })
        .child(progress(frac, if s.scrub.is_some() { 6.0 } else { 4.0 }).w_full());
    if let Some(f) = s.scrub.or(s.seek_hover).filter(|_| p.dur > 0.0) { track = track.child(seek_preview(s, f)) }
    let mut bar = row().key("bar").w_full().h(BAR).px(14.0).gap(10.0).center_y()
        .on_hover_change(|s: &mut State, inside, _| s.bar_hover = inside)
        .child(toggle);
    // Mute, and a slider: drag it, or scroll over it.
    let vol = s.vol_drag.unwrap_or(p.volume);
    bar = bar.child(round("mute", false).child(icon(s, |i| if p.muted || p.volume <= 0.0 { &i.muted } else { &i.volume }, 20.0))
            .on_press(|s: &mut State| mpv(s, &["cycle", "mute"])))
        .child(row().key("volume").w(VOL_W).h(BAR).center_y().cursor(Cursor::Pointer).on_drag(volume_drag)
            // one 5% step per wheel event; a trackpad's many small events step fast
            .on_wheel(|s: &mut State, e: rune::WheelEvent, _| {
                if e.dy == 0.0 { return false }
                mpv(s, &["add", "volume", if e.dy > 0.0 { "5" } else { "-5" }]);
                true
            })
            .child(progress(if p.muted { 0.0 } else { (vol / 100.0) as f32 }, if s.vol_drag.is_some() { 6.0 } else { 4.0 }).w_full()))
        .child(el().w(8.0));
    bar = bar.child(time(at)).child(track).child(time(p.dur));
    if !s.streams.is_empty() {
        let now = s.streams.iter().find(|st| st.url == s.playing).map_or("Quality", resolution);
        bar = bar.child(tap(row().key("quality").h(32.0).px(12.0).radius(16.0).center().border(1.0, ink(0.35))
            .fill(if s.quality_open { ink(0.16) } else { Color::TRANSPARENT }).hover(ink(0.14))
            .child(label(now, 13.0, 650.0, Color::WHITE)))
            .on_press(|s: &mut State| { s.quality_open = !s.quality_open; s.subs_open = false; s.cast_open = false }));
    }
    bar.child(round("cast", s.cast_open).child(icon(s, |i| &i.cast, 20.0)).on_press(cast::toggle_list))
        .child(round("subs", s.subs_open).child(icon(s, |i| &i.subs, 20.0))
            .on_press(|s: &mut State| { s.subs_open = !s.subs_open; s.quality_open = false; s.cast_open = false }))
        .child(round("fullscreen", false).child(icon(s, |i| if s.fullscreen { &i.shrink } else { &i.expand }, 18.0))
            .on_press_tx(fullscreen))
}

/// PiP's overlay, as Arc's: it drags the window from anywhere, and on hover shows close at the
/// top left, back to the full window at the top right, ±10 s and play/pause in the middle, and
/// a seek bar along the bottom.
fn pip(s: &State) -> El {
    let o = all(el()).key("pip").window_drag().on_hover_change(|s: &mut State, inside, _| s.bar_hover = inside);
    if !s.controls { return o }
    let p = &s.playback;
    let at = s.scrub.map_or(p.pos, |f| f as f64 * p.dur);
    let frac = if p.dur > 0.0 { (at / p.dur) as f32 } else { 0.0 };
    let round = |key: &str, size: f32| tap(row().key(key.to_string()).size(size, size).radius(size / 2.0).center()
        .fill(Color::BLACK.alpha(0.4)).hover(ink(0.22)));
    o.fill(Color::BLACK.alpha(0.3))
        .child(row().key("pip-middle").pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).center().gap(16.0)
            .child(round("pip-back", 36.0).child(icon(s, |i| &i.rewind, 18.0)).on_press(|s: &mut State| mpv(s, &["seek", "-10"])))
            .child(round("pip-toggle", 48.0).child(icon(s, |i| if p.paused { &i.play } else { &i.pause }, 22.0))
                .on_press(|s: &mut State| mpv(s, &["cycle", "pause"])))
            .child(round("pip-fwd", 36.0).child(icon(s, |i| &i.forward, 18.0)).on_press(|s: &mut State| mpv(s, &["seek", "10"]))))
        .child(round("pip-close", 28.0).pin(Some(8.0), Some(8.0), None, None).child(icon(s, |i| &i.close, 12.0)).on_press(stop))
        .child(round("pip-full", 28.0).pin(None, Some(8.0), Some(8.0), None).child(icon(s, |i| &i.expand, 12.0))
            .on_press(|_: &mut State| window::pip::exit()))
        .child(row().key("pip-seek").pin(Some(12.0), None, Some(12.0), Some(4.0)).h(16.0).center_y().cursor(Cursor::Pointer)
            .on_layout(|s: &mut State, r| s.seek_w = r[2])
            .on_drag(seek)
            .child(progress(frac, if s.scrub.is_some() { 5.0 } else { 3.0 }).w_full()))
}

/// A choice in a side panel: a dot when it's the current one, and an optional second line.
fn option(key: String, words: String, under: &str, on: bool) -> El {
    let mut words = col().grow(1.0).min_w(0.0).gap(2.0)
        .child(label(words, 15.0, if on { 600.0 } else { 500.0 }, if on { Color::WHITE } else { ink(0.8) }).truncate());
    if !under.is_empty() { words = words.child(label(under.to_string(), 12.0, 500.0, ink(0.5)).truncate()) }
    tap(row().key(key).w_full().min_h(44.0).py(6.0).px(14.0).gap(12.0).radius(10.0).center_y()
        .fill(if on { ink(0.12) } else { Color::TRANSPARENT }).hover(ink(0.08))
        .child(el().size(8.0, 8.0).radius(4.0).fill(if on { Color::WHITE } else { Color::TRANSPARENT }))
        .child(words))
}

fn heading(t: &str) -> El { label(t, 12.0, 600.0, ink(0.45)).pt(12.0).px(14.0) }
fn section_title(t: &str) -> El { label(t, 16.0, 700.0, ink(0.9)).pt(16.0).pb(4.0).px(14.0) }

/// A panel over the video's right edge, frosted: the video blurs behind it.
fn panel(s: &State, key: &str, title: &str, list: El) -> El {
    glass(col().key(key.to_string()).w(SUBS_W).h_full().pt(22.0 + top(s)).px(14.0).gap(12.0)).fill(bg().alpha(0.72))
        .transition(Transition::offset(SUBS_W, 0.0).combined(Transition::opacity()))
        .child(row().w_full().px(6.0).center_y()
            .child(label(title.to_string(), 22.0, 700.0, Color::WHITE).grow(1.0))
            .child(tap(row().key("panel-close").size(36.0, 36.0).radius(18.0).center().hover(ink(0.12))
                .child(icon(s, |i| &i.close, 16.0))).on_press(|s: &mut State| { s.subs_open = false; s.quality_open = false; s.cast_open = false })))
        .child(list.w_full().gap(2.0).pb(24.0).scroll(ScrollSpec { stick_bottom: false }).grow(1.0).shrink().min_h(0.0))
}

/// Audio & subtitle choices: the file's audio tracks, then subtitles (off, the file's own
/// tracks, then the addons').
fn subtitles(s: &State) -> El {
    let p = &s.playback;
    let mut list = col();
    if !p.audio.is_empty() { list = list.child(section_title("Audio")) }
    for t in &p.audio {
        let id = t.id.clone();
        list = list.child(option(format!("aud-{id}"), t.label.clone(), "", p.aid == t.id).on_press(move |s: &mut State| mpv(s, &["set", "aid", &id])));
    }
    list = list.child(section_title("Subtitles"))
        .child(option("sub-off".into(), "Off".into(), "", p.sid.is_empty() || p.sid == "no").on_press(|s: &mut State| mpv(s, &["set", "sid", "no"])));
    let embedded: Vec<_> = p.tracks.iter().filter(|t| t.external.is_empty()).collect();
    if !embedded.is_empty() { list = list.child(heading("IN THIS VIDEO")) }
    for t in embedded {
        let id = t.id.clone();
        list = list.child(option(format!("sub-{id}"), t.label.clone(), "", p.sid == t.id).on_press(move |s: &mut State| mpv(s, &["set", "sid", &id])));
    }
    if !s.addon_subs.is_empty() { list = list.child(heading("FROM YOUR ADDONS")) }
    let mut subs: Vec<(usize, &(String, String))> = s.addon_subs.iter().enumerate().collect();
    subs.sort_by_key(|(_, (lang, _))| language(lang));
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (i, (lang, url)) in subs {
        let n = seen.entry(language(lang)).and_modify(|n| *n += 1).or_insert(1);
        let on = p.tracks.iter().any(|t| t.external == *url && t.id == p.sid);
        let (l, u) = (lang.clone(), url.clone());
        list = list.child(option(format!("asub-{i}"), format!("{} {n}", language(lang)), "", on).on_press(move |s: &mut State| add_sub(s, &l, &u)));
    }
    panel(s, "subtitles", "Audio & Subtitles", list)
}

/// The video's streams by resolution, each with its source line; a pick switches to it at the
/// same position.
fn quality(s: &State) -> El {
    let mut list = col();
    for (res, group) in qualities(&s.streams) {
        list = list.child(section_title(res));
        for (i, st) in group {
            let url = st.url.clone();
            let source = st.title.lines().next().unwrap_or("");
            list = list.child(option(format!("q-{i}"), st.name.clone(), source, st.url == s.playing)
                .on_press(move |s: &mut State| switch_stream(s, &url)));
        }
    }
    panel(s, "quality", "Quality", list)
}
