// SPDX-License-Identifier: BSD-3-Clause

//! A PostgreSQL client that speaks the v3 wire protocol directly.
//!
//! One connection, one statement at a time, every parameter and every result
//! in the text format the server itself prints. That is a deliberate floor
//! rather than a first draft of something larger: the binary format is
//! per-type and versioned, and a driver that gets one of those wrong corrupts
//! a value silently instead of failing.
//!
//! Everything is bounded -- the message it will read, the rows it will hold,
//! the columns a result may have -- and everything blocks. A query occupies
//! the calling thread until the server answers, the same way
//! [`sako_process::spawn_native_with_bounded_output`] occupies it until a
//! child exits. Making it not block means giving it to the IOCP reactor, which
//! is the next piece of work and not this one.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::time::Duration;

mod auth;
mod protocol;

pub use protocol::{Column, ServerMessage};

use protocol::{Authentication, FieldReader, MessageWriter, backend};

/// The largest single protocol message this client will read. A server that
/// wants to send more than this is either misbehaving or returning a value no
/// caller asked to hold in memory at once.
pub const MAXIMUM_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// The most rows one query will accumulate before failing.
pub const MAXIMUM_ROWS: usize = 1_000_000;
/// The most columns a result may have.
pub const MAXIMUM_COLUMNS: usize = 1_600;
/// The most parameters one statement may bind. PostgreSQL's own limit.
pub const MAXIMUM_PARAMETERS: usize = 65_535;
/// The most bytes one result set may hold across every value in it.
pub const MAXIMUM_RESULT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum PostgresError {
    Io(io::Error),
    /// The server said no, and said why.
    Server(Box<ServerMessage>),
    /// What arrived was not something this protocol allows.
    Protocol(String),
    /// A real part of PostgreSQL this driver does not implement.
    Unsupported(String),
    /// A bound this driver enforces was reached.
    Limit(String),
    /// The connection string could not be read.
    Configuration(String),
}

impl fmt::Display for PostgresError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Server(message) => formatter.write_str(&message.describe()),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            Self::Limit(message) => write!(formatter, "limit reached: {message}"),
            Self::Configuration(message) => {
                write!(formatter, "invalid connection string: {message}")
            }
        }
    }
}

impl std::error::Error for PostgresError {}

impl From<io::Error> for PostgresError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl PostgresError {
    /// The SQLSTATE the server reported, when the failure came from the
    /// server. A caller retrying a serialization failure or reporting a unique
    /// violation needs the code rather than the sentence.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Server(message) => Some(&message.code),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub application_name: String,
    pub connect_timeout: Duration,
    /// How long to wait for a server that has stopped answering. It bounds one
    /// read, not one query, so a long result that keeps arriving is fine.
    pub query_timeout: Duration,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 5432,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            application_name: "sako".to_owned(),
            connect_timeout: Duration::from_secs(10),
            query_timeout: Duration::from_secs(60),
        }
    }
}

impl PostgresConfig {
    /// Reads a `postgres://user:password@host:port/database?option=value` URL.
    ///
    /// The user and password are percent-decoded, because a password with an
    /// `@` in it is otherwise unusable and silently truncating one is worse
    /// than refusing it.
    pub fn from_url(url: &str) -> Result<Self, PostgresError> {
        let rest = url
            .strip_prefix("postgresql://")
            .or_else(|| url.strip_prefix("postgres://"))
            .ok_or_else(|| {
                PostgresError::Configuration(
                    "a connection string starts with postgres:// or postgresql://".to_owned(),
                )
            })?;
        let mut config = Self::default();

        let (authority, path_and_query) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => (rest, ""),
        };
        let (credentials, endpoint) = match authority.rfind('@') {
            Some(index) => (&authority[..index], &authority[index + 1..]),
            None => ("", authority),
        };
        if !credentials.is_empty() {
            let (user, password) = match credentials.find(':') {
                Some(index) => (&credentials[..index], &credentials[index + 1..]),
                None => (credentials, ""),
            };
            config.user = percent_decode(user)?;
            config.password = percent_decode(password)?;
        }
        if !endpoint.is_empty() {
            // A bracketed IPv6 literal keeps its colons.
            let (host, port) = if let Some(end) = endpoint.strip_prefix('[') {
                match end.find(']') {
                    Some(index) => (&end[..index], end[index + 1..].strip_prefix(':')),
                    None => {
                        return Err(PostgresError::Configuration(
                            "an IPv6 host must be bracketed".to_owned(),
                        ));
                    }
                }
            } else {
                match endpoint.rfind(':') {
                    Some(index) => (&endpoint[..index], Some(&endpoint[index + 1..])),
                    None => (endpoint, None),
                }
            };
            if !host.is_empty() {
                config.host = percent_decode(host)?;
            }
            if let Some(port) = port.filter(|port| !port.is_empty()) {
                config.port = port.parse().map_err(|_| {
                    PostgresError::Configuration(format!("port is not a number: {port}"))
                })?;
            }
        }

