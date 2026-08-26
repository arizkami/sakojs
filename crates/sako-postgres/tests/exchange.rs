// SPDX-License-Identifier: BSD-3-Clause

//! The client against a server that speaks the protocol back at it.
//!
//! There is no PostgreSQL running in this test, and that is the point: the
//! fake below answers with exactly the bytes the protocol specifies, so a test
//! failure is the client's and not a server version's. What it cannot check is
//! whether a real server agrees with the specification -- `real_server` at the
//! bottom does that, against `SAKO_POSTGRES_URL` when one is configured.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use sako_postgres::{Connection, PostgresConfig, PostgresError};

/// Builds one backend message: a tag, the length that counts itself, the body.
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn authentication_ok() -> Vec<u8> {
    message(b'R', &0i32.to_be_bytes())
}

fn ready(status: u8) -> Vec<u8> {
    message(b'Z', &[status])
}

fn parameter_status(name: &str, value: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    message(b'S', &body)
}

fn row_description(columns: &[(&str, u32)]) -> Vec<u8> {
    let mut body = (columns.len() as i16).to_be_bytes().to_vec();
    for (name, oid) in columns {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
    }
    message(b'T', &body)
}

fn data_row(values: &[Option<&str>]) -> Vec<u8> {
    let mut body = (values.len() as i16).to_be_bytes().to_vec();
    for value in values {
        match value {
            Some(text) => {
                body.extend_from_slice(&(text.len() as i32).to_be_bytes());
                body.extend_from_slice(text.as_bytes());
            }
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    message(b'D', &body)
}

fn command_complete(tag: &str) -> Vec<u8> {
    let mut body = tag.as_bytes().to_vec();
    body.push(0);
    message(b'C', &body)
}

fn error_response(fields: &[(u8, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (field, value) in fields {
        body.push(*field);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    message(b'E', &body)
}

/// Reads one frontend message and returns its tag and body. The startup
/// message has no tag, so `expect_startup` handles that one.
fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).expect("a frontend message");
    let length = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let mut body = vec![0; length as usize - 4];
    stream.read_exact(&mut body).expect("a message body");
    (header[0], body)
}

fn read_startup(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).expect("a startup length");
    let length = i32::from_be_bytes(length);
    let mut body = vec![0; length as usize - 4];
    stream.read_exact(&mut body).expect("a startup body");
    body
}

/// Starts a one-connection server that runs `session` and returns its address.
fn serve(session: impl FnOnce(TcpStream) + Send + 'static) -> (String, u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("a client");
        session(stream);
    });
    ("127.0.0.1".to_owned(), port, handle)
}

fn config(port: u16) -> PostgresConfig {
    PostgresConfig {
        port,
        user: "sako".to_owned(),
        database: "app".to_owned(),
        ..PostgresConfig::default()
    }
}

#[test]
fn connects_reads_parameters_and_runs_a_query() {
    let (_, port, server) = serve(|mut stream| {
        let startup = read_startup(&mut stream);
        assert!(startup.windows(5).any(|window| window == b"sako\0"));
        assert!(startup.windows(4).any(|window| window == b"app\0"));

        let mut hello = authentication_ok();
        hello.extend(parameter_status("server_version", "16.2"));
        hello.extend(ready(b'I'));
        stream.write_all(&hello).unwrap();

        // Parse, Bind, Describe, Execute, Sync all arrive in one write.
        let (parse, body) = read_message(&mut stream);
        assert_eq!(parse, b'P');
        assert!(body.windows(9).any(|window| window == b"select $1"));
        for expected in *b"BDES" {
            let (tag, _) = read_message(&mut stream);
            assert_eq!(tag, expected);
        }

        let mut answer = message(b'1', &[]);
        answer.extend(message(b'2', &[]));
        answer.extend(row_description(&[("id", 23), ("email", 25), ("note", 25)]));
        answer.extend(data_row(&[Some("7"), Some("a@example"), None]));
        answer.extend(command_complete("SELECT 1"));
        answer.extend(ready(b'I'));
        stream.write_all(&answer).unwrap();

        let (terminate, _) = read_message(&mut stream);
        assert_eq!(terminate, b'X');
    });

    let mut connection = Connection::connect(&config(port)).expect("the fake server accepts");
    assert_eq!(
        connection
            .parameters()
            .get("server_version")
            .map(String::as_str),
        Some("16.2")
    );
    let result = connection
        .query("select $1", &[Some(b"7".to_vec())])
        .expect("the query succeeds");
    assert_eq!(result.columns.len(), 3);
    assert_eq!(result.columns[0].name, "id");
    assert_eq!(result.columns[0].type_oid, 23);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0].as_deref(), Some(&b"7"[..]));
    assert_eq!(result.rows[0][1].as_deref(), Some(&b"a@example"[..]));
    // A NULL is not an empty string, and the difference has to survive.
    assert_eq!(result.rows[0][2], None);
    assert_eq!(result.command, "SELECT 1");
    assert_eq!(result.affected_rows, 1);
    connection.close();
    server.join().unwrap();
}

