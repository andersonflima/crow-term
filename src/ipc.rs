//! Local-only, length-framed IPC. No TCP listener and no shell interpolation.
use crate::model::{Request, Response};
use anyhow::{Context, Result, bail};
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
use interprocess::local_socket::{Stream, prelude::*};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Internal wire-format generation. It names the socket/pipe, so a GUI or CLI
/// never decodes frames of a daemon built from another model revision: a newer
/// client starts its own daemon and older sessions keep their own.
/// v3: kitty keyboard flags and modifyOtherKeys level in `Modes`.
/// v4: underline style and underline color in `Cell`.
/// v5: cell pixel metrics on resize, bell count, hyperlinks, alternate scroll.
pub const WIRE_VERSION: u32 = 5;
const DEADLINE: Duration = Duration::from_secs(5);
/// Unix sockets block in the kernel for at most this long per call, so the
/// frame deadline is still checked while no byte flows.
#[cfg(unix)]
const KERNEL_WAIT: Duration = Duration::from_millis(250);

/// Configure a connected stream for framed I/O on both ends.
///
/// Unix: blocking calls with kernel timeouts. Nonblocking reads used to poll
/// with a 1 ms sleep per would-block, and macOS socket buffers hold ~8 KiB:
/// a multi-pane snapshot of a few MiB needed thousands of sleeps and took
/// seconds, long enough to trip the 5 s deadline under load.
/// Windows: PIPE_NOWAIT (see [`FramedStream`]), polled.
pub fn prepare(stream: &Stream) -> Result<()> {
    #[cfg(unix)]
    {
        stream.set_nonblocking(false)?;
        stream.set_recv_timeout(Some(KERNEL_WAIT))?;
        stream.set_send_timeout(Some(KERNEL_WAIT))?;
    }
    #[cfg(windows)]
    stream.set_nonblocking(true)?;
    Ok(())
}

#[cfg(unix)]
pub type IpcListener = interprocess::local_socket::Listener;
#[cfg(windows)]
pub struct IpcListener(
    interprocess::os::windows::named_pipe::PipeListener<
        interprocess::os::windows::named_pipe::pipe_mode::Bytes,
        interprocess::os::windows::named_pipe::pipe_mode::Bytes,
    >,
);
#[cfg(windows)]
impl IpcListener {
    pub fn accept(&self) -> std::io::Result<Stream> {
        let stream =
            interprocess::os::windows::named_pipe::local_socket::Stream::from(self.0.accept()?);
        Ok(stream.into())
    }
}
#[cfg(windows)]
fn windows_pipe_basename() -> Result<String> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    runtime_dir()?.hash(&mut hash);
    Ok(format!("crow-term-v{WIRE_VERSION}-{:x}", hash.finish()))
}

/// The Windows PIPE_NOWAIT transport can report a successful zero-byte read
/// while still connected. Keep this quirk out of the generic framing reader,
/// where zero bytes must continue to mean EOF (files, cursors and Unix sockets).
pub struct FramedStream(Stream);
impl FramedStream {
    pub fn new(stream: Stream) -> Self {
        Self(stream)
    }
}
impl Read for FramedStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(not(windows))]
        return self.0.read(buffer);
        #[cfg(windows)]
        {
            if buffer.is_empty() {
                return Ok(0);
            }
            use std::os::windows::io::{AsHandle, AsRawHandle};
            use windows_sys::Win32::System::Pipes::PeekNamedPipe;
            let Stream::NamedPipe(pipe) = &self.0;
            let mut available = 0;
            // SAFETY: borrowed live pipe handle; no input/output buffer is used,
            // and the available-byte output points to initialized writable storage.
            let connected = unsafe {
                PeekNamedPipe(
                    pipe.as_handle().as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if connected == 0 {
                let error = std::io::Error::last_os_error();
                return if matches!(error.raw_os_error(), Some(109 | 232 | 233)) {
                    Ok(0)
                } else {
                    Err(error)
                };
            }
            if available == 0 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            // Request only queued bytes. An empty/oversized NOWAIT ReadFileEx
            // must never stall both sides while a larger frame is being sent.
            let length = buffer.len().min(available as usize);
            let count = self.0.read(&mut buffer[..length])?;
            if count == 0 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            Ok(count)
        }
    }
}
impl Write for FramedStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

