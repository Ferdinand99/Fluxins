//! One running Inlet per user. A second start hands its command line (a deep link,
//! say) to the first and exits.
//!
//! - Unix: a socket in a folder only the user can open (`$XDG_RUNTIME_DIR`, or the
//!   per-user `$TMPDIR` on macOS), with an `flock` on a lock file deciding who is
//!   first. Both sides check the other end's user id (`SO_PEERCRED`/`getpeereid`).
//! - Windows: a named pipe whose owner and only allowed user is the current user,
//!   created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, refusing remote clients. The
//!   second instance checks that the pipe's owner is its own user before it sends
//!   anything, and connects with identification-level impersonation only. All pipe
//!   I/O is overlapped and cancelled with `CancelIoEx` when its deadline passes.
//!
//! Deadlines: the first instance gives each connection 2 s in all (a client that
//! connects and sends nothing is dropped after that); a second instance gives up
//! after 5 s in all.
//!
//! Messages are bounded: at most 32 arguments, 2 KiB each, 16 KiB in all, UTF-8.
//! Longer or extra arguments are dropped with a warning before anything is sent.
//!
//! **What the app must do with an error:** any `Err` from [`single_instance`] means
//! another instance may be running (for example [`Error::NotResponding`]: the pipe
//! or socket exists but nobody answered). Show the error and exit; never carry on as
//! a second primary, which would run every account twice.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"INLETSI1";
const MAX_ARGS: usize = 32;
const MAX_ARG_BYTES: usize = 2048;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const ACK: u8 = 0x06;
/// Time the first instance gives one connection, from accept to the end of the ACK.
const SERVE_DEADLINE: Duration = Duration::from_secs(2);
/// Time a second instance gives the whole hand-over, connecting included.
const SEND_DEADLINE: Duration = Duration::from_secs(5);

/// Which instance this process is.
pub enum Instance {
    /// The first instance. Command lines of later starts arrive here.
    Primary(mpsc::Receiver<Vec<String>>),
    /// Another instance is running and has received this command line; exit now.
    Secondary,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Instance::Primary(_) => "Primary",
            Instance::Secondary => "Secondary",
        })
    }
}

/// Becomes the primary instance, or hands this process's arguments (without the
/// program name) to the running one. Arguments that are not valid Unicode are
/// dropped. On `Err`, exit (see the module documentation).
pub fn single_instance(app_id: &str) -> Result<Instance> {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .filter_map(|arg| match arg.into_string() {
            Ok(arg) => Some(arg),
            Err(_) => {
                log::warn!("dropped a command-line argument that is not valid Unicode");
                None
            }
        })
        .collect();
    SingleInstance::new(app_id).start(&args)
}

/// Options for [`single_instance`].
#[derive(Clone, Debug)]
pub struct SingleInstance {
    app_id: String,
    #[cfg(unix)]
    dir: Option<std::path::PathBuf>,
}

impl SingleInstance {
    pub fn new(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            #[cfg(unix)]
            dir: None,
        }
    }

    /// Uses `dir` instead of the runtime folder (it must still be private).
    #[cfg(unix)]
    pub fn in_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// See [`single_instance`]. On `Err`, exit.
    pub fn start(&self, args: &[String]) -> Result<Instance> {
        let ok = !self.app_id.is_empty()
            && self.app_id.len() <= 64
            && self
                .app_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(Error::InvalidInput("app id"));
        }
        // What does not fit is dropped first, so a long argument cannot keep the
        // first instance from starting.
        let message = encode(&forwardable(args))?;
        imp::start(self, &message)
    }
}

/// The arguments that fit in one message: too long ones are dropped, then the
/// list is cut at the argument and size limits.
fn forwardable(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut size = MAGIC.len() + 4;
    for arg in args {
        if arg.len() > MAX_ARG_BYTES {
            log::warn!(
                "dropped a {}-byte argument that is too long to forward",
                arg.len()
            );
            continue;
        }
        if out.len() == MAX_ARGS || size + 4 + arg.len() > MAX_MESSAGE_BYTES {
            log::warn!("dropped command-line arguments beyond the forwarding limit");
            break;
        }
        size += 4 + arg.len();
        out.push(arg.clone());
    }
    out
}

