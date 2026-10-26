//! Downloading release assets and checking them against published SHA-256
//! checksums. Nothing is installed unless this module says the file is good.

use crate::error::{Error, ErrorCode, Result};
use crate::http::{read_body, Http};
use crate::release::{Asset, Release};
use crate::source::Source;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use url::Url;

pub const MAX_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 4 * 1024 * 1024;

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn asset_url(source: &Source, release: &Release, asset: &Asset) -> Result<Url> {
    let url = Url::parse(&asset.url)
        .map_err(|_| Error::new(ErrorCode::Download, format!("The release lists a bad address for {}", asset.name)))?;
    if !source.allows_download(&url, &release.assets) {
        return Err(Error::new(
            ErrorCode::Download,
            format!("Refusing to download {}: its address is not part of {}/{}'s releases", asset.name, source.owner, source.repo),
        ));
    }
    Ok(url)
}

/// Download `asset` into `dir` under its own name. `progress(bytes, total)` is
/// called at most every 150 ms, and once at the end.
pub fn download(
    http: &Http,
    source: &Source,
    release: &Release,
    asset: &Asset,
    dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let url = asset_url(source, release, asset)?;
    let name = Path::new(&asset.name).file_name().and_then(|n| n.to_str());
    if name != Some(asset.name.as_str()) || asset.name.starts_with('.') {
        return Err(Error::new(ErrorCode::Download, format!("The release has an odd file name ({:?}); not downloading it", asset.name)));
    }
    let resp = http.get(&url, "application/octet-stream", None, &[])?;
    if resp.status() != 200 {
        return Err(Error::new(ErrorCode::Download, format!("The download of {} failed (HTTP {})", asset.name, resp.status())));
    }
    let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()).or(Some(asset.size).filter(|s| *s > 0));
    if total.is_some_and(|t| t > MAX_ASSET_BYTES) {
        return Err(Error::new(ErrorCode::Download, format!("{} is unreasonably large; not downloading it", asset.name)));
    }
    let dest = dir.join(&asset.name);
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&dest)?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let (mut bytes, mut last) = (0u64, Instant::now());
    progress(0, total);
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::new(ErrorCode::Download, format!("The connection dropped while downloading {}: {e}", asset.name)))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        bytes += n as u64;
        if bytes > MAX_ASSET_BYTES {
            return Err(Error::new(ErrorCode::Download, format!("{} is unreasonably large; stopped", asset.name)));
        }
        if last.elapsed() >= Duration::from_millis(150) {
            progress(bytes, total);
            last = Instant::now();
        }
    }
    out.sync_all()?;
    if let Some(t) = total {
        if t != bytes {
            return Err(Error::new(ErrorCode::Download, format!("The download of {} was cut short ({bytes} of {t} bytes)", asset.name)));
        }
    }
    progress(bytes, total);
    Ok(dest)
}

/// Find the digest for `name` in a checksum file. `bare_ok` accepts a file
/// holding just a hash (the `<asset>.sha256` convention). Understands
/// `hash  name`, `hash *name` and BSD `SHA256 (name) = hash`.
pub fn parse_checksum(contents: &str, name: &str, bare_ok: bool) -> Option<String> {
    let is_hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let clean = |s: &str| s.trim_start_matches('*').trim_start_matches("./").to_string();
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SHA256 (") {
            if let Some((n, h)) = rest.split_once(") = ") {
                if n == name && is_hash(h.trim()) {
                    return Some(h.trim().to_ascii_lowercase());
                }
            }
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(h), second) = (it.next(), it.next()) else { continue };
        if !is_hash(h) {
            continue;
        }
        match second {
            None if bare_ok => return Some(h.to_ascii_lowercase()),
            Some(n) if clean(n) == name => return Some(h.to_ascii_lowercase()),
            _ => {}
        }
    }
    None
}

fn fetch_text(http: &Http, source: &Source, release: &Release, asset: &Asset) -> Result<String> {
    let url = asset_url(source, release, asset)?;
    let resp = http.get(&url, "text/plain", None, &[])?;
    if resp.status() != 200 {
        return Err(Error::new(ErrorCode::Download, format!("Could not fetch {} (HTTP {})", asset.name, resp.status())));
    }
    let bytes = read_body(resp, MAX_CHECKSUM_BYTES)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Check `file` (the downloaded `asset`) against the release's published
/// checksums: `<asset>.sha256` and/or `SHA256SUMS`. Every source that has an
/// entry for the asset must agree with the file; with none, verification fails.
/// Returns the digest on success.
pub fn verify(http: &Http, source: &Source, release: &Release, asset: &Asset, file: &Path) -> Result<String> {
    let sidecar = format!("{}.sha256", asset.name);
    let mut expected: Vec<(String, String)> = Vec::new();
    if let Some(a) = release.assets.iter().find(|a| a.name == sidecar) {
        if let Some(h) = parse_checksum(&fetch_text(http, source, release, a)?, &asset.name, true) {
            expected.push((sidecar.clone(), h));
        }
    }
    if let Some(a) = release.assets.iter().find(|a| matches!(a.name.to_ascii_lowercase().as_str(), "sha256sums" | "sha256sums.txt")) {
        if let Some(h) = parse_checksum(&fetch_text(http, source, release, a)?, &asset.name, false) {
            expected.push((a.name.clone(), h));
        }
    }
    if expected.is_empty() {
        return Err(Error::new(
            ErrorCode::ChecksumMissing,
            format!("The release has no usable checksum for {} (looked for {sidecar} and SHA256SUMS), so it will not be installed", asset.name),
        ));
    }
    let actual = sha256_file(file)?;
    for (src, want) in &expected {
        if *want != actual {
            return Err(Error::new(
                ErrorCode::ChecksumMismatch,
                format!("The downloaded {} does not match the checksum in {src}. It was not installed.", asset.name),
            ));
        }
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha256_of_known_input() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("abc");
        std::fs::write(&p, "abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), H);
    }

    #[test]
    fn checksum_formats() {
        assert_eq!(parse_checksum(&format!("{H}\n"), "a.deb", true).as_deref(), Some(H));
        assert_eq!(parse_checksum(&format!("{H}  a.deb\n"), "a.deb", true).as_deref(), Some(H));
        assert_eq!(parse_checksum(&format!("{H}  other.deb\n"), "a.deb", true), None);
        assert_eq!(parse_checksum(&format!("{H} *a.deb"), "a.deb", false).as_deref(), Some(H));
        assert_eq!(parse_checksum(&format!("{H}  ./a.deb"), "a.deb", false).as_deref(), Some(H));
        assert_eq!(parse_checksum(&format!("SHA256 (a.deb) = {H}"), "a.deb", false).as_deref(), Some(H));
        assert_eq!(parse_checksum(&format!("{H}\n"), "a.deb", false), None);
        assert_eq!(parse_checksum("not a hash  a.deb", "a.deb", true), None);
        let sums = format!("{}  x\n{H}  a.deb\n", "0".repeat(64));
        assert_eq!(parse_checksum(&sums, "a.deb", false).as_deref(), Some(H));
        assert_eq!(parse_checksum(&H.to_uppercase(), "a.deb", true).as_deref(), Some(H));
    }
}