pub fn runtime_dir() -> Result<PathBuf> {
    let path = match std::env::var_os("CROW_TERM_RUNTIME_DIR") {
        Some(p) => PathBuf::from(p),
        None => {
            let dirs = directories::ProjectDirs::from("dev", "crow", "crow-term")
                .context("Cannot locate user directories")?;
            dirs.runtime_dir().unwrap_or(dirs.cache_dir()).join("ipc")
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        if !path.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)?;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            bail!(
                "Runtime directory must be owned by this user, not a symlink, and mode 0700: {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn socket_file() -> String {
    format!("daemon-v{WIRE_VERSION}.sock")
}

fn socket_name() -> Result<interprocess::local_socket::Name<'static>> {
    #[cfg(unix)]
    {
        let path = runtime_dir()?.join(socket_file());
        if path.as_os_str().len() > 100 {
            bail!("IPC path too long; set CROW_TERM_RUNTIME_DIR to a shorter private directory");
        }
        Ok(path.to_fs_name::<GenericFilePath>()?.into_owned())
    }
    #[cfg(windows)]
    {
        Ok(windows_pipe_basename()?
            .to_ns_name::<GenericNamespaced>()?
            .into_owned())
    }
}

#[cfg(unix)]
pub fn listen() -> Result<IpcListener> {
    let options = ListenerOptions::new().name(socket_name()?);
    let listener = options
        .create_sync()
        .context("Cannot bind Crow daemon (already running or stale socket; use doctor)")?;
    // macOS does not implement chmod-at-bind. The containing directory is already
    // private (0700), so restricting the socket after bind never exposes it to peers.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            runtime_dir()?.join(socket_file()),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    Ok(listener)
}

#[cfg(windows)]
pub fn listen() -> Result<IpcListener> {
    use interprocess::os::windows::{
        named_pipe::{PipeListenerOptions, pipe_mode::Bytes},
        security_descriptor::SecurityDescriptor,
    };
    let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")?;
    let mut options =
        PipeListenerOptions::new().path(format!(r"\\.\pipe\{}", windows_pipe_basename()?));
    // The generic adapter requests 512-byte buffers. Bounded larger buffers
    // amortize scheduler round trips without changing peer authentication.
    // 1 MiB: a multi-pane snapshot crosses in a few polls, not hundreds of
    // timer-resolution sleeps (Windows sleeps are ~15 ms).
    options.input_buffer_size_hint = 1024 * 1024;
    options.output_buffer_size_hint = 1024 * 1024;
    options.security_descriptor = Some(SecurityDescriptor::deserialize(&sddl)?);
    Ok(IpcListener(
        options
            .create_duplex::<Bytes>()
            .context("Cannot bind Crow daemon")?,
    ))
}

pub fn check_peer(stream: &Stream) -> Result<()> {
    #[cfg(unix)]
    if stream.peer_creds()?.euid() != Some(rustix::process::geteuid().as_raw()) {
        bail!("IPC peer belongs to a different user");
    }
    #[cfg(windows)]
    check_windows_peer(
        stream
            .peer_creds()?
            .pid()
            .context("Named pipe peer has no process ID")?,
    )?;
    Ok(())
}

#[cfg(windows)]
fn check_windows_peer(pid: u32) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{EqualSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser},
        System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn token(process: HANDLE) -> Result<OwnedHandle> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: process is a live handle; output pointer is valid for this call.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut handle) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(OwnedHandle(handle))
    }
    fn user(token: HANDLE) -> Result<Vec<usize>> {
        let mut length = 0;
        // SAFETY: null/zero is the documented buffer-size query.
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut length);
        }
        if !(std::mem::size_of::<TOKEN_USER>()..=65536).contains(&(length as usize)) {
            bail!("Invalid token information size");
        }
        let mut data = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: usize storage gives TOKEN_USER alignment; length bytes are allocated.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                data.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(data)
    }
    // Hold the peer process handle through comparison to prevent PID-reuse races.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let process = OwnedHandle(process);
    let peer = user(token(process.0)?.0)?;
    let own = user(token(unsafe { GetCurrentProcess() })?.0)?;
    // SAFETY: both allocations contain successful TOKEN_USER results and stay alive.
    let (peer_sid, own_sid) = unsafe {
        (
            (*(peer.as_ptr().cast::<TOKEN_USER>())).User.Sid,
            (*(own.as_ptr().cast::<TOKEN_USER>())).User.Sid,
        )
    };
    if unsafe { IsValidSid(peer_sid) } == 0
        || unsafe { IsValidSid(own_sid) } == 0
        || unsafe { EqualSid(peer_sid, own_sid) } == 0
    {
        bail!("Named pipe peer belongs to a different Windows user");
    }
    Ok(())
}

