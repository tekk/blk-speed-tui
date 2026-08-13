//! The measurement engine.
//!
//! Everything here runs on a dedicated worker thread and reports back over a
//! channel, so the UI thread never blocks on I/O and the frame rate stays
//! steady no matter how slow the device is.
//!
//! Non-destructive by construction: the block device is only ever opened
//! read-only, and every write goes to a scratch file on a mounted filesystem
//! which is removed when the run ends — including when the user quits early or
//! the run fails.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
#[cfg(not(target_os = "linux"))]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::aligned::AlignedBuf;

pub const SEQ_BLOCK: usize = 4 << 20; // 4 MiB — enough to saturate NVMe
pub const RAND_BLOCK: usize = 4 << 10; // 4 KiB — the classic IOPS block size

/// Upper bound on the scratch file. Large enough that a fast drive cannot serve
/// the whole thing from its SLC cache in one pass, small enough to create in a
/// couple of seconds on a slow SD card.
const SCRATCH_MAX: u64 = 1 << 30;
const SCRATCH_MIN: u64 = 64 << 20;
/// Never consume more than this fraction of the free space on the target.
const SCRATCH_FREE_FRACTION: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PassKind {
    SeqRead,
    RandRead,
    SeqWrite,
    RandWrite,
}

impl PassKind {
    pub const ALL: [PassKind; 4] = [
        PassKind::SeqRead,
        PassKind::RandRead,
        PassKind::SeqWrite,
        PassKind::RandWrite,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PassKind::SeqRead => "Sequential read",
            PassKind::RandRead => "Random read",
            PassKind::SeqWrite => "Sequential write",
            PassKind::RandWrite => "Random write",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            PassKind::SeqRead => "seq_read",
            PassKind::RandRead => "rand_read",
            PassKind::SeqWrite => "seq_write",
            PassKind::RandWrite => "rand_write",
        }
    }

    pub fn is_write(self) -> bool {
        matches!(self, PassKind::SeqWrite | PassKind::RandWrite)
    }

    pub fn is_random(self) -> bool {
        matches!(self, PassKind::RandRead | PassKind::RandWrite)
    }

    pub fn block_size(self) -> usize {
        if self.is_random() {
            RAND_BLOCK
        } else {
            SEQ_BLOCK
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Block device (or file) to read from. Opened read-only, always.
    pub read_path: PathBuf,
    pub read_size: u64,
    /// Directory for the scratch file. `None` disables the write passes.
    pub write_dir: Option<PathBuf>,
    pub free_space: u64,
    /// Bypass the page cache (`O_DIRECT` / `F_NOCACHE`).
    pub direct: bool,
    /// Wall-clock budget for the whole run, split across the enabled passes.
    pub budget: Duration,
    pub passes: Vec<PassKind>,
}

impl Config {
    /// A directory target (`--target /mnt/sd`) measures the filesystem rather
    /// than a raw device, so the read passes have to read the scratch file —
    /// you cannot `read()` a directory.
    pub fn read_via_scratch(&self) -> bool {
        self.read_path.is_dir()
    }

    fn has(&self, want_write: bool) -> bool {
        self.passes.iter().any(|k| k.is_write() == want_write)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Pending,
    Running,
    Done,
    Skipped(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct PassResult {
    pub kind: PassKind,
    pub block_size: usize,
    pub bytes: u64,
    pub ops: u64,
    pub elapsed: Duration,
    pub avg_latency: Duration,
    pub max_latency: Duration,
    /// Whether cache bypass was actually in effect. Filesystems that reject
    /// `O_DIRECT` fall back to buffered I/O, and the result must say so.
    pub direct: bool,
    pub status: Status,
}

impl PassResult {
    fn empty(kind: PassKind, status: Status) -> Self {
        Self {
            kind,
            block_size: kind.block_size(),
            bytes: 0,
            ops: 0,
            elapsed: Duration::ZERO,
            avg_latency: Duration::ZERO,
            max_latency: Duration::ZERO,
            direct: false,
            status,
        }
    }

    pub fn rate(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / s
        }
    }

    pub fn iops(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s <= 0.0 {
            0.0
        } else {
            self.ops as f64 / s
        }
    }
}

/// Progress and lifecycle events from the worker thread.
#[derive(Debug, Clone)]
pub enum Msg {
    Phase(String),
    PassStart {
        index: usize,
    },
    Progress {
        index: usize,
        fraction: f64,
        bytes: u64,
        /// Throughput over the last sample window, for the live gauge.
        rate_now: f64,
    },
    PassDone {
        index: usize,
        result: Box<PassResult>,
    },
    Finished,
    Fatal(String),
}

/// Handle to a running benchmark. Dropping it asks the worker to stop.
pub struct Run {
    pub rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Run {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Ask the worker to stop and wait for it, so the scratch file is gone
    /// before we return.
    pub fn join(&mut self) {
        self.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.join();
    }
}

/// Split the budget evenly across the passes that will actually run.
pub fn per_pass_duration(config: &Config) -> Duration {
    let runnable = config
        .passes
        .iter()
        .filter(|k| !k.is_write() || config.write_dir.is_some())
        .count()
        .max(1);
    config.budget.saturating_sub(overhead(config)) / runnable as u32
}

/// Time reserved for scratch preparation, so the total run stays inside the
/// budget the user asked for.
fn overhead(config: &Config) -> Duration {
    if config.write_dir.is_none() {
        return Duration::from_millis(200);
    }
    // Reading through the scratch file means it must be filled with real data
    // first, which is far more expensive than merely creating it.
    if config.read_via_scratch() && config.has(false) {
        config.budget / 3
    } else {
        config.budget / 8
    }
}

pub fn scratch_size(free: u64) -> u64 {
    SCRATCH_MAX.min(free / SCRATCH_FREE_FRACTION)
}

pub fn spawn(config: Config) -> Run {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut scratch = None;
        if let Err(e) = run(&config, &tx, &worker_stop, &mut scratch) {
            let _ = tx.send(Msg::Fatal(e.to_string()));
        }
        // `scratch` drops here, removing the file, whatever path we took out.
        drop(scratch);
        let _ = tx.send(Msg::Finished);
    });
    Run {
        rx,
        stop,
        handle: Some(handle),
    }
}

/// Owns the scratch file's lifetime: the file is removed when this is dropped.
struct Scratch {
    path: PathBuf,
    size: u64,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn run(
    config: &Config,
    tx: &Sender<Msg>,
    stop: &AtomicBool,
    scratch: &mut Option<Scratch>,
) -> io::Result<()> {
    let slice = per_pass_duration(config);
    // Reads need a filled scratch file only when the target is a directory; a
    // raw device or an existing file already has data to read.
    let fill = config.read_via_scratch() && config.has(false);

    if config.has(true) || fill {
        if let Some(dir) = &config.write_dir {
            let _ = tx.send(Msg::Phase("preparing scratch file".into()));
            let fill_budget = fill.then(|| overhead(config));
            match prepare_scratch(dir, config.free_space, fill_budget, config.direct, stop) {
                Ok(s) => *scratch = Some(s),
                Err(e) => {
                    // Reported per pass below rather than aborting the run: any
                    // results we can still get are worth having.
                    let _ = tx.send(Msg::Phase(format!("scratch file unavailable: {e}")));
                }
            }
        }
    }

    for (index, &kind) in config.passes.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let _ = tx.send(Msg::PassStart { index });

        let result = if kind.is_write() {
            match (&config.write_dir, scratch.as_ref()) {
                (None, _) => PassResult::empty(
                    kind,
                    Status::Skipped("no writable filesystem on this device".into()),
                ),
                (Some(_), None) => {
                    PassResult::empty(kind, Status::Skipped("scratch file unavailable".into()))
                }
                (Some(_), Some(s)) => {
                    match write_pass(kind, s, config.direct, slice, index, tx, stop) {
                        Ok(r) => r,
                        Err(e) => PassResult::empty(kind, Status::Failed(e.to_string())),
                    }
                }
            }
        } else {
            // Directory targets read back the scratch file; everything else
            // reads the device or file the user named.
            let target = if config.read_via_scratch() {
                scratch.as_ref().map(|s| (s.path.as_path(), s.size))
            } else {
                Some((config.read_path.as_path(), config.read_size))
            };
            match target {
                None => PassResult::empty(
                    kind,
                    Status::Skipped("no readable scratch file on this filesystem".into()),
                ),
                Some((path, size)) => {
                    match read_pass(kind, path, size, config.direct, slice, index, tx, stop) {
                        Ok(r) => r,
                        Err(e) => PassResult::empty(kind, Status::Failed(e.to_string())),
                    }
                }
            }
        };

        let _ = tx.send(Msg::PassDone {
            index,
            result: Box::new(result),
        });
    }
    Ok(())
}

// ------------------------------------------------------------ open paths ----

/// Open for reading with cache bypass where the platform supports it.
///
/// Returns whether the bypass actually took effect: `O_DIRECT` is refused by
/// tmpfs and some network filesystems, and silently reporting cached numbers
/// would make the whole benchmark a lie.
fn open_read(path: &Path, direct: bool) -> io::Result<(File, bool)> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    open_with_direct(opts, path, direct)
}

/// Open the scratch file read-write. Never called with a block device path.
fn open_write(path: &Path, direct: bool) -> io::Result<(File, bool)> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    open_with_direct(opts, path, direct)
}