fn encode(args: &[String]) -> Result<Vec<u8>> {
    if args.len() > MAX_ARGS {
        return Err(Error::InvalidInput("too many arguments to forward"));
    }
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(args.len() as u32).to_le_bytes());
    for arg in args {
        if arg.len() > MAX_ARG_BYTES {
            return Err(Error::InvalidInput("argument too long to forward"));
        }
        out.extend_from_slice(&(arg.len() as u32).to_le_bytes());
        out.extend_from_slice(arg.as_bytes());
    }
    if out.len() > MAX_MESSAGE_BYTES {
        return Err(Error::InvalidInput("command line too long to forward"));
    }
    Ok(out)
}

/// Reads one message, never more than [`MAX_MESSAGE_BYTES`].
fn read_message(stream: &mut impl Read) -> io::Result<Vec<String>> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let mut limited = stream.take(MAX_MESSAGE_BYTES as u64);
    let mut magic = [0u8; 8];
    limited.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("bad magic"));
    }
    let mut word = [0u8; 4];
    limited.read_exact(&mut word)?;
    let count = u32::from_le_bytes(word) as usize;
    if count > MAX_ARGS {
        return Err(bad("too many arguments"));
    }
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        limited.read_exact(&mut word)?;
        let len = u32::from_le_bytes(word) as usize;
        if len > MAX_ARG_BYTES {
            return Err(bad("argument too long"));
        }
        let mut bytes = vec![0u8; len];
        limited.read_exact(&mut bytes)?;
        args.push(String::from_utf8(bytes).map_err(|_| bad("argument is not UTF-8"))?);
    }
    Ok(args)
}

/// Serves one connection: reads the message, passes it on, acknowledges, and waits
/// (within the stream's deadline) for the client to hang up, so the ACK is read
/// before the connection is torn down.
fn serve(stream: &mut (impl Read + Write), tx: &mpsc::Sender<Vec<String>>) {
    match read_message(stream) {
        Ok(args) => {
            let _ = tx.send(args);
            if stream
                .write_all(&[ACK])
                .and_then(|()| stream.flush())
                .is_ok()
            {
                let _ = stream.read(&mut [0u8; 1]);
            }
        }
        Err(err) => log::warn!("refused a message from another instance: {err}"),
    }
}

fn send(stream: &mut (impl Read + Write), message: &[u8]) -> Result<Instance> {
    let not_responding = |_| Error::NotResponding;
    stream.write_all(message).map_err(not_responding)?;
    stream.flush().map_err(not_responding)?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).map_err(not_responding)?;
    if ack[0] != ACK {
        return Err(Error::NotResponding);
    }
    Ok(Instance::Secondary)
}

