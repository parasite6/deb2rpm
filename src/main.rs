//! deb2rpm — convert a compatible .deb into an .rpm with alien, build it without root
//! and install it with dnf.
//!
//! What alien gets wrong, and what this does about it:
//! * It claims shared system directories (/, /usr, /opt, /usr/share/icons/hicolor/…) as owned by the
//!   package. Every `%dir` that an installed rpm already owns is dropped; the app's own directories stay.
//! * It runs the Debian maintainer scripts with rpm's numeric arguments. On an rpm upgrade the old
//!   package's postrm runs *after* the new postinst, so a postrm that removes a symlink breaks every
//!   upgrade. The arguments are translated (install/upgrade/configure/remove), including versions,
//!   so correctly written Debian scripts can distinguish upgrade cleanup from removal.
//! * The scripts are unpacked into a predictable /tmp/alien.$$ as root; a private mktemp dir is used.
//! * The spec has no %defattr, so file ownership would come from whoever builds it. With
//!   %defattr(-,root,root,-) the build runs as you (under fakeroot); installation and its status check use sudo.
//! * An empty Summary (Description starting with a blank line) stops rpmbuild; it's filled in.
//! * RPM dependencies are preserved; dnf resolves the complete transaction, including self-provides.
//! * The ELF scan is advisory and never executes package binaries.
//!
//! Everything runs in ~/.cache/deb2rpm/<work dir>; on success it goes to the Trash, on failure it stays
//! with a full log.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// System tools by absolute path: ~/.linuxbrew/bin shadows some of them (gio, …) on this machine.
const ALIEN: &str = "/usr/bin/alien";
const FAKEROOT: &str = "/usr/bin/fakeroot";
const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const RPM: &str = "/usr/bin/rpm";
const RPMBUILD: &str = "/usr/bin/rpmbuild";
const DNF: &str = "/usr/bin/dnf";
const SUDO: &str = "/usr/bin/sudo";
const GIO: &str = "/usr/bin/gio";
const READELF: &str = "/usr/bin/readelf";
const LDCONFIG: &str = "/usr/sbin/ldconfig";

/// Directories that are always shared, used only if asking rpm who owns a directory fails.
const SYSTEM_DIRS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/etc",
    "/home",
    "/lib",
    "/lib64",
    "/media",
    "/mnt",
    "/opt",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/usr",
    "/usr/bin",
    "/usr/include",
    "/usr/lib",
    "/usr/lib64",
    "/usr/libexec",
    "/usr/local",
    "/usr/sbin",
    "/usr/share",
    "/usr/share/applications",
    "/usr/share/doc",
    "/usr/share/icons",
    "/usr/share/icons/hicolor",
    "/usr/share/man",
    "/usr/share/mime",
    "/usr/share/pixmaps",
    "/usr/share/metainfo",
    "/var",
    "/var/lib",
    "/etc/xdg",
    "/etc/profile.d",
];

type Res<T> = Result<T, String>;

// --- Ctrl+C / closed window / kill ---------------------------------------------------------------
// Build children have their own group. The main loop forwards stop signals and waits before cleanup.
// sudo retains terminal access and forwards signals to its installation child; never force-kill dnf.

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static STOP_SIGNAL: AtomicI32 = AtomicI32::new(15);
/// Set just before dnf runs: a stop after this point may have changed the system.
static INSTALL_STARTED: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn flock(fd: i32, operation: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

extern "C" fn on_signal(sig: i32) {
    STOP_SIGNAL.store(sig, Ordering::SeqCst);
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    for sig in [1 /* HUP */, 2 /* INT */, 15 /* TERM */] {
        unsafe { signal(sig, on_signal as *const () as usize) };
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

const STOPPED: &str = "stopped (Ctrl+C, window closed or killed)";

/// Present in a work dir while a run is using it; a dir that still has it after the run is gone was
/// interrupted.
const IN_PROGRESS: &str = ".in-progress";

// --- options -------------------------------------------------------------------------------------

struct Opts {
    deb: PathBuf,
    build_only: bool,
    output: Option<PathBuf>,
    yes: bool,
    allow_downgrade: bool,
    no_scripts: bool,
    keep_work: bool,
    force_arch: bool,
    pause: bool,
    delete_work: bool,
}

const USAGE: &str = "\
deb2rpm — install a .deb on Fedora

  deb2rpm app.deb          convert it to an .rpm and install it (dnf asks before installing)
  deb2rpm -n app.deb       only make the .rpm, don't install it (saved next to the .deb)

Where the .rpm goes
  -o DIR                   save a copy of the .rpm in DIR (otherwise it goes to the Trash after installing)

Installing
  -y                       install without dnf asking \"Is this ok?\"
  --allow-downgrade        install even if you already have a newer version

Unusual packages
  --no-scripts             skip the package's own setup scripts (if they fail on Fedora)
  --force-arch             build a .deb made for another kind of CPU anyway

Troubleshooting
  --keep-work              keep the temporary files instead of moving them to the Trash
  --delete-work            delete successful/interrupted work instead of filling the Trash
  --pause                  wait for Enter before closing (used when opened from Files)
";

fn parse_args() -> Res<Opts> {
    let mut o = Opts {
        deb: PathBuf::new(),
        build_only: false,
        output: None,
        yes: false,
        allow_downgrade: false,
        no_scripts: false,
        keep_work: false,
        force_arch: false,
        pause: false,
        delete_work: false,
    };
    let mut debs = Vec::new();
    let mut args = env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("-n") | Some("--build-only") => o.build_only = true,
            Some("-o") | Some("--output") => {
                o.output = Some(PathBuf::from(
                    args.next().ok_or("--output needs a directory")?,
                ))
            }
            Some("-y") | Some("--yes") => o.yes = true,
            Some("--allow-downgrade") => o.allow_downgrade = true,
            Some("--no-scripts") => o.no_scripts = true,
            Some("--keep-work") => o.keep_work = true,
            Some("--delete-work") => o.delete_work = true,
            Some("--force-arch") => o.force_arch = true,
            Some("--pause") => o.pause = true,
            Some("-h") | Some("--help") => {
                out(USAGE);
                std::process::exit(0);
            }
            Some("--") => {
                debs.extend(args.map(PathBuf::from));
                break;
            }
            Some(s) if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option {s}\n\n{USAGE}"))
            }
            _ => debs.push(PathBuf::from(a)),
        }
    }
    match debs.len() {
        1 => o.deb = debs.remove(0),
        0 => return Err(format!("no .deb given\n\n{USAGE}")),
        _ => return Err("one .deb at a time".into()),
    }
    if o.keep_work && o.delete_work {
        return Err("--keep-work and --delete-work conflict".into());
    }
    Ok(o)
}

