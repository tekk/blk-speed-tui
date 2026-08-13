//! Application state and input handling, kept free of rendering and terminal
//! concerns so it can be driven directly from tests.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;

use crate::bench::{self, Config, Msg, PassKind, PassResult, Run, Status};
use crate::device::Device;
use crate::theme::{Mode, Theme};

/// Points kept for the live throughput sparkline (~15s at one point per frame
/// pair, which is enough to show the shape of a pass without scrolling).
const SPARK_LEN: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Select,
    Running,
    Results,
}

#[derive(Debug, Clone)]
pub struct PassView {
    pub kind: PassKind,
    pub status: Status,
    pub fraction: f64,
    pub rate_now: f64,
    pub bytes: u64,
    pub result: Option<PassResult>,
}

impl PassView {
    fn new(kind: PassKind) -> Self {
        Self {
            kind,
            status: Status::Pending,
            fraction: 0.0,
            rate_now: 0.0,
            bytes: 0,
            result: None,
        }
    }
}

pub struct App {
    pub devices: Vec<Device>,
    pub list: ListState,
    pub screen: Screen,
    pub theme: Theme,
    pub direct: bool,
    pub budget: Duration,
    pub passes: Vec<PassKind>,
    pub views: Vec<PassView>,
    pub spark: VecDeque<u64>,
    pub phase: String,
    pub message: Option<String>,
    pub started: Option<Instant>,
    pub finished: Option<Duration>,
    pub should_quit: bool,
    run: Option<Run>,
}

impl App {
    pub fn new(devices: Vec<Device>, theme: Mode, direct: bool, budget: Duration) -> Self {
        let mut list = ListState::default();
        if !devices.is_empty() {
            list.select(Some(0));
        }
        Self {
            devices,
            list,
            screen: Screen::Select,
            theme: Theme::new(theme),
            direct,
            budget,
            passes: PassKind::ALL.to_vec(),
            views: Vec::new(),
            spark: VecDeque::new(),
            phase: String::new(),
            message: None,
            started: None,
            finished: None,
            should_quit: false,
            run: None,
        }
    }

    pub fn selected(&self) -> Option<&Device> {
        self.list.selected().and_then(|i| self.devices.get(i))
    }

    pub fn is_running(&self) -> bool {
        self.screen == Screen::Running
    }

    fn move_selection(&mut self, delta: isize) {
        if self.devices.is_empty() {
            return;
        }
        let len = self.devices.len() as isize;
        let cur = self.list.selected().unwrap_or(0) as isize;
        // Wrap around: with a handful of devices, wrapping is faster than
        // clamping at both ends.
        self.list.select(Some(((cur + delta + len) % len) as usize));
    }

    pub fn handle_key(&mut self, key: char) {
        match key {
            'q' => self.quit(),
            't' => self.theme = Theme::new(self.theme.mode.toggled()),
            'c' if !self.is_running() => {
                self.direct = !self.direct;
                self.message = Some(format!(
                    "cache bypass {}",
                    if self.direct { "on" } else { "off" }
                ));
            }
            'j' => self.on_down(),
            'k' => self.on_up(),
            'r' if self.screen == Screen::Results => self.start(),
            _ => {}
        }
    }

    pub fn on_up(&mut self) {
        if self.screen == Screen::Select {
            self.move_selection(-1);
        }
    }

    pub fn on_down(&mut self) {
        if self.screen == Screen::Select {
            self.move_selection(1);
        }
    }

    pub fn on_enter(&mut self) {
        match self.screen {
            Screen::Select => self.start(),
            Screen::Results => self.start(),
            Screen::Running => {}
        }
    }

    /// Escape backs out: it cancels a run, or returns from results to the
    /// picker. It never quits, so it cannot lose a finished run by accident.
    pub fn on_escape(&mut self) {
        match self.screen {
            Screen::Running => {
                self.cancel();
                self.screen = Screen::Results;
                self.message = Some("run cancelled".into());
            }
            Screen::Results => {
                self.screen = Screen::Select;
                self.message = None;
            }
            Screen::Select => self.quit(),
        }
    }

