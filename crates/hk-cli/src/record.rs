//! `hk record` (T-061): records a selection's, emitter's or band's outputs on a running
//! `hk serve` (`POST /api/outputs/record/start`), waits until the recording ends (its `max_s` or
//! `max_bytes`, or Ctrl-C, which stops it), then downloads every file and sidecar into `--out`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde_json::{Value, json};

/// Whose outputs to record.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordTarget {
    /// A persisted selection id.
    Selection(String),
    /// An inventory emitter id.
    Emitter(String),
    /// A band, Hz.
    Band(f64, f64),
}

/// `hk record` options.
#[derive(Clone, Debug)]
pub struct RecordOptions {
    /// The `hk serve` API address.
    pub server: SocketAddr,
    /// API token.
    pub token: String,
    /// Target.
    pub target: RecordTarget,
    /// Kinds: `bits`, `symbols`, `audio`, `iq`.
    pub kinds: Vec<String>,
    /// Longest recording, s.
    pub max_s: Option<f64>,
    /// Largest recording, bytes.
    pub max_bytes: Option<u64>,
    /// Directory the files are downloaded into (created).
    pub out: PathBuf,
}

/// A finished `hk record`.
#[derive(Clone, Debug)]
pub struct RecordOutcome {
    /// The final session JSON.
    pub session: Value,
    /// Downloaded files.
    pub files: Vec<PathBuf>,
}

/// Parses `LO:HI` in Hz (e.g. `433.8e6:434.1e6`).
pub fn parse_band(s: &str) -> anyhow::Result<(f64, f64)> {
    let (lo, hi) = s
        .split_once(':')
        .context("a band is LO:HI in Hz, e.g. 433.8e6:434.1e6")?;
    let lo: f64 = lo.trim().parse().context("band LO")?;
    let hi: f64 = hi.trim().parse().context("band HI")?;
    if !(lo > 0.0 && lo < hi) {
        bail!("a band needs 0 < LO < HI");
    }
    Ok((lo, hi))
}

/// One HTTP/1.1 request with the bearer token; returns status and body.
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut s = TcpStream::connect(addr).with_context(|| format!("connecting to {addr}"))?;
    s.set_read_timeout(Some(Duration::from_secs(120)))?;
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n"
    );
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(payload.as_bytes())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed HTTP response")?;
    let status = std::str::from_utf8(&raw[..split])?
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .context("malformed HTTP status line")?;
    Ok((status, raw[split + 4..].to_vec()))
}

fn json_call(
    opts: &RecordOptions,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> anyhow::Result<Value> {
    let (status, bytes) = http(opts.server, method, path, &opts.token, body)?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status != 200 {
        bail!(
            "{method} {path}: {status} {}: {}",
            v["code"].as_str().unwrap_or("error"),
            v["error"].as_str().unwrap_or("request failed")
        );
    }
    Ok(v)
}

/// Runs a recording to its end; `stop` is polled and stops it early when it returns `true`.
pub fn run(opts: &RecordOptions, stop: impl Fn() -> bool) -> anyhow::Result<RecordOutcome> {
    let mut body = json!({ "kinds": opts.kinds });
    match &opts.target {
        RecordTarget::Selection(id) => body["selection_id"] = json!(id),
        RecordTarget::Emitter(id) => body["emitter_id"] = json!(id),
        RecordTarget::Band(lo, hi) => body["band"] = json!({ "f_lo": lo, "f_hi": hi }),
    }
    if let Some(s) = opts.max_s {
        body["max_s"] = json!(s);
    }
    if let Some(b) = opts.max_bytes {
        body["max_bytes"] = json!(b);
    }
    let started = json_call(opts, "POST", "/api/outputs/record/start", Some(&body))?;
    let id = started["recording"]["id"]
        .as_str()
        .context("the start response has no recording id")?
        .to_owned();
    eprintln!("hk record: recording {id} ({})", opts.kinds.join(", "));
    let session = loop {
        if stop() {
            eprintln!("hk record: stopping {id}");
            break json_call(
                opts,
                "POST",
                "/api/outputs/record/stop",
                Some(&json!({ "id": id })),
            )?["recording"]
                .clone();
        }
        let list = json_call(opts, "GET", "/api/outputs", None)?;
        let current = list["recordings"]
            .as_array()
            .and_then(|a| a.iter().find(|s| s["id"] == id.as_str()))
            .cloned()
            .context("the recording disappeared from /api/outputs")?;
        if current["active"] != true {
            break current;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    std::fs::create_dir_all(&opts.out)
        .with_context(|| format!("creating {}", opts.out.display()))?;
    let mut files = Vec::new();
    for f in session["files"].as_array().into_iter().flatten() {
        eprintln!(
            "hk record: {} {}: {} bytes, {} records, {} dropped{}",
            f["kind"].as_str().unwrap_or("?"),
            f["state"].as_str().unwrap_or("?"),
            f["bytes"],
            f["records"],
            f["dropped_records"],
            f["message"]
                .as_str()
                .map(|m| format!(" ({m})"))
                .unwrap_or_default()
        );
        if f["state"] == "refused" {
            continue;
        }
        let urls = [&f["url"], &f["sidecar_url"]]
            .into_iter()
            .chain(f["extra_urls"].as_array().into_iter().flatten());
        for url in urls.filter_map(Value::as_str) {
            files.push(download(opts, url, &opts.out)?);
        }
    }
    Ok(RecordOutcome { session, files })
}

fn download(opts: &RecordOptions, url: &str, out: &Path) -> anyhow::Result<PathBuf> {
    let name = url.rsplit('/').next().context("download url")?;
    let (status, bytes) = http(opts.server, "GET", url, &opts.token, None)?;
    if status != 200 {
        bail!("GET {url}: {status}");
    }
    let path = out.join(name);
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_parse() {
        assert_eq!(parse_band("433.8e6:434.1e6").unwrap(), (433.8e6, 434.1e6));
        assert!(parse_band("434e6:433e6").is_err());
        assert!(parse_band("434e6").is_err());
    }
}