        let (path, query) = match path_and_query.find('?') {
            Some(index) => (&path_and_query[..index], &path_and_query[index + 1..]),
            None => (path_and_query, ""),
        };
        if !path.is_empty() {
            config.database = percent_decode(path)?;
        }
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = match pair.find('=') {
                Some(index) => (&pair[..index], &pair[index + 1..]),
                None => (pair, ""),
            };
            let value = percent_decode(value)?;
            match name {
                "application_name" => config.application_name = value,
                "dbname" | "database" => config.database = value,
                "user" => config.user = value,
                "password" => config.password = value,
                "connect_timeout" => {
                    if let Ok(seconds) = value.parse() {
                        config.connect_timeout = Duration::from_secs(seconds);
                    }
                }
                "sslmode" | "ssl" if !matches!(value.as_str(), "disable" | "false" | "0") => {
                    return Err(PostgresError::Unsupported(
                        "TLS connections are not implemented; only sslmode=disable works"
                            .to_owned(),
                    ));
                }
                "sslmode" | "ssl" => {}
                _ => {}
            }
        }
        if config.user.is_empty() {
            return Err(PostgresError::Configuration(
                "a connection string needs a user".to_owned(),
            ));
        }
        if config.database.is_empty() {
            config.database = config.user.clone();
        }
        Ok(config)
    }
}

fn percent_decode(value: &str) -> Result<String, PostgresError> {
    if !value.contains('%') {
        return Ok(value.to_owned());
    }
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = bytes
                .get(index + 1..index + 3)
                .and_then(|digits| std::str::from_utf8(digits).ok())
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                .ok_or_else(|| {
                    PostgresError::Configuration("a percent escape is malformed".to_owned())
                })?;
            output.push(digits);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output)
        .map_err(|_| PostgresError::Configuration("a percent escape is not UTF-8".to_owned()))
}

/// One result set: the columns, the rows as they arrived, and what the command
/// reported about itself.
#[derive(Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    /// Row-major, one entry per column, `None` for SQL NULL. Values are the
    /// server's own text form.
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    pub command: String,
    pub affected_rows: u64,
}

/// A live connection to one server.
pub struct Connection {
    stream: TcpStream,
    writer: MessageWriter,
    incoming: Vec<u8>,
    parameters: BTreeMap<String, String>,
    process_id: i32,
    secret_key: i32,
    /// The last transaction status the server reported: `I` idle, `T` in a
    /// transaction, `E` in a failed transaction.
    transaction_status: u8,
    notices: Vec<ServerMessage>,
    closed: bool,
}

impl Connection {
    pub fn connect(config: &PostgresConfig) -> Result<Self, PostgresError> {
        let address = (config.host.as_str(), config.port)
            .to_socket_addrs()
            .map_err(PostgresError::Io)?
            .next()
            .ok_or_else(|| {
                PostgresError::Configuration(format!(
                    "no address for {}:{}",
                    config.host, config.port
                ))
            })?;
        let stream = TcpStream::connect_timeout(&address, config.connect_timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(config.query_timeout))?;
        stream.set_write_timeout(Some(config.query_timeout))?;

        let mut connection = Self {
            stream,
            writer: MessageWriter::new(),
            incoming: Vec::new(),
            parameters: BTreeMap::new(),
            process_id: 0,
            secret_key: 0,
            transaction_status: b'I',
            notices: Vec::new(),
            closed: false,
        };
        connection.start_up(config)?;
        Ok(connection)
    }

