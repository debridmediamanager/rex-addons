//! A shallow fetch of one commit over git's smart HTTP protocol, written out as plain files.
//! Public repositories over HTTPS only. Protocol v2 where the server speaks it (GitHub, GitLab,
//! Codeberg and every git since 2.18), else v0. The pack is read here: zlib from flate2 and SHA-1
//! from ring, both already in the binary for HTTPS, so this adds no dependency.
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use flate2::{Decompress, FlushDecompress, Status};

/// The pack a fetch may download.
pub const MAX_PACK: u64 = 32 << 20;
/// What a checkout may write: files, and bytes in all.
pub const MAX_FILES: usize = 2000;
pub const MAX_BYTES: u64 = 64 << 20;
/// What the pack's objects may inflate to in all.
const MAX_INFLATED: usize = 256 << 20;
/// Some servers serve the smart protocol only to a client that names itself git.
const AGENT: &str = concat!("git/2.0 (rex-addons/", env!("CARGO_PKG_VERSION"), ")");

pub type Oid = [u8; 20];

pub fn hex(oid: &Oid) -> String {
    oid.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Oid> {
    let bytes = text.as_bytes();
    if bytes.len() != 40 {
        return None;
    }
    let mut oid = [0u8; 20];
    for (i, pair) in bytes.chunks(2).enumerate() {
        oid[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(oid)
}

/// The repository's address and the branch its `#fragment` names, if any.
pub fn split(url: &str) -> Result<(url::Url, Option<String>), String> {
    let mut parsed = url::Url::parse(url.trim()).map_err(|e| format!("not a link: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("give the repository's https:// address".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("only public repositories: no login in the address".into());
    }
    let branch = parsed.fragment().filter(|b| !b.is_empty()).map(str::to_string);
    parsed.set_fragment(None);
    parsed.set_query(None);
    let path = parsed.path().trim_end_matches('/').to_string();
    if path.is_empty() {
        return Err("the address names no repository".into());
    }
    parsed.set_path(&path);
    Ok((parsed, branch))
}

enum Pkt<'a> {
    Flush,
    Delim,
    Data(&'a [u8]),
}

fn pkts(mut bytes: &[u8]) -> Result<Vec<Pkt<'_>>, String> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let len = std::str::from_utf8(bytes.get(..4).ok_or("a cut-off answer")?)
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or("not git's protocol")?;
        match len {
            0 => out.push(Pkt::Flush),
            1 => out.push(Pkt::Delim),
            2 => out.push(Pkt::Flush), // response-end, v2
            3 => return Err("not git's protocol".into()),
            _ => out.push(Pkt::Data(bytes.get(4..len).ok_or("a cut-off answer")?)),
        }
        bytes = &bytes[len.max(4)..];
    }
    Ok(out)
}

fn pkt(out: &mut Vec<u8>, line: &str) {
    out.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
}

fn read_capped(response: ureq::Response, cap: u64) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    response.into_reader().take(cap + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
    if body.len() as u64 > cap {
        return Err(format!("the repository is over {} MB", cap >> 20));
    }
    Ok(body)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(5)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(AGENT)
        .build()
}

/// The service's base URL (after redirects, `.git` added where the server wants it) and its
/// advertisement.
fn discover(agent: &ureq::Agent, repo: &url::Url, v2: bool) -> Result<(String, Vec<u8>), String> {
    let mut candidates = vec![repo.to_string()];
    if !repo.path().ends_with(".git") {
        candidates.push(format!("{repo}.git"));
    }
    let mut refused = String::from("no git repository there");
    for base in candidates {
        let mut request = agent.get(&format!("{base}/info/refs?service=git-upload-pack"));
        if v2 {
            request = request.set("Git-Protocol", "version=2");
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(401 | 403, _)) => {
                refused = "no public git repository there (it asks for a login)".into();
                continue;
            }
            Err(ureq::Error::Status(status, _)) => {
                refused = format!("no git repository there (the server answered {status})");
                continue;
            }
            Err(ureq::Error::Transport(error)) => return Err(format!("{}: {}", repo.host_str().unwrap_or(""), error.kind())),
        };
        if response.content_type() != "application/x-git-upload-pack-advertisement" {
            refused = "not a git repository (the server answered a web page)".into();
            continue;
        }
        let final_url = response.get_url().to_string();
        let base = final_url.split("/info/refs").next().unwrap_or(&base).to_string();
        return Ok((base, read_capped(response, 16 << 20)?));
    }
    Err(refused)
}

fn post(agent: &ureq::Agent, base: &str, body: &[u8], v2: bool) -> Result<Vec<u8>, String> {
    let mut request = agent
        .post(&format!("{base}/git-upload-pack"))
        .set("Content-Type", "application/x-git-upload-pack-request")
        .set("Accept", "application/x-git-upload-pack-result");
    if v2 {
        request = request.set("Git-Protocol", "version=2");
    }
    match request.send_bytes(body) {
        Ok(response) => read_capped(response, MAX_PACK + (1 << 20)),
        Err(ureq::Error::Status(status, _)) => Err(format!("the server refused the fetch ({status})")),
        Err(ureq::Error::Transport(error)) => Err(format!("fetching: {}", error.kind())),
    }
}

/// The pack inside an upload-pack answer: side band 1 is the pack, 2 progress, 3 an error.
fn demux(answer: &[u8]) -> Result<Vec<u8>, String> {
    let mut pack = Vec::new();
    let mut in_pack = false;
    for pkt in pkts(answer)? {
        let Pkt::Data(data) = pkt else { continue };
        if !in_pack {
            // v2's "packfile" section header, or v0's first side-band packet after its NAK.
            if data == b"packfile\n" {
                in_pack = true;
                continue;
            }
            if !matches!(data.first(), Some(1..=3)) {
                if let Some(error) = data.strip_prefix(b"ERR ") {
                    return Err(String::from_utf8_lossy(error).trim().to_string());
                }
                continue;
            }
            in_pack = true;
        }
        match data.first() {
            Some(1) => pack.extend_from_slice(&data[1..]),
            Some(3) => return Err(format!("the server: {}", String::from_utf8_lossy(&data[1..]).trim())),
            _ => {}
        }
    }
    if pack.is_empty() {
        return Err("the server sent no files".into());
    }
    Ok(pack)
}

/// The commit to fetch, and the fetch's answer.
fn negotiate(agent: &ureq::Agent, base: &str, advert: &[u8], branch: Option<&str>, v2: bool) -> Result<(Oid, Vec<u8>), String> {
    let lines: Vec<&[u8]> = pkts(advert)?
        .into_iter()
        .filter_map(|p| if let Pkt::Data(d) = p { Some(d) } else { None })
        .filter(|d| !d.starts_with(b"# service="))
        .collect();
    let wanted_ref = branch.map(|b| format!("refs/heads/{b}"));
    let name = || branch.map_or("its default branch".to_string(), |b| format!("branch {b}"));
    if lines.first() == Some(&&b"version 2\n"[..]) {
        if !v2 {
            return Err("the server speaks only git protocol v2".into());
        }
        let mut body = Vec::new();
        pkt(&mut body, "command=ls-refs\n");
        body.extend_from_slice(b"0001");
        pkt(&mut body, "symrefs\n");
        pkt(&mut body, &format!("ref-prefix {}\n", wanted_ref.as_deref().unwrap_or("HEAD")));
        body.extend_from_slice(b"0000");
        let refs = post(agent, base, &body, true)?;
        let want = pkts(&refs)?
            .into_iter()
            .filter_map(|p| if let Pkt::Data(d) = p { Some(String::from_utf8_lossy(d).into_owned()) } else { None })
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let (oid, refname) = (parts.next()?, parts.next()?);
                (refname == wanted_ref.as_deref().unwrap_or("HEAD")).then(|| unhex(oid)).flatten()
            })
            .ok_or_else(|| format!("the repository has no {}", name()))?;
        let mut body = Vec::new();
        pkt(&mut body, "command=fetch\n");
        body.extend_from_slice(b"0001");
        for line in ["no-progress\n", "ofs-delta\n", "deepen 1\n", &format!("want {}\n", hex(&want)), "done\n"] {
            pkt(&mut body, line);
        }
        body.extend_from_slice(b"0000");
        return Ok((want, post(agent, base, &body, true)?));
    }
    // v0: the refs, the first carrying the server's capabilities after a NUL.
    let mut caps = String::new();
    let mut want = None;
    for line in &lines {
        let (refline, cap) = match line.iter().position(|b| *b == 0) {
            Some(at) => (&line[..at], Some(&line[at + 1..])),
            None => (&line[..], None),
        };
        if let Some(cap) = cap {
            caps = String::from_utf8_lossy(cap).into_owned();
        }
        let refline = String::from_utf8_lossy(refline);
        let mut parts = refline.split_whitespace();
        if let (Some(oid), Some(refname)) = (parts.next(), parts.next()) {
            if refname == wanted_ref.as_deref().unwrap_or("HEAD") {
                want = unhex(oid);
            }
        }
    }
    let want = want.ok_or_else(|| format!("the repository has no {}", name()))?;
    let caps: Vec<&str> = caps.split_whitespace().collect();
    let band = ["side-band-64k", "side-band"].into_iter().find(|c| caps.contains(c)).ok_or("the server sends no side band")?;
    if !caps.contains(&"shallow") {
        return Err("the server cannot send one commit alone (no shallow)".into());
    }
    let extra: String = ["ofs-delta", "no-progress"].iter().filter(|c| caps.contains(c)).map(|c| format!(" {c}")).collect();
    let mut body = Vec::new();
    pkt(&mut body, &format!("want {} {band} shallow{extra} agent={AGENT}\n", hex(&want)));
    pkt(&mut body, "deepen 1\n");
    body.extend_from_slice(b"0000");
    pkt(&mut body, "done\n");
    Ok((want, post(agent, base, &body, false)?))
}