/// Time left until `until`, or a `TimedOut` error once it has passed.
fn remaining(until: Instant) -> io::Result<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::Error::new(io::ErrorKind::TimedOut, "deadline passed"))
    } else {
        Ok(left)
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::{DirBuilder, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{remaining, send, serve, Instance, SingleInstance, SEND_DEADLINE, SERVE_DEADLINE};
    use crate::{Error, Result};

    fn uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    fn base_dir() -> PathBuf {
        #[cfg(target_os = "linux")]
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            if dir.is_absolute() {
                return dir;
            }
        }
        std::env::temp_dir()
    }

    /// Creates (or reuses) `<base>/<app_id>-<uid>` and refuses it unless it is a real
    /// directory, owned by this user, with no access for anyone else.
    pub(super) fn private_dir(base: &Path, app_id: &str) -> Result<PathBuf> {
        let dir = base.join(format!("{app_id}-{}", uid()));
        match DirBuilder::new().mode(0o700).create(&dir) {
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err.into()),
            _ => {}
        }
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.file_type().is_dir() {
            return Err(Error::Insecure("instance folder is not a directory"));
        }
        if meta.uid() != uid() {
            return Err(Error::Insecure("instance folder belongs to another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(Error::Insecure("instance folder is open to other users"));
        }
        Ok(dir)
    }

    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        let fd = stream.as_raw_fd();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // SAFETY: ucred is plain old data; all-zero is a valid value.
            let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: `cred` and `len` are valid for the call and sized for SO_PEERCRED.
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut cred as *mut libc::ucred).cast(),
                    &mut len,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(cred.uid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let (mut uid, mut gid) = (0, 0);
            // SAFETY: both out-pointers are valid for the call.
            if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(uid)
        }
    }

    fn try_lock(file: &File) -> io::Result<bool> {
        // SAFETY: a valid descriptor; the lock lives as long as the file is open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(err)
        }
    }

    /// A socket whose reads and writes share one deadline.
    struct Deadline<'a> {
        stream: &'a mut UnixStream,
        until: Instant,
    }

    impl Read for Deadline<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.stream.set_read_timeout(Some(remaining(self.until)?))?;
            self.stream.read(buf)
        }
    }

    impl Write for Deadline<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.stream
                .set_write_timeout(Some(remaining(self.until)?))?;
            self.stream.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    pub(super) fn start(opts: &SingleInstance, message: &[u8]) -> Result<Instance> {
        let base = opts.dir.clone().unwrap_or_else(base_dir);
        let dir = private_dir(&base, &opts.app_id)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.join("lock"))?;
        let socket = dir.join("socket");
        if try_lock(&lock)? {
            primary(lock, &socket)
        } else {
            secondary(&socket, message)
        }
    }

    fn primary(lock: File, socket: &Path) -> Result<Instance> {
        // Only the lock holder gets here, so a socket file left behind is stale.
        match std::fs::remove_file(socket) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        let listener = UnixListener::bind(socket)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("inlet-single-instance".into())
            .spawn(move || {
                // The lock is held for as long as this thread runs: the life of the app.
                let _lock = lock;
                for stream in listener.incoming() {
                    let mut stream = match stream {
                        Ok(stream) => stream,
                        Err(err) => {
                            // Out of descriptors, say: back off instead of spinning.
                            log::warn!("single-instance accept failed: {err}");
                            std::thread::sleep(Duration::from_millis(200));
                            continue;
                        }
                    };
                    match peer_uid(&stream) {
                        Ok(peer) if peer == uid() => {}
                        _ => {
                            log::warn!("refused a connection from another user");
                            continue;
                        }
                    }
                    let until = Instant::now() + SERVE_DEADLINE;
                    serve(
                        &mut Deadline {
                            stream: &mut stream,
                            until,
                        },
                        &tx,
                    );
                }
            })?;
        Ok(Instance::Primary(rx))
    }

    fn secondary(socket: &Path, message: &[u8]) -> Result<Instance> {
        // The first instance holds the lock, so it exists: anything but a hand-over
        // within the deadline is "not responding", never "go ahead".
        let until = Instant::now() + SEND_DEADLINE;
        loop {
            match UnixStream::connect(socket) {
                Ok(mut stream) => {
                    if peer_uid(&stream)? != uid() {
                        return Err(Error::Insecure(
                            "the running instance belongs to another user",
                        ));
                    }
                    return send(
                        &mut Deadline {
                            stream: &mut stream,
                            until,
                        },
                        message,
                    );
                }
                // It may hold the lock but not have bound its socket yet.
                Err(_) if remaining(until).is_ok() => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(_) => return Err(Error::NotResponding),
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io::{self, Read, Write};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows::core::{HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE,
        ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        HANDLE, HLOCAL, WAIT_OBJECT_0,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
        FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    use super::{remaining, send, serve, Instance, SingleInstance, SEND_DEADLINE, SERVE_DEADLINE};
    use crate::{Error, Result};

    /// An owned handle that is closed on drop.
    struct Handle(HANDLE);
    // SAFETY: a kernel handle may be used from any thread; each one here is only used
    // by one thread at a time.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle is owned and closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn os_error(err: windows::core::Error) -> io::Error {
        if err.code() == ERROR_BROKEN_PIPE.to_hresult() {
            io::Error::new(io::ErrorKind::BrokenPipe, err)
        } else if err.code() == ERROR_PIPE_CONNECTED.to_hresult() {
            io::Error::new(io::ErrorKind::AlreadyExists, err)
        } else {
            io::Error::other(err)
        }
    }

    /// Overlapped I/O on a pipe, every operation bounded by one shared deadline. When
    /// the deadline passes, the operation is cancelled with `CancelIoEx`.
    struct Overlapped<'a> {
        pipe: &'a Handle,
        event: Handle,
        until: Option<Instant>,
    }

    impl<'a> Overlapped<'a> {
        fn new(pipe: &'a Handle, until: Option<Instant>) -> io::Result<Self> {
            // SAFETY: an unnamed manual-reset event with default security.
            let event =
                unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(os_error)?;
            Ok(Self {
                pipe,
                event: Handle(event),
                until,
            })
        }

        /// Starts an operation with `start` and waits for it within the deadline.
        /// Once the deadline has passed no new operation is started, so there is
        /// nothing to cancel.
        fn run(
            &self,
            start: impl FnOnce(&mut OVERLAPPED) -> windows::core::Result<()>,
        ) -> io::Result<u32> {
            if let Some(until) = self.until {
                remaining(until)?;
            }
            self.run_started(start)
        }

        /// Starts the operation and waits for it. Every started operation ends in
        /// completion, or in `CancelIoEx` plus `GetOverlappedResult(wait = TRUE)`
        /// through [`InFlight`], before `ov` (and the caller's buffer) can go away,
        /// also on an early return or a panic.
        fn run_started(
            &self,
            start: impl FnOnce(&mut OVERLAPPED) -> windows::core::Result<()>,
        ) -> io::Result<u32> {
            let mut ov = OVERLAPPED {
                hEvent: self.event.0,
                ..Default::default()
            };
            let started = start(&mut ov);
            let mut in_flight = InFlight {
                pipe: self.pipe.0,
                ov: &ov,
                finished: false,
            };
            match started {
                Ok(()) => {}
                Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => {
                    // Saturating: a deadline that has just passed waits 0 ms and then
                    // cancels below; nothing here may return early.
                    let wait_ms = match self.until {
                        Some(until) => until
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .min(60_000) as u32,
                        None => INFINITE,
                    };
                    // SAFETY: the event belongs to this operation and stays alive.
                    let woke = unsafe { WaitForSingleObject(self.event.0, wait_ms) };
                    if woke != WAIT_OBJECT_0 {
                        // Cancels the operation and waits until the kernel is done
                        // with `ov` and the buffer.
                        drop(in_flight);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "pipe deadline passed",
                        ));
                    }
                }
                Err(err) => {
                    // Failed at once: nothing was started.
                    in_flight.finished = true;
                    return Err(os_error(err));
                }
            }
            let mut done = 0u32;
            // SAFETY: the operation on `ov` has signalled completion.
            let result =
                unsafe { GetOverlappedResult(self.pipe.0, in_flight.ov, &mut done, false) };
            match result {
                Err(err) if err.code() == ERROR_IO_INCOMPLETE.to_hresult() => {
                    // Not actually done: the guard cancels and waits.
                    Err(os_error(err))
                }
                result => {
                    in_flight.finished = true;
                    result.map_err(os_error)?;
                    Ok(done)
                }
            }
        }
    }

    /// A started overlapped operation. Unless it was seen to finish, dropping this
    /// cancels it and waits for the cancellation, so the `OVERLAPPED` and the buffer
    /// it points to are never freed while the kernel still owns them.
    struct InFlight<'o> {
        pipe: HANDLE,
        ov: &'o OVERLAPPED,
        finished: bool,
    }

    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            if self.finished {
                return;
            }
            let mut ignored = 0u32;
            // SAFETY: cancels only this operation, then blocks until it has
            // completed (cancelled or not), after which the kernel no longer uses
            // `ov` or the buffer.
            unsafe {
                let _ = CancelIoEx(self.pipe, Some(self.ov));
                let _ = GetOverlappedResult(self.pipe, self.ov, &mut ignored, true);
            }
        }
    }

    impl Read for Overlapped<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let pipe = self.pipe.0;
            // SAFETY: `buf` and the OVERLAPPED outlive the operation (run waits for it).
            match self.run(|ov| unsafe { ReadFile(pipe, Some(buf), None, Some(ov)) }) {
                Ok(n) => Ok(n as usize),
                // The other side hung up: end of stream.
                Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(0),
                Err(err) => Err(err),
            }
        }
    }

    impl Write for Overlapped<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let pipe = self.pipe.0;
            // SAFETY: `buf` and the OVERLAPPED outlive the operation (run waits for it).
            self.run(|ov| unsafe { WriteFile(pipe, Some(buf), None, Some(ov)) })
                .map(|n| n as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            // Writes complete when the data is in the pipe; FlushFileBuffers would
            // wait for the reader without a deadline.
            Ok(())
        }
    }

    /// The current user's SID, as a buffer holding a TOKEN_USER.
    struct UserSid {
        buf: Vec<u64>,
    }

    impl UserSid {
        fn current() -> Result<Self> {
            let mut token = HANDLE::default();
            // SAFETY: querying our own process token.
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
            let token = Handle(token);
            let mut len = 0u32;
            // SAFETY: first call only asks for the size.
            let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut len) };
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            // SAFETY: the buffer is at least `len` bytes and 8-byte aligned.
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    Some(buf.as_mut_ptr().cast()),
                    len,
                    &mut len,
                )?
            };
            Ok(Self { buf })
        }

        fn psid(&self) -> PSID {
            // SAFETY: the buffer holds a TOKEN_USER written by GetTokenInformation.
            unsafe { (*(self.buf.as_ptr() as *const TOKEN_USER)).User.Sid }
        }

        fn string(&self) -> Result<String> {
            let mut out = PWSTR::null();
            // SAFETY: valid SID; the string is freed with LocalFree once read.
            unsafe {
                ConvertSidToStringSidW(self.psid(), &mut out)?;
                let text = out.to_string().map_err(|e| Error::Os(e.to_string()));
                let _ = LocalFree(Some(HLOCAL(out.0.cast())));
                text
            }
        }
    }

    fn pipe_name(app_id: &str, sid: &str) -> HSTRING {
        HSTRING::from(format!(r"\\.\pipe\{app_id}-{sid}"))
    }

    /// Opens the pipe as a client: identification level only, so the server cannot
    /// act as this user.
    fn open_client(name: &HSTRING, overlapped: bool) -> windows::core::Result<HANDLE> {
        let mut flags = SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0;
        if overlapped {
            flags |= FILE_FLAG_OVERLAPPED.0;
        }
        // SAFETY: opening a named pipe by a valid, NUL-terminated name.
        unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0 | READ_CONTROL.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(flags),
                None,
            )
        }
    }

    /// A client that connects and then sends nothing (tests only).
    #[cfg(test)]
    pub(super) struct SilentClient(#[allow(dead_code)] Handle);

    #[cfg(test)]
    pub(super) fn silent_client(app_id: &str) -> SilentClient {
        let sid = UserSid::current().unwrap().string().unwrap();
        SilentClient(Handle(
            open_client(&pipe_name(app_id, &sid), false).unwrap(),
        ))
    }

    /// Runs a pending read and a pending write whose deadline has already passed
    /// (tests only): both must come back `TimedOut` after being cancelled, and the
    /// pipe must still work afterwards.
    #[cfg(test)]
    pub(super) fn pending_io_after_the_deadline() {
        let name = HSTRING::from(format!(
            r"\\.\pipe\inlet-test-{}-pending",
            std::process::id()
        ));
        // SAFETY: a test pipe with tiny buffers, so a large write stays pending.
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1,
                1,
                0,
                None,
            )
        };
        assert!(!server.is_invalid());
        let server = Handle(server);
        let client = Handle(open_client(&name, false).unwrap());
        accept(&server).unwrap();

        let past = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap_or_else(Instant::now);
        let io = Overlapped::new(&server, Some(past)).unwrap();
        let handle = server.0;

        // A read with nothing to read stays pending; the deadline has passed.
        let mut buf = [0u8; 16];
        // SAFETY: `buf` outlives the call; run_started waits for the operation.
        let err = io
            .run_started(|ov| unsafe { ReadFile(handle, Some(&mut buf), None, Some(ov)) })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        // A write larger than the pipe buffer with nobody reading stays pending.
        let big = vec![7u8; 256 * 1024];
        // SAFETY: `big` outlives the call; run_started waits for the operation.
        let err = io
            .run_started(|ov| unsafe { WriteFile(handle, Some(&big), None, Some(ov)) })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        // Through the public path nothing is even started after the deadline.
        let mut late = Overlapped::new(&server, Some(past)).unwrap();
        assert_eq!(
            late.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );

        // The pipe still works: the client writes, a fresh deadline reads it.
        let mut written = 0u32;
        // SAFETY: synchronous write on the client handle from a valid buffer.
        unsafe { WriteFile(client.0, Some(b"x"), Some(&mut written), None) }.unwrap();
        let mut fresh =
            Overlapped::new(&server, Some(Instant::now() + Duration::from_secs(2))).unwrap();
        let mut one = [0u8; 1];
        assert_eq!(fresh.read(&mut one).unwrap(), 1);
        assert_eq!(one[0], b'x');
    }

    pub(super) fn start(opts: &SingleInstance, message: &[u8]) -> Result<Instance> {
        let user = UserSid::current()?;
        let sid = user.string()?;
        let name = pipe_name(&opts.app_id, &sid);
        // Owner and only allowed user: this user. Protected DACL, nothing inherited.
        let sddl = HSTRING::from(format!("O:{sid}D:P(A;;GA;;;{sid})"));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the descriptor is freed with LocalFree below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?
        };
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: valid name and attributes; one instance only.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                Some(&attrs),
            )
        };
        // Read the error before anything else can overwrite the thread's last error.
        let create_error = pipe.is_invalid().then(windows::core::Error::from_thread);
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if let Some(err) = create_error {
            // With FILE_FLAG_FIRST_PIPE_INSTANCE and one allowed instance, an existing
            // pipe gives ACCESS_DENIED or PIPE_BUSY: another instance is running (or a
            // squatter, which `secondary` refuses after checking the owner).
            if err.code() == ERROR_ACCESS_DENIED.to_hresult()
                || err.code() == ERROR_PIPE_BUSY.to_hresult()
            {
                return secondary(&name, &user, message);
            }
            return Err(err.into());
        }
        primary(Handle(pipe))
    }

    /// Waits (without a deadline) for the next client to connect.
    fn accept(pipe: &Handle) -> io::Result<()> {
        let io = Overlapped::new(pipe, None)?;
        let handle = pipe.0;
        // SAFETY: valid pipe handle; `run` waits for the operation to finish.
        match io.run(|ov| unsafe { ConnectNamedPipe(handle, Some(ov)) }) {
            Ok(_) => Ok(()),
            // The client connected between CreateNamedPipe/Disconnect and this call.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn primary(pipe: Handle) -> Result<Instance> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("inlet-single-instance".into())
            .spawn(move || loop {
                match accept(&pipe) {
                    Ok(()) => {
                        let until = Instant::now() + SERVE_DEADLINE;
                        match Overlapped::new(&pipe, Some(until)) {
                            Ok(mut io) => serve(&mut io, &tx),
                            Err(err) => log::warn!("single-instance pipe: {err}"),
                        }
                    }
                    Err(err) => {
                        log::warn!("single-instance pipe connect failed: {err}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                // SAFETY: ends this client's session (a silent one is cut off here);
                // the handle stays valid for the next client.
                unsafe {
                    let _ = DisconnectNamedPipe(pipe.0);
                }
            })?;
        Ok(Instance::Primary(rx))
    }

    fn secondary(name: &HSTRING, user: &UserSid, message: &[u8]) -> Result<Instance> {
        // The pipe exists, so an instance is running: anything but a hand-over within
        // the deadline is "not responding", never "go ahead".
        let until = Instant::now() + SEND_DEADLINE;
        loop {
            let Ok(left) = remaining(until) else {
                return Err(Error::NotResponding);
            };
            match open_client(name, true) {
                Ok(handle) => {
                    let pipe = Handle(handle);
                    check_owner(&pipe, user)?;
                    let mut io =
                        Overlapped::new(&pipe, Some(until)).map_err(|_| Error::NotResponding)?;
                    return send(&mut io, message);
                }
                Err(err) if err.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    let wait_ms = left.as_millis().clamp(1, 5_000) as u32;
                    // SAFETY: waits, bounded, for the pipe instance to become free.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), wait_ms) };
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    /// Refuses a pipe that someone else created under our name.
    fn check_owner(pipe: &Handle, user: &UserSid) -> Result<()> {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the descriptor is freed with LocalFree; `owner` points into it.
        let err = unsafe {
            GetSecurityInfo(
                pipe.0,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                None,
                None,
                Some(&mut sd),
            )
        };
        if err.is_err() {
            return Err(Error::Os(format!("could not read the pipe owner: {err:?}")));
        }
        // SAFETY: both SIDs are valid while `sd` lives.
        let same = unsafe { EqualSid(owner, user.psid()) }.is_ok();
        // SAFETY: allocated by GetSecurityInfo and no longer used.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if same {
            Ok(())
        } else {
            Err(Error::Insecure("the instance pipe belongs to another user"))
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::{Instance, SingleInstance};
    use crate::Result;

    pub(super) fn start(_opts: &SingleInstance, _message: &[u8]) -> Result<Instance> {
        Ok(Instance::Primary(std::sync::mpsc::channel().1))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn opts(_dir: &std::path::Path, app_id: &str) -> SingleInstance {
        let opts = SingleInstance::new(app_id);
        #[cfg(unix)]
        let opts = opts.in_dir(_dir);
        opts
    }

    #[test]
    fn single_instance_forwards_args_to_the_primary() {
        let dir = tempfile::tempdir().unwrap();
        let app_id = format!("inlet-test-{}-fwd", std::process::id());
        let Instance::Primary(rx) = opts(dir.path(), &app_id).start(&[]).unwrap() else {
            panic!("first start should be primary");
        };
        let sent = args(&[
            "--url",
            "--",
            "inlet://channels/@me/42",
            "\u{00E6}\u{00F8}\u{00E5}",
        ]);
        let second = opts(dir.path(), &app_id).start(&sent).unwrap();
        assert!(matches!(second, Instance::Secondary));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), sent);
        // And again: the primary keeps serving.
        let third = opts(dir.path(), &app_id).start(&args(&["x"])).unwrap();
        assert!(matches!(third, Instance::Secondary));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            args(&["x"])
        );
    }

    /// A client that connects and sends nothing is cut off after the serve deadline;
    /// the next start still hands over within its own deadline.
    #[test]
    fn a_silent_client_does_not_block_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let app_id = format!("inlet-test-{}-silent", std::process::id());
        let Instance::Primary(rx) = opts(dir.path(), &app_id).start(&[]).unwrap() else {
            panic!("first start should be primary");
        };
        #[cfg(unix)]
        let _silent = std::os::unix::net::UnixStream::connect(
            imp::private_dir(dir.path(), &app_id)
                .unwrap()
                .join("socket"),
        )
        .unwrap();
        #[cfg(windows)]
        let _silent = imp::silent_client(&app_id);
        let started = std::time::Instant::now();
        let second = opts(dir.path(), &app_id).start(&args(&["x"])).unwrap();
        assert!(matches!(second, Instance::Secondary));
        assert!(started.elapsed() < SEND_DEADLINE, "{:?}", started.elapsed());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            args(&["x"])
        );
    }

    /// A pending read and a pending write whose deadline has passed are cancelled
    /// and waited for before their memory goes away.
    #[cfg(windows)]
    #[test]
    fn pending_pipe_io_after_the_deadline_is_cancelled() {
        imp::pending_io_after_the_deadline();
    }

    #[test]
    fn long_or_extra_arguments_are_dropped_not_fatal() {
        let long = "a".repeat(MAX_ARG_BYTES + 1);
        let kept = forwardable(&[long, "inlet://invite/abc".into()]);
        assert_eq!(kept, args(&["inlet://invite/abc"]));
        let many: Vec<String> = (0..100).map(|i| i.to_string()).collect();
        assert_eq!(forwardable(&many).len(), MAX_ARGS);
        let big = vec!["b".repeat(MAX_ARG_BYTES); 20];
        let kept = forwardable(&big);
        assert!(encode(&kept).is_ok());
        assert!(kept.len() < 20);
        // A first instance with an oversized argument still starts.
        let dir = tempfile::tempdir().unwrap();
        let app_id = format!("inlet-test-{}-long", std::process::id());
        let start = opts(dir.path(), &app_id).start(&["c".repeat(10_000)]);
        assert!(matches!(start, Ok(Instance::Primary(_))));
    }

    #[test]
    fn oversized_or_malformed_messages_are_refused() {
        assert!(encode(&args(&["a"; MAX_ARGS + 1])).is_err());
        assert!(encode(&["a".repeat(MAX_ARG_BYTES + 1)]).is_err());
        assert!(encode(&vec!["a".repeat(MAX_ARG_BYTES); 9]).is_err());
        let ok = encode(&args(&["one", "two"])).unwrap();
        assert_eq!(
            read_message(&mut ok.as_slice()).unwrap(),
            args(&["one", "two"])
        );

        let mut bad_magic = ok.clone();
        bad_magic[0] = b'X';
        assert!(read_message(&mut bad_magic.as_slice()).is_err());
        // A declared count above the limit.
        let mut many = MAGIC.to_vec();
        many.extend_from_slice(&1000u32.to_le_bytes());
        assert!(read_message(&mut many.as_slice()).is_err());
        // A declared length above the limit.
        let mut long = MAGIC.to_vec();
        long.extend_from_slice(&1u32.to_le_bytes());
        long.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_message(&mut long.as_slice()).is_err());
        // Truncated.
        assert!(read_message(&mut &ok[..ok.len() - 1]).is_err());
        // Not UTF-8.
        let mut latin = MAGIC.to_vec();
        latin.extend_from_slice(&1u32.to_le_bytes());
        latin.extend_from_slice(&1u32.to_le_bytes());
        latin.push(0xFF);
        assert!(read_message(&mut latin.as_slice()).is_err());
        // An endless stream stops at the message limit.
        let mut endless = MAGIC.to_vec();
        endless.extend_from_slice(&32u32.to_le_bytes());
        for _ in 0..32 {
            endless.extend_from_slice(&(MAX_ARG_BYTES as u32).to_le_bytes());
            endless.extend(std::iter::repeat_n(b'a', MAX_ARG_BYTES));
        }
        assert!(read_message(&mut endless.as_slice()).is_err());
    }

    #[test]
    fn app_ids_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "../x", "a/b", "a b"] {
            assert!(opts(dir.path(), bad).start(&[]).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_open_to_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let app_id = "inlet-test-perm";
        let private = imp::private_dir(dir.path(), app_id).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            opts(dir.path(), app_id).start(&[]),
            Err(Error::Insecure(_))
        ));
        // A symlink in its place is refused too.
        std::fs::remove_dir(&private).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &private).unwrap();
        assert!(matches!(
            opts(dir.path(), app_id).start(&[]),
            Err(Error::Insecure(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn junk_on_the_socket_does_not_reach_the_app() {
        use std::os::unix::net::UnixStream;
        let dir = tempfile::tempdir().unwrap();
        let app_id = "inlet-test-junk";
        let Instance::Primary(rx) = opts(dir.path(), app_id).start(&[]).unwrap() else {
            panic!("primary");
        };
        let socket = imp::private_dir(dir.path(), app_id).unwrap().join("socket");
        let mut junk = UnixStream::connect(&socket).unwrap();
        junk.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        drop(junk);
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        // A real message afterwards still arrives.
        assert!(matches!(
            opts(dir.path(), app_id).start(&args(&["ok"])).unwrap(),
            Instance::Secondary
        ));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            args(&["ok"])
        );
    }
}
