//! The user's login shell, the way their own terminal starts it.
//!
//! * Which shell: the login shell from the user database (`getpwuid`), then
//!   `$SHELL`, then `/bin/sh`; an explicit `shell` in the config wins. Never a
//!   PATH-based guess: an app started from Finder or the Dock has launchd's
//!   minimal environment, where PATH and SHELL are not the user's.
//! * How: as a login shell, so profiles, PATH set by `path_helper`/profile
//!   files and plugin managers load as in Terminal, iTerm or Ghostty. Each
//!   shell's own login switch is used (table in [`login_args`]).
//! * Environment: when the daemon itself was started outside a terminal
//!   (no `TERM`), it adopts the login environment captured once through the
//!   same shell ([`capture_login_env`]), so programs started without a shell
//!   (`crow-term start -- nvim`) also find the user's PATH.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the default shell came from (shown by `crow-term doctor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Config,
    UserDatabase,
    ShellVariable,
    Fallback,
}
impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Config => "config `shell`",
            Source::UserDatabase => "user database (login shell)",
            Source::ShellVariable => "$SHELL",
            Source::Fallback => "fallback",
        }
    }
}

/// The login shell recorded for the current user, if it is an executable file.
#[cfg(unix)]
pub fn user_database_shell() -> Option<String> {
    use std::ffi::CStr;
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: getpwuid_r writes into `entry` and `buffer`, both owned here and
    // large enough; `result` is null on error or when no entry exists.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_shell.is_null() {
        return None;
    }
    // SAFETY: pw_shell points into `buffer`, NUL-terminated by getpwuid_r.
    let shell = unsafe { CStr::from_ptr(entry.pw_shell) }
        .to_str()
        .ok()?
        .to_owned();
    executable(&shell).then_some(shell)
}
#[cfg(not(unix))]
pub fn user_database_shell() -> Option<String> {
    None
}

#[cfg(unix)]
fn executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    Path::new(path).is_absolute()
        && std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable(path: &str) -> bool {
    Path::new(path).is_file()
}

/// Selection order, with the sources injected so it is testable.
pub fn choose(
    configured: Option<&str>,
    user_database: Option<String>,
    shell_variable: Option<String>,
    is_executable: impl Fn(&str) -> bool,
) -> (String, Source) {
    if let Some(shell) = configured {
        return (shell.to_owned(), Source::Config);
    }
    if cfg!(windows) {
        return ("powershell.exe".into(), Source::Fallback);
    }
    if let Some(shell) = user_database {
        return (shell, Source::UserDatabase);
    }
    if let Some(shell) = shell_variable.filter(|shell| is_executable(shell)) {
        return (shell, Source::ShellVariable);
    }
    ("/bin/sh".into(), Source::Fallback)
}

pub fn default_shell(configured: Option<&str>) -> (String, Source) {
    choose(
        configured,
        user_database_shell(),
        std::env::var("SHELL").ok(),
        executable,
    )
}

fn shell_name(program: &str) -> String {
    // Both separators: a Windows path is named the same way on every host.
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_owned()
}

/// Arguments that start `program` as a login shell, or `None` when the shell
/// has no login mode (elvish, Windows PowerShell 5, cmd) or is unknown.
pub fn login_args(program: &str) -> Option<Vec<String>> {
    let flag = match shell_name(program).as_str() {
        // POSIX-family, csh-family, fish, nushell and xonsh all accept -l.
        "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" | "ksh93" | "mksh" | "pdksh" | "yash"
        | "fish" | "nu" | "xonsh" | "tcsh" | "csh" => "-l",
        // PowerShell 7 on any platform: -Login must be the first argument.
        "pwsh" | "pwsh-preview" => "-Login",
        _ => return None,
    };
    Some(vec![flag.to_owned()])
}

/// Command line that prints a marker, then the environment NUL-separated, in
/// the syntax of `program`, run as a login shell. `None` when the shell has no
/// login mode or no `-c`-style switch we know.
pub fn env_command(program: &str) -> Option<Vec<String>> {
    let name = shell_name(program);
    let mut args = login_args(program)?;
    let script = match name.as_str() {
        "nu" => format!("print {MARKER}; ^/usr/bin/env -0"),
        "pwsh" | "pwsh-preview" => format!("Write-Output {MARKER}; & /usr/bin/env -0"),
        // csh-family accepts -l only as its sole argument: no -c with it, so
        // its login environment is not captured (its panes are still login shells).
        "tcsh" | "csh" => return None,
        _ => format!("echo {MARKER}; /usr/bin/env -0"),
    };
    // zsh and bash read their interactive rc (.zshrc/.bashrc, where oh-my-zsh,
    // nvm and most PATH edits live) only when interactive: capture like the
    // pane runs them, as an interactive login shell. Prompt and plugin output
    // before the marker is ignored.
    if matches!(name.as_str(), "zsh" | "bash") {
        args.push("-i".into());
    }
    let switch = if name.starts_with("pwsh") {
        "-Command"
    } else {
        "-c"
    };
    args.push(switch.into());
    args.push(script);
    Some(args)
}

