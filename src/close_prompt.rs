use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::process::{Command, Stdio};

#[derive(Default)]
pub struct Input {
    paste: bool,
    escape: Vec<u8>,
}
impl Input {
    pub fn feed(&mut self, byte: u8) -> bool {
        const START: &[u8] = b"\x1b[200~";
        const END: &[u8] = b"\x1b[201~";
        if !self.escape.is_empty() || byte == 27 {
            self.escape.push(byte);
            if self.escape == START {
                self.paste = true;
                self.escape.clear();
            } else if self.escape == END {
                self.paste = false;
                self.escape.clear();
            } else if !START.starts_with(&self.escape) && !END.starts_with(&self.escape) {
                self.escape.clear();
            }
            return false;
        }
        !self.paste && (byte == b'\n' || byte == b'\r')
    }
}

extern "C" {
    fn tcflush(fd: i32, queue: i32) -> i32;
}

/// Save/restore the exact terminal settings without assuming a platform-specific
/// termios struct layout. Keep ISIG enabled so Ctrl+C/TERM still stop the wait.
pub struct Terminal {
    input: OwnedFd,
    saved: String,
    bracketed: bool,
}

fn settings(input: &OwnedFd, args: &[&str]) -> io::Result<String> {
    let output = Command::new("/usr/bin/stty")
        .args(args)
        .stdin(Stdio::from(input.try_clone()?))
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl Terminal {
    pub fn new() -> io::Result<Option<Self>> {
        if !io::stdin().is_terminal() {
            return Ok(None);
        }
        let input = io::stdin().as_fd().try_clone_to_owned()?;
        let saved = settings(&input, &["-g"])?;
        if saved.is_empty() {
            return Err(io::Error::other("could not save terminal settings"));
        }
        let mut terminal = Self {
            input,
            saved,
            bracketed: false,
        };
        // TCSAFLUSH is not needed: stty provides portable attribute handling,
        // and tcflush discards both typeahead and text following the closing Enter.
        if unsafe { tcflush(terminal.input.as_raw_fd(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        settings(
            &terminal.input,
            &[
                "-echo", "-echonl", "-icanon", "isig", "-ixon", "susp", "undef", "quit", "undef",
                "min", "1", "time", "0",
            ],
        )?;
        if io::stdout().is_terminal() {
            terminal.bracketed = true;
            let _ = io::stdout().write_all(b"\x1b[?2004h");
            let _ = io::stdout().flush();
        }
        Ok(Some(terminal))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            tcflush(self.input.as_raw_fd(), 0);
        }
        if self.bracketed {
            let _ = io::stdout().write_all(b"\x1b[?2004l");
            let _ = io::stdout().flush();
        }
        if settings(&self.input, &[&self.saved]).is_err() {
            let _ = io::stderr().write_all(b"warning: could not restore terminal input settings\n");
        }
        unsafe {
            tcflush(self.input.as_raw_fd(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_commands_and_newlines_cannot_close_the_prompt() {
        let mut input = Input::default();
        for byte in b"\x1b[200~echo unexpected\nmore commands\r\n\x1b[201~" {
            assert!(!input.feed(*byte));
        }
        assert!(input.feed(b'\r'));
    }

    #[test]
    fn typing_and_terminal_escape_sequences_are_discarded() {
        let mut input = Input::default();
        for byte in b"ls -la\x1b[A\x1b[B\x1b[3~" {
            assert!(!input.feed(*byte));
        }
        assert!(input.feed(b'\n'));
    }
}
