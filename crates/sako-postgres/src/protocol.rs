// SPDX-License-Identifier: BSD-3-Clause

//! The PostgreSQL v3 frontend/backend protocol, as bytes.
//!
//! Every message is a one-byte tag, a four-byte length that counts itself, and
//! a body -- the startup message alone omits the tag. Nothing here talks to a
//! socket: this module turns messages into bytes and bytes back into readable
//! fields, so the connection above it is the only thing that has to reason
//! about I/O.

use crate::PostgresError;

/// Protocol version 3.0, as the startup message spells it.
pub const PROTOCOL_VERSION: i32 = 196_608;

pub mod frontend {
    pub const PASSWORD: u8 = b'p';
    pub const PARSE: u8 = b'P';
    pub const BIND: u8 = b'B';
    pub const DESCRIBE: u8 = b'D';
    pub const EXECUTE: u8 = b'E';
    pub const SYNC: u8 = b'S';
    pub const TERMINATE: u8 = b'X';
}

pub mod backend {
    pub const AUTHENTICATION: u8 = b'R';
    pub const BACKEND_KEY_DATA: u8 = b'K';
    pub const BIND_COMPLETE: u8 = b'2';
    pub const CLOSE_COMPLETE: u8 = b'3';
    pub const COMMAND_COMPLETE: u8 = b'C';
    pub const DATA_ROW: u8 = b'D';
    pub const EMPTY_QUERY: u8 = b'I';
    pub const ERROR: u8 = b'E';
    pub const NO_DATA: u8 = b'n';
    pub const NOTICE: u8 = b'N';
    pub const NOTIFICATION: u8 = b'A';
    pub const PARAMETER_DESCRIPTION: u8 = b't';
    pub const PARAMETER_STATUS: u8 = b'S';
    pub const PARSE_COMPLETE: u8 = b'1';
    pub const PORTAL_SUSPENDED: u8 = b's';
    pub const READY_FOR_QUERY: u8 = b'Z';
    pub const ROW_DESCRIPTION: u8 = b'T';
    /// COPY, which this driver does not implement. Recognized only so the
    /// failure names the reason.
    pub const COPY_IN: u8 = b'G';
    pub const COPY_OUT: u8 = b'H';
    pub const COPY_BOTH: u8 = b'W';
}

/// The authentication requests a server can make.
#[derive(Debug, Eq, PartialEq)]
pub enum Authentication {
    Ok,
    CleartextPassword,
    Md5Password([u8; 4]),
    /// The mechanisms the server offers, in the order it offered them.
    Sasl(Vec<String>),
    SaslContinue(Vec<u8>),
    SaslFinal(Vec<u8>),
    /// Kerberos, GSSAPI, SSPI: named so the failure is specific.
    Unsupported(i32),
}

/// Builds frontend messages into one reusable buffer.
#[derive(Default)]
pub struct MessageWriter {
    buffer: Vec<u8>,
}

