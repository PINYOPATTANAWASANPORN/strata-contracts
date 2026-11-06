#!/usr/bin/env bash

# Strata x Blend deploy — TESTNET ONLY. UNAUDITED.
#
# > **STATUS: run against Stellar testnet on 2026-11-05**, with Stellar CLI
# > 28.1.0 and soroban-sdk 27.0.6. It produced adapter
# > `CBJIQYHM5SIXY3QMT7X6ARQUPUWSNH3EZ5T3JOILETKBXR6SPWFKAKXI` and manager
# > `CALMXJCAKZALDPYFFBWQNUSBAHUM7HIL5NSITX7FYUEUSYN4Y4CSFPVT`; the record is
# > `deployments/testnet-blend.json` and the settled epoch is written up in
# > `docs/deployment.md`. The adapter it deployed predates `refresh()`, so a run
# > from the current tree produces a different Wasm hash (`dbf5a1f4…`).
# >
# > "Run" is not "audited": nothing here has been reviewed by a third party,
# > and the Blend pool is somebody else's contract.
#
# Refuses any network but `testnet`. The real Blend pool exists only on testnet,
# and the adapter's pool address is a fixed constant: a "local" or "futurenet"
# deployment would point at an address that is not a pool there. Mainnet is not
# a supported path for this project, by design.
#
# Usage:
#   ./scripts/deploy-testnet-blend.sh <identity-name> [testnet] [--force]
#   ./scripts/deploy-testnet-blend.sh --self-test
#   ./scripts/deploy-testnet-blend.sh --help
#
# `<identity-name>` is a Stellar CLI identity (`stellar keys ls`), not a key
# file. It becomes the epoch manager's admin and pays for both deployments.
#
# `--force` overwrites an existing `deployments/testnet-blend.json`. Without it
# the script refuses, because a fresh run deploys NEW contract IDs and silently
# replacing the record would leave the old contracts live but unrecorded.
#
# Why a salt: the adapter takes the manager's address in its constructor, and the
# manager takes the adapter's address in its. The manager's constructor also
# calls `vault.asset()`, so the adapter must exist first. The script therefore
# derives the manager's future contract id from `--salt` before either deploy,
# passes that id to the adapter, then deploys the manager with the same salt and
# asserts the resulting id is the one the adapter was built with.

set -euo pipefail

# One entry, deliberately. Adding another network is a reviewed change to this
# list, not a flag.
ALLOWED_NETWORKS=("testnet")

POOL_ID="CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF"
RESERVE_INDEX=0

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

WASM_DIR="target/wasm32v1-none/release"
RECORD="deployments/testnet-blend.json"

die() {
    echo "error: $*" >&2
    exit 1
}

say() {
    echo "==> $*"
}

assert_network_is_allowed() {
    local network="$1"
    for candidate in "${ALLOWED_NETWORKS[@]}"; do
        if [ "$network" = "$candidate" ]; then
            return 0
        fi
    done
    return 1
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "no sha256sum or shasum on PATH; cannot hash the wasm artefacts"
    fi
}

usage() {
    cat <<'EOF'
usage: ./scripts/deploy-testnet-blend.sh <identity-name> [testnet] [--force]
       ./scripts/deploy-testnet-blend.sh --self-test

Deploys a fresh blend-adapter and a new epoch-manager against the real testnet
Blend pool, then writes deployments/testnet-blend.json. Testnet only.

  --force     overwrite an existing deployments/testnet-blend.json
  --self-test check the network guard and hashing without touching a network
EOF
}

