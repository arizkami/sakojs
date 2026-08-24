// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufReader, Cursor, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Range;
use std::sync::Arc;

use rustls::{ServerConfig, ServerConnection};
use sako_net::{ConnectionId, TcpAcceptor, TcpServerConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version {
    Http10,
    Http11,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HeaderRange {
    name: Range<usize>,
    value: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct RequestHead<'a> {
    source: &'a [u8],
    method: Range<usize>,
    target: Range<usize>,
    version: Version,
    headers: Vec<HeaderRange>,
}

impl<'a> RequestHead<'a> {
    pub fn method(&self) -> &'a [u8] {
        &self.source[self.method.clone()]
    }

    pub fn target(&self) -> &'a [u8] {
        &self.source[self.target.clone()]
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn header_count(&self) -> usize {
        self.headers.len()
    }

    pub fn headers(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + '_ {
        self.headers.iter().map(|header| {
            (
                &self.source[header.name.clone()],
                &self.source[header.value.clone()],
            )
        })
    }

    pub fn header(&self, name: &[u8]) -> Option<&'a [u8]> {
        self.headers()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    pub fn keep_alive(&self) -> bool {
        match self.header(b"connection") {
            Some(value) if value.eq_ignore_ascii_case(b"close") => false,
            Some(value) if value.eq_ignore_ascii_case(b"keep-alive") => true,
            _ => self.version == Version::Http11,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParserLimits {
    pub maximum_head_bytes: usize,
    pub maximum_headers: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            maximum_head_bytes: 64 * 1024,
            maximum_headers: 128,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParsedRequest<'a> {
    pub head: RequestHead<'a>,
    pub consumed: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    Incomplete,
    HeadTooLarge,
    TooManyHeaders,
    InvalidRequestLine,
    InvalidMethod,
    InvalidTarget,
    UnsupportedVersion,
    InvalidHeader,
    InvalidContentLength,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Incomplete => "HTTP request head is incomplete",
            Self::HeadTooLarge => "HTTP request head exceeds byte limit",
            Self::TooManyHeaders => "HTTP request has too many headers",
            Self::InvalidRequestLine => "HTTP request line is invalid",
            Self::InvalidMethod => "HTTP method is invalid",
            Self::InvalidTarget => "HTTP request target is invalid",
            Self::UnsupportedVersion => "HTTP version is unsupported",
            Self::InvalidHeader => "HTTP header is invalid",
            Self::InvalidContentLength => "HTTP Content-Length is invalid",
        })
    }
}

impl std::error::Error for ParseError {}

pub fn parse_request_head(
    source: &[u8],
    limits: ParserLimits,
) -> Result<ParsedRequest<'_>, ParseError> {
    let head_end = find_sequence(source, b"\r\n\r\n").ok_or({
        if source.len() >= limits.maximum_head_bytes {
            ParseError::HeadTooLarge
        } else {
            ParseError::Incomplete
        }
    })?;
    let consumed = head_end + 4;
    if consumed > limits.maximum_head_bytes {
        return Err(ParseError::HeadTooLarge);
    }

    let request_line_end =
        find_sequence(&source[..head_end + 2], b"\r\n").ok_or(ParseError::InvalidRequestLine)?;
    let request_line = &source[..request_line_end];
    let first_space = request_line
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(ParseError::InvalidRequestLine)?;
    let second_space = request_line[first_space + 1..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|index| first_space + 1 + index)
        .ok_or(ParseError::InvalidRequestLine)?;
    if request_line[second_space + 1..].contains(&b' ') {
        return Err(ParseError::InvalidRequestLine);
    }
    if first_space == 0
        || !request_line[..first_space]
            .iter()
            .all(|byte| is_token(*byte))
    {
        return Err(ParseError::InvalidMethod);
    }
    if second_space == first_space + 1
        || request_line[first_space + 1..second_space]
            .iter()
            .any(|byte| *byte <= 0x20 || *byte == 0x7f)
    {
        return Err(ParseError::InvalidTarget);
    }
    let version = match &request_line[second_space + 1..] {
        b"HTTP/1.0" => Version::Http10,
        b"HTTP/1.1" => Version::Http11,
        _ => return Err(ParseError::UnsupportedVersion),
    };

    let mut headers = Vec::with_capacity(limits.maximum_headers.min(16));
    let mut cursor = request_line_end + 2;
    while cursor < head_end {
        if headers.len() >= limits.maximum_headers {
            return Err(ParseError::TooManyHeaders);
        }
        let relative_end = find_sequence(&source[cursor..head_end + 2], b"\r\n")
            .ok_or(ParseError::InvalidHeader)?;
        let line_end = cursor + relative_end;
        let line = &source[cursor..line_end];
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(ParseError::InvalidHeader)?;
        if colon == 0 || !line[..colon].iter().all(|byte| is_token(*byte)) {
            return Err(ParseError::InvalidHeader);
        }
        let mut value_start = cursor + colon + 1;
        while value_start < line_end && matches!(source[value_start], b' ' | b'\t') {
            value_start += 1;
        }
        let mut value_end = line_end;
        while value_end > value_start && matches!(source[value_end - 1], b' ' | b'\t') {
            value_end -= 1;
        }
        if source[value_start..value_end]
            .iter()
            .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
        {
            return Err(ParseError::InvalidHeader);
        }
        headers.push(HeaderRange {
            name: cursor..cursor + colon,
            value: value_start..value_end,
        });
        cursor = line_end + 2;
    }

    Ok(ParsedRequest {
        head: RequestHead {
            source,
            method: 0..first_space,
            target: first_space + 1..second_space,
            version,
            headers,
        },
        consumed,
    })
}

