# Cover Hover + Leftover Catalog Pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cover hover (Play / My Collection / More) on shared cards, plus real Explore, Feed, Save for Later, and Home Staff Picks / Uploads pages; Videos stays a stub.

**Architecture:** Reuse `card_tracks_action`, `set_card_favorite`, and `request-card-menu`. Extract a shared modules renderer from Home. New `tide-api` parsers (schema-tolerant) for v2 home feeds, v1 pages, feed activities, and later items. Collection IDs cached from `favorites/ids` so hearts do not fetch on hover.

**Tech Stack:** Rust, Slint, `tide-api` untyped JSON, `cargo test -p tide-api` / `tide-app`.

## Global Constraints

- 100% Rust / Slint; no video decoder
- Heart = My Collection, not Save for Later
- Mix: Play + … only; artist overlay: Play + … (Follow stays on artist page); folders: no overlay
- Cover click (not a button) still opens; right-click still opens the same menu
- Home tab change does not add history
- Videos stub unchanged; sidebar unchanged
- Cache kv: `explore`, `feed`, `later`, `home:staff`, `home:uploads`
- Empty copy: `Loading…` / `Nothing here yet`
- Do **not** git commit or push (user instruction)

## File map

| File | Responsibility |
| --- | --- |
| `crates/tide-api/src/web.rs` | `pub(crate)` parse helpers; `home_feed(static\|editorial\|uploads)`; `favorite_ids()` |
| `crates/tide-api/src/catalog.rs` | v1 pages (`explore`, nested `catalog_page`); feed activities + time buckets; later parser |
| `crates/tide-api/src/lib.rs` | `mod catalog`; re-export `LaterItems`, `feed_group` |
| `crates/tide-app/ui/common.slint` | `CardData.favorite`; `play-card` / `toggle-card-favorite` / `set-home-tab`; playing-card ids; `home-tab` |
| `crates/tide-app/ui/views.slint` | `CoverHover` on `MediaCard`/`RowCard`; `RowTrack` play+heart; extract `ModulesView`; Home tabs |
| `crates/tide-app/ui/feed.slint` | Time-grouped Feed list |
| `crates/tide-app/ui/app.slint` | `modules` + `feed` views |
| `crates/tide-app/src/controller.rs` | `Page::{Explore,Feed,Later,Catalog}`; play/favorite helpers; collection cache |
| `Goal.md` | Mark leftover pages + cover hover |

Live probe (2026-11-12, session present):

- Explore: `GET https://api.tidal.com/v1/pages/explore` (`rows[].modules[]`)
- Nested e.g. `pages/genre_hip_hop`, `pages/m_1950s`
- Home For you: existing `{WEB_V2}/home/feed/static`
- Staff Picks: `{WEB_V2}/home/feed/editorial`
- Uploads: `{WEB_V2}/home/feed/uploads`
- Feed: `GET https://api.tidal.com/v2/feed/activities` (`activities[].followableActivity`)
- Save for Later: probed URLs 404/400; parser still lands against typical mixed `items[]` and tries those URLs

---

### Task 1: API parsers (TDD)

**Files:**
- Modify: `crates/tide-api/src/web.rs` (`parse_card`, `home`, helpers)
- Create: `crates/tide-api/src/catalog.rs`
- Modify: `crates/tide-api/src/lib.rs`

**Interfaces:**
- Produces: `ApiHandle::home_feed(&self, feed: &str) -> Result<Vec<Section>, ApiError>` with `feed` in `static|editorial|uploads`
- Produces: `ApiHandle::explore(&self) -> Result<Vec<Section>, ApiError>`
- Produces: `ApiHandle::catalog_page(&self, path: &str) -> Result<Vec<Section>, ApiError>` (`path` like `pages/genre_hip_hop`)
- Produces: `ApiHandle::feed(&self) -> Result<Vec<Section>, ApiError>` (grouped cards)
- Produces: `ApiHandle::later(&self) -> Result<LaterItems, ApiError>`
- Produces: `pub struct LaterItems { pub cards: Vec<Card>, pub tracks: Vec<TrackItem> }`
- Produces: `pub fn feed_group(iso: &str) -> &'static str` → `"Last week" \| "This month" \| "Last month" \| "Older"`
- Produces: `ApiHandle::favorite_ids(&self) -> Result<FavoriteIds, ApiError>`
- Produces: `pub struct FavoriteIds { pub albums: HashSet<String>, pub playlists: HashSet<String> }`
- Consumes: existing `Section`, `Card`, `parse_track`, `parse_card`

- [ ] **Step 1: Write failing tests** in `web.rs` `tests` and `catalog.rs` `tests`