    pub fn quit(&mut self) {
        self.cancel();
        self.should_quit = true;
    }

    /// Stops the worker and waits for it, so the scratch file is removed before
    /// the process exits.
    pub fn cancel(&mut self) {
        if let Some(mut run) = self.run.take() {
            run.join();
        }
    }

    pub fn config(&self) -> Option<Config> {
        let device = self.selected()?;
        Some(Config {
            read_path: device.path.clone(),
            read_size: device.size,
            write_dir: device.write_dir().map(PathBuf::from),
            free_space: device.mount.as_ref().map(|m| m.free).unwrap_or(0),
            direct: self.direct,
            budget: self.budget,
            passes: self.passes.clone(),
        })
    }

    pub fn start(&mut self) {
        let Some(device) = self.selected() else {
            self.message = Some("no block devices found".into());
            return;
        };
        if let Some(reason) = device.blocker() {
            self.message = Some(reason);
            return;
        }
        let Some(config) = self.config() else { return };

        self.cancel();
        self.views = self.passes.iter().copied().map(PassView::new).collect();
        self.spark.clear();
        self.phase = "starting".into();
        self.message = None;
        self.finished = None;
        self.started = Some(Instant::now());
        self.screen = Screen::Running;
        self.run = Some(bench::spawn(config));
    }

    /// Drain worker messages. Called once per frame; never blocks.
    pub fn poll(&mut self) {
        let Some(run) = &self.run else { return };
        let messages: Vec<Msg> = run.rx.try_iter().collect();
        for msg in messages {
            self.apply(msg);
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Phase(p) => self.phase = p,
            Msg::PassStart { index } => {
                if let Some(v) = self.views.get_mut(index) {
                    v.status = Status::Running;
                }
                self.phase = self
                    .views
                    .get(index)
                    .map(|v| v.kind.label().to_string())
                    .unwrap_or_default();
            }
            Msg::Progress {
                index,
                fraction,
                bytes,
                rate_now,
            } => {
                if let Some(v) = self.views.get_mut(index) {
                    v.fraction = fraction;
                    v.bytes = bytes;
                    v.rate_now = rate_now;
                }
                self.push_spark(rate_now);
            }
            Msg::PassDone { index, result } => {
                if let Some(v) = self.views.get_mut(index) {
                    v.fraction = 1.0;
                    v.status = result.status.clone();
                    v.bytes = result.bytes;
                    v.result = Some(*result);
                }
            }
            Msg::Finished => {
                // Passes still pending when the worker stops were cut short by
                // a cancel; mark them so the results table is not misleading.
                for v in self.views.iter_mut() {
                    if matches!(v.status, Status::Pending | Status::Running) {
                        v.status = Status::Skipped("not run".into());
                    }
                }
                self.finished = self.started.map(|s| s.elapsed());
                self.phase = "done".into();
                self.screen = Screen::Results;
                self.run = None;
            }
            Msg::Fatal(e) => {
                self.message = Some(e);
                self.screen = Screen::Results;
            }
        }
    }

    fn push_spark(&mut self, rate: f64) {
        if self.spark.len() == SPARK_LEN {
            self.spark.pop_front();
        }
        // Scaled to kB/s: sparkline heights are relative, and this keeps the
        // values well inside u64 for any plausible device.
        self.spark.push_back((rate / 1000.0) as u64);
    }

    /// Overall progress across every pass, for the top-level gauge.
    pub fn overall(&self) -> f64 {
        if self.views.is_empty() {
            return 0.0;
        }
        let sum: f64 = self
            .views
            .iter()
            .map(|v| match v.status {
                Status::Pending => 0.0,
                Status::Running => v.fraction,
                _ => 1.0,
            })
            .sum();
        (sum / self.views.len() as f64).clamp(0.0, 1.0)
    }

    pub fn results(&self) -> Vec<&PassResult> {
        self.views
            .iter()
            .filter_map(|v| v.result.as_ref())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Kind, Mount};