fn open_with_direct(
    #[allow(unused_mut)] mut opts: OpenOptions,
    path: &Path,
    direct: bool,
) -> io::Result<(File, bool)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if direct {
            let mut direct_opts = opts.clone();
            direct_opts.custom_flags(libc::O_DIRECT);
            match direct_opts.open(path) {
                Ok(f) => return Ok((f, true)),
                // EINVAL means the filesystem does not support O_DIRECT; any
                // other error would recur without it, so let it surface.
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
                Err(e) => return Err(e),
            }
        }
        Ok((opts.open(path)?, false))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let file = opts.open(path)?;
        if direct {
            // macOS has no O_DIRECT; F_NOCACHE is the documented equivalent.
            // SAFETY: fd is owned by `file` and valid for the call.
            let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
            return Ok((file, rc == 0));
        }
        Ok((file, false))
    }
}

// ----------------------------------------------------------------- passes ----

/// Sample window for the live rate readout. Short enough to feel responsive,
/// long enough that a single slow 4 MiB read does not make it jump around.
const SAMPLE: Duration = Duration::from_millis(150);

struct Meter {
    started: Instant,
    deadline: Instant,
    bytes: u64,
    ops: u64,
    max_latency: Duration,
    last_report: Instant,
    last_bytes: u64,
}

