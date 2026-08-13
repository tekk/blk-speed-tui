# blkspeed

A terminal UI for measuring how fast a disk actually is, on the machine it is
plugged into. Built for single-board computers - Radxa, Raspberry Pi, NanoPi,
Orange Pi - and equally at home on an x86 PC or a Mac.

A full run takes well under a minute and never destroys anything.

```
┌──────────────────────────────────────────────────────────────────────────────┐
│blkspeed  non-destructive disk speed test   I/O: direct (cache bypassed)      │
└──────────────────────────────────────────────────────────────────────────────┘
┌ Devices ─────────────────────────────┐┌ Details ────────────────────────────┐
│▌nvme0n1     954 GiB  SSD      /       ││Device      /dev/nvme0n1             │
│ └ nvme0n1p1  16 MiB  SSD              ││Model       SKHynix_HFS001TDE9X084N  │
│ └ nvme0n1p2 426 GiB  SSD      /boot   ││Capacity    954 GiB                  │
│  sda       5.46 TiB  HDD              ││Filesystem  /                        │
│                                       ││                                     │
│                                       ││Write test: temporary file, removed  │
│                                       ││Read test:  raw device, read-only    │
└───────────────────────────────────────┘└─────────────────────────────────────┘
 ↑/↓ select   enter start   c cache bypass   t theme   q quit
```

## Sample recording

[![asciicast](https://asciinema.org/a/1262949.svg)](https://asciinema.org/a/1262949)

## Why it is non-destructive

This is the part worth being precise about, because most disk benchmarks are
not safe to point at a mounted system disk.

- **Reads** open the block device **read-only**. The device is never opened for
  writing - not once, anywhere in the program.
- **Writes** go to a temporary file on a filesystem mounted from that device,
  and the file is deleted when the run ends. That includes when you quit early,
  cancel with <kbd>Esc</kbd>, or the run fails: the cleanup is tied to the
  file's lifetime rather than to the happy path.

So the write figures describe the device *through its filesystem*, which is
what you would actually get when writing files to it. Raw-device writes would
be faster to measure and would also destroy the partition table, so they are
not offered.

## What it measures

Four passes, sharing the time budget evenly:

| Pass             | Block size | Tells you                              |
| ---------------- | ---------- | -------------------------------------- |
| Sequential read  | 4 MiB      | Large-file and boot-image throughput   |
| Random read      | 4 KiB      | Latency and IOPS - how snappy it feels |
| Sequential write | 4 MiB      | Bulk copy and logging speed            |
| Random write     | 4 KiB      | Database and package-manager workloads |

**Cache bypass is on by default** (`O_DIRECT` on Linux, `F_NOCACHE` on macOS).
Without it a benchmark mostly measures RAM: page-cache reads on an SBC happily
report several GB/s from an SD card that can barely sustain 20 MB/s. Some
filesystems refuse `O_DIRECT`; when that happens the pass falls back to
buffered I/O and the result is **labelled `buffered`**, with a warning in the
summary, rather than being quietly passed off as a device speed.

Write passes flush with `fsync` before the clock stops, so the number includes
getting the data onto the medium.

## Install

Download a binary for your board from the
[releases page](https://github.com/tekk/blk-speed-tui/releases) and unpack it:

```sh
tar xzf blkspeed-v0.1.0-aarch64-unknown-linux-musl.tar.gz
sudo install -m755 blkspeed-*/blkspeed /usr/local/bin/
```

Pick `musl` if you are unsure - those binaries are statically linked and run on
any Linux distribution regardless of its glibc version, which matters on older
SBC images.

| Board                                 | Target                            |
| ------------------------------------- | --------------------------------- |
| Radxa Rock, RPi 3/4/5, NanoPi (64-bit)| `aarch64-unknown-linux-musl`      |
| RPi 2/3/4 on a 32-bit OS, Orange Pi   | `armv7-unknown-linux-musleabihf`  |
| RPi 1 / Zero / Zero W                 | `arm-unknown-linux-gnueabihf`     |
| x86 PC, mini PC                       | `x86_64-unknown-linux-musl`       |
| StarFive VisionFive, Milk-V           | `riscv64gc-unknown-linux-gnu`     |
| Apple Silicon Mac                     | `aarch64-apple-darwin`            |
| Intel Mac                             | `x86_64-apple-darwin`             |

Or build it yourself - the only dependencies are `ratatui` and `libc`:

```sh
cargo build --release
```

## Usage

```sh
sudo blkspeed                       # pick a device from the list
sudo blkspeed --target /dev/mmcblk0 # go straight to one device
blkspeed --target /mnt/usb          # test a filesystem, no root needed
blkspeed --target . --json          # headless, machine-readable
```

Raw-device reads need root, so run under `sudo` for a full test. Without it,
every device is marked `🔒 needs root` in the picker - pointing `--target` at a
directory still gives you a complete filesystem-level measurement as a normal
user.

### Keys

| Key                          | Action                              |
| ---------------------------- | ----------------------------------- |
| <kbd>↑</kbd> <kbd>↓</kbd>    | Select a device (<kbd>j</kbd>/<kbd>k</kbd> also work) |
| <kbd>Enter</kbd>             | Start                               |
| <kbd>Esc</kbd>               | Cancel a run, or go back            |
| <kbd>r</kbd>                 | Re-run                              |
| <kbd>c</kbd>                 | Toggle cache bypass                 |
| <kbd>t</kbd>                 | Toggle light/dark                   |
| <kbd>q</kbd>                 | Quit                                |

### Options

```
-t, --target <PATH>    Device, directory or file to test
-b, --budget <SECS>    Total budget for all passes (default 40, max 600)
-p, --passes <LIST>    seq_read,rand_read,seq_write,rand_write
    --no-direct        Allow the page cache
    --theme <MODE>     auto | dark | light
-l, --list             List detected devices and exit
-j, --json             Headless mode, JSON on stdout
```

### JSON output

For logging results across a fleet of boards, or comparing SD cards:

```sh
blkspeed --target /mnt/sdcard --budget 30 --json
```

```json
{
  "device": "/mnt/sdcard",
  "size_bytes": 31914983424,
  "total_seconds": 28.4,
  "passes": [
    {"pass": "seq_read", "status": "ok", "bytes_per_second": 89128960,
     "human": "89.1 MB/s", "iops": 21, "avg_latency_us": 47060.0,
     "cache_bypassed": true}
  ]
}
```

## Terminal support

Light and dark terminals are both first-class. The app never paints its own
background - it leaves the terminal's showing through and themes only the
foreground - so it inherits your colour scheme instead of fighting it. Detection
uses `COLORFGBG` where the terminal sets it, and <kbd>t</kbd> switches manually
if the guess is wrong.

Rendering is double-buffered and diffed: only the cells that changed are
written each frame, and the screen is cleared exactly once at startup. There is
no flicker even on a slow serial console.

## Notes on interpreting results

- **Run it more than once.** SD cards and cheap SSDs have write caches that
  make the first run of a pass look better than sustained performance.
- **Thermals matter on SBCs.** An NVMe drive in a Pi hat will throttle; a
  40-second test may not reach that point, which is deliberate - it measures
  what you get in ordinary use.
- **Random 4 KiB writes are the number to watch** if the board feels slow. A
  card doing 90 MB/s sequential but 2 MB/s random will make a package upgrade
  crawl.
- The scratch file is capped at 1 GiB and at 1/8 of free space, and shrinks
  further on slow media rather than overrunning the time budget.

## Development

```sh
cargo test      # 47 tests, including UI rendering against a test backend
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Tagging a commit `v*` builds and publishes binaries for every target above.

## License

MIT
