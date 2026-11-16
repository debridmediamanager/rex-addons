//! Subtitles through an addon: rex asks it to find some for a title, or to fetch one. The addon
//! only talks to its website; the host bounds each call, checks what comes back, downloads and
//! unpacks the file itself (most sites serve ZIPs, often from a CDN other than their API, and
//! answer 429 to parallel requests), decodes it to UTF-8, and keeps answers in the addon's own
//! `cache/` folder, so the same question reaches the website once.
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Map, Value as Json};

use crate::addons::{read_json, write_atomic, Manifest};
use crate::app::{self, call_ctx, log, masked, warn, App};
use crate::net::{self, Hosts};
use crate::sandbox::{self, Storage};

/// The formats served: text ones Plex players read. VobSub and PGS are pictures.
pub const FORMATS: [&str; 5] = ["srt", "ass", "ssa", "vtt", "sub"];
/// The most candidates one search answers; past it the answer says `truncated`.
const MAX_CANDIDATES: usize = 200;
/// One of an addon's caches: its folder under `cache/`, how long an answer in it is reused, and
/// how much it holds, the oldest going first.
pub struct Shelf {
    pub kind: &'static str,
    pub ttl: Duration,
    pub most: u64,
}

/// Searches, as sites add files slowly and limit how often they're asked; files served.
const SEARCHES: Shelf = Shelf { kind: "search", ttl: Duration::from_secs(12 * 3600), most: 16 << 20 };
const FILES: Shelf = Shelf { kind: "files", ttl: Duration::from_secs(7 * 86400), most: 64 << 20 };
/// What the host downloads for one subtitle, and what one unpacks to at most: a film's SRT is
/// about 150 KB (SubDL's for His Girl Friday, 2026-11-16), an ASS with its fonts a few MB.
const MAX_FILE: u64 = 16 << 20;
/// Entries a ZIP may list.
const MAX_ENTRIES: usize = 4096;
/// The download after `subtitle()` answered, in all from its turn at the website, waits included.
const DOWNLOAD_TIME: Duration = Duration::from_secs(30);
/// A 429 or 503 is waited out this long at most, and a download tried this often.
const MAX_WAIT: Duration = Duration::from_secs(10);
const TRIES: usize = 3;

/// A language code as BCP 47 writes it, its case fixed: `en`, `pt-BR`, `zh-Hant`, `es-419`, or
/// ISO 639-2's three letters (`por`). None for anything else, such as `English`.
pub fn language(code: &str) -> Option<String> {
    let mut parts = code.trim().split(['-', '_']);
    let primary = parts.next().filter(|p| (2..=3).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphabetic()))?;
    let mut fixed = primary.to_ascii_lowercase();
    for part in parts {
        let alpha = part.bytes().all(|b| b.is_ascii_alphabetic());
        fixed.push('-');
        match part.len() {
            2 if alpha => fixed.push_str(&part.to_ascii_uppercase()),
            3 if part.bytes().all(|b| b.is_ascii_digit()) => fixed.push_str(part),
            4 if alpha => fixed.push_str(&format!("{}{}", part[..1].to_ascii_uppercase(), part[1..].to_ascii_lowercase())),
            5..=8 if part.bytes().all(|b| b.is_ascii_alphanumeric()) => fixed.push_str(&part.to_ascii_lowercase()),
            _ => return None,
        }
    }
    Some(fixed)
}

/// A search's query string as `subtitles()` gets it: checked, its fields always in one order and
/// its languages a list, so the same question is one cache entry however it was asked.
pub fn search_query(query: &str) -> Result<Json, String> {
    let (mut found, mut languages) = (Map::new(), Vec::<String>::new());
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let number = || value.parse::<u64>().map(Json::from).map_err(|_| format!("{key} must be a whole number"));
        let checked = match key.as_ref() {
            "type" if matches!(value.as_ref(), "movie" | "episode") => json!(value),
            "type" => return Err("type must be movie or episode".into()),
            "imdb" if value.len() > 2 && value.len() <= 12 && value.starts_with("tt") && value[2..].bytes().all(|b| b.is_ascii_digit()) => json!(value),
            "imdb" => return Err("imdb must be an IMDb id, such as tt0032599".into()),
            "tmdb" | "season" | "episode" | "size" => number()?,
            "hash" if (1..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit()) => json!(value.to_ascii_lowercase()),
            "hash" => return Err("hash must be hexadecimal".into()),
            "filename" if !value.is_empty() && value.len() <= 255 => json!(value),
            "filename" => return Err("filename must be a file name, at most 255 bytes".into()),
            "lang" => {
                for code in value.split(',').map(str::trim).filter(|code| !code.is_empty()) {
                    let code = language(code).ok_or(format!("lang: {code} is no language code, such as en or pt-BR"))?;
                    if !languages.contains(&code) {
                        languages.push(code);
                    }
                }
                continue;
            }
            _ => return Err(format!("{key} is no search field: type, imdb, tmdb, season, episode, lang, hash, size or filename")),
        };
        found.insert(key.into_owned(), checked);
    }
    match found.get("type").and_then(Json::as_str) {
        None => return Err("give type: movie or episode".into()),
        Some("episode") if !(found.contains_key("season") && found.contains_key("episode")) => return Err("an episode needs season and episode".into()),
        Some("movie") if found.contains_key("season") || found.contains_key("episode") => return Err("a movie has no season or episode".into()),
        _ => {}
    }
    if !["imdb", "tmdb", "hash", "filename"].iter().any(|key| found.contains_key(*key)) {
        return Err("give imdb, tmdb, hash or filename".into());
    }
    let mut ordered: Map<String, Json> =
        ["type", "imdb", "tmdb", "season", "episode", "hash", "size", "filename"].iter().filter_map(|key| Some((key.to_string(), found.remove(*key)?))).collect();
    ordered.insert("languages".into(), json!(languages));
    Ok(Json::Object(ordered))
}

/// What `subtitles()` returned, kept to what rex can use: objects with an `id` and a language
/// code, each id once, at most MAX_CANDIDATES, in the fields rex reads. The candidates, why each
/// entry left out was, and whether there were more than it keeps.
pub fn candidates(returned: Json) -> Result<(Vec<Json>, Vec<String>, bool), String> {
    let Json::Array(list) = returned else { return Err("subtitles() must return a list".into()) };
    let (mut kept, mut left_out, mut ids) = (Vec::new(), Vec::new(), HashSet::new());
    for entry in list {
        if kept.len() == MAX_CANDIDATES {
            return Ok((kept, left_out, true));
        }
        let Json::Object(entry) = entry else {
            left_out.push("subtitles(): an entry is not an object; left out".to_string());
            continue;
        };
        let Some(id) = entry.get("id").and_then(Json::as_str).filter(|id| !id.is_empty() && id.len() <= 1024) else {
            left_out.push("subtitles(): an entry has no id; left out".to_string());
            continue;
        };
        let Some(lang) = entry.get("lang").and_then(Json::as_str).and_then(language) else {
            left_out.push(format!("subtitles(): {id} has the lang {}, which is no language code such as en or pt-BR; left out", entry.get("lang").unwrap_or(&Json::Null)));
            continue;
        };
        let format = match entry.get("format") {
            None | Some(Json::Null) => None,
            Some(Json::String(format)) if FORMATS.contains(&format.to_ascii_lowercase().as_str()) => Some(format.to_ascii_lowercase()),
            Some(other) => {
                left_out.push(format!("subtitles(): {id} has the format {other}, which rex-addons doesn't serve (srt, ass, ssa, vtt, sub); left out"));
                continue;
            }
        };
        if !ids.insert(id.to_string()) {
            left_out.push(format!("subtitles(): {id} repeats an id; left out"));
            continue;
        }
        let text = |key: &str, most: usize| entry.get(key).and_then(Json::as_str).map(|text| text.chars().take(most).collect::<String>());
        kept.push(json!({
            "id": id, "lang": lang, "release": text("release", 500), "provider": text("provider", 100), "format": format,
            "hi": entry.get("hi") == Some(&json!(true)), "forced": entry.get("forced") == Some(&json!(true)),
            "fps": entry.get("fps").and_then(Json::as_f64).filter(|fps| *fps > 0.0 && *fps < 1000.0),
            "downloads": entry.get("downloads").and_then(Json::as_u64),
        }));
    }
    Ok((kept, left_out, false))
}

