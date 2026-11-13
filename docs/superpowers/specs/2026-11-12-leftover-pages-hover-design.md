# Design Spec: Cover hover + leftover catalog pages

Date: 2026-11-12
Status: approved
Repo: `tidal-rust` (TIDE)

## Overview

Finish leftover catalog surfaces like TIDAL Desktop:

1. Cover hover on every card: Play, Add/Remove My Collection, More.
2. Real pages for Explore, Feed, Save for Later, and Home Staff Picks / Uploads tabs.
3. Videos stay the current stub. No video decoder.

Play now, collection toggle, and context menu already exist. Put those actions on the artwork and fill stub routes.

## Locked decisions

- Hover: Play + heart + …
- Heart = My Collection, not Save for Later
- Videos = stub; sidebar unchanged
- Mix cards: Play + …, no heart
- Artist portraits: Play (top tracks) + …; Follow stays on artist page
- Folders: no overlay, no menu
- Cover click (not a button) still opens the page
- Right-click still opens the same menu

## 1. Cover hover (MediaCard, RowCard, RowTrack)

Today `MediaCard` only sets opacity 0.8 on hover.

Hover: dark wash; Play bottom-left (`card_tracks_action("play")` / `play-section-track` / `play-track`; if this card is playing, Pause + `toggle-play`); Heart bottom-right (`set_card_favorite`); … next to heart (`request-card-menu` / `TrackMenuButton`).

- `RowCard`: same three actions scaled down.
- `RowTrack`: Play on cover; heart + … on the row.
- Leave `TrackRow` table chrome as-is.

Add `favorite: bool` to `CardData`. Cache collection IDs so the heart is filled without a per-hover fetch.

New App callbacks: `play-card(CardData)`, `toggle-card-favorite(CardData)`.

### Overlay rules

| Kind | Overlay | Play | Heart | More |
| --- | --- | --- | --- | --- |
| album | yes | album tracks | My Collection | card menu |
| playlist (not mine) | yes | playlist tracks | My Collection | card menu |
| playlist (mine) | yes | playlist tracks | hidden | card menu |
| mix | yes | mix tracks | hidden | card menu |
| artist | yes | top tracks | hidden (Follow stays on artist page) | card menu |
| folder | none | — | — | none |
| tracks tile | yes | play favorites | hidden | hidden |

Cover click (not a button) still opens the page. Overlay buttons consume the click. Right-click on the card still opens the same context menu as today.

If the card currently backing the play queue is this card and playback is running, Play becomes Pause and calls `toggle-play`. If it is this card and paused, Play resumes via `toggle-play`. Otherwise Play loads tracks through the existing `card_tracks_action("play")` path.

Heart fill comes from cached collection IDs (`/users/{me}/favorites/ids`), not a fetch on hover. Toggle updates the cache, the card's `favorite` flag, and the existing notice copy ("Added to My Collection" / "Removed from My Collection").

## 2. Pages

- **Home For you:** keep; hover comes from shared cards.
- **Home Staff Picks / Uploads:** real tabs, same module renderer, different fetch; tab change does **not** add history.
- **Explore:** replace `Page::Stub`; module carousels like Home (reuse/extract modules view, don't copy `HomeView`).
- **Feed:** time-grouped list Last week / This month / Last month / Older (official TIDAL Feed); compact rows; hover overlay; click opens album/playlist; Play starts it.
- **Save for Later:** probe with `TIDE --get` first; mixed bookmarks; reuse `TracksView` and/or `GridView` (or the shared modules view when the payload mixes tracks and cards).
- **Videos:** UNCHANGED stub.

Cache kv like home: `explore`, `feed`, `later`, `home:staff`, `home:uploads`.

Empty states: `Loading…` / `Nothing here yet`.

### Home tabs

`App.home-tab`: 0 For you, 1 Staff Picks, 2 Uploads. Switching tabs while already on Home refetches/presents that feed in place and does not push history. Visiting Home from elsewhere still lands on For you.

### Feed grouping

Buckets relative to local today (not the rolling `relative_date` labels):

- **Last week** — 0–10 days old
- **This month** — same calendar month, older than last week
- **Last month** — previous calendar month
- **Older** — everything else

Empty buckets are omitted. Rows are compact (`RowCard` scale): cover, title, subtitle (`Single by …` / `Album - …` / playlist creator line). Hover uses the shared overlay. Click opens the card. Play starts it.

## 3. API

Probe live JSON with `TIDE --get` before hard-coding URLs. Schema-tolerant like `web.rs`. New files under `crates/tide-api/` so overlay vs API don't conflict.

Reuse `home()`'s module-list parser for v2 static feeds. Add a v1 `pages/{name}` parser (`rows[].modules[]` with `pagedList.items`) for Explore and as a fallback for staff/uploads.

Feed: v2 `feed/activities` (album/playlist/mix activity with `occurredAt`), schema-tolerant.

Save for Later: try user-collection later endpoints; parse mixed track/album/playlist items; skip videos.

Favorite-id cache: one GET of `/users/{me}/favorites/ids` (already used by `is_favorite`). Store album and playlist id sets on the controller; stamp `CardData.favorite` when building rows.

## 4. Multi-agent file ownership (if spawning helpers)

Wave 0 parallel: Overlay owns `views.slint` cards + `common.slint` `CardData`/callbacks + `play-card`/`toggle-card-favorite` helpers. API owns **new** `tide-api` modules.

Wave 1 parallel: Modules (Explore + Home tabs), Feed (new `ui/feed.slint`), Later, Orchestrator (`Page` enum, `navigate_key`, `present`, `app.slint`, `Goal.md`). Orchestrator is the only one replacing `Page::Stub` for explore/feed/later. Videos stub stays.

## 5. Testing

`cargo test -p tide-app` and `tide-api`.

- Hover three controls; cover click opens; play/heart/… hit existing callbacks
- Stub copy gone for Explore/Feed/Later; Videos still stub
- Home tabs no extra history
- Parsers: fixture JSON for explore modules, feed activities, later items, feed time buckets
- `CardData.favorite` round-trips in `card_row`

## 6. Out of scope

- Video playback
- Save for Later on album More menu
- Hiding Videos nav
- Shuffle on cover
