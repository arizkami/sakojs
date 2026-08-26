// SPDX-License-Identifier: BSD-3-Clause

use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::io::Read;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use sako_http::{HttpResponse, HttpServer, HttpServerConfig, RequestHead};
use sako_net::resolve_host;
use sako_postgres::{Connection as PostgresConnection, PostgresConfig, QueryResult};
use sako_process::{
    AsyncChild, AsyncSpawn, ChildEvent, activity_tick, spawn_native_with_bounded_output,
    wait_for_activity,
};
use sako_typescript::{OutputModuleKind, transpile};

const ERROR_BUFFER_CAPACITY: usize = 16 * 1024;
const MAXIMUM_DNS_RESULTS: usize = 16;
const MAXIMUM_CHILD_ARGUMENTS: usize = 256;
const MAXIMUM_CHILD_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAXIMUM_CHILD_ENVIRONMENT_VARIABLES: usize = 512;
const MAXIMUM_POSTGRES_PARAMETERS: usize = 4_096;
const MAXIMUM_FETCH_HEADERS: usize = 128;
const MAXIMUM_FETCH_BODY_BYTES: usize = 16 * 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeBytes {
    pub data: *const u8,
    pub length: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeHeader {
    pub name: NativeBytes,
    pub value: NativeBytes,
}

#[repr(C)]
pub struct NativeHttpResponse {
    pub status: u16,
    pub reason: NativeBytes,
    pub headers: *const NativeHeader,
    pub header_count: usize,
    pub body: NativeBytes,
}

struct NativeProcessOutput {
    status: c_int,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct NativeFetchOutput {
    status: u16,
    status_text: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct NativeTypeScriptOutput {
    source: Vec<u8>,
}

/// Transpiles one bounded TypeScript source for the native module loader.
///
/// # Safety
/// `path` and `source` must describe readable UTF-8 ranges for this call.
/// `error` follows the writable-buffer contract. The returned owner must be
/// deleted exactly once with `sako_typescript_output_delete`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_typescript_transpile(
    path: NativeBytes,
    source: NativeBytes,
    commonjs: c_int,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    let path = match copy_utf8(path, "TypeScript path") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        Ok(_) => {
            write_native_error(error, error_capacity, "TypeScript path is empty");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let source = match copy_utf8(source, "TypeScript source") {
        Ok(source) => source,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let output_kind = if commonjs == 0 {
        OutputModuleKind::Esm
    } else {
        OutputModuleKind::CommonJs
    };
    let source = match transpile(&path, &source, output_kind) {
        Ok(source) => source.into_bytes(),
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(NativeTypeScriptOutput { source })).cast()
}

/// Borrows the emitted JavaScript for a live TypeScript output.
///
/// # Safety
/// `output` must remain live until the returned range is consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_typescript_output_source(output: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeTypeScriptOutput>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |output| native_bytes(&output.source),
    )
}

/// Deletes one TypeScript transpilation output owner.
///
/// # Safety
/// `output` must be null or uniquely owned from `sako_typescript_transpile`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_typescript_output_delete(output: *mut c_void) {
    if !output.is_null() {
        // SAFETY: ownership is returned exactly once by the native loader.
        drop(unsafe { Box::from_raw(output.cast::<NativeTypeScriptOutput>()) });
    }
}

/// Executes one bounded HTTP/HTTPS request for the JavaScript Fetch surface.
///
/// # Safety
///
/// Byte ranges and the header array must remain readable for this call. `error`
/// follows the writable-buffer contract. The returned owner must be deleted
/// exactly once with `sako_fetch_output_delete`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_sync(
    url: NativeBytes,
    method: NativeBytes,
    headers: *const NativeHeader,
    header_count: usize,
    body: NativeBytes,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    if header_count > MAXIMUM_FETCH_HEADERS || (header_count != 0 && headers.is_null()) {
        write_native_error(error, error_capacity, "fetch headers are invalid");
        return std::ptr::null_mut();
    }
    let url = match copy_utf8(url, "fetch URL") {
        Ok(value) if !value.is_empty() && value.len() <= 16 * 1024 => value,
        Ok(_) => {
            write_native_error(error, error_capacity, "fetch URL is invalid");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let method = match copy_utf8(method, "fetch method") {
        Ok(value) if !value.is_empty() && value.len() <= 32 => value,
        Ok(_) => {
            write_native_error(error, error_capacity, "fetch method is invalid");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let body = match copy_bytes(body, "fetch body") {
        Ok(value) if value.len() <= MAXIMUM_FETCH_BODY_BYTES => value,
        Ok(_) => {
            write_native_error(error, error_capacity, "fetch body exceeds byte limit");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let native_headers = if header_count == 0 {
        &[][..]
    } else {
        // SAFETY: the caller promises an initialized header_count array.
        unsafe { std::slice::from_raw_parts(headers, header_count) }
    };
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build();
    let mut request = agent.request(&method, &url);
    for header in native_headers {
        let name = match copy_utf8(header.name, "fetch header name") {
            Ok(value) if !value.is_empty() && value.len() <= 1024 => value,
            _ => {
                write_native_error(error, error_capacity, "fetch header name is invalid");
                return std::ptr::null_mut();
            }
        };
        let value = match copy_utf8(header.value, "fetch header value") {
            Ok(value) if value.len() <= 16 * 1024 => value,
            _ => {
                write_native_error(error, error_capacity, "fetch header value is invalid");
                return std::ptr::null_mut();
            }
        };
        request = request.set(&name, &value);
    }
    let response = match if body.is_empty() {
        request.call()
    } else {
        request.send_bytes(&body)
    } {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(cause) => {
            write_native_error(error, error_capacity, &format!("fetch failed: {cause}"));
            return std::ptr::null_mut();
        }
    };
    let status = response.status();
    let status_text = response.status_text().to_owned();
    let final_url = response.get_url().to_owned();
    let mut output_headers = Vec::new();
    for name in response.headers_names() {
        for value in response.all(&name) {
            if output_headers.len() >= MAXIMUM_FETCH_HEADERS {
                write_native_error(error, error_capacity, "fetch response has too many headers");
                return std::ptr::null_mut();
            }
            output_headers.push((name.clone(), value.to_owned()));
        }
    }
    let mut output_body = Vec::new();
    if let Err(cause) = response
        .into_reader()
        .take(MAXIMUM_FETCH_BODY_BYTES as u64 + 1)
        .read_to_end(&mut output_body)
    {
        write_native_error(
            error,
            error_capacity,
            &format!("fetch body failed: {cause}"),
        );
        return std::ptr::null_mut();
    }
    if output_body.len() > MAXIMUM_FETCH_BODY_BYTES {
        write_native_error(
            error,
            error_capacity,
            "fetch response body exceeds byte limit",
        );
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(NativeFetchOutput {
        status,
        status_text,
        url: final_url,
        headers: output_headers,
        body: output_body,
    }))
    .cast()
}

/// Returns the status code for a live Fetch output.
///
/// # Safety
/// `output` must be a live pointer returned by `sako_fetch_sync`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_status(output: *const c_void) -> u16 {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }.map_or(0, |output| output.status)
}

/// Borrows the status text for a live Fetch output.
///
/// # Safety
/// `output` must remain live until the returned range is consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_status_text(output: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |output| native_bytes(output.status_text.as_bytes()),
    )
}

/// Borrows the final URL for a live Fetch output.
///
/// # Safety
/// `output` must remain live until the returned range is consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_url(output: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |output| native_bytes(output.url.as_bytes()),
    )
}

/// Returns the bounded response-header count for a live Fetch output.
///
/// # Safety
/// `output` must be a live pointer returned by `sako_fetch_sync`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_header_count(output: *const c_void) -> usize {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }.map_or(0, |output| output.headers.len())
}