/// A subtitle as served: its text, in UTF-8, and its format, one of FORMATS.
#[derive(Debug, PartialEq)]
pub struct Subtitle {
    pub text: String,
    pub format: &'static str,
}

/// What a downloaded file holds as a subtitle: unpacked (gzip, ZIP), decoded to UTF-8 and its
/// format found from its text. A ZIP's entry is the one named `name`, else the one for `episode`
/// (season, episode), else the only or largest subtitle in it. `lang` is the language it's in.
pub fn subtitle_of(bytes: &[u8], name: Option<&str>, episode: Option<(u64, u64)>, lang: Option<&str>) -> Result<Subtitle, String> {
    let text = decode(&unpack(bytes, name, episode)?, lang)?;
    let format = format_of(&text).ok_or("not a subtitle rex-addons serves: no SubRip, WebVTT, ASS, SSA or MicroDVD in it")?;
    Ok(Subtitle { text, format })
}

fn bounded(bytes: Vec<u8>, what: &str) -> Result<Vec<u8>, String> {
    match bytes.len() as u64 > MAX_FILE {
        true => Err(format!("{what} is over {} MB", MAX_FILE >> 20)),
        false => Ok(bytes),
    }
}

fn unpack(bytes: &[u8], name: Option<&str>, episode: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
    if bytes.starts_with(b"\x1f\x8b") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes).take(MAX_FILE + 1).read_to_end(&mut out).map_err(|e| format!("a damaged gzip file: {e}"))?;
        return bounded(out, "the gzip file, unpacked,");
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        if bytes.len() < 22 {
            return Err("a damaged ZIP: no central directory".into());
        }
        return unzip(bytes, name, episode);
    }
    for (magic, kind) in [(&b"Rar!\x1a\x07"[..], "RAR"), (b"7z\xbc\xaf\x27\x1c", "7z")] {
        if bytes.starts_with(magic) {
            return Err(format!("a {kind} archive, which rex-addons doesn't open: only ZIP and gzip"));
        }
    }
    Ok(bytes.to_vec())
}

/// One file a ZIP's central directory lists.
struct Entry {
    name: String,
    flags: u16,
    method: u16,
    crc: u32,
    packed: usize,
    size: usize,
    offset: usize,
}

fn le16(bytes: &[u8], at: usize) -> usize {
    u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize
}

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

/// A ZIP's entries, from its central directory: the local headers of one written as a stream
/// carry no sizes (they come after the data).
fn entries(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let damaged = || "a damaged ZIP: its central directory runs past its end".to_string();
    // Its end record is last, but for a comment of up to 65,535 bytes (SubDL's say "Subscene").
    let last = bytes.len().saturating_sub(22);
    let end = (last.saturating_sub(0xffff)..=last).rev().find(|&at| bytes[at..].starts_with(b"PK\x05\x06")).ok_or("a damaged ZIP: no central directory")?;
    let (count, mut at) = (le16(bytes, end + 10), le32(bytes, end + 16));
    if count == 0xffff || at == 0xffff_ffff {
        return Err("a ZIP64 archive, which rex-addons doesn't open".into());
    }
    if count > MAX_ENTRIES {
        return Err(format!("a ZIP of over {MAX_ENTRIES} files"));
    }
    let mut found = Vec::with_capacity(count);
    for _ in 0..count {
        let header = bytes.get(at..at + 46).filter(|h| h.starts_with(b"PK\x01\x02")).ok_or_else(damaged)?;
        let (name_length, rest) = (le16(header, 28), le16(header, 30) + le16(header, 32));
        let (packed, size, offset) = (le32(header, 20), le32(header, 24), le32(header, 42));
        if [packed, size, offset].contains(&0xffff_ffff) {
            return Err("a ZIP64 archive, which rex-addons doesn't open".into());
        }
        // UTF-8 when flag 11 says so, else CP437, whose other letters only a person reads.
        let name = String::from_utf8_lossy(bytes.get(at + 46..at + 46 + name_length).ok_or_else(damaged)?).into_owned();
        found.push(Entry { name, flags: le16(header, 8) as u16, method: le16(header, 10) as u16, crc: le32(header, 16) as u32, packed, size, offset });
        at += 46 + name_length + rest;
    }
    Ok(found)
}

/// One entry's bytes: stored or deflated, never more than MAX_FILE whatever its header says,
/// and checked against its size and CRC.
fn read(bytes: &[u8], entry: &Entry) -> Result<Vec<u8>, String> {
    let name = &entry.name;
    if entry.flags & 1 != 0 {
        return Err(format!("{name} in the ZIP is encrypted"));
    }
    let damaged = || format!("{name} in the ZIP is damaged");
    let local = bytes.get(entry.offset..entry.offset + 30).filter(|h| h.starts_with(b"PK\x03\x04")).ok_or_else(damaged)?;
    let start = entry.offset + 30 + le16(local, 26) + le16(local, 28);
    let data = bytes.get(start..start + entry.packed).ok_or_else(damaged)?;
    let out = match entry.method {
        0 => data.to_vec(),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(data).take(MAX_FILE + 1).read_to_end(&mut out).map_err(|_| damaged())?;
            out
        }
        method => return Err(format!("{name} in the ZIP is packed with method {method}, which rex-addons doesn't read: only stored and deflated")),
    };
    let out = bounded(out, &format!("{name}, unpacked,"))?;
    let mut crc = flate2::Crc::new();
    crc.update(&out);
    if out.len() != entry.size || crc.sum() != entry.crc {
        return Err(damaged());
    }
    Ok(out)
}

fn unzip(bytes: &[u8], name: Option<&str>, episode: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
    let entries = entries(bytes)?;
    // Nothing is written to disk, but an archive with a path that leaves it is no archive to trust.
    let escapes = |e: &&Entry| e.name.starts_with(['/', '\\']) || e.name.get(1..2) == Some(":") || e.name.split(['/', '\\']).any(|part| part == "..");
    if let Some(entry) = entries.iter().find(escapes) {
        return Err(format!("the ZIP has a file whose path leaves it: {}", entry.name));
    }
    let base = |e: &Entry| e.name.rsplit(['/', '\\']).next().unwrap_or_default().to_string();
    let split = |e: &Entry| e.name.rsplit_once('.').map(|(stem, ext)| (stem.to_string(), ext.to_ascii_lowercase())).unwrap_or_default();
    // A .sub beside an .idx of its name is VobSub: pictures, not text.
    let vobsub = |e: &Entry| split(e).1 == "sub" && entries.iter().any(|other| split(other) == (split(e).0, "idx".to_string()));
    // A Mac's AppleDouble files (`__MACOSX/`, `._<name>`) carry a file's attributes, not its text.
    let apple = |e: &Entry| e.name.starts_with("__MACOSX/") || base(e).starts_with("._");
    let subtitles: Vec<&Entry> = entries.iter().filter(|e| FORMATS.contains(&split(e).1.as_str()) && !vobsub(e) && !apple(e)).collect();
    if subtitles.is_empty() {
        return Err(match entries.iter().any(|e| matches!(split(e).1.as_str(), "zip" | "rar" | "7z")) {
            true => "the ZIP holds no subtitle, only another archive, which rex-addons doesn't open".into(),
            false => "the ZIP holds no subtitle (.srt, .ass, .ssa, .vtt or .sub)".into(),
        });
    }
    // The first of the largest.
    fn largest(list: Vec<&Entry>) -> Option<&Entry> {
        list.into_iter().rev().max_by_key(|e| e.size)
    }
    let chosen = match (name, episode) {
        (Some(name), _) => subtitles.iter().copied().find(|e| e.name.eq_ignore_ascii_case(name) || base(e).eq_ignore_ascii_case(name)).ok_or(format!("the ZIP holds no {name}"))?,
        (None, Some((season, number))) => {
            let named: Vec<&Entry> = subtitles.iter().copied().filter(|e| episode_of(&base(e)) == Some((season, number))).collect();
            match subtitles.as_slice() {
                _ if !named.is_empty() => largest(named).unwrap(),
                // One subtitle, named for no episode: the episode's own archive.
                [only] if episode_of(&base(only)).is_none() => *only,
                // The largest would be another episode's.
                _ => return Err(format!("the ZIP holds no subtitle for S{season:02}E{number:02}")),
            }
        }
        (None, None) => largest(subtitles).unwrap(),
    };
    read(bytes, chosen)
}

