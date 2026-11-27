//! The headroom object on the system settings: what the worker's claiming gate sees, as an operator reads it.
use super::headroom_json;
use serde_json::json;
use utopia_store::jobs::{Headroom, MemoryUse};

const MIB: u64 = 1024 * 1024;
const LIMIT_BYTES: u64 = 2048 * MIB;

fn headroom(free_connections: u32, working_set_mib: Option<u64>) -> Headroom {
    Headroom {
        free_connections,
        memory: working_set_mib.map(|mib| MemoryUse {
            working_set_bytes: mib * MIB,
            limit_bytes: LIMIT_BYTES,
        }),
    }
}

#[test]
fn claiming_ok_with_the_exact_numbers_when_connections_and_memory_have_room() {
    assert_eq!(
        headroom_json(32, headroom(30, Some(1042))),
        json!({
            "max_connections": 32,
            "free_connections": 30,
            "memory": { "working_set_bytes": 1042 * MIB, "limit_bytes": LIMIT_BYTES },
            "claiming": "ok"
        })
    );
}

#[test]
fn memory_is_null_and_claiming_ok_when_no_limit_is_known() {
    assert_eq!(
        headroom_json(32, headroom(30, None)),
        json!({
            "max_connections": 32,
            "free_connections": 30,
            "memory": null,
            "claiming": "ok"
        })
    );
}

#[test]
fn claiming_paused_for_connections_when_one_connection_is_free() {
    assert_eq!(
        headroom_json(32, headroom(1, Some(1042))),
        json!({
            "max_connections": 32,
            "free_connections": 1,
            "memory": { "working_set_bytes": 1042 * MIB, "limit_bytes": LIMIT_BYTES },
            "claiming": "paused: connections"
        })
    );
}

#[test]
fn claiming_paused_for_memory_when_1771_mib_of_2_gib_is_in_use() {
    assert_eq!(
        headroom_json(32, headroom(30, Some(1771))),
        json!({
            "max_connections": 32,
            "free_connections": 30,
            "memory": { "working_set_bytes": 1771 * MIB, "limit_bytes": LIMIT_BYTES },
            "claiming": "paused: memory"
        })
    );
}

#[test]
fn claiming_paused_for_connections_when_connections_and_memory_are_both_short() {
    assert_eq!(
        headroom_json(32, headroom(1, Some(1771))),
        json!({
            "max_connections": 32,
            "free_connections": 1,
            "memory": { "working_set_bytes": 1771 * MIB, "limit_bytes": LIMIT_BYTES },
            "claiming": "paused: connections"
        })
    );
}
