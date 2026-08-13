//! Rendering. Pure function of `App` state — one full frame is described per
//! call and ratatui diffs it against the previous frame, so only changed cells
//! are written to the terminal. That diffing, plus never clearing the screen
//! between frames, is what keeps the display free of flicker.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, List, ListItem, Padding, Paragraph, Row, Sparkline, Table, Wrap,
};
use ratatui::Frame;

use crate::app::{App, PassView, Screen};
use crate::bench::Status;
use crate::device::{Device, Kind};
use crate::format;
use crate::theme::Theme;

pub fn draw(f: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, header, app);
    match app.screen {
        Screen::Select => draw_select(f, body, app),
        Screen::Running => draw_running(f, body, app),
        Screen::Results => draw_results(f, body, app),
    }
    draw_footer(f, footer, app, &theme);
}

fn block(theme: &Theme, title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .padding(Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), theme.title()))
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let cache = if app.direct {
        Span::styled("direct (cache bypassed)", Style::default().fg(theme.good))
    } else {
        Span::styled("buffered (cached)", Style::default().fg(theme.warn))
    };

    let line = Line::from(vec![
        Span::styled("blkspeed", theme.title()),
        Span::styled("  non-destructive disk speed test", theme.dimmed()),
        Span::raw("   "),
        Span::styled("I/O: ", theme.dimmed()),
        cache,
        Span::styled("   budget: ", theme.dimmed()),
        Span::styled(format::secs(app.budget), theme.text()),
        Span::styled("   theme: ", theme.dimmed()),
        Span::styled(theme.mode.label(), theme.text()),
    ]);

    f.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

// ------------------------------------------------------------- selection ----

fn draw_select(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);

    if app.devices.is_empty() {
        let hint = concat!(
            "No block devices found.\n\n",
            "On Linux this reads /sys/block; on macOS it uses diskutil.\n",
            "You can also point the test at any path with --target <path>."
        );
        f.render_widget(
            Paragraph::new(hint)
                .style(theme.text())
                .wrap(Wrap { trim: false })
                .block(block(&theme, "Devices")),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app.devices.iter().map(|d| device_item(d, &theme)).collect();
    let list = List::new(items)
        .block(block(&theme, "Devices"))
        .highlight_style(theme.selected())
        .highlight_symbol("▌");
    f.render_stateful_widget(list, left, &mut app.list);

    draw_details(f, right, app);
}

fn device_item<'a>(d: &Device, theme: &Theme) -> ListItem<'a> {
    // Partitions are indented so the flat list still reads as a tree.
    let indent = if d.kind == Kind::Partition {
        "  └ "
    } else {
        ""
    };
    let mut spans = vec![
        Span::styled(format!("{indent}{:<12}", d.name), theme.text()),
        Span::styled(format!("{:>9}  ", format::bytes(d.size)), theme.text()),
        Span::styled(format!("{:<6}", d.media()), theme.dimmed()),
    ];
    if d.removable {
        spans.push(Span::styled("removable ", theme.dimmed()));
    }
    if let Some(m) = &d.mount {
        spans.push(Span::styled(
            format!("{} ", m.path.display()),
            Style::default().fg(theme.accent),
        ));
    }
    if !d.readable() {
        spans.push(Span::styled(
            "🔒 needs root",
            Style::default().fg(theme.bad),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn draw_details(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let Some(d) = app.selected() else { return };

    let mut lines = vec![
        field("Device", &d.path.display().to_string(), theme),
        field("Model", &d.model, theme),
        field("Capacity", &format::bytes(d.size), theme),
        field("Media", d.media(), theme),
    ];

    match &d.mount {
        Some(m) => {
            // A whole disk is rarely mounted itself; say plainly that the
            // filesystem belongs to one of its partitions.
            let label = if d.mount_inherited {
                "Filesystem"
            } else {
                "Mounted at"
            };
            lines.push(field(label, &m.path.display().to_string(), theme));
            lines.push(field("Filesystem", &m.fstype, theme));
            lines.push(field("Free space", &format::bytes(m.free), theme));
        }
        None => lines.push(field("Mounted at", "not mounted", theme)),
    }

    lines.push(Line::raw(""));
    // Being explicit about what the write test touches is the whole point of
    // the "non-destructive" claim, so it gets prominent space.
    match d.write_dir() {
        Some(dir) => {
            let size = crate::bench::scratch_size(d.mount.as_ref().map(|m| m.free).unwrap_or(0));
            lines.push(Line::from(Span::styled(
                "Write test: temporary file, removed afterwards",
                Style::default().fg(theme.good),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}/.blkspeed-scratch  ({})",
                    dir.display(),
                    format::bytes(size)
                ),
                theme.dimmed(),
            )));
        }
        None => lines.push(Line::from(Span::styled(
            "Write test: skipped — no writable filesystem here",
            Style::default().fg(theme.warn),
        ))),
    }
    lines.push(Line::from(Span::styled(
        "Read test: raw device, opened read-only",
        Style::default().fg(theme.good),
    )));

    if let Some(reason) = d.blocker() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("⚠ {reason}"),
            Style::default().fg(theme.bad),
        )));
    }
    if let Some(msg) = &app.message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            msg.clone(),
            Style::default().fg(theme.warn),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block(theme, "Details")),
        area,
    );
}