/// The season and episode a file name says: `S01E02`, `s1.e2`, `S01 E02` or `1x02`.
fn episode_of(name: &str) -> Option<(u64, u64)> {
    let b = name.to_ascii_lowercase().into_bytes();
    // A number of 1 to `most` digits at `from`, not part of a longer one, and where it ends.
    let number = |from: usize, most: usize| -> Option<(u64, usize)> {
        let n = b.get(from..)?.iter().take_while(|c| c.is_ascii_digit()).count();
        (1..=most).contains(&n).then(|| (std::str::from_utf8(&b[from..from + n]).unwrap().parse().unwrap(), from + n))
    };
    for at in 0..b.len() {
        if at > 0 && b[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        if b[at] == b's' {
            if let Some((season, mut next)) = number(at + 1, 2) {
                if matches!(b.get(next), Some(b'.' | b' ' | b'_' | b'-')) {
                    next += 1;
                }
                if let Some((episode, _)) = (b.get(next) == Some(&b'e')).then(|| number(next + 1, 3)).flatten() {
                    return Some((season, episode));
                }
            }
        } else if let Some((season, next)) = number(at, 2) {
            if let Some((episode, _)) = (b.get(next) == Some(&b'x')).then(|| number(next + 1, 3)).flatten() {
                return Some((season, episode));
            }
        }
    }
    None
}

/// Windows code pages' 0x80 to 0xFF, the older encodings subtitle files still come in.
#[rustfmt::skip]
const WINDOWS_1254: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD, 0xFFFD, 0xFFFD,
    0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0xFFFD, 0xFFFD, 0x0178,
    0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7, 0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x011E, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x0130, 0x015E, 0x00DF,
    0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x011F, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7, 0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x0131, 0x015F, 0x00FF,
];
#[rustfmt::skip]
const WINDOWS_1250: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0xFFFD, 0x201E, 0x2026, 0x2020, 0x2021, 0xFFFD, 0x2030, 0x0160, 0x2039, 0x015A, 0x0164, 0x017D, 0x0179,
    0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0x0161, 0x203A, 0x015B, 0x0165, 0x017E, 0x017A,
    0x00A0, 0x02C7, 0x02D8, 0x0141, 0x00A4, 0x0104, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x015E, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x017B,
    0x00B0, 0x00B1, 0x02DB, 0x0142, 0x00B4, 0x00B5, 0x00B6, 0x00B7, 0x00B8, 0x0105, 0x015F, 0x00BB, 0x013D, 0x02DD, 0x013E, 0x017C,
    0x0154, 0x00C1, 0x00C2, 0x0102, 0x00C4, 0x0139, 0x0106, 0x00C7, 0x010C, 0x00C9, 0x0118, 0x00CB, 0x011A, 0x00CD, 0x00CE, 0x010E,
    0x0110, 0x0143, 0x0147, 0x00D3, 0x00D4, 0x0150, 0x00D6, 0x00D7, 0x0158, 0x016E, 0x00DA, 0x0170, 0x00DC, 0x00DD, 0x0162, 0x00DF,
    0x0155, 0x00E1, 0x00E2, 0x0103, 0x00E4, 0x013A, 0x0107, 0x00E7, 0x010D, 0x00E9, 0x0119, 0x00EB, 0x011B, 0x00ED, 0x00EE, 0x010F,
    0x0111, 0x0144, 0x0148, 0x00F3, 0x00F4, 0x0151, 0x00F6, 0x00F7, 0x0159, 0x016F, 0x00FA, 0x0171, 0x00FC, 0x00FD, 0x0163, 0x02D9,
];
#[rustfmt::skip]
const WINDOWS_1251: [u16; 128] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F,
    0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F,
    0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
    0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457,
    0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F,
    0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F,
    0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E, 0x043F,
    0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447, 0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F,
];
#[rustfmt::skip]
const WINDOWS_1252: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD, 0x017D, 0xFFFD,
    0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0xFFFD, 0x017E, 0x0178,
    0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7, 0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF,
    0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7, 0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
];

/// ISO 639-2's codes, bibliographic and terminology, of the languages whose code page or script
/// is told apart below, and OpenSubtitles' `pob`, by their ISO 639-1 code.
const THREE_LETTERS: [(&str, &str); 74] = [
    ("alb", "sq"), ("sqi", "sq"), ("amh", "am"), ("ara", "ar"), ("arm", "hy"), ("hye", "hy"), ("asm", "as"), ("aze", "az"),
    ("bel", "be"), ("ben", "bn"), ("bos", "bs"), ("bul", "bg"), ("bur", "my"), ("mya", "my"), ("ces", "cs"), ("cze", "cs"),
    ("chi", "zh"), ("zho", "zh"), ("ell", "el"), ("gre", "el"), ("fas", "fa"), ("per", "fa"), ("geo", "ka"), ("kat", "ka"),
    ("guj", "gu"), ("heb", "he"), ("hin", "hi"), ("hrv", "hr"), ("scr", "hr"), ("hun", "hu"), ("jpn", "ja"), ("kan", "kn"),
    ("kaz", "kk"), ("khm", "km"), ("kir", "ky"), ("kor", "ko"), ("lao", "lo"), ("mac", "mk"), ("mkd", "mk"), ("mal", "ml"),
    ("mar", "mr"), ("mon", "mn"), ("nep", "ne"), ("pan", "pa"), ("pol", "pl"), ("pus", "ps"), ("rum", "ro"), ("ron", "ro"),
    ("rus", "ru"), ("san", "sa"), ("sin", "si"), ("slk", "sk"), ("slo", "sk"), ("slv", "sl"), ("snd", "sd"), ("srp", "sr"),
    ("scc", "sr"), ("tam", "ta"), ("tel", "te"), ("tgk", "tg"), ("tha", "th"), ("tir", "ti"), ("tur", "tr"), ("uig", "ug"),
    ("ukr", "uk"), ("urd", "ur"), ("yid", "yi"), ("pob", "pt"), ("por", "pt"), ("eng", "en"), ("ger", "de"), ("deu", "de"),
    ("fre", "fr"), ("fra", "fr"),
];

/// The language a code names, as what its code page and script are chosen by: its ISO 639-1
/// code, region and script left out (`rus`, `ru-RU` and `ru` are `ru`; `pob` and `pt-BR` are `pt`).
/// A three-letter code with no two-letter one stays as it is (`yue`, `ckb`).
pub fn base(lang: &str) -> String {
    let primary = lang.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    THREE_LETTERS.iter().find(|(three, _)| *three == primary).map_or(primary, |(_, two)| two.to_string())
}