if [ "${1:-}" = "--self-test" ]; then
    assert_network_is_allowed testnet || {
        echo "FAIL: testnet was refused"
        exit 1
    }
    echo "ok: testnet allowed"
    for bad in mainnet public "Main Net" "" futurenet local testnetX; do
        if assert_network_is_allowed "$bad"; then
            echo "FAIL: accepted '$bad'"
            exit 1
        fi
        echo "ok: '$bad' refused"
    done

    tmp_hash_probe="$(mktemp)"
    echo "probe" >"$tmp_hash_probe"
    probe_hash="$(sha256_of "$tmp_hash_probe")"
    rm -f "$tmp_hash_probe"
    [ ${#probe_hash} -eq 64 ] || {
        echo "FAIL: sha256_of returned '$probe_hash'"
        exit 1
    }
    echo "ok: sha256_of works"
    echo "network guard behaves correctly"
    exit 0
fi

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    usage
    exit 0
fi

FORCE=""
if [ "${3:-}" = "--force" ]; then
    FORCE="--force"
elif [ -n "${3:-}" ]; then
    die "unexpected argument '$3'. See --help."
fi

IDENTITY="${1:-}"
NETWORK="${2:-testnet}"

# The guard runs before the identity check and before anything touches the
# filesystem, so `mainnet` fails on the network rather than on a missing key.
assert_network_is_allowed "$NETWORK" || die "network '$NETWORK' is not allowed.
Strata's Blend integration is testnet-only: the pool address is a fixed testnet
constant and there is no mainnet path. Allowed: ${ALLOWED_NETWORKS[*]}"

[ -n "$IDENTITY" ] || {
    usage
    die "an identity is required"
}

command -v stellar >/dev/null 2>&1 ||
    die "stellar CLI not found. See docs/deployment.md."

stellar keys public-key "$IDENTITY" >/dev/null 2>&1 ||
    die "no such identity '$IDENTITY'. Create one with:
  stellar keys generate --network testnet --fund $IDENTITY"

if [ -e "$RECORD" ] && [ -z "$FORCE" ]; then
    die "$RECORD already exists.
A fresh run deploys NEW contract ids, so the existing record would be stale.
Review it, then re-run with --force to deploy and overwrite it."
fi

# --- output parsing ----------------------------------------------------------

extract_contract_id() {
    # Case-sensitive: a wasm hash is 64 lowercase hex and can contain a `c`
    # followed by characters that look like a contract id. Anchoring the shape
    # keeps a log line from being recorded as an address.
    grep -oE '\bC[0-9A-Z]{55}\b' | tail -n 1 || true
}

extract_tx_hashes() {
    # Everything that looks like a transaction hash except the wasm hash, which
    # the CLI also prints and which is not a transaction. Deduplicated, in order.
    grep -oE '\b[0-9a-f]{64}\b' | grep -v "$1" | awk '!seen[$0]++' || true
}

extract_address() {
    grep -oE '\b[CG][A-Z0-9]{55}\b' | tail -n 1 || true
}

json_field() {
    printf '%s' "$1" |
        grep -oE "\"$2\":\"?[0-9]+\"?" | grep -oE '[0-9]+' | head -n 1 || true
}

json_list() {
    local first=1 item
    printf '['
    while IFS= read -r item; do
        [ -n "$item" ] || continue
        if [ $first -eq 0 ]; then
            printf ', '
        fi
        printf '"%s"' "$item"
        first=0
    done
    printf ']'
}

find_wasm() {
    local marker="$1" candidate
    local found=()
    for candidate in "$WASM_DIR"/*"$marker"*.wasm; do
        [ -f "$candidate" ] && found+=("$candidate")
    done
    [ "${#found[@]}" -gt 0 ] || return 1
    printf '%s\n' "${found[0]}"
}

read_sdk_version() {
    sed -n 's/^soroban-sdk[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml |
        head -n 1
}

read_passphrase() {
    stellar network ls --long |
        awk -v want="Name: $1" '
            $0 == want { found = 1; next }
            found && /^Network passphrase: / {
                sub(/^Network passphrase: /, "")
                print
                exit
            }
        '
}

onchain_hash() {
    stellar contract info hash --id "$1" --network "$NETWORK" 2>/dev/null |
        grep -oE '\b[0-9a-f]{64}\b' | tail -n 1 || true
}

# A read-only invoke. Never fatal on its own; the caller decides.
read_view() {
    local id="$1" fn="$2"
    shift 2
    stellar contract invoke \
        --id "$id" \
        --source-account "$IDENTITY" \
        --network "$NETWORK" \
        --no-cache \
        --send=no \
        -- "$fn" "$@" 2>&1 || true
}

# --- build -------------------------------------------------------------------

say "building contracts"
stellar contract build

ADAPTER_WASM=$(find_wasm blend_adapter) ||
    die "no wasm matching *blend_adapter* in $WASM_DIR"
MANAGER_WASM=$(find_wasm epoch_manager) ||
    die "no wasm matching *epoch_manager* in $WASM_DIR"

ADAPTER_WASM_HASH=$(sha256_of "$ADAPTER_WASM")
MANAGER_WASM_HASH=$(sha256_of "$MANAGER_WASM")

for wasm in "$ADAPTER_WASM" "$MANAGER_WASM"; do
    say "$wasm ($(wc -c <"$wasm" | tr -d ' ') bytes)"
done

CLI_VERSION=$(stellar --version | head -n 1 | awk '{print $2}')
SDK_VERSION=$(read_sdk_version)
PASSPHRASE=$(read_passphrase "$NETWORK")

[ -n "$CLI_VERSION" ] || die "could not determine the Stellar CLI version"
[ -n "$SDK_VERSION" ] || die "could not read soroban-sdk from Cargo.toml"
[ -n "$PASSPHRASE" ] || die "could not read the passphrase for '$NETWORK'"

say "stellar CLI $CLI_VERSION, soroban-sdk $SDK_VERSION, network $NETWORK, UNAUDITED"

ADMIN=$(stellar keys public-key "$IDENTITY")
say "admin/deployer: $ADMIN"

# --- the pool and its reserve ------------------------------------------------

# The adapter's asset is the native XLM SAC, the same asset the pool's index-0
# reserve lists. Derive it from the network rather than writing it down.
ASSET_ID=$(stellar contract id asset --asset native --network "$NETWORK")
[ -n "$ASSET_ID" ] || die "could not determine the native XLM SAC id"
say "pool   $POOL_ID"
say "asset  $ASSET_ID (native XLM SAC)"

say "checking the pool lists the asset (a wrong asset makes the adapter trap)"
POOL_RESERVE=$(read_view "$POOL_ID" get_reserve --asset "$ASSET_ID")
printf '%s' "$POOL_RESERVE" | grep -q '"config"' ||
    die "the pool did not return a reserve for $ASSET_ID:
$POOL_RESERVE"
POOL_B_RATE=$(json_field "$POOL_RESERVE" b_rate)
POOL_B_SUPPLY=$(json_field "$POOL_RESERVE" b_supply)
[ -n "$POOL_B_RATE" ] && [ -n "$POOL_B_SUPPLY" ] ||
    die "could not read b_rate/b_supply from the reserve: $POOL_RESERVE"
say "reserve index $RESERVE_INDEX: b_rate $POOL_B_RATE, b_supply $POOL_B_SUPPLY"

# --- manager id from the salt ------------------------------------------------

SALT=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
[ ${#SALT} -eq 64 ] || die "could not generate a 32-byte salt (got '$SALT')"

MANAGER_PREDICTED=$(stellar contract id wasm \
    --salt "$SALT" \
    --source-account "$IDENTITY" \
    --network "$NETWORK" 2>&1 | extract_contract_id)
printf '%s' "$MANAGER_PREDICTED" | grep -qE '^C[0-9A-Z]{55}$' ||
    die "could not derive the manager id from salt $SALT:
$MANAGER_PREDICTED"
say "manager id derived from salt: $MANAGER_PREDICTED"

# --- deploy the adapter ------------------------------------------------------

say "deploying blend-adapter"
ADAPTER_OUT=$(stellar contract deploy \
    --wasm "$ADAPTER_WASM" \
    --source-account "$IDENTITY" \
    --network "$NETWORK" \
    --no-cache \
    --alias "strata-blend-adapter" \
    -- \
    --pool "$POOL_ID" \
    --asset "$ASSET_ID" \
    --manager "$MANAGER_PREDICTED" 2>&1)
ADAPTER_ID=$(printf '%s\n' "$ADAPTER_OUT" | extract_contract_id)
[ -n "$ADAPTER_ID" ] || {
    printf '%s\n' "$ADAPTER_OUT" >&2
    die "could not determine the adapter contract id"
}
ADAPTER_TXS=$(printf '%s\n' "$ADAPTER_OUT" | extract_tx_hashes "$ADAPTER_WASM_HASH")
[ -n "$ADAPTER_TXS" ] || die "the adapter deployment produced no transaction hash"
say "adapter: $ADAPTER_ID"

# --- deploy the manager at the predicted id ----------------------------------

say "deploying a new epoch-manager at the predicated id"
MANAGER_OUT=$(stellar contract deploy \
    --wasm "$MANAGER_WASM" \
    --source-account "$IDENTITY" \
    --network "$NETWORK" \
    --no-cache \
    --alias "strata-blend-epoch-manager" \
    --salt "$SALT" \
    -- \
    --admin "$ADMIN" \
    --vault "$ADAPTER_ID" 2>&1)
MANAGER_ID=$(printf '%s\n' "$MANAGER_OUT" | extract_contract_id)
[ -n "$MANAGER_ID" ] || {
    printf '%s\n' "$MANAGER_OUT" >&2
    die "could not determine the manager contract id. The adapter $ADAPTER_ID is
deployed and points at $MANAGER_PREDICTED, which may not exist. Do not record
this run."
}
MANAGER_TXS=$(printf '%s\n' "$MANAGER_OUT" | extract_tx_hashes "$MANAGER_WASM_HASH")
[ -n "$MANAGER_TXS" ] || die "the manager deployment produced no transaction hash"

[ "$MANAGER_ID" = "$MANAGER_PREDICTED" ] || die "the deployed manager id is
$MANAGER_ID but the adapter was built with $MANAGER_PREDICTED. The adapter
cannot reach its manager; do not use this deployment."
say "manager: $MANAGER_ID (matches the salt-derived id)"

# --- read back every constructor link ---------------------------------------

say "checking the on-chain wiring"
check_view() {
    local label="$1" got="$2" want="$3"
    [ "$got" = "$want" ] || die "$label is '$got', expected '$want'.
The deployment is live but not wired as intended; do not record this run."
    say "  ok $label = $want"
}

check_view "adapter.pool()" "$(read_view "$ADAPTER_ID" pool | extract_address)" "$POOL_ID"
check_view "adapter.asset()" "$(read_view "$ADAPTER_ID" asset | extract_address)" "$ASSET_ID"
check_view "adapter.manager()" "$(read_view "$ADAPTER_ID" manager | extract_address)" "$MANAGER_ID"
check_view "manager.vault()" "$(read_view "$MANAGER_ID" vault | extract_address)" "$ADAPTER_ID"
check_view "manager.asset()" "$(read_view "$MANAGER_ID" asset | extract_address)" "$ASSET_ID"
check_view "manager.admin()" "$(read_view "$MANAGER_ID" admin | extract_address)" "$ADMIN"

# --- wasm hashes -------------------------------------------------------------

say "comparing on-chain wasm hashes with the local artefacts"
ADAPTER_ONCHAIN=$(onchain_hash "$ADAPTER_ID")
MANAGER_ONCHAIN=$(onchain_hash "$MANAGER_ID")
[ "$ADAPTER_ONCHAIN" = "$ADAPTER_WASM_HASH" ] || die "adapter wasm hash mismatch:
  on-chain: $ADAPTER_ONCHAIN
  local:    $ADAPTER_WASM_HASH"
[ "$MANAGER_ONCHAIN" = "$MANAGER_WASM_HASH" ] || die "manager wasm hash mismatch:
  on-chain: $MANAGER_ONCHAIN
  local:    $MANAGER_WASM_HASH"
say "  ok adapter $ADAPTER_ONCHAIN"
say "  ok manager $MANAGER_ONCHAIN"

# --- record ------------------------------------------------------------------

mkdir -p "$(dirname "$RECORD")"
DEPLOYED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)

{
    cat <<EOF
{
  "network": "$NETWORK",
  "network_passphrase": "$PASSPHRASE",
  "deployed_at_utc": "$DEPLOYED_AT",
  "stellar_cli_version": "$CLI_VERSION",
  "soroban_sdk_version": "$SDK_VERSION",
  "admin_public_address": "$ADMIN",
  "audited": false,
  "warning": "UNAUDITED TESTNET SOFTWARE. Do not use real funds.",
  "blend_pool": {
    "contract_id": "$POOL_ID",
    "reserve_index": $RESERVE_INDEX,
    "asset_contract_id": "$ASSET_ID",
    "b_rate_at_deploy": "$POOL_B_RATE",
    "b_supply_at_deploy": "$POOL_B_SUPPLY"
  },
  "manager_salt": "$SALT",
  "contracts": {
    "blend_adapter": {
      "contract_id": "$ADAPTER_ID",
      "wasm_sha256": "$ADAPTER_WASM_HASH",
      "wasm_bytes": $(wc -c <"$ADAPTER_WASM" | tr -d ' '),
      "deployment_txs": $(printf '%s\n' "$ADAPTER_TXS" | json_list),
      "pool": "$POOL_ID",
      "asset": "$ASSET_ID",
      "manager": "$MANAGER_ID"
    },
    "epoch_manager": {
      "contract_id": "$MANAGER_ID",
      "wasm_sha256": "$MANAGER_WASM_HASH",
      "wasm_bytes": $(wc -c <"$MANAGER_WASM" | tr -d ' '),
      "deployment_txs": $(printf '%s\n' "$MANAGER_TXS" | json_list),
      "vault": "$ADAPTER_ID",
      "asset": "$ASSET_ID",
      "admin": "$ADMIN"
    }
  }
}
EOF
} >"$RECORD"

say "wrote $RECORD"

cat <<EOF

Deployed to $NETWORK at $DEPLOYED_AT. UNAUDITED — do not use real funds.

  adapter $ADAPTER_ID
  manager $MANAGER_ID
  pool    $POOL_ID
  asset   $ASSET_ID

The adapter's manager is the new epoch manager, not the mock-vault manager. The
existing deployments/testnet.json is untouched.

Next, open and settle one small epoch (deposits close at maturity, and the
senior tranche is gated on junior capital, so deposit the junior first):

  export STRATA_BLEND_ADAPTER=$ADAPTER_ID
  export STRATA_BLEND_MANAGER=$MANAGER_ID
  export STRATA_BLEND_POOL=$POOL_ID
  export STRATA_ASSET=$ASSET_ID

  stellar contract invoke --id \$STRATA_BLEND_MANAGER --source-account $IDENTITY \\
    --network $NETWORK --no-cache -- create_epoch \\
    --term_seconds 300 --rate_bps 500 --max_senior_ratio_bps 10000

  stellar contract invoke --id \$STRATA_BLEND_MANAGER --source-account <junior> \\
    --network $NETWORK --no-cache -- deposit \\
    --from <junior> --tranche Junior --amount 10000000

  stellar contract invoke --id \$STRATA_BLEND_MANAGER --source-account <senior> \\
    --network $NETWORK --no-cache -- deposit \\
    --from <senior> --tranche Senior --amount 10000000

  # after maturity
  stellar contract invoke --id \$STRATA_BLEND_MANAGER --source-account $IDENTITY \\
    --network $NETWORK --no-cache -- settle
EOF
