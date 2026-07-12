//! A blocking HTTP/1.1 client on `std::net::TcpStream`, run on the threadpool by the fetch
//! op. `Connection: close` per request (no pooling), Content-Length and chunked bodies,
//! redirects followed up to 5 hops. HTTPS uses lumen-tls's dynamically loaded system OpenSSL
//! backend with CA and hostname verification; no TLS crate is linked into the workspace.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::url;

pub(crate) struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Where the response actually came from (after redirects).
    pub url: String,
}

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Response-body cap so a hostile server can't balloon the worker (32 MiB).
const MAX_BODY: u64 = 32 << 20;

pub(crate) fn request(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<HttpResponse, String> {
    let mut method = method.to_ascii_uppercase();
    let mut target = target.to_string();
    let mut body = body.map(|b| b.to_vec());
    for _ in 0..=MAX_REDIRECTS {
        let u = url::parse(&target, None)?;
        match u.scheme.as_str() {
            "http" | "https" => {}
            other => return Err(format!("fetch: unsupported scheme '{other}'")),
        }
        let response = one_request(&method, &u, headers, body.as_deref())?;
        match response.status {
            301 | 302 | 303 | 307 | 308 => {
                let Some(location) = header(&response.headers, "location") else {
                    return Ok(HttpResponse {
                        url: u.href(),
                        ..response
                    });
                };
                target = url::parse(&location, Some(&u.href()))?.href();
                // 303 (and historically 301/302) switch to GET and drop the body.
                if response.status == 303
                    || ((response.status == 301 || response.status == 302) && method == "POST")
                {
                    method = "GET".to_string();
                    body = None;
                }
            }
            _ => {
                return Ok(HttpResponse {
                    url: u.href(),
                    ..response
                })
            }
        }
    }
    Err(format!("fetch '{target}': too many redirects"))
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn one_request(
    method: &str,
    u: &url::Url,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<HttpResponse, String> {
    let port = u.port.unwrap_or(if u.scheme == "https" { 443 } else { 80 });
    let host_header = match u.port {
        Some(p) => format!("{}:{}", u.host, p),
        None => u.host.clone(),
    };
    let stream = TcpStream::connect((u.host.trim_matches(['[', ']']), port))
        .map_err(|e| format!("fetch '{}': connect: {e}", u.href()))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut req = format!(
        "{method} {}{} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n",
        u.path, u.query
    );
    let mut have_ua = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("connection") {
            continue; // we own these
        }
        have_ua |= k.eq_ignore_ascii_case("user-agent");
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !have_ua {
        req.push_str(concat!(
            "User-Agent: lumen/",
            env!("CARGO_PKG_VERSION"),
            "\r\n"
        ));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");

    let mut stream: Box<dyn ReadWrite> = if u.scheme == "https" {
        Box::new(lumen_tls::TlsStream::connect(stream, u.host.trim_matches(['[', ']']))
            .map_err(|error| format!("fetch '{}': {error}", u.href()))?)
    } else {
        Box::new(stream)
    };
    stream
        .write_all(req.as_bytes())
        .and_then(|()| body.map_or(Ok(()), |b| stream.write_all(b)))
        .map_err(|e| format!("fetch '{}': write: {e}", u.href()))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| format!("fetch '{}': read: {e}", u.href()))?;
    // "HTTP/1.1 200 OK"
    let mut parts = status_line.trim_end().splitn(3, ' ');
    let _version = parts.next().unwrap_or("");
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
        format!(
            "fetch '{}': malformed status line {status_line:?}",
            u.href()
        )
    })?;
    let status_text = parts.next().unwrap_or("").to_string();

    let mut headers_out = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("fetch '{}': read headers: {e}", u.href()))?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(i) = line.find(':') {
            headers_out.push((line[..i].to_string(), line[i + 1..].trim().to_string()));
        }
    }

    let body = read_body(&mut reader, &headers_out, method, status)
        .map_err(|e| format!("fetch '{}': body: {e}", u.href()))?;
    Ok(HttpResponse {
        status,
        status_text,
        headers: headers_out,
        body,
        url: String::new(), // stamped by the redirect loop
    })
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn read_body(
    reader: &mut impl BufRead,
    headers: &[(String, String)],
    method: &str,
    status: u16,
) -> std::io::Result<Vec<u8>> {
    if method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Vec::new());
    }
    if header(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return read_chunked(reader);
    }
    if let Some(len) = header(headers, "content-length").and_then(|v| v.parse::<u64>().ok()) {
        let mut body = vec![0u8; len.min(MAX_BODY) as usize];
        reader.read_exact(&mut body)?;
        return Ok(body);
    }
    // No framing: Connection: close means read to EOF.
    let mut body = Vec::new();
    reader.take(MAX_BODY).read_to_end(&mut body)?;
    Ok(body)
}

pub(crate) fn read_chunked(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size = usize::from_str_radix(
            size_line.trim_end().split(';').next().unwrap_or("").trim(),
            16,
        )
        .map_err(|_| std::io::Error::other(format!("bad chunk size {size_line:?}")))?;
        if size == 0 {
            // Trailer section (usually just the final CRLF).
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim_end().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() as u64 + size as u64 > MAX_BODY {
            return Err(std::io::Error::other("response body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_decoding() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut r = std::io::BufReader::new(&raw[..]);
        assert_eq!(read_chunked(&mut r).unwrap(), b"Wikipedia");
    }

    #[test]
    #[ignore = "requires external network and a system OpenSSL trust store"]
    fn https_uses_verified_tls() {
        let response = request("GET", "https://example.com/", &[], None).unwrap();
        assert_eq!(response.status, 200);
    }
}
