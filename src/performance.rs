use std::collections::BTreeMap;
use std::io;

use crate::config::GraphicsOptimizationMode;

const MIN_PHYSICAL_CORES: usize = 8;

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

fn physical_core_plan() -> io::Result<Option<(Vec<usize>, Vec<usize>)>> {
    let allowed = current_affinity()?;
    let selected = select_one_cpu_per_core(&allowed, linux_cpu_topology)?;
    if selected.len() < MIN_PHYSICAL_CORES || selected.len() == allowed.len() {
        return Ok(None);
    }
    Ok(Some((allowed, selected)))
}

fn current_affinity() -> io::Result<Vec<usize>> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };

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

fn set_current_affinity(cpus: &[usize]) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &cpu in cpus {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("CPU {cpu} exceeds CPU_SETSIZE"),
            ));
        }

        unsafe { libc::CPU_SET(cpu, &mut set) };
    }

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
