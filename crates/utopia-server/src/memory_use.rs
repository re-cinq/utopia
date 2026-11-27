//! Container memory from cgroup v2, for the job worker's headroom gate.

use utopia_store::jobs::MemoryUse;

pub fn parse(memory_max: &str, memory_current: &str, memory_stat: &str) -> Option<MemoryUse> {
    let limit_bytes = memory_max.trim().parse::<u64>().ok()?;
    let current = memory_current.trim().parse::<u64>().ok()?;
    let inactive_file = memory_stat
        .lines()
        .map(|line| line.split_whitespace())
        .find_map(|mut tokens| (tokens.next() == Some("inactive_file")).then(|| tokens.next()))
        .flatten()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    Some(MemoryUse {
        working_set_bytes: current.saturating_sub(inactive_file),
        limit_bytes,
    })
}

pub fn read() -> Option<MemoryUse> {
    parse(
        &std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?,
        &std::fs::read_to_string("/sys/fs/cgroup/memory.current").ok()?,
        &std::fs::read_to_string("/sys/fs/cgroup/memory.stat").ok()?,
    )
}

#[cfg(test)]
#[path = "memory_use_tests.rs"]
mod memory_use_tests;
