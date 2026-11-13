//! Explore, Feed, Save for Later, and v1 catalog pages.
//!
//! Schema-tolerant, same spirit as [`crate::web`]: a missing field drops one
//! card, not the whole page.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::web::{
    ApiHandle, Card, Section, V1, V2, artist_names, days_from_civil, parse_card, parse_track, s,
    uuid_origin_url,
};
use crate::{ApiError, TrackItem};

/// Bookmarks on Save for Later: albums/playlists/mixes as cards, tracks as tracks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaterItems {
    pub cards: Vec<Card>,
    pub tracks: Vec<TrackItem>,
}

fn ymd(iso: &str) -> Option<(i64, i64, i64)> {
    let mut parts = iso
        .get(..10)
        .unwrap_or("")
        .split('-')
        .filter_map(|p| p.parse::<i64>().ok());
    Some((parts.next()?, parts.next()?, parts.next()?))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Inverse of days_from_civil (Howard Hinnant).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn today_ymd() -> (i64, i64, i64) {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    civil_from_days(days)
}

/// Official Feed buckets relative to local today.
pub fn feed_group(iso: &str) -> &'static str {
    feed_group_on(iso, today_ymd())
}

/// `today` is `(year, month, day)` so tests can pin the clock.
pub fn feed_group_on(iso: &str, today: (i64, i64, i64)) -> &'static str {
    let Some(date) = ymd(iso) else {
        return "Older";
    };
    let age = days_from_civil(today.0, today.1, today.2) - days_from_civil(date.0, date.1, date.2);
    if (0..=10).contains(&age) {
        return "Last week";
    }
    if date.0 == today.0 && date.1 == today.1 {
        return "This month";
    }
    let (py, pm) = if today.1 == 1 {
        (today.0 - 1, 12)
    } else {
        (today.0, today.1 - 1)
    };
    if date.0 == py && date.1 == pm {
        "Last month"
    } else {
        "Older"
    }
}

