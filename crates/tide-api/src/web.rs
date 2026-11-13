//! Schema-tolerant TIDAL web API access.
//!
//! Everything here goes through untyped JSON so that upstream schema drift
//! degrades a single field instead of failing the whole response.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApiError, TrackItem};

pub(crate) const V1: &str = "https://api.tidal.com/v1";
pub(crate) const V2: &str = "https://api.tidal.com/v2";
pub(crate) const WEB_V2: &str = "https://tidal.com/v2";
const CLIENT_VERSION: &str = "2026.1.6";

/// Album / playlist ids currently in My Collection (`GET …/favorites/ids`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FavoriteIds {
    #[serde(default)]
    pub albums: std::collections::HashSet<String>,
    #[serde(default)]
    pub playlists: std::collections::HashSet<String>,
    #[serde(default)]
    pub videos: std::collections::HashSet<String>,
}

/// A browsable thing: playlist, album, mix, artist, folder or deep link.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    /// "playlist" | "album" | "mix" | "artist" | "folder" | "tracks"
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    /// Album badge label: MAX | HIGH | LOW. HIGH is 16-bit FLAC; LOW is AAC.
    #[serde(default)]
    pub quality: String,
    /// Square artwork, ~320px.
    pub image_url: Option<String>,
    /// ISO date the user added it to their collection (sorting).
    #[serde(default)]
    pub added: String,
    /// ISO release date — albums only.
    #[serde(default)]
    pub released: String,
    /// Who it is by: album artists, or a playlist's creator ("Me" for the user's own).
    #[serde(default)]
    pub by: String,
    /// Length in seconds — videos only.
    #[serde(default)]
    pub duration: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Section {
    pub title: String,
    /// "shortcuts" | "cards" | "tracks"
    pub layout: String,
    pub cards: Vec<Card>,
    pub tracks: Vec<TrackItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogDetail {
    pub card: Card,
    pub artist: Option<Card>,
    /// Every credited artist (albums), in billing order.
    #[serde(default)]
    pub artists: Vec<crate::ArtistRef>,
    pub year: String,
    pub quality: String,
    pub track_count: u64,
    pub duration: u64,
    pub biography: String,
    /// Full ISO release date — albums only.
    #[serde(default)]
    pub release_date: String,
    #[serde(default)]
    pub copyright: String,
}

/// One line of album credits: a role and everyone credited with it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credit {
    pub role: String,
    pub names: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lyrics {
    pub plain: String,
    /// (start time in ms, text). Empty when the provider has no timing.
    pub synced: Vec<(u64, String)>,
}

/// One rendition of a video's HLS master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoVariant {
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    pub url: String,
}

/// A playable video: the HLS master playlist (adaptive, "Auto") and its
/// fixed renditions, highest first.
#[derive(Debug, Clone)]
pub struct VideoStream {
    pub master: String,
    pub variants: Vec<VideoVariant>,
}

/// Renditions listed in an HLS master playlist, highest resolution first.
pub fn parse_hls_master(text: &str, base: &str) -> Vec<VideoVariant> {
    let mut out = Vec::new();
    let mut pending: Option<(u32, u32, u64)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attr = |name: &str| {
                attrs
                    .split(',')
                    .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
                    .map(|v| v.trim_matches('"').to_string())
            };
            let (w, h) = attr("RESOLUTION")
                .and_then(|r| {
                    let (w, h) = r.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .unwrap_or((0, 0));
            let bw = attr("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0);
            pending = Some((w, h, bw));
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some((width, height, bandwidth)) = pending.take()
        {
            let url = if line.starts_with("http") {
                line.to_string()
            } else {
                let base = base.split('?').next().unwrap_or(base);
                match base.rfind('/') {
                    Some(i) => format!("{}/{line}", &base[..i]),
                    None => line.to_string(),
                }
            };
            out.push(VideoVariant {
                width,
                height,
                bandwidth,
                url,
            });
        }
    }
    out.sort_by(|a, b| b.height.cmp(&a.height).then(b.bandwidth.cmp(&a.bandwidth)));
    out.dedup_by_key(|v| v.height);
    out
}

/// Still for a video, at a 16:9 size TIDAL serves (320x180, 640x360, 1280x720).
pub fn video_image_url(uuid: &str, width: u32) -> String {
    format!(
        "https://resources.tidal.com/images/{}/{width}x{}.jpg",
        uuid.replace('-', "/"),
        width * 9 / 16
    )
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub urls: Vec<String>,
    pub quality: String,
    pub bit_depth: u32,
    pub sample_rate: u32,
}

/// Cheap, cloneable snapshot of the credentials needed to call the API.
#[derive(Debug, Clone)]
pub struct ApiHandle {
    pub(crate) token: String,
    pub(crate) country: String,
    pub(crate) user_id: u64,
}

pub(crate) fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("tidal-rust/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client")
    })
}

pub fn image_url(uuid: &str, size: u32) -> String {
    format!(
        "https://resources.tidal.com/images/{}/{size}x{size}.jpg",
        uuid.replace('-', "/")
    )
}

/// Featured / promo stills often have no `{size}x{size}` variant (those 403).
pub fn origin_image_url(uuid: &str) -> String {
    format!(
        "https://resources.tidal.com/images/{}/origin.jpg",
        uuid.replace('-', "/")
    )
}

/// UUID-shaped image ids (with dashes) can be fetched as `origin.jpg`.
pub(crate) fn uuid_origin_url(id: &str) -> Option<String> {
    let id = id.trim();
    let dashes = id.bytes().filter(|b| *b == b'-').count();
    (id.len() >= 32 && dashes >= 4).then(|| origin_image_url(id))
}

