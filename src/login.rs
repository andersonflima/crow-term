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
        "tcsh" | "csh" => {
            // csh-family accepts -l only as the sole argument: no -c with it.
            return None;
        }
        _ => format!("echo {MARKER}; /usr/bin/env -0"),
    };
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
    let text = String::from_utf8_lossy(output);
    let start = text.rfind(MARKER)? + MARKER.len();
    let body = text[start..].trim_start_matches(['\r', '\n']);
    let mut env = BTreeMap::new();
    for entry in body.split('\0') {
        if let Some((key, value)) = entry.split_once('=')
            && !key.is_empty()
            && !key.contains(['\n', ' '])
        {
            env.insert(OsString::from(key), OsString::from(value));
        }
    }
    (!env.is_empty() && env.contains_key(&OsString::from("PATH"))).then_some(env)
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
    let args = env_command(program).ok_or_else(|| format!("{program}: no known login mode"))?;
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut output);
        output
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program}: login environment not printed within {timeout:?}"
                ));
            }
        }
    }
    let output = reader.join().map_err(|_| "reader panicked")?;
    parse_env(&output)
        .ok_or_else(|| format!("{program}: no environment in the login shell's output"))
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
            ["-l", "-c", "echo CROW_LOGIN_ENV_BEGIN; /usr/bin/env -0"]
        );
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

    #[cfg(unix)]
    #[test]
    fn real_login_environment_of_sh_is_captured() {
        let env = capture_login_env("/bin/sh", Duration::from_secs(10)).unwrap();
        assert!(env.contains_key(&OsString::from("PATH")));
    }
}