fn safe_page_path(path: &str) -> Option<&str> {
    let p = path.trim().trim_start_matches('/');
    let p = p.split('?').next()?.trim_end_matches('/');
    (p.starts_with("pages/")
        && !p.contains("..")
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-')))
    .then_some(p)
}

fn page_items(module: &Value) -> impl Iterator<Item = &Value> {
    module
        .get("items")
        .or_else(|| module.pointer("/pagedList/items"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn page_link_card(it: &Value) -> Option<Card> {
    let path = s(it, "apiPath");
    let path = safe_page_path(&path)?;
    let title = s(it, "title");
    if title.is_empty() {
        return None;
    }
    Some(Card {
        id: path.into(),
        kind: "page".into(),
        title,
        image_url: uuid_origin_url(&s(it, "imageId")),
        ..Default::default()
    })
}

fn featured_card(it: &Value, me: u64) -> Option<Card> {
    match s(it, "type").as_str() {
        "ALBUM" => {
            let id = s(it, "artifactId");
            if id.is_empty() {
                return None;
            }
            Some(Card {
                id,
                kind: "album".into(),
                title: s(it, "shortHeader"),
                subtitle: s(it, "shortSubHeader"),
                image_url: uuid_origin_url(&s(it, "imageId")),
                ..Default::default()
            })
        }
        "CATEGORY_PAGES" => {
            let path = s(it, "artifactId");
            let path = safe_page_path(&path)?;
            Some(Card {
                id: path.into(),
                kind: "page".into(),
                title: s(it, "shortHeader"),
                subtitle: s(it, "shortSubHeader"),
                image_url: uuid_origin_url(&s(it, "imageId")),
                ..Default::default()
            })
        }
        // Magazine / external promos are not a page we can open.
        "EXTURL" => None,
        _ => parse_card(&s(it, "type"), it.get("data").unwrap_or(it), me),
    }
}

fn album_list_kind(module_type: &str) -> Option<&'static str> {
    match module_type {
        "ALBUM_LIST" => Some("ALBUM"),
        "PLAYLIST_LIST" => Some("PLAYLIST"),
        "ARTIST_LIST" => Some("ARTIST"),
        "MIX_LIST" => Some("MIX"),
        _ => None,
    }
}

fn section_from_v1_module(module: &Value, me: u64) -> Option<Section> {
    let kind = s(module, "type");
    if matches!(kind.as_str(), "VIDEO_LIST" | "TEXT_BLOCK" | "") {
        return None;
    }
    let title = s(module, "title");
    let mut section = Section {
        title,
        layout: match kind.as_str() {
            "TRACK_LIST" => "tracks",
            "PAGE_LINKS" | "PAGE_LINKS_CLOUD" => "tiles",
            "SHORTCUT_LIST" => "shortcuts",
            _ => "cards",
        }
        .into(),
        ..Default::default()
    };
    match kind.as_str() {
        "FEATURED_PROMOTIONS" => {
            for it in page_items(module) {
                section.cards.extend(featured_card(it, me));
            }
        }
        "PAGE_LINKS" | "PAGE_LINKS_CLOUD" => {
            for it in page_items(module) {
                section.cards.extend(page_link_card(it));
            }
            // Untitled New/Top/Videos chips have no art and no useful layout.
            if section.title.is_empty()
                && section
                    .cards
                    .iter()
                    .all(|c| c.image_url.as_deref().unwrap_or("").is_empty())
            {
                return None;
            }
        }
        "TRACK_LIST" => {
            for it in page_items(module) {
                let data = it.get("item").or_else(|| it.get("data")).unwrap_or(it);
                section.tracks.extend(parse_track(data));
            }
        }
        other => {
            if let Some(card_kind) = album_list_kind(other) {
                for it in page_items(module) {
                    let data = it.get("item").or_else(|| it.get("data")).unwrap_or(it);
                    section.cards.extend(parse_card(card_kind, data, me));
                }
            } else {
                // Same wrapper as the v2 home feed: `{ type, data }`.
                for it in page_items(module) {
                    let (item_kind, data) = (s(it, "type"), it.get("data").unwrap_or(it));
                    if item_kind == "TRACK" {
                        section.tracks.extend(parse_track(data));
                    } else if !item_kind.is_empty() {
                        section.cards.extend(parse_card(&item_kind, data, me));
                    }
                }
            }
        }
    }
    (!section.cards.is_empty() || !section.tracks.is_empty()).then_some(section)
}

pub(crate) fn sections_from_v1_page(v: &Value, me: u64) -> Vec<Section> {
    v.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|row| {
            row.get("modules")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|m| section_from_v1_module(m, me))
        .collect()
}

fn feed_album_subtitle(album: &Value) -> String {
    let artists = artist_names(album);
    match s(album, "type").to_uppercase().as_str() {
        "SINGLE" => format!("Single by {artists}"),
        "EP" => format!("EP by {artists}"),
        _ => format!("Album - {artists}"),
    }
}

fn feed_card(activity: &Value, me: u64) -> Option<Card> {
    if let Some(album) = activity.get("album").filter(|a| a.is_object()) {
        let mut card = parse_card("ALBUM", album, me)?;
        card.subtitle = feed_album_subtitle(album);
        return Some(card);
    }
    if let Some(mix) = activity.get("historyMix").filter(|a| a.is_object()) {
        return parse_card("MIX", mix, me);
    }
    if let Some(playlist) = activity.get("playlist").filter(|a| a.is_object()) {
        return parse_card("PLAYLIST", playlist, me);
    }
    None
}

pub(crate) fn sections_from_feed(v: &Value, me: u64) -> Vec<Section> {
    sections_from_feed_on(v, me, today_ymd())
}

fn sections_from_feed_on(v: &Value, me: u64, today: (i64, i64, i64)) -> Vec<Section> {
    const GROUPS: [&str; 4] = ["Last week", "This month", "Last month", "Older"];
    let mut buckets: [Vec<Card>; 4] = Default::default();
    for act in v
        .get("activities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let fa = act.get("followableActivity").unwrap_or(act);
        let when = s(fa, "occurredAt");
        let Some(mut card) = feed_card(fa, me) else {
            continue;
        };
        card.added = when.clone();
        let group = feed_group_on(&when, today);
        if let Some(i) = GROUPS.iter().position(|g| *g == group) {
            buckets[i].push(card);
        }
    }
    GROUPS
        .iter()
        .zip(buckets)
        .filter(|(_, cards)| !cards.is_empty())
        .map(|(title, cards)| Section {
            title: (*title).into(),
            layout: "rows".into(),
            cards,
            tracks: Vec::new(),
        })
        .collect()
}

fn parse_later_entry(entry: &Value, me: u64, out: &mut LaterItems) {
    let kind = {
        let t = s(entry, "type");
        if t.is_empty() {
            s(entry, "itemType")
        } else {
            t
        }
    }
    .to_uppercase();
    let data = entry
        .get("item")
        .or_else(|| entry.get("data"))
        .unwrap_or(entry);
    match kind.as_str() {
        "TRACK" | "TRACKS" => out.tracks.extend(parse_track(data)),
        "VIDEO" | "VIDEOS" => {}
        other => {
            let card_kind = match other {
                "ALBUM" | "ALBUMS" => "ALBUM",
                "PLAYLIST" | "PLAYLISTS" => "PLAYLIST",
                "MIX" | "MIXES" => "MIX",
                "ARTIST" | "ARTISTS" => "ARTIST",
                _ => return,
            };
            out.cards
                .extend(parse_card(card_kind, data, me).map(|mut c| {
                    c.added = s(entry, "created");
                    if c.added.is_empty() {
                        c.added = s(entry, "addedAt");
                    }
                    c
                }));
        }
    }
}

pub(crate) fn parse_later(v: &Value, me: u64) -> LaterItems {
    let mut out = LaterItems::default();
    let items = v
        .get("items")
        .or_else(|| v.get("data"))
        .cloned()
        .unwrap_or(Value::Null);
    match items {
        Value::Array(list) => {
            for entry in &list {
                parse_later_entry(entry, me, &mut out);
            }
        }
        Value::Object(_) => parse_later_entry(&items, me, &mut out),
        _ => {}
    }
    out
}

impl ApiHandle {
    pub async fn explore(&self) -> Result<Vec<Section>, ApiError> {
        self.catalog_page("pages/explore").await
    }

    pub async fn catalog_page(&self, path: &str) -> Result<Vec<Section>, ApiError> {
        let path = safe_page_path(path)
            .ok_or_else(|| ApiError::Client(format!("bad catalog path {path}")))?;
        let url = format!("{V1}/{path}");
        let v = self.get(&url, &[]).await?;
        Ok(sections_from_v1_page(&v, self.user_id))
    }

    pub async fn feed(&self) -> Result<Vec<Section>, ApiError> {
        let v = self.get(&format!("{V2}/feed/activities"), &[]).await?;
        Ok(sections_from_feed(&v, self.user_id))
    }

    /// Mixed Save for Later bookmarks. Missing endpoints yield an empty list.
    pub async fn later(&self) -> Result<LaterItems, ApiError> {
        let urls = [
            format!("{V1}/users/{}/favorites/later", self.user_id),
            format!("{V2}/favorites/later"),
        ];
        for url in urls {
            match self.get(&url, &[("limit", "100")]).await {
                Ok(v) if !v.is_null() => return Ok(parse_later(&v, self.user_id)),
                Ok(_) => {}
                Err(e) => tracing::debug!("save-for-later {url}: {e}"),
            }
        }
        Ok(LaterItems::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn civil_date_roundtrips() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 11, 12)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    #[test]
    fn feed_group_matches_official_buckets() {
        let t = (2026, 11, 12);
        assert_eq!(feed_group_on("2026-11-06T00:00:00Z", t), "Last week");
        assert_eq!(feed_group_on("2026-11-03T00:00:00Z", t), "Last week");
        assert_eq!(feed_group_on("2026-11-01T00:00:00Z", t), "This month");
        assert_eq!(feed_group_on("2026-10-31T00:00:00Z", t), "Last month");
        assert_eq!(feed_group_on("2026-08-01T00:00:00Z", t), "Older");
    }

    #[test]
    fn v1_page_parses_album_list_and_genre_links() {
        let page = json!({
            "title": "Explore",
            "rows": [
                {"modules": [{
                    "type": "FEATURED_PROMOTIONS",
                    "title": "Featured",
                    "items": [
                        {"type": "ALBUM", "artifactId": "574", "shortHeader": "Phoebe",
                         "shortSubHeader": "Live EP",
                         "imageId": "fd953554-aaaa-bbbb-cccc-ddddeeeeffff"},
                        {"type": "EXTURL", "artifactId": "https://tidal.com/x"},
                        {"type": "CATEGORY_PAGES",
                         "artifactId": "pages/UPLOAD_PAGE_PAYGATED_ALBUMS/view-all?vibe=uploads",
                         "shortHeader": "Uploads for Purchase",
                         "imageId": "67388ba0-1111-2222-3333-444455556666"}
                    ]
                }]},
                {"modules": [{
                    "type": "PAGE_LINKS_CLOUD",
                    "title": "Genres",
                    "pagedList": {"items": [
                        {"apiPath": "pages/genre_hip_hop", "title": "Hip-Hop", "imageId": "hiphop"},
                        {"apiPath": "../secret", "title": "Nope"}
                    ]}
                }]},
                {"modules": [{
                    "type": "PAGE_LINKS",
                    "title": "",
                    "items": [
                        {"apiPath": "pages/new", "title": "New"},
                        {"apiPath": "pages/top", "title": "Top"}
                    ]
                }]},
                {"modules": [{
                    "type": "ALBUM_LIST",
                    "title": "New Albums",
                    "pagedList": {"items": [
                        {"id": 9, "title": "Sea Change", "cover": "c", "artists": [{"name": "Beck"}]}
                    ]}
                }]},
                {"modules": [{"type": "VIDEO_LIST", "title": "Videos", "pagedList": {"items": [{}]}}]}
            ]
        });
        let sections = sections_from_v1_page(&page, 1);
        assert_eq!(
            sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Featured", "Genres", "New Albums"]
        );
        assert_eq!(sections[0].cards.len(), 2);
        assert_eq!(sections[0].cards[0].title, "Phoebe");
        assert_eq!(sections[0].cards[0].kind, "album");
        assert!(
            sections[0].cards[0]
                .image_url
                .as_deref()
                .unwrap_or("")
                .ends_with("/origin.jpg")
        );
        assert_eq!(sections[0].cards[1].kind, "page");
        assert_eq!(sections[0].cards[1].title, "Uploads for Purchase");
        assert!(
            sections[0].cards[1]
                .image_url
                .as_deref()
                .unwrap_or("")
                .contains("/67388ba0/1111/2222/3333/444455556666/origin.jpg")
        );
        assert_eq!(sections[1].layout, "tiles");
        assert_eq!(sections[1].cards[0].id, "pages/genre_hip_hop");
        assert_eq!(sections[1].cards[0].kind, "page");
        assert!(sections[1].cards[0].image_url.is_none());
        assert_eq!(sections[2].cards[0].title, "Sea Change");
    }

    #[test]
    fn feed_parses_album_and_history_mix() {
        let v = json!({
            "activities": [
                {"followableActivity": {
                    "activityType": "NEW_ALBUM_RELEASE",
                    "occurredAt": "2026-11-06T00:00:00.000Z",
                    "album": {
                        "id": 1, "title": "Butter", "type": "SINGLE", "cover": "c",
                        "artists": [{"name": "Dyalla"}]
                    }
                }},
                {"followableActivity": {
                    "activityType": "NEW_HISTORY_MIX",
                    "occurredAt": "2026-11-01T00:00:00.000Z",
                    "historyMix": {
                        "id": "mix1",
                        "title": "October 2026",
                        "subTitle": "Linkin Park and more",
                        "images": {"MEDIUM": {"url": "https://img/m.jpg"}}
                    }
                }}
            ]
        });
        let sections = sections_from_feed_on(&v, 1, (2026, 11, 12));
        let titles: Vec<_> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Last week", "This month"]);
        let cards: Vec<_> = sections.iter().flat_map(|s| s.cards.iter()).collect();
        let butter = cards.iter().find(|c| c.title == "Butter").unwrap();
        assert_eq!(butter.kind, "album");
        assert_eq!(butter.subtitle, "Single by Dyalla");
        let mix = cards.iter().find(|c| c.kind == "mix").unwrap();
        assert_eq!(mix.title, "October 2026");
        assert_eq!(mix.id, "mix1");
    }

    #[test]
    fn later_parses_mixed_items() {
        let v = json!({
            "items": [
                {"type": "track", "item": {"id": 11, "title": "Famous", "artists": [{"name": "Kanye West"}]}},
                {"type": "ALBUM", "created": "2026-01-01", "item": {"id": 22, "title": "Clancy", "cover": "x", "artists": [{"name": "twenty one pilots"}]}},
                {"type": "VIDEO", "item": {"id": 99, "title": "clip"}}
            ]
        });
        let later = parse_later(&v, 1);
        assert_eq!(later.tracks.len(), 1);
        assert_eq!(later.tracks[0].title, "Famous");
        assert_eq!(later.cards.len(), 1);
        assert_eq!(later.cards[0].title, "Clancy");
        assert_eq!(later.cards[0].added, "2026-01-01");
    }

    #[test]
    fn bad_page_paths_are_rejected() {
        assert!(safe_page_path("pages/genre_hip_hop").is_some());
        assert!(safe_page_path("pages/m_1950s").is_some());
        assert!(safe_page_path("../etc").is_none());
        assert!(safe_page_path("https://evil").is_none());
        assert!(safe_page_path("pages/foo/../bar").is_none());
    }
}