```rust
#[test]
fn feed_group_matches_official_buckets() {
    // Pin "today" via feed_group_on; Nov 12, 2026.
    let t = (2026, 11, 12);
    assert_eq!(feed_group_on("2026-11-06T00:00:00Z", t), "Last week");
    assert_eq!(feed_group_on("2026-11-01T00:00:00Z", t), "This month");
    assert_eq!(feed_group_on("2026-10-31T00:00:00Z", t), "Last month");
    assert_eq!(feed_group_on("2026-08-01T00:00:00Z", t), "Older");
}

#[test]
fn v1_page_parses_album_list_and_genre_links() { /* json! fixture → sections */ }

#[test]
fn feed_parses_album_and_history_mix() { /* NEW_ALBUM_RELEASE + NEW_HISTORY_MIX */ }

#[test]
fn later_parses_mixed_items() { /* type track/album/playlist wrappers */ }
```

- [ ] **Step 2: Run tests, expect FAIL**

Run: `cargo test -p tide-api -- feed_group_matches_official_buckets v1_page_parses feed_parses later_parses`

- [ ] **Step 3: Implement parsers**

`home_feed`: same as today’s `home()`, URL `{WEB_V2}/home/feed/{feed}`. `home()` calls `home_feed("static")`.

v1 page: walk `rows[].modules[]`. Skip `VIDEO_LIST` / `TEXT_BLOCK`. `ALBUM_LIST`/`PLAYLIST_LIST`/`ARTIST_LIST`/`MIX_LIST` → cards via `parse_card`. `TRACK_LIST` → tracks. `FEATURED_PROMOTIONS` ALBUM → album card from `artifactId`/`shortHeader`/`imageId`; skip `EXTURL`. `PAGE_LINKS*` → `Card { kind: "page", id: apiPath, title }`, layout `"links"`.

Feed grouping: age ≤ 10 days → Last week; else same calendar month → This month; else previous month → Last month; else Older. Album subtitle `Single by …` / `EP by …` / `Album - …`. History mix → `parse_card("MIX", historyMix)`.

Later: try `GET {V1}/users/{id}/favorites/later` then `GET {V2}/favorites/later`; parse mixed `items[]` (`item` wrap or JSON:API `data`); skip videos; `Ok(empty)` if every URL fails.

`favorite_ids`: existing `GET {V1}/users/{id}/favorites/ids`, collect `ALBUM` + `PLAYLIST` string/number ids.

- [ ] **Step 4: Run `cargo test -p tide-api` — expect PASS**

- [ ] **Step 5: Commit** — skipped (user: do not commit)

---

### Task 2: Cover hover overlay

**Files:**
- Modify: `crates/tide-app/ui/common.slint` (`CardData`, App callbacks/properties)
- Modify: `crates/tide-app/ui/views.slint` (`MediaCard`, `RowCard`, `RowTrack`)

**Interfaces:**
- Consumes: `CardData`, existing `request-card-menu`, `toggle-play`, `play-section-track`
- Produces: `CardData.favorite: bool`
- Produces: `callback play-card(CardData); callback toggle-card-favorite(CardData);`
- Produces: `in property <string> playing-card-id; in property <string> playing-card-kind; in-out property <int> home-tab; callback set-home-tab(int);`

- [ ] **Step 1: Extend `CardData` + App**

```slint
export struct CardData {
    // existing fields...
    favorite: bool,
}

callback play-card(CardData);
callback toggle-card-favorite(CardData);
in property <string> playing-card-id: "";
in property <string> playing-card-kind: "";
in-out property <int> home-tab: 0;
callback set-home-tab(int);
```

- [ ] **Step 2: `CoverHover` on artwork** (not folders). Dark wash `#00000099`. Play bottom-left (Pause + `toggle-play` if `playing-card-id/kind` match and `App.playing`; else `play-card`). Heart bottom-right if album, or playlist with `by != "Me"` and `by != ""`. More next to heart for album/playlist/mix/artist via `request-card-menu`. Tracks tile: Play only. Buttons consume clicks; parent cover click still `open-card`. Drop opacity-0.8 hover.

- [ ] **Step 3: `RowCard`** — same overlay scaled to `art`. `RowTrack` — Play on cover; heart (`toggle-favorite`) + `TrackMenuButton` on the row. Leave `TrackRow` table chrome as-is.

- [ ] **Step 4: `cargo test -p tide-app` still compiles/passes** (callbacks default no-op until Task 3)

- [ ] **Step 5: Commit** — skipped

---

### Task 3: play-card / favorite cache / controller helpers

**Files:**
- Modify: `crates/tide-app/src/controller.rs` (`card_row`, `State`, `wire`, `play_queue`, `card_tracks_action`, new helpers)

**Interfaces:**
- Consumes: `play-card`, `toggle-card-favorite`, `ApiHandle::favorite_ids`, `set_card_favorite`, `card_tracks_action`
- Produces: `State { collection_albums, collection_playlists, playing_card: Option<(String,String)> }`
- Produces: `fn play_card(&Rc<Self>, CardData)`; `fn toggle_card_favorite(&Rc<Self>, CardData)`

