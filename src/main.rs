use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crow_term::{config::Config, ipc, model::*, server, ui};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Parser)]
#[command(version, about = "Crow Term: native terminal and persistent sessions")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true, default_value = "main")]
    session: String,
    #[arg(long)]
    shell: Option<String>,
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    /// Run real tmux and read its existing config; native prefix is disabled.
    #[arg(long, global = true)]
    tmux: Option<String>,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    /// Print the bytes the GUI sends for a key (test aid; not a stable interface).
    #[command(hide = true)]
    DebugEncodeKey {
        /// Character or Enter/Tab/Backspace/Escape/Space/Up/Down/Left/Right/F1/F3/F5.
        key: String,
        /// Kitty keyboard flags the application enabled (0 = legacy).
        #[arg(long, default_value_t = 0)]
        flags: u8,
        /// xterm modifyOtherKeys level.
        #[arg(long, default_value_t = 0)]
        modify_other_keys: u8,
        /// shift, ctrl, alt (repeatable).
        #[arg(long = "mod")]
        mods: Vec<String>,
        #[arg(long)]
        release: bool,
    },
    /// Open/reattach a GUI. Arguments after -- execute a program without a shell.
    Start {
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Attach GUI to an existing session.
    Attach {
        name: String,
    },
    /// Create a persistent session without opening a GUI.
    NewSession {
        name: String,
    },
    ListSessions,
    NewWindow,
    SplitPane {
        pane: u64,
        #[arg(long)]
        window: u64,
        #[arg(long)]
        horizontal: bool,
    },
    RenameWindow {
        window: u64,
        name: String,
    },
    ResizePane {
        pane: u64,
        #[arg(long)]
        rows: u16,
        #[arg(long)]
        cols: u16,
    },
    ClosePane {
        pane: u64,
        #[arg(long)]
        window: u64,
    },
    SendKeys {
        pane: u64,
        text: String,
        #[arg(long)]
        enter: bool,
        /// Seconds to wait for a busy or frozen daemon (0: until it exits).
        #[arg(long, default_value_t = 30)]
        wait: u64,
    },
    CapturePane {
        pane: u64,
        #[arg(long)]
        json: bool,
    },
    KillSession {
        name: String,
    },
    /// Stop daemon; --force explicitly terminates all sessions.
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// Foreground daemon (normally autostarted).
    Daemon,
    /// Print effective config or its expected path. Never modifies dotfiles.
    Config {
        #[arg(long)]
        path: bool,
    },
    /// Inspect local tools, daemon and paths without installing anything.
    Doctor,
    /// Run isolated, opt-in fish/zsh profile setup shipped beside this binary.
    ShellSetup {
        /// Trusted checkout/archive root, explicitly selected instead of guessing from CWD.
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Bundled locations only: next to the invoked path (portable archive, which may be
/// a symlink), next to the resolved executable (Homebrew `bin` links), and the
/// macOS app bundle's `Contents/Resources` (signed bundles keep data out of `MacOS`).
fn bundled_shell_setup_candidates() -> Result<Vec<PathBuf>> {
    shell_setup_candidates_for(&std::env::current_exe()?)
}
fn shell_setup_candidates_for(invoked: &Path) -> Result<Vec<PathBuf>> {
    let mut bases = vec![
        invoked
            .parent()
            .context("Executable directory missing")?
            .to_path_buf(),
    ];
    if let Some(resolved) = std::fs::canonicalize(invoked)
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
    {
        // Only a real macOS bundle layout (`X.app/Contents/MacOS`) may look in
        // `../Resources`; elsewhere that path could belong to another user.
        if cfg!(target_os = "macos")
            && resolved.ends_with("Contents/MacOS")
            && resolved
                .parent()
                .and_then(Path::parent)
                .and_then(Path::extension)
                .is_some_and(|ext| ext == "app")
            && let Some(contents) = resolved.parent()
        {
            bases.push(contents.join("Resources"));
        }
        bases.push(resolved);
    }
    Ok(bases
        .into_iter()
        .map(|base| base.join("shell/setup.py"))
        .collect())
}

fn created(response: Response) -> Result<(u64, u64)> {
    if let Response::Created { window, pane } = response {
        Ok((window, pane))
    } else {
        bail!("Unexpected create response")
    }
}
fn shell(config: &Config, cli: &Cli) -> Result<ShellSpec> {
    let cwd = cli.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    let mut spec = config.shell_spec(cwd, cli.tmux.clone())?;
    if cli.tmux.is_none()
        && let Some(program) = &cli.shell
    {
        spec.program = program.clone();
    }
    Ok(spec)
}
fn print_response(response: Response) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    if cli.tmux.is_some() {
        config.prefix = "none".into();
    }
    match cli.command.as_ref() {
        Some(Action::Daemon) => return server::run(config),
        Some(Action::Config { path }) => {
            if *path {
                println!(
                    "{}",
                    cli.config.clone().unwrap_or(Config::path()?).display()
                );
            } else {
                println!("{}", toml::to_string_pretty(&config)?);
            }
            return Ok(());
        }
        Some(Action::Doctor) => {
            println!(
                "Crow Term {} / {}-{}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            println!(
                "config: {}",
                cli.config.clone().unwrap_or(Config::path()?).display()
            );
            println!("runtime: {}", ipc::runtime_dir()?.display());
            for tool in ["fish", "zsh", "tmux", "nvim", "vim", "wsl.exe"] {
                println!(
                    "{tool}: {}",
                    which::which(tool)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "not found (optional)".into())
                );
            }
            println!(
                "daemon: {}",
                if ipc::rpc(&Request::List).is_ok() {
                    "running"
                } else {
                    "not connected"
                }
            );
            println!("renderer: native software framebuffer; OpenType shaping; no WebView");
            println!(
                "osc52: {} (terminal output may {}; reading the clipboard is never allowed)",
                match config.terminal.osc52 {
                    crow_term::config::Osc52Mode::Copy => "copy",
                    crow_term::config::Osc52Mode::Off => "off",
                },
                match config.terminal.osc52 {
                    crow_term::config::Osc52Mode::Copy => "set the clipboard",
                    crow_term::config::Osc52Mode::Off => "not access the clipboard",
                }
            );
            return Ok(());
        }
        Some(Action::ShellSetup { source, args }) => {
            let candidates = match source {
                Some(source) => vec![source.join("shell/setup.py")],
                None => bundled_shell_setup_candidates()?,
            };
            let Some(script) = candidates.into_iter().find(|path| path.is_file()) else {
                bail!(
                    "Bundled shell/setup.py missing; use the full archive, run python3 shell/setup.py explicitly, or pass --source /trusted/checkout"
                );
            };
            let python = which::which("python3")
                .or_else(|_| which::which("python"))
                .context("Python3 required only for shell setup")?;
            let status = Command::new(python).arg(script).args(args).status()?;
            if !status.success() {
                bail!("Shell setup failed: {status}");
            }
            return Ok(());
        }
        Some(Action::Stop { force }) => {
            return print_response(ipc::rpc(&Request::Shutdown { force: *force })?);
        }
        _ => {}
    }
    // Pure key encoding (test aid): never needs or starts the daemon.
    if let Some(Action::DebugEncodeKey {
        key,
        flags,
        modify_other_keys,
        mods,
        release,
    }) = &cli.command
    {
        use std::io::Write;
        use winit::keyboard::ModifiersState;
        let mut state = ModifiersState::empty();
        for name in mods {
            state |= match name.as_str() {
                "shift" => ModifiersState::SHIFT,
                "ctrl" => ModifiersState::CONTROL,
                "alt" => ModifiersState::ALT,
                other => bail!("unknown modifier {other}"),
            };
        }
        let modes = Modes {
            keyboard: *flags,
            modify_other_keys: *modify_other_keys,
            ..Modes::default()
        };
        let bytes = ui::encode_named_key(key, state, *release, &modes);
        std::io::stdout().write_all(&bytes)?;
        return Ok(());
    }
    ipc::ensure_daemon(cli.config.as_deref())?;
    match cli.command.as_ref() {
        Some(Action::ListSessions) => print_response(ipc::rpc(&Request::List)?),
        Some(Action::KillSession { name }) => print_response(ipc::rpc(&Request::KillSession {
            session: name.clone(),
        })?),
        Some(Action::NewSession { name }) => print_response(ipc::rpc(&Request::Ensure {
            session: name.clone(),
            shell: shell(&config, &cli)?,
            size: Size::default(),
        })?),
        Some(Action::NewWindow) => print_response(ipc::rpc(&Request::NewWindow {
            session: cli.session.clone(),
            shell: shell(&config, &cli)?,
            size: Size::default(),
        })?),
        Some(Action::SplitPane {
            pane,
            window,
            horizontal,
        }) => print_response(ipc::rpc(&Request::Split {
            session: cli.session.clone(),
            window: *window,
            pane: *pane,
            axis: if *horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            },
            shell: shell(&config, &cli)?,
            size: Size::default(),
        })?),
        Some(Action::RenameWindow { window, name }) => {
            print_response(ipc::rpc(&Request::RenameWindow {
                session: cli.session.clone(),
                window: *window,
                name: name.clone(),
            })?)
        }
        Some(Action::ResizePane { pane, rows, cols }) => {
            print_response(ipc::rpc(&Request::Resize {
                pane: *pane,
                size: Size {
                    rows: *rows,
                    cols: *cols,
                },
                cell: None,
            })?)
        }
        Some(Action::ClosePane { pane, window }) => {
            print_response(ipc::rpc(&Request::ClosePane {
                session: cli.session.clone(),
                window: *window,
                pane: *pane,
            })?)
        }
        Some(Action::SendKeys {
            pane,
            text,
            enter,
            wait,
        }) => {
            let mut bytes = text.as_bytes().to_vec();
            if *enter {
                bytes.push(b'\r');
            }
            // Like the GUI: waited for while the daemon lives, never a timeout.
            let limit = (*wait > 0).then(|| std::time::Duration::from_secs(*wait));
            print_response(ipc::rpc_patient_for(
                &Request::Input { pane: *pane, bytes },
                limit,
            )?)
        }
        Some(Action::CapturePane { pane, json }) => {
            let Response::View(view) = ipc::rpc(&Request::List)? else {
                bail!("Invalid list response")
            };
            let (session, window) = view
                .sessions
                .iter()
                .find_map(|s| {
                    s.windows
                        .iter()
                        .find(|w| w.layout.panes().contains(pane))
                        .map(|w| (s.name.clone(), w.id))
                })
                .context("Pane not found")?;
            let Response::View(view) = ipc::rpc(&Request::Watch {
                session,
                window,
                since: 0,
            })?
            else {
                bail!("Invalid capture response")
            };
            let screen = view
                .screens
                .into_iter()
                .find(|(id, _)| id == pane)
                .context("Pane missing from snapshot")?
                .1;
            if *json {
                println!("{}", serde_json::to_string_pretty(&screen)?);
            } else {
                for row in screen.cells {
                    println!(
                        "{}",
                        row.into_iter()
                            .filter(|c| !c.wide_spacer)
                            .map(|c| c.text)
                            .collect::<String>()
                            .trim_end()
                    );
                }
            }
            Ok(())
        }
        None | Some(Action::Start { .. }) | Some(Action::Attach { .. }) => {
            let session = if let Some(Action::Attach { name }) = &cli.command {
                name.clone()
            } else {
                cli.session.clone()
            };
            let mut spec = shell(&config, &cli)?;
            if let Some(Action::Start { command }) = &cli.command
                && let Some(program) = command.first()
            {
                spec.program = program.clone();
                spec.args = command[1..].to_vec();
            }
            let (window, pane) = if matches!(cli.command, Some(Action::Attach { .. })) {
                let Response::View(view) = ipc::rpc(&Request::List)? else {
                    bail!("Invalid list response")
                };
                let window = view
                    .sessions
                    .iter()
                    .find(|s| s.name == session)
                    .and_then(|s| s.windows.first())
                    .context("Session not found; use new-session")?;
                (window.id, window.layout.panes()[0])
            } else {
                created(ipc::rpc(&Request::Ensure {
                    session: session.clone(),
                    shell: spec.clone(),
                    size: Size::default(),
                })?)?
            };
            ui::run(config, cli.config.clone(), session, window, pane, spec)
        }
        _ => unreachable!("local commands handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_setup_resources_only_inside_an_app_bundle() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let lone = root.join("sub/crow-term");
        std::fs::create_dir_all(lone.parent().unwrap()).unwrap();
        std::fs::write(&lone, b"").unwrap();
        let candidates = shell_setup_candidates_for(&lone).unwrap();
        assert!(
            candidates
                .iter()
                .all(|c| !c.starts_with(root.join("Resources"))),
            "{candidates:?}"
        );
        let app = root.join("Crow Term.app/Contents/MacOS/crow-term");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&app, b"").unwrap();
        let candidates = shell_setup_candidates_for(&app).unwrap();
        let resources = root.join("Crow Term.app/Contents/Resources/shell/setup.py");
        assert_eq!(
            candidates.contains(&resources),
            cfg!(target_os = "macos"),
            "{candidates:?}"
        );
    }
}
