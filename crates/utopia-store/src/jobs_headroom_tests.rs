//! Headroom gate: the worker claims another job only while the pool has free connections and memory is not close to the limit.

use super::{may_claim, Headroom, MemoryUse, PauseReason};

const MIB: u64 = 1024 * 1024;

fn headroom(free_connections: u32, memory: Option<(u64, u64)>) -> Headroom {
    Headroom {
        free_connections,
        memory: memory.map(|(working_set_bytes, limit_bytes)| MemoryUse {
            working_set_bytes,
            limit_bytes,
        }),
    }
}

#[test]
fn allows_with_2_free_connections_and_no_memory_limit() {
    assert_eq!(may_claim(headroom(2, None)), Ok(()));
}

#[test]
fn allows_at_exactly_85_percent_memory() {
    assert_eq!(may_claim(headroom(2, Some((850, 1000)))), Ok(()));
}

#[test]
fn pauses_on_connections_with_1_free() {
    assert_eq!(
        may_claim(headroom(1, None)),
        Err(PauseReason::Connections { free: 1 })
    );
}

#[test]
fn pauses_on_connections_with_0_free() {
    assert_eq!(
        may_claim(headroom(0, None)),
        Err(PauseReason::Connections { free: 0 })
    );
}

#[test]
fn pauses_on_memory_at_851_of_1000_bytes() {
    assert_eq!(
        may_claim(headroom(2, Some((851, 1000)))),
        Err(PauseReason::Memory(MemoryUse {
            working_set_bytes: 851,
            limit_bytes: 1000
        }))
    );
}

#[test]
fn reports_connections_when_1_free_and_memory_at_99_percent() {
    assert_eq!(
        may_claim(headroom(1, Some((990, 1000)))),
        Err(PauseReason::Connections { free: 1 })
    );
}

#[test]
fn allows_a_0_byte_limit_because_it_is_no_usable_limit() {
    assert_eq!(may_claim(headroom(2, Some((500, 0)))), Ok(()));
}

#[test]
fn pauses_at_1771_mib_of_a_2_gib_limit() {
    assert_eq!(
        may_claim(headroom(10, Some((1771 * MIB, 2048 * MIB)))),
        Err(PauseReason::Memory(MemoryUse {
            working_set_bytes: 1771 * MIB,
            limit_bytes: 2048 * MIB
        }))
    );
}

#[test]
fn allows_1042_mib_of_a_2_gib_limit() {
    assert_eq!(
        may_claim(headroom(10, Some((1042 * MIB, 2048 * MIB)))),
        Ok(())
    );
}
