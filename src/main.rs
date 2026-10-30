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
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod close_prompt;
mod interactive_output;
mod progress;
mod tinfo_compat;
use progress::Stage;

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
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn waitid(kind: i32, id: u32, info: *mut u64, options: i32) -> i32;
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
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
        self.write_bytes(text.as_bytes());
    }
    fn write_bytes(&mut self, bytes: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.write_all(bytes) {
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
    progress::suspend();
    let _ = io::stderr().write_all(text.as_bytes());
}

fn step(stage: Stage, msg: &str) {
    if !progress::step(stage, msg) {
        out(&format!("==> {msg}\n"));
    }
    log(&format!("\n==> {msg}\n"));
}

fn info(msg: &str) {
    log(&format!("    {msg}\n"));
    if !progress::info(msg) {
        out(&format!("    {msg}\n"));
    }
}

fn warn(msg: &str) {
    err_out(&format!("    \x1b[38;2;164;101;76mwarning:\x1b[0m {msg}\n"));
    log(&format!("    warning: {msg}\n"));
    progress::resume();
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
    let child = cmd.spawn().map_err(|e| format!("can't run {prog}: {e}"))?;
    let mut child = CapturedChild {
        child,
        reaped: false,
    };
    let mut stdout_pipe = child.child.stdout.take().ok_or("missing stdout pipe")?;
    let mut stderr_pipe = child.child.stderr.take().ok_or("missing stderr pipe")?;
    nonblocking(stdout_pipe.as_raw_fd()).map_err(|e| e.to_string())?;
    nonblocking(stderr_pipe.as_raw_fd()).map_err(|e| e.to_string())?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let (mut stdout_done, mut stderr_done) = (false, false);
    let mut stopping = None;
    loop {
        let exited = child
            .exited()
            .map_err(|e| format!("waiting for child: {e}"))?;
        if (interrupted() || exited) && stopping.is_none() {
            child.signal(if interrupted() {
                STOP_SIGNAL.load(Ordering::SeqCst)
            } else {
                15
            });
            stopping = Some(Instant::now());
        }
        if stopping.is_some_and(|t| t.elapsed() >= Duration::from_secs(5)) {
            child.signal(9);
        }
        if !stdout_done {
            stdout_done = drain_pipe(&mut stdout_pipe, &mut stdout).map_err(|e| e.to_string())?;
        }
        if !stderr_done {
            stderr_done = drain_pipe(&mut stderr_pipe, &mut stderr).map_err(|e| e.to_string())?;
        }
        if exited && stdout_done && stderr_done {
            break;
        }
        // A descendant that deliberately escaped the process group can retain a pipe.
        // Bound that wait too; dropping the pipes must not strand reader threads.
        if stopping.is_some_and(|t| t.elapsed() >= Duration::from_secs(6)) {
            return Err(if interrupted() {
                STOPPED.into()
            } else {
                "command exited but descendants kept its output pipes open".into()
            });
        }
        progress::tick();
        std::thread::sleep(Duration::from_millis(30));
    }
    // Keep the leader unreaped until the last group signal, pinning its PID/PGID
    // so cleanup can never signal an unrelated, recycled process group.
    child.signal(9);
    let status = child.child.wait().map_err(|e| e.to_string())?;
    child.reaped = true;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
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

struct CapturedChild {
    child: std::process::Child,
    reaped: bool,
}

impl CapturedChild {
    fn signal(&self, sig: i32) {
        unsafe {
            kill(-(self.child.id() as i32), sig);
        }
    }

    fn exited(&self) -> io::Result<bool> {
        // Linux siginfo_t is 128 bytes. Only si_signo is inspected; waitid with
        // WNOWAIT observes exit without reaping the process-group leader.
        let mut info = [0u64; 16];
        let rc = unsafe {
            waitid(
                1, /* P_PID */
                self.child.id(),
                info.as_mut_ptr(),
                4 /* WEXITED */ | 1 /* WNOHANG */ | 0x01000000, /* WNOWAIT */
            )
        };
        if rc == 0 {
            Ok(info[0] != 0)
        } else {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.signal(9);
            let _ = self.child.wait();
        }
    }
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe {
        fcntl(fd, 3 /* F_GETFL */)
    };
    if flags < 0
        || unsafe {
            fcntl(
                fd,
                4,             /* F_SETFL */
                flags | 0x800, /* O_NONBLOCK */
            )
        } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buf = [0u8; 8192];
    // Bound each batch so a continuously writing child cannot starve signals
    // or the other pipe.
    for _ in 0..16 {
        match pipe.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

fn wait_child(
    child: &mut std::process::Child,
    group: bool,
    mut pump: impl FnMut(),
) -> Res<std::process::ExitStatus> {
    let mut signalled = None;
    loop {
        pump();
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
            pump();
            return Ok(st);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn relay_interactive(stream: &mut impl Read, stderr: bool, pty: bool, errors: &mut Vec<String>) {
    let mut bytes = Vec::new();
    if let Err(e) = drain_pipe(stream, &mut bytes) {
        // Linux PTY masters report EIO when the last slave is closed: normal EOF.
        if !(pty && e.raw_os_error() == Some(5)) && errors.is_empty() {
            errors.push(format!("interactive output: {e}"));
        }
    }
    if !bytes.is_empty() {
        if stderr {
            let _ = io::stderr().write_all(&bytes);
        } else {
            let _ = io::stdout().write_all(&bytes);
            let _ = io::stdout().flush();
        }
        if let Ok(mut log) = LOG.lock() {
            log.write_bytes(&bytes);
        }
    }
}

/// Runs a command attached to the terminal (sudo password, dnf's confirmation).
fn run_interactive(prog: &str, args: &[&str]) -> Res<bool> {
    log(&format!("$ {} {}\n", prog, args.join(" ")));
    if interrupted() {
        return Err(STOPPED.into());
    }
    // Configure nonblocking readers before spawning: setup failure cannot leave
    // a privileged installer running without supervision. Stdin remains the real
    // terminal; no input or passwords are captured. Only stdout/stderr are logged.
    let (mut output, stdout_writer, stderr_writer) =
        interactive_output::prepare().map_err(|e| e.to_string())?;
    let _screen = progress::InteractiveScreen::enter();
    let mut child = Command::new(prog)
        .args(args)
        .stdout(stdout_writer)
        .stderr(stderr_writer)
        .spawn()
        .map_err(|e| format!("can't run {prog}: {e}"))?;
    let mut errors = Vec::new();
    let st = wait_child(&mut child, false, || match &mut output {
        interactive_output::Output::Terminal(pty) => {
            if let Err(e) = pty.refresh_size() {
                if errors.is_empty() {
                    errors.push(format!("terminal resize: {e}"));
                }
            }
            relay_interactive(&mut pty.master, false, true, &mut errors);
        }
        interactive_output::Output::Pipes(stdout, stderr) => {
            relay_interactive(stdout, false, false, &mut errors);
            relay_interactive(stderr, true, false, &mut errors);
        }
    })?;
    if interrupted() {
        return Err(STOPPED.into());
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
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
            // O_NOFOLLOW|O_CLOEXEC: Rust also sets CLOEXEC by default; keep it
            // explicit so a future custom_flags change cannot drop it.
            .custom_flags(0x20000 | 0x80000 /* O_NOFOLLOW | O_CLOEXEC on Linux */)
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
    step(Stage::Repacking, "Repacking the .deb so alien can read it");
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

type Ownership = BTreeMap<String, Vec<String>>;
static OWNERS: OnceLock<Mutex<BTreeMap<String, Option<Vec<String>>>>> = OnceLock::new();

fn rpm_ownership_query(args: &[&str]) -> Option<Out> {
    let mut command = vec!["LC_ALL=C", RPM];
    command.extend_from_slice(args);
    run("/usr/bin/env", &command, None).ok()
}

fn owner_packages(paths: &[String], out: &Out) -> Option<BTreeSet<String>> {
    if !out.stderr.is_empty() {
        return None;
    }
    let mut packages = BTreeSet::new();
    let mut unowned = false;
    for line in out.stdout.lines() {
        if let Some(name) = line.strip_prefix("OWNER\t") {
            if name.is_empty() || name.chars().any(char::is_control) {
                return None;
            }
            packages.insert(name.to_string());
        } else if paths
            .iter()
            .any(|p| line == format!("file {p} is not owned by any package"))
        {
            unowned = true;
        } else {
            return None;
        }
    }
    if !out.ok && !(out.code == Some(1) && unowned) {
        return None;
    }
    if out.ok && packages.is_empty() {
        return None;
    }
    if out.ok && unowned {
        return None;
    }
    Some(packages)
}

/// Use RPM's file index to select candidate packages, then verify literal file
/// ownership in their headers. Virtual provides and symlink targets are never
/// mistaken for ownership of the requested path. Any query/parse failure is None.
fn query_owners(paths: &[String]) -> Option<Ownership> {
    let mut map: Ownership = paths.iter().map(|p| (p.clone(), Vec::new())).collect();
    if paths.is_empty() {
        return Some(map);
    }
    let mut args = vec!["-q", "--whatprovides", "--qf", "OWNER\t%{NAME}\n", "--"];
    args.extend(paths.iter().map(String::as_str));
    let packages = owner_packages(paths, &rpm_ownership_query(&args)?)?;
    if packages.is_empty() {
        return Some(map);
    }
    let mut args = vec!["-q", "--qf", "[%{FILENAMES}\t%{=NAME}\n]", "--"];
    args.extend(packages.iter().map(String::as_str));
    let out = rpm_ownership_query(&args)?;
    if !out.ok || !out.stderr.is_empty() {
        return None;
    }
    for line in out.stdout.lines() {
        let (path, name) = line.rsplit_once('\t')?;
        if !packages.contains(name) || !path.starts_with('/') {
            return None;
        }
        if let Some(owners) = map.get_mut(path) {
            if !owners.iter().any(|n| n == name) {
                owners.push(name.into());
            }
        }
    }
    Some(map)
}

fn cache_owners(paths: &[String]) {
    let cache = OWNERS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let missing: BTreeSet<String> = match cache.lock() {
        Ok(cache) => paths
            .iter()
            .filter(|p| !cache.contains_key(*p))
            .cloned()
            .collect(),
        Err(_) => return,
    };
    let missing: Vec<_> = missing.into_iter().collect();
    // Bound argv size, including unusually long package-controlled filenames.
    for chunk in missing.chunks(64) {
        let result = query_owners(chunk);
        if let Ok(mut cache) = cache.lock() {
            for path in chunk {
                cache.insert(
                    path.clone(),
                    result.as_ref().and_then(|map| map.get(path).cloned()),
                );
            }
        }
    }
}

/// None = query failure; Some(empty) = verified unowned. Never conflate the two.
fn owners(path: &str) -> Option<Vec<String>> {
    cache_owners(&[path.to_string()]);
    OWNERS.get()?.lock().ok()?.get(path).cloned().flatten()
}

// Accept alien's generated layout, not arbitrary RPM programs. In particular,
// a Debian description or filename must never become an RPM directive/macro.
//
// Invariant: after %% → % expansion, no package-controlled line may begin with
// '%'. Doubling alone is not enough — "%%prep" becomes "%prep", which rpm then
// treats as a real section header and may execute via rpmbuild. Fail closed on
// any body line that, after stripping leading Unicode whitespace (and VT),
// starts with '%'; also reject ASCII controls other than tab in free-text
// bodies. Body text is split on '\n' only so a bare '\r' cannot hide a '%'
// line. Checks run on the final text that is written into the sanitized spec.
fn sanitize_spec(text: &str, pkg: &str) -> Res<String> {
    let (prefix, rest) = text
        .split_once("\n%description\n")
        .ok_or("alien's spec has no description section")?;
    let (description, files) = rest
        .rsplit_once("\n%files\n")
        .ok_or("alien's spec has no files section")?;
    let mut clean = String::new();
    let mut in_script = false;
    let mut script_field = "";
    let mut script_line = 0usize;
    let mut headers = HashSet::new();
    // Split on '\n' only (not str::lines): a bare '\r' must remain in-line so
    // it cannot hide a leading '%' from the body checks below.
    for line in prefix.split('\n') {
        if matches!(line, "%pre" | "%post" | "%preun" | "%postun") {
            if !headers.insert(line.to_string()) {
                return Err("duplicate script section in alien spec".into());
            }
            in_script = true;
            script_field = line;
            script_line = 0;
            writeln!(clean, "{line}").unwrap();
        } else if in_script {
            script_line += 1;
            // Alien already doubles percent signs in encoded scripts. Preserve
            // that quoting, and neutralize any unescaped RPM macros as well.
            // Reject on this final emitted line (not the pre-transform copy).
            let emitted = line.replace("%%", "%").replace('%', "%%");
            reject_body_controls(script_field, script_line, &emitted)?;
            reject_leading_percent(script_field, script_line, &emitted)?;
            writeln!(clean, "{emitted}").unwrap();
        } else if line.is_empty() {
            clean.push('\n');
        } else if let Some(version) = line.strip_prefix("# deb2rpm Debian version: ") {
            if !spec_version(version) {
                return Err("invalid Debian version in spec".into());
            }
            writeln!(clean, "{line}").unwrap();
        } else if matches!(
            line,
            "%define _rpmdir ../"
                | "%define _rpmfilename %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm"
                | "%define _unpackaged_files_terminate_build 0"
        ) {
            writeln!(clean, "{line}").unwrap();
        } else {
            let (key, value) = line
                .split_once(':')
                .ok_or("unexpected directive in alien spec")?;
            if !matches!(
                key,
                "Buildroot"
                    | "Name"
                    | "Version"
                    | "Release"
                    | "Requires"
                    | "Summary"
                    | "License"
                    | "Distribution"
                    | "Group"
            ) || !headers.insert(key.into())
            {
                return Err(format!("unexpected or duplicate spec field: {key}"));
            }
            // Single-line fields must not carry embedded newlines that could
            // place package text at column 0 of a later physical line.
            if value.contains('\n') || value.contains('\r') {
                return Err(format!("invalid spec field: {key} (embedded newline)"));
            }
            if (key == "Name" && value.trim() != pkg)
                || (matches!(key, "Version" | "Release") && !spec_version(value.trim()))
                || value.chars().any(|c| c.is_control() && c != '\t')
            {
                return Err(format!("invalid spec field: {key}"));
            }
            writeln!(clean, "{key}:{}", value.replace('%', "%%")).unwrap();
        }
    }
    for required in ["Name", "Version", "Release"] {
        if !headers.contains(required) {
            return Err(format!("missing spec field: {required}"));
        }
    }
    let description_emitted = description.replace('%', "%%");
    for (i, line) in description_emitted.split('\n').enumerate() {
        reject_body_controls("%description", i + 1, line)?;
        reject_leading_percent("%description", i + 1, line)?;
    }
    clean.push_str("%description\n");
    clean.push_str(&description_emitted);
    clean.push_str("\n%files\n");
    // A quoted path must not span a newline (would put package text at col 0).
    {
        let mut in_quote = false;
        for c in files.chars() {
            match c {
                '"' => in_quote = !in_quote,
                '\n' | '\r' if in_quote => {
                    return Err("unsafe filename in alien spec".into());
                }
                _ => {}
            }
        }
    }
    for line in files.split('\n') {
        if line.trim().is_empty() {
            clean.push('\n');
            continue;
        }
        let path = line
            .strip_prefix("%dir ")
            .or_else(|| line.strip_prefix("%config "))
            .unwrap_or(line);
        let path = path
            .strip_prefix('"')
            .and_then(|p| p.strip_suffix('"'))
            .ok_or("unsupported file entry in alien spec")?;
        if !path.starts_with('/')
            || path.contains('"')
            || path.contains('\\')
            || path.contains('\n')
            || path.contains('\r')
            || path.chars().any(char::is_control)
        {
            return Err("unsafe filename in alien spec".into());
        }
        let directive = if line.starts_with("%dir ") {
            "%dir "
        } else if line.starts_with("%config ") {
            "%config "
        } else {
            ""
        };
        writeln!(clean, "{directive}\"{}\"", path.replace('%', "%%")).unwrap();
    }
    Ok(clean)
}

fn reject_body_controls(field: &str, line_no: usize, line: &str) -> Res<()> {
    for c in line.chars() {
        if c.is_ascii_control() && c != '\t' {
            return Err(format!(
                "{field} line {line_no}: contains ASCII control character U+{:04X}, so the package is rejected",
                c as u32
            )
            .into());
        }
    }
    Ok(())
}

fn reject_leading_percent(field: &str, line_no: usize, line: &str) -> Res<()> {
    // Match rpm's isspace()-style leading skip, including VT; also strip any
    // Unicode whitespace (e.g. NBSP) so a disguised "%prep" cannot slip through.
    let trimmed = line.trim_start_matches(|c: char| c.is_whitespace() || c == '\x0b');
    // Any leading '%' (case of the following name is irrelevant) becomes a
    // section header after %% expansion; do not try to repair the text.
    if trimmed.starts_with('%') {
        return Err(format!(
            "{field} line {line_no}: begins with '%'; after %% expansion rpm would parse this as a section header (e.g. %prep/%build/%files), so the package is rejected"
        )
        .into());
    }
    Ok(())
}

fn spec_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+:~_-".contains(c))
}

// A bare directory in %files recursively claims its contents. Expand it into
// explicit entries before filtering, so dropping a shared parent never drops
// payload files or lets a recursive parent reintroduce a filtered directory.
fn explicit_file_directories(text: &str, tree: &Path) -> Res<String> {
    let (header, files) = text.split_once("\n%files\n").ok_or("missing %files")?;
    let entry_path = |line: &str| {
        unquote(
            line.strip_prefix("%dir ")
                .or_else(|| line.strip_prefix("%config "))
                .unwrap_or(line),
        )
        .replace("%%", "%")
    };
    let mut listed: HashSet<String> = files
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(entry_path)
        .collect();
    let mut out = format!("{header}\n%files\n");
    let mut stack = Vec::new();
    for line in files.lines() {
        let path = entry_path(line);
        let source = tree.join(path.trim_start_matches('/'));
        if line.starts_with('"') && source.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            let _ = writeln!(out, "%dir {line}");
            stack.push(source);
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    let mut visited = HashSet::new();
    let canonical_tree = fs::canonicalize(tree).ok();
    while let Some(dir) = stack.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        let canonical = fs::canonicalize(&dir).map_err(|e| e.to_string())?;
        if !canonical_tree
            .as_ref()
            .is_some_and(|root| canonical.starts_with(root))
        {
            return Err(format!(
                "payload directory escapes buildroot: {}",
                dir.display()
            ));
        }
        let mut entries = fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let source = entry.path();
            let is_dir = entry.file_type().map_err(|e| e.to_string())?.is_dir();
            if is_dir {
                stack.push(source.clone());
            }
            let relative = source
                .strip_prefix(tree)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("non-UTF-8 payload path")?;
            if relative.contains(['"', '\\']) || relative.chars().any(char::is_control) {
                return Err("unsafe filename in payload directory".into());
            }
            let path = format!("/{relative}");
            if listed.insert(path.clone()) {
                let directive = if is_dir { "%dir " } else { "" };
                let _ = writeln!(out, "{directive}\"{}\"", path.replace('%', "%%"));
            }
        }
    }
    Ok(out)
}

fn fix_spec(text: &str, pkg: &str, installing: bool, tree: &Path) -> Res<SpecFix> {
    let sanitized = sanitize_spec(text, pkg)?;
    let explicit = explicit_file_directories(&sanitized, tree)?;
    if installing {
        let paths = explicit
            .split_once("\n%files\n")
            .ok_or("missing %files")?
            .1
            .lines()
            .filter_map(|line| line.strip_prefix("%dir "))
            .map(|path| unquote(path).replace("%%", "%"))
            .filter(|path| path != "/" && Path::new(path).exists())
            .collect::<Vec<_>>();
        cache_owners(&paths);
    }
    let text = explicit.as_str();
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
            let path = unquote(trimmed["%dir".len()..].trim()).replace("%%", "%");
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

#[derive(Clone)]
struct DependencyElf {
    path: String,
    identity: [u8; 4],
    needed: Vec<String>,
}

fn is_node_prebuild(path: &str) -> bool {
    let Some((_, relative)) = path.rsplit_once("/node_modules/") else {
        return false;
    };
    if path.ends_with(".node") {
        return true;
    }
    // FFI packages also ship shared libraries. Limit these to files directly
    // inside a platform-specific package, not arbitrary vendored .so files.
    if !path.ends_with(".so") {
        return false;
    }
    let parts: Vec<_> = relative.split('/').collect();
    let expected = if relative.starts_with('@') { 3 } else { 2 };
    if parts.len() != expected {
        return false;
    }
    let package = parts[parts.len() - 2];
    ["linux-x64", "linux-arm64"].iter().any(|platform| {
        ["musl", "gnu", "glibc"]
            .iter()
            .any(|libc| package.ends_with(&format!("-{platform}-{libc}")))
    })
}

fn native_glibc(elf: &DependencyElf, identity: [u8; 4]) -> bool {
    elf.identity == identity
        && elf.needed.iter().any(|n| n == "libc.so.6")
        && !elf
            .needed
            .iter()
            .any(|n| n.starts_with("libc.musl-") || n == "libc.so")
}

// These are dynamically loaded execution providers, not the CPU runtime itself.
// Recognize only the upstream Node layout with a complete native runtime, and
// retain requirements if any payload ELF directly links to the GPU provider.
fn optional_onnx_provider(
    elf: &DependencyElf,
    elfs: &[DependencyElf],
    identity: [u8; 4],
    cpu: &str,
) -> bool {
    let Some((dir, name)) = elf.path.rsplit_once('/') else {
        return false;
    };
    if !matches!(
        name,
        "libonnxruntime_providers_cuda.so" | "libonnxruntime_providers_tensorrt.so"
    ) || !dir.ends_with(&format!(
        "/node_modules/onnxruntime-node/bin/napi-v3/linux/{cpu}"
    )) || !native_glibc(elf, identity)
        || elfs.iter().any(|consumer| {
            consumer
                .needed
                .iter()
                .any(|needed| needed.rsplit('/').next() == Some(name))
        })
    {
        return false;
    }
    [
        "libonnxruntime.so.1",
        "onnxruntime_binding.node",
        "libonnxruntime_providers_shared.so",
    ]
    .iter()
    .all(|name| {
        elfs.iter().any(|native| {
            native.path == format!("{dir}/{name}")
                && native_glibc(native, identity)
                && !native.needed.iter().any(|needed| {
                    matches!(
                        needed.rsplit('/').next(),
                        Some(
                            "libonnxruntime_providers_cuda.so"
                                | "libonnxruntime_providers_tensorrt.so"
                        )
                    )
                })
                && (*name != "onnxruntime_binding.node"
                    || native.needed.iter().any(|n| n == "libonnxruntime.so.1"))
        })
    })
}

// Only verified optional Node alternatives and independent ONNX GPU plugins.
// Keep dependencies for arbitrary foreign binaries and musl-only applications.
fn dependency_exclusions(elfs: &[DependencyElf], arch: &str) -> Vec<String> {
    let (identity, platform) = match arch {
        "x86_64" => ([2, 1, 62, 0], "linux-x64"),
        "aarch64" => ([2, 1, 183, 0], "linux-arm64"),
        _ => return Vec::new(),
    };
    let mut excluded = BTreeSet::new();
    for elf in elfs {
        let cpu = if platform == "linux-x64" {
            "x64"
        } else {
            "arm64"
        };
        if optional_onnx_provider(elf, elfs, identity, cpu) {
            excluded.insert(elf.path.clone());
            continue;
        }
        if !is_node_prebuild(&elf.path) {
            continue;
        }
        let foreign = elf.identity != identity;
        let musl = elf
            .needed
            .iter()
            .any(|n| n.starts_with("libc.musl-") || n == "libc.so")
            && (elf.path.contains("-musl/")
                || elf.path.contains("-musl.")
                || elf.path.contains("/prebuilds/linuxmusl-")
                || elf.path.ends_with(".musl.node"));
        if !foreign && !musl {
            continue;
        }
        let mut candidate = elf.path.clone();
        if foreign {
            let other = if platform == "linux-x64" {
                "linux-arm64"
            } else {
                "linux-x64"
            };
            candidate = candidate
                .replace(&format!("{other}/"), &format!("{platform}/"))
                .replace(&format!("{other}-"), &format!("{platform}-"))
                .replace(&format!("{other}."), &format!("{platform}."));
        }
        let candidates = if musl {
            vec![
                candidate
                    .replace("/prebuilds/linuxmusl-", "/prebuilds/linux-")
                    .replace("-musl", "-gnu")
                    .replace(".musl.node", ".node"),
                candidate
                    .replace("/prebuilds/linuxmusl-", "/prebuilds/linux-")
                    .replace("-musl", "-glibc")
                    .replace(".musl.node", ".node"),
            ]
        } else {
            vec![candidate]
        };
        if candidates.iter().any(|path| {
            path != &elf.path
                && elfs
                    .iter()
                    .any(|native| &native.path == path && native_glibc(native, identity))
        }) {
            excluded.insert(elf.path.clone());
        }
    }
    excluded.into_iter().collect()
}

fn prebuild_exclusions(tree: &Path, arch: &str) -> Res<Vec<String>> {
    let mut elfs = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let relative = format!(
                    "/{}",
                    path.strip_prefix(tree)
                        .map_err(|e| e.to_string())?
                        .to_str()
                        .ok_or("non-UTF-8 payload path")?
                );
                // Inspect all ELFs so a direct dependency from an unrelated
                // executable prevents treating a GPU provider as optional.
                let Some(identity) = elf_identity(&path) else {
                    continue;
                };
                let out = run(
                    READELF,
                    &["-d", "-W", path.to_str().ok_or("non-UTF-8 ELF path")?],
                    None,
                )?;
                if !out.ok {
                    return Err(format!(
                        "could not inspect ELF {}: {}",
                        path.display(),
                        out.stderr
                    ));
                }
                let needed = out
                    .stdout
                    .lines()
                    .filter(|l| l.contains("(NEEDED)"))
                    .filter_map(|l| l.split('[').nth(1)?.split(']').next().map(str::to_string))
                    .collect();
                elfs.push(DependencyElf {
                    path: relative,
                    identity,
                    needed,
                });
            }
        }
    }
    Ok(dependency_exclusions(&elfs, arch))
}

// RPM matches installed paths without the buildroot. Escape ERE and RPM syntax.
fn dependency_exclude_regex(paths: &[String]) -> String {
    let literals: Vec<String> = paths
        .iter()
        .map(|path| {
            let mut out = String::new();
            for c in path.chars() {
                if ".[]{}()*+?^$|\\".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            // --define consumes one backslash layer before the regex is used.
            out.replace('\\', "\\\\").replace('%', "%%")
        })
        .collect();
    format!("^({})$", literals.join("|"))
}

// --- rpmbuild ------------------------------------------------------------------------------------

fn rpmbuild(dir: &Path, spec: &Path, arch: &str, work: &Path, excluded: &[String]) -> Res<PathBuf> {
    let d = dir.to_str().ok_or("path isn't UTF-8")?;
    let s = spec.to_str().ok_or("path isn't UTF-8")?;
    let buildroot = format!("--buildroot={d}");
    let topdir = format!("_topdir {}", work.join("rpmbuild").display());
    // fakeroot: ownership in the payload comes from %defattr; this keeps rpmbuild from tripping over
    // files the unprivileged user can't stat the "real" owner of.
    // No post-processing: Fedora's brp scripts strip binaries, byte-compile Python, rewrite shebangs and
    // add build-id links — the payload must stay byte-identical to the .deb.
    // Low-cost zstd compression and bounded RPM workers keep local conversions
    // responsive. Use at most half the available CPUs, capped at two workers.
    let workers = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 2))
        .unwrap_or(1);
    let payload = format!("_binary_payload w3T{workers}.zstdio");
    let build_cpus = format!("_smp_build_ncpus {workers}");
    let build_threads = format!("_smp_build_nthreads {workers}");
    let max_cpus = format!("_smp_ncpus_max {workers}");
    let max_threads = format!("_smp_nthreads_max {workers}");
    for path in excluded {
        info(&format!(
            "excluding optional native component from RPM requirements/provides: {path}"
        ));
    }
    let mut dependency_args = Vec::new();
    if let Some(alternative) = tinfo_compat::alternative(dir, arch, excluded) {
        info(&format!(
            "using verified terminfo dependency alternative: {alternative}"
        ));
        let original = fs::read_to_string(spec).map_err(|e| e.to_string())?;
        fs::write(spec, format!("Requires: {alternative}\n{original}"))
            .map_err(|e| e.to_string())?;
        // Replace exactly one generated capability with the explicit rich
        // dependency above. The SONAME requirement and all other automatic
        // requirements/provides remain generated normally.
        let regex = dependency_exclude_regex(&[tinfo_compat::REQUIREMENT.into()]);
        dependency_args.push(format!("__requires_exclude {regex}"));
    }
    if !excluded.is_empty() {
        let regex = dependency_exclude_regex(excluded);
        dependency_args.push(format!("__requires_exclude_from {regex}"));
        dependency_args.push(format!("__provides_exclude_from {regex}"));
    }
    let mut args = vec![
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
        &payload,
        "--define",
        &build_cpus,
        "--define",
        &build_threads,
        "--define",
        &max_cpus,
        "--define",
        &max_threads,
        "-bb",
        "--target",
        arch,
        s,
    ];
    for definition in &dependency_args {
        args.extend(["--define", definition]);
    }
    let out = run(FAKEROOT, &args, Some(dir))?;
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
    let existing = list
        .stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(path, perms)| {
            !perms.starts_with('d') && Path::new(path).symlink_metadata().is_ok()
        })
        .map(|(path, _)| path.to_string())
        .collect::<Vec<_>>();
    cache_owners(&existing);
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
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x800 /* O_NONBLOCK on Linux */)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    file.read_exact(&mut b).ok()?;
    if &b[..4] != b"\x7fELF" {
        return None;
    }
    Some([b[4], b[5], b[18], b[19]]) // class, endianness, machine
}

/// Advisory only: do not execute binaries or claim to prove complete loader compatibility.
fn missing_libraries(tree: &Path, excluded: &[String]) -> LibraryReport {
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
        let relative = format!("/{}", elf.strip_prefix(tree).unwrap_or(&elf).display());
        if excluded.contains(&relative) {
            continue;
        }
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
    progress::start(opts.build_only);
    let result = convert(opts);
    progress::finish(result.is_ok(), interrupted());
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if interrupted() => {
            err_out(&format!("\n\x1b[38;2;60;115;135m{e}\x1b[0m\n"));
            ExitCode::from(130)
        }
        Err(e) => {
            err_out(&format!("\n\x1b[31merror:\x1b[0m {e}\n"));
            log(&format!("\nerror: {e}\n"));
            ExitCode::FAILURE
        }
    };
    if pause && !interrupted() {
        if let Err(e) = wait_for_enter() {
            if interrupted() {
                return ExitCode::from(130);
            }
            err_out(&format!("\nerror waiting for input: {e}\n"));
            return ExitCode::FAILURE;
        }
    }
    if interrupted() {
        ExitCode::from(130)
    } else {
        code
    }
}

