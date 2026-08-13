//! blkspeed — non-destructive block device read/write speed tester.
//!
//! Reads go straight to the raw device, opened read-only. Writes go to a
//! temporary file on a mounted filesystem and are cleaned up afterwards. The
//! device is never opened for writing, so no data can be overwritten.

mod aligned;
mod app;
mod bench;
mod device;
mod format;
mod theme;
mod ui;

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use bench::{Msg, PassKind, Status};

const DEFAULT_BUDGET_SECS: u64 = 40;
/// Frame interval. 20 fps is smooth for gauges while leaving the terminal
/// mostly idle, which matters on a single-core SBC.
const FRAME: Duration = Duration::from_millis(50);

const HELP: &str = "\
blkspeed — non-destructive block device speed test

USAGE:
    blkspeed [OPTIONS]

OPTIONS:
    -t, --target <PATH>    Test a specific device, directory or file instead of
                           picking one from the list
    -b, --budget <SECS>    Total wall-clock budget for all passes (default 40,
                           split evenly across the passes that can run)
    -p, --passes <LIST>    Comma-separated: seq_read,rand_read,seq_write,rand_write
        --no-direct        Allow the page cache (default: bypass it)
        --theme <MODE>     auto | dark | light (default auto)
    -l, --list             List detected devices and exit
    -j, --json             Run without the TUI and print JSON results
    -h, --help             Show this help
    -V, --version          Show version

SAFETY:
    The block device is only ever opened read-only. Write passes use a
    temporary file on a mounted filesystem, removed when the run ends.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match Options::parse(&args) {
        Ok(Some(o)) => o,
        Ok(None) => return,
        Err(e) => {
            eprintln!("blkspeed: {e}\n\nTry --help.");
            std::process::exit(2);
        }
    };

    let code = match run(opts) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("blkspeed: {e}");
            1
        }
    };
    std::process::exit(code);
}

struct Options {
    target: Option<PathBuf>,
    budget: Duration,
    passes: Vec<PassKind>,
    direct: bool,
    theme: theme::Mode,
    list: bool,
    json: bool,
}

impl Options {
    /// Returns `Ok(None)` when the flag was handled entirely here (`--help`).
    fn parse(args: &[String]) -> Result<Option<Self>, String> {
        let mut o = Options {
            target: None,
            budget: Duration::from_secs(DEFAULT_BUDGET_SECS),
            passes: PassKind::ALL.to_vec(),
            direct: true,
            theme: theme::detect(),
            list: false,
            json: false,
        };

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            // Accept both `--flag value` and `--flag=value`.
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) => (f, Some(v.to_string())),
                None => (arg, None),
            };
            let mut value = || -> Result<String, String> {
                if let Some(v) = inline.clone() {
                    return Ok(v);
                }
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };

            match flag {
                "-h" | "--help" => {
                    print!("{HELP}");
                    return Ok(None);
                }
                "-V" | "--version" => {
                    println!("blkspeed {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                "-t" | "--target" => o.target = Some(PathBuf::from(value()?)),
                "-b" | "--budget" => {
                    let secs: u64 = value()?
                        .parse()
                        .map_err(|_| "budget must be a whole number of seconds".to_string())?;
                    if !(5..=600).contains(&secs) {
                        return Err("budget must be between 5 and 600 seconds".into());
                    }
                    o.budget = Duration::from_secs(secs);
                }
                "-p" | "--passes" => o.passes = parse_passes(&value()?)?,
                "--no-direct" => o.direct = false,
                "--direct" => o.direct = true,
                "--theme" => {
                    let v = value()?;
                    o.theme = theme::parse(&v)
                        .ok_or_else(|| format!("unknown theme {v:?} (auto, dark or light)"))?;
                }
                "-l" | "--list" => o.list = true,
                "-j" | "--json" => o.json = true,
                other => return Err(format!("unknown option {other:?}")),
            }
            i += 1;
        }
        Ok(Some(o))
    }
}