pub struct Object {
    pub kind: u8,
    pub data: Vec<u8>,
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "commit",
        2 => "tree",
        3 => "blob",
        _ => "tag",
    }
}

pub fn oid_of(kind: u8, data: &[u8]) -> Oid {
    let mut context = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    context.update(format!("{} {}\0", kind_name(kind), data.len()).as_bytes());
    context.update(data);
    context.finish().as_ref().try_into().unwrap()
}

fn inflate(input: &[u8], size: usize) -> Result<(Vec<u8>, usize), String> {
    let mut out = Vec::with_capacity(size);
    let mut stream = Decompress::new(true);
    match stream.decompress_vec(input, &mut out, FlushDecompress::Finish) {
        Ok(Status::StreamEnd) if out.len() == size => Ok((out, stream.total_in() as usize)),
        _ => Err("a damaged pack".into()),
    }
}

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "a damaged delta".to_string();
    let mut at = 0;
    let varint = |at: &mut usize| -> Result<usize, String> {
        let (mut value, mut shift) = (0usize, 0);
        loop {
            let byte = *delta.get(*at).ok_or_else(bad)?;
            *at += 1;
            value |= ((byte & 0x7f) as usize) << shift;
            shift += 7;
            if byte & 0x80 == 0 || shift > 56 {
                return Ok(value);
            }
        }
    };
    let (base_size, size) = (varint(&mut at)?, varint(&mut at)?);
    if base_size != base.len() || size > MAX_INFLATED {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(size);
    while at < delta.len() {
        let op = delta[at];
        at += 1;
        if op & 0x80 != 0 {
            let (mut offset, mut length) = (0usize, 0usize);
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    offset |= (*delta.get(at).ok_or_else(bad)? as usize) << (8 * bit);
                    at += 1;
                }
            }
            for bit in 0..3 {
                if op & (0x10 << bit) != 0 {
                    length |= (*delta.get(at).ok_or_else(bad)? as usize) << (8 * bit);
                    at += 1;
                }
            }
            let length = if length == 0 { 0x10000 } else { length };
            out.extend_from_slice(base.get(offset..offset + length).ok_or_else(bad)?);
        } else if op != 0 {
            out.extend_from_slice(delta.get(at..at + op as usize).ok_or_else(bad)?);
            at += op as usize;
        } else {
            return Err(bad());
        }
    }
    if out.len() != size {
        return Err(bad());
    }
    Ok(out)
}

