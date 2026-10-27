# deb2rpm

> Convert Debian `.deb` packages → Fedora `.rpm` and install with `dnf`. No root build; user-space conversion with `fakeroot`.

📋 [Tree](#-contents) · 🚀 [Quickstart](#-quickstart) · 🧾 [Cheat Sheet](#-cheat-sheet) · 📖 [Usage](#-usage) · 🔧 [Conversion Details](#-conversion-behavior) · 🐛 [Audit / Issues](#-audit-findings) · 📚 [Development](#-development)

---

## 📋 Contents (Jump Tree)

```
deb2rpm/
├── 🚀 Quickstart          → convert, build, install in 30 sec
├── 🧾 Cheat Sheet          → flags, commands, common patterns
├── 📖 Usage                → CLI options & install modes
├── 🔧 Conversion Behavior  → what it fixes, what it skips
├── 💾 Work Files & Logs    → troubleshooting & cleanup
├── 🐛 Audit Findings       → known issues (patched / open)
└── 📚 Development          → build & test
```

---

## 🚀 Quickstart

```sh
# 1. Install conversion tools (Fedora)
sudo dnf install alien fakeroot dpkg rpm-build glib2 binutils

# 2. Build & install deb2rpm
bash install.sh          # installs to ~/.local/bin/deb2rpm

# 3. Convert + install a .deb
deb2rpm app.deb           # asks before installing

# 4. Just make the .rpm (no install)
deb2rpm -n app.deb

# 5. Save RPM elsewhere + install
deb2rpm -o ./rpms app.deb
```

💡 **Run as your normal user** — `sudo` is only used for `dnf` installation. The desktop entry (`Convert & install`) launches in a terminal with `--pause`.

---

## 🧾 Cheat Sheet

| Task | Command |
|---|---|
| Convert + install | `deb2rpm app.deb` |
| Build RPM only (`-n`) | `deb2rpm -n app.deb` |
| Save RPM to dir (`-o`) | `deb2rpm -o ./rpms app.deb` |
| Skip dnf confirmation (`-y`) | `deb2rpm -y app.deb` |
| Allow downgrade | `deb2rpm --allow-downgrade app.deb` |
| Skip maintainer scripts | `deb2rpm --no-scripts app.deb` |
| Force different arch | `deb2rpm --force-arch app.deb` |
| Keep build files | `deb2rpm --keep-work app.deb` |
| Delete work (no Trash) | `deb2rpm --delete-work app.deb` |
| Pause before exit | `deb2rpm --pause app.deb` |

⚡ **One `.deb` per run.** `--keep-work` and `--delete-work` conflict. Use `--` before filenames starting with `-`.

---

## 📖 Usage

Convert and install:

```sh
deb2rpm app.deb
```

Run as your normal user. The tool requests elevated privileges only when installing and checking script status. `dnf` asks for confirmation unless `-y` is supplied.

Build an RPM without installing (saved next to `.deb` by default):

```sh
deb2rpm -n app.deb
```

Choose output directory:

```sh
deb2rpm -n -o ./rpms app.deb
```

Install and keep a separate RPM copy:

```sh
deb2rpm -o ./rpms app.deb
```

### Option Table

| Option | Behavior |
|---|---|
| `-n`, `--build-only` | Build RPM, don't install |
| `-o`, `--output DIR` | Save RPM copy in `DIR` |
| `-y`, `--yes` | Skip `dnf` confirmation |
| `--allow-downgrade` | Replace newer installed version |
| `--no-scripts` | Skip Debian maintainer scripts |
| `--force-arch` | Override architecture checks |
| `--keep-work` | Retain temporary build dir |
| `--delete-work` | Delete work (no Trash) |
| `--pause` | Wait for Enter before exit |
| `-h`, `--help` | Show usage |

Only one `.deb` is accepted per invocation.

---

## 🔧 Conversion Behavior

- **Repairs empty RPM summaries** and sets packaged file ownership to `root:root`.
- **Removes conflicting shared directory claims** (`/`, `/usr`, etc.) before installation. Ownership checks batch relevant paths through RPM's file index, then verify exact filenames and every owner in the selected package headers. Query failures remain distinct from unowned paths. Build-only mode retains directory entries except `/`.
- **Translates Debian maintainer-script arguments** for install / upgrade / configure / remove. Replaces `alien`'s predictable `/tmp/alien.$$` with a private `mktemp` dir.
- **Preserves automatic RPM requirements / provides** so `dnf` resolves the full transaction. For `x86_64` and `aarch64`, known Node addon prebuild layouts are checked: an incompatible architecture or musl variant is excluded from dependency scanning only when a matching target-architecture glibc ELF is present. Each exclusion is logged and also applies to the advisory library scan.
- **Checks file ownership conflicts** before installation and blocks downgrades unless `--allow-downgrade` is set.
- **Reinstall behavior** when the built version matches the installed version.
- **Advisory ELF library inspection** (no binary execution) checks shared-library requirements without running packaged binaries.

⚠️ Conversion does **not** guarantee a Debian app works on Fedora. `Depends` entries are shown but not translated into RPM dependencies. Scripts may require Debian-specific behavior. `--no-scripts` skips them but also skips their setup work. Use trusted sources only — installation runs maintainer scripts with elevated privileges.

---

## 💾 Work Files & Troubleshooting

Work directories live under `$XDG_CACHE_HOME/deb2rpm` (or `~/.cache/deb2rpm`). Each conversion writes a `deb2rpm.log` in its work directory.

DNF uses a separate terminal screen and returns to the original progress row when finished. Interactive output is captured through a genuine output terminal with matching window dimensions, preserving terminal rendering and resize behavior. Its stdout and stderr are retained in the conversion log, including failure diagnostics; input stays attached to the original terminal, and terminal input and passwords are not recorded. With `--pause`, the close prompt disables text echo, ignores bracketed paste, discards queued input, and restores the saved terminal settings when closing or interrupted. TTY pause handling uses `/usr/bin/stty` from coreutils.

- **Success** → work moved to Trash (unless `-o` saved a copy or build-only mode). 
- **Failure** → work retained for inspection. 
- **Interrupted** → cleaned up by default; stale `.in-progress` directories handled on next run. Use `--keep-work` to retain, `--delete-work` to skip Trash.

If interrupted during `dnf`, check package state before retrying:

```sh
rpm -q PACKAGE_NAME
```

---

## 🐛 Audit Findings

Two audit reports cover `src/main.rs`, `install.sh`, and `Cargo.toml`:

- `docs/audit-script.md` — original review (2026-10-13). **Priority:** repair dependency lookup (H-1), replace the lock, then script compatibility and library check.
- `audit.md` — updated review (2026-10-22, **PATCHED** in v1.0.2). Four findings fixed: package metadata execution (P1), descendant hang (P2), FIFO blocking library scan (P2), pause prompt ignoring termination (P3).

Both reports include reproduction steps, fix directions, and verification notes.

---

## 📚 Development

Build without installing:

```sh
blaze build --release
./target/release/deb2rpm --help
```

Run tests:

```sh
blaze test
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -q
```

Install from repo:

```sh
bash install.sh
```

The installer builds the release binary to `~/.local/bin/deb2rpm`, installs a `deb2rpm-terminal` launcher beside it, and adds a desktop entry at `~/.local/share/applications/deb2rpm.desktop` (`XDG_BIN_HOME` / `XDG_DATA_HOME` override these). It uses Blaze when available, otherwise Cargo. The launcher respects a `TERMINAL` executable override, then uses `xdg-terminal-exec` when installed. Without that utility, it reads XDG terminal preference lists for common terminals, including Kitty, instead of choosing an arbitrary installed terminal. It waits for Enter after conversion (`--pause`). Custom terminal desktop entries should use `xdg-terminal-exec`; `TERMINAL` must name an executable, without shell arguments.

---

*Built with Rust — no other Rust dependencies.*