/// Borrows one response-header name for a live Fetch output.
///
/// # Safety
/// `output` must remain live and `index` must be below its header count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_header_name(
    output: *const c_void,
    index: usize,
) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }
        .and_then(|output| output.headers.get(index))
        .map_or(
            NativeBytes {
                data: std::ptr::null(),
                length: 0,
            },
            |(name, _)| native_bytes(name.as_bytes()),
        )
}

/// Borrows one response-header value for a live Fetch output.
///
/// # Safety
/// `output` must remain live and `index` must be below its header count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_header_value(
    output: *const c_void,
    index: usize,
) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }
        .and_then(|output| output.headers.get(index))
        .map_or(
            NativeBytes {
                data: std::ptr::null(),
                length: 0,
            },
            |(_, value)| native_bytes(value.as_bytes()),
        )
}

/// Borrows the response body for a live Fetch output.
///
/// # Safety
/// `output` must remain live until the returned range is consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_body(output: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeFetchOutput>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |output| native_bytes(&output.body),
    )
}

/// Deletes one Fetch output owner.
///
/// # Safety
/// `output` must be null or a uniquely owned pointer from `sako_fetch_sync`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_fetch_output_delete(output: *mut c_void) {
    if !output.is_null() {
        // SAFETY: ownership is returned exactly once by the native bridge.
        drop(unsafe { Box::from_raw(output.cast::<NativeFetchOutput>()) });
    }
}

/// Runs one Job Object-owned child and captures bounded output.
///
/// # Safety
/// All `NativeBytes` inputs must describe readable UTF-8 ranges for this call.
/// `arguments` must point to `argument_count` initialized entries. `error`
/// follows the writable buffer contract. The returned pointer must be deleted
/// exactly once with `sako_process_output_delete`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_process_spawn_sync(
    executable: NativeBytes,
    arguments: *const NativeBytes,
    argument_count: usize,
    cwd: NativeBytes,
    verbatim_arguments: c_int,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    if argument_count > MAXIMUM_CHILD_ARGUMENTS || (argument_count != 0 && arguments.is_null()) {
        write_native_error(error, error_capacity, "child argument input is invalid");
        return std::ptr::null_mut();
    }
    let executable = match copy_utf8(executable, "child executable") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) => {
            write_native_error(error, error_capacity, "child executable is empty");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let native_arguments = if argument_count == 0 {
        &[][..]
    } else {
        // SAFETY: the caller promises an initialized array of argument_count entries.
        unsafe { std::slice::from_raw_parts(arguments, argument_count) }
    };
    let mut values = Vec::with_capacity(native_arguments.len());
    for argument in native_arguments {
        match copy_utf8(*argument, "child argument") {
            Ok(value) => values.push(value),
            Err(cause) => {
                write_native_error(error, error_capacity, &cause);
                return std::ptr::null_mut();
            }
        }
    }
    let cwd = match copy_utf8(cwd, "child cwd") {
        Ok(value) => value,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let cwd = (!cwd.is_empty()).then(|| std::path::Path::new(&cwd));
    let output = match spawn_native_with_bounded_output(
        &executable,
        &values,
        cwd,
        MAXIMUM_CHILD_OUTPUT_BYTES,
        verbatim_arguments != 0,
    ) {
        Ok(output) => output,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(NativeProcessOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    }))
    .cast()
}

/// Returns the child exit status from a live native process output.
///
/// # Safety
/// `output` must be a live pointer from `sako_process_spawn_sync`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_process_output_status(output: *const c_void) -> c_int {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeProcessOutput>().as_ref() }.map_or(-1, |output| output.status)
}

/// Borrows captured stdout from a live native process output.
///
/// # Safety
/// `output` must remain live and unaliased by delete while the returned range is used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_process_output_stdout(output: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeProcessOutput>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |output| native_bytes(&output.stdout),
    )
}

/// Borrows captured stderr from a live native process output.
///
/// # Safety
/// `output` must remain live and unaliased by delete while the returned range is used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_process_output_stderr(output: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live output pointer contract.
    unsafe { output.cast::<NativeProcessOutput>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |output| native_bytes(&output.stderr),
    )
}

/// Deletes one native process output owner.
///
/// # Safety
/// `output` must be null or a live uniquely owned pointer from
/// `sako_process_spawn_sync` and cannot be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_process_output_delete(output: *mut c_void) {
    if !output.is_null() {
        // SAFETY: ownership is returned exactly once by the native C++ owner.
        drop(unsafe { Box::from_raw(output.cast::<NativeProcessOutput>()) });
    }
}

/// How the bridge learns what a live child has said. `kind` names the event,
/// `bytes` carries output or an error message, and `status` carries the exit
/// code. Everything the callback receives is borrowed for that one call.
pub type NativeChildEvent = extern "C" fn(*mut c_void, c_int, NativeBytes, c_int);

const CHILD_EVENT_STDOUT: c_int = 0;
const CHILD_EVENT_STDERR: c_int = 1;
const CHILD_EVENT_STDOUT_END: c_int = 2;
const CHILD_EVENT_STDERR_END: c_int = 3;
const CHILD_EVENT_EXITED: c_int = 4;
const CHILD_EVENT_FAILED: c_int = 5;
const CHILD_EVENT_STDIN_FAILED: c_int = 6;
const CHILD_EVENT_STDIN_DRAINED: c_int = 7;