- [ ] **Step 1: `card_row` copies `favorite` from collection sets (album/playlist id).**

- [ ] **Step 2: `play_card`** — if this card is `playing_card` → `toggle_play`; `kind=="tracks"` → play favorites; `kind=="page"|"folder"` → no-op; else existing `card_tracks_action("play")`. `card_tracks_action("play")` sets `playing_card`. `play_from_page` / section clears or sets from page card. `present_current` pushes ids to App.

- [ ] **Step 3: `toggle_card_favorite`** — `can_favorite` only; optimistic set + `set_card_favorite`; patch collection HashSet; refresh `favorite` on visible card models; same notices as the menu.

- [ ] **Step 4: `sync_library` also fetches `favorite_ids`.** Wire `on_play_card` / `on_toggle_card_favorite`.

- [ ] **Step 5: Unit test `same_page` still compiles; `cargo test -p tide-app`**

- [ ] **Step 6: Commit** — skipped

---

### Task 4: Modules view, Home tabs, Explore

**Files:**
- Modify: `crates/tide-app/ui/views.slint` (extract `ModulesView`, Home tabs, `ModulesPage`)
- Modify: `crates/tide-app/ui/app.slint` (`view == "modules"`)
- Modify: `crates/tide-app/src/controller.rs` (`Page::Explore`, `Page::Catalog`, `set_home_tab`, `present`)

**Interfaces:**
- Consumes: `ApiHandle::home_feed`, `explore`, `catalog_page`; `App.home-tab` / `set-home-tab`
- Produces: `export component ModulesView`; Home tabs call `set-home-tab` (no `navigate`); `Page::Explore`; `Page::Catalog { path, title }`; `open_card` kind `page` → Catalog

- [ ] **Step 1: Extract section loop from `HomeView` into `ModulesView`.** Add `layout=="links"` chip row (title pills; click `open-card`). Empty: `Loading…` / `Nothing here yet`.

- [ ] **Step 2: Home tabs For you / Staff Picks / Uploads use `App.home-tab`; underline selected; `clicked => App.set-home-tab(i)`.** `set_home_tab` only if current page is Home; fetch `static|editorial|uploads` into `home` / `home:staff` / `home:uploads`; do not `show()`.

- [ ] **Step 3: `Page::Explore` presents `view="modules"`, `nav-key=explore`, load `explore` kv. Nested `kind=="page"` → `Page::Catalog`. Videos nav still stub.**

- [ ] **Step 4: `cargo test -p tide-app`**

- [ ] **Step 5: Commit** — skipped

---

### Task 5: Feed page

**Files:**
- Create: `crates/tide-app/ui/feed.slint`
- Modify: `crates/tide-app/ui/app.slint` (import + `view=="feed"`)
- Modify: `crates/tide-app/src/controller.rs` (`Page::Feed`)

**Interfaces:**
- Consumes: `ApiHandle::feed` → `Vec<Section>` already grouped; `RowCard` overlay; `open-card` / `play-card`
- Produces: `export component FeedView`

- [ ] **Step 1: `FeedView` — title Feed; for each section a time header + compact `RowCard`s; empty/loading copy.**

- [ ] **Step 2: `Page::Feed` → `view=feed`, `nav-key=feed`, cache `feed`.**

- [ ] **Step 3: `cargo test -p tide-app`**

- [ ] **Step 4: Commit** — skipped

---

### Task 6: Save for Later + Goal.md

**Files:**
- Modify: `crates/tide-app/src/controller.rs` (`Page::Later`, `present`)
- Modify: `Goal.md`

**Interfaces:**
- Consumes: `LaterItems`; `TracksView` / `GridView` / `ModulesView`
- Produces: Later page; Videos still `Page::Stub`

- [ ] **Step 1: `Page::Later`.** If only tracks → `TracksView`. If only cards → `GridView` (`grid-kind=later`, no create). If mixed → `view=modules` with a tracks section and a cards section. Cache `later`. Empty: `Nothing here yet`. Probe miss → empty list, not stub copy.

- [ ] **Step 2: Goal.md — Explore / Feed / Save for Later done; Videos still placeholder; mention cover hover.**

- [ ] **Step 3: `cargo test -p tide-app -p tide-api`**

- [ ] **Step 4: Commit** — skipped

---

## Self-review

1. Spec coverage: hover overlay + callbacks + favorite cache; Home tabs without history; Explore modules + nested pages; Feed buckets; Later mixed; Videos stub; cache keys; empty copy; tests.
2. Placeholders: none. Later live URL may 404 — parser + empty state is the spec fallback.
3. Types: `home_feed(&str)`, `LaterItems`, `feed_group_on`, `CardData.favorite`, `play-card` / `toggle-card-favorite`, `Page::{Explore,Feed,Later,Catalog}` stay consistent across tasks.