fn transfer<T>(deadline: Instant, operation: impl FnMut() -> std::io::Result<T>) -> Result<T> {
    let mut operation = operation;
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
        }
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // Unix: the kernel timeout already waited. Windows: poll.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if cfg!(windows) {
                    std::thread::sleep(Duration::from_millis(1))
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_exact(reader: &mut impl Read, buffer: &mut [u8], deadline: Instant) -> Result<()> {
    let mut offset = 0;
    while offset < buffer.len() {
        let count = transfer(deadline, || {
            let result = reader.read(&mut buffer[offset..]);
            #[cfg(windows)]
            let result = result.map_err(|error| {
                // PIPE_NOWAIT reports ERROR_NO_DATA (232) when the pipe is
                // connected but no bytes are available. Interprocess preserves
                // that raw error rather than mapping it to WouldBlock.
                // Only reads retry it; a write-side 232 still means failure.
                if error.raw_os_error() == Some(232) {
                    std::io::Error::from(std::io::ErrorKind::WouldBlock)
                } else {
                    error
                }
            });
            result
        })?;
        if count == 0 {
            bail!("Truncated IPC frame");
        }
        offset += count;
    }
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let deadline = Instant::now() + DEADLINE;
    let mut length = [0; 4];
    read_exact(reader, &mut length, deadline)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        bail!("Invalid IPC frame length: {length}");
    }
    let mut data = vec![0; length];
    read_exact(reader, &mut data, deadline)?;
    Ok(postcard::from_bytes(&data)?)
}

pub fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let data = postcard::to_allocvec(value)?;
    if data.len() > MAX_FRAME {
        bail!("IPC frame exceeds limit");
    }
    let framed = [(data.len() as u32).to_le_bytes().as_slice(), &data].concat();
    let deadline = Instant::now() + DEADLINE;
    let mut offset = 0;
    while offset < framed.len() {
        // Bounded Windows writes amortize system calls within our 64-KiB hints;
        // the receiver drains between chunks. Unix sockets can accept the rest.
        let end = if cfg!(windows) {
            (offset + 64 * 1024).min(framed.len())
        } else {
            framed.len()
        };
        let count = transfer(deadline, || {
            let count = writer.write(&framed[offset..end])?;
            // PIPE_NOWAIT writes can succeed with no progress when its byte
            // buffer is full. Preserve the offset and retry only on Windows.
            #[cfg(windows)]
            if count == 0 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            Ok(count)
        })?;
        if count == 0 {
            bail!("IPC peer stopped accepting data");
        }
        offset += count;
    }
    Ok(())
}

/// Written by the daemon when it is stopped on purpose (`crow-term stop`).
pub const STOPPED_MARKER: &str = "daemon.stopped";
const ALIVE_LOCK: &str = "daemon.alive";

/// Take the daemon's lifetime lock (held until the process exits).
pub fn hold_daemon_lock() -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(runtime_dir()?.join(ALIVE_LOCK))?;
    // A starter's liveness probe holds the lock for an instant: retry briefly
    // so that probe never makes a starting daemon give up.
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("A Crow daemon is already running for this runtime directory")
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("lock daemon lifetime file");
            }
        }
    }
}

