// SPDX-License-Identifier: BSD-3-Clause

use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};

const ERROR_BUFFER_CAPACITY: usize = 16 * 1024;
const RUNTIME_NEVER_STARTED: u8 = 0;
const RUNTIME_ACTIVE: u8 = 1;
const RUNTIME_DISPOSED: u8 = 2;
static RUNTIME_STATE: AtomicU8 = AtomicU8::new(RUNTIME_NEVER_STARTED);

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
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn sako_v8_runtime_delete(runtime: *mut c_void);
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

impl Runtime {
    pub fn new() -> Result<Self, V8Error> {
        RUNTIME_STATE
            .compare_exchange(
                RUNTIME_NEVER_STARTED,
                RUNTIME_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| V8Error("V8 can only be initialized once per process".into()))?;

        match Self::initialize() {
            Ok(runtime) => Ok(runtime),
            Err(error) => {
                RUNTIME_STATE.store(RUNTIME_DISPOSED, Ordering::Release);
                Err(error)
            }
        }
    }

    fn initialize() -> Result<Self, V8Error> {
        let executable = std::env::current_exe()
            .map_err(|error| V8Error(format!("cannot locate the Sako executable: {error}")))?;
        let icu_data = PathBuf::from(env!("SAKO_V8_ROOT"))
            .join("bin")
            .join("icudtl.dat");
        let executable = path_to_c_string(executable)?;
        let icu_data = path_to_c_string(icu_data)?;
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
        if source.len() > i32::MAX as usize {
            return Err(V8Error("JavaScript source exceeds V8 string limits".into()));
        }
        if resource_name.len() > i32::MAX as usize {
            return Err(V8Error("script path exceeds V8 string limits".into()));
        }
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

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: raw was created by sako_v8_runtime_new and is deleted exactly once.
        unsafe { sako_v8_runtime_delete(self.raw.as_ptr()) };
        RUNTIME_STATE.store(RUNTIME_DISPOSED, Ordering::Release);
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
