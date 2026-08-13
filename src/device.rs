//! Block device discovery.
//!
//! Linux reads `/sys/block`, macOS shells out to `diskutil list`. Both then
//! cross-reference the mount table so the UI can tell the user where a write
//! test would actually put its scratch file.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Disk,
    Partition,
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub path: PathBuf,
    pub fstype: String,
    pub writable: bool,
    pub free: u64,
}

#[derive(Debug, Clone)]
pub struct Device {
    /// Kernel name, e.g. `nvme0n1` or `disk0s2`.
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub model: String,
    pub rotational: Option<bool>,
    pub removable: bool,
    pub mount: Option<Mount>,
    /// True when `mount` was borrowed from a partition of this disk rather than
    /// being a filesystem on the device itself. Selecting a whole disk is the
    /// common case, and it is almost never mounted directly.
    pub mount_inherited: bool,
}

impl Device {
    /// Whether we can open the raw device for reading. Root is usually needed;
    /// the selector shows this up front instead of failing mid-benchmark.
    pub fn readable(&self) -> bool {
        access(&self.path, libc::R_OK)
    }

    /// Scratch directory for the write passes, if one exists. We never open a
    /// block device for writing — the write test always goes through a regular
    /// file on a mounted filesystem, which is what makes this non-destructive.
    pub fn write_dir(&self) -> Option<&Path> {
        self.mount
            .as_ref()
            .filter(|m| m.writable)
            .map(|m| m.path.as_path())
    }

    pub fn media(&self) -> &'static str {
        match self.rotational {
            Some(true) => "HDD",
            Some(false) => "SSD",
            None => "—",
        }
    }

    /// One-line reason the device cannot be tested at all, if any.
    pub fn blocker(&self) -> Option<String> {
        if self.size == 0 {
            return Some("no media / zero size".into());
        }
        if !self.readable() {
            return Some(format!(
                "no read permission on {} — try running with sudo",
                self.path.display()
            ));
        }
        None
    }
}

fn access(path: &Path, mode: libc::c_int) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

fn free_space(path: &Path) -> u64 {
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return 0;
    };
    // SAFETY: statvfs only writes into `st`, which we fully initialise first.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return 0;
        }
        // f_bavail is the space available to unprivileged users, which is the
        // number that matters for a scratch file.
        (st.f_bavail as u64).saturating_mul(st.f_frsize as u64)
    }
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Enumerate candidate devices, largest disks first, partitions under them.
pub fn enumerate() -> Vec<Device> {
    let mounts = mount_table();
    let mut devices = platform_enumerate(&mounts);
    devices.retain(|d| d.size > 0);
    sort_devices(&mut devices);
    inherit_mounts(&mut devices);
    devices
}

/// Give each unmounted disk a scratch location borrowed from one of its own
/// partitions, so selecting a whole disk still runs the write passes — and
/// still writes to the same physical device being measured.
fn inherit_mounts(devices: &mut [Device]) {
    let disks: Vec<String> = devices
        .iter()
        .filter(|d| d.kind == Kind::Disk && d.mount.is_none())
        .map(|d| d.name.clone())
        .collect();

    for disk in disks {
        let best = devices
            .iter()
            .filter(|d| d.kind == Kind::Partition && d.name.starts_with(&disk))
            .filter_map(|d| d.mount.clone())
            .filter(|m| m.writable)
            // Most free space wins: the scratch file needs room.
            .max_by_key(|m| m.free);

        if let Some(mount) = best {
            if let Some(d) = devices.iter_mut().find(|d| d.name == disk) {
                d.mount = Some(mount);
                d.mount_inherited = true;
            }
        }
    }
}