/// Bytes as text: by their BOM (UTF-8, UTF-16 LE or BE); UTF-16 without one when a quarter of
/// their first bytes are zeros, every other one; UTF-8 when they are valid UTF-8; else one older
/// Windows code page by language: 1251 for Cyrillic ones, 1250 for Central European ones (SubDL's
/// Romanian His Girl Friday is one), 1252 for the rest. Read that way, a file of a language with
/// a script of its own that isn't mostly in it is refused rather than served as gibberish.
// `as_chunks` would need Rust 1.88, which the Android build may not have.
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
pub fn decode(bytes: &[u8], lang: Option<&str>) -> Result<String, String> {
    let utf16 = |bytes: &[u8], big: bool| -> String {
        let units = bytes.chunks_exact(2).map(|pair| if big { u16::from_be_bytes([pair[0], pair[1]]) } else { u16::from_le_bytes([pair[0], pair[1]]) });
        char::decode_utf16(units).map(|c| c.unwrap_or('\u{fffd}')).collect()
    };
    if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        return Ok(utf16(rest, false));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        return Ok(utf16(rest, true));
    }
    let head = &bytes[..bytes.len().min(512) & !1];
    let zeros = |odd: usize| head.iter().skip(odd).step_by(2).filter(|b| **b == 0).count();
    if head.len() >= 8 && zeros(1) * 4 > head.len() {
        return Ok(utf16(bytes, false));
    }
    if head.len() >= 8 && zeros(0) * 4 > head.len() {
        return Ok(utf16(bytes, true));
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }
    let (page, table) = match base(lang.unwrap_or_default()).as_str() {
        "ru" | "uk" | "be" | "bg" | "mk" | "kk" | "ky" | "tg" | "mn" => ("windows-1251", &WINDOWS_1251),
        "cs" | "sk" | "pl" | "hu" | "sl" | "hr" | "bs" | "sr" | "ro" | "sq" => ("windows-1250", &WINDOWS_1250),
        "tr" | "az" => ("windows-1254", &WINDOWS_1254),
        _ => ("windows-1252", &WINDOWS_1252),
    };
    let text: String = bytes.iter().map(|&b| if b < 0x80 { b as char } else { char::from_u32(table[b as usize - 0x80] as u32).unwrap() }).collect();
    match lang.and_then(script) {
        Some((name, ranges)) if !mostly_in(&text, ranges) => {
            Err(format!("not UTF-8 or UTF-16, and not {name} text in {page}, the one older encoding rex-addons reads for {}", lang.unwrap_or_default()))
        }
        _ => Ok(text),
    }
}

/// A subtitle's format, from how its text starts: WebVTT's header, an ASS or SSA script's
/// `[Script Info]`, SubRip's `00:00:01,000 --> 00:00:03,000` cue times, MicroDVD's `{1}{25}` frames.
pub fn format_of(text: &str) -> Option<&'static str> {
    let head: String = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']).chars().take(8192).collect();
    let lower = head.to_ascii_lowercase();
    if head.starts_with("WEBVTT") {
        return Some("vtt");
    }
    if lower.contains("[script info]") {
        return Some(if lower.contains("[v4+ styles]") || lower.contains("scripttype: v4.00+") { "ass" } else { "ssa" });
    }
    let lines = head.lines().map(str::trim).filter(|line| !line.is_empty());
    let cue = |line: &str| {
        line.split_once("-->").is_some_and(|(start, _)| {
            let start = start.trim();
            start.contains(':') && start.len() >= 8 && start.bytes().all(|b| b.is_ascii_digit() || matches!(b, b':' | b',' | b'.'))
        })
    };
    if lines.clone().take(20).any(cue) {
        return Some("srt");
    }
    let frames = |line: &str| line.strip_prefix('{').and_then(|rest| rest.split_once('}')).is_some_and(|(n, rest)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && rest.starts_with('{'));
    lines.take(5).any(frames).then_some("sub")
}

/// A script's name and its Unicode blocks.
pub type Script = (&'static str, &'static [RangeInclusive<char>]);

const CYRILLIC: Script = ("Cyrillic", &['\u{0400}'..='\u{052F}', '\u{1C80}'..='\u{1C8F}', '\u{2DE0}'..='\u{2DFF}', '\u{A640}'..='\u{A69F}']);
const ARABIC: Script = ("Arabic", &['\u{0600}'..='\u{06FF}', '\u{0750}'..='\u{077F}', '\u{08A0}'..='\u{08FF}', '\u{FB50}'..='\u{FDFF}', '\u{FE70}'..='\u{FEFF}']);
const HAN: Script = ("Han", &['\u{3400}'..='\u{4DBF}', '\u{4E00}'..='\u{9FFF}', '\u{F900}'..='\u{FAFF}', '\u{20000}'..='\u{2FA1F}']);

/// The script a language is written in, when it has one of its own: none for Latin ones, nor for
/// one written in several (Serbian) unless its code names one (`sr-Cyrl`).
#[allow(clippy::single_range_in_vec_init)]
pub fn script(lang: &str) -> Option<Script> {
    let mut parts = lang.split('-');
    parts.next()?;
    let primary = base(lang);
    match parts.find(|part| part.len() == 4) {
        Some("Cyrl") => return Some(CYRILLIC),
        Some("Arab") => return Some(ARABIC),
        Some("Hans" | "Hant") => return Some(HAN),
        Some("Latn") => return None,
        _ => {}
    }
    Some(match primary.as_str() {
        "my" => ("Myanmar", &['\u{1000}'..='\u{109F}', '\u{A9E0}'..='\u{A9FF}', '\u{AA60}'..='\u{AA7F}']),
        "th" => ("Thai", &['\u{0E00}'..='\u{0E7F}']),
        "km" => ("Khmer", &['\u{1780}'..='\u{17FF}', '\u{19E0}'..='\u{19FF}']),
        "lo" => ("Lao", &['\u{0E80}'..='\u{0EFF}']),
        "ar" | "fa" | "ur" | "ps" | "ckb" | "sd" | "ug" => ARABIC,
        "he" | "yi" => ("Hebrew", &['\u{0590}'..='\u{05FF}', '\u{FB1D}'..='\u{FB4F}']),
        "ru" | "uk" | "be" | "bg" | "mk" | "kk" | "ky" | "tg" | "mn" => CYRILLIC,
        "el" => ("Greek", &['\u{0370}'..='\u{03FF}', '\u{1F00}'..='\u{1FFF}']),
        "ka" => ("Georgian", &['\u{10A0}'..='\u{10FF}', '\u{1C90}'..='\u{1CBF}', '\u{2D00}'..='\u{2D2F}']),
        "hy" => ("Armenian", &['\u{0530}'..='\u{058F}', '\u{FB13}'..='\u{FB17}']),
        "hi" | "mr" | "ne" | "sa" | "mai" | "bho" => ("Devanagari", &['\u{0900}'..='\u{097F}', '\u{A8E0}'..='\u{A8FF}']),
        "bn" | "as" => ("Bengali", &['\u{0980}'..='\u{09FF}']),
        "ta" => ("Tamil", &['\u{0B80}'..='\u{0BFF}']),
        "te" => ("Telugu", &['\u{0C00}'..='\u{0C7F}']),
        "kn" => ("Kannada", &['\u{0C80}'..='\u{0CFF}']),
        "ml" => ("Malayalam", &['\u{0D00}'..='\u{0D7F}']),
        "gu" => ("Gujarati", &['\u{0A80}'..='\u{0AFF}']),
        "pa" => ("Gurmukhi", &['\u{0A00}'..='\u{0A7F}']),
        "si" => ("Sinhala", &['\u{0D80}'..='\u{0DFF}']),
        "am" | "ti" => ("Ethiopic", &['\u{1200}'..='\u{139F}', '\u{2D80}'..='\u{2DDF}', '\u{AB00}'..='\u{AB2F}']),
        "ko" => ("Hangul", &['\u{1100}'..='\u{11FF}', '\u{3130}'..='\u{318F}', '\u{A960}'..='\u{A97F}', '\u{AC00}'..='\u{D7FF}']),
        // Kana with kanji, which are Han.
        "ja" => ("Japanese", &['\u{3040}'..='\u{30FF}', '\u{31F0}'..='\u{31FF}', '\u{FF66}'..='\u{FF9F}', '\u{3400}'..='\u{4DBF}', '\u{4E00}'..='\u{9FFF}', '\u{F900}'..='\u{FAFF}']),
        "zh" | "yue" => HAN,
        _ => return None,
    })
}

