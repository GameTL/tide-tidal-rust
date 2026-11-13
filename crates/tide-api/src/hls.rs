//! The audio of a TIDAL music video, taken from its HLS rendition.
//!
//! AVFoundation plays the video, but it will not hand over the audio of an
//! HLS stream (audio mixes and processing taps are ignored for HLS). To send
//! the soundtrack through the app's own output (DSP, bit-perfect device rate)
//! the app fetches the same rendition's MPEG-TS segments and pulls the AAC
//! (ADTS) elementary stream out of them here.

use crate::ApiError;

/// One media segment of a rendition playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct HlsSegment {
    /// Where the segment starts on the player's timeline, in seconds.
    pub start: f64,
    pub duration: f64,
    pub url: String,
}

/// The segments of an HLS media (rendition) playlist, in order. Relative
/// segment URIs are resolved against `base`, the playlist's own URL.
pub fn parse_media_playlist(text: &str, base: &str) -> Vec<HlsSegment> {
    let mut out = Vec::new();
    let mut start = 0.0;
    let mut pending: Option<f64> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let secs = rest.split(',').next().unwrap_or("").trim();
            pending = secs
                .parse::<f64>()
                .ok()
                .filter(|d| d.is_finite() && *d > 0.0);
        } else if !line.is_empty() && !line.starts_with('#') {
            let Some(duration) = pending.take() else {
                continue;
            };
            out.push(HlsSegment {
                start,
                duration,
                url: resolve(base, line),
            });
            start += duration;
        }
    }
    out
}

fn resolve(base: &str, uri: &str) -> String {
    if uri.contains("://") {
        return uri.to_string();
    }
    let path_end = base.find('?').unwrap_or(base.len());
    let base = &base[..path_end];
    if let Some(rest) = uri.strip_prefix('/') {
        let origin_end = base
            .find("://")
            .and_then(|i| base[i + 3..].find('/').map(|j| i + 3 + j))
            .unwrap_or(base.len());
        return format!("{}/{rest}", &base[..origin_end]);
    }
    match base.rfind('/') {
        Some(i) => format!("{}/{uri}", &base[..i]),
        None => uri.to_string(),
    }
}

/// The index of the segment that holds `secs` (the last one past the end).
pub fn segment_at(segments: &[HlsSegment], secs: f64) -> usize {
    segments.iter().rposition(|s| s.start <= secs).unwrap_or(0)
}

/// What one MPEG-TS segment carries for the soundtrack.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TsAudio {
    /// The AAC elementary stream (ADTS frames), ready to append to the last.
    pub adts: Vec<u8>,
    /// Presentation time of the segment's first audio and video frame, in
    /// seconds (90 kHz clock).
    pub audio_pts: Option<f64>,
    pub video_pts: Option<f64>,
}

impl TsAudio {
    /// How far the first audio frame sits from the segment's start on the
    /// player's timeline, which begins at the first video frame.
    pub fn audio_lead(&self) -> f64 {
        match (self.audio_pts, self.video_pts) {
            (Some(a), Some(v)) => a - v,
            _ => 0.0,
        }
    }
}

const TS_PACKET: usize = 188;
const STREAM_AAC_ADTS: u8 = 0x0F;

