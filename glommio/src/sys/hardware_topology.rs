//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/).
//! Copyright 2020 Datadog, Inc.
//!

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self},
    path::Path,
};

use super::sysfs::ListIterator;

/// A description of the CPU's location in the machine topology.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CpuLocation {
    /// Holds the CPU id.  This is the most granular field and will distinguish
    /// among [`hyper-threads`].
    ///
    /// [`hyper-threads`]: https://en.wikipedia.org/wiki/Hyper-threading
    pub cpu: usize,
    /// Holds the core id on which the `cpu` is located.
    pub core: usize,
    /// Holds the package or socket id on which the `cpu` is located.
    pub package: usize,
    /// Holds the NUMA node on which the `cpu` is located.
    pub numa_node: usize,
    /// Holds the last-level cache domain in which the `cpu` sits, i.e. the set
    /// of CPUs it shares an L3 with.
    ///
    /// This is a finer distinction than [`Self::package`] and on several
    /// current parts it is the one that matters. A single-socket AMD
    /// Threadripper reports one package and one NUMA node while physically
    /// having four L3 domains, and moving a cache line between two of them
    /// costs an order of magnitude more than keeping it inside one. Placement
    /// that only knows about packages and NUMA nodes cannot see that
    /// difference.
    ///
    /// Falls back to the package id when the kernel does not expose cache
    /// topology, which reproduces the previous behaviour.
    pub cache_domain: usize,
}

fn build_cpu_location(
    sysfs_path: &Path,
    cpu: usize,
    numa_node: usize,
    cpu_to_core: &mut HashMap<usize, usize>,
) -> io::Result<CpuLocation> {
    let cpu_path = sysfs_path.join(format!("cpu/cpu{cpu}/topology"));

    let package_id = ListIterator::from_path(&cpu_path.join("physical_package_id"))?
        .next()
        .ok_or_else(|| io::Error::other("failed to parse physical_package_id"))??;

    Ok(CpuLocation {
        cpu,
        core: get_core_id(cpu, &cpu_path, cpu_to_core)?,
        package: package_id,
        numa_node,
        // Provisional: replaced with a dense id below, or left as the package
        // id if this machine does not report cache topology.
        cache_domain: get_cache_domain_id(sysfs_path, cpu).unwrap_or(package_id),
    })
}