/// Text without its markup: SubRip's and WebVTT's `<i>` and `<font color="#ffff00">`, ASS's
/// `{\an8}`. Its letters are no script's.
pub fn untagged(text: &str) -> String {
    let (mut out, mut closing) = (String::with_capacity(text.len()), None);
    for c in text.chars() {
        match (closing, c) {
            (None, '<') => closing = Some('>'),
            (None, '{') => closing = Some('}'),
            (None, c) => out.push(c),
            (Some(end), c) if c == end => closing = None,
            _ => {}
        }
    }
    out
}

/// Whether at least half of the letters in `text`, its tags left out, are in the script's
/// blocks; text with no letters (numbers, ♪) is.
pub fn mostly_in(text: &str, ranges: &[RangeInclusive<char>]) -> bool {
    let (mut letters, mut inside) = (0, 0);
    for c in untagged(text).chars().filter(|c| c.is_alphabetic()) {
        letters += 1;
        inside += usize::from(ranges.iter().any(|range| range.contains(&c)));
    }
    inside * 2 >= letters
}

/// Each website's turn: the host downloads from one at a time, as subtitle sites answer 429 to
/// requests in parallel.
static TURNS: Mutex<BTreeMap<String, Arc<Mutex<()>>>> = Mutex::new(BTreeMap::new());

/// `url` fetched through the addon's sandboxed client (its hosts, each redirect checked), one
/// request at a time per website, all of it within `time` of its turn. A 429 or 503 is waited out as its
/// `Retry-After` asks, up to MAX_WAIT, TRIES times. Errors name the website only: a link can
/// carry a key.
pub fn download(hosts: &Hosts, url: &str, headers: &[(String, String)], time: Duration) -> Result<Vec<u8>, String> {
    let host = url::Url::parse(url).ok().and_then(|link| link.host_str().map(str::to_ascii_lowercase)).ok_or("subtitle() returned a url that is no link")?;
    let turn = TURNS.lock().unwrap().entry(host.clone()).or_default().clone();
    let _turn = turn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // From its turn on: a download waiting behind another of the same website's would run out of time.
    let deadline = Instant::now() + time;
    let agent = net::sandboxed_agent(hosts);
    let mut tried = 0;
    loop {
        tried += 1;
        let (response, _) = net::request_until(&agent, hosts, "GET", url, headers, None, Some(deadline))?;
        let status = response.status();
        if (200..300).contains(&status) {
            let mut bytes = Vec::new();
            response.into_reader().take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| format!("{host}: {e}"))?;
            return bounded(bytes, &format!("{host}'s file"));
        }
        let wait = response.header("retry-after").and_then(|after| after.trim().parse().ok()).map_or(Duration::from_secs(1), Duration::from_secs);
        if !matches!(status, 429 | 503) || tried == TRIES || wait > MAX_WAIT || Instant::now() + wait >= deadline {
            let asked = if matches!(status, 429 | 503) { format!(", asking to wait {} s", wait.as_secs()) } else { String::new() };
            return Err(format!("{host} answered {status}{asked}"));
        }
        std::thread::sleep(wait);
    }
}

/// A cache entry's name: a digest of what it answers, so nothing a client sends names a path.
pub fn digest(value: &Json) -> String {
    crate::git::hex(ring::digest::digest(&ring::digest::SHA256, value.to_string().as_bytes()).as_ref())[..32].to_string()
}

pub fn cached(folder: &Path, key: &str, ttl: Duration) -> Option<Json> {
    let path = folder.join(format!("{key}.json"));
    let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().unwrap_or_default();
    if age < ttl { read_json(&path) } else { None }
}