/// Starts a child that keeps running, with its pipes held open.
///
/// This is what `spawn_native_with_bounded_output` is not: nothing here waits
/// for the child. Output arrives through `sako_child_drain` as the child
/// produces it, and `sako_child_write` answers on stdin, which is the only
/// shape a build service or a language server can be spoken to in.
///
/// # Safety
/// All `NativeBytes` inputs must describe readable UTF-8 ranges for this call.
/// `arguments` must point to `argument_count` initialized entries, and
/// `environment` to `environment_count` -- names and values alternating, so
/// the count is even. `error` follows the writable buffer contract. The
/// returned pointer must be deleted exactly once with `sako_child_delete`.
///
/// A non-zero `replace_environment` makes `environment` the child's whole
/// environment rather than a set of additions to this process's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_spawn(
    executable: NativeBytes,
    arguments: *const NativeBytes,
    argument_count: usize,
    cwd: NativeBytes,
    verbatim_arguments: c_int,
    environment: *const NativeBytes,
    environment_count: usize,
    replace_environment: c_int,
    piped_stdin: c_int,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    if argument_count > MAXIMUM_CHILD_ARGUMENTS || (argument_count != 0 && arguments.is_null()) {
        write_native_error(error, error_capacity, "child argument input is invalid");
        return std::ptr::null_mut();
    }
    if !environment_count.is_multiple_of(2)
        || environment_count > 2 * MAXIMUM_CHILD_ENVIRONMENT_VARIABLES
        || (environment_count != 0 && environment.is_null())
    {
        write_native_error(error, error_capacity, "child environment input is invalid");
        return std::ptr::null_mut();
    }
    let executable = match copy_utf8(executable, "child executable") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) => {
            write_native_error(error, error_capacity, "child executable is empty");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let native_arguments = if argument_count == 0 {
        &[][..]
    } else {
        // SAFETY: the caller promises an initialized array of argument_count entries.
        unsafe { std::slice::from_raw_parts(arguments, argument_count) }
    };
    let mut values = Vec::with_capacity(native_arguments.len());
    for argument in native_arguments {
        match copy_utf8(*argument, "child argument") {
            Ok(value) => values.push(value),
            Err(cause) => {
                write_native_error(error, error_capacity, &cause);
                return std::ptr::null_mut();
            }
        }
    }
    let native_environment = if environment_count == 0 {
        &[][..]
    } else {
        // SAFETY: the caller promises an initialized array of environment_count entries.
        unsafe { std::slice::from_raw_parts(environment, environment_count) }
    };
    let mut variables = Vec::with_capacity(native_environment.len() / 2);
    for pair in native_environment.chunks_exact(2) {
        let name = copy_utf8(pair[0], "child environment name");
        let value = copy_utf8(pair[1], "child environment value");
        match (name, value) {
            (Ok(name), Ok(value)) => variables.push((name.into(), value.into())),
            (Err(cause), _) | (_, Err(cause)) => {
                write_native_error(error, error_capacity, &cause);
                return std::ptr::null_mut();
            }
        }
    }
    let cwd = match copy_utf8(cwd, "child cwd") {
        Ok(value) => value,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let child = match AsyncChild::spawn(&AsyncSpawn {
        executable: &executable,
        arguments: &values,
        cwd: (!cwd.is_empty()).then(|| std::path::Path::new(&cwd)),
        environment: &variables,
        replace_environment: replace_environment != 0,
        verbatim_arguments: verbatim_arguments != 0,
        piped_stdin: piped_stdin != 0,
    }) {
        Ok(child) => child,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(child)).cast()
}

/// The operating system identifier of a live child.
///
/// # Safety
/// `child` must be a live pointer from `sako_child_spawn`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_pid(child: *const c_void) -> u32 {
    // SAFETY: the caller upholds the live child pointer contract.
    unsafe { child.cast::<AsyncChild>().as_ref() }.map_or(0, AsyncChild::pid)
}

/// Hands every event the child has queued to `callback`, oldest first, and
/// reports how many there were.
///
/// # Safety
/// `child` must be a live pointer from `sako_child_spawn`. `callback` must
/// remain valid for this call and must not re-enter this child.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_drain(
    child: *const c_void,
    callback: NativeChildEvent,
    user: *mut c_void,
) -> c_int {
    // SAFETY: the caller upholds the live child pointer contract.
    let Some(child) = (unsafe { child.cast::<AsyncChild>().as_ref() }) else {
        return -1;
    };
    let mut delivered: c_int = 0;
    for event in child.drain() {
        let empty = NativeBytes {
            data: std::ptr::null(),
            length: 0,
        };
        match &event {
            ChildEvent::Stdout(chunk) => {
                callback(user, CHILD_EVENT_STDOUT, native_bytes(chunk), 0);
            }
            ChildEvent::Stderr(chunk) => {
                callback(user, CHILD_EVENT_STDERR, native_bytes(chunk), 0);
            }
            ChildEvent::StdoutEnd => callback(user, CHILD_EVENT_STDOUT_END, empty, 0),
            ChildEvent::StderrEnd => callback(user, CHILD_EVENT_STDERR_END, empty, 0),
            ChildEvent::Exited(status) => callback(user, CHILD_EVENT_EXITED, empty, *status),
            ChildEvent::Failed(message) => {
                callback(
                    user,
                    CHILD_EVENT_FAILED,
                    native_bytes(message.as_bytes()),
                    0,
                );
            }
            ChildEvent::StdinFailed(message) => {
                callback(
                    user,
                    CHILD_EVENT_STDIN_FAILED,
                    native_bytes(message.as_bytes()),
                    0,
                );
            }
            ChildEvent::StdinDrained => callback(user, CHILD_EVENT_STDIN_DRAINED, empty, 0),
        }
        delivered = delivered.saturating_add(1);
    }
    delivered
}

/// Queues bytes for the child's stdin. Reports 1 when the caller may keep
/// writing, 0 when it should wait for the queue to drain, and -1 on failure.
///
/// # Safety
/// `child` must be a live pointer from `sako_child_spawn`, and `bytes` must
/// describe a readable range for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_write(
    child: *const c_void,
    bytes: NativeBytes,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    // SAFETY: the caller upholds the live child pointer contract.
    let Some(child) = (unsafe { child.cast::<AsyncChild>().as_ref() }) else {
        write_native_error(error, error_capacity, "child handle is invalid");
        return -1;
    };
    let chunk = match copy_bytes(bytes, "child stdin chunk") {
        Ok(chunk) => chunk,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return -1;
        }
    };
    match child.write_stdin(&chunk) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            -1
        }
    }
}

/// Closes the child's stdin once everything queued has been written.
///
/// # Safety
/// `child` must be a live pointer from `sako_child_spawn`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_close_stdin(child: *const c_void) -> c_int {
    // SAFETY: the caller upholds the live child pointer contract.
    let Some(child) = (unsafe { child.cast::<AsyncChild>().as_ref() }) else {
        return -1;
    };
    child.close_stdin();
    0
}

/// Ends the child and everything it started.
///
/// # Safety
/// `child` must be a live pointer from `sako_child_spawn`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_kill(child: *const c_void, force: c_int) -> c_int {
    // SAFETY: the caller upholds the live child pointer contract.
    let Some(child) = (unsafe { child.cast::<AsyncChild>().as_ref() }) else {
        return -1;
    };
    match child.kill(force != 0) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Deletes one child owner, ending the child if it is still running.
///
/// # Safety
/// `child` must be null or a live uniquely owned pointer from
/// `sako_child_spawn` and cannot be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_child_delete(child: *mut c_void) {
    if !child.is_null() {
        // SAFETY: ownership is returned exactly once by the native bridge.
        drop(unsafe { Box::from_raw(child.cast::<AsyncChild>()) });
    }
}

