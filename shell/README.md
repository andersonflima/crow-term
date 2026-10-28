# Reproducible, opt-in shell profiles

Run `python3 shell/setup.py --help`. Requires Python 3.9+ on Unix/macOS/WSL.
Profiles are separate from personal startup files; installation and launching
are two explicit operations. Nothing calls `chsh`, a package manager, `curl|sh`,
`git clone`, Fisher, or a plugin's installer.

`vendor/` contains only the runtime source files and required license notices
listed in `plugins.lock.json`. The lock records official repository URLs,
immutable upstream commits, upstream archive SHA256 and every copied file's
SHA256. Files are unmodified, except that unrelated upstream files are omitted.
Setup verifies each selected vendored file before writing any target. No network
is used by setup, dry-run, or tests.

| Plugin | Upstream commit | License notice |
| --- | --- | --- |
| [Fisher](https://github.com/jorgebucaran/fisher) | `791da644d33d392216f6b1a9b5fc1e470db6d7f2` | `vendor/fisher/LICENSE.md` (MIT) |
| [fish z](https://github.com/jethrokuan/z) | `26a50962bc68f5cb60fc488ee008b3d4d5be75f4` | `vendor/z/LICENSE` (MIT) |
| [zsh-autosuggestions](https://github.com/zsh-users/zsh-autosuggestions) | `85919cd1ffa7d2d5412f6d3fe437ebdbeeec4fc5` | `vendor/zsh-autosuggestions/LICENSE` (MIT) |
| [zsh-syntax-highlighting](https://github.com/zsh-users/zsh-syntax-highlighting) | `e6683f20e5c38d3e5447f45845d04ddda75d0f23` | `vendor/zsh-syntax-highlighting/COPYING.md` (BSD-3-Clause) |

These exact official archives were downloaded and hashed on 2026-10-28. This
is a reproducibility pin, not a promise that upstream code is vulnerability-free.
To update: review upstream changes and licenses, fetch a chosen immutable commit
from its official repository, replace only this allowlist and regenerate/review
the SHA256 entries together. Changing a digest alone to suppress an unexplained
mismatch is not a valid update. Profiles installed by an older lock are never
overwritten: choose a new versioned prefix or back up and manage them explicitly.

The fuller source setup declared Fisher, z, Shellder, Tide v5 and Catppuccin fish,
while invoking Starship. Crow Term intentionally chooses only Fisher + z, native
fish editing/completions and one small built-in monochrome prompt. It does not
stack competing prompt engines. The zsh choice is a new minimal portable profile,
not a claim that these plugins were discovered in the owner's `.zshrc`.

See [shell integration](../docs/shell-integration.md) for launch semantics,
platform limits and the boundary between a terminal and a shell.