fn wait_for_enter() -> io::Result<()> {
    let _terminal = close_prompt::Terminal::new()?;
    out("\nPress Enter to close… ");
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut keys = close_prompt::Input::default();
    let mut fd = PollFd {
        fd: input.as_raw_fd(),
        events: 1, /* POLLIN */
        revents: 0,
    };
    while !interrupted() {
        // A bounded poll closes the race between checking the flag and sleeping,
        // even on platforms where signal() installs SA_RESTART.
        let ready = unsafe { poll(&mut fd, 1, 50) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if ready > 0 && !interrupted() {
            let mut bytes = [0; 4096];
            match input.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    if bytes[..count].iter().any(|byte| keys.feed(*byte)) {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn convert(opts: Opts) -> Res<()> {
    if is_root() {
        return Err("run deb2rpm as your normal user; it uses sudo only for dnf".into());
    }
    let mut tools = vec![ALIEN, FAKEROOT, DPKG_DEB, RPM, RPMBUILD];
    if !opts.build_only {
        tools.extend([DNF, SUDO]);
    }
    if opts.pause && io::stdin().is_terminal() {
        tools.push("/usr/bin/stty");
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
                "/usr/bin/stty" => "coreutils",
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
            info(&format!("found a leftover work directory: {}", d.display()));
            if !opts.keep_work && !finish_work(&d, opts.delete_work) {
                warn(&format!("couldn't clean {}", d.display()));
            }
        }
    }

    step(Stage::Reading, "Reading the .deb");
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
        Err(_) => info(&format!(
            "work dir and full log kept for inspection: {}",
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

    step(Stage::Unpacking, "Unpacking with alien");
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
    let excluded = prebuild_exclusions(&dir, arch)?;
    let missing = missing_libraries(&dir, &excluded);

    step(Stage::Fixing, "Fixing alien's spec");
    let fix = fix_spec(&original, &deb.name, !opts.build_only, &dir)?;
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

    step(Stage::Building, "Building the rpm");
    let t = Instant::now();
    let rpm = rpmbuild(&dir, &spec_path, arch, work, &excluded)?;
    info(&format!(
        "{} in {:.0?}",
        rpm.file_name().unwrap_or_default().to_string_lossy(),
        t.elapsed()
    ));

    step(Stage::Checking, "Checking the rpm");
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
    step(
        Stage::Installing,
        &format!("Installing ({verb}) — dnf will ask for your password"),
    );
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
    step(Stage::Done, &format!("Done — {name} {built} is installed"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that spawn/fork with tests that assert flock release.
    /// Under parallel cargo test, another thread's clone3(CLONE_VFORK) copies
    /// the process fd table (including a held lock fd). Until that child closes
    /// the CLOEXEC fd at exec, drop() of the lock does not release the OFD
    /// flock, so a re-take sees EAGAIN. Confirmed via strace -f. Holding this
    /// mutex for the whole test avoids that window — no retries or sleeps.
    static FORK_LOCK_TEST_GUARD: Mutex<()> = Mutex::new(());

    fn fork_lock_test_guard() -> std::sync::MutexGuard<'static, ()> {
        FORK_LOCK_TEST_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn temporary_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = env::temp_dir().join(format!(
            "deb2rpm-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&d).unwrap();
        d
    }

    #[test]
    fn live_lock_cannot_be_replaced() {
        let _guard = fork_lock_test_guard();
        let d = temporary_dir("lock");
        let first = Lock::take(&d).unwrap();
        assert!(Lock::take(&d).is_err());
        drop(first);
        // Closing the fd releases the flock; a second take must succeed without
        // retries. Retries would hide a real OFD/CLOEXEC leak.
        let second = Lock::take(&d).unwrap();
        drop(second);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn lock_rejects_symlinked_lock_path() {
        let _guard = fork_lock_test_guard();
        let d = temporary_dir("lock-symlink");
        let target = temporary_dir("lock-symlink-target");
        fs::write(target.join("elsewhere"), b"x").unwrap();
        std::os::unix::fs::symlink(target.join("elsewhere"), d.join("lock")).unwrap();
        let err = match Lock::take(&d) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("symlinked lock path must be rejected"),
        };
        assert!(
            err.contains("can't open") && err.contains("lock"),
            "expected clear open failure for symlinked lock, got {err}"
        );
        // O_NOFOLLOW → ELOOP on Linux ("Too many levels of symbolic links").
        assert!(
            err.contains("symbolic link") || err.contains("Too many levels"),
            "expected symlink/ELOOP detail, got {err}"
        );
        fs::remove_dir_all(d).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    fn addon(path: &str, machine: u8, needed: &[&str]) -> DependencyElf {
        DependencyElf {
            path: path.into(),
            identity: [2, 1, machine, 0],
            needed: needed.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn mixed_architecture_prebuilds_exclude_only_foreign_alternative() {
        let elfs = vec![
            addon(
                "/opt/app/node_modules/pty/prebuilds/linux-arm64/pty.node",
                183,
                &["libc.so.6"],
            ),
            addon(
                "/opt/app/node_modules/pty/prebuilds/linux-x64/pty.node",
                62,
                &["libc.so.6"],
            ),
            addon("/opt/required-arm64/tool", 183, &["libc.so.6"]),
        ];
        assert_eq!(
            dependency_exclusions(&elfs, "x86_64"),
            vec![elfs[0].path.clone()]
        );
        assert_eq!(
            dependency_exclusions(&elfs, "aarch64"),
            vec![elfs[1].path.clone()]
        );
        assert!(dependency_exclusions(&elfs[..1], "x86_64").is_empty());
        assert!(dependency_exclusions(&elfs, "noarch").is_empty());
    }

    #[test]
    fn mixed_libc_prebuilds_keep_glibc_and_standalone_musl() {
        for (musl, native, needed) in [
            (
                "better-sqlite3/prebuilds/linuxmusl-x64.node",
                "better-sqlite3/prebuilds/linux-x64.node",
                "libc.musl-x86_64.so.1",
            ),
            (
                "watcher-linux-x64-musl/watcher.node",
                "watcher-linux-x64-glibc/watcher.node",
                "libc.musl-x86_64.so.1",
            ),
            (
                "napi-linux-x64-musl/a.linux-x64-musl.node",
                "napi-linux-x64-gnu/a.linux-x64-gnu.node",
                "libc.so",
            ),
            (
                "prebuilds/linux-x64/a.musl.node",
                "prebuilds/linux-x64/a.node",
                "libc.musl-x86_64.so.1",
            ),
        ] {
            let elfs = vec![
                addon(&format!("/opt/app/node_modules/{musl}"), 62, &[needed]),
                addon(
                    &format!("/opt/app/node_modules/{native}"),
                    62,
                    &["libc.so.6"],
                ),
            ];
            assert_eq!(
                dependency_exclusions(&elfs, "x86_64"),
                vec![elfs[0].path.clone()]
            );
            assert!(dependency_exclusions(&elfs[..1], "x86_64").is_empty());
            let mut wrong = elfs.clone();
            wrong[1].identity[2] = 183;
            assert!(dependency_exclusions(&wrong, "x86_64").is_empty());
            wrong[1] = addon(&elfs[1].path, 62, &["libc.so"]);
            assert!(dependency_exclusions(&wrong, "x86_64").is_empty());
        }
        // Neither a suggestive filename nor unversioned libc alone proves an optional musl addon.
        let elfs = vec![
            addon("/opt/libc.so", 62, &["libc.so"]),
            addon("/opt/tool-musl.node", 62, &["libc.so"]),
        ];
        assert!(dependency_exclusions(&elfs, "x86_64").is_empty());
    }

    #[test]
    fn shared_library_prebuilds_require_a_verified_native_counterpart() {
        let musl = "/opt/T3 Code (Beta)/resources/app.asar.unpacked/node_modules/@ff-labs/fff-bin-linux-x64-musl/libfff_c.so";
        let native = musl.replace("-musl/", "-gnu/");
        let elfs = vec![
            addon(musl, 62, &["libc.so"]),
            addon(&native, 62, &["libc.so.6"]),
        ];
        assert_eq!(
            dependency_exclusions(&elfs, "x86_64"),
            vec![musl.to_string()]
        );
        assert!(dependency_exclusions(&elfs[..1], "x86_64").is_empty());
        for identity in [[2, 1, 183, 0], [1, 1, 62, 0], [2, 2, 0, 62]] {
            let mut wrong = elfs.clone();
            wrong[1].identity = identity;
            assert!(dependency_exclusions(&wrong, "x86_64").is_empty());
        }
        let wrong_libc = vec![elfs[0].clone(), addon(&native, 62, &["libc.so"])];
        assert!(dependency_exclusions(&wrong_libc, "x86_64").is_empty());
        let unrelated_name = vec![
            elfs[0].clone(),
            addon(
                &native.replace("libfff_c.so", "other.so"),
                62,
                &["libc.so.6"],
            ),
        ];
        assert!(dependency_exclusions(&unrelated_name, "x86_64").is_empty());
        for path in [
            "/opt/lib-linux-x64-musl/libfff_c.so",
            "/opt/app/node_modules/unrelated-musl/libfff_c.so",
            "/opt/app/node_modules/pkg/vendor/lib-linux-x64-musl/libfff_c.so",
        ] {
            let elfs = vec![
                addon(path, 62, &["libc.so"]),
                addon(&path.replace("-musl/", "-gnu/"), 62, &["libc.so.6"]),
            ];
            assert!(dependency_exclusions(&elfs, "x86_64").is_empty(), "{path}");
        }
    }

    #[test]
    fn onnx_gpu_plugins_require_an_independent_native_runtime() {
        let dir = "/opt/app/node_modules/onnxruntime-node/bin/napi-v3/linux/x64";
        let elfs = vec![
            addon(
                &format!("{dir}/libonnxruntime_providers_cuda.so"),
                62,
                &["libc.so.6", "libcudart.so.12"],
            ),
            addon(
                &format!("{dir}/libonnxruntime_providers_tensorrt.so"),
                62,
                &["libc.so.6", "libnvinfer.so.10"],
            ),
            addon(&format!("{dir}/libonnxruntime.so.1"), 62, &["libc.so.6"]),
            addon(
                &format!("{dir}/onnxruntime_binding.node"),
                62,
                &["libc.so.6", "libonnxruntime.so.1"],
            ),
            addon(
                &format!("{dir}/libonnxruntime_providers_shared.so"),
                62,
                &["libc.so.6"],
            ),
        ];
        let expected = vec![elfs[0].path.clone(), elfs[1].path.clone()];
        assert_eq!(dependency_exclusions(&elfs, "x86_64"), expected);
        assert!(dependency_exclusions(&elfs, "aarch64").is_empty());
        for index in 2..5 {
            let mut missing = elfs.clone();
            missing.remove(index);
            assert!(dependency_exclusions(&missing, "x86_64").is_empty());
            for identity in [[2, 1, 183, 0], [1, 1, 62, 0], [2, 2, 0, 62]] {
                let mut wrong = elfs.clone();
                wrong[index].identity = identity;
                assert!(dependency_exclusions(&wrong, "x86_64").is_empty());
            }
            let mut musl = elfs.clone();
            musl[index].needed = vec!["libc.so".into()];
            assert!(dependency_exclusions(&musl, "x86_64").is_empty());
        }
        for consumer in [2, 3, 4] {
            let mut linked = elfs.clone();
            linked[consumer]
                .needed
                .push("libonnxruntime_providers_cuda.so".into());
            assert_eq!(
                dependency_exclusions(&linked, "x86_64"),
                Vec::<String>::new()
            );
        }
        let mut linked = elfs.clone();
        linked.push(addon(
            "/opt/app/required-tool",
            62,
            &["libonnxruntime_providers_cuda.so"],
        ));
        assert_eq!(
            dependency_exclusions(&linked, "x86_64"),
            vec![elfs[1].path.clone()]
        );
        linked.last_mut().unwrap().needed = vec![elfs[0].path.clone()];
        assert_eq!(
            dependency_exclusions(&linked, "x86_64"),
            vec![elfs[1].path.clone()]
        );
        for from in ["onnxruntime-node", "napi-v3/linux/x64"] {
            let unrelated: Vec<_> = elfs
                .iter()
                .cloned()
                .map(|mut elf| {
                    elf.path = elf.path.replace(from, "unrelated");
                    elf
                })
                .collect();
            assert!(dependency_exclusions(&unrelated, "x86_64").is_empty());
        }
    }

    #[test]
    fn dependency_regex_survives_rpm_expansion_as_literal_paths() {
        let paths = vec!["/opt/app+(1)/node_modules/a[2]/50%{name}.node".into()];
        let regex = dependency_exclude_regex(&paths);
        let definition = format!("__requires_exclude_from {regex}");
        let out = run(
            RPM,
            &[
                "--define",
                &definition,
                "--eval",
                "%{__requires_exclude_from}",
            ],
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(
            out.stdout.trim(),
            r"^(/opt/app\+\(1\)/node_modules/a\[2\]/50%\{name\}\.node)$"
        );
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn rpm_filters_mixed_prebuild_files_and_preserves_native_dependencies() {
        let _guard = fork_lock_test_guard();
        let work = temporary_dir("mixed-rpm");
        let tree = work.join("payload");
        let modules = tree.join("opt/T3 Code (Beta)/node_modules");
        let native = modules.join("pty/prebuilds/linux-x64/pty.node");
        let foreign = modules.join("pty/prebuilds/linux-arm64/pty.node");
        let glibc = modules.join("watcher-linux-x64-glibc/watcher.node");
        let musl = modules.join("watcher-linux-x64-musl/watcher.node");
        let glibc_so = modules.join("@ff-labs/fff-bin-linux-x64-gnu/libfff_c.so");
        let musl_so = modules.join("@ff-labs/fff-bin-linux-x64-musl/libfff_c.so");
        for p in [&native, &foreign, &glibc, &musl, &glibc_so, &musl_so] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        fs::write(
            work.join("addon.c"),
            b"extern int puts(const char *); int addon(void) { return puts(\"probe\"); }\n",
        )
        .unwrap();
        assert!(Command::new("/usr/bin/gcc")
            .args(["-shared", "-fPIC", "-Wl,-soname,libnativeprobe.so"])
            .arg(work.join("addon.c"))
            .arg("-o")
            .arg(&native)
            .status()
            .unwrap()
            .success());
        fs::copy(&native, &glibc).unwrap();
        fs::copy(&native, &glibc_so).unwrap();
        let mut bytes = fs::read(&native).unwrap();
        bytes[18] = 183; // a real dynamic ELF, with a foreign e_machine
        let soname = b"libnativeprobe.so";
        let offset = bytes
            .windows(soname.len())
            .position(|w| w == soname)
            .unwrap();
        bytes[offset..offset + soname.len()].copy_from_slice(b"libforeignprob.so");
        fs::write(&foreign, bytes).unwrap();
        fs::write(work.join("stub.c"), b"int probe(void) { return 0; }\n").unwrap();
        let libc = work.join("libc.musl-x86_64.so.1");
        assert!(Command::new("/usr/bin/gcc")
            .args([
                "-shared",
                "-fPIC",
                "-nostdlib",
                "-Wl,-soname,libc.musl-x86_64.so.1"
            ])
            .arg(work.join("stub.c"))
            .arg("-o")
            .arg(&libc)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("/usr/bin/gcc")
            .args([
                "-shared",
                "-fPIC",
                "-nostdlib",
                "-Wl,--no-as-needed",
                "-Wl,-soname,libmuslprobe.so"
            ])
            .arg(work.join("stub.c"))
            .arg(&libc)
            .arg("-o")
            .arg(&musl)
            .status()
            .unwrap()
            .success());
        // Reproduce the FFI library's unversioned musl libc requirement.
        let libc_so = work.join("libc.so");
        assert!(Command::new("/usr/bin/gcc")
            .args(["-shared", "-fPIC", "-nostdlib", "-Wl,-soname,libc.so"])
            .arg(work.join("stub.c"))
            .arg("-o")
            .arg(&libc_so)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("/usr/bin/gcc")
            .args([
                "-shared",
                "-fPIC",
                "-nostdlib",
                "-Wl,--no-as-needed",
                "-Wl,-soname,libmuslsoprobe.so"
            ])
            .arg(work.join("stub.c"))
            .arg(&libc_so)
            .arg("-o")
            .arg(&musl_so)
            .status()
            .unwrap()
            .success());
        let excluded = prebuild_exclusions(&tree, "x86_64").unwrap();
        assert_eq!(excluded.len(), 3);
        // Exercise the flat linuxmusl naming and real ONNX shared-library
        // layout through readelf, RPM's generators, and the advisory scan.
        let sqlite = modules.join("better-sqlite3/prebuilds");
        fs::create_dir_all(&sqlite).unwrap();
        fs::copy(&native, sqlite.join("linux-x64.node")).unwrap();
        fs::copy(&musl, sqlite.join("linuxmusl-x64.node")).unwrap();
        let onnx = modules.join("onnxruntime-node/bin/napi-v3/linux/x64");
        fs::create_dir_all(&onnx).unwrap();
        let core = onnx.join("libonnxruntime.so.1");
        let compile = |output: &Path, soname: &str, source: &Path, libraries: &[&Path]| {
            assert!(Command::new("/usr/bin/gcc")
                .args(["-shared", "-fPIC", "-Wl,--no-as-needed"])
                .arg(format!("-Wl,-soname,{soname}"))
                .arg(source)
                .args(libraries)
                .arg("-o")
                .arg(output)
                .status()
                .unwrap()
                .success());
        };
        compile(&core, "libonnxruntime.so.1", &work.join("addon.c"), &[]);
        compile(
            &onnx.join("onnxruntime_binding.node"),
            "libonnxbindingprobe.so",
            &work.join("addon.c"),
            &[&core],
        );
        fs::copy(&native, onnx.join("libonnxruntime_providers_shared.so")).unwrap();
        let cuda_stub = work.join("libcudart.so.12");
        compile(&cuda_stub, "libcudart.so.12", &work.join("stub.c"), &[]);
        compile(
            &onnx.join("libonnxruntime_providers_cuda.so"),
            "libonnxruntime_providers_cuda.so",
            &work.join("addon.c"),
            &[&cuda_stub],
        );
        let excluded = prebuild_exclusions(&tree, "x86_64").unwrap();
        assert_eq!(excluded.len(), 5);
        let required = modules.parent().unwrap().join("required-tool.so");
        compile(
            &required,
            "librequiredprobe.so",
            &work.join("addon.c"),
            &[&onnx.join("libonnxruntime_providers_cuda.so")],
        );
        let linked = prebuild_exclusions(&tree, "x86_64").unwrap();
        assert_eq!(linked.len(), 4);
        assert!(!linked
            .iter()
            .any(|path| path.ends_with("libonnxruntime_providers_cuda.so")));
        fs::remove_file(required).unwrap();
        assert!(missing_libraries(&tree, &[])
            .missing
            .iter()
            .any(|(lib, _)| lib == "libcudart.so.12"));
        assert!(!missing_libraries(&tree, &excluded)
            .missing
            .iter()
            .any(|(lib, _)| lib == "libcudart.so.12"));
        assert!(missing_libraries(&tree, &[])
            .missing
            .iter()
            .any(|(lib, _)| lib == "libc.so"));
        assert!(!missing_libraries(&tree, &excluded)
            .missing
            .iter()
            .any(|(lib, _)| lib == "libc.so"));
        let spec = work.join("probe.spec");
        fs::write(&spec, "Name: mixedprobe\nVersion: 1\nRelease: 1\nSummary: mixed ELF test\nLicense: MIT\n%description\nTest\n%files\n%defattr(-,root,root,-)\n\"/opt/T3 Code (Beta)\"\n").unwrap();
        let rpm = rpmbuild(&tree, &spec, "x86_64", &work, &excluded).unwrap();
        let payload_flags = run(
            RPM,
            &["-qp", "--qf", "%{PAYLOADFLAGS}", rpm.to_str().unwrap()],
            None,
        )
        .unwrap();
        assert!(payload_flags.ok);
        assert!(
            matches!(payload_flags.stdout.trim(), "3T1" | "3T2"),
            "{}",
            payload_flags.stdout
        );
        let requirements = run(RPM, &["-qp", "--requires", rpm.to_str().unwrap()], None).unwrap();
        assert!(requirements.ok);
        assert!(requirements.stdout.contains("libc.so.6(GLIBC_"));
        assert!(!requirements.stdout.contains("libcudart.so.12"));
        assert!(
            !requirements.stdout.contains("libc.so()(64bit)"),
            "{}",
            requirements.stdout
        );
        assert!(
            !requirements.stdout.contains("musl"),
            "{}",
            requirements.stdout
        );
        let provides = run(RPM, &["-qp", "--provides", rpm.to_str().unwrap()], None).unwrap();
        assert!(provides.ok);
        assert!(provides.stdout.contains("libnativeprobe.so"));
        assert!(provides.stdout.contains("libonnxruntime.so.1"));
        assert!(!provides.stdout.contains("libonnxruntime_providers_cuda.so"));
        assert!(
            !provides.stdout.contains("libmuslsoprobe.so"),
            "{}",
            provides.stdout
        );
        assert!(
            !provides.stdout.contains("libforeignprob.so"),
            "{}",
            provides.stdout
        );
        assert!(
            !provides.stdout.contains("libmuslprobe.so"),
            "{}",
            provides.stdout
        );
        // RPM keeps the excluded files in the payload.
        let files = run(RPM, &["-qpl", rpm.to_str().unwrap()], None).unwrap();
        for path in excluded {
            assert!(files.stdout.contains(&path));
        }
        fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn ownership_query_errors_are_not_treated_as_unowned() {
        let paths = vec!["/tmp/unowned".into()];
        let unowned = Out {
            ok: false,
            code: Some(1),
            stdout: "file /tmp/unowned is not owned by any package\n".into(),
            stderr: String::new(),
        };
        assert_eq!(owner_packages(&paths, &unowned), Some(BTreeSet::new()));
        for (code, stdout, stderr) in [
            (1, "", "database error"),
            (1, "OWNER\tfilesystem\n", ""),
            (2, "file /tmp/unowned is not owned by any package\n", ""),
            (1, "unrecognized failure\n", ""),
        ] {
            let out = Out {
                ok: false,
                code: Some(code),
                stdout: stdout.into(),
                stderr: stderr.into(),
            };
            assert!(owner_packages(&paths, &out).is_none());
        }
    }

    #[test]
    fn indexed_ownership_matches_full_rpm_snapshot_including_multiple_owners() {
        let _guard = fork_lock_test_guard();
        let temp = temporary_dir("ownership");
        let unowned = temp.join("unowned");
        fs::write(&unowned, b"probe").unwrap();
        let paths = vec![
            "/usr".into(),
            "/usr/bin".into(),
            "/usr/lib".into(),
            "/bin".into(),
            "/usr/bin/sh".into(),
            "/usr/share/icons/hicolor".into(),
            unowned.to_str().unwrap().into(),
        ];
        let indexed = query_owners(&paths).expect("indexed RPM lookup must succeed");
        let all = Command::new(RPM)
            .args(["-qa", "--qf", "[%{FILENAMES}\t%{=NAME}\n]"])
            .output()
            .unwrap();
        assert!(all.status.success());
        let mut reference: BTreeMap<String, BTreeSet<String>> =
            paths.iter().map(|p| (p.clone(), BTreeSet::new())).collect();
        for line in String::from_utf8(all.stdout).unwrap().lines() {
            if let Some((path, name)) = line.rsplit_once('\t') {
                if let Some(owners) = reference.get_mut(path) {
                    owners.insert(name.into());
                }
            }
        }
        for path in paths {
            assert_eq!(
                indexed[&path].iter().cloned().collect::<BTreeSet<_>>(),
                reference[&path],
                "{path}"
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn bare_directories_are_filtered_without_losing_recursive_payload() {
        let _guard = fork_lock_test_guard();
        let tree = temporary_dir("bare-dirs");
        for dir in [
            "usr/bin",
            "usr/lib/Deb2rpmOwnershipFixture/lib",
            "usr/share/deb2rpm-ownership-fixture",
        ] {
            fs::create_dir_all(tree.join(dir)).unwrap();
        }
        fs::write(tree.join("usr/bin/deb2rpm-ownership-fixture"), b"app").unwrap();
        fs::write(
            tree.join("usr/lib/Deb2rpmOwnershipFixture/lib/data"),
            b"data",
        )
        .unwrap();
        fs::write(
            tree.join("usr/share/deb2rpm-ownership-fixture/50%data"),
            b"data",
        )
        .unwrap();
        std::os::unix::fs::symlink("Deb2rpmOwnershipFixture", tree.join("usr/lib/alias")).unwrap();
        let spec = "Name: demo\nVersion: 1\nRelease: 1\nSummary: demo\nLicense: MIT\nGroup: Applications\n%description\nDemo\n%files\n\"/usr\"\n%config \"/usr/share/deb2rpm-ownership-fixture/50%data\"\n";
        let fixed = fix_spec(spec, "demo", true, &tree).unwrap();
        for dir in ["/usr", "/usr/bin", "/usr/lib"] {
            assert!(fixed.dropped_dirs.contains(&dir.to_string()), "{dir}");
            assert!(!fixed.text.lines().any(|l| l == format!("%dir \"{dir}\"")));
        }
        assert!(fixed
            .text
            .contains("%dir \"/usr/lib/Deb2rpmOwnershipFixture/lib\""));
        for file in [
            "/usr/bin/deb2rpm-ownership-fixture",
            "/usr/lib/Deb2rpmOwnershipFixture/lib/data",
            "/usr/lib/alias",
        ] {
            assert!(
                fixed.text.lines().any(|l| l == format!("\"{file}\"")),
                "{file}"
            );
        }
        assert_eq!(
            fixed
                .text
                .matches("/usr/share/deb2rpm-ownership-fixture/50%%data")
                .count(),
            1
        );
        assert!(fixed
            .text
            .contains("%config \"/usr/share/deb2rpm-ownership-fixture/50%%data\""));
        assert!(!fixed.text.contains("/usr/lib/alias/lib/data"));
        let build_only = fix_spec(spec, "demo", false, &tree).unwrap();
        assert!(build_only.text.contains("%dir \"/usr/bin\""));
        assert!(build_only
            .text
            .contains("\"/usr/bin/deb2rpm-ownership-fixture\""));
        let work = temporary_dir("bare-dirs-rpm");
        let spec_path = work.join("fixture.spec");
        fs::write(&spec_path, &fixed.text).unwrap();
        let rpm = rpmbuild(&tree, &spec_path, &machine_arch(), &work, &[]).unwrap();
        let files = run(RPM, &["-qpl", rpm.to_str().unwrap()], None).unwrap();
        assert!(files.ok);
        for dir in ["/usr", "/usr/bin", "/usr/lib"] {
            assert!(!files.stdout.lines().any(|p| p == dir), "{dir}");
        }
        for file in [
            "/usr/bin/deb2rpm-ownership-fixture",
            "/usr/lib/Deb2rpmOwnershipFixture/lib/data",
            "/usr/lib/alias",
            "/usr/share/deb2rpm-ownership-fixture/50%data",
        ] {
            assert!(files.stdout.lines().any(|p| p == file), "{file}");
        }
        fs::remove_dir_all(tree).unwrap();
        fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn spec_keeps_automatic_dependencies() {
        let spec = "Name: demo\nVersion: 2\nRelease: 1\nSummary: demo\nLicense: MIT\nGroup: Applications\n%description\nDemo\n\n%files\n\"/opt/demo\"\n";
        assert!(!fix_spec(spec, "demo", false, Path::new("/nonexistent"))
            .unwrap()
            .text
            .contains("AutoReqProv: no"));
    }

    #[test]
    fn upgrade_scripts_preserve_versions() {
        let spec = "Name: demo\nVersion: 2\nRelease: 1\nSummary: demo\nLicense: MIT\nGroup: Applications\n%pre\n#!/bin/sh\ntrue\n%post\n#!/bin/sh\ntrue\n%preun\n#!/bin/sh\ntrue\n%postun\n#!/bin/sh\ntrue\n%description\nDemo\n\n%files\n";
        let fixed = fix_spec(spec, "demo", true, Path::new("/nonexistent"))
            .unwrap()
            .text;
        assert!(fixed.contains("set -- upgrade \"$DEB2RPM_OLD\""));
        assert!(fixed.contains("set -- configure \"$DEB2RPM_OLD\""));
        assert!(fixed.contains("set -- upgrade \"$DEB2RPM_NEW\""));
    }

    fn alienish_spec(description: &str, scripts: &[(&str, &str)]) -> String {
        // Layout mirrors alien 8.95's prep() output (Alien/Package/Rpm.pm).
        let mut spec = String::from(
            "Buildroot: /tmp/work\n\
             Name: demo\n\
             Version: 1\n\
             Release: 1\n\
             Summary: demo\n\
             License: MIT\n\
             Distribution: Debian\n\
             Group: Converted/utils\n\
             \n\
             %define _rpmdir ../\n\
             %define _rpmfilename %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm\n\
             %define _unpackaged_files_terminate_build 0\n\
             \n",
        );
        for (header, body) in scripts {
            spec.push_str(header);
            spec.push('\n');
            spec.push_str(body);
            if !body.is_empty() && !body.ends_with('\n') {
                spec.push('\n');
            }
        }
        spec.push_str("%description\n");
        spec.push_str(description);
        if !description.is_empty() && !description.ends_with('\n') {
            spec.push('\n');
        }
        spec.push_str("\n%files\n\"/opt/demo\"\n");
        spec
    }

    #[test]
    fn sanitize_spec_rejects_leading_percent_in_description() {
        for injected in [
            "%prep", "  %prep", "\t%build", "%PREP", "%files", "%install",
        ] {
            let spec = alienish_spec(&format!("{injected}\ntouch marker\n"), &[]);
            let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
            assert!(
                err.contains("%description") && err.contains('%'),
                "expected leading-% rejection for {injected:?}, got {err}"
            );
        }
    }

    #[test]
    fn sanitize_spec_rejects_leading_percent_in_each_scriptlet() {
        for hook in ["%pre", "%post", "%preun", "%postun"] {
            for injected in [
                "%prep", "  %prep", "\t%build", "%PREP", "%files", "%install",
            ] {
                // Alien doubles % in script bodies; both raw and doubled forms
                // begin with '%' after whitespace strip and must be rejected.
                let trimmed = injected.trim_start_matches([' ', '\t', '\r', '\u{0c}']);
                let pad = &injected[..injected.len() - trimmed.len()];
                let alien_doubled = format!("{pad}%{trimmed}");
                for body in [
                    format!("#!/bin/sh\n{injected}\ntouch marker\n"),
                    format!("#!/bin/sh\n{alien_doubled}\ntouch marker\n"),
                ] {
                    let spec = alienish_spec("Demo\n", &[(hook, &body)]);
                    let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
                    assert!(
                        err.contains(hook) && err.contains('%'),
                        "expected leading-% rejection in {hook} for {injected:?}, got {err}"
                    );
                }
            }
        }
    }

    #[test]
    fn sanitize_spec_accepts_mid_line_percent() {
        let spec = alienish_spec("100% done\na %% b\n", &[]);
        let clean = sanitize_spec(&spec, "demo").unwrap();
        assert!(clean.contains("100%% done"));
        assert!(clean.contains("a %%%% b"));
        let with_scripts = alienish_spec("Demo\n", &[("%pre", "#!/bin/sh\necho 100%% done\n")]);
        let clean = sanitize_spec(&with_scripts, "demo").unwrap();
        // Alien-style %% in the script is normalized then re-doubled to %%.
        assert!(clean.contains("100%% done"));
    }

    #[test]
    fn sanitize_spec_rejects_obscured_leading_percent_in_description() {
        for injected in [
            "\x0b%prep",
            "\r%prep",
            "\x0c%build",
            "\u{a0}%install",
            "  \t%files",
        ] {
            let spec = alienish_spec(&format!("{injected}\ntouch marker\n"), &[]);
            let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
            assert!(
                err.contains("%description"),
                "expected rejection for {injected:?}, got {err}"
            );
        }
    }

    #[test]
    fn sanitize_spec_rejects_percent_prep_in_pre() {
        let spec = alienish_spec("Demo\n", &[("%pre", "#!/bin/sh\n%prep\ntouch marker\n")]);
        let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
        assert!(err.contains("%pre") && err.contains('%'), "{err}");
    }

    #[test]
    fn sanitize_spec_rejects_percent_prep_in_post() {
        let spec = alienish_spec("Demo\n", &[("%post", "#!/bin/sh\n%prep\ntouch marker\n")]);
        let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
        assert!(err.contains("%post") && err.contains('%'), "{err}");
    }

    #[test]
    fn sanitize_spec_rejects_percent_prep_in_preun() {
        let spec = alienish_spec("Demo\n", &[("%preun", "#!/bin/sh\n%prep\ntouch marker\n")]);
        let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
        assert!(err.contains("%preun") && err.contains('%'), "{err}");
    }

    #[test]
    fn sanitize_spec_rejects_percent_prep_in_postun() {
        let spec = alienish_spec("Demo\n", &[("%postun", "#!/bin/sh\n%prep\ntouch marker\n")]);
        let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
        assert!(err.contains("%postun") && err.contains('%'), "{err}");
    }

    #[test]
    fn sanitize_spec_rejects_embedded_newline_in_version_release_and_files() {
        for key in ["Version", "Release"] {
            let base = alienish_spec("Demo\n", &[]);
            // Survives '\n'-only splitting as a single physical line with \r in the value.
            let with_cr = base.replacen(&format!("{key}: 1\n"), &format!("{key}: 1\r%prep\n"), 1);
            let err = sanitize_spec(&with_cr, "demo").unwrap_err().to_string();
            assert!(
                err.contains(key),
                "expected {key} rejection for embedded CR, got {err}"
            );
            // Embedded LF splits the value across lines; the "%prep" fragment is rejected.
            let with_lf = base.replacen(&format!("{key}: 1\n"), &format!("{key}: 1\n%prep\n"), 1);
            assert!(
                sanitize_spec(&with_lf, "demo").is_err(),
                "expected rejection for {key} value split by \\n%prep"
            );
        }
        let files_inject =
            alienish_spec("Demo\n", &[]).replacen("\"/opt/demo\"\n", "\"/opt/demo\n%prep\"\n", 1);
        let err = sanitize_spec(&files_inject, "demo")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("filename") || err.contains("file"),
            "expected %files path rejection for embedded \\n%prep, got {err}"
        );
    }

    #[test]
    fn sanitize_spec_rejects_section_injection_before_rpmbuild() {
        // Regression: a description line that is "%prep" must fail closed in
        // sanitize_spec so rpmbuild is never reached with an injectable spec.
        let spec = alienish_spec("%prep\ntouch /tmp/cursor-p1/marker\n", &[]);
        let err = sanitize_spec(&spec, "demo").unwrap_err().to_string();
        assert!(
            err.contains("%description"),
            "injection must be rejected by sanitize_spec: {err}"
        );
        assert!(
            err.contains("line"),
            "error must cite the offending line: {err}"
        );
    }

    #[test]
    #[ignore = "documents P1: doubling-only turns %%prep into %prep under rpmbuild -bp"]
    fn doubling_only_section_injection_executes_under_rpmbuild() {
        let _guard = fork_lock_test_guard();
        // Control: old neutralize-by-doubling behavior, without rejecting a
        // leading '%'. Package text "%prep" becomes "%%prep" in the spec; rpm
        // expands that back to "%prep" and rpmbuild -bp runs the following
        // shell. Containment: only touch under /tmp/cursor-p1.
        let root = Path::new("/tmp/cursor-p1");
        let topdir = root.join("rpmbuild");
        let marker = root.join("marker");
        let _ = fs::remove_file(&marker);
        for sub in ["SPECS", "SOURCES", "BUILD", "RPMS", "SRPMS"] {
            fs::create_dir_all(topdir.join(sub)).unwrap();
        }
        let buildroot = root.join("buildroot");
        fs::create_dir_all(&buildroot).unwrap();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let xdg = root.join("xdg");
        fs::create_dir_all(xdg.join("config")).unwrap();
        fs::create_dir_all(xdg.join("cache")).unwrap();
        fs::create_dir_all(xdg.join("data")).unwrap();
        fs::create_dir_all(xdg.join("state")).unwrap();
        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp).unwrap();

        // Old sanitize_spec behavior: double every % in the description body.
        let package_description = "%prep\ntouch /tmp/cursor-p1/marker\n";
        let doubled = package_description.replace('%', "%%");
        assert_eq!(doubled.lines().next().unwrap(), "%%prep");
        let spec = format!(
            "Buildroot: {}\nName: demo\nVersion: 1\nRelease: 1\nSummary: demo\nLicense: MIT\n\
             %description\n{doubled}\n%files\n",
            buildroot.display()
        );
        let spec_path = topdir.join("SPECS/demo.spec");
        fs::write(&spec_path, &spec).unwrap();
        let rpmbuild = Path::new(RPMBUILD);
        assert!(
            rpmbuild.exists(),
            "rpmbuild required for ignored P1 control"
        );
        let out = Command::new(rpmbuild)
            .args([
                "-bp",
                "--define",
                &format!("_topdir {}", topdir.display()),
                spec_path.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("TMPDIR", &tmp)
            .output()
            .unwrap();
        assert!(
            marker.exists(),
            "expected doubling-only bug to execute marker; status={} stdout={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn package_metadata_cannot_execute_rpm_macros() {
        let _guard = fork_lock_test_guard();
        // Layout mirrors alien 8.95's prep() output (Alien/Package/Rpm.pm): one
        // blank line before the %defines (which are already %%-quoted there),
        // License/Distribution/Group headers, a trailing "(Converted from a
        // … package by alien …)" line inside the description, and quoted
        // %files entries. Mid-line %(…) must be doubled; a leading '%' on a
        // body line is rejected separately (see sanitize_spec_rejects_*).
        let d = temporary_dir("spec-macro");
        let marker = d.join("executed");
        let buildroot = format!("{}/work", d.display());
        let attack = format!("%(touch {})", marker.display());
        let spec = format!(
            "Buildroot: {buildroot}\nName: demo\nVersion: 1\nRelease: 1\nSummary: probe {attack}\nLicense: see /usr/share/doc/demo/copyright\nDistribution: Debian\nGroup: Converted/utils\n\n%define _rpmdir ../\n%define _rpmfilename %%{{NAME}}-%%{{VERSION}}-%%{{RELEASE}}.%%{{ARCH}}.rpm\n%define _unpackaged_files_terminate_build 0\n\n%description\nprobe {attack}\n\n(Converted from a deb package by alien version 8.95.)\n\n%files\n%dir \"/opt/\"\n%dir \"/opt/demo/\"\n\"/opt/demo/plain.txt\"\n"
        );
        let fixed = fix_spec(&spec, "demo", false, Path::new("/nonexistent"))
            .unwrap()
            .text;
        assert!(fixed.contains(&format!("Summary: probe %%(touch {}", marker.display())));
        assert!(fixed.contains(&format!("%%(touch {}", marker.display())));

        let rpmspec = Path::new("/usr/bin/rpmspec");
        if rpmspec.exists() {
            let path = d.join("demo.spec");
            fs::write(&path, fixed).unwrap();
            let home = d.join("home");
            fs::create_dir_all(&home).unwrap();
            let parsed = Command::new(rpmspec)
                .args(["--parse", path.to_str().unwrap()])
                .env("HOME", &home)
                .env("TMPDIR", &d)
                .output()
                .unwrap();
            assert!(
                parsed.status.success(),
                "{}",
                String::from_utf8_lossy(&parsed.stderr)
            );
            assert!(!marker.exists(), "RPM macro executed from package metadata");
        }
        fs::remove_dir_all(d).unwrap();
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
        let _guard = fork_lock_test_guard();
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
        assert!(!missing_libraries(&d, &[])
            .missing
            .iter()
            .any(|(lib, _)| lib == "libauditprobe.so.1"));
        fs::remove_file(d.join("libauditprobe.so.1")).unwrap();
        fs::write(d.join("libauditprobe.so.1"), b"not an ELF library").unwrap();
        assert!(missing_libraries(&d, &[])
            .missing
            .iter()
            .any(|(lib, _)| lib == "libauditprobe.so.1"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn library_scan_rejects_fifo_without_blocking() {
        let _guard = fork_lock_test_guard();
        let d = temporary_dir("fifo");
        let fifo = d.join("library");
        assert!(Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert_eq!(elf_identity(&fifo), None);
        std::os::unix::fs::symlink(&fifo, d.join("library-link")).unwrap();
        assert_eq!(elf_identity(&d.join("library-link")), None);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn exited_child_does_not_wait_for_descendant_pipe() {
        let _guard = fork_lock_test_guard();
        let d = temporary_dir("descendant");
        let pid_file = d.join("descendant.pid");
        let pid_path = pid_file.to_str().unwrap();
        let start = Instant::now();
        let output = run(
            "/bin/sh",
            &[
                "-c",
                "sleep 10 & echo $! > \"$1\"; exit 0",
                "worker",
                pid_path,
            ],
            Some(&d),
        )
        .unwrap();
        assert!(output.ok);
        assert!(start.elapsed() < Duration::from_secs(3));
        let pid: u32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit_once(") ").map(|(_, rest)| rest.to_string()))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_owned));
            if state.as_deref().is_none_or(|state| state == "Z") {
                fs::remove_dir_all(d).unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("descendant process {pid} survived captured-command cleanup");
    }

    #[test]
    fn scriptlets_receive_versions_and_record_failures() {
        let _guard = fork_lock_test_guard();
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
        assert!(run("/bin/sh", &["-c", "sleep 10 & wait"], None).is_err());
        assert!(interrupted());
    }

    #[test]
    fn parent_signal_stops_child() {
        let _guard = fork_lock_test_guard();
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

    #[test]
    #[ignore = "PTY worker for terminal lifecycle regression checks"]
    fn terminal_lifecycle_worker() {
        install_signal_handlers();
        let scenario = env::var("DEB2RPM_TERMINAL_SCENARIO").unwrap();
        if scenario == "pause" || scenario == "pause-signal" {
            wait_for_enter().unwrap();
            assert_eq!(interrupted(), scenario == "pause-signal");
            return;
        }
        let log_path = env::var("DEB2RPM_TERMINAL_LOG").unwrap();
        LOG.lock().unwrap().file = Some(fs::File::create(log_path).unwrap());
        progress::start(false);
        step(Stage::Installing, "Installing probe");
        let script = if scenario == "interactive-signal" {
            "trap 'exit 143' TERM; printf 'READY_FOR_SIGNAL\\n'; while :; do sleep 0.05; done"
        } else if scenario == "descendant" {
            "(sleep 2) & printf 'background pipe holder\\n'; exit 0"
        } else if scenario == "native-tty" {
            "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || exit 90; exec 3<&1; [ \"$(stty size <&3)\" = '24 80' ] || exit 92; printf 'NATIVE_TTY_OK\\n'"
        } else if scenario == "resize-tty" {
            "exec 3<&1; printf 'READY_TO_RESIZE\\n'; IFS= read -r answer; size=$(stty size <&3); printf 'WINDOW_SIZE=%s\\n' \"$size\"; [ \"$size\" = '30 100' ]"
        } else if scenario == "raw-lines" {
            "saved=$(stty -g); trap 'stty \"$saved\"' EXIT; stty -opost; printf 'LAYOUT_FIRST\\nLAYOUT_SECOND\\n'; printf '\\033[A'; sleep 0.2"
        } else {
            "[ -t 0 ] || exit 90; printf 'PROBE_STDOUT\\n'; printf 'PROBE_STDERR\\n' >&2; printf 'Confirm probe: '; IFS= read -r answer; [ \"$answer\" = yes ] || exit 91; if [ \"$DEB2RPM_TERMINAL_SCENARIO\" = failure ]; then printf 'PROBE_FAILURE_DETAIL\\n' >&2; exit 7; fi"
        };
        let result = run_interactive("/bin/sh", &["-c", script]);
        if scenario == "interactive-signal" {
            assert!(interrupted());
            assert!(result.is_err());
            progress::finish(false, true);
        } else {
            let success = result.unwrap();
            assert_eq!(success, scenario != "failure");
            progress::finish(success, false);
        }
        out("PROBE_FINISHED\n");
    }

    #[test]
    #[ignore = "worker subprocess for pause_signal_stops_input_wait"]
    fn pause_signal_worker() {
        install_signal_handlers();
        fs::write(env::var_os("DEB2RPM_PAUSE_READY_FILE").unwrap(), b"ready").unwrap();
        wait_for_enter().unwrap();
        assert!(interrupted());
    }

    #[test]
    fn pause_signal_stops_input_wait() {
        let _guard = fork_lock_test_guard();
        // SIGTERM must interrupt the input wait. Outside interference has been
        // observed making a single kill delivery ineffective, so allow a few
        // attempts: a real regression fails all of them.
        let d = temporary_dir("pause");
        let ready_file = d.join("ready");
        let mut last = String::from("no attempt ran");
        for attempt in 0..4 {
            let mut child = Command::new(env::current_exe().unwrap())
                .args(["--ignored", "--exact", "tests::pause_signal_worker"])
                .env("DEB2RPM_PAUSE_READY_FILE", ready_file.to_str().unwrap())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let start = Instant::now();
            while !ready_file.exists() {
                assert!(start.elapsed() < Duration::from_secs(3));
                assert!(child.try_wait().unwrap().is_none());
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = fs::remove_file(&ready_file);
            unsafe {
                kill(child.id() as i32, 15);
            }
            let status = child.wait().unwrap();
            match (status.code(), interrupted_signal_of(&status)) {
                (Some(0), _) => {
                    fs::remove_dir_all(d).unwrap();
                    return;
                }
                _ => last = format!("attempt {attempt}: worker exited with {status:?}"),
            }
        }
        panic!("wait_for_enter did not stop on SIGTERM; {last}");
    }

    fn interrupted_signal_of(status: &std::process::ExitStatus) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }

    fn cursor_p1_env(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let home = root.join("home");
        let xdg = root.join("xdg");
        let tmp = root.join("tmp");
        for p in [
            &home,
            &tmp,
            &xdg.join("config"),
            &xdg.join("cache"),
            &xdg.join("data"),
            &xdg.join("state"),
            &root.join("rpmbuild"),
        ] {
            fs::create_dir_all(p).unwrap();
        }
        // Keep rpmbuild contained if it were ever reached.
        fs::write(
            home.join(".rpmmacros"),
            format!("%_topdir {}\n", root.join("rpmbuild").display()),
        )
        .unwrap();
        (home, xdg, tmp)
    }

    fn build_hostile_deb(
        staging: &Path,
        description_attack: bool,
        postinst_attack: bool,
    ) -> PathBuf {
        let root = staging.join("tree");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("DEBIAN")).unwrap();
        fs::create_dir_all(root.join("usr/share/doc/p1probe")).unwrap();
        fs::write(root.join("usr/share/doc/p1probe/README"), b"p1probe\n").unwrap();
        let description = if description_attack {
            "harmless summary\n %prep\n touch /tmp/cursor-p1/marker2\n"
        } else {
            "harmless summary\n"
        };
        fs::write(
            root.join("DEBIAN/control"),
            format!(
                "Package: p1probe\n\
                 Version: 1.0-1\n\
                 Section: utils\n\
                 Priority: optional\n\
                 Architecture: all\n\
                 Maintainer: P1 Probe <p1@example.invalid>\n\
                 Installed-Size: 1\n\
                 Description: {description}"
            ),
        )
        .unwrap();
        if postinst_attack {
            fs::write(
                root.join("DEBIAN/postinst"),
                "#!/bin/sh\n%prep\ntouch /tmp/cursor-p1/marker2\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(root.join("DEBIAN/postinst"))
                    .unwrap()
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(root.join("DEBIAN/postinst"), perms).unwrap();
            }
        }
        let deb = staging.join("p1probe_1.0-1_all.deb");
        let out = Command::new(DPKG_DEB)
            .args([
                "-b",
                "-Zgzip",
                root.to_str().unwrap(),
                deb.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "dpkg-deb failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        deb
    }

    fn deb2rpm_bin() -> PathBuf {
        // Unit-test exe is the harness; build/run the real binary instead.
        if let Some(p) = option_env!("CARGO_BIN_EXE_deb2rpm") {
            return PathBuf::from(p);
        }
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(&cargo)
            .args(["build", "-q", "--bin", "deb2rpm"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .unwrap();
        assert!(status.success(), "cargo build --bin deb2rpm failed");
        let target = env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        target.join("debug/deb2rpm")
    }

    fn run_deb2rpm_contained(deb: &Path, root: &Path) -> std::process::Output {
        let (home, xdg, tmp) = cursor_p1_env(root);
        let bin = deb2rpm_bin();
        assert!(bin.exists(), "missing deb2rpm binary at {}", bin.display());
        Command::new(&bin)
            .args(["-n", "--delete-work", deb.to_str().unwrap()])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("TMPDIR", &tmp)
            .output()
            .unwrap()
    }

    #[test]
    #[ignore = "P1 e2e: hostile Description .deb through real deb2rpm"]
    fn e2e_hostile_description_deb_rejected_before_rpmbuild() {
        let _guard = fork_lock_test_guard();
        let marker = Path::new("/tmp/cursor-p1/marker2");
        let _ = fs::remove_file(marker);
        // Per-test HOME/XDG root so parallel e2e cases do not share deb2rpm/lock.
        let root = Path::new("/tmp/cursor-p1/e2e-desc-root");
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let deb = build_hostile_deb(&staging, true, false);
        let out = run_deb2rpm_contained(&deb, root);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !out.status.success(),
            "deb2rpm should reject hostile description: {combined}"
        );
        assert!(
            combined.contains("%description") && combined.contains("begins with '%'"),
            "missing rejection message: {combined}"
        );
        assert!(
            !combined.contains("Building the rpm"),
            "rpmbuild step must not run: {combined}"
        );
        assert!(!marker.exists(), "marker2 must not exist");
    }

    #[test]
    #[ignore = "P1 e2e: hostile postinst .deb through real deb2rpm"]
    fn e2e_hostile_postinst_deb_rejected_before_rpmbuild() {
        let _guard = fork_lock_test_guard();
        let marker = Path::new("/tmp/cursor-p1/marker2");
        let _ = fs::remove_file(marker);
        let root = Path::new("/tmp/cursor-p1/e2e-postinst-root");
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let deb = build_hostile_deb(&staging, false, true);
        let out = run_deb2rpm_contained(&deb, root);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !out.status.success(),
            "deb2rpm should reject hostile postinst: {combined}"
        );
        assert!(
            combined.contains("%post") && combined.contains("begins with '%'"),
            "missing rejection message: {combined}"
        );
        assert!(
            !combined.contains("Building the rpm"),
            "rpmbuild step must not run: {combined}"
        );
        assert!(!marker.exists(), "marker2 must not exist");
    }
}
