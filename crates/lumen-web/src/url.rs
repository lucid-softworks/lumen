//! A pragmatic WHATWG-URL subset: absolute parsing, relative resolution, special-scheme
//! default ports, path normalization. Deliberately NOT implemented yet (each is a visible
//! error, not a wrong answer): IDNA/punycode hosts (non-ASCII hosts are rejected), full
//! percent-encode sets (existing escapes pass through untouched), file-URL windows drive
//! quirks.

/// Parsed components. `port` is `None` when absent or the scheme default.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Url {
    pub scheme: String,
    pub username: String,
    pub password: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: String,    // includes leading '?' when non-empty
    pub fragment: String, // includes leading '#' when non-empty
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

fn is_special(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp" | "file")
}

impl Url {
    pub fn href(&self) -> String {
        let mut out = format!("{}:", self.scheme);
        if !self.host.is_empty() || self.scheme == "file" {
            out.push_str("//");
            if !self.username.is_empty() || !self.password.is_empty() {
                out.push_str(&self.username);
                if !self.password.is_empty() {
                    out.push(':');
                    out.push_str(&self.password);
                }
                out.push('@');
            }
            out.push_str(&self.host);
            if let Some(p) = self.port {
                out.push_str(&format!(":{p}"));
            }
        }
        out.push_str(&self.path);
        out.push_str(&self.query);
        out.push_str(&self.fragment);
        out
    }

    pub fn origin(&self) -> String {
        match self.port {
            Some(p) => format!("{}://{}:{}", self.scheme, self.host, p),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// `input` parsed on its own, or resolved against `base` when relative.
pub(crate) fn parse(input: &str, base: Option<&str>) -> Result<Url, String> {
    let input = input.trim_matches(|c: char| c.is_ascii_whitespace() || c.is_control());
    if let Some(url) = try_parse_absolute(input)? {
        return Ok(url);
    }
    let Some(base) = base else {
        return Err(format!(
            "invalid URL '{input}' (relative, and no base given)"
        ));
    };
    let base =
        try_parse_absolute(base.trim())?.ok_or_else(|| format!("invalid base URL '{base}'"))?;
    resolve(input, base)
}

/// `Some(url)` when input has a scheme, `None` when it's relative.
fn try_parse_absolute(input: &str) -> Result<Option<Url>, String> {
    let Some(colon) = input.find(':') else {
        return Ok(None);
    };
    let scheme = &input[..colon];
    if scheme.is_empty()
        || !scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Ok(None); // "a/b:c" style — not a scheme, treat as relative
    }
    let scheme = scheme.to_ascii_lowercase();
    let rest = &input[colon + 1..];

    if let Some(rest) = rest.strip_prefix("//") {
        let (authority_and_path, query, fragment) = split_query_fragment(rest);
        let (authority, path) = match authority_and_path.find('/') {
            Some(i) => (&authority_and_path[..i], &authority_and_path[i..]),
            None => (authority_and_path, ""),
        };
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(i) => (&authority[..i], &authority[i + 1..]),
            None => ("", authority),
        };
        let (username, password) = match userinfo.find(':') {
            Some(i) => (&userinfo[..i], &userinfo[i + 1..]),
            None => (userinfo, ""),
        };
        let (host, port) = parse_hostport(hostport, &scheme)?;
        if is_special(&scheme) && host.is_empty() && scheme != "file" {
            return Err(format!("invalid URL '{input}': missing host"));
        }
        let path = if path.is_empty() && is_special(&scheme) {
            "/".to_string()
        } else {
            normalize_path(path)
        };
        Ok(Some(Url {
            scheme,
            username: username.to_string(),
            password: password.to_string(),
            host,
            port,
            path,
            query: query.to_string(),
            fragment: fragment.to_string(),
        }))
    } else if is_special(&scheme) {
        Err(format!("invalid URL '{input}': special scheme without //"))
    } else {
        // Opaque path (mailto:, data:, javascript:); kept verbatim.
        let (path, query, fragment) = split_query_fragment(rest);
        Ok(Some(Url {
            scheme,
            username: String::new(),
            password: String::new(),
            host: String::new(),
            port: None,
            path: path.to_string(),
            query: query.to_string(),
            fragment: fragment.to_string(),
        }))
    }
}

fn parse_hostport(hostport: &str, scheme: &str) -> Result<(String, Option<u16>), String> {
    // [v6::addr]:port — the bracket form is the only place ':' is part of a host.
    let (host, port_str) = if let Some(rest) = hostport.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(format!("invalid host '{hostport}'"));
        };
        let after = &rest[end + 1..];
        let port = after.strip_prefix(':').unwrap_or("");
        (format!("[{}]", &rest[..end]), port)
    } else {
        match hostport.find(':') {
            Some(i) => (hostport[..i].to_string(), &hostport[i + 1..]),
            None => (hostport.to_string(), ""),
        }
    };
    if !host.is_ascii() {
        return Err(format!(
            "non-ASCII host '{host}' (IDNA is not implemented yet)"
        ));
    }
    let host = host.to_ascii_lowercase();
    let port = if port_str.is_empty() {
        None
    } else {
        let p: u16 = port_str
            .parse()
            .map_err(|_| format!("invalid port '{port_str}'"))?;
        (Some(p) != default_port(scheme)).then_some(p)
    };
    Ok((host, port))
}

