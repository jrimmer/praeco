#![allow(dead_code)]
//! Test support: a fake release server on 127.0.0.1, fixtures, and shim scripts.

use praeco::detect::Env;
use praeco::source::Source;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Req {
    /// Path without the query string.
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub struct FakeServer {
    pub base: String,
    addr: std::net::SocketAddr,
    routes: Arc<Mutex<HashMap<String, Resp>>>,
    pub requests: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        403 => "Forbidden",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn handle(mut s: TcpStream, routes: &Mutex<HashMap<String, Resp>>, requests: &Mutex<Vec<Req>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 65536 {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let target = first.split_whitespace().nth(1).unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/").to_string();
    let headers: Vec<(String, String)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let req = Req { path: path.clone(), headers };
    let inm = req.header("if-none-match").map(str::to_string);
    requests.lock().unwrap().push(req);
    let resp = routes.lock().unwrap().get(&path).cloned();
    let mut resp = resp.unwrap_or(Resp { status: 404, headers: vec![], body: b"not found".to_vec() });
    if let Some(inm) = inm {
        if resp.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("etag") && *v == inm) {
            resp = Resp { status: 304, headers: resp.headers.clone(), body: vec![] };
        }
    }
    let mut out = format!("HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, reason(resp.status), resp.body.len());
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let _ = s.write_all(out.as_bytes());
    let _ = s.write_all(&resp.body);
    let _ = s.flush();
}

impl FakeServer {
    pub fn start() -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let routes: Arc<Mutex<HashMap<String, Resp>>> = Arc::default();
        let requests: Arc<Mutex<Vec<Req>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (r, q, st) = (routes.clone(), requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(c) = conn {
                    let (r, q) = (r.clone(), q.clone());
                    std::thread::spawn(move || handle(c, &r, &q));
                }
            }
        });
        FakeServer { base: format!("http://{addr}"), addr, routes, requests, stop }
    }

    pub fn route(&self, path: &str, status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.routes.lock().unwrap().insert(path.to_string(), Resp { status, headers, body: body.into() });
    }

    pub fn requests_for(&self, path: &str) -> Vec<Req> {
        self.requests.lock().unwrap().iter().filter(|r| r.path == path).cloned().collect()
    }

    pub fn source(&self) -> Source {
        Source::forgejo(&self.base, "o/r").unwrap()
    }

    pub fn releases_path() -> &'static str {
        "/api/v1/repos/o/r/releases"
    }

    pub fn asset_path(tag: &str, name: &str) -> String {
        format!("/o/r/releases/download/{tag}/{name}")
    }

    pub fn asset_url(&self, tag: &str, name: &str) -> String {
        format!("{}{}", self.base, FakeServer::asset_path(tag, name))
    }

    /// Publish the release list.
    pub fn publish_releases(&self, releases: Vec<Value>) {
        self.route(FakeServer::releases_path(), 200, &[("Content-Type", "application/json")], serde_json::to_vec(&releases).unwrap());
    }

    /// Release JSON whose assets point at this server.
    pub fn release(&self, tag: &str, body: &str, assets: &[&str]) -> Value {
        let assets: Vec<Value> = assets.iter().map(|n| json!({"name": n, "browser_download_url": self.asset_url(tag, n), "size": 0})).collect();
        json!({"tag_name": tag, "name": tag, "body": body, "draft": false, "prerelease": false,
               "html_url": format!("{}/o/r/releases/tag/{tag}", self.base), "published_at": "2026-10-01T00:00:00Z", "assets": assets})
    }

    pub fn serve_asset(&self, tag: &str, name: &str, bytes: &[u8]) {
        self.route(&FakeServer::asset_path(tag, name), 200, &[], bytes.to_vec());
    }

    /// Serve `name` plus a correct `name.sha256`; returns the release JSON.
    pub fn publish_one(&self, tag: &str, name: &str, bytes: &[u8]) -> Value {
        self.serve_asset(tag, name, bytes);
        self.serve_asset(tag, &format!("{name}.sha256"), format!("{}  {name}\n", sha256_hex(bytes)).as_bytes());
        self.release(tag, "notes", &[name, &format!("{name}.sha256")])
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn write_exec(path: &Path, script: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A shim for a tool: logs its argv to `<dir>/<name>.log`, prints a line
/// resembling a package query, and exits with `code`.
pub fn shim(dir: &Path, name: &str, code: i32) -> PathBuf {
    let p = dir.join(name);
    write_exec(&p, &format!("#!/bin/sh\necho \"$@\" >> '{}/{name}.log'\necho \"{name}: owned by fairfax\"\nexit {code}\n", dir.display()));
    p
}

/// An escalation command that records its argv (one per line) in `rec` and a
/// copy of its last argument in `rec.file`, then exits with `code`.
pub fn escalator(dir: &Path, name: &str, rec: &Path, code: i32, stderr: &str) -> PathBuf {
    let p = dir.join(name);
    write_exec(
        &p,
        &format!(
            "#!/bin/sh\nPATH=/usr/bin:/bin\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{rec}'; done\nfor last; do :; done\ncp \"$last\" '{rec}.file' 2>/dev/null\necho '{stderr}' >&2\nexit {code}\n",
            rec = rec.display()
        ),
    );
    p
}

pub fn recorded(rec: &Path) -> Vec<String> {
    std::fs::read_to_string(rec).unwrap_or_default().lines().map(str::to_string).collect()
}

/// An Env that sees only `path_dir` for tools, no AppImage, and a fake /proc.
pub fn env_with(path_dir: &Path, home: &Path) -> Env {
    Env {
        path: OsString::from(path_dir),
        appimage: None,
        appdir: None,
        home: Some(home.to_path_buf()),
        euid: 1000,
        graphical: true,
        proc_root: path_dir.join("no-proc"),
        terminal: None,
        arch: "x86_64".into(),
    }
}

pub fn tgz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    {
        let mut b = tar::Builder::new(&mut gz);
        for (name, data, mode) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.finish().unwrap();
    }
    gz.finish().unwrap()
}