/// Every object in a pack, by its id, deltas applied.
pub fn unpack(pack: &[u8]) -> Result<HashMap<Oid, Object>, String> {
    let damaged = || "a damaged pack".to_string();
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err(damaged());
    }
    let (body, trailer) = pack.split_at(pack.len() - 20);
    if ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, body).as_ref() != trailer {
        return Err("the pack's checksum does not match".into());
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    enum Base {
        Offset(usize),
        Id(Oid),
    }
    // Each entry: (offset, kind or delta base, inflated data).
    let mut entries: Vec<(usize, Option<Base>, u8, Vec<u8>)> = Vec::with_capacity(count.min(100_000));
    let (mut at, mut inflated) = (12usize, 0usize);
    for _ in 0..count {
        let start = at;
        let mut byte = *body.get(at).ok_or_else(damaged)?;
        at += 1;
        let kind = (byte >> 4) & 7;
        let (mut size, mut shift) = ((byte & 15) as usize, 4);
        while byte & 0x80 != 0 {
            byte = *body.get(at).ok_or_else(damaged)?;
            at += 1;
            size |= ((byte & 0x7f) as usize) << shift;
            shift += 7;
            if shift > 56 {
                return Err(damaged());
            }
        }
        let base = match kind {
            6 => {
                let mut byte = *body.get(at).ok_or_else(damaged)?;
                at += 1;
                let mut back = (byte & 0x7f) as usize;
                while byte & 0x80 != 0 {
                    byte = *body.get(at).ok_or_else(damaged)?;
                    at += 1;
                    back = ((back + 1) << 7) | (byte & 0x7f) as usize;
                }
                Some(Base::Offset(start.checked_sub(back).ok_or_else(damaged)?))
            }
            7 => {
                let oid: Oid = body.get(at..at + 20).ok_or_else(damaged)?.try_into().unwrap();
                at += 20;
                Some(Base::Id(oid))
            }
            1..=4 => None,
            _ => return Err(damaged()),
        };
        inflated += size;
        if inflated > MAX_INFLATED {
            return Err(format!("the repository is over {} MB unpacked", MAX_INFLATED >> 20));
        }
        let (data, used) = inflate(&body[at..], size)?;
        at += used;
        entries.push((start, base, kind, data));
    }
    // Deltas apply to objects earlier in the pack, or (by id) anywhere in it: resolve until done.
    let index: HashMap<usize, usize> = entries.iter().enumerate().map(|(i, e)| (e.0, i)).collect();
    let mut resolved: Vec<Option<(u8, Vec<u8>, Oid)>> = vec![None; entries.len()];
    let mut by_id: HashMap<Oid, usize> = HashMap::new();
    loop {
        let mut progress = false;
        for i in 0..entries.len() {
            if resolved[i].is_some() {
                continue;
            }
            let (_, base, kind, data) = &entries[i];
            let done = match base {
                None => Some((*kind, data.clone())),
                Some(Base::Offset(offset)) => {
                    let b = *index.get(offset).ok_or_else(damaged)?;
                    resolved[b].as_ref().map(|(k, d, _)| (*k, d)).map(|(k, d)| apply_delta(d, data).map(|out| (k, out))).transpose()?
                }
                Some(Base::Id(oid)) => match by_id.get(oid) {
                    Some(&b) => resolved[b].as_ref().map(|(k, d, _)| apply_delta(d, data).map(|out| (*k, out))).transpose()?,
                    None => None,
                },
            };
            if let Some((kind, data)) = done {
                let oid = oid_of(kind, &data);
                by_id.insert(oid, i);
                resolved[i] = Some((kind, data, oid));
                progress = true;
            }
        }
        if resolved.iter().all(Option::is_some) {
            break;
        }
        if !progress {
            return Err("the pack names objects it does not hold".into());
        }
    }
    Ok(resolved.into_iter().flatten().map(|(kind, data, oid)| (oid, Object { kind, data })).collect())
}