pub(crate) fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Readable artist bio: keep `[wimpLink …]Name[/wimpLink]` inner text, turn `<br/>` into paragraphs.
pub fn plain_bio(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(n) = html_break_len(&bytes[i..]) {
            out.push('\n');
            i += n;
            continue;
        }
        if bytes[i] == b'[' || bytes[i] == b'<' {
            let close = if bytes[i] == b'[' { b']' } else { b'>' };
            match bytes[i + 1..].iter().position(|&b| b == close) {
                Some(rel) => {
                    i += 1 + rel + 1;
                    continue;
                }
                None => break,
            }
        }
        let c = raw[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    decode_entities(&out)
        .split('\n')
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn html_break_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 || !bytes[..3].eq_ignore_ascii_case(b"<br") {
        return None;
    }
    let mut i = 3;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'/' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    (i < bytes.len() && bytes[i] == b'>').then_some(i + 1)
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let hit = [
            ("&nbsp;", " "),
            ("&quot;", "\""),
            ("&#39;", "'"),
            ("&apos;", "'"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&amp;", "&"),
        ]
        .into_iter()
        .find(|(e, _)| rest.starts_with(*e));
        if let Some((e, r)) = hit {
            out.push_str(r);
            rest = &rest[e.len()..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Every credited artist with an id (`artists[]`, falling back to the single `artist`).
pub(crate) fn artist_refs(v: &Value) -> Vec<crate::ArtistRef> {
    let one = |a: &Value| {
        let (id, name) = (s(a, "id"), s(a, "name"));
        (!id.is_empty() && !name.is_empty()).then_some(crate::ArtistRef { id, name })
    };
    match v.get("artists").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().filter_map(one).collect(),
        _ => v.get("artist").and_then(one).into_iter().collect(),
    }
}

pub(crate) fn artist_names(v: &Value) -> String {
    let names: Vec<String> = v
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| s(x, "name"))
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        v.get("artist").map(|a| s(a, "name")).unwrap_or_default()
    } else {
        names.join(", ")
    }
}

pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026-11-11T04:40:02.184+0000" -> "Today" / "Last week" / "Mar 2024".
pub fn relative_date(iso: &str) -> String {
    let mut parts = iso
        .get(..10)
        .unwrap_or("")
        .split('-')
        .filter_map(|p| p.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return String::new();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        / 86_400;
    let age = now - days_from_civil(y, m, d);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match age {
        i64::MIN..=0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=13 => "Last week".into(),
        14..=59 => "Last month".into(),
        _ => format!("{} {y}", MONTHS[(m as usize).clamp(1, 12) - 1]),
    }
}

pub fn parse_track(v: &Value) -> Option<TrackItem> {
    let id = s(v, "id");
    if id.is_empty() || v.get("title").is_none() {
        return None;
    }
    let mut title = s(v, "title");
    let version = s(v, "version");
    if !version.is_empty() && !title.contains(&version) {
        title = format!("{title} ({version})");
    }
    let album = v.get("album").cloned().unwrap_or(Value::Null);
    let cover = s(&album, "cover");
    let hires = v
        .pointer("/mediaMetadata/tags")
        .and_then(Value::as_array)
        .is_some_and(|t| t.iter().any(|x| x == "HIRES_LOSSLESS"));
    let first_artist_id = v
        .get("artists")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .or_else(|| v.get("artist"))
        .map(|a| s(a, "id"))
        .filter(|x| !x.is_empty());
    Some(TrackItem {
        id,
        title,
        artist: artist_names(v),
        artist_id: first_artist_id,
        artists: artist_refs(v),
        album: s(&album, "title"),
        album_id: Some(s(&album, "id")).filter(|x| !x.is_empty()),
        duration_secs: v.get("duration").and_then(Value::as_u64).unwrap_or(0) as u32,
        cover_url: (!cover.is_empty()).then(|| image_url(&cover, 160)),
        audio_quality: if hires {
            "HI_RES_LOSSLESS".into()
        } else {
            s(v, "audioQuality")
        },
        explicit: v.get("explicit").and_then(Value::as_bool).unwrap_or(false),
        date_added: None,
        bpm: v.get("bpm").and_then(Value::as_u64).map(|b| b as u32),
    })
}

fn parse_track_list(v: &Value) -> Vec<TrackItem> {
    v.get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| {
                    // Playlist/album/mix entries wrap the track in `item`; search does not.
                    if entry
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t != "track")
                    {
                        return None;
                    }
                    let mut t = parse_track(entry.get("item").unwrap_or(entry))?;
                    t.date_added = entry
                        .get("created")
                        .or_else(|| entry.pointer("/item/dateAdded"))
                        .and_then(Value::as_str)
                        .map(relative_date);
                    Some(t)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn text_info(d: &Value, keys: &[&str]) -> String {
    for k in keys {
        match d.get(*k) {
            Some(Value::String(x)) if !x.is_empty() => return x.clone(),
            Some(o @ Value::Object(_)) => {
                let t = s(o, "text");
                if !t.is_empty() {
                    return t;
                }
            }
            _ => {}
        }
    }
    String::new()
}

fn mix_image(d: &Value) -> Option<String> {
    // Home feed: `mixImages: [{size,url}]`; favourites: `images: {SMALL:{url}}`.
    if let Some(list) = d.get("mixImages").and_then(Value::as_array) {
        let pick = list
            .iter()
            .find(|i| i.get("size").is_some_and(|x| x == "MEDIUM"))
            .or_else(|| list.first());
        return pick.map(|i| s(i, "url")).filter(|u| !u.is_empty());
    }
    let images = d.get("images")?;
    ["MEDIUM", "SMALL", "LARGE"]
        .iter()
        .find_map(|k| images.get(*k))
        .map(|i| s(i, "url"))
        .filter(|u| !u.is_empty())
}

fn album_quality_badge(d: &Value) -> String {
    let audio = s(d, "audioQuality");
    let raw = if d
        .pointer("/mediaMetadata/tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| {
            tags.iter()
                .any(|tag| tag.as_str() == Some("HIRES_LOSSLESS"))
        }) {
        "HI_RES_LOSSLESS"
    } else {
        audio.as_str()
    };
    match raw.to_uppercase().as_str() {
        "HI_RES_LOSSLESS" | "HIRES_LOSSLESS" => "MAX".into(),
        // TIDAL's album page: High = 16-bit FLAC, Low = AAC. The API still says LOSSLESS / HIGH.
        "LOSSLESS" => "HIGH".into(),
        "HIGH" | "LOW" => "LOW".into(),
        _ => String::new(),
    }
}

fn parse_video_list(data: &Value, me: u64) -> Vec<Card> {
    data["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| parse_card("VIDEO", item.get("item").unwrap_or(item), me))
        .collect()
}

pub(crate) fn parse_card(kind: &str, d: &Value, me: u64) -> Option<Card> {
    let card = match kind {
        "ALBUM" => {
            let cover = s(d, "cover");
            Card {
                id: s(d, "id"),
                kind: "album".into(),
                title: s(d, "title"),
                subtitle: artist_names(d),
                quality: album_quality_badge(d),
                image_url: (!cover.is_empty()).then(|| image_url(&cover, 320)),
                released: s(d, "releaseDate"),
                by: artist_names(d),
                ..Default::default()
            }
        }
        "PLAYLIST" => {
            let img = Some(s(d, "squareImage")).filter(|x| !x.is_empty());
            let creator = d.get("creator").cloned().unwrap_or(Value::Null);
            let mine = creator.get("id").and_then(Value::as_u64) == Some(me);
            let name = s(&creator, "name");
            let owner = if mine {
                "Me".to_string()
            } else if name.is_empty() {
                "TIDAL".into()
            } else {
                name
            };
            let by = if mine {
                "Created by me".to_string()
            } else {
                format!("Created by {owner}")
            };
            let id = {
                let uuid = s(d, "uuid");
                if uuid.is_empty() { s(d, "id") } else { uuid }
            };
            Card {
                id,
                kind: "playlist".into(),
                title: s(d, "title"),
                subtitle: by,
                image_url: img.map(|u| image_url(&u, 320)),
                by: owner,
                ..Default::default()
            }
        }
        "MIX" => Card {
            id: s(d, "id"),
            kind: "mix".into(),
            title: text_info(d, &["titleTextInfo", "title"]),
            subtitle: text_info(
                d,
                &[
                    "subtitleTextInfo",
                    "subTitleTextInfo",
                    "subTitle",
                    "shortSubtitleTextInfo",
                ],
            ),
            image_url: mix_image(d),
            added: s(d, "dateAdded"),
            ..Default::default()
        },
        "ARTIST" => {
            // Artists without a portrait fall back to one of their album covers.
            let pic = Some(s(d, "picture"))
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| s(d, "selectedAlbumCoverFallback"));
            Card {
                id: s(d, "id"),
                kind: "artist".into(),
                title: s(d, "name"),
                subtitle: "Artist".into(),
                image_url: (!pic.is_empty()).then(|| image_url(&pic, 320)),
                ..Default::default()
            }
        }
        "VIDEO" => {
            let still = s(d, "imageId");
            Card {
                id: s(d, "id"),
                kind: "video".into(),
                title: s(d, "title"),
                subtitle: artist_names(d),
                image_url: (!still.is_empty()).then(|| video_image_url(&still, 640)),
                released: s(d, "releaseDate"),
                by: artist_names(d),
                duration: d.get("duration").and_then(Value::as_u64).unwrap_or(0) as u32,
                ..Default::default()
            }
        }
        "DEEP_LINK" if s(d, "url").ends_with("my-collection/tracks") => Card {
            id: "tracks".into(),
            kind: "tracks".into(),
            title: s(d, "title"),
            ..Default::default()
        },
        _ => return None,
    };
    (!card.id.is_empty()).then_some(card)
}

pub(crate) fn sections_from_v2_feed(v: &Value, me: u64) -> Vec<Section> {
    v.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let items = m.get("items").and_then(Value::as_array)?;
            let mut section = Section {
                title: s(m, "title"),
                layout: match s(m, "type").as_str() {
                    "SHORTCUT_LIST" => "shortcuts",
                    "TRACK_LIST" => "tracks",
                    _ => "cards",
                }
                .into(),
                ..Default::default()
            };
            for it in items {
                let (kind, data) = (s(it, "type"), it.get("data").unwrap_or(&Value::Null));
                if kind == "TRACK" {
                    section.tracks.extend(parse_track(data));
                } else {
                    section.cards.extend(parse_card(&kind, data, me));
                }
            }
            (!section.cards.is_empty() || !section.tracks.is_empty()).then_some(section)
        })
        .collect()
}

fn parse_lrc(subtitles: &str) -> Vec<(u64, String)> {
    subtitles
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('[')?;
            let (stamp, text) = rest.split_once(']')?;
            let (min, sec) = stamp.split_once(':')?;
            let ms = min.parse::<u64>().ok()? * 60_000 + (sec.parse::<f64>().ok()? * 1000.0) as u64;
            Some((ms, text.trim().to_string()))
        })
        .collect()
}