fn field<'a>(name: &str, value: &str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{name:<12}"), theme.dimmed()),
        Span::styled(value.to_string(), theme.text()),
    ])
}

// --------------------------------------------------------------- running ----

fn draw_running(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let [top, passes, spark] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(7),
    ])
    .areas(area);

    let elapsed = app.started.map(|s| s.elapsed()).unwrap_or_default();
    let overall = app.overall();
    f.render_widget(
        Gauge::default()
            .block(block(theme, "Overall"))
            .gauge_style(theme.gauge())
            .ratio(overall.clamp(0.0, 1.0))
            .label(format!(
                "{:.0}%  ·  {} elapsed  ·  {}",
                overall * 100.0,
                format::secs(elapsed),
                app.phase
            )),
        top,
    );

    let rows = Layout::vertical(
        app.views
            .iter()
            .map(|_| Constraint::Length(2))
            .collect::<Vec<_>>(),
    )
    .split(block(theme, "Passes").inner(passes));
    f.render_widget(block(theme, "Passes"), passes);
    for (view, &row) in app.views.iter().zip(rows.iter()) {
        draw_pass(f, row, view, theme);
    }

    let data: Vec<u64> = app.spark.iter().copied().collect();
    let peak = data.iter().copied().max().unwrap_or(0);
    f.render_widget(
        Sparkline::default()
            .block(block(
                theme,
                &format!("Throughput  (peak {})", format::rate(peak as f64 * 1000.0)),
            ))
            .data(data)
            .style(Style::default().fg(theme.accent)),
        spark,
    );
}

fn draw_pass(f: &mut Frame, area: Rect, view: &PassView, theme: &Theme) {
    let [label_area, gauge_area] =
        Layout::horizontal([Constraint::Length(24), Constraint::Min(10)]).areas(area);

    let (marker, style) = match &view.status {
        Status::Pending => ("○", theme.dimmed()),
        Status::Running => ("●", Style::default().fg(theme.accent)),
        Status::Done => ("✓", Style::default().fg(theme.good)),
        Status::Skipped(_) => ("–", theme.dimmed()),
        Status::Failed(_) => ("✗", Style::default().fg(theme.bad)),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::styled(view.kind.label().to_string(), theme.text()),
            Span::styled(
                format!(" {}", format::block_size(view.kind.block_size())),
                theme.dimmed(),
            ),
        ])),
        label_area,
    );

    // Finished, skipped and failed passes show their outcome in place of a
    // progress bar, so the screen never goes back to being ambiguous.
    let label = match &view.status {
        Status::Skipped(why) => return render_note(f, gauge_area, why, theme.dimmed()),
        Status::Failed(why) => {
            return render_note(f, gauge_area, why, Style::default().fg(theme.bad))
        }
        Status::Done => view
            .result
            .as_ref()
            .map(|r| format!("{}  ({})", format::rate(r.rate()), format::bytes(r.bytes)))
            .unwrap_or_default(),
        _ => format!(
            "{}  ({} so far)",
            format::rate(view.rate_now),
            format::bytes(view.bytes)
        ),
    };

    f.render_widget(
        Gauge::default()
            .gauge_style(theme.gauge())
            .ratio(view.fraction.clamp(0.0, 1.0))
            .label(label),
        gauge_area,
    );
}

fn render_note(f: &mut Frame, area: Rect, text: &str, style: Style) {
    f.render_widget(Paragraph::new(Span::styled(text.to_string(), style)), area);
}

// --------------------------------------------------------------- results ----

