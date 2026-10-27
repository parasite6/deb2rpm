//! Capture interactive output without changing the child's terminal contract.
//! Input stays attached to the original terminal; it is never relayed or logged.
use std::ffi::{c_char, CStr};
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::process::Stdio;

extern "C" {
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, name: *mut c_char, length: usize) -> i32;
    fn ioctl(fd: i32, request: usize, ...) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct WindowSize {
    rows: u16,
    cols: u16,
    x: u16,
    y: u16,
}

fn window_size() -> io::Result<WindowSize> {
    let mut size = WindowSize {
        rows: 0,
        cols: 0,
        x: 0,
        y: 0,
    };
    if unsafe { ioctl(io::stdout().as_raw_fd(), 0x5413, &mut size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if size.rows == 0 {
        size.rows = 24;
    }
    if size.cols == 0 {
        size.cols = 80;
    }
    Ok(size)
}

pub struct Pty {
    pub master: File,
    size: WindowSize,
}

impl Pty {
    pub fn refresh_size(&mut self) -> io::Result<()> {
        let size = window_size()?;
        if self.size != size {
            if unsafe { ioctl(self.master.as_raw_fd(), 0x5414, &size) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.size = size;
        }
        Ok(())
    }
}

pub enum Output {
    Terminal(Pty),
    Pipes(UnixStream, UnixStream),
}

pub fn prepare() -> io::Result<(Output, Stdio, Stdio)> {
    if !(io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()) {
        let (stdout, stdout_writer) = UnixStream::pair()?;
        let (stderr, stderr_writer) = UnixStream::pair()?;
        stdout.set_nonblocking(true)?;
        stderr.set_nonblocking(true)?;
        return Ok((
            Output::Pipes(stdout, stderr),
            Stdio::from(OwnedFd::from(stdout_writer)),
            Stdio::from(OwnedFd::from(stderr_writer)),
        ));
    }

    // O_NOCTTY prevents the output-only PTY from becoming our controlling
    // terminal. sudo/password input continues to use the original terminal.
    // File/OpenOptions also ensures CLOEXEC on every parent-owned descriptor.
    let master = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x100)
        .open("/dev/ptmx")?;
    if unsafe { grantpt(master.as_raw_fd()) } != 0 || unsafe { unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut name = [0 as c_char; 128];
    let result = unsafe { ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    let path = std::ffi::OsStr::from_bytes(unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes());
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x100)
        .open(path)?;
    let size = window_size()?;
    if unsafe { ioctl(master.as_raw_fd(), 0x5414, &size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    crate::nonblocking(master.as_raw_fd())?;
    let stderr = slave.try_clone()?;
    Ok((
        Output::Terminal(Pty { master, size }),
        Stdio::from(slave),
        Stdio::from(stderr),
    ))
}