/// Opens one PostgreSQL connection from a `postgres://` URL.
///
/// The connection blocks: a query occupies the calling thread until the server
/// answers, exactly as `sako_fetch_sync` does. That is a bound worth knowing
/// about rather than a detail -- a server sharing this thread serves nobody
/// while a query is in flight.
///
/// # Safety
/// `url` must describe a readable UTF-8 range for this call, and `error`
/// follows the writable buffer contract. The returned pointer must be deleted
/// exactly once with `sako_postgres_delete`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_connect(
    url: NativeBytes,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    let url = match copy_utf8(url, "PostgreSQL URL") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) => {
            write_native_error(error, error_capacity, "PostgreSQL URL is empty");
            return std::ptr::null_mut();
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let config = match PostgresConfig::from_url(&url) {
        Ok(config) => config,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    match PostgresConnection::connect(&config) {
        Ok(connection) => Box::into_raw(Box::new(connection)).cast(),
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Runs one statement and reads its whole result.
///
/// Parameters are text, and `nulls[index]` being non-zero marks a SQL NULL --
/// which an empty byte range cannot express on its own, an empty string being
/// a perfectly good value.
///
/// # Safety
/// `parameters` must point to `parameter_count` initialized entries and
/// `nulls` to as many bytes. `connection` must be live. The returned pointer
/// must be deleted exactly once with `sako_postgres_result_delete`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_query(
    connection: *mut c_void,
    sql: NativeBytes,
    parameters: *const NativeBytes,
    nulls: *const u8,
    parameter_count: usize,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    // SAFETY: the caller upholds the live connection pointer contract.
    let Some(connection) = (unsafe { connection.cast::<PostgresConnection>().as_mut() }) else {
        write_native_error(error, error_capacity, "the connection handle is invalid");
        return std::ptr::null_mut();
    };
    if parameter_count > MAXIMUM_POSTGRES_PARAMETERS
        || (parameter_count != 0 && (parameters.is_null() || nulls.is_null()))
    {
        write_native_error(error, error_capacity, "query parameter input is invalid");
        return std::ptr::null_mut();
    }
    let sql = match copy_utf8(sql, "SQL statement") {
        Ok(value) => value,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let (native_parameters, native_nulls) = if parameter_count == 0 {
        (&[][..], &[][..])
    } else {
        // SAFETY: the caller promises both arrays hold parameter_count entries.
        unsafe {
            (
                std::slice::from_raw_parts(parameters, parameter_count),
                std::slice::from_raw_parts(nulls, parameter_count),
            )
        }
    };
    let mut values = Vec::with_capacity(parameter_count);
    for (index, parameter) in native_parameters.iter().enumerate() {
        if native_nulls[index] != 0 {
            values.push(None);
            continue;
        }
        match copy_bytes(*parameter, "query parameter") {
            Ok(value) => values.push(Some(value)),
            Err(cause) => {
                write_native_error(error, error_capacity, &cause);
                return std::ptr::null_mut();
            }
        }
    }
    match connection.query(&sql, &values) {
        Ok(result) => Box::into_raw(Box::new(result)).cast(),
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Ends the session tidily. The handle stays valid and every later query on it
/// fails.
///
/// # Safety
/// `connection` must be a live pointer from `sako_postgres_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_close(connection: *mut c_void) {
    // SAFETY: the caller upholds the live connection pointer contract.
    if let Some(connection) = unsafe { connection.cast::<PostgresConnection>().as_mut() } {
        connection.close();
    }
}

/// Deletes one connection owner, ending the session if it is still open.
///
/// # Safety
/// `connection` must be null or a live uniquely owned pointer from
/// `sako_postgres_connect` and cannot be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_delete(connection: *mut c_void) {
    if !connection.is_null() {
        // SAFETY: ownership is returned exactly once by the native bridge.
        drop(unsafe { Box::from_raw(connection.cast::<PostgresConnection>()) });
    }
}

/// A server parameter such as `server_version`, or an empty range.
///
/// # Safety
/// `connection` must be live, and `name` must describe a readable UTF-8 range.
/// The returned range borrows the connection until its next query.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_parameter(
    connection: *const c_void,
    name: NativeBytes,
) -> NativeBytes {
    let empty = NativeBytes {
        data: std::ptr::null(),
        length: 0,
    };
    // SAFETY: the caller upholds the live connection pointer contract.
    let Some(connection) = (unsafe { connection.cast::<PostgresConnection>().as_ref() }) else {
        return empty;
    };
    let Ok(name) = copy_utf8(name, "parameter name") else {
        return empty;
    };
    connection
        .parameters()
        .get(&name)
        .map_or(empty, |value| native_bytes(value.as_bytes()))
}

/// # Safety
/// `result` must be a live pointer from `sako_postgres_query`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_column_count(result: *const c_void) -> usize {
    // SAFETY: the caller upholds the live result pointer contract.
    unsafe { result.cast::<QueryResult>().as_ref() }.map_or(0, |result| result.columns.len())
}

/// # Safety
/// `result` must be live and `index` within its column count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_column_name(
    result: *const c_void,
    index: usize,
) -> NativeBytes {
    // SAFETY: the caller upholds the live result pointer contract.
    unsafe { result.cast::<QueryResult>().as_ref() }
        .and_then(|result| result.columns.get(index))
        .map_or(
            NativeBytes {
                data: std::ptr::null(),
                length: 0,
            },
            |column| native_bytes(column.name.as_bytes()),
        )
}

/// The PostgreSQL type OID of one column, so the caller can decide what the
/// text it is about to read actually means.
///
/// # Safety
/// `result` must be live and `index` within its column count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_column_type(
    result: *const c_void,
    index: usize,
) -> u32 {
    // SAFETY: the caller upholds the live result pointer contract.
    unsafe { result.cast::<QueryResult>().as_ref() }
        .and_then(|result| result.columns.get(index))
        .map_or(0, |column| column.type_oid)
}

/// # Safety
/// `result` must be a live pointer from `sako_postgres_query`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_row_count(result: *const c_void) -> usize {
    // SAFETY: the caller upholds the live result pointer contract.
    unsafe { result.cast::<QueryResult>().as_ref() }.map_or(0, |result| result.rows.len())
}

/// One value, in the server's own text form. `is_null` reports a SQL NULL,
/// which an empty range cannot be told apart from an empty string.
///
/// # Safety
/// `result` must be live, the indices within range, and `is_null` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_value(
    result: *const c_void,
    row: usize,
    column: usize,
    is_null: *mut c_int,
) -> NativeBytes {
    let empty = NativeBytes {
        data: std::ptr::null(),
        length: 0,
    };
    if !is_null.is_null() {
        // SAFETY: the caller promises a writable flag.
        unsafe { *is_null = 1 };
    }
    // SAFETY: the caller upholds the live result pointer contract.
    let Some(result) = (unsafe { result.cast::<QueryResult>().as_ref() }) else {
        return empty;
    };
    let Some(value) = result.rows.get(row).and_then(|row| row.get(column)) else {
        return empty;
    };
    match value {
        Some(bytes) => {
            if !is_null.is_null() {
                // SAFETY: the caller promises a writable flag.
                unsafe { *is_null = 0 };
            }
            native_bytes(bytes)
        }
        None => empty,
    }
}

/// The command tag, such as `SELECT 3` or `INSERT 0 1`.
///
/// # Safety
/// `result` must be a live pointer from `sako_postgres_query`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_command(result: *const c_void) -> NativeBytes {
    // SAFETY: the caller upholds the live result pointer contract.
    unsafe { result.cast::<QueryResult>().as_ref() }.map_or(
        NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        |result| native_bytes(result.command.as_bytes()),
    )
}

/// # Safety
/// `result` must be a live pointer from `sako_postgres_query`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_affected(result: *const c_void) -> u64 {
    // SAFETY: the caller upholds the live result pointer contract.
    unsafe { result.cast::<QueryResult>().as_ref() }.map_or(0, |result| result.affected_rows)
}

/// # Safety
/// `result` must be null or a live uniquely owned pointer from
/// `sako_postgres_query` and cannot be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_postgres_result_delete(result: *mut c_void) {
    if !result.is_null() {
        // SAFETY: ownership is returned exactly once by the native bridge.
        drop(unsafe { Box::from_raw(result.cast::<QueryResult>()) });
    }
}