/// Pull the AAC audio and the first audio / video timestamps out of one
/// MPEG-TS segment.
pub fn demux_ts(data: &[u8]) -> TsAudio {
    let mut out = TsAudio::default();
    let mut pmt_pid = None;
    let mut audio_pid = None;
    let mut video_pid = None;
    // Bytes of the PES packet being gathered for the audio PID.
    let mut pes: Vec<u8> = Vec::new();
    let flush = |pes: &mut Vec<u8>, out: &mut TsAudio| {
        if let Some((pts, payload)) = parse_pes(pes) {
            if out.audio_pts.is_none() {
                out.audio_pts = pts;
            }
            out.adts.extend_from_slice(payload);
        }
        pes.clear();
    };
    for packet in data.as_chunks::<TS_PACKET>().0 {
        if packet[0] != 0x47 {
            continue;
        }
        let start = packet[1] & 0x40 != 0;
        let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
        let control = (packet[3] >> 4) & 0x3;
        if control & 0x1 == 0 {
            continue; // no payload
        }
        let mut at = 4;
        if control & 0x2 != 0 {
            at += 1 + packet[4] as usize;
        }
        if at >= TS_PACKET {
            continue;
        }
        let payload = &packet[at..];
        if pid == 0 && start {
            pmt_pid = parse_pat(payload).or(pmt_pid);
        } else if Some(pid) == pmt_pid && start {
            if let Some((audio, video)) = parse_pmt(payload) {
                audio_pid = audio.or(audio_pid);
                video_pid = video.or(video_pid);
            }
        } else if Some(pid) == audio_pid {
            if start {
                flush(&mut pes, &mut out);
            }
            pes.extend_from_slice(payload);
        } else if Some(pid) == video_pid && start && out.video_pts.is_none() {
            out.video_pts = parse_pes(payload).and_then(|(pts, _)| pts);
        }
    }
    flush(&mut pes, &mut out);
    out
}

/// The PMT's PID from a PAT section (the first programme's).
fn parse_pat(payload: &[u8]) -> Option<u16> {
    let section = payload.get(1 + *payload.first()? as usize..)?;
    let length = ((usize::from(section.get(1)? & 0x0F)) << 8) | usize::from(*section.get(2)?);
    let entries = section.get(8..(3 + length).saturating_sub(4))?;
    entries.as_chunks::<4>().0.iter().find_map(|e| {
        let program = u16::from_be_bytes([e[0], e[1]]);
        (program != 0).then(|| (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]))
    })
}

/// (AAC audio PID, video PID) from a PMT section.
fn parse_pmt(payload: &[u8]) -> Option<(Option<u16>, Option<u16>)> {
    let section = payload.get(1 + *payload.first()? as usize..)?;
    let length = ((usize::from(section.get(1)? & 0x0F)) << 8) | usize::from(*section.get(2)?);
    let info_len = ((usize::from(section.get(10)? & 0x0F)) << 8) | usize::from(*section.get(11)?);
    let end = (3 + length).saturating_sub(4).min(section.len());
    let mut at = 12 + info_len;
    let (mut audio, mut video) = (None, None);
    while at + 5 <= end {
        let kind = section[at];
        let pid = (u16::from(section[at + 1] & 0x1F) << 8) | u16::from(section[at + 2]);
        let es_len = ((usize::from(section[at + 3] & 0x0F)) << 8) | usize::from(section[at + 4]);
        match kind {
            STREAM_AAC_ADTS if audio.is_none() => audio = Some(pid),
            // H.264, HEVC, MPEG-2 video
            0x1B | 0x24 | 0x02 if video.is_none() => video = Some(pid),
            _ => {}
        }
        at += 5 + es_len;
    }
    Some((audio, video))
}

/// (PTS in seconds, payload) of a PES packet.
fn parse_pes(pes: &[u8]) -> Option<(Option<f64>, &[u8])> {
    if pes.len() < 9 || pes[..3] != [0, 0, 1] {
        return None;
    }
    let header_len = pes[8] as usize;
    let payload = pes.get(9 + header_len..)?;
    let pts = (pes[7] & 0x80 != 0 && header_len >= 5).then(|| {
        let b = &pes[9..14];
        let ticks = (u64::from(b[0] >> 1 & 0x07) << 30)
            | (u64::from(b[1]) << 22)
            | (u64::from(b[2] >> 1) << 15)
            | (u64::from(b[3]) << 7)
            | u64::from(b[4] >> 1);
        ticks as f64 / 90_000.0
    });
    Some((pts, payload))
}

