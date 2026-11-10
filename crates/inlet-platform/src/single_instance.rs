//! One running Inlet per user. A second start hands its command line (a deep link,
//! say) to the first and exits.
//!
//! - Unix: a socket in a folder only the user can open (`$XDG_RUNTIME_DIR`, or the
//!   per-user `$TMPDIR` on macOS), with an `flock` on a lock file deciding who is
//!   first. Both sides check the other end's user id (`SO_PEERCRED`/`getpeereid`).
//! - Windows: a named pipe whose owner and only allowed user is the current user,
//!   created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, refusing remote clients. The
//!   second instance checks that the pipe's owner is its own user before it sends
//!   anything, and connects with identification-level impersonation only.
//!
//! Messages are bounded: at most 32 arguments, 2 KiB each, 16 KiB in all, UTF-8.

use std::io::{self, Read, Write};
use std::sync::mpsc;

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"INLETSI1";
const MAX_ARGS: usize = 32;
const MAX_ARG_BYTES: usize = 2048;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const ACK: u8 = 0x06;

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
/// program name) to the running one.
pub fn single_instance(app_id: &str) -> Result<Instance> {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        let message = encode(args)?;
        imp::start(self, &message)
    }
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

/// Serves one connection: reads the message, passes it on, acknowledges.
fn serve(stream: &mut (impl Read + Write), tx: &mpsc::Sender<Vec<String>>) {
    match read_message(stream) {
        Ok(args) => {
            let _ = tx.send(args);
            let _ = stream.write_all(&[ACK]);
            let _ = stream.flush();
        }
        Err(err) => log::warn!("refused a message from another instance: {err}"),
    }
}

fn send(stream: &mut (impl Read + Write), message: &[u8]) -> Result<Instance> {
    stream.write_all(message)?;
    stream.flush()?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack)?;
    if ack[0] != ACK {
        return Err(Error::Os("the running instance did not acknowledge".into()));
    }
    Ok(Instance::Secondary)
}

#[cfg(unix)]
mod imp {
    use std::fs::{DirBuilder, File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{send, serve, Instance, SingleInstance};
    use crate::{Error, Result};

    fn uid() -> u32 {
        // SAFETY: getuid cannot fail.
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
            Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => return Err(err.into()),
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

    fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
        let fd = stream.as_raw_fd();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: `cred` and `len` are valid for the call.
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
                return Err(std::io::Error::last_os_error());
            }
            Ok(cred.uid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let (mut uid, mut gid) = (0, 0);
            // SAFETY: both out-pointers are valid.
            if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(uid)
        }
    }

    fn try_lock(file: &File) -> std::io::Result<bool> {
        // SAFETY: a valid descriptor; the lock lives as long as the file is open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(err)
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
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
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
                    let Ok(mut stream) = stream else { continue };
                    match peer_uid(&stream) {
                        Ok(peer) if peer == uid() => {}
                        _ => {
                            log::warn!("refused a connection from another user");
                            continue;
                        }
                    }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    serve(&mut stream, &tx);
                }
            })?;
        Ok(Instance::Primary(rx))
    }

    fn secondary(socket: &Path, message: &[u8]) -> Result<Instance> {
        // The first instance may hold the lock but not have bound its socket yet.
        let mut last = None;
        for _ in 0..50 {
            match UnixStream::connect(socket) {
                Ok(mut stream) => {
                    if peer_uid(&stream)? != uid() {
                        return Err(Error::Insecure(
                            "the running instance belongs to another user",
                        ));
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    return send(&mut stream, message);
                }
                Err(err) => last = Some(err),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(last.map_or(Error::Os("no running instance answered".into()), Error::Io))
    }
}

#[cfg(windows)]
mod imp {
    use std::io::{self, Read, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    use windows::core::{HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
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
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        READ_CONTROL, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{send, serve, Instance, SingleInstance};
    use crate::{Error, Result};

    /// An owned handle that is closed on drop. Pipe handles may be used from any thread.
    struct Handle(HANDLE);
    // SAFETY: a kernel handle is just a number; the pipe is only used by one thread at a time.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle is owned and closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    impl Read for Handle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut read = 0u32;
            // SAFETY: synchronous read into a valid buffer.
            unsafe { ReadFile(self.0, Some(buf), Some(&mut read), None) }
                .map_err(io::Error::other)?;
            Ok(read as usize)
        }
    }

    impl Write for Handle {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut written = 0u32;
            // SAFETY: synchronous write from a valid buffer.
            unsafe { WriteFile(self.0, Some(buf), Some(&mut written), None) }
                .map_err(io::Error::other)?;
            Ok(written as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            // SAFETY: valid pipe handle.
            unsafe { FlushFileBuffers(self.0) }.map_err(io::Error::other)
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
            // SAFETY: valid SID; the string is freed with LocalFree.
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
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                Some(&attrs),
            )
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if pipe.is_invalid() {
            let err = windows::core::Error::from_thread();
            if err.code() == ERROR_ACCESS_DENIED.to_hresult() {
                // The pipe exists: another instance (or a squatter, checked next).
                return secondary(&name, &user, message);
            }
            return Err(err.into());
        }
        primary(Handle(pipe))
    }

    fn primary(mut pipe: Handle) -> Result<Instance> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("inlet-single-instance".into())
            .spawn(move || loop {
                // SAFETY: valid pipe handle owned by this thread.
                let connected = unsafe { ConnectNamedPipe(pipe.0, None) };
                let ok = match connected {
                    Ok(()) => true,
                    Err(err) => err.code() == ERROR_PIPE_CONNECTED.to_hresult(),
                };
                if ok {
                    serve(&mut pipe, &tx);
                }
                // SAFETY: ends this client's session; the handle stays valid.
                unsafe {
                    let _ = DisconnectNamedPipe(pipe.0);
                }
            })?;
        Ok(Instance::Primary(rx))
    }

    fn secondary(name: &HSTRING, user: &UserSid, message: &[u8]) -> Result<Instance> {
        for _ in 0..50 {
            // SAFETY: opening a named pipe; identification level only, so the
            // server cannot act as this user.
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0 | READ_CONTROL.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0),
                    None,
                )
            };
            match opened {
                Ok(handle) => {
                    let mut pipe = Handle(handle);
                    check_owner(&pipe, user)?;
                    return send(&mut pipe, message);
                }
                Err(err) if err.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    // SAFETY: waits for the pipe instance to become free.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 2000) };
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        Err(Error::Os("no running instance answered".into()))
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