// --- output + logging ----------------------------------------------------------------------------

struct Log {
    file: Option<fs::File>,
}

impl Log {
    fn write(&mut self, text: &str) {
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.write_all(text.as_bytes()) {
                err_out(&format!("warning: log write failed: {e}\n"));
                self.file = None;
            }
        }
    }
}

static LOG: Mutex<Log> = Mutex::new(Log { file: None });

fn log(text: &str) {
    if let Ok(mut l) = LOG.lock() {
        l.write(text);
    }
}

// Never println!: it panics when the terminal is gone (window closed), and cleanup must still happen.
fn out(text: &str) {
    let _ = io::stdout().write_all(text.as_bytes());
    let _ = io::stdout().flush();
}

fn err_out(text: &str) {
    let _ = io::stderr().write_all(text.as_bytes());
}

fn step(msg: &str) {
    out(&format!("\x1b[1m==> {msg}\x1b[0m\n"));
    log(&format!("\n==> {msg}\n"));
}

fn info(msg: &str) {
    out(&format!("    {msg}\n"));
    log(&format!("    {msg}\n"));
}

fn warn(msg: &str) {
    err_out(&format!("    \x1b[33mwarning:\x1b[0m {msg}\n"));
    log(&format!("    warning: {msg}\n"));
}

// --- running commands ----------------------------------------------------------------------------

struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs a command, captures its output (also into the log). Never fails on a non-zero exit: the
/// caller decides; only a command that can't be started is an error.
fn run(prog: &str, args: &[&str], cwd: Option<&Path>) -> Res<Out> {
    log(&format!("$ {} {}\n", prog, args.join(" ")));
    let mut cmd = Command::new(prog);
    cmd.args(args).stdin(Stdio::null());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    if interrupted() {
        return Err(STOPPED.into());
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("can't run {prog}: {e}"))?;
    let mut stdout_pipe = child.stdout.take().ok_or("missing stdout pipe")?;
    let mut stderr_pipe = child.stderr.take().ok_or("missing stderr pipe")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        stdout_pipe.read_to_end(&mut b).map(|_| b)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        stderr_pipe.read_to_end(&mut b).map(|_| b)
    });
    let status = wait_child(&mut child, true)?;
    let stdout = String::from_utf8_lossy(
        &stdout_reader
            .join()
            .map_err(|_| "stdout reader failed")?
            .map_err(|e| e.to_string())?,
    )
    .into_owned();
    let stderr = String::from_utf8_lossy(
        &stderr_reader
            .join()
            .map_err(|_| "stderr reader failed")?
            .map_err(|e| e.to_string())?,
    )
    .into_owned();
    log(&stdout);
    log(&stderr);
    if interrupted() {
        return Err(STOPPED.into());
    }
    Ok(Out {
        ok: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

fn wait_child(child: &mut std::process::Child, group: bool) -> Res<std::process::ExitStatus> {
    let mut signalled = None;
    loop {
        if interrupted() && signalled.is_none() {
            let target = if group {
                -(child.id() as i32)
            } else {
                child.id() as i32
            };
            unsafe {
                kill(target, STOP_SIGNAL.load(Ordering::SeqCst));
            }
            signalled = Some(Instant::now());
        }
        if group && signalled.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
            unsafe {
                kill(-(child.id() as i32), 9);
            }
        }
        if let Some(st) = child
            .try_wait()
            .map_err(|e| format!("waiting for child: {e}"))?
        {
            return Ok(st);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Runs a command attached to the terminal (sudo password, dnf's confirmation).
fn run_interactive(prog: &str, args: &[&str]) -> Res<bool> {
    log(&format!("$ {} {}\n", prog, args.join(" ")));
    if interrupted() {
        return Err(STOPPED.into());
    }
    let mut child = Command::new(prog)
        .args(args)
        .spawn()
        .map_err(|e| format!("can't run {prog}: {e}"))?;
    let st = wait_child(&mut child, false)?;
    if interrupted() {
        return Err(STOPPED.into());
    }
    Ok(st.success())
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

// --- small helpers -------------------------------------------------------------------------------

fn home() -> Res<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_root() -> bool {
    // /proc/self/status: "Uid:\treal\teffective\t…"
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .map(|l| l.split_whitespace().nth(2) == Some("0"))
        })
        .unwrap_or(false)
}

fn available_bytes(dir: &Path) -> Option<u64> {
    let out = Command::new("/usr/bin/df")
        .args(["--output=avail", "-B1"])
        .arg(dir)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}

fn unquote(path: &str) -> String {
    let p = path.trim().trim_matches('"');
    if p.len() > 1 {
        p.trim_end_matches('/').to_string()
    } else {
        p.to_string()
    }
}

fn trash(path: &Path) -> bool {
    Command::new(GIO)
        .arg("trash")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// --- lock ----------------------------------------------------------------------------------------

struct Lock {
    _file: fs::File,
}

impl Lock {
    fn take(dir: &Path) -> Res<Lock> {
        let path = dir.join("lock");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(0x20000 /* O_NOFOLLOW on Linux */)
            .open(&path)
            .map_err(|e| format!("can't open {}: {e}", path.display()))?;
        if unsafe {
            flock(file.as_raw_fd(), 2 | 4 /* LOCK_EX | LOCK_NB */)
        } != 0
        {
            return Err(format!(
                "another conversion holds {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        file.set_len(0).map_err(|e| e.to_string())?;
        write!(file, "{}", std::process::id()).map_err(|e| e.to_string())?;
        Ok(Lock { _file: file })
    }
}

// Closing the descriptor releases the lock. Never unlink the inode being locked.

// --- the .deb ------------------------------------------------------------------------------------

struct Deb {
    path: PathBuf,
    name: String,
    version: String,
    arch: String,
    depends: String,
    installed_kb: u64,
}

fn read_deb(path: &Path) -> Res<Deb> {
    let path = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let mut magic = [0u8; 8];
    fs::File::open(&path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|e| format!("can't read {}: {e}", path.display()))?;
    if &magic != b"!<arch>\n" {
        return Err(format!(
            "{} is not a .deb (not an ar archive)",
            path.display()
        ));
    }
    let p = path.to_str().ok_or("the .deb's path isn't valid UTF-8")?;
    let info = run(DPKG_DEB, &["--info", p], None)?;
    if !info.ok {
        return Err(format!(
            "dpkg-deb can't read the package:\n{}",
            tail(&info.stderr, 10)
        ));
    }
    let field = |f: &str| -> Res<String> {
        let o = run(DPKG_DEB, &["-f", p, f], None)?;
        if !o.ok {
            return Err(format!("can't read Debian field {f}: {}", o.stderr));
        }
        Ok(o.stdout.trim().to_string())
    };
    let name = field("Package")?;
    let version = field("Version")?;
    if name.is_empty() || version.is_empty() {
        return Err("the package has no Package/Version field".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+-._".contains(c))
    {
        return Err(format!(
            "unusual package name {name:?}; refusing to build it"
        ));
    }
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".+:~_-".contains(c))
    {
        return Err(format!("unsupported Debian version {version:?}"));
    }
    Ok(Deb {
        name,
        version,
        arch: field("Architecture")?,
        depends: field("Depends")?,
        installed_kb: field("Installed-Size")?.parse().unwrap_or(0),
        path,
    })
}

fn rpm_arch(deb_arch: &str) -> Option<&'static str> {
    Some(match deb_arch {
        "all" => "noarch",
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        "i386" => "i686",
        "armhf" => "armv7hl",
        "ppc64el" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

fn machine_arch() -> String {
    Command::new("/usr/bin/uname")
        .arg("-m")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// --- alien ---------------------------------------------------------------------------------------

/// Runs alien in `work` and returns the directory it prepared. If alien can't read the .deb (old
/// alien vs new compression, odd member order), the .deb is unpacked and rebuilt with dpkg-deb in a
/// conservative format and alien is tried again.
fn alien_generate(deb: &Path, work: &Path, scripts: bool) -> Res<PathBuf> {
    let try_alien = |deb: &Path| -> Res<Option<PathBuf>> {
        let before: HashSet<PathBuf> = dirs_in(work);
        let d = deb.to_str().ok_or("path isn't UTF-8")?;
        let mut args = vec![ALIEN, "--to-rpm", "--generate"];
        if scripts {
            args.push("--scripts");
        }
        args.push(d);
        let out = run(FAKEROOT, &args, Some(work))?;
        let new: Vec<PathBuf> = dirs_in(work)
            .into_iter()
            .filter(|p| !before.contains(p))
            .collect();
        if !out.ok || new.len() != 1 {
            warn(&format!(
                "alien failed (exit {:?}): {}",
                out.code,
                tail(&(out.stdout + &out.stderr), 6)
            ));
            for d in new {
                trash_or_keep(&d);
            }
            return Ok(None);
        }
        Ok(Some(new.into_iter().next().unwrap()))
    };

    if let Some(dir) = try_alien(deb)? {
        return Ok(dir);
    }
    step("Repacking the .deb so alien can read it");
    let raw = work.join("repack");
    let w = |p: &Path| {
        p.to_str()
            .map(str::to_string)
            .ok_or_else(|| "path isn't UTF-8".to_string())
    };
    let o = run(FAKEROOT, &[DPKG_DEB, "-R", &w(deb)?, &w(&raw)?], None)?;
    if !o.ok {
        return Err(format!(
            "dpkg-deb couldn't unpack the .deb:\n{}",
            tail(&o.stderr, 8)
        ));
    }
    let repacked = work.join("repacked.deb");
    let o = run(
        FAKEROOT,
        &[
            DPKG_DEB,
            "--root-owner-group",
            "-Zxz",
            "-b",
            &w(&raw)?,
            &w(&repacked)?,
        ],
        None,
    )?;
    if !o.ok {
        return Err(format!(
            "dpkg-deb couldn't rebuild the .deb:\n{}",
            tail(&o.stderr, 8)
        ));
    }
    trash_or_keep(&raw);
    try_alien(&repacked)?
        .ok_or_else(|| "alien failed on the original and on a repacked .deb (see the log)".into())
}

fn dirs_in(dir: &Path) -> HashSet<PathBuf> {
    fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

fn trash_or_keep(p: &Path) {
    if p.exists() && !trash(p) {
        warn(&format!(
            "couldn't move {} to the Trash; left in place",
            p.display()
        ));
    }
}

fn find_spec(dir: &Path) -> Res<PathBuf> {
    let specs: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "spec"))
        .collect();
    match specs.len() {
        1 => Ok(specs.into_iter().next().unwrap()),
        n => Err(format!(
            "expected one .spec from alien in {}, found {n}",
            dir.display()
        )),
    }
}

// --- fixing the spec -----------------------------------------------------------------------------

struct SpecFix {
    text: String,
    dropped_dirs: Vec<String>,
    kept_dirs: usize,
    scriptlets: Vec<String>,
}

/// Who owns `path` according to the rpm database (several packages can own one directory).
/// None = the query itself failed; Some(empty) = exists but unowned.
fn owners(path: &str) -> Option<Vec<String>> {
    static OWNERS: OnceLock<Option<BTreeMap<String, Vec<String>>>> = OnceLock::new();
    let map = OWNERS
        .get_or_init(|| {
            let out = run(RPM, &["-qa", "--qf", "[%{FILENAMES}\t%{=NAME}\n]"], None).ok()?;
            if !out.ok {
                return None;
            }
            let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for line in out.stdout.lines() {
                if let Some((p, name)) = line.split_once('\t') {
                    map.entry(p.into()).or_default().push(name.into());
                }
            }
            Some(map)
        })
        .as_ref()?;
    Some(map.get(path).cloned().unwrap_or_default())
}

fn fix_spec(text: &str, pkg: &str, installing: bool) -> Res<SpecFix> {
    let mut out = String::with_capacity(text.len() + 1024);
    let mut dropped = Vec::new();
    let mut kept = 0usize;
    let mut scriptlets = Vec::new();
    let mut in_files = false;
    let mut saw_files = false;
    let mut summary_fixed = false;
    let version = text
        .lines()
        .find_map(|l| l.strip_prefix("Version:"))
        .unwrap_or("")
        .trim();
    let version = text
        .lines()
        .find_map(|l| l.strip_prefix("# deb2rpm Debian version: "))
        .unwrap_or(version);
    let has_scripts = text
        .lines()
        .any(|l| matches!(l.trim(), "%pre" | "%post" | "%preun" | "%postun"));
    let mut prepared = text.to_string();
    if has_scripts {
        let mut extra = String::new();
        for hook in ["%pre", "%post"] {
            if !text.lines().any(|l| l.trim() == hook) {
                let _ = writeln!(extra, "{hook}");
            }
        }
        if !extra.is_empty() {
            prepared = prepared.replacen("%description", &(extra + "%description"), 1);
        }
    }

    for line in prepared.lines() {
        let trimmed = line.trim_start();
        let head = trimmed.split_whitespace().next().unwrap_or("");

        // section headers
        if head.starts_with('%')
            && matches!(
                head,
                "%pre"
                    | "%post"
                    | "%preun"
                    | "%postun"
                    | "%files"
                    | "%description"
                    | "%prep"
                    | "%build"
                    | "%install"
                    | "%clean"
                    | "%changelog"
                    | "%pretrans"
                    | "%posttrans"
            )
        {
            in_files = head == "%files";
            out.push_str(line);
            out.push('\n');
            // Debian scripts expect operations and versions; RPM passes installed-package counts.
            let guard = script_guard(head, pkg, version);
            if let Some(g) = guard {
                out.push_str("# deb2rpm: Debian-style arguments for the Debian script\n");
                out.push_str(&g);
                out.push('\n');
                scriptlets.push(head.trim_start_matches('%').to_string());
            }
            if in_files {
                saw_files = true;
                out.push_str("%defattr(-,root,root,-)\n");
            }
            continue;
        }

        // header fields
        if !saw_files && line.starts_with("Summary:") && line["Summary:".len()..].trim().is_empty()
        {
            let _ = writeln!(out, "Summary: {pkg} (converted from a .deb)");
            summary_fixed = true;
            continue;
        }

        // alien's private script dir: predictable /tmp/alien.$$ as root → mktemp
        if line.contains("/tmp/alien.$$") {
            if trimmed.starts_with("mkdir /tmp/alien.$$") {
                out.push_str("ALIEN_TMP=$(mktemp -d) || exit 1\n");
            } else {
                out.push_str(&line.replace("/tmp/alien.$$", "\"$ALIEN_TMP\""));
                out.push('\n');
            }
            continue;
        }

        if in_files && head == "%dir" {
            let path = unquote(trimmed["%dir".len()..].trim());
            let drop = if !installing || !Path::new(&path).exists() {
                false // only this package will create it
            } else {
                match owners(&path) {
                    Some(o) => o.iter().any(|n| n != pkg),
                    None => SYSTEM_DIRS.contains(&path.as_str()),
                }
            } || path == "/";
            if drop {
                dropped.push(path);
                continue;
            }
            kept += 1;
        }
        out.push_str(line);
        out.push('\n');
    }

    if !saw_files {
        return Err("alien's spec has no %files section".into());
    }
    if summary_fixed {
        info("filled in the empty Summary");
    }
    Ok(SpecFix {
        text: out,
        dropped_dirs: dropped,
        kept_dirs: kept,
        scriptlets,
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn script_guard(hook: &str, pkg: &str, version: &str) -> Option<String> {
    if !matches!(hook, "%pre" | "%post" | "%preun" | "%postun") {
        return None;
    }
    let state = shell_quote(&format!("/var/lib/deb2rpm/{pkg}"));
    let mut g = format!("DEB2RPM_STATE={state}\nmkdir -p -- \"$DEB2RPM_STATE\" || exit 1\nchmod 700 -- \"$DEB2RPM_STATE\" || exit 1\n");
    if hook == "%pre" {
        g.push_str(&format!("if [ \"$1\" -le 1 ] 2>/dev/null; then DEB2RPM_OLD=; elif [ -f \"$DEB2RPM_STATE/version\" ]; then DEB2RPM_OLD=$(cat \"$DEB2RPM_STATE/version\") || exit 1; else DEB2RPM_OLD=$(/usr/bin/rpm -q --qf '%%{{VERSION}}' {} 2>/dev/null) || DEB2RPM_OLD=; fi\n", shell_quote(pkg)));
        g.push_str(&format!("printf '%%s' \"$DEB2RPM_OLD\" > \"$DEB2RPM_STATE/previous\" || exit 1\nprintf '%%s' {} > \"$DEB2RPM_STATE/new\" || exit 1\n", shell_quote(version)));
        g.push_str("rm -f -- \"$DEB2RPM_STATE/\"*.status\nDEB2RPM_NEW=$(cat \"$DEB2RPM_STATE/new\") || exit 1\nif [ \"$1\" -gt 1 ] 2>/dev/null; then set -- upgrade \"$DEB2RPM_OLD\" \"$DEB2RPM_NEW\"; else set -- install; fi\n");
    } else if hook == "%post" {
        g.push_str("DEB2RPM_OLD=$(cat \"$DEB2RPM_STATE/previous\" 2>/dev/null || :)\nset -- configure \"$DEB2RPM_OLD\"\n");
    } else {
        g.push_str("DEB2RPM_NEW=$(cat \"$DEB2RPM_STATE/new\" 2>/dev/null || :)\nif [ \"$1\" -ge 1 ] 2>/dev/null; then set -- upgrade \"$DEB2RPM_NEW\"; else set -- remove; fi\n");
    }
    let status = hook.trim_start_matches('%');
    let save_version = if hook == "%post" {
        "if [ \"$rc\" = 0 ]; then cp -- \"$DEB2RPM_STATE/new\" \"$DEB2RPM_STATE/version\" || rc=1; fi; "
    } else {
        ""
    };
    g.push_str(&format!("printf 'pending' > \"$DEB2RPM_STATE/{status}.status\" || exit 1\ntrap 'rc=$?; {save_version}printf \"%%s\" \"$rc\" > \"$DEB2RPM_STATE/{status}.status\"; exit \"$rc\"' EXIT\n"));
    Some(g)
}

// --- rpmbuild ------------------------------------------------------------------------------------

fn rpmbuild(dir: &Path, spec: &Path, arch: &str, work: &Path) -> Res<PathBuf> {
    let d = dir.to_str().ok_or("path isn't UTF-8")?;
    let s = spec.to_str().ok_or("path isn't UTF-8")?;
    let buildroot = format!("--buildroot={d}");
    let topdir = format!("_topdir {}", work.join("rpmbuild").display());
    // fakeroot: ownership in the payload comes from %defattr; this keeps rpmbuild from tripping over
    // files the unprivileged user can't stat the "real" owner of.
    // No post-processing: Fedora's brp scripts strip binaries, byte-compile Python, rewrite shebangs and
    // add build-id links — the payload must stay byte-identical to the .deb. Multithreaded zstd -7
    // instead of single-threaded -19 (minutes faster on big Electron apps, slightly larger rpm).
    let out = run(
        FAKEROOT,
        &[
            RPMBUILD,
            &buildroot,
            "--define",
            &topdir,
            "--define",
            "__spec_install_post %{nil}",
            "--define",
            "__os_install_post %{nil}",
            "--define",
            "debug_package %{nil}",
            "--define",
            "_build_id_links none",
            "--define",
            "_binary_payload w7T0.zstdio",
            "-bb",
            "--target",
            arch,
            s,
        ],
        Some(dir),
    )?;
    let wrote = out
        .stdout
        .lines()
        .chain(out.stderr.lines())
        .filter_map(|l| l.strip_prefix("Wrote: "))
        .next_back()
        .map(|s| s.trim().to_string());
    if !out.ok || wrote.is_none() {
        return Err(format!(
            "rpmbuild failed (exit {:?}):\n{}",
            out.code,
            tail(&(out.stdout + &out.stderr), 15)
        ));
    }
    let mut rpm = PathBuf::from(wrote.unwrap());
    if rpm.is_relative() {
        rpm = dir.join(rpm);
    }
    let rpm = fs::canonicalize(&rpm)
        .map_err(|e| format!("rpmbuild reported {} but: {e}", rpm.display()))?;
    // keep it inside the work dir whatever %_rpmdir said
    let dest = work.join(rpm.file_name().ok_or("odd rpm path")?);
    if rpm != dest {
        fs::rename(&rpm, &dest)
            .or_else(|_| fs::copy(&rpm, &dest).map(|_| ()))
            .map_err(|e| format!("can't move the rpm into {}: {e}", work.display()))?;
    }
    Ok(dest)
}

// --- checks on the built rpm ---------------------------------------------------------------------

// dnf's transaction solver resolves full requirements, self-provides and repositories.
// A failed advisory query must never change the package's dependency metadata.

/// Files in the rpm that already exist and belong to another package (dnf would refuse).
fn conflicts(rpm: &str, pkg: &str) -> Res<Vec<String>> {
    let list = run(
        RPM,
        &["-qp", "--qf", "[%{FILENAMES}\t%{FILEMODES:perms}\n]", rpm],
        None,
    )?;
    if !list.ok {
        return Err(format!("can't inspect RPM files: {}", list.stderr));
    }
    let mut out = Vec::new();
    for line in list.stdout.lines() {
        let Some((path, perms)) = line.split_once('\t') else {
            continue;
        };
        if perms.starts_with('d') {
            continue; // shared directories are fine
        }
        let p = Path::new(path);
        if p.symlink_metadata().is_err() {
            continue;
        }
        match owners(path) {
            Some(o) if o.iter().any(|n| n != pkg) => {
                out.push(format!("{path} (owned by {})", o.join(", ")))
            }
            Some(o) if o.is_empty() => warn(&format!(
                "{path} already exists and no package owns it; it will be replaced"
            )),
            None => {
                return Err("could not read installed file ownership; refusing installation".into())
            }
            _ => {}
        }
    }
    Ok(out)
}

#[derive(Default)]
struct LibraryReport {
    missing: BTreeSet<(String, String)>,
    unverified: Vec<String>,
}

fn elf_identity(path: &Path) -> Option<[u8; 4]> {
    let mut b = [0u8; 20];
    fs::File::open(path).ok()?.read_exact(&mut b).ok()?;
    if &b[..4] != b"\x7fELF" {
        return None;
    }
    Some([b[4], b[5], b[18], b[19]]) // class, endianness, machine
}

/// Advisory only: do not execute binaries or claim to prove complete loader compatibility.
fn missing_libraries(tree: &Path) -> LibraryReport {
    let mut report = LibraryReport::default();
    let mut system: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    match run(LDCONFIG, &["-p"], None) {
        Ok(o) if o.ok => {
            for l in o.stdout.lines() {
                if let Some((left, right)) = l.split_once(" => ") {
                    if let Some(name) = left.split_whitespace().next() {
                        system
                            .entry(name.into())
                            .or_default()
                            .push(PathBuf::from(right.trim()));
                    }
                }
            }
        }
        _ => report
            .unverified
            .push("could not read the system loader cache".into()),
    }
    let mut elfs = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) => {
                report.unverified.push(format!("{}: {e}", d.display()));
                continue;
            }
        };
        for e in rd {
            let e = match e {
                Ok(e) => e,
                Err(e) => {
                    report.unverified.push(e.to_string());
                    continue;
                }
            };
            let p = e.path();
            match p.symlink_metadata() {
                Ok(m) if m.is_dir() => stack.push(p),
                Ok(m) if m.is_file() && elf_identity(&p).is_some() => elfs.push(p),
                Err(e) => report.unverified.push(format!("{}: {e}", p.display())),
                _ => {}
            }
        }
    }
    let canonical_tree = fs::canonicalize(tree).unwrap_or_else(|_| tree.into());
    for elf in elfs {
        let Some(e) = elf.to_str() else {
            report.unverified.push("non-UTF-8 ELF path".into());
            continue;
        };
        let o = match run(READELF, &["-d", "-W", e], None) {
            Ok(o) if o.ok => o,
            _ => {
                report
                    .unverified
                    .push(format!("could not inspect {}", elf.display()));
                continue;
            }
        };
        let identity = elf_identity(&elf);
        let mut paths = Vec::new();
        for l in o
            .stdout
            .lines()
            .filter(|l| l.contains("(RUNPATH)") || l.contains("(RPATH)"))
        {
            if let Some(value) = l.split('[').nth(1).and_then(|s| s.split(']').next()) {
                for p in value.split(':') {
                    let origin = elf.parent().unwrap_or(tree).to_string_lossy();
                    if p.contains("$ORIGIN") || p.contains("${ORIGIN}") {
                        paths.push(PathBuf::from(
                            p.replace("${ORIGIN}", &origin).replace("$ORIGIN", &origin),
                        ));
                    } else if p.starts_with('/') {
                        paths.push(tree.join(p.trim_start_matches('/')));
                    } else {
                        report.unverified.push(format!(
                            "unsupported loader path {p:?} in {}",
                            elf.display()
                        ));
                    }
                }
            }
        }
        for p in ["lib", "lib64", "usr/lib", "usr/lib64"] {
            paths.push(tree.join(p));
        }
        for l in o.stdout.lines().filter(|l| l.contains("(NEEDED)")) {
            let Some(lib) = l.split('[').nth(1).and_then(|s| s.split(']').next()) else {
                continue;
            };
            let bundled = paths.iter().any(|p| {
                fs::canonicalize(p.join(lib))
                    .ok()
                    .is_some_and(|p| p.starts_with(&canonical_tree) && elf_identity(&p) == identity)
            });
            let installed = system
                .get(lib)
                .is_some_and(|ps| ps.iter().any(|p| elf_identity(p) == identity));
            if !bundled && !installed {
                report.missing.insert((
                    lib.into(),
                    format!("/{}", elf.strip_prefix(tree).unwrap_or(&elf).display()),
                ));
            }
        }
    }
    report
}

fn rpm_query(args: &[&str]) -> Option<String> {
    let o = run(RPM, args, None).ok()?;
    if !o.ok {
        return None;
    }
    o.stdout
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .map(|s| s.trim().to_string())
}

fn vercmp(a: &str, b: &str) -> Option<i32> {
    let expr = format!("%{{lua: print(rpm.vercmp(\"{a}\", \"{b}\"))}}");
    if a.contains('"') || b.contains('"') {
        return None;
    }
    rpm_query(&["--eval", &expr])?.parse().ok()
}

fn unique_dir(parent: &Path, prefix: &str) -> Res<PathBuf> {
    let mut random = [0u8; 12];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| e.to_string())?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let path = parent.join(format!("{prefix}-{suffix}"));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|e| format!("can't create {}: {e}", path.display()))?;
    Ok(path)
}

fn save_rpm(source: &Path, dest: &Path) -> Res<()> {
    let staging = unique_dir(
        dest.parent().ok_or("output has no parent")?,
        ".deb2rpm-output",
    )?;
    let file = staging.join("package.rpm");
    let result = (|| {
        fs::copy(source, &file).map_err(|e| e.to_string())?;
        fs::File::open(&file)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::hard_link(&file, dest).map_err(|e| {
            format!(
                "can't save {} (existing files are never overwritten): {e}",
                dest.display()
            )
        })
    })();
    if let Err(e) = fs::remove_dir_all(&staging) {
        warn(&format!("could not remove {}: {e}", staging.display()));
    }
    result
}

fn finish_work(work: &Path, delete: bool) -> bool {
    if delete {
        fs::remove_dir_all(work).is_ok()
    } else {
        trash(work)
    }
}

// --- main flow -----------------------------------------------------------------------------------

fn main() -> ExitCode {
    install_signal_handlers();
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            err_out(&format!("{e}\n"));
            return ExitCode::from(2);
        }
    };
    let pause = opts.pause;
    let code = match convert(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if interrupted() => {
            err_out(&format!("\n\x1b[33m{e}\x1b[0m\n"));
            ExitCode::from(130)
        }
        Err(e) => {
            err_out(&format!("\n\x1b[31merror:\x1b[0m {e}\n"));
            log(&format!("\nerror: {e}\n"));
            ExitCode::FAILURE
        }
    };
    if pause && !interrupted() {
        out("\nPress Enter to close… ");
        let _ = io::stdin().lock().read_line(&mut String::new());
    }
    code
}

fn convert(opts: Opts) -> Res<()> {
    if is_root() {
        return Err("run deb2rpm as your normal user; it uses sudo only for dnf".into());
    }
    let mut tools = vec![ALIEN, FAKEROOT, DPKG_DEB, RPM, RPMBUILD];
    if !opts.build_only {
        tools.extend([DNF, SUDO]);
    }
    if !opts.keep_work && !opts.delete_work {
        tools.push(GIO);
    }
    for tool in tools {
        if !Path::new(tool).exists() {
            let pkg = match tool {
                ALIEN => "alien",
                FAKEROOT => "fakeroot",
                DPKG_DEB => "dpkg",
                RPMBUILD => "rpm-build",
                GIO => "glib2",
                _ => "",
            };
            return Err(format!(
                "{tool} is missing{}",
                if pkg.is_empty() {
                    String::new()
                } else {
                    format!(" (sudo dnf install {pkg})")
                }
            ));
        }
    }

    let cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(home()?.join(".cache"))
        .join("deb2rpm");
    fs::create_dir_all(&cache).map_err(|e| format!("can't create {}: {e}", cache.display()))?;
    let _lock = Lock::take(&cache)?;

    // work dirs of runs that were interrupted (Ctrl+C, crash, power loss) still carry the in-progress
    // marker: they're ours, move them to the Trash. Dirs kept on purpose (--keep-work, a failure) don't.
    for d in dirs_in(&cache) {
        if d.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("work-"))
            && d.join(IN_PROGRESS).exists()
        {
            info(&format!(
                "found a leftover work directory: {}",
                d.display()
            ));
            if !opts.keep_work && !finish_work(&d, opts.delete_work) {
                warn(&format!("couldn't clean {}", d.display()));
            }
        }
    }

    step("Reading the .deb");
    let deb = read_deb(&opts.deb)?;
    info(&format!("{} {} ({})", deb.name, deb.version, deb.arch));

    let machine = machine_arch();
    let arch = match rpm_arch(&deb.arch) {
        Some(a) => a.to_string(),
        None => {
            if !opts.force_arch {
                return Err(format!("unsupported Debian architecture {:?}; use --force-arch to explicitly label it as {machine}", deb.arch));
            }
            warn(&format!(
                "unknown Debian architecture {:?}; building for {machine}",
                deb.arch
            ));
            machine.clone()
        }
    };
    if arch != "noarch"
        && arch != machine
        && !(arch == "i686" && machine == "x86_64")
        && !opts.force_arch
    {
        return Err(format!(
            "the .deb is for {arch}, this machine is {machine} (use --force-arch to build anyway)"
        ));
    }

    let work = unique_dir(&cache, &format!("work-{}-{}", deb.name, now_secs()))?;
    let log_path = work.join("deb2rpm.log");
    LOG.lock().map_err(|_| "log mutex poisoned")?.file =
        Some(fs::File::create(&log_path).map_err(|e| format!("can't create log: {e}"))?);
    log(&format!(
        "deb2rpm {} — {}\n",
        env!("CARGO_PKG_VERSION"),
        deb.path.display()
    ));

    fs::write(work.join(IN_PROGRESS), b"")
        .map_err(|e| format!("can't mark work directory: {e}"))?;
    let result = build_and_install(&opts, &deb, &arch, &work);
    if let Err(e) = fs::remove_file(work.join(IN_PROGRESS)) {
        warn(&format!("can't remove in-progress marker: {e}"));
    }
    match &result {
        Ok(()) if !opts.keep_work => {
            LOG.lock().map_err(|_| "log mutex poisoned")?.file = None;
            if finish_work(&work, opts.delete_work) {
                info(if opts.delete_work {
                    "work files deleted"
                } else {
                    "work files moved to the Trash (recoverable from Files)"
                });
            } else {
                warn(&format!(
                    "couldn't clean the work directory; it's at {}",
                    work.display()
                ));
            }
        }
        Ok(()) => info(&format!("work dir kept: {}", work.display())),
        Err(_) if interrupted() => {
            // nothing worth inspecting: a half-unpacked tree. Clean up now rather than on the next run.
            LOG.lock().map_err(|_| "log mutex poisoned")?.file = None;
            let cleaned = !opts.keep_work && finish_work(&work, opts.delete_work);
            let what = if INSTALL_STARTED.load(Ordering::SeqCst) {
                format!(
                    "Stopped while dnf was running: check what got installed with  rpm -q {}",
                    deb.name
                )
            } else {
                "Stopped. Nothing was installed.".to_string()
            };
            let files = if cleaned {
                "Temporary files cleaned up.".to_string()
            } else {
                format!("Temporary files: {}", work.display())
            };
            return Err(format!("{what} {files}"));
        }
        Err(_) => err_out(&format!(
            "\n    work dir and full log kept for inspection: {}\n",
            log_path.display()
        )),
    }
    result
}

fn build_and_install(opts: &Opts, deb: &Deb, arch: &str, work: &Path) -> Res<()> {
    // space: unpacked tree (twice while alien copies) + the rpm
    let need = deb
        .installed_kb
        .saturating_mul(1024)
        .saturating_mul(3)
        .saturating_add(
            fs::metadata(&deb.path)
                .map(|m| m.len())
                .unwrap_or(0)
                .saturating_mul(2),
        );
    if let Some(avail) = available_bytes(work) {
        if need > 0 && avail < need {
            return Err(format!(
                "not enough space in {}: about {} MB needed, {} MB free",
                work.display(),
                need >> 20,
                avail >> 20
            ));
        }
    }

    if !deb.depends.is_empty() {
        info(&format!(
            "Debian dependencies (not carried over by alien): {}",
            deb.depends
        ));
    }

    step("Unpacking with alien");
    let t = Instant::now();
    let dir = alien_generate(&deb.path, work, !opts.no_scripts)?;
    let spec_path = find_spec(&dir)?;
    let original = format!(
        "# deb2rpm Debian version: {}\n{}",
        deb.version,
        fs::read_to_string(&spec_path).map_err(|e| format!("can't read the spec: {e}"))?
    );
    info(&format!(
        "prepared {} in {:.0?}",
        dir.file_name().unwrap_or_default().to_string_lossy(),
        t.elapsed()
    ));

    // before building: rpmbuild may clean up its buildroot (the unpacked tree)
    let missing = missing_libraries(&dir);

    step("Fixing alien's spec");
    let fix = fix_spec(&original, &deb.name, !opts.build_only)?;
    for d in &fix.dropped_dirs {
        info(&format!("not claiming shared directory {d}"));
    }
    info(&format!(
        "{} shared directories dropped, {} of the package's own kept",
        fix.dropped_dirs.len(),
        fix.kept_dirs
    ));
    if !fix.scriptlets.is_empty() {
        warn(&format!("maintainer scripts kept ({}), with version-aware Debian arguments; upgrade cleanup is retained. Debian-specific tools may still be required",
            fix.scriptlets.join(", ")));
    }
    fs::write(&spec_path, &fix.text).map_err(|e| format!("can't write the spec: {e}"))?;

    step("Building the rpm");
    let t = Instant::now();
    let rpm = rpmbuild(&dir, &spec_path, arch, work)?;
    info(&format!(
        "{} in {:.0?}",
        rpm.file_name().unwrap_or_default().to_string_lossy(),
        t.elapsed()
    ));

    step("Checking the rpm");
    let rpm_s = rpm.to_str().ok_or("path isn't UTF-8")?.to_string();
    if missing.missing.is_empty() && missing.unverified.is_empty() {
        info(
            "advisory ELF scan found no missing direct libraries; dnf will verify RPM dependencies",
        );
    } else {
        for (lib, user) in &missing.missing {
            warn(&format!("{lib} was not found in the inspected loader paths (needed by {user}); dnf may install it"));
        }
        for reason in &missing.unverified {
            warn(&format!("library scan incomplete: {reason}"));
        }
    }

    let name =
        rpm_query(&["-qp", "--qf", "%{NAME}\n", &rpm_s]).ok_or("can't read the built rpm")?;
    let fmt = "%|EPOCH?{%{EPOCH}:}|%{VERSION}-%{RELEASE}\n";
    let built =
        rpm_query(&["-qp", "--qf", fmt, &rpm_s]).ok_or("can't read the built rpm's version")?;
    info(&format!(
        "built {name} {built}; automatic requirements and provides preserved"
    ));

    // keep a copy of the rpm if asked (or when only building)
    let keep_dir = opts.output.clone().or_else(|| {
        opts.build_only
            .then(|| deb.path.parent().map(Path::to_path_buf))
            .flatten()
    });
    if let Some(dir) = keep_dir {
        fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let dest = dir.join(rpm.file_name().unwrap());
        save_rpm(&rpm, &dest)?;
        info(&format!("rpm saved: {}", dest.display()));
    }
    if opts.build_only {
        return Ok(());
    }

    let clashes = conflicts(&rpm_s, &name)?;
    if !clashes.is_empty() {
        return Err(format!(
            "files already owned by other packages:\n  {}",
            clashes.join("\n  ")
        ));
    }
    let installed = rpm_query(&["-q", "--qf", fmt, &name]);
    if !deb.depends.is_empty() {
        warn("Debian Depends are not translated; dnf checks RPM dependencies, not every Debian runtime assumption");
    }

    let verb = match installed.as_deref().and_then(|i| vercmp(&built, i)) {
        None if installed.is_none() => "install",
        Some(0) => "reinstall",
        Some(c) if c > 0 => "install",
        Some(_) if opts.allow_downgrade => "downgrade",
        Some(_) => {
            return Err(format!(
                "a newer {name} ({}) is installed; use --allow-downgrade to replace it",
                installed.unwrap_or_default()
            ))
        }
        None => "install",
    };
    step(&format!(
        "Installing ({verb}) — dnf will ask for your password"
    ));
    let mut args = vec![DNF, verb];
    if opts.yes {
        args.push("-y");
    }
    args.push(&rpm_s);
    INSTALL_STARTED.store(true, Ordering::SeqCst);
    if !run_interactive(SUDO, &args)? {
        return Err(format!(
            "dnf {verb} failed or was cancelled; the rpm is at {rpm_s}"
        ));
    }
    let now = rpm_query(&["-q", "--qf", fmt, &name]);
    if now.as_deref() != Some(built.as_str()) {
        return Err(format!(
            "dnf finished but {name} is at {} instead of {built}",
            now.unwrap_or("nothing".into())
        ));
    }
    for hook in &fix.scriptlets {
        if hook != "pre" && hook != "post" {
            continue;
        }
        let state = PathBuf::from(format!("/var/lib/deb2rpm/{name}/{hook}.status"));
        // Root-owned state is readable but cannot be altered by the invoking user.
        let status = run(
            SUDO,
            &["/usr/bin/cat", state.to_str().ok_or("invalid status path")?],
            None,
        )?;
        if !status.ok || status.stdout.trim() != "0" {
            return Err(format!(
                "{name} installed, but {hook} setup did not finish successfully; inspect {}",
                state.display()
            ));
        }
    }
    step(&format!("Done — {name} {built} is installed"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_dir(label: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("deb2rpm-test-{label}-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn live_lock_cannot_be_replaced() {
        let d = temporary_dir("lock");
        let first = Lock::take(&d).unwrap();
        assert!(Lock::take(&d).is_err());
        drop(first);
        assert!(Lock::take(&d).is_ok());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn spec_keeps_automatic_dependencies() {
        let spec = "Name: demo\nVersion: 2\nRelease: 1\nSummary: demo\n%files\n\"/opt/demo\"\n";
        assert!(!fix_spec(spec, "demo", false)
            .unwrap()
            .text
            .contains("AutoReqProv: no"));
    }

    #[test]
    fn upgrade_scripts_preserve_versions() {
        let spec = "Name: demo\nVersion: 2\nRelease: 1\n%pre\n#!/bin/sh\ntrue\n%post\n#!/bin/sh\ntrue\n%preun\n#!/bin/sh\ntrue\n%postun\n#!/bin/sh\ntrue\n%files\n";
        let fixed = fix_spec(spec, "demo", true).unwrap().text;
        assert!(fixed.contains("set -- upgrade \"$DEB2RPM_OLD\""));
        assert!(fixed.contains("set -- configure \"$DEB2RPM_OLD\""));
        assert!(fixed.contains("set -- upgrade \"$DEB2RPM_NEW\""));
    }

    #[test]
    fn saving_never_overwrites_or_follows_symlinks() {
        let d = temporary_dir("output");
        let source = d.join("source");
        let victim = d.join("victim");
        let dest = d.join("package.rpm");
        fs::write(&source, b"new").unwrap();
        fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, &dest).unwrap();
        assert!(save_rpm(&source, &dest).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        fs::remove_file(&dest).unwrap();
        save_rpm(&source, &dest).unwrap();
        assert!(save_rpm(&source, &dest).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn library_scan_accepts_real_symlinks_but_rejects_text() {
        let d = temporary_dir("libraries");
        fs::write(d.join("lib.c"), b"int audit_value(void) { return 42; }").unwrap();
        fs::write(
            d.join("app.c"),
            b"extern int audit_value(void); int main(void) { return audit_value(); }",
        )
        .unwrap();
        assert!(Command::new("/usr/bin/gcc")
            .current_dir(&d)
            .args([
                "-shared",
                "-fPIC",
                "-Wl,-soname,libauditprobe.so.1",
                "lib.c",
                "-o",
                "libauditprobe.so.1.0"
            ])
            .status()
            .unwrap()
            .success());
        std::os::unix::fs::symlink("libauditprobe.so.1.0", d.join("libauditprobe.so.1")).unwrap();
        assert!(Command::new("/usr/bin/gcc")
            .current_dir(&d)
            .args([
                "app.c",
                "-L.",
                "-l:libauditprobe.so.1",
                "-Wl,-rpath,$ORIGIN",
                "-o",
                "app"
            ])
            .status()
            .unwrap()
            .success());
        assert!(!missing_libraries(&d)
            .missing
            .iter()
            .any(|(lib, _)| lib == "libauditprobe.so.1"));
        fs::remove_file(d.join("libauditprobe.so.1")).unwrap();
        fs::write(d.join("libauditprobe.so.1"), b"not an ELF library").unwrap();
        assert!(missing_libraries(&d)
            .missing
            .iter()
            .any(|(lib, _)| lib == "libauditprobe.so.1"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn scriptlets_receive_versions_and_record_failures() {
        let d = temporary_dir("scriptlets");
        let state = d.join("demo");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("version"), b"1:1.0-1").unwrap();
        let body = |hook| {
            script_guard(hook, "demo", "2:2.0-1")
                .unwrap()
                .replace("/var/lib/deb2rpm/demo", state.to_str().unwrap())
                .replace("%%", "%")
        };
        let pre = body("%pre") + "test \"$1\" = upgrade && test \"$2\" = '1:1.0-1'\n";
        assert!(Command::new("/bin/sh")
            .args(["-c", &pre, "script", "2"])
            .status()
            .unwrap()
            .success());
        let post = body("%post") + "test \"$1\" = configure && test \"$2\" = '1:1.0-1'\n";
        assert!(Command::new("/bin/sh")
            .args(["-c", &post, "script", "2"])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            fs::read_to_string(state.join("version")).unwrap(),
            "2:2.0-1"
        );
        let removal = body("%postun") + "test \"$1\" = upgrade && test \"$2\" = '2:2.0-1'\n";
        assert!(Command::new("/bin/sh")
            .args(["-c", &removal, "script", "1"])
            .status()
            .unwrap()
            .success());
        let failed = body("%post") + "exit 7\n";
        assert_eq!(
            Command::new("/bin/sh")
                .args(["-c", &failed, "script", "2"])
                .status()
                .unwrap()
                .code(),
            Some(7)
        );
        assert_eq!(fs::read_to_string(state.join("post.status")).unwrap(), "7");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    #[ignore = "worker subprocess for parent_signal_stops_child"]
    fn signal_worker() {
        install_signal_handlers();
        assert!(run("/usr/bin/sleep", &["10"], None).is_err());
        assert!(interrupted());
    }

    #[test]
    fn parent_signal_stops_child() {
        let mut child = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "tests::signal_worker"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let tasks = format!("/proc/{}/task", child.id());
        while !fs::read_dir(&tasks)
            .unwrap()
            .filter_map(Result::ok)
            .any(|t| {
                !fs::read_to_string(t.path().join("children"))
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
            })
        {
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            kill(child.id() as i32, 15);
        }
        assert!(child.wait().unwrap().success());
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