/// The counter every live child bumps when it has something to say. Read it
/// before draining, hand it back to `sako_child_wait_activity` afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn sako_child_activity_tick() -> u64 {
    activity_tick()
}

/// Blocks until some child's queue changes past `since`, or the timeout runs
/// out, and reports the counter it saw.
#[unsafe(no_mangle)]
pub extern "C" fn sako_child_wait_activity(since: u64, timeout_milliseconds: u32) -> u64 {
    wait_for_activity(
        since,
        Duration::from_millis(u64::from(timeout_milliseconds)),
    )
}

/// Resolves a bounded set of IP addresses for the JavaScript DNS compatibility layer.
///
/// # Safety
/// `host` must describe a readable UTF-8 byte range. `output` and `error` must
/// be null or point to their declared writable capacities for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_dns_resolve(
    host: NativeBytes,
    family: c_int,
    output: *mut c_char,
    output_capacity: usize,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    if output.is_null() || output_capacity == 0 {
        write_native_error(error, error_capacity, "DNS output buffer is invalid");
        return -1;
    }
    if !matches!(family, 0 | 4 | 6) {
        write_native_error(error, error_capacity, "DNS family must be 0, 4, or 6");
        return -1;
    }
    let host = match copy_utf8(host, "DNS hostname") {
        Ok(host) if !host.is_empty() => host,
        Ok(_) => {
            write_native_error(error, error_capacity, "DNS hostname is empty");
            return -1;
        }
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return -1;
        }
    };
    let addresses = match resolve_host(&host, 0, MAXIMUM_DNS_RESULTS) {
        Ok(addresses) => addresses,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return -1;
        }
    };
    let mut values = addresses
        .into_iter()
        .filter(|address| family == 0 || family == if address.is_ipv4() { 4 } else { 6 })
        .map(|address| address.ip().to_string())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    if values.is_empty() {
        write_native_error(
            error,
            error_capacity,
            "DNS lookup returned no matching addresses",
        );
        return -1;
    }
    let text = values.join("\n");
    if text.len() >= output_capacity {
        write_native_error(error, error_capacity, "DNS results exceed output buffer");
        return -1;
    }
    // SAFETY: output was validated and the length is strictly below its capacity.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), output.cast(), text.len());
        *output.add(text.len()) = 0;
    }
    c_int::try_from(values.len()).unwrap_or(c_int::MAX)
}

/// Returns 0 when `response` was filled, 2 when the answer is deferred and
/// will arrive through `sako_http_server_respond` quoting `ticket`, and
/// anything else on failure.
type NativeHttpHandler = unsafe extern "C" fn(
    context: *mut c_void,
    ticket: u64,
    method: NativeBytes,
    target: NativeBytes,
    body: NativeBytes,
    headers: *const NativeHeader,
    header_count: usize,
    response: *mut NativeHttpResponse,
) -> c_int;

/// The status a handler returns to keep a request and answer it later.
const NATIVE_HTTP_DEFERRED: c_int = 2;

struct NativeHttpServer {
    server: HttpServer,
}

/// Creates an HTTP server owned by the native V8 bridge.
///
/// # Safety
/// `output_port` must be writable, and `error` must be either null or point to
/// `error_capacity` writable bytes. The returned pointer must be deleted once
/// with `sako_http_server_delete` and used only on its creating thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_new(
    port: u16,
    output_port: *mut u16,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    if output_port.is_null() {
        write_native_error(error, error_capacity, "HTTP output port is null");
        return std::ptr::null_mut();
    }
    let server = match HttpServer::bind(("127.0.0.1", port), HttpServerConfig::default()) {
        Ok(server) => server,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    let local_port = match server.local_addr() {
        Ok(address) => address.port(),
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    // SAFETY: output_port was validated and belongs to the synchronous caller.
    unsafe { *output_port = local_port };
    Box::into_raw(Box::new(NativeHttpServer { server })).cast()
}

/// Creates a TLS-enabled HTTP server owned by the native V8 bridge.
///
/// # Safety
///
/// Certificate/key ranges must be readable for this call. Output and error
/// pointers follow `sako_http_server_new`; the returned owner uses the same
/// tick, close, stats, and delete functions as a plaintext HTTP server.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_https_server_new(
    port: u16,
    certificate: NativeBytes,
    private_key: NativeBytes,
    output_port: *mut u16,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    if output_port.is_null() || certificate.length > 1024 * 1024 || private_key.length > 1024 * 1024
    {
        write_native_error(error, error_capacity, "HTTPS certificate input is invalid");
        return std::ptr::null_mut();
    }
    let certificate = match copy_bytes(certificate, "HTTPS certificate") {
        Ok(value) => value,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let private_key = match copy_bytes(private_key, "HTTPS private key") {
        Ok(value) => value,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause);
            return std::ptr::null_mut();
        }
    };
    let server = match HttpServer::bind_tls(
        ("127.0.0.1", port),
        HttpServerConfig::default(),
        &certificate,
        &private_key,
    ) {
        Ok(server) => server,
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    let local_port = match server.local_addr() {
        Ok(address) => address.port(),
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            return std::ptr::null_mut();
        }
    };
    // SAFETY: output_port was validated and belongs to the synchronous caller.
    unsafe { *output_port = local_port };
    Box::into_raw(Box::new(NativeHttpServer { server })).cast()
}

/// Polls one bounded batch of work for an HTTP server.
///
/// # Safety
/// `server` must be a live pointer returned by `sako_http_server_new` and may
/// not be aliased by another tick/delete call. `handler` and `context` must
/// remain valid for every synchronous callback. `error` follows the buffer
/// contract documented by `sako_http_server_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_tick(
    server: *mut c_void,
    handler: Option<NativeHttpHandler>,
    context: *mut c_void,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    let Some(handler) = handler else {
        write_native_error(error, error_capacity, "HTTP handler is null");
        return -1;
    };
    // SAFETY: the C++ owner passes a pointer created by sako_http_server_new and
    // serializes tick/delete calls on the isolate thread.
    let Some(server) = (unsafe { server.cast::<NativeHttpServer>().as_mut() }) else {
        write_native_error(error, error_capacity, "HTTP server is null");
        return -1;
    };
    let result =
        server.server.tick_deferrable(|request, body, ticket| {
            match dispatch_native_http(handler, context, request, body, ticket) {
                Ok(outcome) => outcome,
                Err(message) => Some(HttpResponse {
                    status: 500,
                    reason: "Internal Server Error".into(),
                    headers: vec![("Connection".into(), "close".into())],
                    body: message.into_bytes(),
                }),
            }
        });
    match result {
        Ok(handled) => c_int::try_from(handled).unwrap_or(c_int::MAX),
        Err(cause) => {
            write_native_error(error, error_capacity, &cause.to_string());
            -1
        }
    }
}