const MARKER: &str = "CROW_LOGIN_ENV_BEGIN";

/// Parse `env -0` output after the marker (profiles may print before it).
pub fn parse_env(output: &[u8]) -> Option<BTreeMap<OsString, OsString>> {
    let marker = MARKER.as_bytes();
    let start = output
        .windows(marker.len())
        .rposition(|window| window == marker)?
        + marker.len();
    let mut body = &output[start..];
    while let [b'\r' | b'\n', rest @ ..] = body {
        body = rest;
    }
    let mut env = BTreeMap::new();
    for entry in body.split(|byte| *byte == 0) {
        let Some(equals) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, value) = (&entry[..equals], &entry[equals + 1..]);
        if key.is_empty() || key.iter().any(|byte| matches!(byte, b'\n' | b' ')) {
            continue;
        }
        env.insert(os_string(key), os_string(value));
    }
    (!env.is_empty() && env.contains_key(&OsString::from("PATH"))).then_some(env)
}

/// Bytes as an OS string: exact on Unix (values need not be UTF-8).
fn os_string(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Variables the daemon keeps from its own environment.
fn keep_own(key: &str) -> bool {
    key.starts_with("CROW_TERM_")
        || matches!(
            key,
            "TERM" | "COLORTERM" | "TERM_PROGRAM" | "SHLVL" | "_" | "PWD" | "OLDPWD"
        )
}

/// The changes that make `current` look like `login` (login values win,
/// except the daemon's own `CROW_TERM_*` and terminal identity variables).
pub fn merge(
    current: &BTreeMap<OsString, OsString>,
    login: &BTreeMap<OsString, OsString>,
) -> Vec<(OsString, OsString)> {
    login
        .iter()
        .filter(|(key, value)| {
            !keep_own(&key.to_string_lossy()) && current.get(*key) != Some(*value)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Run `program` once as a login shell and return its environment. Bounded by
/// `timeout`; stdin is closed so a profile cannot wait for input.
pub fn capture_login_env(
    program: &str,
    timeout: Duration,
) -> Result<BTreeMap<OsString, OsString>, String> {
    capture_with(program, timeout, None)
}

fn capture_with(
    program: &str,
    timeout: Duration,
    home: Option<&Path>,
) -> Result<BTreeMap<OsString, OsString>, String> {
    let args = env_command(program).ok_or_else(|| {
        format!("{program}: login environment capture not supported for this shell")
    })?;
    let mut command = Command::new(program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = home {
        command.env("HOME", home);
    }
    // Own session (hence own process group, no controlling terminal): an
    // interactive zsh/bash cannot be stopped by job control for touching a
    // terminal, and on timeout the whole group goes, including background jobs
    // a profile started that would otherwise keep the pipe open.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid().map_err(std::io::Error::from)?;
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut output);
        let _ = sender.send(output);
    });
    let deadline = Instant::now() + timeout;
    // The output ends when every holder of the pipe exits, not only the shell.
    let output = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let kill_group = |child: &mut std::process::Child| {
        #[cfg(unix)]
        if let Some(group) = i32::try_from(child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let Ok(output) = output else {
        kill_group(&mut child);
        return Err(format!(
            "{program}: login environment not printed within {timeout:?}"
        ));
    };
    // The shell printed everything; reap it (and any straggler of its group).
    if !matches!(child.try_wait(), Ok(Some(_))) {
        kill_group(&mut child);
    }
    parse_env(&output)
        .ok_or_else(|| format!("{program}: no environment in the login shell's output"))
}

/// The environment a daemon started outside a terminal should run with: the
/// user's login environment merged over the current one. `None` when the
/// caller is in a terminal (`TERM` set: its environment is already the user's),
/// when login shells are disabled, or when capture fails (logged by caller).
pub fn daemon_environment(
    shell: &str,
    login_shell: bool,
    timeout: Duration,
) -> Option<Result<Vec<(OsString, OsString)>, String>> {
    if std::env::var_os("TERM").is_some() || cfg!(windows) || !login_shell {
        return None;
    }
    Some(capture_login_env(shell, timeout).map(|login| {
        let current = std::env::vars_os().collect();
        merge(&current, &login)
    }))
}

/// Whether `args` start the shell as a login shell.
pub fn is_login(program: &str, args: &[String]) -> bool {
    login_args(program).is_some_and(|login| !login.is_empty() && args.starts_with(&login))
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-l" | "--login" | "-Login" | "-login"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_then_user_database_then_shell_variable_then_sh() {
        let any = |_: &str| true;
        assert_eq!(
            choose(
                Some("/opt/x/fish"),
                Some("/bin/zsh".into()),
                Some("/bin/bash".into()),
                any
            ),
            ("/opt/x/fish".into(), Source::Config)
        );
        if cfg!(windows) {
            return;
        }
        assert_eq!(
            choose(None, Some("/bin/zsh".into()), Some("/bin/bash".into()), any),
            ("/bin/zsh".into(), Source::UserDatabase),
            "the user database wins over a PATH-less or stale $SHELL"
        );
        assert_eq!(
            choose(None, None, Some("/bin/bash".into()), any),
            ("/bin/bash".into(), Source::ShellVariable)
        );
        assert_eq!(
            choose(None, None, Some("/nope".into()), |_| false),
            ("/bin/sh".into(), Source::Fallback)
        );
    }

    #[test]
    fn every_known_shell_gets_its_own_login_switch() {
        for shell in [
            "/bin/sh",
            "/bin/bash",
            "/bin/zsh",
            "/usr/bin/dash",
            "/bin/ksh",
            "/usr/bin/mksh",
            "/opt/homebrew/bin/fish",
            "/usr/local/bin/nu",
            "/usr/bin/xonsh",
            "/bin/tcsh",
            "/bin/csh",
        ] {
            assert_eq!(login_args(shell), Some(vec!["-l".to_owned()]), "{shell}");
        }
        assert_eq!(
            login_args("/usr/local/bin/pwsh"),
            Some(vec!["-Login".to_owned()])
        );
        assert_eq!(
            login_args("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            Some(vec!["-Login".to_owned()])
        );
        for none in [
            "/usr/bin/elvish",
            "powershell.exe",
            "cmd.exe",
            "/usr/bin/python3",
        ] {
            assert_eq!(login_args(none), None, "{none}");
        }
    }

    #[test]
    fn env_commands_use_each_shell_syntax() {
        assert_eq!(
            env_command("/bin/zsh").unwrap(),
            [
                "-l",
                "-i",
                "-c",
                "echo CROW_LOGIN_ENV_BEGIN; /usr/bin/env -0"
            ]
        );
        assert_eq!(env_command("/bin/bash").unwrap()[..3], ["-l", "-i", "-c"]);
        assert_eq!(env_command("/bin/dash").unwrap()[..2], ["-l", "-c"]);
        assert_eq!(env_command("/opt/homebrew/bin/fish").unwrap()[0], "-l");
        assert!(env_command("/usr/local/bin/nu").unwrap()[2].starts_with("print "));
        let pwsh = env_command("/usr/local/bin/pwsh").unwrap();
        assert_eq!(&pwsh[..2], ["-Login", "-Command"]);
        assert_eq!(env_command("/bin/tcsh"), None);
        assert_eq!(env_command("/usr/bin/elvish"), None);
    }

    #[test]
    fn env_output_is_parsed_after_the_marker_only() {
        let output = b"Welcome from .zprofile\nPATH=/fake\nCROW_LOGIN_ENV_BEGIN\nPATH=/opt/homebrew/bin:/usr/bin\0HOME=/Users/u\0MULTI=a\nb\0\0";
        let env = parse_env(output).unwrap();
        assert_eq!(
            env[&OsString::from("PATH")],
            OsString::from("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(env[&OsString::from("MULTI")], OsString::from("a\nb"));
        assert!(parse_env(b"no marker PATH=/x\0").is_none());
        assert!(
            parse_env(b"CROW_LOGIN_ENV_BEGIN\nHOME=/u\0").is_none(),
            "PATH is required"
        );
    }

    #[test]
    fn merge_takes_login_values_but_keeps_the_daemon_identity() {
        let current: BTreeMap<OsString, OsString> = [
            ("PATH", "/usr/bin"),
            ("CROW_TERM_RUNTIME_DIR", "/rt"),
            ("HOME", "/u"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let login: BTreeMap<OsString, OsString> = [
            ("PATH", "/opt/homebrew/bin:/usr/bin"),
            ("CROW_TERM_RUNTIME_DIR", "/other"),
            ("HOME", "/u"),
            ("TERM", "dumb"),
            ("EDITOR", "nvim"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let changes = merge(&current, &login);
        assert_eq!(
            changes,
            vec![
                ("EDITOR".into(), "nvim".into()),
                ("PATH".into(), "/opt/homebrew/bin:/usr/bin".into())
            ]
        );
    }

    #[test]
    fn login_state_is_recognized_in_any_spelling() {
        assert!(is_login("/bin/zsh", &["-l".into()]));
        assert!(is_login("/bin/bash", &["--login".into()]));
        assert!(is_login("/usr/bin/pwsh", &["-Login".into()]));
        assert!(!is_login("/bin/zsh", &[]));
    }

    #[cfg(unix)]
    #[test]
    fn a_profile_that_leaves_a_background_job_cannot_hold_the_capture() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".profile"), "sleep 30 &\n").unwrap();
        let started = Instant::now();
        let captured = capture_with("/bin/sh", Duration::from_secs(2), Some(home.path()));
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
        assert!(
            captured.is_err(),
            "the background job holds stdout past the limit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_login_environment_of_sh_is_captured() {
        let env = capture_login_env("/bin/sh", Duration::from_secs(10)).unwrap();
        assert!(env.contains_key(&OsString::from("PATH")));
    }
}