fn find_sequence(source: &[u8], needle: &[u8]) -> Option<usize> {
    sako_accel::find_sequence(source, needle)
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub fn encode_response(
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    maximum_bytes: usize,
) -> Result<Vec<u8>, ParseError> {
    if !(100..=999).contains(&status)
        || reason.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        || headers.iter().any(|(name, value)| {
            name.is_empty()
                || !name.bytes().all(is_token)
                || value.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
                || name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
        })
    {
        return Err(ParseError::InvalidHeader);
    }
    let mut output = Vec::with_capacity(128_usize.saturating_add(body.len()));
    output.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());
    for (name, value) in headers {
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(b": ");
        output.extend_from_slice(value.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    output.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    output.extend_from_slice(body);
    if output.len() > maximum_bytes {
        return Err(ParseError::HeadTooLarge);
    }
    Ok(output)
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            reason: "OK".into(),
            headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
            body: body.into().into_bytes(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpServerConfig {
    pub tcp: TcpServerConfig,
    pub parser: ParserLimits,
    pub read_chunk_bytes: usize,
    pub maximum_response_bytes: usize,
    pub maximum_request_body_bytes: usize,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            tcp: TcpServerConfig::default(),
            parser: ParserLimits::default(),
            read_chunk_bytes: 16 * 1024,
            maximum_response_bytes: 16 * 1024 * 1024,
            maximum_request_body_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
struct HttpConnection {
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    wire_output: Vec<u8>,
    wire_written: usize,
    tls: Option<ServerConnection>,
    tls_close_notify_sent: bool,
    close_after_write: bool,
}

pub struct HttpServer {
    tcp: TcpAcceptor,
    connections: BTreeMap<ConnectionId, HttpConnection>,
    config: HttpServerConfig,
    tls: Option<Arc<ServerConfig>>,
}

impl HttpServer {
    pub fn bind(address: impl ToSocketAddrs, config: HttpServerConfig) -> io::Result<Self> {
        if config.read_chunk_bytes == 0
            || config.parser.maximum_head_bytes == 0
            || config.maximum_response_bytes == 0
            || config.maximum_request_body_bytes == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP byte limits must be positive",
            ));
        }
        let tcp = TcpAcceptor::bind(address, config.tcp)?;
        Ok(Self {
            tcp,
            connections: BTreeMap::new(),
            config,
            tls: None,
        })
    }

    pub fn bind_tls(
        address: impl ToSocketAddrs,
        config: HttpServerConfig,
        certificate_pem: &[u8],
        private_key_pem: &[u8],
    ) -> io::Result<Self> {
        let mut certificates = BufReader::new(certificate_pem);
        let certificates = rustls_pemfile::certs(&mut certificates)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if certificates.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TLS certificate chain is empty",
            ));
        }
        let mut private_key = BufReader::new(private_key_pem);
        let private_key = rustls_pemfile::private_key(&mut private_key)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "TLS private key is empty")
            })?;
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut server = Self::bind(address, config)?;
        server.tls = Some(Arc::new(tls));
        Ok(server)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    pub fn connection_count(&self) -> usize {
        self.tcp.connection_count()
    }

    pub fn rejected_connections(&self) -> u64 {
        self.tcp.rejected_connections()
    }

    pub fn close(&mut self) {
        self.tcp.stop_accepting();
        for connection in self.connections.values_mut() {
            connection.close_after_write = true;
        }
    }

    pub fn tick<F>(&mut self, mut handler: F) -> io::Result<usize>
    where
        F: FnMut(&RequestHead<'_>) -> HttpResponse,
    {
        self.tick_with_body(|request, _body| handler(request))
    }

    pub fn tick_with_body<F>(&mut self, mut handler: F) -> io::Result<usize>
    where
        F: FnMut(&RequestHead<'_>, &[u8]) -> HttpResponse,
    {
        for id in self.tcp.accept_ready()? {
            let tls = self
                .tls
                .as_ref()
                .map(|config| {
                    let mut connection = ServerConnection::new(config.clone())
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    connection.set_buffer_limit(Some(self.config.maximum_response_bytes));
                    Ok::<_, io::Error>(connection)
                })
                .transpose()?;
            self.connections.insert(
                id,
                HttpConnection {
                    input: Vec::with_capacity(self.config.parser.maximum_head_bytes.min(4096)),
                    tls,
                    ..HttpConnection::default()
                },
            );
        }

        let mut handled = 0;
        let mut closing = Vec::new();
        let ids = self.connections.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let Some(connection) = self.connections.get_mut(&id) else {
                continue;
            };
            if connection.tls.is_some()
                && connection.wire_output.is_empty()
                && !connection.output.is_empty()
            {
                connection
                    .tls
                    .as_mut()
                    .unwrap()
                    .writer()
                    .write_all(&connection.output)?;
                connection.output.clear();
                connection.written = 0;
                drain_tls_output(connection, self.config.maximum_response_bytes)?;
            }
            if connection.tls.is_some()
                && connection.close_after_write
                && !connection.tls_close_notify_sent
                && connection.output.is_empty()
            {
                connection.tls.as_mut().unwrap().send_close_notify();
                connection.tls_close_notify_sent = true;
                drain_tls_output(connection, self.config.maximum_response_bytes)?;
            }
            if connection.tls.is_some() {
                if connection.wire_written < connection.wire_output.len() {
                    match self
                        .tcp
                        .write(id, &connection.wire_output[connection.wire_written..])
                    {
                        Ok(0) => closing.push(id),
                        Ok(bytes) => connection.wire_written += bytes,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => closing.push(id),
                    }
                    if connection.wire_written < connection.wire_output.len() {
                        continue;
                    }
                    connection.wire_output.clear();
                    connection.wire_written = 0;
                }
            } else if connection.written < connection.output.len() {
                match self.tcp.write(id, &connection.output[connection.written..]) {
                    Ok(0) => closing.push(id),
                    Ok(bytes) => connection.written += bytes,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => closing.push(id),
                }
                if connection.written < connection.output.len() {
                    continue;
                }
                connection.output.clear();
                connection.written = 0;
            }
            if connection.close_after_write
                && connection.output.is_empty()
                && connection.wire_output.is_empty()
            {
                closing.push(id);
                continue;
            }

            let maximum_input_bytes = self
                .config
                .parser
                .maximum_head_bytes
                .saturating_add(self.config.maximum_request_body_bytes);
            let remaining = maximum_input_bytes.saturating_sub(connection.input.len());
            if remaining != 0 {
                let read_length = if connection.tls.is_some() {
                    self.config.read_chunk_bytes
                } else {
                    remaining.min(self.config.read_chunk_bytes)
                };
                let mut buffer = vec![0_u8; read_length];
                match self.tcp.read(id, &mut buffer) {
                    Ok(0) => {
                        closing.push(id);
                        continue;
                    }
                    Ok(bytes) if connection.tls.is_some() => {
                        let tls = connection.tls.as_mut().unwrap();
                        tls.read_tls(&mut Cursor::new(&buffer[..bytes]))?;
                        tls.process_new_packets()
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        let mut plaintext = [0_u8; 16 * 1024];
                        loop {
                            match tls.reader().read(&mut plaintext) {
                                Ok(0) => break,
                                Ok(count) => {
                                    if connection.input.len().saturating_add(count)
                                        > maximum_input_bytes
                                    {
                                        closing.push(id);
                                        break;
                                    }
                                    connection.input.extend_from_slice(&plaintext[..count]);
                                }
                                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                                Err(error) => return Err(error),
                            }
                        }
                        drain_tls_output(connection, self.config.maximum_response_bytes)?;
                    }
                    Ok(bytes) => connection.input.extend_from_slice(&buffer[..bytes]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        closing.push(id);
                        continue;
                    }
                }
            }

            match parse_request_head(&connection.input, self.config.parser) {
                Ok(parsed) => {
                    let keep_alive = parsed.head.keep_alive();
                    let unsupported_encoding = parsed.head.header(b"transfer-encoding").is_some();
                    let content_length = match request_content_length(&parsed.head) {
                        Ok(length) => length,
                        Err(_) => {
                            connection.output = encode_response(
                                400,
                                "Bad Request",
                                &[("Connection", "close")],
                                b"invalid content length",
                                self.config.maximum_response_bytes,
                            )
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                            connection.close_after_write = true;
                            continue;
                        }
                    };
                    if content_length > self.config.maximum_request_body_bytes {
                        connection.output = encode_response(
                            413,
                            "Payload Too Large",
                            &[("Connection", "close")],
                            b"request body too large",
                            self.config.maximum_response_bytes,
                        )
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        connection.close_after_write = true;
                        continue;
                    }
                    let request_bytes = parsed.consumed.saturating_add(content_length);
                    if connection.input.len() < request_bytes {
                        continue;
                    }
                    let response = if unsupported_encoding {
                        HttpResponse {
                            status: 501,
                            reason: "Not Implemented".into(),
                            headers: Vec::new(),
                            body: b"transfer encoding is not implemented".to_vec(),
                        }
                    } else {
                        handler(
                            &parsed.head,
                            &connection.input[parsed.consumed..request_bytes],
                        )
                    };
                    let close_after_write = !keep_alive || unsupported_encoding;
                    let mut response_headers = response.headers;
                    if close_after_write
                        && !response_headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("connection"))
                    {
                        response_headers.push(("Connection".into(), "close".into()));
                    }
                    let headers = response_headers
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str()))
                        .collect::<Vec<_>>();
                    connection.output = encode_response(
                        response.status,
                        &response.reason,
                        &headers,
                        &response.body,
                        self.config.maximum_response_bytes,
                    )
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    connection.input.drain(..request_bytes);
                    connection.close_after_write = close_after_write;
                    handled += 1;
                }
                Err(ParseError::Incomplete) => {}
                Err(error) => {
                    let (status, reason, body) = match error {
                        ParseError::HeadTooLarge | ParseError::TooManyHeaders => (
                            431,
                            "Request Header Fields Too Large",
                            &b"request head too large"[..],
                        ),
                        _ => (400, "Bad Request", &b"bad request"[..]),
                    };
                    connection.output = encode_response(
                        status,
                        reason,
                        &[("Connection", "close")],
                        body,
                        self.config.maximum_response_bytes,
                    )
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    connection.close_after_write = true;
                }
            }
        }
        closing.sort_unstable();
        closing.dedup();
        for id in closing {
            self.connections.remove(&id);
            self.tcp.close(id);
        }
        Ok(handled)
    }
}