/// Whether a daemon process holds the lifetime lock (alive, even if it does not
/// answer or accept right now).
pub fn daemon_alive() -> bool {
    let Ok(dir) = runtime_dir() else {
        return false;
    };
    let path = dir.join(ALIVE_LOCK);
    // No lock file but a socket: a daemon may run whose lock file was removed
    // (runtime-dir clean-up) or an older one without it. Unknown counts as
    // alive: never replace it on a guess.
    if !path.exists() {
        #[cfg(unix)]
        return dir.join(socket_file()).exists();
        // Windows: a pipe exists only while its daemon serves it.
        #[cfg(not(unix))]
        return false;
    }
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) else {
        return true;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

pub fn rpc(request: &Request) -> Result<Response> {
    let stream = Stream::connect(socket_name()?).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ) {
            anyhow::Error::new(crate::link::Unreachable(error))
        } else {
            anyhow::Error::new(crate::link::NotSent(error.into()))
        }
    })?;
    let not_sent = |error: anyhow::Error| anyhow::Error::new(crate::link::NotSent(error));
    check_peer(&stream)
        .context("authenticate IPC server")
        .map_err(not_sent)?;
    prepare(&stream)
        .context("configure client IPC stream")
        .map_err(not_sent)?;
    let mut stream = FramedStream::new(stream);
    write_frame(&mut stream, request)
        .context("write IPC request")
        .map_err(not_sent)?;
    let response = read_frame(&mut stream).context("read IPC response")?;
    if let Response::Error(message) = response {
        return Err(anyhow::Error::new(crate::link::Refused(message)));
    }
    Ok(response)
}

/// Make sure a daemon serves this runtime directory, starting one only when
/// none accepts connections. A daemon that is merely slow to answer is never
/// replaced (that would orphan its sessions): its replies are awaited.
pub fn ensure_daemon(config: Option<&std::path::Path>) -> Result<()> {
    start_daemon(config, true)
}

/// Like [`ensure_daemon`], for a GUI whose daemon disappeared: it never starts
/// one while a daemon process is alive, nor after `crow-term stop`.
pub fn restart_daemon(config: Option<&std::path::Path>) -> Result<()> {
    start_daemon(config, false)
}

/// Why a GUI must not restart the daemon.
#[derive(Debug)]
pub enum NoRestart {
    StoppedOnPurpose,
    StillAlive,
}
impl std::fmt::Display for NoRestart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NoRestart::StoppedOnPurpose => "the daemon was stopped with `crow-term stop`",
            NoRestart::StillAlive => "the daemon process is alive but not accepting connections",
        })
    }
}
impl std::error::Error for NoRestart {}

