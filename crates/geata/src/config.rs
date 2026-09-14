use std::{collections::BTreeMap, net::IpAddr, path::Path};

use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
#[error("Geatafile line {line}: {message}")]
pub struct ConfigError {
    line: usize,
    message: String,
}

#[derive(Clone, Debug)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub authority: String,
}

#[derive(Clone, Debug)]
pub struct Site {
    pub domain: String,
    pub https: bool,
    pub handler: Handler,
}

#[derive(Clone, Debug)]
pub enum Handler {
    Proxy(Upstream),
    Respond { body: String, status: u16 },
}

#[derive(Debug)]
pub struct Config {
    pub sites: BTreeMap<String, Site>,
}

impl Config {
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        Self::parse(
            &std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        )
        .map_err(Into::into)
    }

    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let error = |line, message: &str| ConfigError {
            line,
            message: message.to_owned(),
        };
        if input.len() > 1024 * 1024 {
            return Err(error(1, "configuration exceeds 1 MiB"));
        }
        let tokens = tokenize(input)?;
        let mut sites = BTreeMap::new();
        let mut cursor = 0;
        while cursor < tokens.len() {
            let line = tokens[cursor].line;
            let address = tokens[cursor]
                .text()
                .ok_or_else(|| error(line, "expected a site address"))?;
            let https = !address.starts_with("http://");
            let domain = address
                .strip_prefix("http://")
                .or_else(|| address.strip_prefix("https://"))
                .unwrap_or(address)
                .to_ascii_lowercase();
            if !valid_domain(&domain, https) {
                return Err(error(
                    line,
                    "expected a domain (e.g. example.com); use http://localhost for local HTTP. Wildcards, local HTTPS, and site ports are not supported yet",
                ));
            }
            if !tokens
                .get(cursor + 1)
                .is_some_and(|t| matches!(t.kind, TokenKind::Open))
            {
                return Err(error(line, "expected '{' after the site address"));
            }
            let start = cursor + 2;
            cursor = start;
            while tokens
                .get(cursor)
                .is_some_and(|t| matches!(t.kind, TokenKind::Text(_)))
            {
                cursor += 1;
            }
            if !tokens
                .get(cursor)
                .is_some_and(|t| matches!(t.kind, TokenKind::Close))
            {
                return Err(error(
                    line,
                    "expected '}' after one reverse_proxy or respond directive",
                ));
            }
            let args = &tokens[start..cursor];
            let handler = match args.first().and_then(Token::text) {
                Some("reverse_proxy") if args.len() == 2 => Handler::Proxy(
                    parse_upstream(args[1].text().unwrap_or_default())
                        .map_err(|message| error(args[1].line, &message))?,
                ),
                Some("respond") => parse_response(&args[1..], line)?,
                _ => {
                    return Err(error(
                        line,
                        "expected one directive: reverse_proxy <backend> or respond \"Hello world!\" [status]",
                    ));
                }
            };
            let site = Site {
                domain: domain.clone(),
                https,
                handler,
            };
            if sites.insert(domain.clone(), site).is_some() {
                return Err(error(line, &format!("duplicate site {domain}")));
            }
            cursor += 1;
        }
        if sites.is_empty() {
            return Err(error(
                1,
                "add at least one site: example.com { reverse_proxy localhost:3000 }",
            ));
        }
        Ok(Self { sites })
    }

    pub fn https_domains(&self) -> impl Iterator<Item = &str> {
        self.sites
            .values()
            .filter(|s| s.https)
            .map(|s| s.domain.as_str())
    }
}

struct Token {
    line: usize,
    kind: TokenKind,
    quoted: bool,
}

enum TokenKind {
    Text(String),
    Open,
    Close,
}

impl Token {
    fn text(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Text(text) => Some(text),
            _ => None,
        }
    }
}

