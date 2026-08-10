use std::mem::MaybeUninit;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

const LOW_RAM_MAX_BYTES: u64 = 1024 * MIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemorySnapshot {
    pub(crate) available_bytes: u64,

    pub(crate) total_bytes: u64,
}

impl MemorySnapshot {
    fn new(total_bytes: u64, available_bytes: u64) -> Option<Self> {
        (total_bytes != 0).then_some(Self {
            available_bytes: available_bytes.min(total_bytes),
            total_bytes,
        })
    }

    #[must_use]
    pub(crate) const fn is_low_ram_device(self) -> bool {
        self.total_bytes <= LOW_RAM_MAX_BYTES
    }

    #[must_use]
    pub(crate) const fn total_mib(self) -> u64 {
        self.total_bytes / MIB
    }
}

fn parse_kib_value(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?.trim_start();
    let mut fields = rest.split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    if fields.next()? != "kB" || fields.next().is_some() {
        return None;
    }
    value.checked_mul(KIB)
}

fn parse_meminfo(contents: &str) -> Option<MemorySnapshot> {
    let mut total = None;
    let mut available = None;
    for line in contents.lines() {
        if total.is_none() {
            total = parse_kib_value(line, "MemTotal:");
        }
        if available.is_none() {
            available = parse_kib_value(line, "MemAvailable:");
        }
        if total.is_some() && available.is_some() {
            break;
        }
    }
    MemorySnapshot::new(total?, available?)
}

fn sysinfo_snapshot() -> Option<MemorySnapshot> {
    let mut raw = MaybeUninit::<libc::sysinfo>::zeroed();

    if unsafe { libc::sysinfo(raw.as_mut_ptr()) } != 0 {
        return None;
    }

    let raw = unsafe { raw.assume_init() };
    let unit = u64::from(raw.mem_unit.max(1));
    let total = raw.totalram.checked_mul(unit)?;
    let available_units = raw.freeram.checked_add(raw.bufferram)?;
    let available = available_units.checked_mul(unit)?;
    MemorySnapshot::new(total, available)
}

#[must_use]
pub(crate) fn host_memory_snapshot() -> Option<MemorySnapshot> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| parse_meminfo(&contents))
        .or_else(sysinfo_snapshot)
}

#[must_use]
pub(crate) const fn managed_heap_mib() -> u32 {
    crate::runtime::HEAP_MIB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parser_returns_kernel_values_in_bytes() {
        let snapshot = parse_meminfo(
            "MemTotal:       32423572 kB\nMemFree:          100000 kB\n\
             MemAvailable:   17081728 kB\nBuffers:           50000 kB\n",
        )
        .expect("valid meminfo must parse");
        assert_eq!(snapshot.total_bytes, 32_423_572 * KIB);
        assert_eq!(snapshot.available_bytes, 17_081_728 * KIB);
    }

    #[test]
    fn meminfo_parser_rejects_missing_wrong_unit_and_overflow() {
        assert_eq!(parse_meminfo("MemTotal: 4 kB\n"), None);
        assert_eq!(
            parse_meminfo("MemTotal: 4096 MB\nMemAvailable: 2048 kB\n"),
            None
        );
        assert_eq!(
            parse_meminfo(&format!("MemTotal: {} kB\nMemAvailable: 1 kB\n", u64::MAX)),
            None
        );
    }

    #[test]
    fn snapshot_clamps_available_and_classifies_low_ram() {
        let low = MemorySnapshot::new(512 * MIB, 900 * MIB).expect("non-zero total");
        assert_eq!(low.available_bytes, low.total_bytes);
        assert!(low.is_low_ram_device());

        let normal = MemorySnapshot::new(2 * 1024 * MIB, 1024 * MIB).expect("non-zero total");
        assert!(!normal.is_low_ram_device());
        assert_eq!(MemorySnapshot::new(0, 0), None);
    }

    #[test]
    fn live_host_snapshot_is_nonzero_and_heap_class_matches_boot_plan() {
        let snapshot = host_memory_snapshot().expect("Linux must expose meminfo or sysinfo");
        assert!(snapshot.total_bytes >= snapshot.available_bytes);
        assert!(snapshot.total_bytes >= MIB);
        assert_eq!(managed_heap_mib(), 768);
    }
}