/// Blocks until an HTTP server has socket activity or `timeout_milliseconds`
/// elapses, returning the number of completions applied.
///
/// The event loop calls this instead of sleeping between ticks so an arriving
/// request wakes the runtime immediately.
///
/// # Safety
/// `server` must be a live pointer returned by `sako_http_server_new` and may
/// not be aliased by another tick/wait/delete call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_wait(
    server: *mut c_void,
    timeout_milliseconds: u32,
) -> c_int {
    // SAFETY: the caller upholds the live, uniquely borrowed server contract.
    let Some(server) = (unsafe { server.cast::<NativeHttpServer>().as_mut() }) else {
        return -1;
    };
    match server.server.wait(std::time::Duration::from_millis(
        timeout_milliseconds.into(),
    )) {
        Ok(applied) => c_int::try_from(applied).unwrap_or(c_int::MAX),
        Err(_) => -1,
    }
}

/// Deletes an HTTP server returned by `sako_http_server_new`.
///
/// # Safety
/// `server` must be null or a live, uniquely owned pointer from
/// `sako_http_server_new`, and it must not be used again after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_delete(server: *mut c_void) {
    if !server.is_null() {
        // SAFETY: ownership is returned exactly once by the native C++ owner.
        drop(unsafe { Box::from_raw(server.cast::<NativeHttpServer>()) });
    }
}

/// Stops accepting connections and begins graceful HTTP connection shutdown.
///
/// # Safety
/// `server` must be a live, uniquely borrowed pointer from
/// `sako_http_server_new` and must not be in a concurrent tick/delete call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_close(server: *mut c_void) -> c_int {
    // SAFETY: the caller upholds the live, uniquely borrowed server contract.
    let Some(server) = (unsafe { server.cast::<NativeHttpServer>().as_mut() }) else {
        return 1;
    };
    server.server.close();
    0
}

/// Reads live connection and overload counters from an HTTP server.
///
/// # Safety
/// `server` must be a live, uniquely borrowed pointer from
/// `sako_http_server_new`; both output pointers must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_stats(
    server: *mut c_void,
    connections: *mut u64,
    rejected_connections: *mut u64,
) -> c_int {
    if connections.is_null() || rejected_connections.is_null() {
        return 1;
    }
    // SAFETY: the caller upholds the live, unique server pointer contract.
    let Some(server) = (unsafe { server.cast::<NativeHttpServer>().as_ref() }) else {
        return 1;
    };
    // SAFETY: both output pointers were validated above.
    unsafe {
        *connections = server.server.connection_count() as u64;
        *rejected_connections = server.server.rejected_connections();
    }
    0
}

fn dispatch_native_http(
    handler: NativeHttpHandler,
    context: *mut c_void,
    request: &RequestHead<'_>,
    body: &[u8],
    ticket: u64,
) -> Result<Option<HttpResponse>, String> {
    let request_headers = request
        .headers()
        .map(|(name, value)| NativeHeader {
            name: native_bytes(name),
            value: native_bytes(value),
        })
        .collect::<Vec<_>>();
    let mut response = NativeHttpResponse {
        status: 500,
        reason: NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
        headers: std::ptr::null(),
        header_count: 0,
        body: NativeBytes {
            data: std::ptr::null(),
            length: 0,
        },
    };
    // SAFETY: all request slices remain borrowed for this synchronous callback;
    // the callback's response slices are copied before this function returns.
    let status = unsafe {
        handler(
            context,
            ticket,
            native_bytes(request.method()),
            native_bytes(request.target()),
            native_bytes(body),
            request_headers.as_ptr(),
            request_headers.len(),
            &mut response,
        )
    };
    if status == NATIVE_HTTP_DEFERRED {
        return Ok(None);
    }
    if status != 0 {
        return Err("JavaScript HTTP handler failed".into());
    }
    // SAFETY: every slice the callback handed back is live until this returns.
    collect_native_response(
        response.status,
        response.reason,
        response.headers,
        response.header_count,
        response.body,
    )
    .map(Some)
}

/// Reads the response a caller assembled in C into the owned form the server
/// encodes from. Shared by the synchronous dispatch path and the deferred one.
fn collect_native_response(
    status: u16,
    reason: NativeBytes,
    headers: *const NativeHeader,
    header_count: usize,
    body: NativeBytes,
) -> Result<HttpResponse, String> {
    if header_count > 128 {
        return Err("HTTP response exceeds header count limit".into());
    }
    let reason = copy_utf8(reason, "HTTP reason")?;
    let body = copy_bytes(body, "HTTP body")?;
    let native = if header_count == 0 {
        &[][..]
    } else {
        if headers.is_null() {
            return Err("HTTP response header pointer is null".into());
        }
        // SAFETY: the caller promises the array is live for this call.
        unsafe { std::slice::from_raw_parts(headers, header_count) }
    };
    let mut collected = Vec::with_capacity(native.len());
    for header in native {
        let name = copy_utf8(header.name, "HTTP header name")?;
        let value = copy_utf8(header.value, "HTTP header value")?;
        if name.eq_ignore_ascii_case("content-length") {
            if value.parse::<usize>().ok() != Some(body.len()) {
                return Err("HTTP Content-Length does not match response body".into());
            }
            continue;
        }
        collected.push((name, value));
    }
    Ok(HttpResponse {
        status,
        reason,
        headers: collected,
        body,
    })
}

/// Delivers a response for a request the handler deferred.
///
/// Returns 0 when the answer reached the connection, 1 when the ticket is no
/// longer owed -- the client hung up while the handler was working, which is
/// ordinary -- and -1 on a malformed response.
///
/// # Safety
/// `server` must be a live pointer from `sako_http_server_new` that is not in
/// a concurrent tick/delete call, and the response slices must be readable for
/// the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_http_server_respond(
    server: *mut c_void,
    ticket: u64,
    status: u16,
    reason: NativeBytes,
    headers: *const NativeHeader,
    header_count: usize,
    body: NativeBytes,
) -> c_int {
    // SAFETY: the caller upholds the live, uniquely borrowed server contract.
    let Some(server) = (unsafe { server.cast::<NativeHttpServer>().as_mut() }) else {
        return -1;
    };
    let Ok(response) = collect_native_response(status, reason, headers, header_count, body) else {
        return -1;
    };
    match server.server.respond(ticket, response) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(_) => -1,
    }
}

fn native_bytes(bytes: &[u8]) -> NativeBytes {
    NativeBytes {
        data: bytes.as_ptr(),
        length: bytes.len(),
    }
}

fn copy_bytes(value: NativeBytes, label: &str) -> Result<Vec<u8>, String> {
    if value.length == 0 {
        return Ok(Vec::new());
    }
    if value.data.is_null() {
        return Err(format!("{label} pointer is null"));
    }
    // SAFETY: the native callback promises a readable range for the duration of dispatch.
    Ok(unsafe { std::slice::from_raw_parts(value.data, value.length) }.to_vec())
}

fn copy_utf8(value: NativeBytes, label: &str) -> Result<String, String> {
    let bytes = copy_bytes(value, label)?;
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|_| format!("{label} is not UTF-8"))
}

/// Digest algorithms the runtime can compute, as small integers so the
/// boundary with the C++ bridge carries no string and no allocation. The
/// values are mirrored by `kHash*` in bridge.cc.
pub const SAKO_HASH_SHA1: u32 = 1;
pub const SAKO_HASH_SHA256: u32 = 2;
pub const SAKO_HASH_SHA512: u32 = 3;

/// Largest digest any supported algorithm produces, and so the buffer size a
/// caller has to provide.
pub const SAKO_HASH_MAXIMUM_BYTES: usize = 64;