/// Keeps `value` as `key`, then trims the folder: what is older than `ttl` goes, then the oldest
/// until it holds `most` bytes at most. Whether it was written.
pub fn keep(folder: &Path, key: &str, value: &Json, ttl: Duration, most: u64) -> bool {
    if std::fs::create_dir_all(folder).is_err() || write_atomic(&folder.join(format!("{key}.json")), value.to_string().as_bytes()).is_err() {
        return false;
    }
    let mut files: Vec<(SystemTime, u64, PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    for (at, size, path) in files {
        if total <= most && at.elapsed().unwrap_or_default() < ttl {
            break;
        }
        let _ = std::fs::remove_file(path);
        total -= size;
    }
    true
}

/// What a call failed with, as answered: 504 when it ran out of time.
fn failure(error: &str) -> u16 {
    if ["took longer than", "no time left", "out of time", "timed out"].iter().any(|said| error.contains(said)) { 504 } else { 502 }
}

/// What a search asked, for its log line: no file name or hash, which say what is on the disk.
fn asked(query: &Json) -> String {
    let mut parts: Vec<String> = ["type", "imdb", "tmdb"].iter().filter_map(|key| query.get(*key)).map(|v| v.as_str().map_or(v.to_string(), str::to_string)).collect();
    if let (Some(season), Some(episode)) = (query["season"].as_u64(), query["episode"].as_u64()) {
        parts.push(format!("S{season:02}E{episode:02}"));
    }
    parts.extend(query["languages"].as_array().filter(|l| !l.is_empty()).map(|l| l.iter().filter_map(Json::as_str).collect::<Vec<_>>().join(",")));
    parts.join(" ")
}

/// An addon's manifest, code, settings, storage, and which answers are current (`Addon::answers`).
type Callable = (Arc<Manifest>, Arc<String>, Map<String, Json>, Arc<Storage>, u64);

impl App {
    /// The addon `id`, for a call of its `group` (`subtitles`, `translate`), when it may be
    /// called now: as `open()`, enabled, signed in, not waiting for an update, its settings set.
    pub(crate) fn callable(&self, id: &str, group: &str) -> Result<Callable, (u16, String)> {
        let addons = self.addons.lock().unwrap();
        let a = addons.get(id).ok_or((404, "no such addon".to_string()))?;
        if !a.provides(group) {
            let what = if group == "translate" { "translates nothing: its main.js exports no translate()" } else { "finds no subtitles: its main.js exports no subtitles() and subtitle()" };
            return Err((404, format!("{} {what}", a.manifest.name)));
        }
        if let Some(update) = a.manifest.retired() {
            return Err((409, update));
        }
        if !a.enabled {
            return Err((409, "the addon is disabled".into()));
        }
        if !a.signed_in() {
            return Err((409, "sign in first".into()));
        }
        if let Some(label) = a.manifest.missing(&a.config).first() {
            return Err((409, format!("set {label} first")));
        }
        Ok((a.manifest.clone(), a.source.clone(), a.config.clone(), a.storage.clone(), a.answers))
    }

    /// An answer kept on the addon's `shelf`, unless the addon was removed, or its settings or
    /// version changed, since the call that found it began (`answers`). Under the addons lock, as
    /// a change empties the cache under it: nothing slips in after it, and no removed addon's
    /// folder comes back.
    pub(crate) fn keep_answer(&self, id: &str, answers: u64, shelf: &Shelf, key: &str, value: &Json) -> Result<(), String> {
        let addons = self.addons.lock().unwrap();
        if !addons.get(id).is_some_and(|a| a.answers == answers) {
            return Err("the addon's settings or version changed, or it was removed, meanwhile".into());
        }
        match keep(&app::dir(self, id).join("cache").join(shelf.kind), key, value, shelf.ttl, shelf.most) {
            true => Ok(()),
            false => Err("rex-addons could not write it to disk".into()),
        }
    }

    /// `GET /subtitles/{id}/search`: the addon's `subtitles(query, ctx)` within SUBTITLES' time,
    /// its candidates checked. The same question within SEARCH_TTL answers from the cache.
    pub fn subtitle_search(self: &Arc<Self>, id: &str, query: &str) -> Result<Json, (u16, String)> {
        let query = search_query(query).map_err(|e| (400, e))?;
        let (manifest, source, config, storage, answers) = self.callable(id, "subtitles")?;
        let (folder, key) = (app::dir(self, id).join("cache").join(SEARCHES.kind), digest(&query));
        if let Some(answer) = cached(&folder, &key, SEARCHES.ttl) {
            return Ok(answer);
        }
        let started = Instant::now();
        let env = self.env(id, &manifest, &config, &source, storage, true);
        let what = asked(&query);
        let returned = sandbox::call(&env, "subtitles", &[query, call_ctx(&manifest, &config)], sandbox::SUBTITLES);
        let (found, left_out, truncated) = returned.and_then(candidates).map_err(|error| {
            let error = masked(id, &error);
            warn(id, &format!("subtitles({what}) failed: {error}"));
            (failure(&error), app::first_line(&error))
        })?;
        for why in left_out.iter().take(5) {
            warn(id, why);
        }
        let more = if left_out.is_empty() { String::new() } else { format!(", {} left out", left_out.len()) };
        log(id, &format!("subtitles({what}): {} found in {:.1} s{more}", found.len(), started.elapsed().as_secs_f64()));
        let answer = json!({"candidates": found, "truncated": truncated, "left_out": left_out.len()});
        let _ = self.keep_answer(id, answers, &SEARCHES, &key, &answer);
        Ok(answer)
    }

    /// `GET /subtitles/{id}/file/{candidate}`: the addon's `subtitle(candidate, ctx)` and, for a
    /// link, the host's own download of it, unpacked and decoded. `query` may give the `season`
    /// and `episode` wanted from a season's archive and the `lang` it's in. The same file within
    /// FILE_TTL answers from the cache.
    pub fn subtitle_file(self: &Arc<Self>, id: &str, candidate: &str, query: &str) -> Result<Subtitle, (u16, String)> {
        let (mut season, mut episode, mut lang) = (None, None, None);
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let number = || value.parse::<u64>().map(Some).map_err(|_| (400, format!("{key} must be a whole number")));
            match key.as_ref() {
                "season" => season = number()?,
                "episode" => episode = number()?,
                "lang" => lang = Some(language(&value).ok_or((400, format!("lang: {value} is no language code, such as en or pt-BR")))?),
                _ => return Err((400, format!("{key} is no field of a file: season, episode or lang"))),
            }
        }
        let wanted = match (season, episode) {
            (Some(season), Some(episode)) => Some((season, episode)),
            (None, None) => None,
            _ => return Err((400, "give season and episode together".into())),
        };
        if candidate.is_empty() || candidate.len() > 1024 {
            return Err((400, "give the id of a candidate its search found".into()));
        }
        let (manifest, source, config, storage, answers) = self.callable(id, "subtitles")?;
        let (folder, key) = (app::dir(self, id).join("cache").join(FILES.kind), digest(&json!([candidate, season, episode, lang])));
        let hit = cached(&folder, &key, FILES.ttl).and_then(|v| Some(Subtitle { format: FORMATS.into_iter().find(|f| v["format"] == *f)?, text: v["text"].as_str()?.to_string() }));
        if let Some(subtitle) = hit {
            return Ok(subtitle);
        }
        let started = Instant::now();
        // The client's, decoded: quoted, so a line break in it can't start a line of the log.
        let shown: String = candidate.chars().take(80).collect();
        let failed = |error: String| {
            let error = masked(id, &error);
            warn(id, &format!("subtitle({shown:?}) failed: {error}"));
            (failure(&error), app::first_line(&error))
        };
        let env = self.env(id, &manifest, &config, &source, storage, true);
        let answer = sandbox::call(&env, "subtitle", &[json!(candidate), call_ctx(&manifest, &config)], sandbox::OPEN).map_err(failed)?;
        let hint = |key: &str| answer.get(key).and_then(Json::as_u64);
        let wanted = wanted.or(hint("season").zip(hint("episode")));
        let bytes = match (&answer["text"], &answer["url"]) {
            (Json::String(text), _) => text.clone().into_bytes(),
            (_, Json::String(url)) => {
                let headers: Vec<(String, String)> = answer["headers"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
                download(&env.hosts, url, &headers, DOWNLOAD_TIME).map_err(failed)?
            }
            _ => return Err(failed("subtitle() returned neither text nor url".into())),
        };
        let subtitle = subtitle_of(&bytes, answer["name"].as_str(), wanted, lang.as_deref()).map_err(failed)?;
        log(id, &format!("subtitle({shown:?}): {}, {} KB in {:.1} s", subtitle.format, subtitle.text.len().div_ceil(1024), started.elapsed().as_secs_f64()));
        let _ = self.keep_answer(id, answers, &FILES, &key, &json!({"format": subtitle.format, "text": subtitle.text}));
        Ok(subtitle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real files from SubDL (dl.subdl.com, 2026-11-16) for His Girl Friday (1940, public domain),
    /// and archives written by Python's zipfile, Info-ZIP and gzip where no real one shows the shape.
    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/subtitles").join(name)).unwrap()
    }

    #[test]
    fn language_codes_have_their_case_fixed_and_nothing_else_is_one() {
        for (code, fixed) in [("en", "en"), ("EN", "en"), ("pt-br", "pt-BR"), ("pt_BR", "pt-BR"), ("zh-hant", "zh-Hant"), ("es-419", "es-419"), ("por", "por"), ("sr-cyrl-RS", "sr-Cyrl-RS")] {
            assert_eq!(language(code).as_deref(), Some(fixed), "{code}");
        }
        for code in ["English", "e", "", "en-", "en-x", "12", "pt-BRAZIL!"] {
            assert_eq!(language(code), None, "{code}");
        }
    }

    #[test]
    fn a_search_is_checked_and_asked_in_one_shape() {
        let a = search_query("lang=EN,pt-br&imdb=tt0032599&type=movie").unwrap();
        let b = search_query("type=movie&imdb=tt0032599&lang=en&lang=pt-BR&lang=en").unwrap();
        assert_eq!(a.to_string(), b.to_string());
        assert_eq!(a, json!({"type": "movie", "imdb": "tt0032599", "languages": ["en", "pt-BR"]}));
        let episode = search_query("type=episode&tmdb=1396&season=1&episode=2&hash=8E245D9679D31E12&size=734003200&filename=Show.S01E02.mkv").unwrap();
        assert_eq!(episode.to_string(), r#"{"type":"episode","tmdb":1396,"season":1,"episode":2,"hash":"8e245d9679d31e12","size":734003200,"filename":"Show.S01E02.mkv","languages":[]}"#);
        for (query, error) in [
            ("imdb=tt0032599", "give type: movie or episode"),
            ("type=film&imdb=tt0032599", "type must be movie or episode"),
            ("type=episode&imdb=tt0903747&season=1", "an episode needs season and episode"),
            ("type=movie&imdb=tt0032599&season=1", "a movie has no season or episode"),
            ("type=movie&lang=en", "give imdb, tmdb, hash or filename"),
            ("type=movie&imdb=0032599", "imdb must be an IMDb id, such as tt0032599"),
            ("type=movie&tmdb=abc", "tmdb must be a whole number"),
            ("type=movie&imdb=tt0032599&lang=English", "lang: English is no language code, such as en or pt-BR"),
            ("type=movie&imdb=tt0032599&year=1940", "year is no search field: type, imdb, tmdb, season, episode, lang, hash, size or filename"),
        ] {
            assert_eq!(search_query(query).unwrap_err(), error, "{query}");
        }
    }

    #[test]
    fn candidates_keep_what_rex_can_use_and_say_why_the_rest_was_left_out() {
        let returned = json!([
            {"id": "1", "lang": "EN", "release": "His.Girl.Friday.1940.1080p", "provider": "SubDL", "format": "SRT", "hi": true, "fps": 23.976, "downloads": 1200, "extra": "dropped"},
            {"lang": "en"},
            {"id": "2", "lang": "English"},
            {"id": "3", "lang": "pt-br", "format": "sup"},
            {"id": "1", "lang": "fr"},
            "4",
            {"id": "5", "lang": "my", "release": "x".repeat(600), "fps": "25", "forced": "yes", "downloads": -1},
        ]);
        let (kept, left_out, truncated) = candidates(returned).unwrap();
        assert_eq!(kept, [
            json!({"id": "1", "lang": "en", "release": "His.Girl.Friday.1940.1080p", "provider": "SubDL", "format": "srt", "hi": true, "forced": false, "fps": 23.976, "downloads": 1200}),
            json!({"id": "5", "lang": "my", "release": "x".repeat(500), "provider": null, "format": null, "hi": false, "forced": false, "fps": null, "downloads": null}),
        ]);
        assert_eq!(left_out, [
            "subtitles(): an entry has no id; left out",
            "subtitles(): 2 has the lang \"English\", which is no language code such as en or pt-BR; left out",
            "subtitles(): 3 has the format \"sup\", which rex-addons doesn't serve (srt, ass, ssa, vtt, sub); left out",
            "subtitles(): 1 repeats an id; left out",
            "subtitles(): an entry is not an object; left out",
        ]);
        assert!(!truncated);
        let many: Vec<Json> = (0..MAX_CANDIDATES + 5).map(|n| json!({"id": n.to_string(), "lang": "en"})).collect();
        let (kept, _, truncated) = candidates(Json::Array(many)).unwrap();
        assert_eq!((kept.len(), truncated), (MAX_CANDIDATES, true));
        assert_eq!(candidates(json!({"candidates": []})).unwrap_err(), "subtitles() must return a list");
    }

    /// SubDL's ZIP as served: one SubRip file in UTF-8 with a BOM, the archive's comment after it.
    #[test]
    fn a_real_zip_gives_its_subtitle() {
        let zip = fixture("subdl-578492-1497278.zip");
        let subtitle = subtitle_of(&zip, None, None, Some("en")).unwrap();
        assert_eq!(subtitle.format, "srt");
        assert!(subtitle.text.starts_with("1\r\n00:01:12,990 --> 00:01:16,200\r\n(chattering)"), "{:?}", &subtitle.text[..60]);
        assert_eq!(subtitle.text.matches("-->").count(), 1878);
        // Named, by its file name or its whole path; a name it doesn't hold is no other file.
        let named = "His.Girl.Friday.1940.1080p-720p.BluRay.x264-SiNNERS.srt";
        assert_eq!(subtitle_of(&zip, Some(named), None, None).unwrap(), subtitle);
        assert_eq!(subtitle_of(&zip, Some("Other.srt"), None, None).unwrap_err(), "the ZIP holds no Other.srt");
        // An episode asked of a film's archive: its one subtitle, named for no episode.
        assert_eq!(subtitle_of(&zip, None, Some((1, 2)), None).unwrap(), subtitle);
        // Cut short, it has no central directory; a byte changed in its data fails the checksum.
        assert_eq!(subtitle_of(&zip[..zip.len() / 2], None, None, None).unwrap_err(), "a damaged ZIP: no central directory");
        let mut changed = zip.clone();
        changed[200] ^= 0xff;
        let error = subtitle_of(&changed, None, None, None).unwrap_err();
        assert_eq!(error, format!("{named} in the ZIP is damaged"));
    }

    /// A season's archive written as a stream (sizes after each file's data, as zipfile does
    /// without seeking): the episode asked for, the largest of its files, never another episode's.
    #[test]
    fn a_season_pack_gives_the_episode_asked_for() {
        let zip = fixture("season-pack.FABRICATED.zip");
        assert_eq!(&zip[6..8], &[0x08, 0x00], "written as a stream: flag 3, sizes after the data");
        let text = |episode, name| subtitle_of(&zip, name, episode, None).map(|s| s.text.lines().nth(2).unwrap().to_string());
        assert_eq!(text(Some((1, 1)), None).unwrap(), "episode one");
        assert_eq!(text(Some((1, 2)), None).unwrap(), "episode two");
        assert_eq!(text(Some((1, 3)), None).unwrap(), "episode three");
        assert_eq!(text(Some((1, 5)), None).unwrap_err(), "the ZIP holds no subtitle for S01E05");
        assert_eq!(text(Some((2, 1)), None).unwrap_err(), "the ZIP holds no subtitle for S02E01");
        assert_eq!(text(None, Some("Show.S01E02.720p.WEB.forced.srt")).unwrap(), "episode two, forced");
        assert_eq!(text(None, None).unwrap(), "episode three");
    }

    #[test]
    fn archives_that_are_no_subtitle_are_refused_saying_why() {
        for (file, error) in [
            ("traversal.FABRICATED.zip", "the ZIP has a file whose path leaves it: ../../evil.srt"),
            ("encrypted.FABRICATED.zip", "locked.srt in the ZIP is encrypted"),
            ("bomb.FABRICATED.zip", "big.srt, unpacked, is over 16 MB"),
            ("nested.FABRICATED.zip", "the ZIP holds no subtitle, only another archive, which rex-addons doesn't open"),
        ] {
            assert_eq!(subtitle_of(&fixture(file), None, None, None).unwrap_err(), error, "{file}");
        }
        assert_eq!(subtitle_of(b"Rar!\x1a\x07\x01\x00rest", None, None, None).unwrap_err(), "a RAR archive, which rex-addons doesn't open: only ZIP and gzip");
        assert_eq!(subtitle_of(b"7z\xbc\xaf\x27\x1c\x00\x04", None, None, None).unwrap_err(), "a 7z archive, which rex-addons doesn't open: only ZIP and gzip");
        for short in [&b"PK\x05\x06"[..], b"PK\x03\x04\x14\x00", b"PK\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00\x00\x00"] {
            assert_eq!(subtitle_of(short, None, None, None).unwrap_err(), "a damaged ZIP: no central directory");
        }
        assert_eq!(subtitle_of(b"<html>Too many requests</html>", None, None, None).unwrap_err(), "not a subtitle rex-addons serves: no SubRip, WebVTT, ASS, SSA or MicroDVD in it");
    }

    #[test]
    fn gzip_utf16_and_older_encodings_are_read_as_utf8() {
        let gz = subtitle_of(&fixture("his-girl-friday.excerpt.FABRICATED.srt.gz"), None, None, None).unwrap();
        assert_eq!((gz.format, gz.text.lines().nth(2)), ("srt", Some("(chattering)")));
        let srt = "1\r\n00:00:01,000 --> 00:00:02,000\r\nCafé ♪\r\n";
        let utf16 = |big: bool| srt.encode_utf16().flat_map(move |u| if big { u.to_be_bytes() } else { u.to_le_bytes() }).collect::<Vec<u8>>();
        for (bytes, how) in [
            ([&b"\xff\xfe"[..], &utf16(false)].concat(), "UTF-16 LE with its BOM"),
            ([&b"\xfe\xff"[..], &utf16(true)].concat(), "UTF-16 BE with its BOM"),
            (utf16(false), "UTF-16 LE without one"),
            (utf16(true), "UTF-16 BE without one"),
            ([&b"\xef\xbb\xbf"[..], srt.as_bytes()].concat(), "UTF-8 with its BOM"),
        ] {
            assert_eq!(decode(&bytes, None).unwrap(), srt, "{how}");
        }
        assert_eq!(decode(b"Caf\xe9 \x93quoted\x94", None).unwrap(), "Café \u{201c}quoted\u{201d}");
        // SubDL's Romanian file is windows-1250: read as 1252, its ă ş ţ would be ã º þ.
        let romanian = fixture("subdl-578514-530576.excerpt.srt");
        let read = decode(&romanian, Some("ro")).unwrap();
        assert!(read.contains("- Grăbeşte-te.") && read.contains("Ţine liftul !"), "{read}");
        assert!(decode(&romanian, None).unwrap().contains("Grãbeºte-te"));
        assert_eq!(format_of(&read), Some("srt"));
        assert_eq!(decode(b"\xcf\xf0\xe8\xe2\xe5\xf2", Some("ru")).unwrap(), "Привет");
        // Greek in windows-1253, which it doesn't read, is refused rather than served as Latin.
        assert_eq!(decode(b"\xc3\xe5\xe9\xdc", Some("el")).unwrap_err(), "not UTF-8 or UTF-16, and not Greek text in windows-1252, the one older encoding rex-addons reads for el");
    }

    #[test]
    fn formats_are_found_from_the_text() {
        for (text, format) in [
            ("WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n", Some("vtt")),
            ("[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n", Some("ass")),
            ("[Script Info]\nScriptType: v4.00\n\n[V4 Styles]\n", Some("ssa")),
            ("\u{feff}\r\n1\r\n00:00:01,000 --> 00:00:02,000\r\nHi\r\n", Some("srt")),
            ("{1}{25}Hi|there\n{26}{50}Bye\n", Some("sub")),
            ("Downloaded from somewhere\nhttps://example.com\n", None),
        ] {
            assert_eq!(format_of(text), format, "{text:?}");
        }
    }

    #[test]
    fn episodes_are_read_from_file_names() {
        for (name, episode) in [
            ("Show.S01E02.720p.srt", Some((1, 2))),
            ("show s1.e2.srt", Some((1, 2))),
            ("Show S01 E02.srt", Some((1, 2))),
            ("Show.1x02.srt", Some((1, 2))),
            ("Show.S01E02E03.srt", Some((1, 2))),
            ("Show.S10E102.srt", Some((10, 102))),
            ("Show.S01.1080p.x264.srt", None),
            ("Film.2019.1080x720.srt", None),
            ("Seasons.srt", None),
        ] {
            assert_eq!(episode_of(name), episode, "{name}");
        }
    }

    #[test]
    fn scripts_of_languages_written_in_their_own() {
        let in_script = |lang: &str, text: &str| script(lang).map(|(_, ranges)| mostly_in(text, ranges));
        assert_eq!(script("my").unwrap().0, "Myanmar");
        assert_eq!(in_script("my", "မင်္ဂလာပါ"), Some(true));
        // Extended-A, as Shan and Karen letters are.
        assert_eq!(in_script("my", "\u{aa60}\u{aa61}"), Some(true));
        assert_eq!(in_script("my", "Hello there"), Some(false));
        assert_eq!(in_script("my", "မင်္ဂလာပါ Bob"), Some(true));
        assert_eq!(in_script("my", "♪ 1, 2, 3 ♪"), Some(true));
        assert_eq!(in_script("ja", "こんにちは、世界"), Some(true));
        assert_eq!(in_script("zh-Hant", "你好"), Some(true));
        assert_eq!(in_script("sr-Cyrl", "Здраво"), Some(true));
        assert_eq!(in_script("ar", "مرحبا"), Some(true));
        for latin in ["en", "pt-BR", "sr", "sr-Latn", "tr"] {
            assert!(script(latin).is_none(), "{latin}");
        }
    }

    #[test]
    fn a_cache_keeps_what_is_fresh_within_its_size() {
        let folder = std::env::temp_dir().join(format!("rex-addons-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let day = Duration::from_secs(86400);
        let age = |key: &str, ago: Duration| std::fs::File::options().write(true).open(folder.join(format!("{key}.json"))).unwrap().set_modified(SystemTime::now() - ago).unwrap();
        keep(&folder, "stale", &json!("x".repeat(100)), day, 1 << 20);
        age("stale", 2 * day);
        assert_eq!(cached(&folder, "stale", day), None);
        // The next entry kept takes the stale one away; one past the room takes the oldest.
        keep(&folder, "old", &json!("x".repeat(100)), day, 1 << 20);
        age("old", Duration::from_secs(60));
        assert!(!folder.join("stale.json").exists() && folder.join("old.json").exists());
        keep(&folder, "new", &json!("x".repeat(100)), day, 150);
        assert!(!folder.join("old.json").exists());
        assert_eq!(cached(&folder, "new", day), Some(json!("x".repeat(100))));
        assert_eq!(digest(&json!(["a"])).len(), 32);
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A language named by its three letters (ISO 639-2, as OpenSubtitles and others answer) or
    /// with a region is the same language: its code page and its script are found all the same.
    /// Turkish has a code page of its own.
    #[test]
    fn three_letter_and_regional_codes_pick_the_same_code_page_and_script() {
        for lang in ["ru", "rus", "ru-RU"] {
            assert_eq!(decode(b"\xcf\xf0\xe8\xe2\xe5\xf2", Some(lang)).unwrap(), "Привет", "{lang}");
        }
        assert_eq!(decode(b"Gr\xe3be\xbate", Some("rum")).unwrap(), "Grăbeşte");
        for (lang, name) in [("mya", "Myanmar"), ("bur", "Myanmar"), ("tha", "Thai"), ("gre", "Greek"), ("zho", "Han"), ("jpn", "Japanese")] {
            assert_eq!(script(lang).map(|(name, _)| name), Some(name), "{lang}");
        }
        for lang in ["pob", "pt-BR", "eng"] {
            assert!(script(lang).is_none(), "{lang}");
        }
        assert_eq!(decode(b"\xdei\xfeli \xddstanbul \xf0", Some("tr")).unwrap(), "Şişli İstanbul ğ");
        assert_eq!(decode(b"\xdei\xfeli", Some("tur")).unwrap(), "Şişli");
        assert_eq!((base("pob"), base("pt-BR"), base("yue")), ("pt".to_string(), "pt".to_string(), "yue".to_string()));
    }

    /// Downloads from one website wait their turn, one at a time; each then has its whole time,
    /// so episodes fetched side by side don't run out of it waiting. Out of time is a 504.
    #[test]
    fn a_download_waiting_its_turn_gets_its_own_time() {
        assert_eq!(failure("dl.subs.example: out of time"), 504);
        let lan = std::net::UdpSocket::bind("0.0.0.0:0").ok().and_then(|s| s.connect("192.0.2.1:9").ok().and(s.local_addr().ok())).map(|a| a.ip()).filter(|ip| !ip.is_loopback());
        let Some(lan) = lan else {
            eprintln!("SKIP: no network address for the website (loopback is never fetched)");
            return;
        };
        let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
        let url = format!("http://{lan}:{}/a.srt", server.server_addr().to_ip().unwrap().port());
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(700));
                    let _ = request.respond(tiny_http::Response::from_string("1\n00:00:01,000 --> 00:00:02,000\nHi\n"));
                });
            }
        });
        let hosts = Hosts::from(vec![lan.to_string()]);
        let fetched: Vec<Result<Vec<u8>, String>> = std::thread::scope(|scope| {
            let both: Vec<_> = (0..2).map(|_| scope.spawn(|| download(&hosts, &url, &[], Duration::from_millis(1500)))).collect();
            both.into_iter().map(|one| one.join().unwrap()).collect()
        });
        for one in fetched {
            assert_eq!(one.unwrap(), b"1\n00:00:01,000 --> 00:00:02,000\nHi\n");
        }
    }

    /// An archive packed on a Mac (macOS's own `ditto -c -k --sequesterRsrc`, a file with an
    /// extended attribute) holds an AppleDouble `__MACOSX/._<name>` beside each file: no subtitle,
    /// so a film's archive with an episode asked is still its one subtitle.
    #[test]
    fn a_macs_appledouble_files_are_no_subtitles() {
        let zip = fixture("macos-ditto.FABRICATED.zip");
        for episode in [None, Some((1, 2))] {
            let subtitle = subtitle_of(&zip, None, episode, None).unwrap();
            assert_eq!((subtitle.format, subtitle.text.lines().nth(2)), ("srt", Some("(chattering)")), "{episode:?}");
        }
    }
}