fn split_query_fragment(s: &str) -> (&str, &str, &str) {
    let (before_frag, fragment) = match s.find('#') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let (path, query) = match before_frag.find('?') {
        Some(i) => (&before_frag[..i], &before_frag[i..]),
        None => (before_frag, ""),
    };
    (path, query, fragment)
}

/// Resolve `.`/`..` segments; preserves a trailing slash.
fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let trailing_slash = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut s = String::from("/");
    s.push_str(&out.join("/"));
    if trailing_slash && s.len() > 1 {
        s.push('/');
    }
    s
}

/// RFC 3986-style relative resolution against an already-parsed base.
fn resolve(input: &str, base: Url) -> Result<Url, String> {
    if let Some(rest) = input.strip_prefix("//") {
        // Protocol-relative: keep the scheme, reparse the rest as authority.
        return try_parse_absolute(&format!("{}://{}", base.scheme, rest))?
            .ok_or_else(|| format!("invalid URL '//{rest}'"));
    }
    let (path_part, query, fragment) = split_query_fragment(input);
    let (path, query) = if path_part.is_empty() && query.is_empty() {
        // Fragment-only (or empty): keep base path AND query.
        (base.path.clone(), base.query.clone())
    } else if path_part.is_empty() {
        (base.path.clone(), query.to_string())
    } else if path_part.starts_with('/') {
        (normalize_path(path_part), query.to_string())
    } else {
        let dir = match base.path.rfind('/') {
            Some(i) => &base.path[..=i],
            None => "/",
        };
        (
            normalize_path(&format!("{dir}{path_part}")),
            query.to_string(),
        )
    };
    Ok(Url {
        path,
        query,
        fragment: fragment.to_string(),
        ..base
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Url {
        parse(s, None).unwrap()
    }

    #[test]
    fn absolute_basics() {
        let u = p("HTTP://User:Pw@Example.COM:8080/a/b?q=1#frag");
        assert_eq!(u.scheme, "http");
        assert_eq!(u.username, "User");
        assert_eq!(u.password, "Pw");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8080));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query, "?q=1");
        assert_eq!(u.fragment, "#frag");
        assert_eq!(u.href(), "http://User:Pw@example.com:8080/a/b?q=1#frag");
    }

    #[test]
    fn default_port_dropped_and_path_added() {
        assert_eq!(p("http://x.com:80").href(), "http://x.com/");
        assert_eq!(p("https://x.com:443/a").href(), "https://x.com/a");
        assert_eq!(p("http://x.com:8080").port, Some(8080));
    }

    #[test]
    fn path_normalization() {
        assert_eq!(p("http://x.com/a/b/../c/./d").path, "/a/c/d");
        assert_eq!(p("http://x.com/a/..").path, "/");
        assert_eq!(p("http://x.com/a/b/").path, "/a/b/");
    }

    #[test]
    fn relative_resolution() {
        let base = Some("http://x.com/a/b/c?old#f");
        assert_eq!(parse("d", base).unwrap().href(), "http://x.com/a/b/d");
        assert_eq!(parse("../d", base).unwrap().href(), "http://x.com/a/d");
        assert_eq!(parse("/d", base).unwrap().href(), "http://x.com/d");
        assert_eq!(
            parse("?q=2", base).unwrap().href(),
            "http://x.com/a/b/c?q=2"
        );
        assert_eq!(
            parse("#g", base).unwrap().href(),
            "http://x.com/a/b/c?old#g"
        );
        assert_eq!(parse("//y.com/z", base).unwrap().href(), "http://y.com/z");
    }

    #[test]
    fn ipv6_and_errors() {
        let u = p("http://[::1]:9000/x");
        assert_eq!(u.host, "[::1]");
        assert_eq!(u.port, Some(9000));
        assert!(parse("http://", None).is_err());
        assert!(parse("nobase", None).is_err());
        assert!(parse("http://bücher.de/", None).is_err(), "IDNA flagged");
    }

    #[test]
    fn opaque_schemes() {
        let u = p("mailto:a@b.c");
        assert_eq!(u.scheme, "mailto");
        assert_eq!(u.path, "a@b.c");
        assert_eq!(u.href(), "mailto:a@b.c");
    }
}