/// Pull every segment URL out of a DASH MPD (single representation, SegmentTemplate).
fn parse_dash(mpd: &str) -> Vec<String> {
    fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
        let end = tag[start..].find('"')? + start;
        Some(&tag[start..end])
    }
    let unescape = |u: &str| u.replace("&amp;", "&");
    let Some(tpl_start) = mpd.find("<SegmentTemplate") else {
        return Vec::new();
    };
    let tpl_end = mpd[tpl_start..]
        .find('>')
        .map_or(mpd.len(), |e| e + tpl_start);
    let tpl = &mpd[tpl_start..tpl_end];
    let (Some(init), Some(media)) = (attr(tpl, "initialization"), attr(tpl, "media")) else {
        return Vec::new();
    };
    let first: u64 = attr(tpl, "startNumber")
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);

    let mut segments = 0u64;
    for piece in mpd.split("<S ").skip(1) {
        let tag = &piece[..piece.find('>').unwrap_or(piece.len())];
        segments += 1 + attr(tag, "r")
            .and_then(|r| r.parse::<u64>().ok())
            .unwrap_or(0);
    }
    let mut urls = vec![unescape(init)];
    urls.extend(
        (first..first + segments).map(|n| unescape(&media.replace("$Number$", &n.to_string()))),
    );
    urls
}

fn b64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

impl ApiHandle {
    pub fn user_id(&self) -> u64 {
        self.user_id
    }

    async fn send(&self, req: reqwest::RequestBuilder, url: &str) -> Result<Value, ApiError> {
        let resp = req
            .bearer_auth(&self.token)
            .header("x-tidal-client-version", CLIENT_VERSION)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let body: String = text.chars().take(200).collect();
            return Err(ApiError::Client(format!("HTTP {status} for {url}: {body}")));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn get(&self, url: &str, params: &[(&str, &str)]) -> Result<Value, ApiError> {
        let mut q: Vec<(&str, &str)> = params.to_vec();
        for (k, v) in [
            ("countryCode", self.country.as_str()),
            ("locale", "en_US"),
            ("deviceType", "BROWSER"),
        ] {
            if !q.iter().any(|(key, _)| *key == k) {
                q.push((k, v));
            }
        }
        self.send(http().get(url).query(&q), url).await
    }

    /// One page of the user's favourite tracks, newest first, plus the total count.
    pub async fn favorite_tracks(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<TrackItem>, u32), ApiError> {
        let url = format!("{V1}/users/{}/favorites/tracks", self.user_id);
        let (o, l) = (offset.to_string(), limit.to_string());
        let v = self
            .get(
                &url,
                &[
                    ("offset", &o),
                    ("limit", &l),
                    ("order", "DATE"),
                    ("orderDirection", "DESC"),
                ],
            )
            .await?;
        let total = v
            .get("totalNumberOfItems")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        Ok((parse_track_list(&v), total))
    }

    pub async fn set_favorite(&self, track_id: &str, favorite: bool) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        if favorite {
            let url = format!("{V1}/users/{}/favorites/tracks", self.user_id);
            let form = [("trackIds", track_id), ("onArtifactNotFound", "FAIL")];
            self.send(http().post(&url).query(&q).form(&form), &url)
                .await?;
        } else {
            let url = format!("{V1}/users/{}/favorites/tracks/{track_id}", self.user_id);
            self.send(http().delete(&url).query(&q), &url).await?;
        }
        Ok(())
    }

