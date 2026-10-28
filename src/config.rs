use crate::model::ShellSpec;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub ligatures: bool,
    pub scrollback: usize,
    pub background: String,
    pub foreground: String,
    pub selection: String,
    pub cursor: String,
    pub palette: Vec<String>,
    pub shell: Option<String>,
    pub shell_args: Vec<String>,
    pub prefix: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: "Crowquill Mono".into(),
            font_size: 18.0,
            line_height: 1.25,
            ligatures: true,
            scrollback: 10000,
            background: "#FFFFFF".into(),
            foreground: "#444444".into(),
            selection: "#DADADA".into(),
            cursor: "#000000".into(),
            palette: [
                "#000000", "#3C3C3C", "#4E4E4E", "#606060", "#525252", "#444444", "#363636",
                "#8A8A8A", "#6C6C6C", "#2A2A2A", "#383838", "#4A4A4A", "#3E3E3E", "#303030",
                "#202020", "#000000",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            shell: None,
            shell_args: vec![],
            prefix: "Ctrl-t".into(),
        }
    }
}
impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(directories::ProjectDirs::from("dev", "crow", "crow-term")
            .context("Cannot locate config directory")?
            .config_dir()
            .join("config.toml"))
    }
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let default = Self::path()?;
        let path = path.unwrap_or(&default);
        let config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("Invalid config {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path == default => {
                Self::default()
            }
            Err(error) => return Err(error.into()),
        };
        Self::validate(&config)?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.font_size.is_finite() && (8.0..=72.0).contains(&self.font_size),
            "font_size must be8..72"
        );
        ensure!(
            self.line_height.is_finite() && (1.0..=2.0).contains(&self.line_height),
            "line_height must be1..2"
        );
        ensure!(self.scrollback <= 100000, "scrollback must be <=100000");
        ensure!(
            !self.font_family.is_empty() && self.font_family.len() <= 200,
            "Invalid font_family"
        );
        ensure!(
            self.palette.len() == 16,
            "palette must contain exactly16 colors"
        );
        for color in [
            &self.background,
            &self.foreground,
            &self.selection,
            &self.cursor,
        ]
        .into_iter()
        .chain(self.palette.iter())
        {
            ensure!(
                color.len() == 7
                    && color.starts_with("#")
                    && color[1..].bytes().all(|b| b.is_ascii_hexdigit()),
                "Colors must be #RRGGBB"
            );
        }
        ensure!(
            self.prefix == "Ctrl-t" || self.prefix == "none",
            "prefix must be Ctrl-t or none"
        );
        Ok(())
    }
    pub fn shell_spec(
        &self,
        cwd: Option<String>,
        tmux: Option<String>,
    ) -> anyhow::Result<ShellSpec> {
        if let Some(session) = tmux {
            crate::server::validate_name(&session)?;
            let (program, args) = if cfg!(windows) {
                (
                    which::which("wsl.exe")?,
                    vec![
                        "--exec".into(),
                        "tmux".into(),
                        "new-session".into(),
                        "-A".into(),
                        "-s".into(),
                        session,
                    ],
                )
            } else {
                (
                    which::which("tmux")?,
                    vec!["new-session".into(), "-A".into(), "-s".into(), session],
                )
            };
            return Ok(ShellSpec {
                program: program.to_string_lossy().into_owned(),
                args,
                cwd,
            });
        }
        let program = match &self.shell {
            Some(s) => s.clone(),
            None => default_shell(),
        };
        Ok(ShellSpec {
            program,
            args: self.shell_args.clone(),
            cwd,
        })
    }
}
fn default_shell() -> String {
    if cfg!(windows) {
        "powershell.exe".into()
    } else {
        which::which("fish")
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_valid_and_roundtrip() {
        let c = Config::default();
        c.validate().unwrap();
        let text = toml::to_string(&c).unwrap();
        toml::from_str::<Config>(&text).unwrap().validate().unwrap();
    }
    #[test]
    fn rejects_invalid_config() {
        let mut c = Config {
            font_size: f32::NAN,
            ..Config::default()
        };
        assert!(c.validate().is_err());
        c.font_size = 18.0;
        c.palette.clear();
        assert!(c.validate().is_err());
    }
    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<Config>("font_szie=18").is_err());
    }
}
