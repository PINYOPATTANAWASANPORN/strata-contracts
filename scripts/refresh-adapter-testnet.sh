#!/usr/bin/env bash

# Strata Blend adapter position refresh — TESTNET ONLY. UNAUDITED.
#
# Calls the adapter's permissionless `refresh()` and reports the pool's
# position entry's live-until ledger before and after, read from the network.
#
# > **STATUS: run against Stellar testnet on 2026-11-05.** The call succeeded
# > from an identity that is not the adapter's manager, and `last_refreshed`
# > moved. The pool entry's live-until ledger did **not** move: 7 642 944
# > before and after. The entry still had ~120 days of life, above Blend's
# > ~100-day extension threshold, so the read was a no-op. This script exits 3
# > in that case, and it is not evidence that refresh fails; it is evidence
# > that the entry was too fresh for the test to observe an extension.
#
# Usage:
#   ./scripts/refresh-adapter-testnet.sh <identity> <adapter-id> [network]
#   ./scripts/refresh-adapter-testnet.sh --self-test
#
# <identity> pays the fee and signs the call. It does not have to be the
# adapter's manager: `refresh()` has no authorisation requirement.
#
# Exit codes:
#   0  the live-until ledger moved forward
#   1  usage, network, tooling or read failure
#   2  network refused by the guard
#   3  the call succeeded but the live-until ledger did not move
#
# This is not the TTL audit and it does not close risk B4. It measures one
# entry at one moment.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

say() { printf '%s\n' "$*"; }
step() { say ""; say "==> $*"; }
die() {
    say "error: $*" >&2
    exit 1
}

# Testnet only. A refresh is harmless anywhere, but this project has no
# mainnet code path and this script is not where one gets added.
assert_network_is_testnet() {
    [ "${1:-}" = "testnet" ]
}

# Reads `liveUntilLedgerSeq` out of `stellar ledger entry fetch` JSON.
parse_live_until() {
    grep -oE '"liveUntilLedgerSeq"[[:space:]]*:[[:space:]]*[0-9]+' |
        grep -oE '[0-9]+$' | head -n 1 || true
}

# Blend stores a user's position under `PoolDataKey::Positions(address)`, a
# contracttype enum encoded as a vector of [variant symbol, address].
position_key_xdr() {
    printf '{"vec":[{"symbol":"Positions"},{"address":"%s"}]}' "$1" |
        stellar xdr encode --type ScVal --output single-base64
}

if [ "${1:-}" = "--self-test" ]; then
    assert_network_is_testnet testnet || {
        echo "FAIL: rejected testnet"
        exit 1
    }
    echo "ok: testnet allowed"
    for bad in mainnet public futurenet local "" "testnetX"; do
        if assert_network_is_testnet "$bad"; then
            echo "FAIL: accepted '$bad'"
            exit 1
        fi
        echo "ok: '$bad' refused"
    done

    sample='{"entries":[{"liveUntilLedgerSeq": 7642944, "latestLedger": 1}]}'
    parsed=$(printf '%s\n' "$sample" | parse_live_until)
    if [ "$parsed" != "7642944" ]; then
        echo "FAIL: parse_live_until returned '$parsed'"
        exit 1
    fi
    echo "ok: parse_live_until reads the ledger"

    if command -v stellar >/dev/null 2>&1; then
        key=$(position_key_xdr GC6A5FQOXIVRYYOQACUVFIK4BUQCC3LCR75XKJFQOD5HFL6IWD3MVHRS)
        if [ -z "$key" ]; then
            echo "FAIL: position_key_xdr returned nothing"
            exit 1
        fi
        echo "ok: position_key_xdr encodes a key"
    else
        echo "ok: position_key_xdr skipped (no stellar CLI on PATH)"
    fi
    echo "self-test passed"
    exit 0
fi

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    sed -n '3,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
fi

IDENTITY="${1:-}"
ADAPTER="${2:-}"
NETWORK="${3:-testnet}"

assert_network_is_testnet "$NETWORK" || {
    say "error: network '$NETWORK' is not allowed.
Strata is unaudited and testnet-only. This script accepts only 'testnet'." >&2
    exit 2
}

[ -n "$IDENTITY" ] && [ -n "$ADAPTER" ] || {
    die "usage: $0 <identity> <adapter-id> [network]"
}
command -v stellar >/dev/null 2>&1 || die "stellar CLI not found"
stellar keys public-key "$IDENTITY" >/dev/null 2>&1 ||
    die "no such identity '$IDENTITY' in the Stellar CLI store"

step "reading the adapter's pool"
POOL=$(stellar contract invoke \
    --id "$ADAPTER" \
    --source-account "$IDENTITY" \
    --network "$NETWORK" \
    --no-cache \
    -- pool 2>/dev/null | tail -n 1)
[ -n "$POOL" ] || die "could not read pool() from $ADAPTER"

KEY_XDR=$(position_key_xdr "$ADAPTER")
[ -n "$KEY_XDR" ] || die "could not encode the pool's Positions key"

fetch_live_until() {
    stellar ledger entry fetch contract-data \
        --contract "$POOL" \
        --key-xdr "$KEY_XDR" \
        --network "$NETWORK" \
        --output json-formatted 2>/dev/null |
        parse_live_until
}

say "  adapter $ADAPTER"
say "  pool    $POOL"

step "live-until before"
BEFORE=$(fetch_live_until)
[ -n "$BEFORE" ] ||
    die "could not read the pool's position entry. Has the adapter deposited?"
say "  $BEFORE"

step "refresh (permissionless)"
OUT=$(stellar contract invoke \
    --id "$ADAPTER" \
    --source-account "$IDENTITY" \
    --network "$NETWORK" \
    --no-cache \
    -- refresh 2>&1) || {
    printf '%s\n' "$OUT" >&2
    die "refresh call failed"
}
REFRESHED_AT=$(printf '%s\n' "$OUT" | tail -n 1)
TX=$(printf '%s\n' "$OUT" | grep -oE '\b[0-9a-f]{64}\b' | tail -n 1 || true)
say "  last_refreshed: ${REFRESHED_AT:-unknown}"
say "  transaction:    ${TX:-unknown}"

step "live-until after"
AFTER=$(fetch_live_until)
[ -n "$AFTER" ] || die "could not read the pool's position entry after refresh"
say "  $AFTER"

if [ "$AFTER" -gt "$BEFORE" ]; then
    step "result"
    say "The entry moved forward by $((AFTER - BEFORE)) ledgers."
    exit 0
fi

step "result"
say "refresh succeeded, but the live-until ledger did not move: $BEFORE -> $AFTER."
say ""
say "This is the expected result while the entry is more than ~100 days from"
say "expiry. Blend only extends a user entry when its remaining life is below"
say "its threshold, so a freshly created position is already at the ceiling."
say "It is not proof that refresh() extends the entry, and this script does not"
say "claim it. To observe an extension, run it against a position that is"
say "within ~100 days of expiry."
exit 3