/// Fetch a rendition playlist (pre-signed, no token needed).
pub async fn fetch_media_playlist(url: &str) -> Result<Vec<HlsSegment>, ApiError> {
    let text = crate::web::http()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(parse_media_playlist(&text, url))
}

/// Fetch one segment and take its soundtrack out.
pub async fn fetch_segment_audio(url: &str) -> Result<TsAudio, ApiError> {
    let bytes = crate::web::http()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok(demux_ts(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_playlist_lists_segments_on_one_timeline() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXTINF:6.006,\nseg0.ts?sig=a\n#EXTINF:6.006,\nhttps://cdn.example/seg1.ts\n#EXTINF:3.5,\n/abs/seg2.ts\n#EXT-X-ENDLIST\n";
        let segs = parse_media_playlist(text, "https://host.example/a/b/list.m3u8?x=1");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].url, "https://host.example/a/b/seg0.ts?sig=a");
        assert_eq!(segs[1].url, "https://cdn.example/seg1.ts");
        assert_eq!(segs[2].url, "https://host.example/abs/seg2.ts");
        assert!((segs[2].start - 12.012).abs() < 1e-9);
        assert_eq!(segment_at(&segs, 0.0), 0);
        assert_eq!(segment_at(&segs, 6.5), 1);
        assert_eq!(segment_at(&segs, 99.0), 2);
    }

    /// A 188-byte TS packet carrying `payload` (padded with an adaptation field).
    fn packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![
            0x47,
            (if start { 0x40 } else { 0 }) | (pid >> 8) as u8,
            pid as u8,
        ];
        let stuffing = 184 - payload.len();
        if stuffing == 0 {
            p.push(0x10);
        } else {
            p.push(0x30);
            p.push((stuffing - 1) as u8);
            if stuffing > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), 188);
        p
    }

    fn pts_bytes(ticks: u64) -> [u8; 5] {
        [
            0x21 | ((ticks >> 29) & 0x0E) as u8,
            (ticks >> 22) as u8,
            0x01 | ((ticks >> 14) & 0xFE) as u8,
            (ticks >> 7) as u8,
            0x01 | ((ticks << 1) & 0xFE) as u8,
        ]
    }

    fn pes(stream: u8, ticks: u64, body: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 1, stream, 0, 0, 0x80, 0x80, 5];
        p.extend_from_slice(&pts_bytes(ticks));
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn ts_demux_keeps_the_aac_stream_and_first_timestamps() {
        // PAT: programme 1 -> PMT PID 0x1000.
        let pat = [
            0, 0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, 0xF0, 0x00, 0, 0, 0, 0,
        ];
        // PMT: AAC on 0x22, H.264 on 0x21.
        let pmt = [
            0, 0x02, 0xB0, 23, 0, 1, 0xC1, 0, 0, 0xE0, 0x21, 0xF0, 0, 0x0F, 0xE0, 0x22, 0xF0, 0,
            0x1B, 0xE0, 0x21, 0xF0, 0, 0, 0, 0, 0,
        ];
        let mut ts = Vec::new();
        ts.extend(packet(0, true, &pat));
        ts.extend(packet(0x1000, true, &pmt));
        ts.extend(packet(0x21, true, &pes(0xE0, 900_000, &[9; 20])));
        let audio = pes(0xC0, 895_816, &[1, 2, 3]);
        ts.extend(packet(0x22, true, &audio));
        ts.extend(packet(0x22, false, &[4, 5]));
        ts.extend(packet(0x22, true, &pes(0xC0, 897_816, &[6])));
        let out = demux_ts(&ts);
        assert_eq!(out.adts, [1, 2, 3, 4, 5, 6]);
        assert!((out.video_pts.unwrap() - 10.0).abs() < 1e-9);
        assert!((out.audio_pts.unwrap() - 9.953_511).abs() < 1e-5);
        assert!((out.audio_lead() + 0.046_488).abs() < 1e-5);
    }
}