impl Meter {
    fn new(duration: Duration) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            deadline: now + duration,
            bytes: 0,
            ops: 0,
            max_latency: Duration::ZERO,
            last_report: now,
            last_bytes: 0,
        }
    }

    fn record(&mut self, n: u64, latency: Duration) {
        self.bytes += n;
        self.ops += 1;
        self.max_latency = self.max_latency.max(latency);
    }

    /// Emit a progress event at most once per [`SAMPLE`].
    fn maybe_report(&mut self, index: usize, duration: Duration, tx: &Sender<Msg>) {
        let now = Instant::now();
        let since = now.duration_since(self.last_report);
        if since < SAMPLE {
            return;
        }
        let rate_now = (self.bytes - self.last_bytes) as f64 / since.as_secs_f64();
        self.last_report = now;
        self.last_bytes = self.bytes;
        let elapsed = now.duration_since(self.started).as_secs_f64();
        let _ = tx.send(Msg::Progress {
            index,
            fraction: (elapsed / duration.as_secs_f64()).clamp(0.0, 1.0),
            bytes: self.bytes,
            rate_now,
        });
    }

    fn finish(self, kind: PassKind, block_size: usize, direct: bool) -> PassResult {
        let elapsed = self.started.elapsed();
        PassResult {
            kind,
            block_size,
            bytes: self.bytes,
            ops: self.ops,
            elapsed,
            avg_latency: if self.ops > 0 {
                elapsed.div_f64(self.ops as f64)
            } else {
                Duration::ZERO
            },
            max_latency: self.max_latency,
            direct,
            status: Status::Done,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn read_pass(
    kind: PassKind,
    path: &Path,
    size: u64,
    want_direct: bool,
    duration: Duration,
    index: usize,
    tx: &Sender<Msg>,
    stop: &AtomicBool,
) -> io::Result<PassResult> {
    let block = kind.block_size();
    let (file, direct) = open_read(path, want_direct)?;

    let span = usable_span(size, block)?;
    let mut buf = AlignedBuf::new(block);
    let mut rng = Rng::from_clock();
    let mut offset = 0u64;

    // Warm up the path (open, first fault, drive spin-up) outside the clock so
    // it does not skew a ten-second measurement.
    let _ = file.read_at(&mut buf.as_mut_slice()[..block], 0);

    let mut meter = Meter::new(duration);
    while Instant::now() < meter.deadline {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let at = if kind.is_random() {
            rng.aligned_offset(span, block)
        } else {
            let o = offset;
            offset += block as u64;
            // Wrap rather than stop: on a small device a fast sequential read
            // would otherwise finish long before the time slice.
            if offset > span {
                offset = 0;
            }
            o
        };

        let t = Instant::now();
        let n = file.read_at(&mut buf.as_mut_slice()[..block], at)?;
        let latency = t.elapsed();
        if n == 0 {
            offset = 0;
            continue;
        }
        meter.record(n as u64, latency);
        meter.maybe_report(index, duration, tx);
    }
    Ok(meter.finish(kind, block, direct))
}

fn write_pass(
    kind: PassKind,
    scratch: &Scratch,
    direct: bool,
    duration: Duration,
    index: usize,
    tx: &Sender<Msg>,
    stop: &AtomicBool,
) -> io::Result<PassResult> {
    let block = kind.block_size();
    let (file, direct) = open_write(&scratch.path, direct)?;

    let span = usable_span(scratch.size, block)?;
    let mut buf = AlignedBuf::new(block);
    buf.fill_incompressible(0x9E3779B97F4A7C15);
    let mut rng = Rng::from_clock();
    let mut offset = 0u64;

    let mut meter = Meter::new(duration);
    while Instant::now() < meter.deadline {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let at = if kind.is_random() {
            rng.aligned_offset(span, block)
        } else {
            let o = offset;
            offset += block as u64;
            if offset > span {
                offset = 0;
            }
            o
        };

        let t = Instant::now();
        file.write_all_at(&buf.as_slice()[..block], at)?;
        let latency = t.elapsed();
        meter.record(block as u64, latency);
        meter.maybe_report(index, duration, tx);
    }

    // Count the flush: without it a buffered fallback would report the speed of
    // memcpy into the page cache rather than the speed of the device.
    file.sync_data()?;
    Ok(meter.finish(kind, block, direct))
}

/// Largest offset span that is a whole number of blocks.
fn usable_span(size: u64, block: usize) -> io::Result<u64> {
    let span = size - size % block as u64;
    if span < block as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("target is smaller than one {block}-byte block"),
        ));
    }
    Ok(span)
}