impl MessageWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buffer
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Opens a tagged message and returns where its length field starts.
    fn begin(&mut self, tag: u8) -> usize {
        self.buffer.push(tag);
        let start = self.buffer.len();
        self.buffer.extend_from_slice(&[0, 0, 0, 0]);
        start
    }

    /// Writes the length a message turned out to have. The count includes the
    /// four bytes of the length field itself and excludes the tag.
    fn finish(&mut self, start: usize) {
        let length = (self.buffer.len() - start) as u32;
        self.buffer[start..start + 4].copy_from_slice(&length.to_be_bytes());
    }

    fn push_string(&mut self, value: &str) {
        self.buffer.extend_from_slice(value.as_bytes());
        self.buffer.push(0);
    }

    fn push_i16(&mut self, value: i16) {
        self.buffer.extend_from_slice(&value.to_be_bytes());
    }

    fn push_i32(&mut self, value: i32) {
        self.buffer.extend_from_slice(&value.to_be_bytes());
    }

    /// The first thing a connection sends: the protocol version and the
    /// parameters the session starts with. It carries no tag, because the
    /// server has not yet agreed which protocol is being spoken.
    pub fn startup(&mut self, parameters: &[(&str, &str)]) {
        let start = self.buffer.len();
        self.buffer.extend_from_slice(&[0, 0, 0, 0]);
        self.push_i32(PROTOCOL_VERSION);
        for (name, value) in parameters {
            self.push_string(name);
            self.push_string(value);
        }
        self.buffer.push(0);
        self.finish(start);
    }

    pub fn password(&mut self, secret: &[u8]) {
        let start = self.begin(frontend::PASSWORD);
        self.buffer.extend_from_slice(secret);
        self.buffer.push(0);
        self.finish(start);
    }

    /// SASL's first message shares the password tag and adds the mechanism.
    pub fn sasl_initial(&mut self, mechanism: &str, data: &[u8]) {
        let start = self.begin(frontend::PASSWORD);
        self.push_string(mechanism);
        self.push_i32(data.len() as i32);
        self.buffer.extend_from_slice(data);
        self.finish(start);
    }

    pub fn sasl(&mut self, data: &[u8]) {
        let start = self.begin(frontend::PASSWORD);
        self.buffer.extend_from_slice(data);
        self.finish(start);
    }

    pub fn parse(&mut self, name: &str, sql: &str) {
        let start = self.begin(frontend::PARSE);
        self.push_string(name);
        self.push_string(sql);
        // No parameter types: the server infers them, which is what a driver
        // that sends every parameter as text wants it to do.
        self.push_i16(0);
        self.finish(start);
    }

    /// Binds text parameters to a portal and asks for text results.
    ///
    /// Text both ways is a deliberate choice: the binary format is per-type
    /// and versioned, and getting one wrong corrupts a value silently, where
    /// text is what the server itself prints and what every client can read.
    pub fn bind(&mut self, portal: &str, statement: &str, parameters: &[Option<Vec<u8>>]) {
        let start = self.begin(frontend::BIND);
        self.push_string(portal);
        self.push_string(statement);
        // Zero format codes means "text for all of them".
        self.push_i16(0);
        self.push_i16(parameters.len() as i16);
        for parameter in parameters {
            match parameter {
                Some(value) => {
                    self.push_i32(value.len() as i32);
                    self.buffer.extend_from_slice(value);
                }
                None => self.push_i32(-1),
            }
        }
        self.push_i16(0);
        self.finish(start);
    }

    pub fn describe_portal(&mut self, name: &str) {
        let start = self.begin(frontend::DESCRIBE);
        self.buffer.push(b'P');
        self.push_string(name);
        self.finish(start);
    }

    pub fn execute(&mut self, portal: &str, maximum_rows: u32) {
        let start = self.begin(frontend::EXECUTE);
        self.push_string(portal);
        self.push_i32(maximum_rows as i32);
        self.finish(start);
    }

    pub fn sync(&mut self) {
        let start = self.begin(frontend::SYNC);
        self.finish(start);
    }

    pub fn terminate(&mut self) {
        let start = self.begin(frontend::TERMINATE);
        self.finish(start);
    }
}

/// Reads fields out of one message body, refusing to walk off the end.
pub struct FieldReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> FieldReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], PostgresError> {
        let end = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| PostgresError::Protocol("message ended early".to_owned()))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.position..]
    }

    pub fn is_empty(&self) -> bool {
        self.position >= self.bytes.len()
    }

    pub fn u8(&mut self) -> Result<u8, PostgresError> {
        Ok(self.take(1)?[0])
    }

    pub fn i16(&mut self) -> Result<i16, PostgresError> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub fn i32(&mut self) -> Result<i32, PostgresError> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn u32(&mut self) -> Result<u32, PostgresError> {
        Ok(self.i32()? as u32)
    }

    /// A null-terminated string. Invalid UTF-8 is an error rather than a
    /// replacement character: a column name or an error message that did not
    /// survive the wire is not something to guess at.
    pub fn string(&mut self) -> Result<&'a str, PostgresError> {
        let start = self.position;
        let end = self.bytes[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| start + offset)
            .ok_or_else(|| PostgresError::Protocol("string is not terminated".to_owned()))?;
        self.position = end + 1;
        std::str::from_utf8(&self.bytes[start..end])
            .map_err(|_| PostgresError::Protocol("string is not UTF-8".to_owned()))
    }

    /// A length-prefixed value, where -1 means SQL NULL.
    pub fn value(&mut self) -> Result<Option<&'a [u8]>, PostgresError> {
        let length = self.i32()?;
        if length < 0 {
            return Ok(None);
        }
        Ok(Some(self.take(length as usize)?))
    }
}