fn draw_results(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    // Grow the summary to fit however many skip/failure notes there are, so a
    // run where everything was skipped still explains itself.
    let notes = app
        .views
        .iter()
        .filter(|v| matches!(v.status, Status::Skipped(_) | Status::Failed(_)))
        .count() as u16;
    let summary_height = (5 + notes).min(area.height.saturating_sub(6).max(3));
    let [table_area, summary_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(summary_height)]).areas(area);

    let header = Row::new(
        [
            "Pass",
            "Block",
            "Speed",
            "IOPS",
            "Transferred",
            "Avg lat",
            "Max lat",
            "Cache",
        ]
        .into_iter()
        .map(|h| Cell::from(Span::styled(h, theme.dimmed()))),
    );

    let rows: Vec<Row> = app
        .views
        .iter()
        .map(|v| match (&v.status, &v.result) {
            (Status::Done, Some(r)) => Row::new(vec![
                Cell::from(Span::styled(
                    r.kind.label().to_string(),
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                )),
                Cell::from(format::block_size(r.block_size)),
                Cell::from(Span::styled(
                    format::rate(r.rate()),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(format::iops(r.iops())),
                Cell::from(format::bytes(r.bytes)),
                Cell::from(format::latency(r.avg_latency)),
                Cell::from(format::latency(r.max_latency)),
                Cell::from(if r.direct {
                    Span::styled("bypassed", Style::default().fg(theme.good))
                } else {
                    Span::styled("buffered", Style::default().fg(theme.warn))
                }),
            ]),
            (status, _) => {
                // Only a short marker fits the Speed column; the reason itself
                // goes in the summary below, where it has room to be read.
                let (word, style) = match status {
                    Status::Skipped(_) => ("skipped", theme.dimmed()),
                    Status::Failed(_) => ("failed", Style::default().fg(theme.bad)),
                    _ => ("not run", theme.dimmed()),
                };
                Row::new(vec![
                    Cell::from(v.kind.label().to_string()),
                    Cell::from(format::block_size(v.kind.block_size())),
                    Cell::from(Span::styled(word, style)),
                ])
            }
        })
        .collect();

    let widths = [
        Constraint::Length(18),
        Constraint::Length(6),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(9),
    ];
    f.render_widget(
        Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .style(theme.text())
            .block(block(theme, "Results")),
        table_area,
    );

    draw_summary(f, summary_area, app);
}

fn draw_summary(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let mut lines = Vec::new();

    if let Some(d) = app.selected() {
        lines.push(field(
            "Device",
            &format!("{} — {}", d.path.display(), d.model),
            theme,
        ));
    }
    if let Some(total) = app.finished {
        lines.push(field("Total time", &format::secs(total), theme));
    }

    let results = app.results();
    let best = |predicate: fn(&crate::bench::PassResult) -> bool| {
        results
            .iter()
            .filter(|r| predicate(r) && r.status == Status::Done)
            .map(|r| r.rate())
            .fold(0.0f64, f64::max)
    };
    let read = best(|r| !r.kind.is_write() && !r.kind.is_random());
    let write = best(|r| r.kind.is_write() && !r.kind.is_random());
    if read > 0.0 || write > 0.0 {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "Sequential"), theme.dimmed()),
            Span::styled("read ", theme.dimmed()),
            Span::styled(format::rate(read), Style::default().fg(theme.good)),
            Span::styled("   write ", theme.dimmed()),
            Span::styled(format::rate(write), Style::default().fg(theme.good)),
        ]));
    }

    // Full text for anything the table could only mark as skipped or failed.
    for v in &app.views {
        let (why, colour) = match &v.status {
            Status::Skipped(why) => (why, theme.dim),
            Status::Failed(why) => (why, theme.bad),
            _ => continue,
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{}: ", v.kind.label()), theme.dimmed()),
            Span::styled(why.clone(), Style::default().fg(colour)),
        ]));
    }

    // Buffered numbers can be an order of magnitude too high; say so rather
    // than letting someone quote a page-cache figure as a disk speed.
    if results
        .iter()
        .any(|r| !r.direct && r.status == Status::Done)
    {
        lines.push(Line::from(Span::styled(
            "⚠ some passes fell back to buffered I/O — those figures include the page cache",
            Style::default().fg(theme.warn),
        )));
    }
    if let Some(msg) = &app.message {
        lines.push(Line::from(Span::styled(
            msg.clone(),
            Style::default().fg(theme.warn),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block(theme, "Summary")),
        area,
    );
}

// ---------------------------------------------------------------- footer ----

fn draw_footer(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let keys: &[(&str, &str)] = match app.screen {
        Screen::Select => &[
            ("↑/↓", "select"),
            ("enter", "start"),
            ("c", "cache bypass"),
            ("t", "theme"),
            ("q", "quit"),
        ],
        Screen::Running => &[("esc", "cancel"), ("t", "theme"), ("q", "quit")],
        Screen::Results => &[
            ("r", "rerun"),
            ("esc", "back"),
            ("t", "theme"),
            ("q", "quit"),
        ],
    };

    let mut spans = Vec::new();
    for (key, desc) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!("{desc}   "), theme.dimmed()));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PassView;
    use crate::bench::{PassKind, PassResult};
    use crate::device::Mount;
    use crate::theme::Mode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use std::time::Duration;

    fn device(mounted: bool) -> Device {
        Device {
            name: "nvme0n1".into(),
            path: PathBuf::from("/dev/nvme0n1"),
            kind: Kind::Disk,
            size: 1 << 40,
            model: "Test SSD".into(),
            rotational: Some(false),
            removable: false,
            mount: mounted.then(|| Mount {
                path: PathBuf::from("/"),
                fstype: "ext4".into(),
                writable: true,
                free: 100 << 30,
            }),
            mount_inherited: mounted,
        }
    }

    fn app() -> App {
        let mut a = App::new(
            vec![device(true), device(false)],
            Mode::Dark,
            true,
            Duration::from_secs(40),
        );
        a.views = PassKind::ALL
            .iter()
            .copied()
            .map(|k| PassView {
                kind: k,
                status: Status::Pending,
                fraction: 0.0,
                rate_now: 0.0,
                bytes: 0,
                result: None,
            })
            .collect();
        a
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn selection_screen_shows_devices_and_the_write_target() {
        let mut a = app();
        let out = render(&mut a, 130, 40);
        assert!(out.contains("nvme0n1"));
        assert!(out.contains("Test SSD"));
        // The non-destructive contract must be visible, not just documented.
        assert!(out.contains("removed afterwards"));
        assert!(out.contains("read-only"));
    }

    #[test]
    fn unwritable_devices_say_the_write_test_is_skipped() {
        let mut a = app();
        a.list.select(Some(1));
        let out = render(&mut a, 130, 40);
        assert!(out.contains("no writable filesystem"));
    }

    #[test]
    fn running_screen_shows_progress_and_live_rates() {
        let mut a = app();
        a.screen = Screen::Running;
        a.started = Some(std::time::Instant::now());
        a.views[0].status = Status::Running;
        a.views[0].fraction = 0.5;
        a.views[0].rate_now = 1_500_000_000.0;
        a.spark.extend([1u64, 5, 3, 9]);

        let out = render(&mut a, 130, 40);
        assert!(out.contains("Sequential read"));
        assert!(out.contains("1.50 GB/s"));
    }

    #[test]
    fn results_screen_reports_rates_and_skips() {
        let mut a = app();
        a.screen = Screen::Results;
        a.finished = Some(Duration::from_secs(38));
        a.views[0].status = Status::Done;
        a.views[0].result = Some(PassResult {
            kind: PassKind::SeqRead,
            block_size: 4 << 20,
            bytes: 10 << 30,
            ops: 2560,
            elapsed: Duration::from_secs(10),
            avg_latency: Duration::from_micros(3900),
            max_latency: Duration::from_millis(12),
            direct: true,
            status: Status::Done,
        });
        a.views[2].status = Status::Skipped("read-only filesystem".into());

        let out = render(&mut a, 130, 40);
        assert!(
            out.contains("1.07 GB/s"),
            "expected the sequential read rate"
        );
        assert!(out.contains("bypassed"));
        assert!(out.contains("read-only filesystem"));
    }

    #[test]
    fn buffered_results_carry_a_warning() {
        let mut a = app();
        a.screen = Screen::Results;
        let mut r = PassResult {
            kind: PassKind::SeqRead,
            block_size: 4 << 20,
            bytes: 1 << 30,
            ops: 256,
            elapsed: Duration::from_secs(1),
            avg_latency: Duration::from_micros(100),
            max_latency: Duration::from_micros(900),
            direct: false,
            status: Status::Done,
        };
        a.views[0].status = Status::Done;
        a.views[0].result = Some(r.clone());
        r.direct = true;
        let out = render(&mut a, 130, 40);
        assert!(
            out.contains("page cache"),
            "must not pass off cached numbers"
        );
    }

    #[test]
    fn every_screen_survives_a_tiny_terminal() {
        // Layout maths and gauge ratios both panic on bad input, and a user
        // resizing the window must never take the process down mid-run.
        for screen in [Screen::Select, Screen::Running, Screen::Results] {
            for (w, h) in [(20, 5), (1, 1), (40, 8), (200, 60)] {
                let mut a = app();
                a.screen = screen;
                a.started = Some(std::time::Instant::now());
                render(&mut a, w, h);
            }
        }
    }

    #[test]
    fn an_empty_device_list_renders_guidance() {
        let mut a = App::new(Vec::new(), Mode::Light, true, Duration::from_secs(40));
        let out = render(&mut a, 100, 30);
        assert!(out.contains("No block devices found"));
        assert!(out.contains("--target"));
    }
}
