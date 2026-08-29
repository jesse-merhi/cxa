use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anstream::print;
use anstyle::{AnsiColor, Style};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::account_store::{UsageBucket, UsageRecord, UsageWindow};

pub const ACCENT: Style = AnsiColor::Cyan.on_default().bold();
pub const SUCCESS: Style = AnsiColor::Green.on_default().bold();
pub const WARNING: Style = AnsiColor::Yellow.on_default().bold();
pub const ERROR: Style = AnsiColor::Red.on_default().bold();
pub const EMPHASIS: Style = Style::new().bold();
pub const MUTED: Style = Style::new().dimmed();

const BAR_WIDTH: usize = 16;
const WATCH_SIGNALS: [libc::c_int; 3] = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM];
static WATCH_EXIT_SIGNAL: AtomicBool = AtomicBool::new(false);

pub fn usage_plan(usage: Option<&UsageRecord>) -> Option<String> {
    let usage = usage?;
    let first = usage
        .buckets
        .iter()
        .find_map(|bucket| bucket.plan_type.as_deref())?;
    if usage
        .buckets
        .iter()
        .filter_map(|bucket| bucket.plan_type.as_deref())
        .all(|plan| plan == first)
    {
        Some(plan_label(first))
    } else {
        None
    }
}

pub fn usage_recency(usage: Option<&UsageRecord>, now: i64) -> Option<String> {
    let usage = usage?;
    if usage.error.is_some() || usage.buckets.is_empty() {
        Some(format!(
            "checked {}",
            age_label(usage.last_attempted_at.max(usage.observed_at), now)
        ))
    } else {
        Some(format!("updated {}", age_label(usage.observed_at, now)))
    }
}

pub fn print_usage(usage: Option<&UsageRecord>, now: i64) {
    print!("{}", render_usage(usage, now));
}

pub fn render_usage(usage: Option<&UsageRecord>, now: i64) -> String {
    let mut output = String::new();
    let Some(usage) = usage else {
        writeln!(output, "    {WARNING}Usage unknown{WARNING:#}").unwrap();
        return output;
    };
    if let Some(error) = &usage.error {
        writeln!(output, "    {WARNING}{error}{WARNING:#}").unwrap();
        return output;
    }
    if usage.buckets.is_empty() {
        writeln!(output, "    {WARNING}No quota data{WARNING:#}").unwrap();
        return output;
    }

    for bucket in &usage.buckets {
        write_bucket(&mut output, bucket, now);
    }
    output
}

pub struct FetchSpinner {
    stop: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
}

pub struct LiveRegion {
    origin_saved: bool,
    reserved_rows: usize,
    full_screen: bool,
    active: bool,
}

