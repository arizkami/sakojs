// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufReader, Cursor, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

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
    parse_request_head_with(source, limits, Vec::new())
}

/// Parses a request head, reusing `headers` as the range storage so a busy
/// connection does not allocate a fresh header vector per request.
fn parse_request_head_with(
    source: &[u8],
    limits: ParserLimits,
    mut headers: Vec<HeaderRange>,
) -> Result<ParsedRequest<'_>, ParseError> {
    headers.clear();
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

    headers.reserve(limits.maximum_headers.min(16));
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
    let mut output = Vec::with_capacity(128_usize.saturating_add(body.len()));
    encode_response_into(
        &mut output,
        status,
        reason,
        headers.iter().map(|(name, value)| (*name, *value)),
        body,
        maximum_bytes,
    )?;
    Ok(output)
}

/// Appends an encoded response to `output`, which lets a connection reuse one
/// response buffer instead of allocating per request. Nothing is appended when
/// the response is rejected.
fn encode_response_into<'a, H>(
    output: &mut Vec<u8>,
    status: u16,
    reason: &str,
    headers: H,
    body: &[u8],
    maximum_bytes: usize,
) -> Result<(), ParseError>
where
    H: Iterator<Item = (&'a str, &'a str)> + Clone,
{
    if !(100..=999).contains(&status)
        || reason.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        || headers.clone().any(|(name, value)| {
            name.is_empty()
                || !name.bytes().all(is_token)
                || value.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
                || name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
        })
    {
        return Err(ParseError::InvalidHeader);
    }
    let start = output.len();
    output.extend_from_slice(b"HTTP/1.1 ");
    write_integer(output, status as u64);
    output.push(b' ');
    output.extend_from_slice(reason.as_bytes());
    output.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(b": ");
        output.extend_from_slice(value.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    output.extend_from_slice(b"Content-Length: ");
    write_integer(output, body.len() as u64);
    output.extend_from_slice(b"\r\n\r\n");
    output.extend_from_slice(body);
    if output.len() - start > maximum_bytes {
        output.truncate(start);
        return Err(ParseError::HeadTooLarge);
    }
    Ok(())
}

fn write_integer(output: &mut Vec<u8>, value: u64) {
    let mut digits = [0_u8; 20];
    let mut cursor = digits.len();
    let mut remaining = value;
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    output.extend_from_slice(&digits[cursor..]);
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
    consumed: usize,
    output: Vec<u8>,
    written: usize,
    wire_output: Vec<u8>,
    wire_written: usize,
    tls: Option<ServerConnection>,
    tls_close_notify_sent: bool,
    close_after_write: bool,
}

/// Per-server work counters. Every field is a plain count kept on paths that
/// already run per tick or per request, so reading them costs nothing and
/// keeping them costs one increment. `sako --memory-stats` prints them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HttpServerCounters {
    /// Calls into `tick_with_body`.
    pub ticks: u64,
    /// Ticks that dispatched no request at all.
    pub idle_ticks: u64,
    /// Requests handed to the handler.
    pub requests: u64,
    /// Connections examined across all ticks. Divided by `ticks` this is the
    /// average per-tick walk, and divided by `requests` it is how many
    /// connections were touched to deliver one request.
    pub connection_visits: u64,
    /// Connections that had buffered input or an unflushed response when
    /// visited, so the visit did real work.
    pub connection_visits_with_work: u64,
    /// Overlapped receives handed to the kernel.
    pub receives_submitted: u64,
    /// Overlapped sends handed to the kernel.
    pub sends_submitted: u64,
    /// Completions dequeued from the port.
    pub completions: u64,
    /// GetQueuedCompletionStatusEx calls that returned at least one entry.
    pub completion_dequeues: u64,
}

pub struct HttpServer {
    tcp: TcpAcceptor,
    connections: BTreeMap<ConnectionId, HttpConnection>,
    config: HttpServerConfig,
    tls: Option<Arc<ServerConfig>>,
    /// Scratch state reused across ticks so a busy server allocates nothing per
    /// request: the connection identifiers to service, the ones to close, the
    /// TLS plaintext staging buffer, and the request parser's header ranges.
    active: Vec<ConnectionId>,
    closing: Vec<ConnectionId>,
    tls_scratch: Vec<u8>,
    header_scratch: Vec<HeaderRange>,
    counters: HttpServerCounters,
}