fn parse_passes(s: &str) -> Result<Vec<PassKind>, String> {
    let mut out = Vec::new();
    for name in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let kind = PassKind::ALL
            .iter()
            .find(|k| k.short() == name)
            .ok_or_else(|| format!("unknown pass {name:?}"))?;
        if !out.contains(kind) {
            out.push(*kind);
        }
    }
    if out.is_empty() {
        return Err("no passes selected".into());
    }
    Ok(out)
}

fn run(opts: Options) -> io::Result<()> {
    let devices = match &opts.target {
        Some(path) => vec![device::from_path(path)?],
        None => device::enumerate(),
    };

    if opts.list {
        return print_list(&devices);
    }

    let mut app = App::new(devices, opts.theme, opts.direct, opts.budget);
    app.passes = opts.passes;
    // An explicit --target means the user already chose; don't make them press
    // enter on a one-item list.
    if (opts.json || opts.target.is_some()) && app.devices.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no block devices found (try --target <path>, or run with sudo)",
        ));
    }

    if opts.json {
        return run_headless(&mut app);
    }
    run_tui(app)
}

fn print_list(devices: &[device::Device]) -> io::Result<()> {
    if devices.is_empty() {
        println!("no block devices found");
        return Ok(());
    }
    println!(
        "{:<14} {:>10}  {:<6} {:<24} MOUNT",
        "DEVICE", "SIZE", "MEDIA", "MODEL"
    );
    for d in devices {
        println!(
            "{:<14} {:>10}  {:<6} {:<24} {}",
            d.name,
            format::bytes(d.size),
            d.media(),
            truncate(&d.model, 24),
            d.mount
                .as_ref()
                .map(|m| m.path.display().to_string())
                .unwrap_or_else(|| "—".into()),
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

// -------------------------------------------------------------- headless ----

/// Runs the same engine with no terminal control, for SSH sessions, cron jobs
/// and CI on headless SBCs.
fn run_headless(app: &mut App) -> io::Result<()> {
    let device = app
        .selected()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no device selected"))?;
    if let Some(reason) = device.blocker() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
    }
    let name = device.name.clone();
    let model = device.model.clone();
    let size = device.size;

    let config = app
        .config()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no device selected"))?;
    let direct = config.direct;
    let run = bench::spawn(config);

    let started = Instant::now();
    let mut results = Vec::new();
    // Blocking receive: without a UI there is nothing else to do, and it keeps
    // the process off the CPU while the device works.
    while let Ok(msg) = run.rx.recv() {
        match msg {
            Msg::PassDone { result, .. } => results.push(*result),
            Msg::Fatal(e) => return Err(io::Error::other(e)),
            Msg::Finished => break,
            _ => {}
        }
    }

    let mut out = io::stdout().lock();
    writeln!(out, "{{")?;
    writeln!(out, "  \"device\": {},", json_str(&name))?;
    writeln!(out, "  \"model\": {},", json_str(&model))?;
    writeln!(out, "  \"size_bytes\": {size},")?;
    writeln!(out, "  \"cache_bypass_requested\": {direct},")?;
    writeln!(
        out,
        "  \"total_seconds\": {:.3},",
        started.elapsed().as_secs_f64()
    )?;
    writeln!(out, "  \"passes\": [")?;
    for (i, r) in results.iter().enumerate() {
        let comma = if i + 1 == results.len() { "" } else { "," };
        match &r.status {
            Status::Done => writeln!(
                out,
                concat!(
                    "    {{\"pass\": {}, \"status\": \"ok\", \"block_bytes\": {}, ",
                    "\"bytes\": {}, \"seconds\": {:.3}, \"bytes_per_second\": {:.0}, ",
                    "\"human\": {}, \"iops\": {:.0}, \"avg_latency_us\": {:.1}, ",
                    "\"max_latency_us\": {:.1}, \"cache_bypassed\": {}}}{}"
                ),
                json_str(r.kind.short()),
                r.block_size,
                r.bytes,
                r.elapsed.as_secs_f64(),
                r.rate(),
                json_str(&format::rate(r.rate())),
                r.iops(),
                r.avg_latency.as_secs_f64() * 1e6,
                r.max_latency.as_secs_f64() * 1e6,
                r.direct,
                comma
            )?,
            other => {
                let (status, note) = match other {
                    Status::Skipped(w) => ("skipped", w.as_str()),
                    Status::Failed(w) => ("failed", w.as_str()),
                    _ => ("incomplete", ""),
                };
                writeln!(
                    out,
                    "    {{\"pass\": {}, \"status\": \"{status}\", \"note\": {}}}{comma}",
                    json_str(r.kind.short()),
                    json_str(note)
                )?
            }
        }
    }
    writeln!(out, "  ]")?;
    writeln!(out, "}}")?;
    Ok(())
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ------------------------------------------------------------------- tui ----

/// Restores the terminal on the way out, including on panic — otherwise a
/// crash would leave the user in raw mode with no echo.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            default_hook(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn run_tui(mut app: App) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    // The only full clear in the program. Every later frame is a diff against
    // the previous one, which is what keeps the display from flickering.
    terminal.clear()?;

    // An explicit target has exactly one candidate, so start measuring at once.
    if app.devices.len() == 1 && app.selected().is_some_and(|d| d.blocker().is_none()) {
        app.start();
    }

    while !app.should_quit {
        app.poll();
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Block until either input arrives or the next frame is due, so an idle
        // picker uses no CPU at all.
        if event::poll(FRAME)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(&mut app, key),
                // Redraw on resize happens naturally on the next loop.
                _ => {}
            }
        }
    }

    app.cancel();
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Ctrl-C must always quit, even though plain 'c' toggles cache bypass.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.quit();
        return;
    }
    match key.code {
        KeyCode::Up => app.on_up(),
        KeyCode::Down => app.on_down(),
        KeyCode::Enter => app.on_enter(),
        KeyCode::Esc => app.on_escape(),
        KeyCode::Char(c) => app.handle_key(c.to_ascii_lowercase()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Result<Option<Options>, String> {
        Options::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn defaults_bypass_the_cache_and_run_every_pass() {
        let o = opts(&[]).unwrap().unwrap();
        assert!(o.direct);
        assert_eq!(o.passes.len(), 4);
        assert_eq!(o.budget, Duration::from_secs(DEFAULT_BUDGET_SECS));
    }

    #[test]
    fn flags_accept_both_separated_and_inline_values() {
        let a = opts(&["--budget", "30"]).unwrap().unwrap();
        let b = opts(&["--budget=30"]).unwrap().unwrap();
        assert_eq!(a.budget, b.budget);
        assert_eq!(a.budget, Duration::from_secs(30));
    }

    #[test]
    fn budget_is_bounded_to_something_sane() {
        assert!(opts(&["--budget", "1"]).is_err());
        assert!(opts(&["--budget", "9999"]).is_err());
        assert!(opts(&["--budget", "abc"]).is_err());
    }

    #[test]
    fn pass_lists_are_parsed_and_deduplicated() {
        let o = opts(&["--passes", "seq_read,seq_read,rand_write"])
            .unwrap()
            .unwrap();
        assert_eq!(o.passes, vec![PassKind::SeqRead, PassKind::RandWrite]);
        assert!(opts(&["--passes", "nope"]).is_err());
        assert!(opts(&["--passes", ""]).is_err());
    }

    #[test]
    fn unknown_options_and_missing_values_are_rejected() {
        assert!(opts(&["--wat"]).is_err());
        assert!(opts(&["--budget"]).is_err());
        assert!(opts(&["--theme", "purple"]).is_err());
    }

    #[test]
    fn help_and_version_stop_before_running() {
        assert!(opts(&["--help"]).unwrap().is_none());
        assert!(opts(&["--version"]).unwrap().is_none());
    }

    #[test]
    fn json_strings_escape_control_characters() {
        assert_eq!(json_str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_str("line\nbreak"), r#""line\nbreak""#);
        assert_eq!(json_str("\u{1}"), r#""\u0001""#);
    }

    #[test]
    fn model_names_are_truncated_with_an_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }
}