/// What a checkout wrote and left out.
#[derive(Debug, Default, PartialEq)]
pub struct Written {
    pub files: usize,
    pub bytes: u64,
    /// Symbolic links and submodules: never written.
    pub skipped: usize,
}

/// Writes the commit's files under `dest`. A name that could leave `dest` refuses the whole
/// checkout; links and submodules are left out.
pub fn checkout(objects: &HashMap<Oid, Object>, commit: &Oid, dest: &Path) -> Result<Written, String> {
    let missing = || "the pack lacks part of the commit".to_string();
    let commit = objects.get(commit).filter(|o| o.kind == 1).ok_or_else(missing)?;
    let tree = std::str::from_utf8(&commit.data)
        .ok()
        .and_then(|text| text.lines().next()?.strip_prefix("tree "))
        .and_then(unhex)
        .ok_or_else(missing)?;
    let mut written = Written::default();
    write_tree(objects, &tree, dest, 0, &mut written)?;
    Ok(written)
}

fn write_tree(objects: &HashMap<Oid, Object>, tree: &Oid, dir: &Path, depth: usize, written: &mut Written) -> Result<(), String> {
    if depth > 32 {
        return Err("folders nested too deep".into());
    }
    let data = &objects.get(tree).filter(|o| o.kind == 2).ok_or("the pack lacks a folder")?.data;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut at = 0;
    while at < data.len() {
        let space = data[at..].iter().position(|b| *b == b' ').ok_or("a damaged folder")? + at;
        let nul = data[space..].iter().position(|b| *b == 0).ok_or("a damaged folder")? + space;
        let oid: Oid = data.get(nul + 1..nul + 21).ok_or("a damaged folder")?.try_into().unwrap();
        let mode = &data[at..space];
        let name = std::str::from_utf8(&data[space + 1..nul]).map_err(|_| "a file name that is not UTF-8")?;
        at = nul + 21;
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) || name.eq_ignore_ascii_case(".git") {
            return Err(format!("the repository names a path that would leave it ({name:?})"));
        }
        let path = dir.join(name);
        match mode {
            b"40000" => write_tree(objects, &oid, &path, depth + 1, written)?,
            b"100644" | b"100755" | b"100664" => {
                let blob = objects.get(&oid).filter(|o| o.kind == 3).ok_or("the pack lacks a file")?;
                written.files += 1;
                written.bytes += blob.data.len() as u64;
                if written.files > MAX_FILES || written.bytes > MAX_BYTES {
                    return Err(format!("the repository holds over {MAX_FILES} files or {} MB", MAX_BYTES >> 20));
                }
                std::fs::write(&path, &blob.data).map_err(|e| e.to_string())?;
            }
            b"120000" | b"160000" => written.skipped += 1,
            _ => return Err(format!("{name}: an entry of unknown kind")),
        }
    }
    Ok(())
}

