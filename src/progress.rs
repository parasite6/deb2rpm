use std::env;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
pub enum Stage {
    Reading,
    Unpacking,
    Repacking,
    Fixing,
    Building,
    Checking,
    Installing,
    Done,
}

impl Stage {
    fn number(self, total: usize) -> usize {
        match self {
            Self::Reading => 1,
            Self::Unpacking | Self::Repacking => 2,
            Self::Fixing => 3,
            Self::Building => 4,
            Self::Checking => 5,
            Self::Installing => 6,
            Self::Done => total,
        }
    }
}

// Deep cobalt, teal, indigo and ocean blue. The moving highlight only dims
// these colors; it never introduces white, yellow, or pastel highlights.
fn gradient(position: usize, seconds: f64, stage: usize) -> [u8; 3] {
    const PALETTE: [[f64; 3]; 4] = [
        [44.0, 91.0, 160.0],
        [25.0, 124.0, 130.0],
        [89.0, 64.0, 151.0],
        [35.0, 108.0, 143.0],
    ];
    let phase = (seconds * 1.6 + position as f64 * 0.09 + stage as f64 * 0.35) % 4.0;
    let index = phase.floor() as usize;
    let blend = phase.fract();
    let shimmer = 0.84 + 0.16 * (position as f64 * 0.55 - seconds * 6.0).sin().powi(2);
    std::array::from_fn(|channel| {
        ((PALETTE[index][channel] * (1.0 - blend) + PALETTE[(index + 1) % 4][channel] * blend)
            * shimmer)
            .round() as u8
    })
}

fn ink(rgb: [u8; 3], text: &str, color: bool) -> String {
    if color {
        format!("\x1b[38;2;{};{};{}m{text}", rgb[0], rgb[1], rgb[2])
    } else {
        text.into()
    }
}

struct Display {
    enabled: bool,
    color: bool,
    running: bool,
    total: usize,
    stage: Stage,
    label: String,
    visible: bool,
    summary: Vec<String>,
    started: Instant,
    animation: Instant,
    drawn: Instant,
    failed: bool,
    interactive: bool,
    interactive_fill: f64,
    fill_from: f64,
    fill_target: f64,
    fill_started: f64,
}