/// Identifies the last-level cache domain `cpu` belongs to.
///
/// Walks `cpu/cpuN/cache/index*`, takes the highest cache level reported, and
/// canonicalises the domain as the lowest CPU id sharing it -- so every CPU in
/// a domain derives the same value without any cross-CPU coordination. Returns
/// `None` when the machine does not expose cache topology at all (some
/// containers, some architectures), in which case the caller falls back to the
/// package.
fn get_cache_domain_id(sysfs_path: &Path, cpu: usize) -> Option<usize> {
    let cache_path = sysfs_path.join(format!("cpu/cpu{cpu}/cache"));
    let mut best: Option<(usize, usize)> = None;

    for entry in std::fs::read_dir(&cache_path).ok()? {
        let dir = entry.ok()?.path();
        if !dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("index"))
        {
            continue;
        }

        let level: usize = match std::fs::read_to_string(dir.join("level")) {
            Ok(s) => match s.trim().parse() {
                Ok(l) => l,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        // Instruction caches are never the sharing domain we care about.
        if matches!(
            std::fs::read_to_string(dir.join("type"))
                .as_deref()
                .map(str::trim),
            Ok("Instruction")
        ) {
            continue;
        }

        let Ok(shared) = ListIterator::from_path(&dir.join("shared_cpu_list")) else {
            continue;
        };
        let Some(min_cpu) = shared.filter_map(Result::ok).min() else {
            continue;
        };

        if best.is_none_or(|(best_level, _)| level > best_level) {
            best = Some((level, min_cpu));
        }
    }

    best.map(|(_, min_cpu)| min_cpu)
}

/// Request the machine topology.  Only CPUs that are currently `online`
/// according to `/sys/devices/system/cpu/online` are provided;  `sysfs` is
/// always at `/sys` per: https://www.kernel.org/doc/html/latest/admin-guide/sysfs-rules.html
pub fn get_machine_topology_unsorted() -> io::Result<Vec<CpuLocation>> {
    let sysfs_path = Path::new("/sys/devices/system");
    let mut cpus_online = HashSet::new();
    for cpu in ListIterator::from_path(&sysfs_path.join("cpu/online"))? {
        cpus_online.insert(cpu?);
    }
    let mut cpu_locations = Vec::new();
    let mut cpu_to_core = HashMap::new();

    let nodes_online = match ListIterator::from_path(&sysfs_path.join("node/online")) {
        Ok(x) => x,
        Err(x) => match x.kind() {
            io::ErrorKind::NotFound => {
                for cpu in cpus_online.drain() {
                    let cpu_location = build_cpu_location(sysfs_path, cpu, 0, &mut cpu_to_core)?;
                    cpu_locations.push(cpu_location);
                }
                return Ok(cpu_locations);
            }
            _ => {
                return Err(x);
            }
        },
    };

    for node in nodes_online {
        let node = node?;
        let node_path = sysfs_path.join(format!("node/node{node}"));
        let node_cpus = ListIterator::from_path(&node_path.join("cpulist"))?;
        for cpu in node_cpus {
            let cpu = cpu?;
            // only map CPUs that are online
            if !cpus_online.contains(&cpu) {
                continue;
            }

            let cpu_location = build_cpu_location(sysfs_path, cpu, node, &mut cpu_to_core)?;
            cpu_locations.push(cpu_location);
        }
    }

    // Assign a virtual core id to each CPU. The basic strategy is to sort CPUs
    // by their (NUMA node id, core id) and assign virtual core id in this order.
    // Note we need to ensure that the CPUs on the same core will have the same core
    // id.

    // Using BTree over HashMap for 2 reasons:
    // 1. to keep mapping consitent between different invocations.
    // 2. to assign smaller virtual core ids to smaller numa node id.
    // numa_node -> (core_id -> [cpu_id])
    let mut node_to_core_to_cpus: BTreeMap<usize, BTreeMap<usize, Vec<usize>>> = BTreeMap::new();
    for l in &cpu_locations {
        node_to_core_to_cpus
            .entry(l.numa_node)
            .or_default()
            .entry(l.core)
            .or_default()
            .push(l.cpu)
    }

    let mut cpu_to_vcore = HashMap::new();
    for (vcore, cpu_on_same_core) in node_to_core_to_cpus
        .values()
        .flat_map(BTreeMap::values)
        .enumerate()
    {
        for &cpu in cpu_on_same_core {
            cpu_to_vcore.insert(cpu, vcore);
        }
    }

    for cpu_location in &mut cpu_locations {
        cpu_location.core = *cpu_to_vcore.get(&cpu_location.cpu).unwrap();
    }

    // Densify cache domain ids the same way, and for the same reason: the
    // placement tree assumes ids at a level are unique and consecutive, and the
    // raw value so far is "lowest CPU sharing this cache", which is neither.
    // BTreeMap keeps the mapping stable across invocations.
    let raw_domains: std::collections::BTreeSet<usize> =
        cpu_locations.iter().map(|l| l.cache_domain).collect();
    let domain_to_dense: HashMap<usize, usize> = raw_domains
        .into_iter()
        .enumerate()
        .map(|(dense, raw)| (raw, dense))
        .collect();
    for cpu_location in &mut cpu_locations {
        cpu_location.cache_domain = domain_to_dense[&cpu_location.cache_domain];
    }

    Ok(cpu_locations)
}

fn get_core_id(
    cpu: usize,
    cpu_path: &Path,
    cpu_to_core: &mut HashMap<usize, usize>,
) -> io::Result<usize> {
    // `hwloc` suggests that some hardware assigns unique `core_id`s to each CPU
    // even though they are hyper-threads, so we ensure we have the same
    // `core_id` for all CPUs in `core_cpus_list` (`thread_siblings` is
    // deprecated in favor of `core_cpus`) see: https://github.com/open-mpi/hwloc/blob/3c8ed197d9a017ca5399007861981b60032e7ca6/hwloc/topology-linux.c#L4267
    let cpu_siblings = ListIterator::from_path(&cpu_path.join("core_cpus_list"))?;
    match cpu_to_core.get(&cpu) {
        Some(core) => Ok(*core),
        None => {
            let core = ListIterator::from_path(&cpu_path.join("core_id"))?
                .next()
                .transpose()?
                .ok_or_else(|| io::Error::other("failed to parse core_id"))?;
            for sibling in cpu_siblings {
                cpu_to_core.insert(sibling?, core);
            }
            Ok(core)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::{CpuLocation, HashMap};

    pub fn check_topolgy(mut topology: Vec<CpuLocation>) {
        // Check that we don't have a system where any hardware component has an id that
        // is not unique system-wide (e.g. both numa node 0 and 1 have a core
        // with id 0); this precondition is assumed throughout
        topology.sort_by_key(|l| (l.numa_node, l.package, l.core, l.cpu));

        let cpus = topology.into_iter();
        let mut cpu_to_core = HashMap::new();
        let mut core_to_pkg = HashMap::new();
        let mut core_to_numa = HashMap::new();
        let mut pkg_to_numa = Some(HashMap::new());
        let mut numa_to_pkg = Some(HashMap::new());

        for cpu in cpus {
            cpu_to_core
                .entry(cpu.cpu)
                .and_modify(|e| {
                    assert_eq!(
                        *e, cpu.core,
                        "cpu {} in cores {} and {}",
                        cpu.cpu, cpu.core, *e
                    )
                })
                .or_insert(cpu.core);

            core_to_pkg
                .entry(cpu.core)
                .and_modify(|e| {
                    assert_eq!(
                        *e, cpu.package,
                        "core {} in packages {} and {}",
                        cpu.core, cpu.package, *e
                    )
                })
                .or_insert(cpu.package);

            core_to_numa
                .entry(cpu.core)
                .and_modify(|e| {
                    assert_eq!(
                        *e, cpu.numa_node,
                        "core {} in numa_nodes {} and {}",
                        cpu.core, cpu.numa_node, *e
                    )
                })
                .or_insert(cpu.numa_node);

            let mut either = false;
            if let Some(ref mut map) = pkg_to_numa {
                if matches!(map.insert(cpu.package, cpu.numa_node), Some(n) if n != cpu.numa_node) {
                    pkg_to_numa = None;
                } else {
                    either = true;
                }
            }
            if let Some(ref mut map) = numa_to_pkg {
                if matches!(map.insert(cpu.numa_node, cpu.package), Some(p) if p != cpu.package) {
                    numa_to_pkg = None;
                } else {
                    either = true;
                }
            }

            assert!(
                either,
                "unsupported topology hierarchy: numa node {} and package {}",
                cpu.numa_node, cpu.package
            );
        }
    }

    /// A CPU on a machine that reports no cache topology, so the cache domain
    /// degenerates to the package. Existing tests use this, and their passing
    /// is what shows the new level changes nothing on such machines.
    pub fn cpu_loc(numa_node: usize, package: usize, core: usize, cpu: usize) -> CpuLocation {
        cpu_loc_cache(numa_node, package, package, core, cpu)
    }

    /// A CPU on a machine with several cache domains per package.
    pub fn cpu_loc_cache(
        numa_node: usize,
        package: usize,
        cache_domain: usize,
        core: usize,
        cpu: usize,
    ) -> CpuLocation {
        CpuLocation {
            cpu,
            core,
            package,
            numa_node,
            cache_domain,
        }
    }
}

#[cfg(test)]
mod test {
    use super::{test_helpers::*, *};

    #[test]
    fn machine_topology() {
        get_machine_topology_unsorted().unwrap();
    }

    #[test]
    fn topology_this_machine_unique_ids() {
        let topology = get_machine_topology_unsorted().unwrap();
        check_topolgy(topology)
    }

    #[test]
    #[should_panic(expected = "unsupported topology hierarchy")]
    fn check_topology_check() {
        // panic because topology level are unclear:
        // numa node 0 is associated with package 0 and 1
        // package 1 is associated with numa node 0 and 2
        let topology = vec![
            cpu_loc(0, 0, 0, 0),
            cpu_loc(0, 1, 1, 1),
            cpu_loc(2, 1, 2, 2),
        ];

        check_topolgy(topology);
    }
}

#[cfg(test)]
mod cache_domain_tests {
    use super::get_cache_domain_id;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    /// A throwaway sysfs tree.
    ///
    /// The parsing under test takes the sysfs root as an argument precisely so
    /// it can be pointed somewhere else, which is the only way to cover
    /// topologies this machine does not have.
    struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "glommio-topology-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create fixture root");
            FakeSysfs(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Adds one cache index for `cpu`, as sysfs would expose it.
        fn cache(&self, cpu: usize, index: usize, level: &str, kind: &str, shared: &str) -> &Self {
            let dir = self.0.join(format!("cpu/cpu{cpu}/cache/index{index}"));
            fs::create_dir_all(&dir).expect("create cache index");
            // Trailing newlines, because sysfs has them and the list parser
            // relies on it: `skip_delim` leaves the final byte alone so the
            // terminator can be checked. A fixture without one hangs.
            fs::write(dir.join("level"), format!("{level}\n")).expect("write level");
            fs::write(dir.join("type"), format!("{kind}\n")).expect("write type");
            fs::write(dir.join("shared_cpu_list"), format!("{shared}\n"))
                .expect("write shared_cpu_list");
            self
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_domain_is_the_lowest_cpu_sharing_the_deepest_cache() {
        let sysfs = FakeSysfs::new();
        // Two packages of four, sharing an L3 each.
        for cpu in 0..8 {
            let l3 = if cpu < 4 { "0-3" } else { "4-7" };
            sysfs
                .cache(cpu, 0, "1", "Data", &cpu.to_string())
                .cache(cpu, 2, "3", "Unified", l3);
        }

        for cpu in 0..4 {
            assert_eq!(get_cache_domain_id(sysfs.path(), cpu), Some(0), "cpu {cpu}");
        }
        for cpu in 4..8 {
            assert_eq!(get_cache_domain_id(sysfs.path(), cpu), Some(4), "cpu {cpu}");
        }
    }

    #[test]
    fn an_instruction_cache_is_not_a_sharing_domain() {
        let sysfs = FakeSysfs::new();
        // An instruction cache reported at a deeper level than the unified one
        // it shares a die with. Taking it would split a domain that is real.
        sysfs
            .cache(0, 0, "2", "Unified", "0-1")
            .cache(0, 1, "3", "Instruction", "0");

        assert_eq!(get_cache_domain_id(sysfs.path(), 0), Some(0));

        let sysfs = FakeSysfs::new();
        sysfs
            .cache(1, 0, "2", "Unified", "0-1")
            .cache(1, 1, "3", "Instruction", "1");

        assert_eq!(
            get_cache_domain_id(sysfs.path(), 1),
            Some(0),
            "the unified cache shared with cpu 0 decides the domain"
        );
    }

    #[test]
    fn a_machine_that_reports_no_cache_topology_has_no_domain() {
        let sysfs = FakeSysfs::new();
        // Some containers and some architectures expose no cache directory at
        // all. The caller falls back to the package id.
        assert_eq!(get_cache_domain_id(sysfs.path(), 0), None);
    }

    #[test]
    fn an_unreadable_level_does_not_discard_the_rest() {
        let sysfs = FakeSysfs::new();
        sysfs
            .cache(0, 0, "2", "Unified", "0-1")
            .cache(0, 1, "not a number", "Unified", "0-7");

        assert_eq!(
            get_cache_domain_id(sysfs.path(), 0),
            Some(0),
            "the index that parsed still decides"
        );
    }
}