/// Fetches the repository's newest commit (on `#branch` when the address names one) into
/// `dest`, replacing what was there only once the whole checkout is written. Its commit id.
pub fn fetch(url: &str, dest: &Path) -> Result<(String, Written), String> {
    fetch_with(url, dest, true)
}

/// The commit wanted and the server's whole answer to the fetch.
fn fetch_answer(url: &str, v2: bool) -> Result<(Oid, Vec<u8>), String> {
    let (repo, branch) = split(url)?;
    let agent = agent();
    let (base, advert) = discover(&agent, &repo, v2)?;
    negotiate(&agent, &base, &advert, branch.as_deref(), v2)
}

fn fetch_with(url: &str, dest: &Path, v2: bool) -> Result<(String, Written), String> {
    let (want, answer) = fetch_answer(url, v2)?;
    let objects = unpack(&demux(&answer)?)?;
    let staging = dest.with_extension(format!("fetch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let written = checkout(&objects, &want, &staging).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&staging, dest).map_err(|e| e.to_string())?;
    Ok((hex(&want), written))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rex-addons-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// GitHub's real answers to a depth-1 fetch of this repository at 1dd7a64, over protocol v2
    /// and v0 (`capture::record_github_answers`).
    #[test]
    fn github_answers_unpack_and_check_out() {
        for name in ["v2", "v0"] {
            let answer = std::fs::read(format!("tests/fixtures/git/rex-addons-1dd7a64-{name}.bin")).unwrap();
            let objects = unpack(&demux(&answer).unwrap()).unwrap();
            let dest = temp(name);
            let written = checkout(&objects, &unhex("1dd7a6453bde8d1d16a85f42332b8a534f0de47b").unwrap(), &dest).unwrap();
            assert_eq!(written, Written { files: 21, bytes: 213910, skipped: 0 }, "{name}");
            assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), include_bytes!("../LICENSE"));
            assert!(dest.join("addons/internet-archive/main.js").is_file());
            let _ = std::fs::remove_dir_all(dest);
        }
    }

    /// Packs git itself made (2.50.1, `git pack-objects --window=10`) of one commit holding a
    /// text and a changed copy of it, one with the copy as an OFS_DELTA, one as a REF_DELTA.
    #[test]
    fn deltas_git_made_apply() {
        let mut a: Vec<String> = (0..300).map(|i| format!("line {i} of a text long enough that git finds it worth a delta\n")).collect();
        let expected_a = a.concat();
        a[150] = "a changed line in the middle\n".into();
        a.push("and one more at the end\n".into());
        for name in ["ofs", "ref"] {
            let objects = unpack(&std::fs::read(format!("tests/fixtures/git/delta-{name}.pack")).unwrap()).unwrap();
            let dest = temp(name);
            let written = checkout(&objects, &unhex("dd14af143dba8164e43e0d93e7213bf7da798edf").unwrap(), &dest).unwrap();
            assert_eq!(written.files, 2);
            assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), expected_a);
            assert_eq!(std::fs::read_to_string(dest.join("sub/b.txt")).unwrap(), a.concat());
            let _ = std::fs::remove_dir_all(dest);
        }
        let mut damaged = std::fs::read("tests/fixtures/git/delta-ofs.pack").unwrap();
        damaged[40] ^= 1;
        assert_eq!(unpack(&damaged).err().unwrap(), "the pack's checksum does not match");
    }

    fn put(objects: &mut HashMap<Oid, Object>, kind: u8, data: Vec<u8>) -> Oid {
        let oid = oid_of(kind, &data);
        objects.insert(oid, Object { kind, data });
        oid
    }

    fn tree(entries: &[(&str, &str, Oid)]) -> Vec<u8> {
        let mut data = Vec::new();
        for (mode, name, oid) in entries {
            data.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            data.extend_from_slice(oid);
        }
        data
    }

    fn commit_of(objects: &mut HashMap<Oid, Object>, entries: &[(&str, &str, Oid)]) -> Oid {
        let root = put(objects, 2, tree(entries));
        put(objects, 1, format!("tree {}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\nm\n", hex(&root)).into_bytes())
    }

    #[test]
    fn names_that_leave_the_repository_are_refused_and_links_left_out() {
        let mut objects = HashMap::new();
        let blob = put(&mut objects, 3, b"hello".to_vec());
        let target = put(&mut objects, 3, b"/etc/passwd".to_vec());
        let dest = temp("links");
        let commit = commit_of(&mut objects, &[("100644", "file.txt", blob), ("120000", "link", target), ("160000", "module", blob)]);
        assert_eq!(checkout(&objects, &commit, &dest).unwrap(), Written { files: 1, bytes: 5, skipped: 2 });
        assert!(std::fs::symlink_metadata(dest.join("link")).is_err());
        assert!(!dest.join("module").exists());
        for evil in ["..", ".", "a/b", "..\\x", ".git", ".GIT", ""] {
            let commit = commit_of(&mut objects, &[("100644", evil, blob)]);
            let error = checkout(&objects, &commit, &temp("evil")).unwrap_err();
            assert!(error.contains("would leave"), "{evil:?}: {error}");
        }
        let _ = std::fs::remove_dir_all(dest);
        let _ = std::fs::remove_dir_all(temp("evil"));
    }

    #[test]
    fn addresses() {
        let (url, branch) = split("https://codeberg.org/someone/addons.git#dev").unwrap();
        assert_eq!((url.as_str(), branch.as_deref()), ("https://codeberg.org/someone/addons.git", Some("dev")));
        assert_eq!(split("https://github.com/a/b/").unwrap().0.as_str(), "https://github.com/a/b");
        for (bad, says) in [("http://github.com/a/b", "https://"), ("git@github.com:a/b.git", "not a link"), ("https://me:pw@host/a", "login"), ("https://host/", "no repository")] {
            let error = split(bad).unwrap_err();
            assert!(error.contains(says), "{bad}: {error}");
        }
    }
}

