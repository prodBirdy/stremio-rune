//! Search, the way Stremio does it: every addon catalog with a `search` extra is queried with
//! the typed text, and each answers into its own group. A child of `ui` (see the `#[path]` there)
//! so the view can use its private helpers.

use super::*;
use rune::Latest;

/// Wait this long after the last keystroke before asking the addons.
const DEBOUNCE: Duration = Duration::from_millis(300);

pub struct Search {
    pub query: TextEdit,
    /// The addons' search catalogs, with their group titles as `name`.
    pub cats: Vec<Catalog>,
    /// Answers for the current query, by index into `cats`.
    pub results: HashMap<usize, Vec<Value>>,
    /// Debounces keystrokes; lands the query to send once typing pauses.
    pub debounce: Latest<String>,
}

impl Default for Search {
    fn default() -> Self {
        Search { query: TextEdit::new(""), cats: vec![], results: HashMap::new(), debounce: Latest::default().debounce(DEBOUNCE) }
    }
}

/// Catalogs that take a `search` extra and need nothing else. The first per type is titled
/// "Movies"/"Shows"; others carry their addon's name.
pub fn catalogs(addons: &[Value]) -> Vec<Catalog> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for a in addons {
        for c in a["manifest"]["catalogs"].as_array().into_iter().flatten() {
            let extra: Vec<&Value> = c["extra"].as_array().into_iter().flatten().collect();
            let required = extra.iter().filter(|e| e["isRequired"] == true).map(|e| s_(&e["name"]))
                .chain(c["extraRequired"].as_array().into_iter().flatten().map(s_));
            let searchable = extra.iter().any(|e| e["name"] == "search") || c["extraSupported"].as_array().is_some_and(|x| x.iter().any(|e| e == "search"));
            if !searchable || required.into_iter().any(|r| r != "search") { continue }
            let ty = s_(&c["type"]).to_string();
            let name = if seen.insert(ty.clone()) { type_label(&ty).to_string() }
                else { format!("{} · {}", s_(&a["manifest"]["name"]), type_label(&ty)) };
            out.push(Catalog { base: base(a), id: s_(&c["id"]).into(), ty, name, ..Default::default() });
        }
    }
    out
}

/// The field changed: search again once typing pauses; an empty field clears at once.
pub fn typed(s: &mut State, _: &mut Tx) {
    let q = s.search.query.text.trim().to_string();
    if q.is_empty() {
        s.search.debounce.cancel();
        s.search.results.clear();
    } else {
        s.search.debounce.request(q, |q| q);
    }
}

/// Ask every search catalog. Answers for older text are dropped in `landed`.
pub fn run(s: &mut State, q: String) {
    s.search.results.clear();
    let enc: String = q.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect();
    for (i, c) in s.search.cats.iter().enumerate() {
        let (url, q) = (format!("{}/catalog/{}/{}/search={enc}.json", c.base, c.ty, c.id), q.clone());
        // Quiet on failure: one addon down shouldn't toast over the others' results.
        spawn(s, move || Ok(Msg::Search(q, i, get(&url).map(|v| v["metas"].as_array().cloned().unwrap_or_default()).unwrap_or_default())));
    }
}

pub fn landed(s: &mut State, q: String, i: usize, metas: Vec<Value>) {
    if q != s.search.query.text.trim() { return } // typed on since
    s.search.results.insert(i, metas); // posters load lazily as their cards lay out
}

pub fn open(s: &mut State) {
    s.page = Page::Search;
    s.menu = false;
    s.search.query.focused = true;
}

/// Search for `q` at once, as if typed: a person's name from a detail page finds their titles
/// (addons built on TMDB match people as well as titles; Cinemeta, some of them).
pub fn find(s: &mut State, q: &str) {
    open(s);
    s.search.query = TextEdit::new(q);
    s.search.query.focused = true;
    s.search.debounce.cancel();
    run(s, q.to_string());
}

fn hint(words: String) -> El {
    label(words, 17.0, 500.0, ink(0.5)).pt(12.0)
}

pub fn view(s: &State) -> El {
    let field = glass(text_input(&s.search.query, TextStyle::new(28.0, 500.0)).key("search-field").single_line()
        .w_full().px(24.0).py(18.0).radius(18.0)).color(Color::WHITE)
        .placeholder("Search movies and shows".to_string(), ink(0.4))
        .on_edit(|s: &mut State| &mut s.search.query).on_input_change(typed)
        // The editor would only blur on Esc; one press should leave search.
        .on_input_key(|s: &mut State, ev: &KeyEvent, _: &mut Tx| { if ev.key != Key::Escape { return false } escape(s); true });
    let mut page = col().w_full().pt(104.0).px(PAD).pb(56.0).gap(28.0).child(field);
    let q = s.search.query.text.trim();
    let r = &s.search.results;
    let done = r.len() >= s.search.cats.len();
    if q.is_empty() { return page.child(hint("Find a movie or show by its title, or an actor's by their name.".into())) }
    if s.search.cats.is_empty() { return page.child(hint("None of your addons offer search.".into())) }
    for (i, c) in s.search.cats.iter().enumerate() {
        let Some(items) = r.get(&i).filter(|v| !v.is_empty()) else { continue };
        let mut grid = row().w_full().equal_columns(7).gap(22.0);
        for m in items { grid = grid.child(poster_card(s, m, format!("s{i}-{}", s_(&m["id"])))) }
        page = page.child(col().w_full().gap(16.0).child(label(c.name.clone(), 24.0, 600.0, Color::WHITE)).child(grid));
    }
    if !done { page = page.child(hint("Searching…".into())) }
    else if r.values().all(|v| v.is_empty()) { page = page.child(hint(format!("No results for “{q}”."))) }
    page
}