/// Computes a digest of `bytes` and writes it to `digest`, returning how many
/// bytes were written, or 0 when `algorithm` is not one of the constants
/// above.
///
/// Runs identically on every platform (RustCrypto), which removes the need for
/// a native crypto API -- BCrypt on Windows, OpenSSL/libcrypto on Linux --
/// from the C++ bridge entirely.
///
/// SHA-256 is here because it is what real toolchains reach for: TypeScript's
/// build mode hashes every source file with it, and refused to run at all
/// while SHA-1 was the only algorithm on offer.
///
/// # Safety
/// `bytes` must describe a readable range for this call. `digest` must be
/// writable for at least `SAKO_HASH_MAXIMUM_BYTES` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sako_hash(algorithm: u32, bytes: NativeBytes, digest: *mut u8) -> usize {
    use sha1::Digest as _;
    let input = if bytes.length == 0 {
        &[][..]
    } else {
        // SAFETY: the caller promises a readable range for this call.
        unsafe { std::slice::from_raw_parts(bytes.data, bytes.length) }
    };
    let output: Vec<u8> = match algorithm {
        SAKO_HASH_SHA1 => sha1::Sha1::digest(input).to_vec(),
        SAKO_HASH_SHA256 => sha2::Sha256::digest(input).to_vec(),
        SAKO_HASH_SHA512 => sha2::Sha512::digest(input).to_vec(),
        _ => return 0,
    };
    debug_assert!(output.len() <= SAKO_HASH_MAXIMUM_BYTES);
    // SAFETY: the caller promises `digest` is writable for the maximum digest
    // size, which every branch above stays within.
    unsafe { std::ptr::copy_nonoverlapping(output.as_ptr(), digest, output.len()) };
    output.len()
}

fn write_native_error(output: *mut c_char, capacity: usize, message: &str) {
    if output.is_null() || capacity == 0 {
        return;
    }
    let length = message.len().min(capacity - 1);
    // SAFETY: the caller supplies a writable buffer of capacity bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(message.as_ptr(), output.cast(), length);
        *output.add(length) = 0;
    }
}