#[cfg(test)]
mod live {
    use super::*;

    fn fetched(url: &str, v2: bool) -> Result<(String, Written, std::path::PathBuf), String> {
        let dest = std::env::temp_dir().join(format!("rex-addons-live-{}-{v2}", std::process::id()));
        fetch_with(url, &dest, v2).map(|(commit, written)| (commit, written, dest))
    }

    #[test]
    #[ignore = "network: GitHub, GitLab, Codeberg"]
    fn real_servers() {
        for v2 in [true, false] {
            let (commit, written, dest) = fetched("https://github.com/debridmediamanager/rex-addons", v2).unwrap();
            eprintln!("github v2={v2}: {commit} {written:?}");
            assert!(dest.join("addons/internet-archive/manifest.json").is_file());
            let _ = std::fs::remove_dir_all(dest);
        }
        // A shallow pack with real deltas in it (6 OFS_DELTA objects, 2026-11-10).
        let (commit, written, dest) = fetched("https://github.com/debridmediamanager/rex-stremio", true).unwrap();
        eprintln!("github rex-stremio: {commit} {written:?}");
        assert!(dest.join("rex_stremio.py").is_file());
        let _ = std::fs::remove_dir_all(dest);
        // GitLab redirects to the .git address; its test repository holds three submodules.
        let (commit, written, dest) = fetched("https://gitlab.com/gitlab-org/gitlab-test", true).unwrap();
        eprintln!("gitlab: {commit} {written:?}");
        assert_eq!(written.skipped, 3);
        assert!(!dest.join("gitlab-shell").exists());
        let _ = std::fs::remove_dir_all(dest);
        let (commit, written, dest) = fetched("https://codeberg.org/Codeberg/Community", true).unwrap();
        eprintln!("codeberg: {commit} {written:?}");
        let _ = std::fs::remove_dir_all(dest);
        assert_eq!(fetched("https://github.com/debridmediamanager/rex-addons#no-such-branch", true).err().unwrap(), "the repository has no branch no-such-branch");
        let error = fetched("https://github.com/debridmediamanager/no-such-repository-here", true).err().unwrap();
        assert!(error.contains("no public git repository"), "{error}");
        let error = fetched("https://notes.debridmediamanager.com/rex", true).err().unwrap();
        assert!(error.contains("not a git repository") || error.contains("no git repository"), "{error}");
    }
}

#[cfg(test)]
mod capture {
    /// Records the fixtures `offline` reads: GitHub's answers for this repository at 1dd7a64.
    #[test]
    #[ignore = "network: records tests/fixtures/git/"]
    fn record_github_answers() {
        for (v2, name) in [(true, "rex-addons-1dd7a64-v2.bin"), (false, "rex-addons-1dd7a64-v0.bin")] {
            let (want, answer) = super::fetch_answer("https://github.com/debridmediamanager/rex-addons#main", v2).unwrap();
            assert_eq!(super::hex(&want), "1dd7a6453bde8d1d16a85f42332b8a534f0de47b", "main moved: record at another commit");
            std::fs::create_dir_all("tests/fixtures/git").unwrap();
            std::fs::write(format!("tests/fixtures/git/{name}"), answer).unwrap();
        }
    }
}
