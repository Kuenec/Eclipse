//! Host performance policy for the Android engine.
//!
//! Roblox's frame-critical `FunctionMarshal` thread can saturate one CPU while its worker pool is
//! sized from `_SC_NPROCESSORS_ONLN`. On SMT systems, reporting every logical CPU makes the engine
//! create two workers per physical core; the extra runnable threads then compete for the same core
//! front-end and caches as the frame-critical thread. Eclipse's Sober-compatible
//! `graphics_optimization_mode = "performance"` therefore narrows the inherited affinity mask to
//! one allowed logical CPU per physical core before ART or `libroblox.so` starts. The engine sees
//! the physical-core count through the existing bionic `sysconf` bridge and sizes its worker pool
//! to match. Balanced/quality mode and small machines retain the kernel's original mask.

use std::collections::BTreeMap;
use std::io;

use crate::config::GraphicsOptimizationMode;

/// Leave at least this many physical cores available before trading SMT throughput for lower
/// frame-thread contention. Roblox's current worker mix occupies roughly four cores in a populated
/// scene; eight physical cores leave enough parallel capacity for the main, audio, I/O, and driver
/// work while keeping frame-critical work off sibling threads. Smaller CPUs keep their full mask.
const MIN_PHYSICAL_CORES: usize = 8;

/// Apply the engine CPU policy selected by the Sober-compatible graphics optimization mode.
///
/// This is deliberately best-effort: unavailable sysfs topology or a denied affinity syscall must
/// never prevent Roblox from starting. Call it before ART boots so every subsequently-created
/// thread inherits the selected mask and the engine's `_SC_NPROCESSORS_ONLN` query observes it.
pub fn configure_engine_cpu_affinity(mode: GraphicsOptimizationMode) {
    if mode != GraphicsOptimizationMode::Performance {
        return;
    }

    match physical_core_plan() {
        Ok(Some((allowed, selected))) => match set_current_affinity(&selected) {
            Ok(()) => tracing::info!(
                logical_cpus = allowed.len(),
                physical_cpus = selected.len(),
                cpus = ?selected,
                "performance mode: using one logical CPU per physical core"
            ),
            Err(error) => tracing::warn!(
                %error,
                "performance mode: could not apply the physical-core affinity hint"
            ),
        },
        Ok(None) => tracing::debug!(
            "performance mode: retaining the existing CPU affinity (no beneficial SMT reduction)"
        ),
        Err(error) => tracing::warn!(
            %error,
            "performance mode: CPU topology unavailable; retaining the existing affinity"
        ),
    }
}

/// Return `(original_allowed, one_cpu_per_physical_core)` when the topology has SMT and enough
/// physical cores for the latency-oriented policy to be useful.
fn physical_core_plan() -> io::Result<Option<(Vec<usize>, Vec<usize>)>> {
    let allowed = current_affinity()?;
    let selected = select_one_cpu_per_core(&allowed, linux_cpu_topology)?;
    if selected.len() < MIN_PHYSICAL_CORES || selected.len() == allowed.len() {
        return Ok(None);
    }
    Ok(Some((allowed, selected)))
}

/// Read the calling thread's kernel affinity mask. The launcher calls this before creating ART, so
/// this is also the mask every runtime/engine child would otherwise inherit.
fn current_affinity() -> io::Result<Vec<usize>> {
    // SAFETY: a zero bit-pattern is the documented empty `cpu_set_t`, immediately populated by
    // `sched_getaffinity` before any bit is inspected.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: pid 0 addresses the calling thread; `set` is writable for exactly the supplied
    // `cpu_set_t` size. The syscall writes only within that object.
    let rc = unsafe {
        libc::sched_getaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            std::ptr::addr_of_mut!(set),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut cpus = Vec::new();
    for cpu in 0..libc::CPU_SETSIZE as usize {
        // SAFETY: `cpu` is bounded by `CPU_SETSIZE`, and `set` was populated successfully above.
        if unsafe { libc::CPU_ISSET(cpu, &set) } {
            cpus.push(cpu);
        }
    }
    if cpus.is_empty() {
        return Err(io::Error::other(
            "sched_getaffinity returned an empty CPU mask",
        ));
    }
    Ok(cpus)
}

/// Narrow the calling thread to `cpus`. Linux threads created afterward inherit this mask.
fn set_current_affinity(cpus: &[usize]) -> io::Result<()> {
    // SAFETY: zero is the documented empty `cpu_set_t`, filled only through `CPU_SET` below.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &cpu in cpus {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("CPU {cpu} exceeds CPU_SETSIZE"),
            ));
        }
        // SAFETY: the explicit bound above keeps `CPU_SET` within `set`.
        unsafe { libc::CPU_SET(cpu, &mut set) };
    }
    // SAFETY: pid 0 selects the caller and `set` is a fully initialized mask of the stated size.
    let rc = unsafe {
        libc::sched_setaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            std::ptr::addr_of!(set),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Linux exposes stable package/core IDs in sysfs. Reading those files is detection, not an
/// assumption about numbering: sparse/offline/cgroup-restricted CPUs are filtered by `allowed`.
fn linux_cpu_topology(cpu: usize) -> io::Result<(u32, u32)> {
    let base = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
    let package = read_topology_id(&format!("{base}/physical_package_id"))?;
    let core = read_topology_id(&format!("{base}/core_id"))?;
    Ok((package, core))
}

fn read_topology_id(path: &str) -> io::Result<u32> {
    let text = std::fs::read_to_string(path)?;
    text.trim().parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid topology id in {path}: {error}"),
        )
    })
}

/// Choose the first allowed logical CPU for every distinct `(package_id, core_id)` pair.
fn select_one_cpu_per_core<F>(allowed: &[usize], mut topology: F) -> io::Result<Vec<usize>>
where
    F: FnMut(usize) -> io::Result<(u32, u32)>,
{
    let mut cores = BTreeMap::<(u32, u32), usize>::new();
    for &cpu in allowed {
        cores.entry(topology(cpu)?).or_insert(cpu);
    }
    Ok(cores.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_selection_deduplicates_smt_siblings_and_packages() {
        let allowed = [0, 1, 2, 3, 8, 9, 10, 11, 16, 20];
        let selected = select_one_cpu_per_core(&allowed, |cpu| {
            let topology = match cpu {
                0 | 8 => (0, 0),
                1 | 9 => (0, 1),
                2 | 10 => (0, 2),
                3 | 11 => (0, 3),
                16 => (1, 0),
                20 => (1, 1),
                _ => unreachable!(),
            };
            Ok(topology)
        })
        .expect("select topology");

        assert_eq!(selected, [0, 1, 2, 3, 16, 20]);
    }

    #[test]
    fn physical_selection_respects_a_restricted_allowed_mask() {
        let allowed = [8, 10];
        let selected = select_one_cpu_per_core(&allowed, |cpu| Ok((0, cpu as u32 - 8)))
            .expect("select topology");
        assert_eq!(selected, allowed);
    }
}