// Keep structural braces and comments distinct from literal response text.
fn tokenize(input: &str) -> Result<Vec<Token>, ConfigError> {
    let mut chars = input.char_indices().peekable();
    let mut line = 1;
    let mut tokens = Vec::new();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            if c == '\n' {
                line += 1;
            }
            continue;
        }
        if c == '#' {
            for (_, c) in chars.by_ref() {
                if c == '\n' {
                    line += 1;
                    break;
                }
            }
            continue;
        }
        let token_line = line;
        let kind = match c {
            '{' => TokenKind::Open,
            '}' => TokenKind::Close,
            '"' => {
                let mut escaped = false;
                let mut end = None;
                for (index, c) in chars.by_ref() {
                    if c == '\n' {
                        line += 1;
                    }
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        end = Some(index + 1);
                        break;
                    }
                }
                let end = end.ok_or_else(|| ConfigError {
                    line: token_line,
                    message: "unterminated quoted string".to_owned(),
                })?;
                let value = serde_json::from_str(&input[start..end]).map_err(|e| ConfigError {
                    line: token_line,
                    message: format!("invalid quoted string: {e}"),
                })?;
                if chars
                    .peek()
                    .is_some_and(|(_, c)| !c.is_whitespace() && !matches!(c, '{' | '}' | '#'))
                {
                    return Err(ConfigError {
                        line,
                        message: "expected whitespace after a quoted string".to_owned(),
                    });
                }
                TokenKind::Text(value)
            }
            _ => {
                let mut end = start + c.len_utf8();
                while let Some(&(index, c)) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '{' | '}' | '#') {
                        break;
                    }
                    if c == '"' {
                        return Err(ConfigError {
                            line,
                            message: "quoted strings must start at the beginning of an argument"
                                .to_owned(),
                        });
                    }
                    end = index + c.len_utf8();
                    chars.next();
                }
                TokenKind::Text(input[start..end].to_owned())
            }
        };
        tokens.push(Token {
            line: token_line,
            kind,
            quoted: c == '"',
        });
    }
    Ok(tokens)
}

fn parse_response(args: &[Token], line: usize) -> Result<Handler, ConfigError> {
    let error = |message: &str| ConfigError {
        line,
        message: message.to_owned(),
    };
    if args.is_empty() || args.len() > 2 {
        return Err(error(
            "expected respond \"body\" [status] or respond <status>",
        ));
    }
    let first = args[0].text().unwrap_or_default();
    let status_only = args.len() == 1
        && !args[0].quoted
        && first.len() == 3
        && first.bytes().all(|c| c.is_ascii_digit());
    let (body, status) = if status_only {
        ("", Some(first))
    } else {
        (first, args.get(1).and_then(Token::text))
    };
    let status = match status {
        Some(text) => text
            .parse::<u16>()
            .ok()
            .filter(|code| text.len() == 3 && (200..=599).contains(code))
            .ok_or_else(|| error("respond status must be a final HTTP status from 200 to 599"))?,
        None => 200,
    };
    if matches!(status, 204 | 205 | 304) && !body.is_empty() {
        return Err(error("status 204, 205, or 304 cannot have a response body"));
    }
    Ok(Handler::Respond {
        body: body.to_owned(),
        status,
    })
}

fn valid_domain(domain: &str, https: bool) -> bool {
    if domain.parse::<IpAddr>().is_ok() {
        return !https;
    }
    if domain.len() > 253 || domain.is_empty() {
        return false;
    }
    if https
        && (!domain.contains('.')
            || [".localhost", ".local", ".internal", ".home.arpa"]
                .iter()
                .any(|s| domain.ends_with(s)))
    {
        return false;
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    })
}

