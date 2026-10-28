//! Local-only, length-framed IPC. No TCP listener and no shell interpolation.
use crate::model::{Request, Response};
use anyhow::{Context, Result, bail};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::{ListenerOptions, Stream, prelude::*};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(5);

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

fn socket_name() -> Result<interprocess::local_socket::Name<'static>> {
    #[cfg(unix)]
    {
        let path = runtime_dir()?.join("daemon-v2.sock");
        if path.as_os_str().len() > 100 {
            bail!("IPC path too long; set CROW_TERM_RUNTIME_DIR to a shorter private directory");
        }
        Ok(path.to_fs_name::<GenericFilePath>()?.into_owned())
    }
    #[cfg(windows)]
    {
        // Separate names for separate profiles/test runtime roots.
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        runtime_dir()?.hash(&mut hash);
        Ok(format!("crow-term-v2-{:x}", hash.finish())
            .to_ns_name::<GenericNamespaced>()?
            .into_owned())
    }
}

pub fn listen() -> Result<interprocess::local_socket::Listener> {
    let options = ListenerOptions::new().name(socket_name()?);
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::{
            local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
        };
        // Protected ACL: object owner and SYSTEM only, not all local users.
        let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")?;
        let sd = SecurityDescriptor::deserialize(&sddl)?;
        options.security_descriptor(sd)
    };
    let listener = options
        .create_sync()
        .context("Cannot bind Crow daemon (already running or stale socket; use doctor)")?;
    // macOS does not implement chmod-at-bind. The containing directory is already
    // private (0700), so restricting the socket after bind never exposes it to peers.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            runtime_dir()?.join("daemon-v2.sock"),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    Ok(listener)
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
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1))
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
        let count = transfer(deadline, || {
            let count = writer.write(&framed[offset..])?;
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

pub fn rpc(request: &Request) -> Result<Response> {
    let mut stream = Stream::connect(socket_name()?).context("Crow daemon is not running")?;
    check_peer(&stream).context("authenticate IPC server")?;
    stream
        .set_nonblocking(true)
        .context("set client IPC nonblocking mode")?;
    write_frame(&mut stream, request).context("write IPC request")?;
    let response = read_frame(&mut stream).context("read IPC response")?;
    if let Response::Error(message) = &response {
        bail!("{message}");
    }
    Ok(response)
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