impl Display {
    fn enter_interactive(&mut self) -> &'static str {
        if !self.enabled || !self.running || self.interactive {
            return "";
        }
        self.interactive = true;
        self.interactive_fill = self.fill_at(self.animation.elapsed().as_secs_f64());
        self.visible = false;
        "\x1b[?1049h\x1b[H"
    }
    fn leave_interactive(&mut self, width: usize) -> String {
        if !self.interactive {
            return String::new();
        }
        self.interactive = false;
        self.fill_from = self.interactive_fill;
        self.fill_started = self.animation.elapsed().as_secs_f64();
        format!("\x1b[?1049l{}", self.resume(width))
    }
    fn fill_at(&self, seconds: f64) -> f64 {
        let t = ((seconds - self.fill_started) / 0.4).clamp(0.0, 1.0);
        let eased = t * t * (3.0 - 2.0 * t);
        self.fill_from + (self.fill_target - self.fill_from) * eased
    }

    fn step_at(&mut self, stage: Stage, msg: &str, seconds: f64) {
        self.fill_from = self.fill_at(seconds);
        self.fill_target = stage.number(self.total) as f64 / self.total as f64;
        self.fill_started = seconds;
        self.stage = stage;
        self.label = msg.into();
    }

    fn new(enabled: bool, color: bool, build_only: bool) -> Self {
        Self {
            enabled,
            color,
            running: true,
            total: if build_only { 6 } else { 7 },
            stage: Stage::Reading,
            label: String::new(),
            visible: false,
            summary: Vec::new(),
            started: Instant::now(),
            animation: Instant::now(),
            drawn: Instant::now(),
            failed: false,
            interactive: false,
            interactive_fill: 0.0,
            fill_from: 0.0,
            fill_target: 0.0,
            fill_started: 0.0,
        }
    }

    fn render(&self, width: usize) -> String {
        self.render_frame(width, self.animation.elapsed().as_secs_f64())
    }

    fn render_frame(&self, width: usize, seconds: f64) -> String {
        // ASCII labels give predictable cell widths and neutralize terminal controls
        // from package metadata. Keep one cell free to avoid terminal autowrap.
        let label: String = self
            .label
            .chars()
            .map(|c| {
                if c.is_ascii() && !c.is_control() {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        let budget = width.saturating_sub(1);
        let n = self.stage.number(self.total);
        let elapsed = self.started.elapsed().as_secs();
        let suffix = if elapsed > 0 && self.running {
            format!(" ({elapsed}s)")
        } else {
            String::new()
        };
        if budget < 24 {
            return label.chars().take(budget).collect();
        }
        let size = if budget >= 75 {
            28
        } else if budget >= 60 {
            20
        } else if budget >= 44 {
            14
        } else {
            8
        };
        let icon = if self.failed {
            "×"
        } else if !self.running {
            "✓"
        } else {
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(seconds * 12.0) as usize % 10]
        };
        let status_color = if self.failed {
            [155, 66, 79]
        } else if !self.running {
            [38, 126, 109]
        } else {
            gradient(0, seconds, 0)
        };
        let mut row = ink(status_color, icon, self.color);
        row.push_str(&ink([52, 66, 84], "  ▕", self.color));
        let eighths = (size as f64 * 8.0 * self.fill_at(seconds)).round() as usize;
        for position in 0..size {
            let filled = eighths.saturating_sub(position * 8).min(8);
            let cell = ["━", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"][filled];
            let rgb = if filled == 0 {
                [35, 46, 62]
            } else if self.failed || !self.running {
                status_color
            } else {
                gradient(position, seconds, 0)
            };
            row.push_str(&ink(rgb, cell, self.color));
        }
        row.push_str(&ink([52, 66, 84], "▏  ", self.color));
        row.push_str(&ink(
            [91, 108, 132],
            &format!("{n:02}/{:02}  ", self.total),
            self.color,
        ));
        let available = budget.saturating_sub(size + 14);
        let text: String = format!("{label}{suffix}").chars().take(available).collect();
        row.push_str(&ink([151, 163, 183], &text, self.color));
        if self.color {
            row.push_str("\x1b[0m");
        }
        row
    }

    fn step(&mut self, stage: Stage, msg: &str, width: usize) -> String {
        self.step_at(stage, msg, self.animation.elapsed().as_secs_f64());
        self.started = Instant::now();
        self.resume(width)
    }

    fn suspend(&mut self) -> &'static str {
        if self.visible {
            self.visible = false;
            "\r\x1b[2K"
        } else {
            ""
        }
    }

    fn resume(&mut self, width: usize) -> String {
        if !self.enabled || !self.running || self.interactive || self.label.is_empty() {
            return String::new();
        }
        self.visible = true;
        self.drawn = Instant::now();
        format!("\r\x1b[2K{}", self.render(width))
    }

    fn finish(&mut self, success: bool, stopped: bool, width: usize) -> String {
        if !self.enabled || !self.running {
            return String::new();
        }
        if success {
            if self.stage != Stage::Done {
                self.label = "Done - RPM ready".into();
            }
            self.stage = Stage::Done;
            self.fill_from = 1.0;
            self.fill_target = 1.0;
        } else {
            let current = self.fill_at(self.animation.elapsed().as_secs_f64());
            self.fill_from = current;
            self.fill_target = current;
            self.failed = true;
            self.label = if stopped { "Stopped" } else { "Failed" }.into();
        }
        self.running = false;
        self.visible = false;
        let mut text = format!("\r\x1b[2K{}\n", self.render(width));
        for line in &self.summary {
            text.push_str(&format!("    {line}\n"));
        }
        text
    }

    fn info(&mut self, msg: &str) -> bool {
        if !self.enabled || !self.running {
            return false;
        }
        if msg.starts_with("rpm saved: ")
            || msg.starts_with("work dir kept: ")
            || msg.starts_with("work dir and full log kept for inspection: ")
        {
            self.summary.push(msg.into());
        }
        true
    }
}

static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

pub struct InteractiveScreen {
    active: bool,
}

impl InteractiveScreen {
    pub fn enter() -> Self {
        let mut active = false;
        if let Ok(mut display) = DISPLAY.lock() {
            if let Some(display) = display.as_mut() {
                let sequence = display.enter_interactive();
                active = !sequence.is_empty();
                write(sequence);
            }
        }
        Self { active }
    }
}

impl Drop for InteractiveScreen {
    fn drop(&mut self) {
        if self.active {
            if let Ok(mut display) = DISPLAY.lock() {
                if let Some(display) = display.as_mut() {
                    write(&display.leave_interactive(columns()));
                }
            }
        }
    }
}

fn write(text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn columns() -> usize {
    #[repr(C)]
    struct WindowSize {
        rows: u16,
        cols: u16,
        x: u16,
        y: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, request: usize, ...) -> i32;
    }
    let mut size = WindowSize {
        rows: 0,
        cols: 0,
        x: 0,
        y: 0,
    };
    // Linux TIOCGWINSZ, read afresh so resizing cannot wrap the progress row.
    if unsafe { ioctl(io::stdout().as_raw_fd(), 0x5413, &mut size) } == 0 && size.cols > 0 {
        return usize::from(size.cols);
    }
    env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(80)
}

pub fn start(build_only: bool) {
    let enabled = io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && env::var("TERM").as_deref() != Ok("dumb");
    if let Ok(mut display) = DISPLAY.lock() {
        *display = Some(Display::new(
            enabled,
            env::var_os("NO_COLOR").is_none(),
            build_only,
        ));
    }
}

pub fn step(stage: Stage, msg: &str) -> bool {
    if let Ok(mut display) = DISPLAY.lock() {
        if let Some(display) = display.as_mut() {
            write(&display.step(stage, msg, columns()));
            return display.enabled;
        }
    }
    false
}

pub fn info(msg: &str) -> bool {
    DISPLAY
        .lock()
        .ok()
        .and_then(|mut d| d.as_mut().map(|d| d.info(msg)))
        .unwrap_or(false)
}

pub fn suspend() {
    if let Ok(mut display) = DISPLAY.lock() {
        if let Some(display) = display.as_mut() {
            write(display.suspend());
        }
    }
}

pub fn resume() {
    if let Ok(mut display) = DISPLAY.lock() {
        if let Some(display) = display.as_mut() {
            write(&display.resume(columns()));
        }
    }
}

pub fn tick() {
    if let Ok(mut display) = DISPLAY.lock() {
        if let Some(display) = display.as_mut() {
            if display.visible && display.drawn.elapsed() >= Duration::from_millis(30) {
                write(&display.resume(columns()));
            }
        }
    }
}

pub fn finish(success: bool, stopped: bool) {
    if success {
        if let Ok(mut display) = DISPLAY.lock() {
            if let Some(display) = display.as_mut() {
                if display.enabled && display.running && display.stage != Stage::Done {
                    write(&display.step(Stage::Done, "Done - RPM ready", columns()));
                }
            }
        }
        // Allow the final leap to reach 100% smoothly before freezing the row.
        // No background renderer runs while sudo or DNF owns the terminal.
        loop {
            let pending = if let Ok(mut display) = DISPLAY.lock() {
                if let Some(display) = display.as_mut() {
                    if display.enabled
                        && display.running
                        && display.fill_at(display.animation.elapsed().as_secs_f64()) < 1.0
                    {
                        write(&display.resume(columns()));
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if !pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }
    if let Ok(mut display) = DISPLAY.lock() {
        if let Some(display) = display.as_mut() {
            write(&display.finish(success, stopped, columns()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_output_uses_a_separate_screen_and_restores_the_original_row() {
        let mut display = Display::new(true, false, false);
        display.step(Stage::Installing, "Installing", 80);
        assert_eq!(display.enter_interactive(), "\x1b[?1049h\x1b[H");
        assert_eq!(display.resume(80), "");
        let restored = display.leave_interactive(80);
        assert!(restored.starts_with("\x1b[?1049l\r\x1b[2K"));
        assert!(restored.contains("Installing"));
    }

    #[test]
    fn stage_fill_eases_instead_of_jumping_to_the_target() {
        let mut display = Display::new(true, false, true);
        display.step_at(Stage::Reading, "Reading", 0.0);
        assert_eq!(display.fill_at(0.0), 0.0);
        assert!(display.fill_at(0.2) > 0.0 && display.fill_at(0.2) < 1.0 / 6.0);
        assert!((display.fill_at(0.5) - 1.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn fast_stages_retarget_from_the_current_visible_fill() {
        let mut display = Display::new(true, false, true);
        display.step_at(Stage::Reading, "Reading", 0.0);
        let before = display.fill_at(0.1);
        display.step_at(Stage::Building, "Building", 0.1);
        assert!((display.fill_at(0.1) - before).abs() < 1e-6);
        let middle = display.fill_at(0.2);
        assert!(middle > before && middle < 4.0 / 6.0);
        display.step_at(Stage::Checking, "Checking", 0.2);
        assert!((display.fill_at(0.2) - middle).abs() < 1e-6);
        assert!((display.fill_at(0.7) - 5.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn bar_uses_animated_truecolor_gradient_and_solid_cells() {
        let mut display = Display::new(true, true, true);
        display.step(Stage::Building, "Building the rpm", 80);
        let first = display.render_frame(80, 0.5);
        let next = display.render_frame(80, 0.7);
        assert!(first.matches("38;2;").count() > 10);
        assert!(first.contains('█') && first.contains('━'));
        assert_ne!(first, next, "animation must advance within the same stage");
        assert!(!first.contains("38;5;214") && !first.contains("38;5;141"));
        assert!(first.contains("Building the rpm"));
    }

    #[test]
    fn animated_palette_stays_in_deep_blue_teal_and_indigo() {
        for frame in 0..100 {
            for position in 0..28 {
                let [red, green, blue] = gradient(position, frame as f64 / 30.0, 4);
                assert!(red <= 89 && green <= 124 && blue <= 160);
                assert!(blue > red && blue >= green);
            }
        }
    }

    #[test]
    fn stages_replace_one_row_and_change_color() {
        let mut display = Display::new(true, true, true);
        let reading = display.step(Stage::Reading, "Reading the .deb", 80);
        let building = display.step(Stage::Building, "Building the rpm", 80);
        assert!(reading.starts_with("\r\x1b[2K"));
        assert!(building.starts_with("\r\x1b[2K"));
        assert!(!reading.contains('\n') && !building.contains('\n'));
        assert!(reading.contains("01/06") && building.contains("04/06"));
        assert!(reading.contains("38;2;") && building.contains("38;2;"));
        assert!(building.contains("Building the rpm"));
        assert!(!building.contains("Reading the .deb"));
    }

    #[test]
    fn warnings_and_interactive_commands_can_suspend_and_resume() {
        let mut display = Display::new(true, false, false);
        display.step(Stage::Installing, "Installing", 80);
        assert_eq!(display.suspend(), "\r\x1b[2K");
        assert_eq!(display.suspend(), "");
        assert!(display.resume(80).contains("06/07"));
        let done = display.finish(true, false, 80);
        assert!(done.contains("07/07") && done.ends_with('\n'));
        assert_eq!(display.resume(80), "");
    }

    #[test]
    fn narrow_rows_never_wrap_and_labels_cannot_inject_controls() {
        for width in 1..100 {
            let mut display = Display::new(true, false, true);
            let row = display.step(
                Stage::Building,
                "Building\n\x1b[31m a very long RPM label",
                width,
            );
            let visible = row.strip_prefix("\r\x1b[2K").unwrap();
            assert!(visible.chars().count() < width.max(2), "{width}: {visible}");
            assert!(!visible.chars().any(char::is_control));
        }
    }

    #[test]
    fn redirected_output_keeps_plain_steps_and_detailed_messages() {
        let mut display = Display::new(false, false, true);
        assert_eq!(display.step(Stage::Reading, "Reading the .deb", 80), "");
        assert!(!display.info("prepared package in 2s"));
        assert_eq!(display.finish(true, false, 80), "");
    }

    #[test]
    fn routine_details_are_hidden_but_output_path_survives_completion() {
        let mut display = Display::new(true, false, true);
        display.step(Stage::Building, "Building the rpm", 80);
        assert!(display.info("not claiming shared directory /usr"));
        assert!(display.info("rpm saved: /tmp/app.rpm"));
        let done = display.finish(true, false, 80);
        assert!(done.contains("06/06") && done.contains("rpm saved: /tmp/app.rpm"));
        assert!(!done.contains("not claiming"));
    }

    #[test]
    fn failures_do_not_fill_the_bar_or_claim_completion() {
        let mut display = Display::new(true, true, true);
        display.step(Stage::Building, "Building the rpm", 80);
        let failed = display.finish(false, false, 80);
        assert!(failed.contains("Failed") && failed.contains("04/06"));
        assert!(failed.contains("38;2;155;66;79"));
        assert!(!failed.contains("Done"));
    }

    #[test]
    fn failure_log_location_is_deferred_until_after_the_fixed_progress_row() {
        let mut display = Display::new(true, false, false);
        display.step(Stage::Installing, "Installing", 80);
        assert!(display.info("work dir and full log kept for inspection: /tmp/probe.log"));
        assert!(display.visible);
        let output = display.finish(false, false, 80);
        assert!(output.find("Failed").unwrap() < output.find("/tmp/probe.log").unwrap());
    }
}