/// One authentication request, parsed out of an 'R' message.
pub fn parse_authentication(body: &[u8]) -> Result<Authentication, PostgresError> {
    let mut reader = FieldReader::new(body);
    Ok(match reader.i32()? {
        0 => Authentication::Ok,
        3 => Authentication::CleartextPassword,
        5 => {
            let salt = reader.take(4)?;
            Authentication::Md5Password([salt[0], salt[1], salt[2], salt[3]])
        }
        10 => {
            let mut mechanisms = Vec::new();
            loop {
                let mechanism = reader.string()?;
                if mechanism.is_empty() {
                    break;
                }
                mechanisms.push(mechanism.to_owned());
                if reader.is_empty() {
                    break;
                }
            }
            Authentication::Sasl(mechanisms)
        }
        11 => Authentication::SaslContinue(reader.remaining().to_vec()),
        12 => Authentication::SaslFinal(reader.remaining().to_vec()),
        other => Authentication::Unsupported(other),
    })
}

/// What the server says went wrong, or what it is warning about.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerMessage {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: String,
    pub hint: String,
    pub position: String,
    pub schema: String,
    pub table: String,
    pub column: String,
    pub constraint: String,
}

impl ServerMessage {
    /// Formats the way `psql` does, so a failure reads the way the same
    /// failure reads everywhere else.
    pub fn describe(&self) -> String {
        let mut text = format!("{}: {}", self.severity, self.message);
        if !self.code.is_empty() {
            text.push_str(&format!(" ({})", self.code));
        }
        for (label, value) in [
            ("DETAIL", &self.detail),
            ("HINT", &self.hint),
            ("CONSTRAINT", &self.constraint),
        ] {
            if !value.is_empty() {
                text.push_str(&format!("\n{label}: {value}"));
            }
        }
        text
    }
}

/// Parses the field list an ErrorResponse or NoticeResponse carries.
pub fn parse_server_message(body: &[u8]) -> Result<ServerMessage, PostgresError> {
    let mut reader = FieldReader::new(body);
    let mut message = ServerMessage::default();
    loop {
        let field = reader.u8()?;
        if field == 0 {
            break;
        }
        let value = reader.string()?.to_owned();
        match field {
            b'S' => message.severity = value,
            // The non-localized severity, when the server sends one.
            b'V' => message.severity = value,
            b'C' => message.code = value,
            b'M' => message.message = value,
            b'D' => message.detail = value,
            b'H' => message.hint = value,
            b'P' => message.position = value,
            b's' => message.schema = value,
            b't' => message.table = value,
            b'c' => message.column = value,
            b'n' => message.constraint = value,
            _ => {}
        }
        if reader.is_empty() {
            break;
        }
    }
    if message.severity.is_empty() {
        message.severity = "ERROR".to_owned();
    }
    Ok(message)
}

/// One column of a result, as RowDescription describes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    pub name: String,
    pub type_oid: u32,
}

pub fn parse_row_description(
    body: &[u8],
    maximum_columns: usize,
) -> Result<Vec<Column>, PostgresError> {
    let mut reader = FieldReader::new(body);
    let count = reader.i16()?;
    if count < 0 || count as usize > maximum_columns {
        return Err(PostgresError::Limit(format!(
            "result has more than {maximum_columns} columns"
        )));
    }
    let mut columns = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = reader.string()?.to_owned();
        let _table_oid = reader.i32()?;
        let _attribute = reader.i16()?;
        let type_oid = reader.u32()?;
        let _type_size = reader.i16()?;
        let _type_modifier = reader.i32()?;
        let _format = reader.i16()?;
        columns.push(Column { name, type_oid });
    }
    Ok(columns)
}