/// Disks descend by size; each disk's partitions follow it in name order, so
/// the list reads as a tree even though it is flat.
fn sort_devices(devices: &mut [Device]) {
    let rank: HashMap<String, (u64, String)> = devices
        .iter()
        .filter(|d| d.kind == Kind::Disk)
        .map(|d| (d.name.clone(), (d.size, d.name.clone())))
        .collect();

    let key = |d: &Device| -> (u64, String, u8, String) {
        let parent = parent_disk(&d.name, &rank);
        let (psize, pname) = rank
            .get(&parent)
            .cloned()
            .unwrap_or((d.size, d.name.clone()));
        (
            u64::MAX - psize,
            pname,
            u8::from(d.kind == Kind::Partition),
            d.name.clone(),
        )
    };
    devices.sort_by_key(key);
}

fn parent_disk(name: &str, disks: &HashMap<String, (u64, String)>) -> String {
    // Longest matching disk name wins: `nvme0n1p1` belongs to `nvme0n1`, not
    // to a hypothetical `nvme0`.
    disks
        .keys()
        .filter(|d| name.starts_with(d.as_str()))
        .max_by_key(|d| d.len())
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// Map device path -> mount info. Only the first mount of a device is kept;
/// bind mounts and later entries would only confuse the picker.
fn mount_table() -> HashMap<PathBuf, Mount> {
    let mut out: HashMap<PathBuf, Mount> = HashMap::new();
    for (dev, path, fstype) in raw_mounts() {
        if !dev.starts_with('/') {
            continue;
        }
        let path = PathBuf::from(path);
        // Pseudo filesystems are never a useful write target.
        if matches!(fstype.as_str(), "squashfs" | "iso9660" | "devtmpfs") {
            continue;
        }
        let dev = PathBuf::from(dev);
        let canonical = fs::canonicalize(&dev).unwrap_or_else(|_| dev.clone());
        let mount = Mount {
            writable: access(&path, libc::W_OK),
            free: free_space(&path),
            fstype,
            path,
        };
        out.entry(canonical).or_insert(mount);
    }
    resolve_stacked(&mut out);
    out
}

/// Attribute mounts sitting on top of device-mapper (LUKS, LVM) to the physical
/// partition underneath.
///
/// Without this, an encrypted root — the default on many desktop installs and
/// increasingly on SBC images — shows every real device as unmounted, silently
/// disabling the write passes. Aliases are added with `or_insert`, so a real
/// mount on the partition always takes precedence.
#[cfg(target_os = "linux")]
fn resolve_stacked(map: &mut HashMap<PathBuf, Mount>) {
    let stacked: Vec<(String, Mount)> = map
        .iter()
        .filter_map(|(path, mount)| {
            let name = path.file_name()?.to_str()?;
            name.starts_with("dm-")
                .then(|| (name.to_string(), mount.clone()))
        })
        .collect();

    for (name, mount) in stacked {
        for parent in dm_parents(&name, 0) {
            map.entry(PathBuf::from("/dev").join(parent))
                .or_insert_with(|| mount.clone());
        }
    }
}

/// Physical devices beneath a device-mapper node, following stacks such as
/// LVM-on-LUKS. The depth limit is pure paranoia against a cyclic sysfs.
#[cfg(target_os = "linux")]
fn dm_parents(name: &str, depth: usize) -> Vec<String> {
    if depth > 4 {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(format!("/sys/block/{name}/slaves")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let slave = entry.file_name().to_string_lossy().into_owned();
        if slave.starts_with("dm-") {
            out.extend(dm_parents(&slave, depth + 1));
        } else {
            out.push(slave);
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn resolve_stacked(_map: &mut HashMap<PathBuf, Mount>) {}

#[cfg(target_os = "linux")]
fn raw_mounts() -> Vec<(String, String, String)> {
    let Ok(text) = fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let dev = f.next()?;
            // Paths in /proc/mounts escape spaces and friends as octal.
            let path = unescape_octal(f.next()?);
            let fstype = f.next()?;
            Some((dev.to_string(), path, fstype.to_string()))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(target_os = "macos")]
fn raw_mounts() -> Vec<(String, String, String)> {
    // `mount` prints: /dev/disk1s5 on / (apfs, local, read-only, journaled)
    let Ok(out) = std::process::Command::new("/sbin/mount").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (dev, rest) = line.split_once(" on ")?;
            let (path, opts) = rest.rsplit_once(" (")?;
            let fstype = opts.trim_end_matches(')').split(',').next()?.trim();
            Some((dev.to_string(), path.to_string(), fstype.to_string()))
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn raw_mounts() -> Vec<(String, String, String)> {
    Vec::new()
}

// ---------------------------------------------------------------- Linux ----

#[cfg(target_os = "linux")]
fn platform_enumerate(mounts: &HashMap<PathBuf, Mount>) -> Vec<Device> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_virtual(&name) {
            continue;
        }
        let sys = entry.path();
        let Some(disk) = linux_device(&sys, &name, Kind::Disk, mounts) else {
            continue;
        };
        let removable = disk.removable;
        let model = disk.model.clone();
        out.push(disk);

        // Partitions are subdirectories carrying a `partition` file.
        let Ok(children) = fs::read_dir(&sys) else {
            continue;
        };
        for child in children.flatten() {
            let cname = child.file_name().to_string_lossy().into_owned();
            if !child.path().join("partition").exists() {
                continue;
            }
            if let Some(mut part) = linux_device(&child.path(), &cname, Kind::Partition, mounts) {
                part.removable = removable;
                part.model.clone_from(&model);
                out.push(part);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn is_virtual(name: &str) -> bool {
    const PREFIXES: [&str; 5] = ["loop", "ram", "zram", "fd", "dm-"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(target_os = "linux")]
fn linux_device(
    sys: &Path,
    name: &str,
    kind: Kind,
    mounts: &HashMap<PathBuf, Mount>,
) -> Option<Device> {
    // `size` is always in 512-byte sectors regardless of the physical sector
    // size — a long-standing sysfs quirk.
    let sectors: u64 = read_trim(sys.join("size"))?.parse().ok()?;
    let path = PathBuf::from("/dev").join(name);

    let model = read_trim(sys.join("device/model"))
        .or_else(|| read_trim(sys.join("../device/model")))
        .or_else(|| read_trim(sys.join("device/name")))
        .unwrap_or_else(|| "unknown".into());

    let rotational = read_trim(sys.join("queue/rotational"))
        .or_else(|| read_trim(sys.join("../queue/rotational")))
        .map(|v| v == "1");

    Some(Device {
        size: sectors.saturating_mul(512),
        model,
        rotational,
        removable: read_trim(sys.join("removable")).as_deref() == Some("1"),
        mount: lookup_mount(&path, mounts),
        mount_inherited: false,
        name: name.to_string(),
        path,
        kind,
    })
}

// ---------------------------------------------------------------- macOS ----

#[cfg(target_os = "macos")]
fn platform_enumerate(mounts: &HashMap<PathBuf, Mount>) -> Vec<Device> {
    // Parsing `diskutil list` avoids pulling in a plist parser or IOKit
    // bindings. The columnar output has been stable for many releases.
    let Ok(out) = std::process::Command::new("/usr/sbin/diskutil")
        .arg("list")
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut devices = Vec::new();
    let mut current_disk = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("/dev/") {
            current_disk = rest.split_whitespace().next().unwrap_or("").to_string();
            continue;
        }
        // "   0:      GUID_partition_scheme   *500.3 GB   disk0"
        let Some((index, rest)) = trimmed.split_once(':') else {
            continue;
        };
        if index.trim().parse::<u32>().is_err() {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let Some(ident) = fields.last() else { continue };
        if !ident.starts_with("disk") {
            continue;
        }
        // Size is the "<number> <unit>" pair immediately before the identifier.
        let size = fields
            .len()
            .checked_sub(3)
            .and_then(|i| parse_diskutil_size(fields[i], fields[i + 1]))
            .unwrap_or(0);

        let kind = if *ident == current_disk {
            Kind::Disk
        } else {
            Kind::Partition
        };
        // Everything between the type and the size is the volume name.
        let model = fields
            .get(1..fields.len().saturating_sub(3))
            .map(|s| s.join(" "))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| fields.first().copied().unwrap_or("unknown").to_string());

        let path = PathBuf::from("/dev").join(ident);
        devices.push(Device {
            name: (*ident).to_string(),
            size,
            model,
            rotational: None,
            removable: false,
            mount: lookup_mount(&path, mounts),
            mount_inherited: false,
            path,
            kind,
        });
    }
    devices
}

#[cfg(target_os = "macos")]
fn parse_diskutil_size(value: &str, unit: &str) -> Option<u64> {
    // A leading '*' marks the whole-disk row.
    let n: f64 = value.trim_start_matches('*').parse().ok()?;
    let mult = match unit {
        "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "PB" => 1e15,
        _ => return None,
    };
    Some((n * mult) as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_enumerate(_mounts: &HashMap<PathBuf, Mount>) -> Vec<Device> {
    Vec::new()
}

/// On macOS the raw device is `/dev/rdiskN`; mounts refer to `/dev/diskN`.
/// Checking both keeps one lookup for every platform.
fn lookup_mount(path: &Path, mounts: &HashMap<PathBuf, Mount>) -> Option<Mount> {
    if let Some(m) = mounts.get(path) {
        return Some(m.clone());
    }
    let name = path.file_name()?.to_string_lossy();
    let alt = path.with_file_name(name.trim_start_matches('r'));
    mounts.get(&alt).cloned()
}

/// Build a synthetic device for an explicit `--target` path so the rest of the
/// program does not need a special case for it.
pub fn from_path(path: &Path) -> std::io::Result<Device> {
    let meta = fs::metadata(path)?;
    let is_dir = meta.is_dir();
    let mounts = mount_table();

    let (size, kind) = if is_dir {
        (free_space(path), Kind::Partition)
    } else {
        (meta.len(), Kind::Partition)
    };

    let mount = lookup_mount(path, &mounts).or(if is_dir {
        Some(Mount {
            path: path.to_path_buf(),
            fstype: "—".into(),
            writable: access(path, libc::W_OK),
            free: free_space(path),
        })
    } else {
        // A plain file: its parent directory is the scratch location.
        path.parent().map(|p| Mount {
            path: p.to_path_buf(),
            fstype: "—".into(),
            writable: access(p, libc::W_OK),
            free: free_space(p),
        })
    });

    Ok(Device {
        name: path.display().to_string(),
        path: path.to_path_buf(),
        kind,
        size,
        model: if is_dir { "directory" } else { "file" }.into(),
        rotational: None,
        removable: false,
        mount,
        mount_inherited: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, size: u64, kind: Kind) -> Device {
        Device {
            name: name.into(),
            path: PathBuf::from("/dev").join(name),
            kind,
            size,
            model: "t".into(),
            rotational: None,
            removable: false,
            mount: None,
            mount_inherited: false,
        }
    }

    #[test]
    fn partitions_follow_their_disk() {
        let mut v = vec![
            disk("sda1", 100, Kind::Partition),
            disk("nvme0n1", 900, Kind::Disk),
            disk("sda", 500, Kind::Disk),
            disk("nvme0n1p1", 400, Kind::Partition),
        ];
        sort_devices(&mut v);
        let names: Vec<&str> = v.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "nvme0n1p1", "sda", "sda1"]);
    }

    #[test]
    fn longest_disk_prefix_wins() {
        let mut disks = HashMap::new();
        disks.insert("nvme0".to_string(), (1, "nvme0".to_string()));
        disks.insert("nvme0n1".to_string(), (1, "nvme0n1".to_string()));
        assert_eq!(parent_disk("nvme0n1p3", &disks), "nvme0n1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_paths_are_unescaped() {
        assert_eq!(unescape_octal(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape_octal("/mnt/plain"), "/mnt/plain");
    }
}
