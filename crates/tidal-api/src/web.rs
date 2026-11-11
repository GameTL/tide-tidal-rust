//! Schema-tolerant TIDAL web API access.
//!
//! Everything here goes through untyped JSON so that upstream schema drift
//! degrades a single field instead of failing the whole response.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApiError, TrackItem};

const V1: &str = "https://api.tidal.com/v1";
const V2: &str = "https://api.tidal.com/v2";
const WEB_V2: &str = "https://tidal.com/v2";
const CLIENT_VERSION: &str = "2026.1.6";

/// A browsable thing: playlist, album, mix, artist, folder or deep link.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    /// "playlist" | "album" | "mix" | "artist" | "folder" | "tracks"
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    /// Square artwork, ~320px.
    pub image_url: Option<String>,
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
pub struct Lyrics {
    pub plain: String,
    /// (start time in ms, text). Empty when the provider has no timing.
    pub synced: Vec<(u64, String)>,
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

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("tidal-rust/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client")
    })
}

pub fn image_url(uuid: &str, size: u32) -> String {
    format!("https://resources.tidal.com/images/{}/{size}x{size}.jpg", uuid.replace('-', "/"))
}

fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn artist_names(v: &Value) -> String {
    let names: Vec<String> = v
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|x| s(x, "name")).filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    if names.is_empty() {
        v.get("artist").map(|a| s(a, "name")).unwrap_or_default()
    } else {
        names.join(", ")
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026-11-11T04:40:02.184+0000" -> "Today" / "Last week" / "Mar 2024".
pub fn relative_date(iso: &str) -> String {
    let mut parts = iso.get(..10).unwrap_or("").split('-').filter_map(|p| p.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return String::new();
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) / 86_400;
    let age = now - days_from_civil(y, m, d);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
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
        album: s(&album, "title"),
        album_id: Some(s(&album, "id")).filter(|x| !x.is_empty()),
        duration_secs: v.get("duration").and_then(Value::as_u64).unwrap_or(0) as u32,
        cover_url: (!cover.is_empty()).then(|| image_url(&cover, 160)),
        audio_quality: if hires { "HI_RES_LOSSLESS".into() } else { s(v, "audioQuality") },
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
                    if entry.get("type").and_then(Value::as_str).is_some_and(|t| t != "track") {
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
        let pick = list.iter().find(|i| i.get("size").is_some_and(|x| x == "MEDIUM")).or_else(|| list.first());
        return pick.map(|i| s(i, "url")).filter(|u| !u.is_empty());
    }
    let images = d.get("images")?;
    ["MEDIUM", "SMALL", "LARGE"].iter().find_map(|k| images.get(*k)).map(|i| s(i, "url")).filter(|u| !u.is_empty())
}

fn parse_card(kind: &str, d: &Value, me: u64) -> Option<Card> {
    let card = match kind {
        "ALBUM" => {
            let cover = s(d, "cover");
            Card {
                id: s(d, "id"),
                kind: "album".into(),
                title: s(d, "title"),
                subtitle: artist_names(d),
                image_url: (!cover.is_empty()).then(|| image_url(&cover, 320)),
            }
        }
        "PLAYLIST" => {
            let img = Some(s(d, "squareImage")).filter(|x| !x.is_empty());
            let creator = d.get("creator").cloned().unwrap_or(Value::Null);
            let by = if creator.get("id").and_then(Value::as_u64) == Some(me) {
                "Created by me".to_string()
            } else {
                let name = s(&creator, "name");
                if name.is_empty() { "Created by TIDAL".into() } else { format!("Created by {name}") }
            };
            Card {
                id: s(d, "uuid"),
                kind: "playlist".into(),
                title: s(d, "title"),
                subtitle: by,
                image_url: img.map(|u| image_url(&u, 320)),
            }
        }
        "MIX" => Card {
            id: s(d, "id"),
            kind: "mix".into(),
            title: text_info(d, &["titleTextInfo", "title"]),
            subtitle: text_info(d, &["subtitleTextInfo", "subTitleTextInfo", "subTitle", "shortSubtitleTextInfo"]),
            image_url: mix_image(d),
        },
        "ARTIST" => {
            let pic = s(d, "picture");
            Card {
                id: s(d, "id"),
                kind: "artist".into(),
                title: s(d, "name"),
                subtitle: "Artist".into(),
                image_url: (!pic.is_empty()).then(|| image_url(&pic, 320)),
            }
        }
        "DEEP_LINK" if s(d, "url").ends_with("my-collection/tracks") => {
            Card { id: "tracks".into(), kind: "tracks".into(), title: s(d, "title"), ..Default::default() }
        }
        _ => return None,
    };
    (!card.id.is_empty()).then_some(card)
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
    let Some(tpl_start) = mpd.find("<SegmentTemplate") else { return Vec::new() };
    let tpl_end = mpd[tpl_start..].find('>').map_or(mpd.len(), |e| e + tpl_start);
    let tpl = &mpd[tpl_start..tpl_end];
    let (Some(init), Some(media)) = (attr(tpl, "initialization"), attr(tpl, "media")) else { return Vec::new() };
    let first: u64 = attr(tpl, "startNumber").and_then(|n| n.parse().ok()).unwrap_or(1);

    let mut segments = 0u64;
    for piece in mpd.split("<S ").skip(1) {
        let tag = &piece[..piece.find('>').unwrap_or(piece.len())];
        segments += 1 + attr(tag, "r").and_then(|r| r.parse::<u64>().ok()).unwrap_or(0);
    }
    let mut urls = vec![unescape(init)];
    urls.extend((first..first + segments).map(|n| unescape(&media.replace("$Number$", &n.to_string()))));
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
        for (k, v) in [("countryCode", self.country.as_str()), ("locale", "en_US"), ("deviceType", "BROWSER")] {
            if !q.iter().any(|(key, _)| *key == k) {
                q.push((k, v));
            }
        }
        self.send(http().get(url).query(&q), url).await
    }

    /// One page of the user's favourite tracks, newest first, plus the total count.
    pub async fn favorite_tracks(&self, offset: u32, limit: u32) -> Result<(Vec<TrackItem>, u32), ApiError> {
        let url = format!("{V1}/users/{}/favorites/tracks", self.user_id);
        let (o, l) = (offset.to_string(), limit.to_string());
        let v = self.get(&url, &[("offset", &o), ("limit", &l), ("order", "DATE"), ("orderDirection", "DESC")]).await?;
        let total = v.get("totalNumberOfItems").and_then(Value::as_u64).unwrap_or(0) as u32;
        Ok((parse_track_list(&v), total))
    }

    pub async fn set_favorite(&self, track_id: &str, favorite: bool) -> Result<(), ApiError> {
        let q = [("countryCode", self.country.as_str())];
        if favorite {
            let url = format!("{V1}/users/{}/favorites/tracks", self.user_id);
            let form = [("trackIds", track_id), ("onArtifactNotFound", "FAIL")];
            self.send(http().post(&url).query(&q).form(&form), &url).await?;
        } else {
            let url = format!("{V1}/users/{}/favorites/tracks/{track_id}", self.user_id);
            self.send(http().delete(&url).query(&q), &url).await?;
        }
        Ok(())
    }

    pub async fn home(&self) -> Result<Vec<Section>, ApiError> {
        let url = format!("{WEB_V2}/home/feed/static");
        let v = self.get(&url, &[("limit", "20"), ("platform", "WEB"), ("timeOffset", "+00:00")]).await?;
        let modules = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(modules
            .iter()
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
                        section.cards.extend(parse_card(&kind, data, self.user_id));
                    }
                }
                (!section.cards.is_empty() || !section.tracks.is_empty()).then_some(section)
            })
            .collect())
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
            for it in v.get("items").and_then(Value::as_array).into_iter().flatten() {
                let d = it.get("data").unwrap_or(&Value::Null);
                if s(it, "itemType") == "FOLDER" {
                    let n = d.get("totalNumberOfItems").and_then(Value::as_u64).unwrap_or(0);
                    out.push(Card {
                        id: s(d, "id"),
                        kind: "folder".into(),
                        title: s(it, "name"),
                        subtitle: format!("{n} playlist{}", if n == 1 { "" } else { "s" }),
                        image_url: None,
                    });
                } else if let Some(mut card) = parse_card("PLAYLIST", d, self.user_id) {
                    let mine = d.pointer("/creator/id").and_then(Value::as_u64) == Some(self.user_id);
                    card.subtitle = if mine {
                        let n = d.get("numberOfTracks").and_then(Value::as_u64).unwrap_or(0)
                            + d.get("numberOfVideos").and_then(Value::as_u64).unwrap_or(0);
                        format!("{n} item{}", if n == 1 { "" } else { "s" })
                    } else {
                        let name = d.pointer("/creator/name").and_then(Value::as_str).unwrap_or("");
                        if name.is_empty() { "TIDAL".into() } else { name.to_string() }
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
        let v = self.get(&format!("{V2}/favorites/mixes"), &[("limit", "50")]).await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|d| parse_card("MIX", d, self.user_id)).collect())
            .unwrap_or_default())
    }

    pub async fn albums(&self) -> Result<Vec<Card>, ApiError> {
        let url = format!("{V1}/users/{}/favorites/albums", self.user_id);
        let v = self.get(&url, &[("limit", "100"), ("order", "DATE"), ("orderDirection", "DESC")]).await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().filter_map(|e| parse_card("ALBUM", e.get("item")?, self.user_id)).collect()
            })
            .unwrap_or_default())
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
        Ok(parse_track_list(&self.get(&url, &[("limit", limit), ("offset", "0")]).await?))
    }

    pub async fn search_tracks(&self, query: &str) -> Result<Vec<TrackItem>, ApiError> {
        let v = self.get(&format!("{V1}/search"), &[("query", query), ("types", "TRACKS"), ("limit", "50")]).await?;
        Ok(v.get("tracks").map(parse_track_list).unwrap_or_default())
    }

    pub async fn track(&self, id: &str) -> Result<TrackItem, ApiError> {
        let v = self.get(&format!("{V1}/tracks/{id}"), &[]).await?;
        parse_track(&v).ok_or_else(|| ApiError::Client(format!("track {id} not found")))
    }

    pub async fn lyrics(&self, track_id: &str) -> Result<Lyrics, ApiError> {
        let v = self.get(&format!("{V1}/tracks/{track_id}/lyrics"), &[]).await?;
        Ok(Lyrics { plain: s(&v, "lyrics"), synced: parse_lrc(&s(&v, "subtitles")) })
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
            let manifest = b64_decode(&s(&v, "manifest")).and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
            let urls = if s(&v, "manifestMimeType").contains("dash") {
                parse_dash(&manifest)
            } else {
                serde_json::from_str::<Value>(&manifest)
                    .ok()
                    .and_then(|m| m.get("urls").and_then(Value::as_array).cloned())
                    .map(|a| a.iter().filter_map(|u| u.as_str().map(String::from)).collect())
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
                sample_rate: v.get("sampleRate").and_then(Value::as_u64).unwrap_or(44_100) as u32,
            });
        }
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn base64_and_lrc() {
        assert_eq!(b64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        let lines = parse_lrc("[00:10.23] first\n[01:02.50] second\njunk");
        assert_eq!(lines, vec![(10_230, "first".to_string()), (62_500, "second".to_string())]);
    }

    #[test]
    fn track_parsing_tolerates_missing_fields() {
        let v: Value = serde_json::json!({"id": 5, "title": "Song", "version": "Live",
            "artists": [{"id": 1, "name": "A"}, {"id": 2, "name": "B"}], "duration": 61,
            "mediaMetadata": {"tags": ["LOSSLESS", "HIRES_LOSSLESS"]}});
        let t = parse_track(&v).unwrap();
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.duration_secs), ("Song (Live)", "A, B", 61));
        assert_eq!(t.audio_quality, "HI_RES_LOSSLESS");
        assert!(t.cover_url.is_none());
    }
}
