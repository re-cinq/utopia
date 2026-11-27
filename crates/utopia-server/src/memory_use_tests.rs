//! The cgroup v2 reading the worker's headroom gate runs on: working set = `memory.current` minus `inactive_file`, the figure Kubernetes kills on.

use super::parse;
use utopia_store::jobs::MemoryUse;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn memory_stat(inactive_file_line: &str) -> String {
    format!(
        "anon 1234\nfile 5678\nkernel_stack 16384\nslab 2048\nsock 0\n{inactive_file_line}\nactive_anon 4096\nworkingset_refault_file 11\n"
    )
}

#[test]
fn working_set_is_1_9_gib_minus_100_mib_of_inactive_file_under_a_2_gib_limit() {
    let current = GIB * 19 / 10;
    assert_eq!(
        parse(
            "2147483648\n",
            &format!("{current}\n"),
            &memory_stat("inactive_file 104857600")
        ),
        Some(MemoryUse {
            working_set_bytes: current - 100 * MIB,
            limit_bytes: 2 * GIB
        })
    );
}

#[test]
fn surrounding_whitespace_and_trailing_newlines_are_accepted() {
    assert_eq!(
        parse(
            "  2147483648 \n\n",
            "\t1073741824\n",
            "anon 1\n  inactive_file   104857600  \n\n"
        ),
        Some(MemoryUse {
            working_set_bytes: GIB - 100 * MIB,
            limit_bytes: 2 * GIB
        })
    );
}

#[test]
fn no_gate_when_memory_max_is_max() {
    assert_eq!(
        parse("max\n", "1073741824\n", &memory_stat("inactive_file 1")),
        None
    );
}

#[test]
fn no_gate_when_memory_max_is_empty() {
    assert_eq!(
        parse("", "1073741824\n", &memory_stat("inactive_file 1")),
        None
    );
}

#[test]
fn no_gate_when_memory_max_is_not_a_number() {
    assert_eq!(
        parse("lots\n", "1073741824\n", &memory_stat("inactive_file 1")),
        None
    );
}

#[test]
fn no_gate_when_memory_current_is_not_a_number() {
    assert_eq!(
        parse("2147483648\n", "plenty\n", &memory_stat("inactive_file 1")),
        None
    );
}

#[test]
fn working_set_is_memory_current_when_memory_stat_has_no_inactive_file_line() {
    assert_eq!(
        parse("2147483648\n", "1073741824\n", "anon 1234\nfile 5678\n"),
        Some(MemoryUse {
            working_set_bytes: GIB,
            limit_bytes: 2 * GIB
        })
    );
}

#[test]
fn a_non_numeric_inactive_file_counts_as_0() {
    assert_eq!(
        parse(
            "2147483648\n",
            "1073741824\n",
            &memory_stat("inactive_file many")
        ),
        Some(MemoryUse {
            working_set_bytes: GIB,
            limit_bytes: 2 * GIB
        })
    );
}

#[test]
fn working_set_is_0_when_inactive_file_exceeds_memory_current() {
    assert_eq!(
        parse(
            "2147483648\n",
            "104857600\n",
            &memory_stat("inactive_file 209715200")
        ),
        Some(MemoryUse {
            working_set_bytes: 0,
            limit_bytes: 2 * GIB
        })
    );
}

#[test]
fn lines_that_merely_contain_inactive_file_are_not_mistaken_for_it() {
    assert_eq!(
        parse(
            "2147483648\n",
            "1073741824\n",
            "inactive_file_extra 5\nworkingset_inactive_file 7\nactive_inactive_file 9\n"
        ),
        Some(MemoryUse {
            working_set_bytes: GIB,
            limit_bytes: 2 * GIB
        })
    );
}