    /// Whether the user follows (has favourited) this artist.
    pub async fn is_artist_followed(&self, artist_id: &str) -> Result<bool, ApiError> {
        self.is_favorite("ARTIST", artist_id).await
    }

    pub async fn set_artist_followed(&self, artist_id: &str, follow: bool) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        if follow {
            let url = format!("{V1}/users/{}/favorites/artists", self.user_id);
            let form = [("artistIds", artist_id), ("onArtifactNotFound", "FAIL")];
            self.send(http().post(&url).query(&q).form(&form), &url)
                .await?;
        } else {
            let url = format!("{V1}/users/{}/favorites/artists/{artist_id}", self.user_id);
            self.send(http().delete(&url).query(&q), &url).await?;
        }
        Ok(())
    }

    pub async fn home(&self) -> Result<Vec<Section>, ApiError> {
        self.home_feed("static").await
    }

    /// Home tabs: `static` (For you), `editorial` (Staff Picks), `uploads`.
    pub async fn home_feed(&self, feed: &str) -> Result<Vec<Section>, ApiError> {
        let feed = match feed {
            "editorial" | "uploads" => feed,
            _ => "static",
        };
        let url = format!("{WEB_V2}/home/feed/{feed}");
        let v = self
            .get(
                &url,
                &[
                    ("limit", "20"),
                    ("platform", "WEB"),
                    ("timeOffset", "+00:00"),
                ],
            )
            .await?;
        Ok(sections_from_v2_feed(&v, self.user_id))
    }

    /// Album and playlist ids in My Collection, for heart fill without a per-card fetch.
    pub async fn favorite_ids(&self) -> Result<FavoriteIds, ApiError> {
        let url = format!("{V1}/users/{}/favorites/ids", self.user_id);
        let v = self.get(&url, &[]).await?;
        let read = |key: &str| -> std::collections::HashSet<String> {
            v.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .or_else(|| x.as_u64().map(|n| n.to_string()))
                })
                .collect()
        };
        Ok(FavoriteIds {
            albums: read("ALBUM"),
            playlists: read("PLAYLIST"),
            videos: read("VIDEO"),
        })
    }

    /// Folders and playlists inside a collection folder ("root" for the top level), by name.
    pub async fn playlists(&self, folder_id: &str) -> Result<Vec<Card>, ApiError> {
        let url = format!("{V2}/my-collection/playlists/folders");
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut params = vec![
                ("folderId", folder_id),
                ("limit", "50"),
                ("order", "NAME"),
                ("orderDirection", "ASC"),
                ("includeOnly", ""),
            ];
            if !cursor.is_empty() {
                params.push(("cursor", &cursor));
            }
            let v = self.get(&url, &params).await?;
            for it in v
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let d = it.get("data").unwrap_or(&Value::Null);
                if s(it, "itemType") == "FOLDER" {
                    let n = d
                        .get("totalNumberOfItems")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    out.push(Card {
                        id: s(d, "id"),
                        kind: "folder".into(),
                        title: s(it, "name"),
                        subtitle: format!("{n} playlist{}", if n == 1 { "" } else { "s" }),
                        image_url: None,
                        added: s(it, "addedAt"),
                        ..Default::default()
                    });
                } else if let Some(mut card) = parse_card("PLAYLIST", d, self.user_id) {
                    let mine =
                        d.pointer("/creator/id").and_then(Value::as_u64) == Some(self.user_id);
                    card.added = s(it, "addedAt");
                    card.subtitle = if mine {
                        let n = d.get("numberOfTracks").and_then(Value::as_u64).unwrap_or(0)
                            + d.get("numberOfVideos").and_then(Value::as_u64).unwrap_or(0);
                        format!("{n} item{}", if n == 1 { "" } else { "s" })
                    } else {
                        let name = d
                            .pointer("/creator/name")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if name.is_empty() {
                            "TIDAL".into()
                        } else {
                            name.to_string()
                        }
                    };
                    out.push(card);
                }
            }
            cursor = s(&v, "cursor");
            if cursor.is_empty() || out.len() >= 500 {
                break;
            }
        }
        // Folders first, like the official client.
        out.sort_by_key(|c| c.kind != "folder");
        Ok(out)
    }

    pub async fn mixes(&self) -> Result<Vec<Card>, ApiError> {
        let v = self
            .get(&format!("{V2}/favorites/mixes"), &[("limit", "50")])
            .await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|d| parse_card("MIX", d, self.user_id))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// One collection list from `/users/{me}/favorites/{what}`, newest first, every page.
    async fn favorite_cards(&self, what: &str, kind: &str) -> Result<Vec<Card>, ApiError> {
        let url = format!("{V1}/users/{}/favorites/{what}", self.user_id);
        let mut out = Vec::new();
        loop {
            let offset = out.len().to_string();
            let v = self
                .get(
                    &url,
                    &[
                        ("limit", "100"),
                        ("offset", &offset),
                        ("order", "DATE"),
                        ("orderDirection", "DESC"),
                    ],
                )
                .await?;
            let items = v
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for entry in &items {
                if let Some(mut card) = entry
                    .get("item")
                    .and_then(|item| parse_card(kind, item, self.user_id))
                {
                    card.added = s(entry, "created");
                    out.push(card);
                }
            }
            let total = v
                .get("totalNumberOfItems")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if items.is_empty() || out.len() >= total || out.len() >= 2000 {
                return Ok(out);
            }
        }
    }

    pub async fn albums(&self) -> Result<Vec<Card>, ApiError> {
        self.favorite_cards("albums", "ALBUM").await
    }

    /// Videos in My Collection, newest first.
    pub async fn videos(&self) -> Result<Vec<Card>, ApiError> {
        self.favorite_cards("videos", "VIDEO").await
    }

    /// "Similar videos" for the video player.
    pub async fn video_recommendations(&self, id: &str) -> Result<Vec<Card>, ApiError> {
        let v = self
            .get(
                &format!("{V1}/videos/{id}/recommendations"),
                &[("limit", "30")],
            )
            .await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|it| parse_card("VIDEO", it.get("video")?, self.user_id))
            .collect())
    }

    pub async fn video(&self, id: &str) -> Result<Card, ApiError> {
        let v = self.get(&format!("{V1}/videos/{id}"), &[]).await?;
        parse_card("VIDEO", &v, self.user_id)
            .ok_or_else(|| ApiError::Client(format!("video {id} not found")))
    }

    pub async fn set_video_favorite(&self, video_id: &str, favorite: bool) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        if favorite {
            let url = format!("{V1}/users/{}/favorites/videos", self.user_id);
            let form = [("videoIds", video_id), ("onArtifactNotFound", "FAIL")];
            self.send(http().post(&url).query(&q).form(&form), &url)
                .await?;
        } else {
            let url = format!("{V1}/users/{}/favorites/videos/{video_id}", self.user_id);
            self.send(http().delete(&url).query(&q), &url).await?;
        }
        Ok(())
    }

    /// Resolve a video to its HLS master playlist and the renditions it offers.
    pub async fn video_stream(&self, id: &str) -> Result<VideoStream, ApiError> {
        let url = format!("{V1}/videos/{id}/playbackinfopostpaywall");
        let v = self
            .get(
                &url,
                &[
                    ("videoquality", "HIGH"),
                    ("playbackmode", "STREAM"),
                    ("assetpresentation", "FULL"),
                ],
            )
            .await?;
        let manifest = b64_decode(&s(&v, "manifest"))
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        let master = serde_json::from_str::<Value>(&manifest)
            .ok()
            .and_then(|m| {
                m.get("urls")?
                    .as_array()?
                    .first()?
                    .as_str()
                    .map(String::from)
            })
            .ok_or_else(|| ApiError::NoStream(id.to_string()))?;
        // The playlist URL is pre-signed; it takes no bearer token.
        let variants = match http().get(&master).send().await {
            Ok(resp) => parse_hls_master(&resp.text().await.unwrap_or_default(), &master),
            Err(e) => {
                tracing::warn!("video playlist fetch failed: {e}");
                Vec::new()
            }
        };
        Ok(VideoStream { master, variants })
    }

    /// Artists the user follows (the Profiles page).
    pub async fn followed_artists(&self) -> Result<Vec<Card>, ApiError> {
        let mut cards = self.favorite_cards("artists", "ARTIST").await?;
        for card in &mut cards {
            card.subtitle.clear();
        }
        Ok(cards)
    }

    pub async fn similar_artists(&self, id: &str) -> Result<Vec<Card>, ApiError> {
        let data = self
            .get(&format!("{V1}/artists/{id}/similar"), &[("limit", "20")])
            .await?;
        Ok(data["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let mut card = parse_card("ARTIST", item, self.user_id)?;
                card.subtitle.clear();
                Some(card)
            })
            .collect())
    }

    pub async fn album_credits(&self, id: &str) -> Result<Vec<Credit>, ApiError> {
        let data = self.get(&format!("{V1}/albums/{id}/credits"), &[]).await?;
        Ok(data
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| Credit {
                role: s(entry, "type"),
                names: entry["contributors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| s(c, "name"))
                    .filter(|n| !n.is_empty())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
            .filter(|credit| !credit.role.is_empty() && !credit.names.is_empty())
            .collect())
    }

    /// Whether `id` is in the user's collection. `kind` is "ALBUM" or "ARTIST".
    pub async fn is_favorite(&self, kind: &str, id: &str) -> Result<bool, ApiError> {
        let url = format!("{V1}/users/{}/favorites/ids", self.user_id);
        let v = self.get(&url, &[]).await?;
        Ok(v.get(kind).and_then(Value::as_array).is_some_and(|ids| {
            ids.iter().any(|x| {
                x.as_str()
                    .map(str::to_string)
                    .or_else(|| x.as_u64().map(|n| n.to_string()))
                    .as_deref()
                    == Some(id)
            })
        }))
    }

    pub async fn set_album_favorite(&self, album_id: &str, favorite: bool) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        if favorite {
            let url = format!("{V1}/users/{}/favorites/albums", self.user_id);
            let form = [("albumIds", album_id), ("onArtifactNotFound", "FAIL")];
            self.send(http().post(&url).query(&q).form(&form), &url)
                .await?;
        } else {
            let url = format!("{V1}/users/{}/favorites/albums/{album_id}", self.user_id);
            self.send(http().delete(&url).query(&q), &url).await?;
        }
        Ok(())
    }

    /// Add or remove someone else's playlist from the user's collection.
    pub async fn set_playlist_favorite(
        &self,
        playlist_id: &str,
        favorite: bool,
    ) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        if favorite {
            let url = format!("{V1}/users/{}/favorites/playlists", self.user_id);
            let form = [("uuids", playlist_id), ("onArtifactNotFound", "FAIL")];
            self.send(http().post(&url).query(&q).form(&form), &url)
                .await?;
        } else {
            let url = format!(
                "{V1}/users/{}/favorites/playlists/{playlist_id}",
                self.user_id
            );
            self.send(http().delete(&url).query(&q), &url).await?;
        }
        Ok(())
    }

    /// Append tracks to one of the user's playlists. Tracks already in it are skipped.
    pub async fn add_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        let url = format!("{V1}/playlists/{playlist_id}");
        // Edits must carry the playlist's current ETag.
        let head = http()
            .get(&url)
            .query(&q)
            .bearer_auth(&self.token)
            .header("x-tidal-client-version", CLIENT_VERSION)
            .send()
            .await?;
        if !head.status().is_success() {
            return Err(ApiError::Client(format!(
                "HTTP {} for {url}",
                head.status()
            )));
        }
        let etag = head
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("*")
            .to_string();
        let items = format!("{url}/items");
        let ids = track_ids.join(",");
        let form = [
            ("trackIds", ids.as_str()),
            ("onArtifactNotFound", "SKIP"),
            ("onDupes", "SKIP"),
        ];
        self.send(
            http()
                .post(&items)
                .query(&q)
                .header("If-None-Match", etag)
                .form(&form),
            &items,
        )
        .await?;
        Ok(())
    }

    /// Create an empty playlist or a folder inside `folder_id` ("root" for the top level).
    pub async fn create_in_collection(
        &self,
        name: &str,
        folder_id: &str,
        folder: bool,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{V2}/my-collection/playlists/folders/{}",
            if folder {
                "create-folder"
            } else {
                "create-playlist"
            }
        );
        let mut q = vec![
            ("name", name),
            ("folderId", folder_id),
            ("countryCode", self.country.as_str()),
        ];
        if folder {
            q.push(("trns", ""));
        } else {
            q.extend([("description", ""), ("isPublic", "false")]);
        }
        self.send(http().put(&url).query(&q), &url).await?;
        Ok(())
    }

    pub async fn rename_folder(&self, folder_id: &str, name: &str) -> Result<(), ApiError> {
        let url = format!("{V2}/my-collection/playlists/folders/rename");
        let trn = format!("trn:folder:{folder_id}");
        let q = [
            ("trn", trn.as_str()),
            ("name", name),
            ("countryCode", self.country.as_str()),
        ];
        self.send(http().put(&url).query(&q), &url).await?;
        Ok(())
    }

    pub async fn delete_folder(&self, folder_id: &str) -> Result<(), ApiError> {
        let url = format!("{V2}/my-collection/playlists/folders/remove");
        let trn = format!("trn:folder:{folder_id}");
        let q = [
            ("trns", trn.as_str()),
            ("countryCode", self.country.as_str()),
        ];
        self.send(http().put(&url).query(&q), &url).await?;
        Ok(())
    }

    pub async fn catalog_detail(&self, kind: &str, id: &str) -> Result<CatalogDetail, ApiError> {
        let (resource, card_kind) = match kind {
            "album" => ("albums", "ALBUM"),
            "artist" => ("artists", "ARTIST"),
            _ => return Err(ApiError::Client(format!("unknown catalog kind {kind}"))),
        };
        let url = format!("{V1}/{resource}/{id}");
        let data = self.get(&url, &[]).await?;
        let card = parse_card(card_kind, &data, self.user_id)
            .ok_or_else(|| ApiError::Client(format!("missing {kind} {id}")))?;
        let artist = data
            .get("artist")
            .or_else(|| data.pointer("/artists/0"))
            .and_then(|artist| parse_card("ARTIST", artist, self.user_id));
        let quality = album_quality_badge(&data);
        let biography = if kind == "artist" {
            self.get(&format!("{url}/bio"), &[])
                .await
                .ok()
                .map(|bio| plain_bio(&s(&bio, "text")))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok(CatalogDetail {
            card,
            artist,
            artists: artist_refs(&data),
            year: s(&data, "releaseDate")
                .split('-')
                .next()
                .unwrap_or_default()
                .into(),
            quality,
            track_count: data["numberOfTracks"].as_u64().unwrap_or_default(),
            duration: data["duration"].as_u64().unwrap_or_default(),
            biography,
            release_date: s(&data, "releaseDate"),
            copyright: s(&data, "copyright"),
        })
    }

    pub async fn artist_releases(&self, id: &str, singles: bool) -> Result<Vec<Card>, ApiError> {
        let url = format!("{V1}/artists/{id}/albums");
        let mut offset = 0;
        // TIDAL lists every edition of a release (clean, Dolby Atmos, re-uploads)
        // under the same title. Keep the first stereo one of each.
        let mut seen = std::collections::HashSet::new();
        let mut cards = Vec::new();
        loop {
            let offset_s = offset.to_string();
            let mut params = vec![("limit", "50"), ("offset", offset_s.as_str())];
            if singles {
                params.push(("filter", "EPSANDSINGLES"));
            }
            let data = self.get(&url, &params).await?;
            let items = data["items"].as_array();
            let count = items.map_or(0, Vec::len);
            for item in items.into_iter().flatten() {
                let stereo = item["audioModes"]
                    .as_array()
                    .is_none_or(|modes| modes.iter().any(|m| m == "STEREO"));
                if !stereo || !seen.insert(s(item, "title").to_lowercase()) {
                    continue;
                }
                let Some(mut card) = parse_card("ALBUM", item, self.user_id) else {
                    continue;
                };
                let date = s(item, "releaseDate");
                let year = date.split('-').next().filter(|year| !year.is_empty());
                let kind = match s(item, "type").as_str() {
                    "EP" => "EP",
                    "SINGLE" => "Single",
                    _ => "Album",
                };
                card.subtitle = year.map_or_else(|| kind.into(), |year| format!("{year} · {kind}"));
                cards.push(card);
            }
            offset += count;
            let total = data["totalNumberOfItems"].as_u64().unwrap_or(0);
            if count == 0 || offset as u64 >= total {
                break;
            }
        }
        Ok(cards)
    }

    /// Tracks behind a card: `kind` is "playlist", "album", "mix" or "artist".
    pub async fn tracks_of(&self, kind: &str, id: &str) -> Result<Vec<TrackItem>, ApiError> {
        let (url, limit) = match kind {
            "playlist" => (format!("{V1}/playlists/{id}/items"), "100"),
            "album" => (format!("{V1}/albums/{id}/items"), "100"),
            "mix" => (format!("{V1}/mixes/{id}/items"), "100"),
            "artist" => (format!("{V1}/artists/{id}/toptracks"), "50"),
            other => return Err(ApiError::Client(format!("unknown collection kind {other}"))),
        };
        let mut offset = 0;
        let mut tracks = Vec::new();
        loop {
            let data = self
                .get(&url, &[("limit", limit), ("offset", &offset.to_string())])
                .await?;
            tracks.extend(parse_track_list(&data));
            let count = data["items"].as_array().map_or(0, Vec::len);
            offset += count;
            let total = data["totalNumberOfItems"].as_u64().unwrap_or(0);
            if kind != "album" || count == 0 || offset as u64 >= total {
                return Ok(tracks);
            }
        }
    }

    pub async fn search_tracks(&self, query: &str) -> Result<Vec<TrackItem>, ApiError> {
        let v = self
            .get(
                &format!("{V1}/search"),
                &[("query", query), ("types", "TRACKS"), ("limit", "50")],
            )
            .await?;
        Ok(v.get("tracks").map(parse_track_list).unwrap_or_default())
    }

    pub async fn search_videos(&self, query: &str) -> Result<Vec<Card>, ApiError> {
        let data = self
            .get(
                &format!("{V1}/search"),
                &[("query", query), ("types", "VIDEOS"), ("limit", "50")],
            )
            .await?;
        Ok(parse_video_list(&data["videos"], self.user_id))
    }

    pub async fn artist_videos(&self, id: &str) -> Result<Vec<Card>, ApiError> {
        let url = format!("{V1}/artists/{id}/videos");
        let mut offset = 0;
        let mut seen = std::collections::HashSet::new();
        let mut cards = Vec::new();
        loop {
            let offset_s = offset.to_string();
            let data = self
                .get(&url, &[("limit", "50"), ("offset", &offset_s)])
                .await?;
            let count = data["items"].as_array().map_or(0, Vec::len);
            let before = cards.len();
            cards.extend(
                parse_video_list(&data, self.user_id)
                    .into_iter()
                    .filter(|card| seen.insert(card.id.clone())),
            );
            offset += count;
            if count == 0
                || cards.len() == before
                || data["totalNumberOfItems"]
                    .as_u64()
                    .map_or(count < 50, |total| offset as u64 >= total)
            {
                break;
            }
        }
        Ok(cards)
    }

    pub async fn search_catalog(&self, query: &str) -> Result<Vec<Card>, ApiError> {
        let data = self
            .get(
                &format!("{V1}/search"),
                &[
                    ("query", query),
                    ("types", "ARTISTS,ALBUMS"),
                    ("limit", "3"),
                ],
            )
            .await?;
        Ok([("artists", "ARTIST"), ("albums", "ALBUM")]
            .into_iter()
            .flat_map(|(key, kind)| {
                data[key]["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(move |item| parse_card(kind, item, self.user_id))
            })
            .collect())
    }

    pub async fn track(&self, id: &str) -> Result<TrackItem, ApiError> {
        let v = self.get(&format!("{V1}/tracks/{id}"), &[]).await?;
        parse_track(&v).ok_or_else(|| ApiError::Client(format!("track {id} not found")))
    }

    pub async fn lyrics(&self, track_id: &str) -> Result<Lyrics, ApiError> {
        let v = self
            .get(&format!("{V1}/tracks/{track_id}/lyrics"), &[])
            .await?;
        Ok(Lyrics {
            plain: s(&v, "lyrics"),
            synced: parse_lrc(&s(&v, "subtitles")),
        })
    }

    /// Resolve a stream at `preference` ("max" | "high" | "low"), falling back to lower tiers.
    pub async fn stream(&self, track_id: &str, preference: &str) -> Result<StreamInfo, ApiError> {
        let ladder: &[&str] = match preference {
            "low" => &["HIGH", "LOW"],
            "high" => &["LOSSLESS", "HIGH", "LOW"],
            _ => &["HI_RES_LOSSLESS", "LOSSLESS", "HIGH"],
        };
        let url = format!("{V1}/tracks/{track_id}/playbackinfopostpaywall");
        let mut last_err = ApiError::NoStream(track_id.to_string());
        for quality in ladder.iter().copied() {
            // Stereo only: the engine cannot decode Dolby Atmos (E-AC-3) or 360 Reality Audio.
            let params = [
                ("audioquality", quality),
                ("playbackmode", "STREAM"),
                ("assetpresentation", "FULL"),
                ("immersiveaudio", "false"),
            ];
            let v = match self.get(&url, &params).await {
                Ok(v) => v,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            let manifest = b64_decode(&s(&v, "manifest"))
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_default();
            let urls = if s(&v, "manifestMimeType").contains("dash") {
                parse_dash(&manifest)
            } else {
                serde_json::from_str::<Value>(&manifest)
                    .ok()
                    .and_then(|m| m.get("urls").and_then(Value::as_array).cloned())
                    .map(|a| {
                        a.iter()
                            .filter_map(|u| u.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let undecodable = ["eac3", "ac4", "mha1"].iter().any(|c| manifest.contains(c));
            if urls.is_empty() || undecodable {
                continue;
            }
            return Ok(StreamInfo {
                urls,
                quality: s(&v, "audioQuality"),
                bit_depth: v.get("bitDepth").and_then(Value::as_u64).unwrap_or(16) as u32,
                sample_rate: v
                    .get("sampleRate")
                    .and_then(Value::as_u64)
                    .unwrap_or(44_100) as u32,
            });
        }
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_keeps_every_credited_artist_separately() {
        let v = serde_json::json!({
            "id": 1, "title": "Monster",
            "artists": [{"id": 4, "name": "Shawn Mendes"}, {"id": 7, "name": "Justin Bieber"}],
        });
        let t = parse_track(&v).unwrap();
        assert_eq!(t.artist, "Shawn Mendes, Justin Bieber");
        assert_eq!(t.artist_id.as_deref(), Some("4"));
        let ids: Vec<_> = t
            .artists
            .iter()
            .map(|a| (a.id.as_str(), a.name.as_str()))
            .collect();
        assert_eq!(ids, [("4", "Shawn Mendes"), ("7", "Justin Bieber")]);
    }

    #[test]
    fn dash_manifest_expands_segment_timeline() {
        let mpd = r#"<MPD><Period><AdaptationSet><Representation id="1">
            <SegmentTemplate timescale="44100" initialization="https://x/init.mp4?a=1&amp;b=2" media="https://x/$Number$.mp4" startNumber="1">
            <SegmentTimeline><S d="176400" r="2"/><S d="1000"/></SegmentTimeline></SegmentTemplate>
            </Representation></AdaptationSet></Period></MPD>"#;
        let urls = parse_dash(mpd);
        assert_eq!(urls.len(), 5);
        assert_eq!(urls[0], "https://x/init.mp4?a=1&b=2");
        assert_eq!(urls[4], "https://x/4.mp4");
    }

    #[test]
    fn hls_master_lists_renditions_highest_first() {
        let m3u8 = "#EXTM3U\n#EXT-X-VERSION:3\n\
            #EXT-X-STREAM-INF:BANDWIDTH=445999,CODECS=\"mp4a.40.2,avc1.42C00D\",RESOLUTION=320x180\n\
            https://x/180.m3u8?a=1\n\
            #EXT-X-STREAM-INF:BANDWIDTH=6074999,CODECS=\"mp4a.40.2,avc1.640028\",RESOLUTION=1920x1080\n\
            1080.m3u8\n";
        let v = parse_hls_master(m3u8, "https://cdn/path/master.m3u8?sig=1");
        assert_eq!(v.len(), 2);
        assert_eq!(
            (v[0].width, v[0].height, v[0].bandwidth),
            (1920, 1080, 6074999)
        );
        assert_eq!(v[0].url, "https://cdn/path/1080.m3u8");
        assert_eq!(v[1].url, "https://x/180.m3u8?a=1");
    }

    #[test]
    fn video_cards_carry_duration_and_wide_stills() {
        let d = serde_json::json!({"id": 7, "title": "Clip", "duration": 161,
            "imageId": "66b0b90e-69f2-4cfb-8e6e-701b333f60c4",
            "artists": [{"id": 1, "name": "A"}, {"id": 2, "name": "B"}]});
        let c = parse_card("VIDEO", &d, 0).unwrap();
        assert_eq!(
            (c.kind.as_str(), c.subtitle.as_str(), c.duration),
            ("video", "A, B", 161)
        );
        assert!(c.image_url.unwrap().ends_with("/640x360.jpg"));
    }

    #[test]
    fn video_lists_parse_search_and_artist_entries_without_losing_metadata() {
        let video = serde_json::json!({
            "id": 7, "title": "Clip", "duration": 161,
            "imageId": "66b0b90e-69f2-4cfb-8e6e-701b333f60c4",
            "releaseDate": "2026-11-13",
            "artists": [{"id": 1, "name": "Artist"}]
        });
        let search = serde_json::json!({"videos": {"items": [video.clone(), {}]}});
        let artist = serde_json::json!({"items": [{"item": video}, null]});
        for data in [&search["videos"], &artist] {
            let cards = parse_video_list(data, 0);
            assert_eq!(cards.len(), 1);
            let card = &cards[0];
            assert_eq!(card.id, "7");
            assert_eq!(card.kind, "video");
            assert_eq!(card.title, "Clip");
            assert_eq!(card.by, "Artist");
            assert_eq!(card.duration, 161);
            assert_eq!(card.released, "2026-11-13");
            assert!(card.image_url.as_ref().unwrap().ends_with("/640x360.jpg"));
        }
    }

    #[test]
    fn missing_or_empty_video_results_are_empty_lists() {
        for data in [
            Value::Null,
            serde_json::json!({}),
            serde_json::json!({"items": []}),
            serde_json::json!({"items": [{"title": "No id"}, null]}),
        ] {
            assert!(parse_video_list(&data, 0).is_empty());
        }
    }

    #[test]
    fn base64_and_lrc() {
        assert_eq!(b64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        let lines = parse_lrc("[00:10.23] first\n[01:02.50] second\njunk");
        assert_eq!(
            lines,
            vec![
                (10_230, "first".to_string()),
                (62_500, "second".to_string())
            ]
        );
    }

    #[test]
    fn origin_url_only_for_dashed_uuids() {
        assert!(uuid_origin_url("hiphop").is_none());
        assert!(uuid_origin_url("aa-bb").is_none());
        assert_eq!(
            uuid_origin_url("fd953554-aaaa-bbbb-cccc-ddddeeeeffff").as_deref(),
            Some(
                "https://resources.tidal.com/images/fd953554/aaaa/bbbb/cccc/ddddeeeeffff/origin.jpg"
            )
        );
    }

    #[test]
    fn album_quality_badge_from_tags_and_field() {
        let hires: Value = serde_json::json!({"audioQuality": "LOSSLESS", "mediaMetadata": {"tags": ["HIRES_LOSSLESS"]}});
        assert_eq!(super::album_quality_badge(&hires), "MAX");
        // Adele 21: API still says LOSSLESS for 16-bit FLAC; the album page prints HIGH.
        let lossless: Value = serde_json::json!({"audioQuality": "LOSSLESS", "mediaMetadata": {"tags": ["LOSSLESS"]}});
        assert_eq!(super::album_quality_badge(&lossless), "HIGH");
        // API HIGH is AAC; the album page prints LOW.
        let aac: Value = serde_json::json!({"audioQuality": "HIGH"});
        assert_eq!(super::album_quality_badge(&aac), "LOW");
        let low: Value = serde_json::json!({"audioQuality": "LOW"});
        assert_eq!(super::album_quality_badge(&low), "LOW");
    }

    #[test]
    fn track_parsing_tolerates_missing_fields() {
        let v: Value = serde_json::json!({"id": 5, "title": "Song", "version": "Live",
            "artists": [{"id": 1, "name": "A"}, {"id": 2, "name": "B"}], "duration": 61,
            "mediaMetadata": {"tags": ["LOSSLESS", "HIRES_LOSSLESS"]}});
        let t = parse_track(&v).unwrap();
        assert_eq!(
            (t.title.as_str(), t.artist.as_str(), t.duration_secs),
            ("Song (Live)", "A, B", 61)
        );
        assert_eq!(t.audio_quality, "HI_RES_LOSSLESS");
        assert!(t.cover_url.is_none());
    }

    #[test]
    fn wimp_bio_keeps_link_text_and_paragraphs() {
        let raw = concat!(
            r#"The skilled U.K. genre-mashers [wimpLink artistId="4611746"]Glass Animals[/wimpLink] "#,
            r#"bring synth pop, indie, R&B, and hip-hop together. On 2014's "#,
            r#"[wimpLink albumId="30209241"]ZABA[/wimpLink], the single "Gooey." "#,
            r#"[wimpLink albumId="374785282"]I Love You So F***ing Much[/wimpLink]."#,
            r#"<br/>The son of a Welsh father met "#,
            r#"[wimpLink artistId="14686"]Missy Elliott[/wimpLink] and "#,
            r#"[wimpLink artistId="5226"]the Neptunes[/wimpLink]."#,
        );
        assert_eq!(
            super::plain_bio(raw),
            concat!(
                "The skilled U.K. genre-mashers Glass Animals bring synth pop, indie, ",
                "R&B, and hip-hop together. On 2014's ZABA, the single \"Gooey.\" ",
                "I Love You So F***ing Much.\n\n",
                "The son of a Welsh father met Missy Elliott and the Neptunes.",
            )
        );
    }

    #[test]
    fn wimp_bio_decodes_entities_and_is_idempotent() {
        let once = super::plain_bio(r#"R&amp;B with [wimpLink artistId="1"]A &amp; B[/wimpLink]"#);
        assert_eq!(once, "R&B with A & B");
        assert_eq!(super::plain_bio(&once), once);
        assert_eq!(super::plain_bio(""), "");
    }
}