unsafe extern "C" {
    fn sako_v8_runtime_new(
        executable_path: *const c_char,
        icu_data_path: *const c_char,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn sako_v8_runtime_execute(
        runtime: *mut c_void,
        source: *const u8,
        source_len: usize,
        resource_name: *const u8,
        resource_name_len: usize,
        argument_bytes: *const *const u8,
        argument_lengths: *const usize,
        argument_count: usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn sako_v8_runtime_memory_stats(
        runtime: *mut c_void,
        heap_used: *mut u64,
        heap_committed: *mut u64,
        heap_limit: *mut u64,
        persistent_handles: *mut u64,
        timers: *mut u64,
        external_memory: *mut u64,
        http_servers: *mut u64,
        sockets: *mut u64,
        http_buffer_bytes: *mut u64,
        native_memory_bytes: *mut u64,
        module_cache_entries: *mut u64,
        queued_operations: *mut u64,
    ) -> c_int;
    fn sako_v8_runtime_execute_module(
        runtime: *mut c_void,
        path: *const u8,
        path_length: usize,
        argument_bytes: *const *const u8,
        argument_lengths: *const usize,
        argument_count: usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn sako_v8_runtime_execute_commonjs(
        runtime: *mut c_void,
        path: *const u8,
        path_length: usize,
        argument_bytes: *const *const u8,
        argument_lengths: *const usize,
        argument_count: usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn sako_v8_runtime_delete(runtime: *mut c_void);
    fn sako_v8_runtime_abandon(runtime: *mut c_void);
    fn sako_v8_restore_terminal();
    fn sako_v8_remember_output_code_page(code_page: u32);
    fn sako_perf_enable();
    fn sako_perf_mark(name: *const c_char);
    fn sako_perf_report();
}

/// Starts recording startup phase timings for `--perf-breakdown`.
///
/// Recording stays off until this is called, so a normal run pays only one
/// branch on a process-wide flag at each phase boundary.
pub fn perf_enable() {
    // SAFETY: the bridge only touches its own process-wide recording state.
    unsafe { sako_perf_enable() };
}

/// Records one startup phase boundary. `name` must be a static C string.
pub fn perf_mark(name: &'static CStr) {
    // SAFETY: the pointer is a live static C string and the bridge only reads
    // it while recording, which ends before the process exits.
    unsafe { sako_perf_mark(name.as_ptr()) };
}

/// Writes the recorded startup breakdown to stderr. A no-op when disabled.
pub fn perf_report() {
    // SAFETY: the bridge writes its own recorded state to the error handle.
    unsafe { sako_perf_report() };
}

/// Restores line editing and echo if a script put the terminal into raw mode.
///
/// Belongs on every exit path. A prompt that ends by returning rather than by
/// calling `process.exit` leaves through the CLI, which terminates the process
/// outright -- so without this the shell that comes next inherits a terminal
/// with no echo. A no-op when raw mode was never entered.
/// Tells the runtime which console output code page to put back.
///
/// `process.exit` terminates rather than returning, so the CLI's own restore
/// never runs for a script that calls it -- and the console was left in UTF-8
/// for whatever the user typed next. Passing the value here lets the exit path
/// undo it. A no-op off Windows, where there is no per-console encoding.
pub fn remember_output_code_page(code_page: Option<u32>) {
    // SAFETY: the bridge only stores the value and reads it back on exit.
    unsafe { sako_v8_remember_output_code_page(code_page.unwrap_or(0)) };
}

pub fn restore_terminal() {
    // SAFETY: the bridge only rewrites the console or termios state it saved,
    // and does nothing at all when it saved none.
    unsafe { sako_v8_restore_terminal() };
}

#[derive(Debug)]
pub struct V8Error(String);

impl fmt::Display for V8Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for V8Error {}

pub struct Runtime {
    raw: NonNull<c_void>,
    // A V8 isolate is thread-affine. Keep Runtime !Send and !Sync.
    _thread_affinity: PhantomData<Rc<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryStats {
    pub heap_used: u64,
    pub heap_committed: u64,
    pub heap_limit: u64,
    pub persistent_handles: u64,
    pub timers: u64,
    pub external_memory: u64,
    pub http_servers: u64,
    pub sockets: u64,
    pub http_buffer_bytes: u64,
    pub native_memory_bytes: u64,
    pub module_cache_entries: u64,
    pub queued_operations: u64,
}

impl Runtime {
    pub fn new() -> Result<Self, V8Error> {
        Self::initialize()
    }

    fn initialize() -> Result<Self, V8Error> {
        let executable = std::env::current_exe()
            .map_err(|error| V8Error(format!("cannot locate the Sako executable: {error}")))?;
        let icu_data = PathBuf::from(env!("SAKO_V8_ROOT"))
            .join("bin")
            .join("icudtl.dat");
        let executable = path_to_c_string(executable)?;
        // An empty path tells the bridge to skip ICU-default-location
        // initialization: a build with icu_use_data_file=false (the staged
        // Linux V8 archive) has no external data file at all and already
        // carries its ICU data compiled in.
        let icu_data = if icu_data.is_file() {
            path_to_c_string(icu_data)?
        } else {
            CString::new("").unwrap()
        };
        let mut error = vec![0_u8; ERROR_BUFFER_CAPACITY];

        // SAFETY: Both C strings and the writable error buffer remain valid for the call.
        let raw = unsafe {
            sako_v8_runtime_new(
                executable.as_ptr(),
                icu_data.as_ptr(),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        let raw = NonNull::new(raw).ok_or_else(|| error_from_buffer(&error))?;

        Ok(Self {
            raw,
            _thread_affinity: PhantomData,
        })
    }

    pub fn execute(&mut self, source: &str, resource_name: &str) -> Result<(), V8Error> {
        self.execute_with_args(source, resource_name, &[])
    }

    pub fn execute_with_args(
        &mut self,
        source: &str,
        resource_name: &str,
        arguments: &[String],
    ) -> Result<(), V8Error> {
        if source.len() > i32::MAX as usize {
            return Err(V8Error("JavaScript source exceeds V8 string limits".into()));
        }
        if resource_name.len() > i32::MAX as usize {
            return Err(V8Error("script path exceeds V8 string limits".into()));
        }
        if arguments.len() > i32::MAX as usize
            || arguments
                .iter()
                .any(|argument| argument.len() > i32::MAX as usize)
        {
            return Err(V8Error("script arguments exceed V8 string limits".into()));
        }
        let argument_bytes: Vec<*const u8> =
            arguments.iter().map(|argument| argument.as_ptr()).collect();
        let argument_lengths: Vec<usize> =
            arguments.iter().map(|argument| argument.len()).collect();
        let mut error = vec![0_u8; ERROR_BUFFER_CAPACITY];
        // SAFETY: Runtime exclusively owns a live native runtime. The byte slices and
        // writable error buffer remain valid for the duration of this synchronous call.
        let status = unsafe {
            sako_v8_runtime_execute(
                self.raw.as_ptr(),
                source.as_ptr(),
                source.len(),
                resource_name.as_ptr(),
                resource_name.len(),
                argument_bytes.as_ptr(),
                argument_lengths.as_ptr(),
                arguments.len(),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(error_from_buffer(&error))
        }
    }

    pub fn memory_stats(&self) -> MemoryStats {
        let mut stats = MemoryStats {
            heap_used: 0,
            heap_committed: 0,
            heap_limit: 0,
            persistent_handles: 0,
            timers: 0,
            external_memory: 0,
            http_servers: 0,
            sockets: 0,
            http_buffer_bytes: 0,
            native_memory_bytes: 0,
            module_cache_entries: 0,
            queued_operations: 0,
        };
        // SAFETY: raw is live and each output pointer refers to initialized writable storage.
        let status = unsafe {
            sako_v8_runtime_memory_stats(
                self.raw.as_ptr(),
                &mut stats.heap_used,
                &mut stats.heap_committed,
                &mut stats.heap_limit,
                &mut stats.persistent_handles,
                &mut stats.timers,
                &mut stats.external_memory,
                &mut stats.http_servers,
                &mut stats.sockets,
                &mut stats.http_buffer_bytes,
                &mut stats.native_memory_bytes,
                &mut stats.module_cache_entries,
                &mut stats.queued_operations,
            )
        };
        debug_assert_eq!(status, 0);
        stats
    }

    pub fn execute_module_with_args(
        &mut self,
        path: &str,
        arguments: &[String],
    ) -> Result<(), V8Error> {
        if path.len() > i32::MAX as usize {
            return Err(V8Error("module path exceeds V8 string limits".into()));
        }
        if arguments.len() > i32::MAX as usize
            || arguments
                .iter()
                .any(|argument| argument.len() > i32::MAX as usize)
        {
            return Err(V8Error("script arguments exceed V8 string limits".into()));
        }
        let argument_bytes: Vec<*const u8> =
            arguments.iter().map(|argument| argument.as_ptr()).collect();
        let argument_lengths: Vec<usize> =
            arguments.iter().map(|argument| argument.len()).collect();
        let mut error = vec![0_u8; ERROR_BUFFER_CAPACITY];
        // SAFETY: Runtime owns a live native runtime. The path, argument slices,
        // pointer arrays, and writable error buffer remain valid for this call.
        let status = unsafe {
            sako_v8_runtime_execute_module(
                self.raw.as_ptr(),
                path.as_ptr(),
                path.len(),
                argument_bytes.as_ptr(),
                argument_lengths.as_ptr(),
                arguments.len(),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(error_from_buffer(&error))
        }
    }

    pub fn execute_commonjs_with_args(
        &mut self,
        path: &str,
        arguments: &[String],
    ) -> Result<(), V8Error> {
        if path.len() > i32::MAX as usize {
            return Err(V8Error("CommonJS path exceeds V8 string limits".into()));
        }
        if arguments.len() > i32::MAX as usize
            || arguments
                .iter()
                .any(|argument| argument.len() > i32::MAX as usize)
        {
            return Err(V8Error("script arguments exceed V8 string limits".into()));
        }
        let argument_bytes: Vec<*const u8> =
            arguments.iter().map(|argument| argument.as_ptr()).collect();
        let argument_lengths: Vec<usize> =
            arguments.iter().map(|argument| argument.len()).collect();
        let mut error = vec![0_u8; ERROR_BUFFER_CAPACITY];
        // SAFETY: Runtime owns a live native runtime. The path, argument slices,
        // pointer arrays, and writable error buffer remain valid for this call.
        let status = unsafe {
            sako_v8_runtime_execute_commonjs(
                self.raw.as_ptr(),
                path.as_ptr(),
                path.len(),
                argument_bytes.as_ptr(),
                argument_lengths.as_ptr(),
                arguments.len(),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(error_from_buffer(&error))
        }
    }
}

impl Runtime {
    /// Abandons the isolate instead of tearing it down.
    ///
    /// Disposing an isolate walks the whole heap, and the platform's worker
    /// threads go with it -- work whose only product is memory the kernel
    /// unmaps a moment later anyway. A caller that exits the process next has
    /// no use for any of it, and on a hello-world run that walk is a
    /// measurable slice of the total.
    ///
    /// Safe because nothing the runtime writes is buffered on the C++ side:
    /// output goes straight to the standard handles and open file descriptors
    /// are closed by the kernel at exit. A caller that keeps running must let
    /// the Runtime drop normally instead.
    pub fn abandon(self) {
        // SAFETY: raw was created by sako_v8_runtime_new and is still live;
        // forgetting `self` afterwards is what keeps the matching delete from
        // ever running.
        unsafe { sako_v8_runtime_abandon(self.raw.as_ptr()) };
        std::mem::forget(self);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: raw was created by sako_v8_runtime_new and is deleted exactly once.
        unsafe { sako_v8_runtime_delete(self.raw.as_ptr()) };
    }
}

fn path_to_c_string(path: PathBuf) -> Result<CString, V8Error> {
    CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| V8Error(format!("path contains an embedded NUL: {}", path.display())))
}

fn error_from_buffer(buffer: &[u8]) -> V8Error {
    // SAFETY: The buffer is zero-initialized and the native bridge always NUL-terminates it.
    let message = unsafe { CStr::from_ptr(buffer.as_ptr().cast()) }
        .to_string_lossy()
        .into_owned();
    if message.is_empty() {
        V8Error("V8 operation failed without an error message".into())
    } else {
        V8Error(message)
    }
}