    fn start_up(&mut self, config: &PostgresConfig) -> Result<(), PostgresError> {
        self.writer.clear();
        self.writer.startup(&[
            ("user", &config.user),
            ("database", &config.database),
            ("application_name", &config.application_name),
            ("client_encoding", "UTF8"),
        ]);
        self.flush()?;

        let mut scram: Option<auth::Scram> = None;
        loop {
            let tag = self.read_message()?;
            match tag {
                backend::AUTHENTICATION => {
                    let body = std::mem::take(&mut self.incoming);
                    let request = protocol::parse_authentication(&body)?;
                    self.incoming = body;
                    match request {
                        Authentication::Ok => {}
                        Authentication::CleartextPassword => {
                            self.writer.clear();
                            self.writer.password(config.password.as_bytes());
                            self.flush()?;
                        }
                        Authentication::Md5Password(salt) => {
                            let secret = auth::md5_password(&config.user, &config.password, salt);
                            self.writer.clear();
                            self.writer.password(secret.as_bytes());
                            self.flush()?;
                        }
                        Authentication::Sasl(mechanisms) => {
                            if !mechanisms.iter().any(|name| name == auth::Scram::MECHANISM) {
                                return Err(PostgresError::Unsupported(format!(
                                    "the server offers only {}, and this client speaks {}",
                                    mechanisms.join(", "),
                                    auth::Scram::MECHANISM
                                )));
                            }
                            let mut exchange = auth::Scram::new(&config.password);
                            let first = exchange.client_first();
                            self.writer.clear();
                            self.writer.sasl_initial(auth::Scram::MECHANISM, &first);
                            self.flush()?;
                            scram = Some(exchange);
                        }
                        Authentication::SaslContinue(challenge) => {
                            let exchange = scram.as_mut().ok_or_else(|| {
                                PostgresError::Protocol(
                                    "the server continued a SASL exchange that never started"
                                        .to_owned(),
                                )
                            })?;
                            let answer = exchange.client_final(&challenge)?;
                            self.writer.clear();
                            self.writer.sasl(&answer);
                            self.flush()?;
                        }
                        Authentication::SaslFinal(proof) => {
                            let exchange = scram.as_ref().ok_or_else(|| {
                                PostgresError::Protocol(
                                    "the server finished a SASL exchange that never started"
                                        .to_owned(),
                                )
                            })?;
                            exchange.verify(&proof)?;
                        }
                        Authentication::Unsupported(code) => {
                            return Err(PostgresError::Unsupported(format!(
                                "authentication method {code} (only password, md5, and {} work)",
                                auth::Scram::MECHANISM
                            )));
                        }
                    }
                }
                backend::BACKEND_KEY_DATA => {
                    let mut reader = FieldReader::new(&self.incoming);
                    self.process_id = reader.i32()?;
                    self.secret_key = reader.i32()?;
                }
                backend::READY_FOR_QUERY => {
                    let mut reader = FieldReader::new(&self.incoming);
                    self.transaction_status = reader.u8()?;
                    return Ok(());
                }
                backend::ERROR => return Err(self.take_server_error()),
                backend::PARAMETER_STATUS | backend::NOTICE | backend::NOTIFICATION => {
                    self.absorb(tag)?;
                }
                other => {
                    return Err(PostgresError::Protocol(format!(
                        "unexpected message '{}' while connecting",
                        other as char
                    )));
                }
            }
        }
    }

