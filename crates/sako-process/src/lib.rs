// SPDX-License-Identifier: BSD-3-Clause

#[cfg(not(any(windows, unix)))]
compile_error!("sako-process currently supports only Windows and Unix-like platforms");

use std::io::{self, Read};
use std::process::ExitStatus;

#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Reads at most `maximum_bytes` from `stream`, one extra probe byte beyond
/// that limit so callers can detect and reject an oversized stream instead of
/// silently truncating it.
fn read_bounded(mut stream: impl Read, maximum_bytes: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .by_ref()
        .take(maximum_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(windows)]
mod windows_impl {
    use super::{BoundedOutput, read_bounded};
    use std::collections::BTreeMap;
    use std::env;
    use std::ffi::{OsStr, OsString, c_void};
    use std::io;
    use std::io::Read as _;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::ExitStatusExt as _;
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const ERROR_BROKEN_PIPE: i32 = 109;
    static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(1);

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            information_class: i32,
            information: *const c_void,
            information_length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    }

    pub struct ChildJob {
        // The job must drop after the Child so an ordinary completed child can be
        // reaped before KILL_ON_JOB_CLOSE tears down any remaining descendants.
        child: Child,
        job: OwnedHandle,
    }

    struct ProcThreadAttributeList {
        storage: Vec<usize>,
        initialized: bool,
    }

    impl ProcThreadAttributeList {
        fn with_handle_list(handles: &[HANDLE]) -> io::Result<Self> {
            let mut bytes = 0;
            // SAFETY: a null first call is the documented size query.
            unsafe {
                InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes);
            }
            if bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            let words = bytes.div_ceil(size_of::<usize>());
            let mut list = Self {
                storage: vec![0; words],
                initialized: false,
            };
            // SAFETY: storage is pointer-aligned and remains stable through process
            // creation; its byte capacity is at least the queried size.
            if unsafe { InitializeProcThreadAttributeList(list.pointer(), 1, 0, &mut bytes) } == 0 {
                return Err(io::Error::last_os_error());
            }
            list.initialized = true;
            // SAFETY: both arrays remain live until after CreateProcessW returns.
            if unsafe {
                UpdateProcThreadAttribute(
                    list.pointer(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    size_of_val(handles),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(list)
        }

        fn pointer(&mut self) -> *mut c_void {
            self.storage.as_mut_ptr().cast()
        }
    }

    impl Drop for ProcThreadAttributeList {
        fn drop(&mut self) {
            if self.initialized {
                // SAFETY: successful initialization owns exactly one attribute list.
                unsafe { DeleteProcThreadAttributeList(self.pointer()) };
            }
        }
    }

    /// Launches a child through the Windows process API with only its three
    /// standard handles inherited. Output is carried by uniquely named byte-mode
    /// pipes and all descendants are owned by a kill-on-close Job Object.
    pub fn spawn_native_with_bounded_output(
        executable: &str,
        arguments: &[String],
        cwd: Option<&Path>,
        maximum_output_bytes: usize,
        verbatim_arguments: bool,
    ) -> io::Result<BoundedOutput> {
        spawn_native_with_bounded_output_in(
            executable,
            arguments,
            cwd,
            maximum_output_bytes,
            verbatim_arguments,
            &[],
        )
    }

    /// As above, with `environment` layered over the inherited variables.
    ///
    /// Package lifecycle scripts need this: npm runs them with
    /// `node_modules/.bin` on PATH and a set of `npm_*` variables describing
    /// the package, and published scripts are written against both.
    pub fn spawn_native_with_bounded_output_in(
        executable: &str,
        arguments: &[String],
        cwd: Option<&Path>,
        maximum_output_bytes: usize,
        verbatim_arguments: bool,
        environment: &[(OsString, OsString)],
    ) -> io::Result<BoundedOutput> {
        if executable.is_empty() || executable.contains('\0') || maximum_output_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native child input is invalid",
            ));
        }
        if arguments.iter().any(|argument| argument.contains('\0')) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child argument contains a null character",
            ));
        }

        let (stdout_reader, stdout_writer) = create_named_capture_pipe("stdout")?;
        let (stderr_reader, stderr_writer) = create_named_capture_pipe("stderr")?;
        let stdin = open_inheritable_null_input()?;
        let job = create_kill_job()?;
        let inherited = [
            stdin.as_raw_handle(),
            stdout_writer.as_raw_handle(),
            stderr_writer.as_raw_handle(),
        ];
        let mut attributes = ProcThreadAttributeList::with_handle_list(&inherited)?;
        let mut startup = unsafe { std::mem::zeroed::<STARTUPINFOEXW>() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited[0];
        startup.StartupInfo.hStdOutput = inherited[1];
        startup.StartupInfo.hStdError = inherited[2];
        startup.lpAttributeList = attributes.pointer();

        let command_line = build_command_line(executable, arguments, verbatim_arguments);
        if command_line.encode_utf16().count() >= 32_767 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child command line exceeds the Windows limit",
            ));
        }
        let mut command_line = command_line
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let cwd_wide = cwd.map(|path| {
            path.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        });
        if cwd_wide
            .as_ref()
            .is_some_and(|path| path[..path.len().saturating_sub(1)].contains(&0))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child working directory contains a null character",
            ));
        }
        let environment_block = build_environment_block(environment)?;
        let mut process_info = unsafe { std::mem::zeroed::<PROCESS_INFORMATION>() };
        // SAFETY: all pointers refer to initialized storage that remains live for
        // the synchronous call. The handle-list attribute restricts inheritance.
        let created = unsafe {
            CreateProcessW(
                ptr::null(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                CREATE_SUSPENDED
                    | EXTENDED_STARTUPINFO_PRESENT
                    | if environment_block.is_some() {
                        CREATE_UNICODE_ENVIRONMENT
                    } else {
                        0
                    },
                environment_block
                    .as_ref()
                    .map_or(ptr::null(), |block| block.as_ptr().cast()),
                cwd_wide.as_ref().map_or(ptr::null(), |path| path.as_ptr()),
                (&raw const startup.StartupInfo),
                &mut process_info,
            )
        };
        if created == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateProcessW returned both handles with ownership to this call.
        let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess) };
        let thread = unsafe { OwnedHandle::from_raw_handle(process_info.hThread) };
        // SAFETY: the process is still suspended and both handles are live.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: process is a live suspended child owned by this function.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(error);
        }
        // SAFETY: thread is the live primary thread returned in suspended state.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            let error = io::Error::last_os_error();
            // SAFETY: process is live and remains owned here.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(error);
        }
        drop(thread);
        drop(stdin);
        drop(stdout_writer);
        drop(stderr_writer);

        let stdout_thread = std::thread::spawn(move || {
            read_named_pipe_bounded(stdout_reader, maximum_output_bytes)
        });
        let stderr_thread = std::thread::spawn(move || {
            read_named_pipe_bounded(stderr_reader, maximum_output_bytes)
        });
        // SAFETY: process is a live owned process handle.
        if unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut exit_code = 0;
        // SAFETY: the process is signaled and exit_code is writable.
        if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let stdout = stdout_thread
            .join()
            .map_err(|_| io::Error::other("child stdout reader panicked"))??;
        let stderr = stderr_thread
            .join()
            .map_err(|_| io::Error::other("child stderr reader panicked"))??;
        if stdout.len() > maximum_output_bytes || stderr.len() > maximum_output_bytes {
            return Err(io::Error::other("child output exceeds byte limit"));
        }
        drop(process);
        drop(job);
        Ok(BoundedOutput {
            status: ExitStatus::from_raw(exit_code),
            stdout,
            stderr,
        })
    }

    /// Builds the child's environment: everything this process has, with
    /// `overrides` layered on top.
    ///
    /// Returns `None` when there is nothing to override, so the ordinary case
    /// still inherits directly rather than rebuilding the block. Windows
    /// matches variable names case-insensitively and wants the block sorted,
    /// so both are done against an upper-cased key.
    fn build_environment_block(overrides: &[(OsString, OsString)]) -> io::Result<Option<Vec<u16>>> {
        if overrides.is_empty() {
            return Ok(None);
        }
        fn upper(name: &OsStr) -> Vec<u16> {
            name.encode_wide()
                .map(|unit| {
                    if (b'a' as u16..=b'z' as u16).contains(&unit) {
                        unit - 32
                    } else {
                        unit
                    }
                })
                .collect()
        }
        let mut variables: BTreeMap<Vec<u16>, (OsString, OsString)> = BTreeMap::new();
        for (name, value) in env::vars_os() {
            variables.insert(upper(&name), (name, value));
        }
        for (name, value) in overrides {
            variables.insert(upper(name), (name.clone(), value.clone()));
        }
        let mut block: Vec<u16> = Vec::new();
        for (name, value) in variables.into_values() {
            // An embedded null would truncate the block and silently drop
            // every variable after it.
            if name.encode_wide().any(|unit| unit == 0) || value.encode_wide().any(|unit| unit == 0)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "child environment contains a null character",
                ));
            }
            block.extend(name.encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(Some(block))
    }

    fn create_named_capture_pipe(label: &str) -> io::Result<(std::fs::File, OwnedHandle)> {
        let identifier = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\sako-{}-{identifier}-{label}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: name is terminated and all optional pointer inputs are null.
        let server = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                ptr::null(),
            )
        };
        if server == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateNamedPipeW returned a unique owned server HANDLE.
        let reader = unsafe { std::fs::File::from_raw_handle(server) };
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: the server instance and terminated name remain live. Security
        // attributes make only this client-side writer inheritable.
        let writer = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                &security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if writer == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a unique owned client HANDLE.
        Ok((reader, unsafe { OwnedHandle::from_raw_handle(writer) }))
    }

    fn open_inheritable_null_input() -> io::Result<OwnedHandle> {
        let name = "NUL\0".encode_utf16().collect::<Vec<_>>();
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: name is terminated and security remains valid for the call.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                0,
                &security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned one uniquely owned HANDLE.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// Joins a program and its arguments into the single string Windows
    /// actually passes to a process.
    ///
    /// `verbatim` turns the quoting off. Callers that have already quoted the
    /// line themselves need that: cross-spawn -- which almost everything in
    /// npm's ecosystem shells out through -- hands over
    /// `cmd.exe /d /s /c "npm.cmd install ..."` with the quotes exactly where
    /// `cmd` wants them, and quoting that again produces a line `cmd` cannot
    /// parse. The executable is still quoted when it has to be, because a path
    /// with a space in it is not the caller's mistake to own.
    fn build_command_line(executable: &str, arguments: &[String], verbatim: bool) -> String {
        if verbatim {
            let program = if executable.contains(' ') || executable.contains('"') {
                quote_windows_argument(executable)
            } else {
                executable.to_owned()
            };
            return std::iter::once(program)
                .chain(arguments.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
        }
        std::iter::once(executable)
            .chain(arguments.iter().map(String::as_str))
            .map(quote_windows_argument)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn quote_windows_argument(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|character| character == ' ' || character == '\t' || character == '"')
        {
            return argument.to_owned();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
            } else if character == '"' {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn read_named_pipe_bounded(
        mut stream: std::fs::File,
        maximum_bytes: usize,
    ) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        while bytes.len() <= maximum_bytes {
            let remaining = maximum_bytes.saturating_sub(bytes.len()) + 1;
            let read_capacity = remaining.min(buffer.len());
            match stream.read(&mut buffer[..read_capacity]) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }

    impl ChildJob {
        pub fn spawn(command: &mut Command) -> io::Result<Self> {
            let job = create_kill_job()?;
            let mut child = command.spawn()?;
            // SAFETY: both handles are live and owned outside this call. Assignment
            // changes kernel membership but transfers neither HANDLE.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0
            {
                let error = io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            Ok(Self { child, job })
        }

        pub fn id(&self) -> u32 {
            self.child.id()
        }

        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.child.wait()
        }

        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.child.try_wait()
        }

        pub fn kill(&mut self) -> io::Result<()> {
            self.child.kill()
        }

        pub fn job_handle(&self) -> &OwnedHandle {
            &self.job
        }

        pub fn spawn_with_bounded_output(
            command: &mut Command,
            maximum_output_bytes: usize,
        ) -> io::Result<BoundedOutput> {
            if maximum_output_bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "output limit must be positive",
                ));
            }
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut owned = Self::spawn(command)?;
            let stdout = owned
                .child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("child stdout pipe is unavailable"))?;
            let stderr = owned
                .child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("child stderr pipe is unavailable"))?;
            let stdout_thread =
                std::thread::spawn(move || read_bounded(stdout, maximum_output_bytes));
            let stderr_thread =
                std::thread::spawn(move || read_bounded(stderr, maximum_output_bytes));
            let status = owned.wait()?;
            let stdout = stdout_thread
                .join()
                .map_err(|_| io::Error::other("child stdout reader panicked"))??;
            let stderr = stderr_thread
                .join()
                .map_err(|_| io::Error::other("child stderr reader panicked"))??;
            if stdout.len() > maximum_output_bytes || stderr.len() > maximum_output_bytes {
                return Err(io::Error::other("child output exceeds byte limit"));
            }
            Ok(BoundedOutput {
                status,
                stdout,
                stderr,
            })
        }
    }

    fn create_kill_job() -> io::Result<OwnedHandle> {
        // SAFETY: null security attributes and name request a new unnamed Job
        // Object. A non-null HANDLE is transferred into OwnedHandle exactly once.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new owned HANDLE above.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits = ExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits has the exact documented layout and remains initialized
        // for the synchronous SetInformationJobObject call.
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&raw const limits).cast(),
                size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn child_runs_inside_an_owned_job() {
            let mut command = Command::new("cmd.exe");
            command.args(["/d", "/c", "exit 0"]);
            let mut child = ChildJob::spawn(&mut command).unwrap();
            assert!(child.id() != 0);
            assert!(child.wait().unwrap().success());
        }

        #[test]
        fn captures_bounded_child_output() {
            let mut command = Command::new("cmd.exe");
            command.args(["/d", "/c", "echo stdout & echo stderr 1>&2 & exit 7"]);
            let output = ChildJob::spawn_with_bounded_output(&mut command, 1024).unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert!(String::from_utf8_lossy(&output.stdout).contains("stdout"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("stderr"));
        }

        #[test]
        fn rejects_child_output_above_the_limit() {
            let mut command = Command::new("cmd.exe");
            command.args(["/d", "/c", "echo output-longer-than-eight-bytes"]);
            let error = match ChildJob::spawn_with_bounded_output(&mut command, 8) {
                Ok(_) => panic!("oversized output should fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("exceeds byte limit"));
        }

        #[test]
        fn native_launcher_uses_named_capture_pipes() {
            let arguments = [
                "/d".to_owned(),
                "/c".to_owned(),
                "echo native-out & echo native-error 1>&2 & exit /b 7".to_owned(),
            ];
            let output =
                spawn_native_with_bounded_output("cmd.exe", &arguments, None, 1024, false).unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert!(String::from_utf8_lossy(&output.stdout).contains("native-out"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("native-error"));
        }

        #[test]
        fn native_launcher_enforces_the_output_limit() {
            let arguments = [
                "/d".to_owned(),
                "/c".to_owned(),
                "echo output-longer-than-eight-bytes".to_owned(),
            ];
            let error = spawn_native_with_bounded_output("cmd.exe", &arguments, None, 8, false)
                .unwrap_err();
            assert!(error.to_string().contains("exceeds byte limit"));
        }

        #[test]
        fn quotes_windows_command_line_arguments() {
            assert_eq!(quote_windows_argument("plain"), "plain");
            assert_eq!(quote_windows_argument(""), "\"\"");
            assert_eq!(quote_windows_argument("two words"), "\"two words\"");
            assert_eq!(quote_windows_argument("say\"hello"), "\"say\\\"hello\"");
            assert_eq!(quote_windows_argument("two words\\"), "\"two words\\\\\"");
        }
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::{BoundedOutput, read_bounded};
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Stdio};

    const SIGKILL: i32 = 9;

    // SAFETY contract lives at each call site: `pid` must be a process group
    // id this module created (a child spawned with `process_group(0)`).
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    /// Kills every process in the group led by `pid`. A group that has already
    /// exited (or a lone child that already exited and was reaped) makes this
    /// a harmless no-op (`ESRCH`); the one inherent race POSIX allows here is
    /// `pid` reuse by an unrelated process between exit and this call, which
    /// Windows avoids via kernel object handles but POSIX process groups can't.
    fn kill_process_group(pid: u32) {
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: pid was produced by this module's own `Command::spawn`
            // with `process_group(0)`, so it is a valid process group leader.
            unsafe { kill(-pid, SIGKILL) };
        }
    }

    fn detach_into_own_group(command: &mut Command) {
        // Passing 0 asks the kernel to make the new child its own process
        // group leader (equivalent to `setpgid(0, 0)` right after fork),
        // giving this module a single id that reaches every descendant that
        // does not explicitly leave the group.
        command.process_group(0);
    }

    /// Launches a child with a clean argv (no shell, no command-line quoting)
    /// and bounded piped output. `std::process::Command` already restricts fd
    /// inheritance to the standard streams on POSIX, so this needs none of the
    /// manual named-pipe/attribute-list plumbing the Windows path requires.
    /// `verbatim_arguments` is accepted and ignored: POSIX passes an argv, so
    /// there is no command line for a caller to have pre-quoted.
    pub fn spawn_native_with_bounded_output(
        executable: &str,
        arguments: &[String],
        cwd: Option<&Path>,
        maximum_output_bytes: usize,
        verbatim_arguments: bool,
    ) -> io::Result<BoundedOutput> {
        spawn_native_with_bounded_output_in(
            executable,
            arguments,
            cwd,
            maximum_output_bytes,
            verbatim_arguments,
            &[],
        )
    }

    /// As above, with `environment` layered over the inherited variables.
    pub fn spawn_native_with_bounded_output_in(
        executable: &str,
        arguments: &[String],
        cwd: Option<&Path>,
        maximum_output_bytes: usize,
        _verbatim_arguments: bool,
        environment: &[(OsString, OsString)],
    ) -> io::Result<BoundedOutput> {
        if executable.is_empty() || maximum_output_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native child input is invalid",
            ));
        }
        let mut command = Command::new(executable);
        command.args(arguments).stdin(Stdio::null());
        for (name, value) in environment {
            command.env(name, value);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        ChildJob::spawn_with_bounded_output(&mut command, maximum_output_bytes)
    }

    pub struct ChildJob {
        child: Child,
    }

    impl ChildJob {
        pub fn spawn(command: &mut Command) -> io::Result<Self> {
            detach_into_own_group(command);
            let child = command.spawn()?;
            Ok(Self { child })
        }

        pub fn id(&self) -> u32 {
            self.child.id()
        }

        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.child.wait()
        }

        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.child.try_wait()
        }

        pub fn kill(&mut self) -> io::Result<()> {
            self.child.kill()
        }

        pub fn spawn_with_bounded_output(
            command: &mut Command,
            maximum_output_bytes: usize,
        ) -> io::Result<BoundedOutput> {
            if maximum_output_bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "output limit must be positive",
                ));
            }
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut owned = Self::spawn(command)?;
            let stdout = owned
                .child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("child stdout pipe is unavailable"))?;
            let stderr = owned
                .child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("child stderr pipe is unavailable"))?;
            let stdout_thread =
                std::thread::spawn(move || read_bounded(stdout, maximum_output_bytes));
            let stderr_thread =
                std::thread::spawn(move || read_bounded(stderr, maximum_output_bytes));
            let status = owned.wait()?;
            let stdout = stdout_thread
                .join()
                .map_err(|_| io::Error::other("child stdout reader panicked"))??;
            let stderr = stderr_thread
                .join()
                .map_err(|_| io::Error::other("child stderr reader panicked"))??;
            if stdout.len() > maximum_output_bytes || stderr.len() > maximum_output_bytes {
                return Err(io::Error::other("child output exceeds byte limit"));
            }
            Ok(BoundedOutput {
                status,
                stdout,
                stderr,
            })
        }
    }

    impl Drop for ChildJob {
        fn drop(&mut self) {
            kill_process_group(self.child.id());
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn child_runs_inside_an_owned_group() {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 0"]);
            let mut child = ChildJob::spawn(&mut command).unwrap();
            assert!(child.id() != 0);
            assert!(child.wait().unwrap().success());
        }

        #[test]
        fn captures_bounded_child_output() {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "echo stdout; echo stderr 1>&2; exit 7"]);
            let output = ChildJob::spawn_with_bounded_output(&mut command, 1024).unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert!(String::from_utf8_lossy(&output.stdout).contains("stdout"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("stderr"));
        }

        #[test]
        fn rejects_child_output_above_the_limit() {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "echo output-longer-than-eight-bytes"]);
            let error = match ChildJob::spawn_with_bounded_output(&mut command, 8) {
                Ok(_) => panic!("oversized output should fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("exceeds byte limit"));
        }

        #[test]
        fn native_launcher_captures_piped_output() {
            let arguments = [
                "-c".to_owned(),
                "echo native-out; echo native-error 1>&2; exit 7".to_owned(),
            ];
            let output =
                spawn_native_with_bounded_output("/bin/sh", &arguments, None, 1024, false).unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert!(String::from_utf8_lossy(&output.stdout).contains("native-out"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("native-error"));
        }

        #[test]
        fn native_launcher_enforces_the_output_limit() {
            let arguments = [
                "-c".to_owned(),
                "echo output-longer-than-eight-bytes".to_owned(),
            ];
            let error = spawn_native_with_bounded_output("/bin/sh", &arguments, None, 8, false)
                .unwrap_err();
            assert!(error.to_string().contains("exceeds byte limit"));
        }

        #[test]
        fn native_launcher_respects_working_directory() {
            let directory = std::env::temp_dir();
            let output = spawn_native_with_bounded_output(
                "/bin/sh",
                &["-c".to_owned(), "pwd".to_owned()],
                Some(&directory),
                4096,
                false,
            )
            .unwrap();
            let canonical = std::fs::canonicalize(&directory).unwrap();
            let printed = String::from_utf8_lossy(&output.stdout);
            assert_eq!(std::fs::canonicalize(printed.trim()).unwrap(), canonical);
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{
    ChildJob, spawn_native_with_bounded_output, spawn_native_with_bounded_output_in,
};

#[cfg(all(unix, not(windows)))]
pub use unix_impl::{
    ChildJob, spawn_native_with_bounded_output, spawn_native_with_bounded_output_in,
};

/// Ends this process immediately, skipping the orderly shutdown the C runtime
/// and the Windows loader would otherwise perform.
///
/// A normal return from `main` walks the C runtime's onexit table -- every
/// C++ static destructor the image carries, V8's among them -- and then makes
/// the loader detach each loaded module. None of that changes anything the
/// process has already written; all of it is bookkeeping over memory the
/// kernel unmaps a moment later. Chrome and Firefox both leave this way for
/// the same reason.
///
/// Anything buffered in this process is lost. Callers must flush their own
/// streams first; nothing after this call runs.
pub fn exit_immediately(code: u8) -> ! {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut core::ffi::c_void;
            fn TerminateProcess(process: *mut core::ffi::c_void, code: u32) -> i32;
        }
        // SAFETY: both calls take no memory of ours and never return here.
        unsafe {
            TerminateProcess(GetCurrentProcess(), u32::from(code));
        }
    }
    #[cfg(all(unix, not(windows)))]
    {
        unsafe extern "C" {
            fn _exit(code: i32) -> !;
        }
        // SAFETY: _exit is always available and never returns.
        unsafe { _exit(i32::from(code)) }
    }
    // TerminateProcess on the current process does not return, but the
    // compiler cannot know that.
    #[allow(unreachable_code)]
    {
        std::process::exit(i32::from(code))
    }
}