/// Requests dispatched for one connection in a single tick before the server
/// moves on, so a pipelining client cannot starve its peers.
const MAXIMUM_REQUESTS_PER_TICK: usize = 64;

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
            active: Vec::new(),
            closing: Vec::new(),
            tls_scratch: Vec::new(),
            header_scratch: Vec::new(),
            counters: HttpServerCounters::default(),
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

    /// Work counters for this server, including the transport's own.
    pub fn counters(&self) -> HttpServerCounters {
        let transport = self.tcp.counters();
        HttpServerCounters {
            receives_submitted: transport.receives_submitted,
            sends_submitted: transport.sends_submitted,
            completions: transport.completions,
            completion_dequeues: transport.completion_dequeues,
            ..self.counters
        }
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

    /// Blocks for up to `timeout` waiting for socket activity. The event loop
    /// calls this instead of sleeping so a request that arrives mid-wait wakes
    /// the runtime immediately rather than at the next timer tick.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<usize> {
        self.tcp.poll_io(timeout)
    }

    pub fn tick_with_body<F>(&mut self, mut handler: F) -> io::Result<usize>
    where
        F: FnMut(&RequestHead<'_>, &[u8]) -> HttpResponse,
    {
        self.tcp.poll_io(Duration::ZERO)?;
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

        let config = self.config;
        self.counters.ticks += 1;
        let mut handled = 0;
        let mut closing = std::mem::take(&mut self.closing);
        let mut active = std::mem::take(&mut self.active);
        let mut headers = std::mem::take(&mut self.header_scratch);
        let mut scratch = std::mem::take(&mut self.tls_scratch);
        closing.clear();
        active.clear();
        active.extend(self.connections.keys().copied());

        self.counters.connection_visits += active.len() as u64;
        for id in active.iter().copied() {
            let Some(connection) = self.connections.get_mut(&id) else {
                continue;
            };
            if connection.consumed < connection.input.len()
                || connection.written < connection.output.len()
            {
                self.counters.connection_visits_with_work += 1;
            }
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
                drain_tls_output(connection, config.maximum_response_bytes)?;
            }
            if connection.tls.is_some()
                && connection.close_after_write
                && !connection.tls_close_notify_sent
                && connection.output.is_empty()
            {
                connection.tls.as_mut().unwrap().send_close_notify();
                connection.tls_close_notify_sent = true;
                drain_tls_output(connection, config.maximum_response_bytes)?;
            }
            if !flush_connection(&mut self.tcp, id, connection, &mut closing) {
                continue;
            }
            if connection.close_after_write
                && connection.output.is_empty()
                && connection.wire_output.is_empty()
            {
                closing.push(id);
                continue;
            }

            let maximum_input_bytes = config
                .parser
                .maximum_head_bytes
                .saturating_add(config.maximum_request_body_bytes);
            let buffered = connection.input.len() - connection.consumed;
            let remaining = maximum_input_bytes.saturating_sub(buffered);
            if remaining != 0 {
                if let Some(tls) = connection.tls.as_mut() {
                    if scratch.len() < config.read_chunk_bytes {
                        scratch.resize(config.read_chunk_bytes, 0);
                    }
                    match self.tcp.read(id, &mut scratch[..config.read_chunk_bytes]) {
                        Ok(0) => {
                            closing.push(id);
                            continue;
                        }
                        Ok(bytes) => {
                            tls.read_tls(&mut Cursor::new(&scratch[..bytes]))?;
                            tls.process_new_packets().map_err(|error| {
                                io::Error::new(io::ErrorKind::InvalidData, error)
                            })?;
                            let mut plaintext = [0_u8; 16 * 1024];
                            loop {
                                match tls.reader().read(&mut plaintext) {
                                    Ok(0) => break,
                                    Ok(count) => {
                                        if connection.input.len().saturating_add(count)
                                            > maximum_input_bytes
                                                .saturating_add(connection.consumed)
                                        {
                                            closing.push(id);
                                            break;
                                        }
                                        connection.input.extend_from_slice(&plaintext[..count]);
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                        break;
                                    }
                                    Err(error) => return Err(error),
                                }
                            }
                            drain_tls_output(connection, config.maximum_response_bytes)?;
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            closing.push(id);
                            continue;
                        }
                    }
                } else {
                    match self.tcp.read_into(
                        id,
                        &mut connection.input,
                        remaining,
                        remaining.min(config.read_chunk_bytes),
                    ) {
                        Ok(0) => {
                            closing.push(id);
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            closing.push(id);
                            continue;
                        }
                    }
                }
            }

            // Pipelined requests are answered into one buffer, so the loop also
            // stops once that buffer reaches the response limit; the queued
            // bytes have to reach the socket before more work is taken on.
            let mut dispatched = 0;
            while dispatched < MAXIMUM_REQUESTS_PER_TICK
                && !connection.close_after_write
                && connection.output.len() < config.maximum_response_bytes
            {
                let pending = &connection.input[connection.consumed..];
                let parsed = match parse_request_head_with(pending, config.parser, headers) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        headers = Vec::new();
                        if error == ParseError::Incomplete {
                            break;
                        }
                        let (status, reason, body) = match error {
                            ParseError::HeadTooLarge | ParseError::TooManyHeaders => (
                                431,
                                "Request Header Fields Too Large",
                                &b"request head too large"[..],
                            ),
                            _ => (400, "Bad Request", &b"bad request"[..]),
                        };
                        encode_response_into(
                            &mut connection.output,
                            status,
                            reason,
                            [("Connection", "close")].into_iter(),
                            body,
                            config.maximum_response_bytes,
                        )
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        connection.close_after_write = true;
                        break;
                    }
                };

                let keep_alive = parsed.head.keep_alive();
                let unsupported_encoding = parsed.head.header(b"transfer-encoding").is_some();
                let content_length = match request_content_length(&parsed.head) {
                    Ok(length) => length,
                    Err(_) => {
                        headers = parsed.head.headers;
                        encode_response_into(
                            &mut connection.output,
                            400,
                            "Bad Request",
                            [("Connection", "close")].into_iter(),
                            b"invalid content length",
                            config.maximum_response_bytes,
                        )
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        connection.close_after_write = true;
                        break;
                    }
                };
                if content_length > config.maximum_request_body_bytes {
                    headers = parsed.head.headers;
                    encode_response_into(
                        &mut connection.output,
                        413,
                        "Payload Too Large",
                        [("Connection", "close")].into_iter(),
                        b"request body too large",
                        config.maximum_response_bytes,
                    )
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    connection.close_after_write = true;
                    break;
                }
                let request_bytes = parsed.consumed.saturating_add(content_length);
                if pending.len() < request_bytes {
                    headers = parsed.head.headers;
                    break;
                }
                let response = if unsupported_encoding {
                    HttpResponse {
                        status: 501,
                        reason: "Not Implemented".into(),
                        headers: Vec::new(),
                        body: b"transfer encoding is not implemented".to_vec(),
                    }
                } else {
                    handler(&parsed.head, &pending[parsed.consumed..request_bytes])
                };
                headers = parsed.head.headers;
                let close_after_write = !keep_alive || unsupported_encoding;
                let close_header = close_after_write
                    && !response
                        .headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case("connection"));
                let encoded = encode_response_into(
                    &mut connection.output,
                    response.status,
                    &response.reason,
                    response
                        .headers
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str()))
                        .chain(close_header.then_some(("Connection", "close"))),
                    &response.body,
                    config.maximum_response_bytes,
                );
                encoded.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                connection.consumed += request_bytes;
                connection.close_after_write = close_after_write;
                handled += 1;
                dispatched += 1;
            }

            if connection.consumed == connection.input.len() {
                connection.input.clear();
                connection.consumed = 0;
            } else if connection.consumed >= config.read_chunk_bytes {
                connection.input.drain(..connection.consumed);
                connection.consumed = 0;
            }

            if dispatched != 0 && connection.tls.is_none() {
                flush_connection(&mut self.tcp, id, connection, &mut closing);
            }
        }

        self.active = active;
        self.header_scratch = headers;
        self.tls_scratch = scratch;
        closing.sort_unstable();
        closing.dedup();
        for id in closing.drain(..) {
            self.connections.remove(&id);
            self.tcp.close(id);
        }
        self.closing = closing;
        self.counters.requests += handled as u64;
        if handled == 0 {
            self.counters.idle_ticks += 1;
        }
        Ok(handled)
    }
}

/// Writes as much of a connection's queued output as the socket accepts,
/// returning whether the queue drained completely.
fn flush_connection(
    tcp: &mut TcpAcceptor,
    id: ConnectionId,
    connection: &mut HttpConnection,
    closing: &mut Vec<ConnectionId>,
) -> bool {
    if connection.tls.is_some() {
        if connection.wire_written < connection.wire_output.len() {
            match tcp.write(id, &connection.wire_output[connection.wire_written..]) {
                Ok(0) => closing.push(id),
                Ok(bytes) => connection.wire_written += bytes,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => closing.push(id),
            }
            if connection.wire_written < connection.wire_output.len() {
                return false;
            }
            connection.wire_output.clear();
            connection.wire_written = 0;
        }
        return true;
    }
    if connection.written < connection.output.len() {
        match tcp.write(id, &connection.output[connection.written..]) {
            Ok(0) => closing.push(id),
            Ok(bytes) => connection.written += bytes,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => closing.push(id),
        }
        if connection.written < connection.output.len() {
            return false;
        }
        connection.output.clear();
        connection.written = 0;
    }
    true
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