    /// Runs one statement with its parameters and reads the whole result.
    ///
    /// One statement per call: the extended protocol this uses does not accept
    /// several separated by semicolons, which also means a parameter can never
    /// smuggle a second statement in behind the first.
    pub fn query(
        &mut self,
        sql: &str,
        parameters: &[Option<Vec<u8>>],
    ) -> Result<QueryResult, PostgresError> {
        if self.closed {
            return Err(PostgresError::Protocol(
                "the connection is closed".to_owned(),
            ));
        }
        if parameters.len() > MAXIMUM_PARAMETERS {
            return Err(PostgresError::Limit(format!(
                "a statement takes at most {MAXIMUM_PARAMETERS} parameters"
            )));
        }
        self.writer.clear();
        self.writer.parse("", sql);
        self.writer.bind("", "", parameters);
        self.writer.describe_portal("");
        self.writer.execute("", 0);
        self.writer.sync();
        self.flush()?;

        let mut result = QueryResult::default();
        let mut failure = None;
        let mut result_bytes = 0usize;
        loop {
            let tag = self.read_message()?;
            match tag {
                backend::ROW_DESCRIPTION => {
                    result.columns =
                        protocol::parse_row_description(&self.incoming, MAXIMUM_COLUMNS)?;
                }
                backend::DATA_ROW => {
                    if result.rows.len() >= MAXIMUM_ROWS {
                        // The connection is left readable on purpose: the rest
                        // of the result is drained below before this returns.
                        failure.get_or_insert(PostgresError::Limit(format!(
                            "a result may hold at most {MAXIMUM_ROWS} rows"
                        )));
                        continue;
                    }
                    let mut reader = FieldReader::new(&self.incoming);
                    let count = reader.i16()?;
                    if count < 0 || count as usize > MAXIMUM_COLUMNS {
                        return Err(PostgresError::Protocol(
                            "a row has an invalid width".to_owned(),
                        ));
                    }
                    let mut row = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        let value = reader.value()?.map(<[u8]>::to_vec);
                        result_bytes += value.as_ref().map_or(0, Vec::len);
                        row.push(value);
                    }
                    if result_bytes > MAXIMUM_RESULT_BYTES {
                        failure.get_or_insert(PostgresError::Limit(format!(
                            "a result may hold at most {MAXIMUM_RESULT_BYTES} bytes"
                        )));
                        continue;
                    }
                    result.rows.push(row);
                }
                backend::COMMAND_COMPLETE => {
                    let mut reader = FieldReader::new(&self.incoming);
                    result.command = reader.string()?.to_owned();
                    result.affected_rows = protocol::parse_affected_rows(&result.command);
                }
                backend::ERROR => {
                    // Not returned yet: the server still owes a ReadyForQuery,
                    // and a connection left mid-exchange is a connection that
                    // cannot be used again.
                    failure.get_or_insert(self.take_server_error());
                }
                backend::READY_FOR_QUERY => {
                    let mut reader = FieldReader::new(&self.incoming);
                    self.transaction_status = reader.u8()?;
                    return match failure {
                        Some(error) => Err(error),
                        None => Ok(result),
                    };
                }
                backend::PARSE_COMPLETE
                | backend::BIND_COMPLETE
                | backend::CLOSE_COMPLETE
                | backend::NO_DATA
                | backend::EMPTY_QUERY
                | backend::PARAMETER_DESCRIPTION
                | backend::PORTAL_SUSPENDED => {}
                backend::COPY_IN | backend::COPY_OUT | backend::COPY_BOTH => {
                    return Err(PostgresError::Unsupported(
                        "COPY is not implemented; the connection cannot continue".to_owned(),
                    ));
                }
                other => self.absorb(other)?,
            }
        }
    }

    /// The parameters the server reported, such as `server_version`.
    pub fn parameters(&self) -> &BTreeMap<String, String> {
        &self.parameters
    }

    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    /// Whether the session is inside a transaction, and whether it has failed.
    pub fn transaction_status(&self) -> char {
        self.transaction_status as char
    }

    /// Notices the server sent, and clears them.
    pub fn take_notices(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.notices)
    }

    /// Says goodbye and stops. A server told the session is over closes it
    /// tidily instead of finding out from a reset socket.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.writer.clear();
        self.writer.terminate();
        let _ = self.stream.write_all(self.writer.bytes());
        let _ = self.stream.flush();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Handles a message that can arrive at any point in any exchange.
    fn absorb(&mut self, tag: u8) -> Result<(), PostgresError> {
        match tag {
            backend::PARAMETER_STATUS => {
                let body = std::mem::take(&mut self.incoming);
                let mut reader = FieldReader::new(&body);
                let name = reader.string()?.to_owned();
                let value = reader.string()?.to_owned();
                self.parameters.insert(name, value);
                self.incoming = body;
                Ok(())
            }
            backend::NOTICE => {
                let body = std::mem::take(&mut self.incoming);
                let notice = protocol::parse_server_message(&body)?;
                self.incoming = body;
                // Bounded like everything else: a chatty server cannot grow
                // this without the caller ever reading it.
                if self.notices.len() < 256 {
                    self.notices.push(notice);
                }
                Ok(())
            }
            // LISTEN/NOTIFY has no surface here yet, so a notification is
            // dropped rather than being mistaken for a result.
            backend::NOTIFICATION => Ok(()),
            other => Err(PostgresError::Protocol(format!(
                "unexpected message '{}'",
                other as char
            ))),
        }
    }

    fn take_server_error(&mut self) -> PostgresError {
        let body = std::mem::take(&mut self.incoming);
        let error = match protocol::parse_server_message(&body) {
            Ok(message) => PostgresError::Server(Box::new(message)),
            Err(error) => error,
        };
        self.incoming = body;
        error
    }

    fn flush(&mut self) -> Result<(), PostgresError> {
        self.stream.write_all(self.writer.bytes())?;
        self.stream.flush()?;
        self.writer.clear();
        Ok(())
    }

    /// Reads one message into `self.incoming` and returns its tag.
    fn read_message(&mut self) -> Result<u8, PostgresError> {
        let mut header = [0u8; 5];
        self.stream.read_exact(&mut header)?;
        let length = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if length < 4 {
            return Err(PostgresError::Protocol(format!(
                "message length {length} is impossible"
            )));
        }
        let body_length = length as usize - 4;
        if body_length > MAXIMUM_MESSAGE_BYTES {
            return Err(PostgresError::Limit(format!(
                "a message of {body_length} bytes exceeds the {MAXIMUM_MESSAGE_BYTES} byte limit"
            )));
        }
        self.incoming.clear();
        self.incoming.resize(body_length, 0);
        self.stream.read_exact(&mut self.incoming)?;
        Ok(header[0])
    }
}