/// Create the scratch file, optionally filling it with real data.
///
/// `fill_budget` is required when the read passes will read this file back:
/// `set_len` alone leaves a sparse file, and reading holes measures the
/// filesystem returning zeroes rather than the device. Filling is capped by the
/// budget and the file is then trimmed to what actually got written, so a slow
/// SD card shortens the file instead of overrunning the time limit.
fn prepare_scratch(
    dir: &Path,
    free: u64,
    fill_budget: Option<Duration>,
    direct: bool,
    stop: &AtomicBool,
) -> io::Result<Scratch> {
    let size = scratch_size(free);
    if size < SCRATCH_MIN {
        // ErrorKind::StorageFull would read better but needs Rust 1.83, above
        // the MSRV that keeps older SBC distro toolchains working.
        return Err(io::Error::other(format!(
            "needs {} free, only {} available",
            crate::format::bytes(SCRATCH_MIN * SCRATCH_FREE_FRACTION),
            crate::format::bytes(free)
        )));
    }
    // PID plus a counter: the PID keeps concurrent blkspeed processes apart,
    // the counter keeps repeated runs inside one process from sharing a file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = dir.join(format!(
        ".blkspeed-scratch-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    // From here on the file is tracked, so any later failure still cleans up.
    let mut scratch = Scratch { path, size };

    file.set_len(size)?;
    if stop.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }

    if let Some(budget) = fill_budget {
        let written = fill_scratch(&scratch.path, size, direct, budget, stop)?;
        let usable = written - written % SEQ_BLOCK as u64;
        if usable < SCRATCH_MIN / 8 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "device too slow to prepare a read sample within the budget",
            ));
        }
        scratch.size = usable;
    }
    Ok(scratch)
}

