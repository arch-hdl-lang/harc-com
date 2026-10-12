#!/usr/bin/env bash
# Shared zero-based fixture sharding. Defaults preserve full local sweeps.
# Set SHARD_INDEX=0..N-1 and SHARD_COUNT=N to partition active registry rows.
fixture_shard_init() {
    SHARD_INDEX="${SHARD_INDEX-0}"
    SHARD_COUNT="${SHARD_COUNT-1}"
    # Canonical decimal integers avoid octal interpretation and overflow.
    if [[ ! "$SHARD_INDEX" =~ ^(0|[1-9][0-9]{0,8})$ ]] ||
       [[ ! "$SHARD_COUNT" =~ ^[1-9][0-9]{0,8}$ ]] ||
       (( SHARD_INDEX >= SHARD_COUNT )); then
        echo "error: require SHARD_COUNT >= 1 and 0 <= SHARD_INDEX < SHARD_COUNT (decimal integers)" >&2
        return 1
    fi
}

fixture_shard_contains() {
    (( $1 % SHARD_COUNT == SHARD_INDEX ))
}