fn start_daemon(config: Option<&std::path::Path>, user_command: bool) -> Result<()> {
    use crate::link::{Failure, classify};
    use std::process::{Command, Stdio};
    let busy_until = Instant::now() + Duration::from_secs(20);
    loop {
        match rpc(&Request::List) {
            Ok(_) => return Ok(()),
            Err(error) if classify(&error) == Failure::Unreachable => break,
            Err(error) if Instant::now() >= busy_until => {
                return Err(error).context("Crow daemon is running but not answering");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    let runtime = runtime_dir()?;
    if !user_command && runtime.join(STOPPED_MARKER).exists() {
        return Err(NoRestart::StoppedOnPurpose.into());
    }
    // One starter at a time per runtime directory (several GUIs or CLIs can
    // notice a stopped daemon together): removing a stale socket must never
    // race with another starter's fresh one.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(runtime.join("daemon.lock"))?;
    lock.lock().context("lock daemon start")?;
    if rpc(&Request::List).is_ok() {
        return Ok(());
    }
    // A live daemon that refuses connections (full accept backlog, stopped by a
    // debugger) keeps its socket and sessions: wait for it, never replace it.
    let alive_until = Instant::now() + Duration::from_secs(20);
    while daemon_alive() {
        if !user_command || Instant::now() >= alive_until {
            return Err(NoRestart::StillAlive.into());
        }
        std::thread::sleep(Duration::from_millis(200));
        if rpc(&Request::List).is_ok() {
            return Ok(());
        }
    }
    let _ = std::fs::remove_file(runtime.join(STOPPED_MARKER));
    #[cfg(unix)]
    {
        // No daemon process holds the lifetime lock (checked under the start
        // lock): the socket file is left over from one that did not exit cleanly.
        let socket = runtime.join(socket_file());
        if socket.exists() {
            match Stream::connect(socket_name()?) {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(&socket).context("remove stale daemon socket")?;
                }
                _ => {}
            }
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options.open(runtime.join("daemon.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    if let Some(path) = config {
        command.arg("--config").arg(path);
    }
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x00000200);
    }
    let mut child = command.spawn().context("Could not start Crow daemon")?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if rpc(&Request::List).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "Daemon exited {status}; inspect {}",
                runtime.join("daemon.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "Daemon startup timed out; log: {}",
        runtime.join("daemon.log").display()
    )
}

pub fn wake_listener() {
    if let Ok(name) = socket_name() {
        let _ = Stream::connect(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_deadline_stops_even_interrupted_operations() {
        assert!(
            transfer::<()>(Instant::now(), || Err(
                std::io::ErrorKind::Interrupted.into()
            ))
            .is_err()
        );
    }
    #[cfg(windows)]
    #[test]
    fn windows_empty_nowait_read_retries_but_eof_stays_terminal() {
        struct Delayed {
            empty: bool,
            data: std::io::Cursor<Vec<u8>>,
        }
        impl Read for Delayed {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.empty {
                    self.empty = false;
                    Err(std::io::Error::from_raw_os_error(232))
                } else {
                    self.data.read(b)
                }
            }
        }
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Request::List).unwrap();
        let mut reader = Delayed {
            empty: true,
            data: std::io::Cursor::new(bytes),
        };
        assert!(matches!(
            read_frame::<Request>(&mut reader).unwrap(),
            Request::List
        ));
        assert!(read_frame::<Request>(&mut reader).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn windows_large_frames_use_bounded_pipe_writes() {
        struct Capture {
            max: usize,
            data: Vec<u8>,
        }
        impl Write for Capture {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.max = self.max.max(b.len());
                self.data.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let value = vec![42u8; 200000];
        let mut writer = Capture {
            max: 0,
            data: Vec::new(),
        };
        write_frame(&mut writer, &value).unwrap();
        assert!(writer.max <= 64 * 1024);
        assert_eq!(
            read_frame::<Vec<u8>>(&mut writer.data.as_slice()).unwrap(),
            value
        );
    }
    #[cfg(windows)]
    #[test]
    fn windows_no_progress_write_retries_without_duplicate_bytes() {
        struct Delayed {
            empty: bool,
            data: Vec<u8>,
        }
        impl Write for Delayed {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if self.empty {
                    self.empty = false;
                    Ok(0)
                } else {
                    let n = b.len().min(2);
                    self.data.extend_from_slice(&b[..n]);
                    Ok(n)
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Delayed {
            empty: true,
            data: Vec::new(),
        };
        write_frame(&mut writer, &Request::List).unwrap();
        assert!(matches!(
            read_frame::<Request>(&mut writer.data.as_slice()).unwrap(),
            Request::List
        ));
    }
    #[test]
    fn roundtrip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Request::List).unwrap();
        let request: Request = read_frame(&mut bytes.as_slice()).unwrap();
        assert!(matches!(request, Request::List));
    }
    #[test]
    fn rejects_huge_frames_before_allocation() {
        assert!(read_frame::<Request>(&mut u32::MAX.to_le_bytes().as_slice()).is_err());
    }
    #[test]
    fn rejects_truncated_frames() {
        assert!(read_frame::<Request>(&mut [4, 0, 0, 0, 1].as_slice()).is_err());
    }
}