/// A server error has to leave the connection usable: the client owes the
/// exchange a ReadyForQuery before it can send anything else.
#[test]
fn a_failed_statement_leaves_the_connection_usable() {
    let (_, port, server) = serve(|mut stream| {
        read_startup(&mut stream);
        let mut hello = authentication_ok();
        hello.extend(ready(b'I'));
        stream.write_all(&hello).unwrap();

        for _ in 0..5 {
            read_message(&mut stream);
        }
        let mut failure = error_response(&[
            (b'S', "ERROR"),
            (b'C', "42P01"),
            (b'M', "relation \"nope\" does not exist"),
        ]);
        failure.extend(ready(b'I'));
        stream.write_all(&failure).unwrap();

        for _ in 0..5 {
            read_message(&mut stream);
        }
        let mut answer = row_description(&[("one", 23)]);
        answer.extend(data_row(&[Some("1")]));
        answer.extend(command_complete("SELECT 1"));
        answer.extend(ready(b'I'));
        stream.write_all(&answer).unwrap();
        read_message(&mut stream);
    });

    let mut connection = Connection::connect(&config(port)).unwrap();
    let error = connection.query("select * from nope", &[]).unwrap_err();
    assert_eq!(error.code(), Some("42P01"));
    assert!(error.to_string().contains("does not exist"));

    let result = connection
        .query("select 1", &[])
        .expect("the connection still works");
    assert_eq!(result.rows[0][0].as_deref(), Some(&b"1"[..]));
    connection.close();
    server.join().unwrap();
}

/// md5 is what an older server asks for, and the answer is a fixed
/// construction the server can check against what it stored.
#[test]
fn answers_an_md5_challenge() {
    let (_, port, server) = serve(|mut stream| {
        read_startup(&mut stream);
        let mut challenge = 5i32.to_be_bytes().to_vec();
        challenge.extend_from_slice(&[9, 8, 7, 6]);
        stream.write_all(&message(b'R', &challenge)).unwrap();

        let (tag, body) = read_message(&mut stream);
        assert_eq!(tag, b'p');
        let answer = String::from_utf8(body[..body.len() - 1].to_vec()).unwrap();
        assert!(answer.starts_with("md5"));
        assert_eq!(answer.len(), 35);

        let mut accept = authentication_ok();
        accept.extend(ready(b'I'));
        stream.write_all(&accept).unwrap();
        read_message(&mut stream);
    });

    let mut config = config(port);
    config.password = "secret".to_owned();
    let mut connection = Connection::connect(&config).expect("md5 is accepted");
    connection.close();
    server.join().unwrap();
}

/// A method this client cannot perform has to say which one, rather than
/// hanging or failing as though the password were wrong.
#[test]
fn refuses_an_authentication_method_it_cannot_perform() {
    let (_, port, server) = serve(|mut stream| {
        read_startup(&mut stream);
        // 7 is GSSAPI.
        stream
            .write_all(&message(b'R', &7i32.to_be_bytes()))
            .unwrap();
        let mut ignored = Vec::new();
        let _ = stream.read_to_end(&mut ignored);
    });

    let error = Connection::connect(&config(port)).unwrap_err();
    assert!(matches!(error, PostgresError::Unsupported(_)));
    assert!(error.to_string().contains("authentication method 7"));
    let _ = server.join();
}

/// A server that says it will send more than the limit allows is refused
/// before the bytes are read, not after they are in memory.
#[test]
fn refuses_an_oversized_message() {
    let (_, port, server) = serve(|mut stream| {
        read_startup(&mut stream);
        let mut header = vec![b'R'];
        header.extend_from_slice(&(i32::MAX).to_be_bytes());
        stream.write_all(&header).unwrap();
        let mut ignored = Vec::new();
        let _ = stream.read_to_end(&mut ignored);
    });

    let error = Connection::connect(&config(port)).unwrap_err();
    assert!(matches!(error, PostgresError::Limit(_)), "{error}");
    let _ = server.join();
}

#[test]
fn reports_a_server_that_is_not_listening() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = Connection::connect(&config(port)).unwrap_err();
    assert!(matches!(error, PostgresError::Io(_)), "{error}");
}

/// The same exchange against a real server, when one is configured. Set
/// `SAKO_POSTGRES_URL` to something like
/// `postgres://postgres:postgres@localhost/postgres` and this stops skipping.
#[test]
fn real_server() {
    let Ok(url) = std::env::var("SAKO_POSTGRES_URL") else {
        eprintln!("skipped: set SAKO_POSTGRES_URL to run against a real server");
        return;
    };
    let config = PostgresConfig::from_url(&url).expect("a usable connection string");
    let mut connection = Connection::connect(&config).expect("the server accepts the connection");
    assert!(connection.parameters().contains_key("server_version"));

    let result = connection
        .query(
            "select $1::int + 1 as answer, null::text as absent",
            &[Some(b"41".to_vec())],
        )
        .expect("a parameterized query runs");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0].as_deref(), Some(&b"42"[..]));
    assert_eq!(result.rows[0][1], None);
    assert_eq!(result.columns[0].name, "answer");

    let error = connection
        .query("select * from a_table_that_does_not_exist", &[])
        .unwrap_err();
    assert_eq!(error.code(), Some("42P01"));

    // The connection survived the failure.
    let after = connection.query("select 1", &[]).expect("still usable");
    assert_eq!(after.rows.len(), 1);
    connection.close();
}