/// The number of rows a command reported, read off its completion tag.
///
/// `INSERT` puts an OID before the count and everything else puts the count
/// last, so the last integer in the tag is the answer either way.
pub fn parse_affected_rows(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_carries_its_own_length_and_no_tag() {
        let mut writer = MessageWriter::new();
        writer.startup(&[("user", "sako"), ("database", "app")]);
        let bytes = writer.bytes();
        let length = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(length as usize, bytes.len());
        assert_eq!(
            i32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            PROTOCOL_VERSION
        );
        assert_eq!(bytes[bytes.len() - 1], 0);
        assert!(bytes.windows(5).any(|window| window == b"sako\0"));
    }

    #[test]
    fn a_tagged_message_counts_its_length_field_but_not_its_tag() {
        let mut writer = MessageWriter::new();
        writer.parse("", "select 1");
        let bytes = writer.bytes();
        assert_eq!(bytes[0], frontend::PARSE);
        let length = i32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        assert_eq!(length as usize, bytes.len() - 1);
        // An empty statement name, the SQL, then a zero parameter-type count.
        assert_eq!(&bytes[5..], b"\0select 1\0\0\0");
    }

    #[test]
    fn bind_marks_a_null_parameter_with_a_negative_length() {
        let mut writer = MessageWriter::new();
        writer.bind("", "", &[Some(b"one".to_vec()), None]);
        let bytes = writer.bytes();
        assert_eq!(bytes[0], frontend::BIND);
        // portal, statement, format count, parameter count, then the values.
        let mut reader = FieldReader::new(&bytes[5..]);
        assert_eq!(reader.string().unwrap(), "");
        assert_eq!(reader.string().unwrap(), "");
        assert_eq!(reader.i16().unwrap(), 0);
        assert_eq!(reader.i16().unwrap(), 2);
        assert_eq!(reader.value().unwrap(), Some(&b"one"[..]));
        assert_eq!(reader.value().unwrap(), None);
    }

    #[test]
    fn reads_every_authentication_request_shape() {
        assert_eq!(
            parse_authentication(&0i32.to_be_bytes()).unwrap(),
            Authentication::Ok
        );
        assert_eq!(
            parse_authentication(&3i32.to_be_bytes()).unwrap(),
            Authentication::CleartextPassword
        );
        let mut md5 = 5i32.to_be_bytes().to_vec();
        md5.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            parse_authentication(&md5).unwrap(),
            Authentication::Md5Password([1, 2, 3, 4])
        );
        let mut sasl = 10i32.to_be_bytes().to_vec();
        sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
        assert_eq!(
            parse_authentication(&sasl).unwrap(),
            Authentication::Sasl(vec!["SCRAM-SHA-256".to_owned()])
        );
        assert_eq!(
            parse_authentication(&6i32.to_be_bytes()).unwrap(),
            Authentication::Unsupported(6)
        );
    }

    #[test]
    fn reads_an_error_response_into_its_fields() {
        let mut body = Vec::new();
        for (field, value) in [
            (b'S', "FATAL"),
            (b'C', "28P01"),
            (b'M', "password authentication failed"),
            (b'H', "check the password"),
        ] {
            body.push(field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let message = parse_server_message(&body).unwrap();
        assert_eq!(message.code, "28P01");
        assert!(
            message
                .describe()
                .contains("password authentication failed")
        );
        assert!(message.describe().contains("28P01"));
        assert!(message.describe().contains("HINT: check the password"));
    }

    #[test]
    fn reads_a_row_description() {
        let mut body = 2i16.to_be_bytes().to_vec();
        for (name, oid) in [("id", 23u32), ("name", 25u32)] {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(&0i32.to_be_bytes());
            body.extend_from_slice(&0i16.to_be_bytes());
            body.extend_from_slice(&oid.to_be_bytes());
            body.extend_from_slice(&4i16.to_be_bytes());
            body.extend_from_slice(&(-1i32).to_be_bytes());
            body.extend_from_slice(&0i16.to_be_bytes());
        }
        let columns = parse_row_description(&body, 64).unwrap();
        assert_eq!(
            columns,
            vec![
                Column {
                    name: "id".to_owned(),
                    type_oid: 23
                },
                Column {
                    name: "name".to_owned(),
                    type_oid: 25
                },
            ]
        );
    }

    #[test]
    fn refuses_a_result_wider_than_the_limit() {
        let body = 4i16.to_be_bytes().to_vec();
        let error = parse_row_description(&body, 2).unwrap_err();
        assert!(matches!(error, PostgresError::Limit(_)));
    }

    #[test]
    fn a_truncated_message_is_an_error_rather_than_a_panic() {
        let mut reader = FieldReader::new(&[0, 1]);
        assert!(reader.i32().is_err());
        let mut reader = FieldReader::new(b"unterminated");
        assert!(reader.string().is_err());
    }

    #[test]
    fn reads_the_row_count_off_a_completion_tag() {
        assert_eq!(parse_affected_rows("SELECT 3"), 3);
        assert_eq!(parse_affected_rows("INSERT 0 12"), 12);
        assert_eq!(parse_affected_rows("UPDATE 7"), 7);
        assert_eq!(parse_affected_rows("BEGIN"), 0);
    }
}
