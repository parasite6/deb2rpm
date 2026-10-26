# deb2rpm

Convert a compatible Debian `.deb` package into an RPM and install it on Fedora. Uses `alien`, builds with `fakeroot`, installs via `dnf`.

## Contents

- [Quickstart](#quickstart)
- [Cheat sheet](#cheat-sheet)
- [Requirements](#requirements)
- [Build and install](#build-and-install)
- [Usage](#usage)
- [Conversion behavior](#conversion-behavior)
- [Work files and troubleshooting](#work-files-and-troubleshooting)
- [Development](#development)

## Quickstart

```sh
# Install dependencies (Fedora)
sudo dnf install alien fakeroot dpkg rpm-build glib2 binutils

# Build and install deb2rpm
bash install.sh

# Convert and install a .deb
deb2rpm app.deb

# Build RPM only (no install)
deb2rpm -n app.deb
```

## Cheat sheet

| Option | Behavior |
| --- | --- |
| `-n`, `--build-only` | Build RPM, don't install |
| `-o`, `--output DIR` | Save RPM copy to `DIR` |
| `-y`, `--yes` | Skip `dnf` confirmation |
| `--allow-downgrade` | Allow newer version replacement |
| `--no-scripts` | Skip maintainer scripts |
| `--force-arch` | Override architecture checks |
| `--keep-work` | Keep temp build directory |
| `--delete-work` | Delete work instead of Trash |
| `--pause` | Wait for Enter before exit |
| `-h`, `--help` | Show usage |

Run as normal user (not `sudo`). Only one `.deb` per invocation.

## Requirements

- Linux with Rust and Cargo to build.
- `alien`, `fakeroot`, `dpkg-deb`, `rpm`, `rpmbuild`.
- `dnf` and `sudo` for installation.
- `gio` for Trash cleanup (default).
- `readelf` and `ldconfig` for library inspection.

On Fedora:

```sh
sudo dnf install alien fakeroot dpkg rpm-build glib2 binutils
```

## Build and install

```sh
bash install.sh
```

Installs to `~/.local/bin/deb2rpm` and adds a desktop entry. `XDG_BIN_HOME` / `XDG_DATA_HOME` override locations. Ensure binary directory is in `PATH`.

To build without installing:

```sh
cargo build --release
./target/release/deb2rpm --help
```

## Usage

Convert and install:

```sh
deb2rpm app.deb
```

Build without installing:

```sh
deb2rpm -n app.deb
deb2rpm -n -o ./rpms app.deb
deb2rpm -o ./rpms app.deb
```

Options table above in [Cheat sheet](#cheat-sheet).

Only one `.deb` is accepted per invocation. `--keep-work` and `--delete-work` cannot be combined. Use `--` before filenames starting with a dash.

## Conversion behavior

- Repairs empty RPM summaries and sets packaged file ownership to `root:root`.
- When preparing an installation, removes shared directory ownership claims that conflict with other installed RPMs. Build-only mode retains directory entries except `/`.
- Adapts Debian maintainer-script arguments for installation, upgrades, and removal, and replaces alien's predictable script temporary directory with a private one.
- Preserves automatic RPM requirements and provides so `dnf` can resolve the transaction. For x86_64 and aarch64 packages, known Node addon prebuild layouts are checked for bundled alternatives: an incompatible architecture or musl variant is excluded from dependency scanning only when a matching target-architecture glibc ELF is present. The files remain in the RPM; standalone musl binaries and unrecognized layouts keep their dependencies. Each exclusion is logged and also applies to the advisory library scan.
- Checks for files owned by other packages before installation and blocks downgrades unless explicitly allowed.
- Uses reinstall behavior when the built version matches the installed version.
- Inspects ELF library requirements without executing the packaged binaries. This inspection is advisory.

Conversion does not guarantee that a Debian application will work on Fedora. Debian `Depends` entries are displayed but are not translated into RPM dependencies, and maintainer scripts may require Debian-specific tools or behavior. `--no-scripts` can avoid incompatible scripts, but it also skips their setup work. Use packages from sources you trust: installation can run their maintainer scripts with elevated privileges.

## Work files and troubleshooting

Work directories live under `$XDG_CACHE_HOME/deb2rpm` when `XDG_CACHE_HOME` is absolute, otherwise under `~/.cache/deb2rpm`. Each conversion records a `deb2rpm.log` in its work directory.

Successful work is moved to the Trash by default, including the generated RPM unless a separate copy was saved with `-o` or build-only mode. Ordinary failures retain the work directory and log for inspection. Interrupted work is normally cleaned up; stale in-progress directories are handled on a later run. Use `--keep-work` to retain work or `--delete-work` to avoid accumulating it in the Trash.

If interrupted while `dnf` is running, check the installed package state before retrying:

```sh
rpm -q PACKAGE_NAME
```

## Development

```sh
cargo test
```