    fn device(name: &str, writable: bool) -> Device {
        Device {
            name: name.into(),
            // A path every test machine can open read-only, so `blocker()`
            // reflects the writable flag rather than permissions.
            path: PathBuf::from("/dev/zero"),
            kind: Kind::Disk,
            size: 1 << 30,
            model: "test".into(),
            rotational: Some(false),
            removable: false,
            mount: writable.then(|| Mount {
                path: std::env::temp_dir(),
                fstype: "tmpfs".into(),
                writable: true,
                free: 100 << 30,
            }),
            mount_inherited: false,
        }
    }

    fn app(devices: Vec<Device>) -> App {
        App::new(devices, Mode::Dark, true, Duration::from_secs(40))
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut a = app(vec![device("a", false), device("b", false)]);
        assert_eq!(a.list.selected(), Some(0));
        a.on_up();
        assert_eq!(a.list.selected(), Some(1));
        a.on_down();
        assert_eq!(a.list.selected(), Some(0));
    }

    #[test]
    fn selection_keys_do_nothing_outside_the_picker() {
        let mut a = app(vec![device("a", false), device("b", false)]);
        a.screen = Screen::Running;
        a.on_down();
        assert_eq!(a.list.selected(), Some(0));
    }

    #[test]
    fn empty_device_list_reports_instead_of_panicking() {
        let mut a = app(Vec::new());
        a.on_enter();
        assert_eq!(a.screen, Screen::Select);
        assert!(a.message.is_some());
    }

    #[test]
    fn write_passes_are_configured_only_with_a_writable_mount() {
        let a = app(vec![device("a", false)]);
        assert!(a.config().unwrap().write_dir.is_none());

        let b = app(vec![device("b", true)]);
        assert!(b.config().unwrap().write_dir.is_some());
    }

    #[test]
    fn escape_backs_out_one_level_at_a_time() {
        let mut a = app(vec![device("a", false)]);
        a.screen = Screen::Results;
        a.on_escape();
        assert_eq!(a.screen, Screen::Select);
        assert!(!a.should_quit);
        a.on_escape();
        assert!(a.should_quit);
    }

    #[test]
    fn cache_bypass_toggles_only_when_idle() {
        let mut a = app(vec![device("a", false)]);
        a.handle_key('c');
        assert!(!a.direct);
        a.screen = Screen::Running;
        a.handle_key('c');
        assert!(!a.direct, "must not change mid-run");
    }

    #[test]
    fn theme_toggle_flips_the_mode() {
        let mut a = app(vec![device("a", false)]);
        assert_eq!(a.theme.mode, Mode::Dark);
        a.handle_key('t');
        assert_eq!(a.theme.mode, Mode::Light);
    }

    #[test]
    fn overall_progress_counts_finished_passes_as_whole() {
        let mut a = app(vec![device("a", false)]);
        a.views = PassKind::ALL.iter().copied().map(PassView::new).collect();
        assert_eq!(a.overall(), 0.0);

        a.views[0].status = Status::Done;
        a.views[1].status = Status::Running;
        a.views[1].fraction = 0.5;
        assert!((a.overall() - 0.375).abs() < 1e-9);
    }

    #[test]
    fn unfinished_passes_are_marked_when_a_run_stops_early() {
        let mut a = app(vec![device("a", false)]);
        a.views = PassKind::ALL.iter().copied().map(PassView::new).collect();
        a.views[0].status = Status::Done;
        a.started = Some(Instant::now());
        a.apply(Msg::Finished);

        assert_eq!(a.screen, Screen::Results);
        assert!(matches!(a.views[1].status, Status::Skipped(_)));
        assert!(a.finished.is_some());
    }

    #[test]
    fn sparkline_history_is_bounded() {
        let mut a = app(vec![device("a", false)]);
        for i in 0..(SPARK_LEN * 2) {
            a.push_spark(i as f64 * 1000.0);
        }
        assert_eq!(a.spark.len(), SPARK_LEN);
        // Oldest points are dropped, newest kept.
        assert_eq!(*a.spark.back().unwrap(), (SPARK_LEN * 2 - 1) as u64);
    }
}