/// Write real data into the scratch file, returning how many bytes landed
/// before the budget ran out.
fn fill_scratch(
    path: &Path,
    size: u64,
    direct: bool,
    budget: Duration,
    stop: &AtomicBool,
) -> io::Result<u64> {
    let (file, _) = open_write(path, direct)?;
    let mut buf = AlignedBuf::new(SEQ_BLOCK);
    buf.fill_incompressible(0x243F6A8885A308D3);

    let deadline = Instant::now() + budget;
    let mut offset = 0u64;
    while offset + SEQ_BLOCK as u64 <= size {
        if Instant::now() >= deadline || stop.load(Ordering::Relaxed) {
            break;
        }
        file.write_all_at(buf.as_slice(), offset)?;
        offset += SEQ_BLOCK as u64;
    }
    file.sync_data()?;
    if offset < size {
        // Drop the unwritten tail so no pass can read a sparse hole.
        file.set_len(offset)?;
    }
    Ok(offset)
}

// -------------------------------------------------------------------- rng ----

/// xorshift64*. Pulling in a full RNG crate for offset selection would only add
/// cross-compilation surface for SBC targets.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Zero is the one state xorshift cannot escape from.
        Self(seed.max(1))
    }

    pub fn from_clock() -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0x2545F4914F6CDD1D);
        Self::new(nanos)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Uniform block-aligned offset in `[0, span - block]`.
    fn aligned_offset(&mut self, span: u64, block: usize) -> u64 {
        let blocks = span / block as u64;
        debug_assert!(blocks > 0);
        (self.next_u64() % blocks) * block as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(passes: Vec<PassKind>, write: bool) -> Config {
        Config {
            read_path: PathBuf::from("/dev/null"),
            read_size: 1 << 30,
            write_dir: write.then(|| PathBuf::from("/tmp")),
            free_space: 100 << 30,
            direct: true,
            budget: Duration::from_secs(40),
            passes,
        }
    }

    #[test]
    fn budget_is_split_across_runnable_passes_only() {
        let all = config(PassKind::ALL.to_vec(), true);
        let slice = per_pass_duration(&all);
        assert!(slice * 4 <= all.budget, "must stay inside the budget");
        assert!(slice >= Duration::from_secs(8));

        // Without a writable filesystem the two write passes are dropped, and
        // the reads get the time instead.
        let reads_only = config(PassKind::ALL.to_vec(), false);
        assert!(per_pass_duration(&reads_only) > slice);
    }

    #[test]
    fn scratch_never_eats_more_than_an_eighth_of_free_space() {
        assert_eq!(scratch_size(800 << 20), 100 << 20);
        assert_eq!(scratch_size(1 << 40), SCRATCH_MAX);
    }

    #[test]
    fn usable_span_rejects_tiny_targets() {
        assert_eq!(usable_span(10_000, 4096).unwrap(), 8192);
        assert!(usable_span(100, 4096).is_err());
    }

    #[test]
    fn random_offsets_are_aligned_and_in_range() {
        let mut rng = Rng::new(7);
        let span = 1 << 20;
        for _ in 0..1000 {
            let off = rng.aligned_offset(span, RAND_BLOCK);
            assert_eq!(off % RAND_BLOCK as u64, 0);
            assert!(off + RAND_BLOCK as u64 <= span);
        }
    }

    fn scratch(fill: Option<Duration>) -> Scratch {
        prepare_scratch(
            &std::env::temp_dir(),
            100 << 30,
            fill,
            true,
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    #[test]
    fn scratch_file_is_removed_on_drop() {
        let s = scratch(None);
        let path = s.path.clone();
        assert!(path.exists());
        drop(s);
        assert!(!path.exists());
    }

    #[test]
    fn read_pass_measures_a_regular_file() {
        // Uses a real file so the pread path, wrap-around and metering are all
        // exercised end to end.
        let s = scratch(None);
        let (tx, rx) = mpsc::channel();
        let result = read_pass(
            PassKind::RandRead,
            &s.path,
            s.size,
            true,
            Duration::from_millis(300),
            0,
            &tx,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(result.status, Status::Done);
        assert!(result.ops > 0);
        assert_eq!(result.bytes, result.ops * RAND_BLOCK as u64);
        assert!(result.rate() > 0.0);
        assert!(rx.try_iter().count() > 0, "should emit progress");
    }

    #[test]
    fn stop_flag_ends_a_pass_early() {
        let s = scratch(None);
        let (tx, _rx) = mpsc::channel();
        let start = Instant::now();
        let _ = read_pass(
            PassKind::SeqRead,
            &s.path,
            s.size,
            true,
            Duration::from_secs(30),
            0,
            &tx,
            &AtomicBool::new(true),
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_filled_scratch_file_has_no_sparse_holes() {
        // Reading holes would measure the filesystem inventing zeroes rather
        // than the device, so every byte inside `size` must be real data.
        let s = scratch(Some(Duration::from_secs(10)));
        assert!(s.size >= SEQ_BLOCK as u64);
        assert_eq!(s.size % SEQ_BLOCK as u64, 0);

        let file = std::fs::File::open(&s.path).unwrap();
        assert!(file.metadata().unwrap().len() >= s.size);

        let mut buf = vec![0u8; SEQ_BLOCK];
        for offset in [0, s.size - SEQ_BLOCK as u64] {
            file.read_exact_at(&mut buf, offset).unwrap();
            assert!(buf.iter().any(|&b| b != 0), "hole at offset {offset}");
        }
    }

    #[test]
    fn filling_stops_at_the_budget_and_trims_the_file() {
        // A very short budget cannot fill a full-size scratch file; the file
        // must shrink to what was written rather than the run overrunning.
        let s = prepare_scratch(
            &std::env::temp_dir(),
            100 << 30,
            Some(Duration::from_millis(1)),
            true,
            &AtomicBool::new(false),
        );
        if let Ok(s) = s {
            assert!(s.size < scratch_size(100 << 30));
            assert_eq!(std::fs::metadata(&s.path).unwrap().len(), s.size);
        }
        // An Err here is also correct: the device was too slow to sample.
    }

    #[test]
    fn directory_targets_read_through_the_scratch_file() {
        let mut cfg = config(PassKind::ALL.to_vec(), true);
        cfg.read_path = std::env::temp_dir();
        assert!(cfg.read_via_scratch());
        // Filling costs time, so the per-pass slice must shrink accordingly.
        assert!(overhead(&cfg) > cfg.budget / 4);

        cfg.read_path = PathBuf::from("/dev/null");
        assert!(!cfg.read_via_scratch());
    }
}
