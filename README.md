# deb2rpm

Convert a compatible Debian `.deb` package into an RPM and install it on Fedora. `deb2rpm` uses `alien` to prepare the package, repairs parts of the generated RPM spec, builds as your normal user with `fakeroot`, and uses `dnf` with `sudo` for installation.

Written in Rust with no other Rust dependencies.

## Requirements

- Linux with Rust and Cargo to build the tool.
- `alien`, `fakeroot`, `dpkg-deb`, `rpm`, and `rpmbuild` for conversion.
- `dnf` and `sudo` for installation. (Should already be installed)
- `gio` for the default move-to-Trash cleanup. (Should already be installed)
- `readelf` and `ldconfig` for the advisory shared-library inspection.

On Fedora, the conversion tools are installed with:

```sh
sudo dnf install alien fakeroot dpkg rpm-build glib2 binutils
```

System tools are addressed by absolute paths under `/usr/bin` and `/usr/sbin`, so alternate versions earlier in `PATH` do not replace them.

## Build and install

From the repository directory, run:

```sh
bash install.sh
```

The installer builds the release binary and installs it to `~/.local/bin/deb2rpm`. It also adds a desktop entry at `~/.local/share/applications/deb2rpm.desktop`, allowing a `.deb` file to be opened with **Convert & install (deb2rpm)** from a file manager. The desktop entry opens a terminal and waits for Enter before closing.

`XDG_BIN_HOME` and `XDG_DATA_HOME` override the binary and desktop-data locations. Ensure the binary directory is in your `PATH`.

To build without installing the tool:

```sh
cargo build --release
./target/release/deb2rpm --help
```

## Usage

Convert and install one package:

```sh
deb2rpm app.deb
```

Run as your normal user, not with `sudo`. The tool requests elevated privileges when installing and checking script status. `dnf` asks for confirmation unless `-y` is supplied.

Build an RPM without installing it; by default, save it next to the input `.deb`:

```sh
deb2rpm -n app.deb
```

Choose an output directory:

```sh
deb2rpm -n -o ./rpms app.deb
```

Install and keep a separate copy of the RPM:

```sh
deb2rpm -o ./rpms app.deb
```

| Option | Behavior |
| --- | --- |
| `-n`, `--build-only` | Build and save the RPM without installing it. |
| `-o`, `--output DIR` | Save a copy of the RPM in `DIR`. |
| `-y`, `--yes` | Skip the `dnf` transaction confirmation; sudo authentication may still be required. |
| `--allow-downgrade` | Permit replacing an installed newer version. |
| `--no-scripts` | Omit the Debian package's maintainer scripts. |
| `--force-arch` | Override architecture checks; this does not translate binaries for another CPU. |
| `--keep-work` | Keep the temporary build directory. |
| `--delete-work` | Delete successful or interrupted work instead of moving it to the Trash. |
| `--pause` | Wait for Enter before exiting, useful for file-manager launches. |
| `-h`, `--help` | Show usage. |

Only one `.deb` is accepted per invocation. `--keep-work` and `--delete-work` cannot be combined. Use `--` before a filename that starts with a dash.

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

Run the unit tests with:

```sh
cargo test
```