fn parse_upstream(value: &str) -> Result<Upstream, String> {
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let url = Url::parse(&value).map_err(|e| format!("invalid backend address: {e}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("backend must be an HTTP(S) host and port without credentials, path, query, or fragment".to_owned());
    }
    let host = url.host().ok_or("backend has no host")?.to_string();
    let port = url.port_or_known_default().ok_or("backend has no port")?;
    if port == 0 {
        return Err("backend port must be nonzero".to_owned());
    }
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    Ok(Upstream {
        host: host.trim_matches(['[', ']']).to_owned(),
        port,
        tls: url.scheme() == "https",
        authority,
    })
}

pub fn request_domain(authority: &str) -> Option<String> {
    let authority = authority.parse::<http::uri::Authority>().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    Some(
        authority
            .host()
            .trim_matches(['[', ']'])
            .trim_end_matches('.')
            .to_ascii_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_sites_and_explicit_http() -> anyhow::Result<()> {
        let config = Config::parse(
            "# comment\nExample.com { reverse_proxy localhost:3000 }\nhttp://localhost { reverse_proxy https://upstream.example:8443 }",
        )?;
        assert_eq!(config.https_domains().collect::<Vec<_>>(), ["example.com"]);
        let Handler::Proxy(backend) = &config.sites["localhost"].handler else {
            anyhow::bail!("expected proxy");
        };
        assert!(backend.tls);
        assert_eq!(backend.port, 8443);
        assert_eq!(backend.authority, "upstream.example:8443");
        Ok(())
    }

    #[test]
    fn refuses_ambiguous_or_unsupported_config() {
        for input in [
            "",
            "example.com {}",
            "example.com { reverse_proxy :3000 }",
            "*.example.com { reverse_proxy localhost:3000 }",
            "localhost { reverse_proxy localhost:3000 }",
            "example.com:443 { reverse_proxy localhost:3000 }",
            "example.com { reverse_proxy http://u:p@localhost }",
            "example.com { reverse_proxy localhost:3000/api }",
            "example.com { reverse_proxy localhost:3000 } EXAMPLE.COM { reverse_proxy localhost:4000 }",
            "example.com { reverse_proxy localhost:3000 typo yes }",
        ] {
            assert!(Config::parse(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn responses_preserve_quoted_text_and_escapes() -> anyhow::Result<()> {
        let config = Config::parse(
            r#"
            example.com {
                respond "Hello # {world}! \"quoted\" \\ café\nnext" 201 # comment
            }
        "#,
        )?;
        let Handler::Respond { body, status } = &config.sites["example.com"].handler else {
            anyhow::bail!("expected response");
        };
        assert_eq!(body, "Hello # {world}! \"quoted\" \\ café\nnext");
        assert_eq!(*status, 201);
        assert_eq!(config.https_domains().collect::<Vec<_>>(), ["example.com"]);
        Ok(())
    }

    #[test]
    fn response_defaults_and_empty_status_responses() -> anyhow::Result<()> {
        for (directive, expected_body, expected_status) in [
            (r#"respond "Hello world!""#, "Hello world!", 200),
            (r#"respond "404""#, "404", 200),
            (r#"respond """#, "", 200),
            ("respond 204", "", 204),
            ("respond 205", "", 205),
            ("respond 304", "", 304),
            (r#"respond "Unavailable" 503"#, "Unavailable", 503),
        ] {
            let config = Config::parse(&format!("http://localhost {{ {directive} }}"))?;
            let Handler::Respond { body, status } = &config.sites["localhost"].handler else {
                anyhow::bail!("expected response");
            };
            assert_eq!(body, expected_body);
            assert_eq!(*status, expected_status);
        }
        Ok(())
    }

    #[test]
    fn malformed_responses_fail_with_line_numbers() {
        for directive in [
            r#"respond "unclosed"#,
            r#"respond "bad\q""#,
            r#"respond "text"suffix"#,
            "respond",
            "respond hello world",
            "respond 103",
            "respond hello 600",
            "respond hello 204",
            "respond hello 205",
            "respond hello 304",
            "respond hello 200 reverse_proxy localhost:3000",
            r#"respond "}" } trailing"#,
        ] {
            let input = format!("# comment\nexample.com {{ {directive} }}");
            let error = Config::parse(&input).expect_err("invalid response was accepted");
            assert_eq!(error.line, 2, "{input}: {error}");
        }
    }

    #[test]
    fn ipv6_backend_and_host_normalization() -> anyhow::Result<()> {
        let upstream = parse_upstream("http://[::1]:3000").map_err(anyhow::Error::msg)?;
        assert_eq!(upstream.host, "::1");
        assert_eq!(upstream.authority, "[::1]:3000");
        assert_eq!(
            request_domain("EXAMPLE.COM.:443").as_deref(),
            Some("example.com")
        );
        assert!(request_domain("evil@example.com").is_none());
        Ok(())
    }
}