impl fmt::Debug for Connection {
    /// Deliberately without the configuration: a connection's debug output
    /// should never be the thing that puts a password in a log.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("process_id", &self.process_id)
            .field("transaction_status", &self.transaction_status())
            .field("closed", &self.closed)
            .finish()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_full_connection_url() {
        let config = PostgresConfig::from_url(
            "postgres://sako:secret@db.example:6543/app?application_name=bench",
        )
        .unwrap();
        assert_eq!(config.user, "sako");
        assert_eq!(config.password, "secret");
        assert_eq!(config.host, "db.example");
        assert_eq!(config.port, 6543);
        assert_eq!(config.database, "app");
        assert_eq!(config.application_name, "bench");
    }

    #[test]
    fn defaults_fill_in_what_a_short_url_leaves_out() {
        let config = PostgresConfig::from_url("postgres://sako@localhost").unwrap();
        assert_eq!(config.port, 5432);
        // Same as psql: no database named means the one named after the user.
        assert_eq!(config.database, "sako");
        assert_eq!(config.password, "");
    }

    #[test]
    fn decodes_a_password_that_needs_escaping() {
        let config =
            PostgresConfig::from_url("postgres://sako:p%40ss%3Aword@localhost/app").unwrap();
        assert_eq!(config.password, "p@ss:word");
    }

    #[test]
    fn keeps_the_colons_in_a_bracketed_ipv6_host() {
        let config = PostgresConfig::from_url("postgres://sako@[::1]:5433/app").unwrap();
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 5433);
    }

    #[test]
    fn refuses_a_url_without_a_user() {
        let error = PostgresConfig::from_url("postgres://localhost/app").unwrap_err();
        assert!(error.to_string().contains("needs a user"));
    }

    #[test]
    fn refuses_a_scheme_it_does_not_speak() {
        let error = PostgresConfig::from_url("mysql://sako@localhost/app").unwrap_err();
        assert!(error.to_string().contains("postgres://"));
    }

    /// TLS is not implemented, and a URL that asks for it has to say so rather
    /// than connecting in the clear and looking like it worked.
    #[test]
    fn refuses_to_pretend_it_can_do_tls() {
        let error =
            PostgresConfig::from_url("postgres://sako@localhost/app?sslmode=require").unwrap_err();
        assert!(error.to_string().contains("TLS"));
        PostgresConfig::from_url("postgres://sako@localhost/app?sslmode=disable").unwrap();
    }

    #[test]
    fn refuses_a_port_that_is_not_a_number() {
        let error = PostgresConfig::from_url("postgres://sako@localhost:http/app").unwrap_err();
        assert!(error.to_string().contains("port is not a number"));
    }
}