fn drain_tls_output(
    connection: &mut HttpConnection,
    maximum_response_bytes: usize,
) -> io::Result<()> {
    let maximum_wire_bytes = maximum_response_bytes.saturating_add(256 * 1024);
    let tls = connection.tls.as_mut().unwrap();
    while tls.wants_write() {
        let written = tls.write_tls(&mut connection.wire_output)?;
        if connection.wire_output.len() > maximum_wire_bytes {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "TLS output exceeds byte limit",
            ));
        }
        if written == 0 {
            break;
        }
    }
    Ok(())
}

fn request_content_length(request: &RequestHead<'_>) -> Result<usize, ParseError> {
    let mut parsed = None;
    for (_, value) in request
        .headers()
        .filter(|(name, _)| name.eq_ignore_ascii_case(b"content-length"))
    {
        if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
            return Err(ParseError::InvalidContentLength);
        }
        let text = std::str::from_utf8(value).map_err(|_| ParseError::InvalidContentLength)?;
        let length = text
            .parse::<usize>()
            .map_err(|_| ParseError::InvalidContentLength)?;
        if parsed.is_some_and(|previous| previous != length) {
            return Err(ParseError::InvalidContentLength);
        }
        parsed = Some(length);
    }
    Ok(parsed.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn parses_ranges_without_materializing_headers() {
        let source = b"GET /hello HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\nbody";
        let parsed = parse_request_head(source, ParserLimits::default()).unwrap();
        assert_eq!(parsed.consumed, source.len() - 4);
        assert_eq!(parsed.head.method(), b"GET");
        assert_eq!(parsed.head.target(), b"/hello");
        assert_eq!(parsed.head.header(b"HOST"), Some(&b"example.com"[..]));
        assert!(!parsed.head.keep_alive());
    }

    #[test]
    fn reports_partial_invalid_and_overloaded_inputs() {
        assert_eq!(
            parse_request_head(b"GET / HTTP/1.1\r\n", ParserLimits::default()).unwrap_err(),
            ParseError::Incomplete
        );
        assert_eq!(
            parse_request_head(
                b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\n\r\n",
                ParserLimits {
                    maximum_headers: 1,
                    ..ParserLimits::default()
                }
            )
            .unwrap_err(),
            ParseError::TooManyHeaders
        );
        assert_eq!(
            parse_request_head(b"GE(T / HTTP/1.1\r\n\r\n", ParserLimits::default()).unwrap_err(),
            ParseError::InvalidMethod
        );
    }

    #[test]
    fn encodes_a_bounded_response_and_rejects_header_injection() {
        let response =
            encode_response(200, "OK", &[("Content-Type", "text/plain")], b"pong", 1024).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(b"\r\n\r\npong"));
        assert!(encode_response(200, "OK", &[("X-Test", "bad\r\nvalue")], b"", 1024).is_err());
    }

    #[test]
    fn native_server_handles_partial_reads_and_keep_alive() {
        let mut server = HttpServer::bind(
            "127.0.0.1:0",
            HttpServerConfig {
                tcp: TcpServerConfig {
                    maximum_connections: 4,
                    maximum_accepts_per_tick: 4,
                },
                ..HttpServerConfig::default()
            },
        )
        .unwrap();
        let mut client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        client.set_nonblocking(true).unwrap();
        client
            .write_all(b"GET /one HTTP/1.1\r\nHost: local\r\n")
            .unwrap();
        server.tick(|_| unreachable!()).unwrap();
        client.write_all(b"\r\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut response = Vec::new();
        loop {
            server
                .tick(|request| {
                    HttpResponse::text(String::from_utf8_lossy(request.target()).into_owned())
                })
                .unwrap();
            let mut buffer = [0_u8; 1024];
            match client.read(&mut buffer) {
                Ok(bytes) if bytes != 0 => response.extend_from_slice(&buffer[..bytes]),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("client read failed: {error}"),
            }
            if response.ends_with(b"/one") {
                break;
            }
            assert!(Instant::now() < deadline, "HTTP response timed out");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(server.connection_count(), 1);

        let deadline = Instant::now() + Duration::from_secs(2);
        client
            .write_all(b"GET /two HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut second = Vec::new();
        while !second.ends_with(b"/two") {
            server
                .tick(|request| {
                    HttpResponse::text(String::from_utf8_lossy(request.target()).into_owned())
                })
                .unwrap();
            let mut buffer = [0_u8; 1024];
            match client.read(&mut buffer) {
                Ok(bytes) if bytes != 0 => second.extend_from_slice(&buffer[..bytes]),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("client read failed: {error}"),
            }
            assert!(Instant::now() < deadline, "keep-alive response timed out");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(second.ends_with(b"/two"));
        while server.connection_count() != 0 {
            server.tick(|_| unreachable!()).unwrap();
            assert!(Instant::now() < deadline, "connection close timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn rejects_exhausted_heads_and_duplicate_response_framing() {
        let limit = b"GET / HTTP/1.1\r\n".len();
        assert_eq!(
            parse_request_head(
                b"GET / HTTP/1.1\r\n",
                ParserLimits {
                    maximum_head_bytes: limit,
                    maximum_headers: 8,
                },
            )
            .unwrap_err(),
            ParseError::HeadTooLarge
        );
        assert!(encode_response(200, "OK", &[("Content-Length", "99")], b"body", 1024,).is_err());
    }

    #[test]
    fn validates_duplicate_content_lengths() {
        let matching = parse_request_head(
            b"POST / HTTP/1.1\r\nContent-Length: 4\r\ncontent-length: 4\r\n\r\nbody",
            ParserLimits::default(),
        )
        .unwrap();
        assert_eq!(request_content_length(&matching.head), Ok(4));

        let conflicting = parse_request_head(
            b"POST / HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\n",
            ParserLimits::default(),
        )
        .unwrap();
        assert_eq!(
            request_content_length(&conflicting.head),
            Err(ParseError::InvalidContentLength)
        );

        let malformed = parse_request_head(
            b"POST / HTTP/1.1\r\nContent-Length: +4\r\n\r\n",
            ParserLimits::default(),
        )
        .unwrap();
        assert_eq!(
            request_content_length(&malformed.head),
            Err(ParseError::InvalidContentLength)
        );
    }

    #[test]
    fn waits_for_a_fragmented_body_before_dispatch() {
        let mut server = HttpServer::bind(
            "127.0.0.1:0",
            HttpServerConfig {
                maximum_request_body_bytes: 8,
                ..HttpServerConfig::default()
            },
        )
        .unwrap();
        let mut client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        client.set_nonblocking(true).unwrap();
        client
            .write_all(b"POST /body HTTP/1.1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nab")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut dispatched = 0;
        while server.connection_count() == 0 {
            server.tick_with_body(|_, _| unreachable!()).unwrap();
            assert!(Instant::now() < deadline, "HTTP accept timed out");
            thread::sleep(Duration::from_millis(1));
        }
        for _ in 0..4 {
            server
                .tick_with_body(|_, _| {
                    dispatched += 1;
                    HttpResponse::text("unexpected")
                })
                .unwrap();
        }
        assert_eq!(dispatched, 0);

        client.write_all(b"cd").unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"abcd") {
            server
                .tick_with_body(|_, body| {
                    dispatched += 1;
                    HttpResponse::text(String::from_utf8_lossy(body))
                })
                .unwrap();
            let mut buffer = [0_u8; 1024];
            match client.read(&mut buffer) {
                Ok(bytes) if bytes != 0 => response.extend_from_slice(&buffer[..bytes]),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("client read failed: {error}"),
            }
            assert!(Instant::now() < deadline, "HTTP body response timed out");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(dispatched, 1);
    }

    #[test]
    fn graceful_close_flushes_a_queued_response() {
        let mut server = HttpServer::bind("127.0.0.1:0", HttpServerConfig::default()).unwrap();
        let mut client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        client.set_nonblocking(true).unwrap();
        client
            .write_all(b"GET /close HTTP/1.1\r\nHost: local\r\n\r\n")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut handled = 0;
        while handled == 0 {
            handled += server
                .tick(|_| HttpResponse::text("flushed-before-close"))
                .unwrap();
            assert!(Instant::now() < deadline, "HTTP dispatch timed out");
            thread::sleep(Duration::from_millis(1));
        }
        server.close();

        let mut response = Vec::new();
        while !response.ends_with(b"flushed-before-close") || server.connection_count() != 0 {
            server.tick(|_| unreachable!()).unwrap();
            let mut buffer = [0_u8; 1024];
            match client.read(&mut buffer) {
                Ok(bytes) if bytes != 0 => response.extend_from_slice(&buffer[..bytes]),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("client read failed: {error}"),
            }
            assert!(Instant::now() < deadline, "graceful HTTP close timed out");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn tls_server_reuses_bounded_http_transport() {
        use rcgen::{CertifiedKey, generate_simple_self_signed};
        use rustls::pki_types::ServerName;
        use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
        use std::sync::Arc;

        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut server = HttpServer::bind_tls(
            "127.0.0.1:0",
            HttpServerConfig::default(),
            cert.pem().as_bytes(),
            signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        let address = server.local_addr().unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client_thread = thread::spawn(move || {
            let connection = ClientConnection::new(
                Arc::new(client_config),
                ServerName::try_from("localhost").unwrap(),
            )
            .unwrap();
            let socket = TcpStream::connect(address).unwrap();
            let mut client = StreamOwned::new(connection, socket);
            client
                .write_all(b"GET /secure HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            response
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut handled = 0;
        while handled == 0 || server.connection_count() != 0 {
            handled += server
                .tick(|_| HttpResponse::text("secure-response"))
                .unwrap();
            assert!(Instant::now() < deadline, "TLS server timed out");
            thread::sleep(Duration::from_millis(1));
        }
        drop(server);
        let response = client_thread.join().unwrap();
        assert!(response.ends_with(b"secure-response"));
    }
}