impl Default for LiveRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveRegion {
    pub fn new() -> Self {
        Self {
            origin_saved: false,
            reserved_rows: 0,
            full_screen: false,
            active: io::stdout().is_terminal(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn width(&self) -> usize {
        match crossterm::terminal::size() {
            Ok((columns, _)) if columns > 0 => usize::from(columns),
            _ => 80,
        }
    }

    pub fn redraw(&mut self, output: &str) -> io::Result<()> {
        if !self.active {
            let mut stdout = anstream::stdout();
            stdout.write_all(output.as_bytes())?;
            return stdout.flush();
        }
        let width = self.width();
        let rows = rendered_rows(output, width);
        let height = match crossterm::terminal::size() {
            Ok((_, rows)) if rows > 0 => usize::from(rows),
            _ => 24,
        };
        let mut control = io::stdout().lock();
        if self.full_screen {
            write!(control, "\x1b[H\x1b[2J")?;
        } else if rows.saturating_add(1) >= height {
            if self.origin_saved {
                write!(control, "\x1b[u\x1b[J")?;
                self.origin_saved = false;
            }
            self.full_screen = true;
            write!(control, "\x1b[H\x1b[2J")?;
        } else if !self.origin_saved || rows > self.reserved_rows {
            if self.origin_saved {
                write!(control, "\x1b[u\x1b[J")?;
            }
            for _ in 0..rows {
                writeln!(control)?;
            }
            write!(control, "\x1b[{rows}A\x1b[s")?;
            self.origin_saved = true;
            self.reserved_rows = rows;
        } else {
            write!(control, "\x1b[u\x1b[J")?;
        }
        control.flush()?;
        drop(control);
        let mut stdout = anstream::stdout();
        stdout.write_all(output.as_bytes())?;
        stdout.flush()
    }

    pub fn write_status(&self, status: &str) -> io::Result<()> {
        let mut control = io::stdout().lock();
        write!(control, "\r\x1b[2K")?;
        control.flush()?;
        drop(control);
        let mut stdout = anstream::stdout();
        write!(stdout, "{status}")?;
        stdout.flush()
    }
}

pub struct WatchTerminal {
    active: bool,
    _signals: WatchSignals,
}

impl WatchTerminal {
    pub fn enter() -> io::Result<Self> {
        let signals = WatchSignals::install()?;
        crossterm::terminal::enable_raw_mode()?;
        if let Err(error) = enable_output_processing() {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error);
        }
        let mut stdout = io::stdout().lock();
        if let Err(error) = write!(stdout, "\x1b[?25l").and_then(|()| stdout.flush()) {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self {
            active: true,
            _signals: signals,
        })
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\r\x1b[2K\x1b[?25h\n");
        let _ = stdout.flush();
    }
}

impl Drop for WatchTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn watch_exit_requested(timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    let mut first_poll = true;
    loop {
        if WATCH_EXIT_SIGNAL.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !first_poll && remaining.is_zero() {
            return Ok(false);
        }
        first_poll = false;
        match event::poll(remaining) {
            Ok(false) => return Ok(WATCH_EXIT_SIGNAL.load(Ordering::Relaxed)),
            Ok(true) => {}
            Err(_error) if WATCH_EXIT_SIGNAL.load(Ordering::Relaxed) => return Ok(true),
            Err(error) => return Err(error),
        }
        let event = match event::read() {
            Ok(event) => event,
            Err(_error) if WATCH_EXIT_SIGNAL.load(Ordering::Relaxed) => return Ok(true),
            Err(error) => return Err(error),
        };
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Release && watch_exit_key(key.code, key.modifiers) {
            return Ok(true);
        }
    }
}

struct WatchSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl WatchSignals {
    fn install() -> io::Result<Self> {
        WATCH_EXIT_SIGNAL.store(false, Ordering::Relaxed);
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = request_watch_exit as usize;
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut installed = Self {
            previous: Vec::new(),
        };
        for signal in WATCH_SIGNALS {
            let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
            if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            installed.previous.push((signal, previous));
        }
        Ok(installed)
    }
}

impl Drop for WatchSignals {
    fn drop(&mut self) {
        for (signal, action) in self.previous.iter().rev() {
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
        WATCH_EXIT_SIGNAL.store(false, Ordering::Relaxed);
    }
}

extern "C" fn request_watch_exit(_signal: libc::c_int) {
    WATCH_EXIT_SIGNAL.store(true, Ordering::Relaxed);
}

fn watch_exit_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q' | 'Q'))
        || code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL)
}

fn enable_output_processing() -> io::Result<()> {
    let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, mode.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut mode = unsafe { mode.assume_init() };
    mode.c_oflag |= libc::OPOST | libc::ONLCR;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl FetchSpinner {
    pub fn start(message: String) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: None,
                worker: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0;
            let mut stderr = anstream::stderr();
            while !worker_stop.load(Ordering::Relaxed) {
                let _ = write!(
                    stderr,
                    "\r\x1b[2K{ACCENT}{}{ACCENT:#} {message}",
                    frames[frame % frames.len()]
                );
                let _ = stderr.flush();
                frame += 1;
                thread::sleep(Duration::from_millis(80));
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    pub fn finish(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
            let mut stderr = anstream::stderr();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
        }
    }
}

impl Drop for FetchSpinner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_bucket(output: &mut String, bucket: &UsageBucket, now: i64) {
    let name = bucket_name(bucket);
    if bucket.exhausted_now(now) {
        writeln!(output, "    {ERROR}{name}  EXHAUSTED{ERROR:#}").unwrap();
    } else {
        writeln!(output, "    {EMPHASIS}{name}{EMPHASIS:#}").unwrap();
    }
    for (fallback, window) in bucket.windows() {
        write_window(output, fallback, window, now);
    }
}

fn write_window(output: &mut String, fallback: &str, window: &UsageWindow, now: i64) {
    let label = window_label(fallback, window.window_minutes);
    let percent = window.used_percent.map(format_percent);
    let bar = progress_bar(window.used_percent);
    let style = usage_style(window.used_percent);
    let percent = percent.unwrap_or_else(|| "--".into());
    let reset = window
        .resets_at
        .map(|reset| format!("  {}", reset_label(reset, now)))
        .unwrap_or_default();
    writeln!(
        output,
        "      {MUTED}{label:<8}{MUTED:#} {style}{bar}{style:#} {style}{percent:>3}% used{style:#}{MUTED}{reset}{MUTED:#}"
    )
    .unwrap();
}

fn rendered_rows(output: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows: usize = 0;
    let mut columns: usize = 0;
    let mut escape = false;
    for character in output.chars() {
        if escape {
            if character == 'm' {
                escape = false;
            }
            continue;
        }
        match character {
            '\x1b' => escape = true,
            '\n' => {
                rows += columns.max(1).div_ceil(width);
                columns = 0;
            }
            '\r' => columns = 0,
            _ => columns += 1,
        }
    }
    if columns > 0 {
        rows += columns.div_ceil(width);
    }
    rows.max(1)
}

fn bucket_name(bucket: &UsageBucket) -> String {
    match bucket.limit_name.as_deref() {
        Some("GPT-5.3-Codex-Spark") => "Codex Spark".into(),
        Some(name) => name.replace('-', " "),
        None if bucket.limit_id == "codex" => "Codex".into(),
        None => bucket.limit_id.replace('_', " "),
    }
}

fn window_label(fallback: &str, minutes: Option<i64>) -> String {
    match minutes {
        Some(300) => "5-hour".into(),
        Some(1_440) => "Daily".into(),
        Some(10_080) => "Weekly".into(),
        Some(minutes) if minutes > 0 && minutes % 1_440 == 0 => {
            format!("{}-day", minutes / 1_440)
        }
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => {
            format!("{}-hour", minutes / 60)
        }
        _ => title_case(fallback),
    }
}

fn progress_bar(percent: Option<f64>) -> String {
    let percent = percent.unwrap_or_default().clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * BAR_WIDTH as f64).round() as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

fn usage_style(percent: Option<f64>) -> Style {
    match percent {
        Some(percent) if percent >= 100.0 => ERROR,
        Some(percent) if percent >= 80.0 => WARNING,
        Some(_) => SUCCESS,
        None => MUTED,
    }
}

fn reset_label(reset: i64, now: i64) -> String {
    if reset <= now {
        return "reset pending".into();
    }
    let remaining = reset - now;
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {minutes}m")
    } else {
        format!("resets in {}m", minutes.max(1))
    }
}

fn age_label(timestamp: i64, now: i64) -> String {
    let age = now.saturating_sub(timestamp);
    if age < 60 {
        "just now".into()
    } else if age < 3_600 {
        format!("{}m ago", age / 60)
    } else if age < 86_400 {
        format!("{}h ago", age / 3_600)
    } else {
        format!("{}d ago", age / 86_400)
    }
}

fn format_percent(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn plan_label(plan: &str) -> String {
    match plan {
        "pro" => "Pro 20x".into(),
        "prolite" => "Pro 5x".into(),
        "self_serve_business_prolite" => "Business Pro Lite".into(),
        "self_serve_business_usage_based" | "business" | "team" => "Business".into(),
        "edu_plus" => "Edu Plus".into(),
        "edu_pro" => "Edu Pro".into(),
        _ => title_case(plan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_clamps_and_fills_to_percentage() {
        assert_eq!(progress_bar(Some(0.0)), "[░░░░░░░░░░░░░░░░]");
        assert_eq!(progress_bar(Some(43.0)), "[███████░░░░░░░░░]");
        assert_eq!(progress_bar(Some(100.0)), "[████████████████]");
        assert_eq!(progress_bar(Some(120.0)), "[████████████████]");
    }

    #[test]
    fn window_duration_has_a_human_label() {
        assert_eq!(window_label("primary", Some(300)), "5-hour");
        assert_eq!(window_label("secondary", Some(10_080)), "Weekly");
        assert_eq!(window_label("primary", None), "Primary");
    }

    #[test]
    fn reset_time_is_compact() {
        assert_eq!(
            reset_label(1_000 + 4 * 3_600 + 59 * 60, 1_000),
            "resets in 4h 59m"
        );
        assert_eq!(
            reset_label(1_000 + 6 * 86_400 + 11 * 3_600, 1_000),
            "resets in 6d 11h"
        );
        assert_eq!(reset_label(900, 1_000), "reset pending");
    }

    #[test]
    fn age_is_readable_without_timestamps() {
        assert_eq!(age_label(1_000, 1_030), "just now");
        assert_eq!(age_label(1_000, 1_300), "5m ago");
        assert_eq!(age_label(1_000, 8_200), "2h ago");
        assert_eq!(age_label(1_000, 260_200), "3d ago");
    }

    #[test]
    fn pro_variants_show_the_selected_multiplier_mapping() {
        assert_eq!(plan_label("pro"), "Pro 20x");
        assert_eq!(plan_label("prolite"), "Pro 5x");
    }

    #[test]
    fn watch_exit_keys_are_q_and_control_c() {
        assert!(watch_exit_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(watch_exit_key(KeyCode::Char('Q'), KeyModifiers::SHIFT));
        assert!(watch_exit_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!watch_exit_key(KeyCode::Char('c'), KeyModifiers::NONE));
    }

    #[test]
    fn rendered_row_count_ignores_styles_and_includes_wrapping() {
        assert_eq!(rendered_rows("12345\n", 5), 1);
        assert_eq!(rendered_rows("123456\n", 5), 2);
        assert_eq!(
            rendered_rows(&format!("{SUCCESS}123456{SUCCESS:#}\n"), 5),
            2
        );
        assert_eq!(rendered_rows("\n", 5), 1);
    }
}
