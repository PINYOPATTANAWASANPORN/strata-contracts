#!/usr/bin/env bash

# Strata deployment verifier — TESTNET ONLY. UNAUDITED.
#
# Checks every deployment record in `deployments/` against the Stellar testnet
# ledger. A record is any `deployments/testnet*.json` that lists contracts:
#
#   deployments/testnet.json              mock-vault + epoch-manager
#   deployments/testnet-blend*.json       Blend adapter deployments
#
# For every contract listed in a record it checks that the contract exists on
# chain (exit 2 if not), that the ledger's Wasm hash equals the recorded
# `wasm_sha256` (exit 3 if not), and that one read-only view function answers
# (exit 1 if the call fails).
#
# Usage:
#   ./scripts/verify-deployment.sh [testnet] [--rebuild] [--source-account G...]
#   ./scripts/verify-deployment.sh --self-test
#   ./scripts/verify-deployment.sh --help
#
# `testnet` is the only accepted network. Strata is unaudited and has no mainnet
# code path, so the network guard runs before the records are even read.
#
# `--rebuild` runs `stellar contract build` and also compares each local Wasm
# artefact with the recorded hash. A record written before a contract changed
# will then report a mismatch, which is the intended signal: the deployed code
# is not the code in this tree. It is not a bug in the verifier.
#
# `--source-account` supplies the address the read-only calls are simulated
# from. It is only needed when a record names no `admin_public_address`; probe
# records, such as the refresh measurement, do not. Nothing is signed and no
# funds move.
#
# Reading the records needs a working `python3` (or `python`). The interpreter
# is probed by running it: on Windows `python3` can resolve to the Microsoft
# Store stub, which is on PATH and exits non-zero without doing anything, so a
# `command -v` check alone would be a lie.
#
# Exit codes:
#   0  every recorded contract exists, every hash matches, every view call passes
#   1  usage, guard, parser, build, source-account or read-only call failure
#   2  a recorded contract is not on the ledger (testnet reset, expired instance)
#   3  a Wasm sha256 mismatch (on-chain vs the record, or local build vs record)
#
# Nothing here writes to the network: every call is a read or a simulation
# (`--send=no`), so verifying a deployment cannot move funds.

set -euo pipefail

# One entry, deliberately. Adding another network is a reviewed change to this
# list, not a flag, and there is no mainnet path anywhere in this repository.
ALLOWED_NETWORKS=("testnet")

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

WASM_DIR="target/wasm32v1-none/release"
RECORD_DIR="deployments"

say() { printf '%s\n' "$*"; }
step() { say ""; say "==> $*"; }
die() {
    say "error: $*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
usage: ./scripts/verify-deployment.sh [testnet] [--rebuild] [--source-account G...]
       ./scripts/verify-deployment.sh --self-test

Verifies every deployments/testnet*.json record against the Stellar testnet
ledger: each recorded contract must exist, its on-chain Wasm hash must match the
recorded sha256, and one read-only view call per contract must answer.

  --rebuild               also build locally and compare the artefacts
  --source-account G...   address to simulate read-only calls from, for records
                          that name no admin_public_address
  --self-test             check the guard, the JSON parser and the failure
                          classifications with no network and no credentials
EOF
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

# Cross-platform sha256. GNU coreutils has sha256sum, macOS has shasum.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "no sha256sum or shasum on PATH; cannot compute wasm hashes"
    fi
}

# Discovered by glob rather than hardcoded, matching `scripts/deploy-testnet.sh`:
# `stellar contract build` names the output after the contract spec, which is not
# guaranteed to match the crate name.
find_wasm() {
    local marker="$1"
    local found=()
    local candidate
    for candidate in "$WASM_DIR"/*"$marker"*.wasm; do
        [ -f "$candidate" ] && found+=("$candidate")
    done
    if [ "${#found[@]}" -eq 0 ]; then
        return 1
    fi
    printf '%s\n' "${found[0]}"
}

# Probed by execution, not by existence. See the note in the header.
find_json_parser() {
    local candidate
    for candidate in python3 python; do
        command -v "$candidate" >/dev/null 2>&1 || continue
        if printf '{}' | "$candidate" -c 'import json, sys; json.load(sys.stdin)' >/dev/null 2>&1; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

# Reads one record and prints tab-separated lines:
#
#   network<TAB>testnet
#   admin<TAB>G...                  when the record names one
#   source<TAB>G...                 otherwise, the first account address it does
#                                   name, for the read-only simulations only
#   contract<TAB>name<TAB>C...<TAB>sha256
#
# Only `contracts.<name>` entries are contracts. A record also carries
# `contract_id` inside `token` (testnet.json) and inside `blend_pool` (the Blend
# records); those are addresses to check by eye, not deployed contracts, and a
# grep-based reader confuses them for one. That is why this parses the JSON.
parse_record() {
    "$PY" - "$1" <<'PYTHON'
import json
import re
import sys

CONTRACT_ID = re.compile(r"C[0-9A-Z]{55}")
ACCOUNT_ID = re.compile(r"G[0-9A-Z]{55}")
WASM_HASH = re.compile(r"[0-9a-f]{64}")


def fail(message):
    print("error: " + message, file=sys.stderr)
    sys.exit(1)


path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as handle:
        record = json.load(handle)
except OSError as exc:
    fail("%s: cannot read the record (%s)" % (path, exc))
except ValueError as exc:
    fail("%s: is not valid JSON (%s)" % (path, exc))

if not isinstance(record, dict):
    fail("%s: the top level is not a JSON object" % path)

network = record.get("network")
if not isinstance(network, str) or not network:
    fail('%s: no "network" string' % path)
print("network\t" + network)

admin = record.get("admin_public_address")
if isinstance(admin, str) and admin:
    if not ACCOUNT_ID.fullmatch(admin):
        fail('%s: admin_public_address "%s" is not an account address' % (path, admin))
    print("admin\t" + admin)
else:
    # Probe records do not name an admin. The first account address they do
    # name is used only as the simulation source, never as a signer.
    found = ACCOUNT_ID.search(json.dumps(record))
    if found:
        print("source\t" + found.group(0))

contracts = record.get("contracts")
if not isinstance(contracts, dict) or not contracts:
    fail('%s: no "contracts" object with any contracts in it' % path)

for name, body in contracts.items():
    if not isinstance(body, dict):
        fail("%s: contracts.%s is not an object" % (path, name))
    contract_id = body.get("contract_id")
    if not isinstance(contract_id, str) or not CONTRACT_ID.fullmatch(contract_id):
        fail("%s: contracts.%s.contract_id is missing or not a contract address"
             % (path, name))
    wasm_hash = body.get("wasm_sha256")
    if not isinstance(wasm_hash, str) or not WASM_HASH.fullmatch(wasm_hash):
        fail("%s: contracts.%s.wasm_sha256 is missing or not a sha256 hash"
             % (path, name))
    print("contract\t%s\t%s\t%s" % (name, contract_id, wasm_hash))
PYTHON
}

# One read-only function per contract type. A new contract type has to be added
# here deliberately, because "the view call failed" is a check that only means
# something if the function exists and is side-effect free.
view_for() {
    case "$1" in
        mock_vault | blend_adapter) printf 'total_assets\n' ;;
        epoch_manager) printf 'current_epoch\n' ;;
        *) return 1 ;;
    esac
}

# Classifies the result of `stellar contract info hash` into:
#   0  exact match with expected_hash
#   2  contract not found on ledger (testnet reset, expired instance, RPC 404)
#   3  hash mismatch between on-chain Wasm and recorded wasm_sha256
classify_hash_check() {
    local label="$1"
    local contract_id="$2"
    local expected_hash="$3"
    local cli_status="$4"
    local cli_output="$5"

    local actual_hash
    actual_hash=$(printf '%s\n' "$cli_output" | grep -oE '\b[0-9a-f]{64}\b' | tail -n 1 || true)

    if [ "$cli_status" -ne 0 ] || [ -z "$actual_hash" ]; then
        say "error: contract not found: $label ($contract_id) is missing on the ledger (testnet may have reset or the contract instance expired)." >&2
        if [ -n "$cli_output" ]; then
            say "  cli output: $cli_output" >&2
        fi
        return 2
    fi

    if [ "$actual_hash" != "$expected_hash" ]; then
        say "error: hash mismatch for $label ($contract_id): recorded wasm_sha256 is $expected_hash, but on-chain wasm hash is $actual_hash." >&2
        return 3
    fi

    say "  ok   $label ($contract_id) on-chain wasm_sha256 = $actual_hash"
    return 0
}

# Compares a locally built Wasm artefact's sha256 against the recorded hash.
check_local_hash() {
    local label="$1"
    local wasm_path="$2"
    local expected_hash="$3"

    local local_hash
    local_hash=$(sha256_of "$wasm_path")
    if [ "$local_hash" != "$expected_hash" ]; then
        say "error: hash mismatch for local build of $label ($wasm_path): recorded wasm_sha256 is $expected_hash, but local artefact sha256 is $local_hash." >&2
        return 3
    fi

    say "  ok   $label local build ($wasm_path) matches wasm_sha256 = $local_hash"
    return 0
}

# A read-only invoke. `--send=no` means the call is simulated, never submitted.
call_readonly() {
    local label="$1" cid="$2" fn="$3" source="$4"
    local out rc
    set +e
    out=$(stellar contract invoke \
        --id "$cid" \
        --source-account "$source" \
        --network "$NETWORK" \
        --no-cache \
        --send=no \
        -- \
        "$fn" 2>&1)
    rc=$?
    set -e

    if [ "$rc" -ne 0 ]; then
        # "PositionMissing" is a real adapter trap and must not be read as a
        # missing contract, so the pattern names absence explicitly rather than
        # matching the word "missing" anywhere in the output.
        if printf '%s\n' "$out" | grep -qiE 'not found|does not exist|no such contract'; then
            say "error: contract not found: $label ($cid) failed read-only call '$fn' because the contract entry is missing on $NETWORK." >&2
            say "  cli output: $out" >&2
            exit 2
        fi
        say "error: read-only call '$fn' on $label ($cid) failed (exit $rc):" >&2
        say "  $out" >&2
        exit 1
    fi
    say "  ok   $label.$fn() -> $(printf '%s' "$out" | tail -n 1)"
}

verify_record() {
    local record="$1"
    local parsed

    step "checking $record"
    parsed="$(parse_record "$record")" || exit 1

    local -a names=() ids=() hashes=()
    local kind f1 f2 f3
    local rec_network="" rec_admin="" rec_source=""
    while IFS=$'\t' read -r kind f1 f2 f3; do
        case "$kind" in
            network) rec_network="$f1" ;;
            admin) rec_admin="$f1" ;;
            source) rec_source="$f1" ;;
            contract)
                names+=("$f1")
                ids+=("$f2")
                hashes+=("$f3")
                ;;
            "") ;;
            *) die "$record: unexpected parser field '$kind'" ;;
        esac
    done <<<"$parsed"

    assert_network_is_allowed "$rec_network" || die "$record records network
'$rec_network'. This verifier only checks testnet: Strata is unaudited and has
no mainnet path."

    local source="$SOURCE_ACCOUNT"
    [ -n "$source" ] || source="$rec_admin"
    [ -n "$source" ] || source="$rec_source"
    [ -n "$source" ] || die "$record records no admin_public_address and no
account address to simulate its read-only calls from. Pass --source-account
<G...>."

    say "  network $rec_network, ${#names[@]} contract(s), simulated from $source"

    # --- on-chain wasm hash --------------------------------------------------
    local i label out rc
    for i in "${!names[@]}"; do
        label="${record%.json}:${names[$i]}"

        if [ "$REBUILD" -eq 1 ]; then
            local wasm
            wasm="$(find_wasm "${names[$i]}")" || die "no wasm matching
*${names[$i]}* in $WASM_DIR after 'stellar contract build'"
            check_local_hash "$label" "$wasm" "${hashes[$i]}" || exit $?
        fi

        set +e
        out=$(stellar contract info hash --id "${ids[$i]}" --network "$NETWORK" 2>&1)
        rc=$?
        set -e

        classify_hash_check "$label" "${ids[$i]}" "${hashes[$i]}" "$rc" "$out" || exit $?
    done

    # --- read-only view ------------------------------------------------------
    local fn
    for i in "${!ids[@]}"; do
        fn="$(view_for "${names[$i]}")" || die "$record lists contract
'${names[$i]}', which has no known read-only function. Add it to view_for() in
this script before verifying it."
        call_readonly "${names[$i]}" "${ids[$i]}" "$fn" "$source"
    done
}

# --- self-test ---------------------------------------------------------------
#
# No network, no credentials, no Stellar CLI. CI runs this on every push and PR,
# which is the only part of this script that can fail without a live deployment.
self_test() {
    local failures=0
    check() {
        local what="$1" got="$2"
        if [ "$got" = "ok" ]; then
            echo "ok: $what"
        else
            echo "FAIL: $what"
            failures=$((failures + 1))
        fi
    }

    check "testnet allowed" "$(assert_network_is_allowed testnet && echo ok || echo FAIL)"
    local bad
    for bad in mainnet public "Main Net" "" local futurenet testnetX; do
        check "'$bad' refused" "$(assert_network_is_allowed "$bad" && echo FAIL || echo ok)"
    done

    # The guard has to run before the records are read, so an invocation naming
    # a real network fails on the network rather than on a missing file.
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    # A good record: one real contract, plus a `token` object whose
    # `contract_id` must not be mistaken for a deployed contract.
    cat >"$tmp/good.json" <<'JSON'
{
  "network": "testnet",
  "admin_public_address": "GBIMKBYJVP3VNFJPU45XMGBKHLESO5QLUYWAAMMRMGTPDF6NUD5OWRIP",
  "token": {
    "contract_id": "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC"
  },
  "contracts": {
    "mock_vault": {
      "contract_id": "CDWJS65BA26QBTA4L6LBX76B2XPGAQHY25USI6Z3MSQ73T5NQTXARSQ6",
      "wasm_sha256": "a3599c7022dc90c5df2fe009e49e25de6fb087536f872c588efc5cc732346759"
    }
  }
}
JSON
    local parsed
    parsed="$(parse_record "$tmp/good.json")" || parsed=""
    check "a good record parses" "$([ -n "$parsed" ] && echo ok || echo FAIL)"
    check "network is read" "$(printf '%s\n' "$parsed" | grep -qxF "network"$'\t'"testnet" && echo ok || echo FAIL)"
    check "admin is read" "$(printf '%s\n' "$parsed" | grep -qxF "admin"$'\t'"GBIMKBYJVP3VNFJPU45XMGBKHLESO5QLUYWAAMMRMGTPDF6NUD5OWRIP" && echo ok || echo FAIL)"
    check "only 'contracts' entries are contracts" \
        "$([ "$(printf '%s\n' "$parsed" | grep -c "^contract"$'\t')" -eq 1 ] && echo ok || echo FAIL)"

    # A probe record with no admin: the first account address it names becomes
    # the read-only simulation source.
    cat >"$tmp/no-admin.json" <<'JSON'
{
  "network": "testnet",
  "contracts": {
    "blend_adapter": {
      "contract_id": "CCBPPPH6T42DQ5BVANHVQYXZRI2NBMVYLKR67HA6B3KJSBCOJPHHBYBQ",
      "wasm_sha256": "dbf5a1f45091d4a8fd94aea604163c2e2b7539cc9f168b6b56fd6db9e525a80a",
      "manager": "GC6A5FQOXIVRYYOQACUVFIK4BUQCC3LCR75XKJFQOD5HFL6IWD3MVHRS"
    }
  }
}
JSON
    parsed="$(parse_record "$tmp/no-admin.json")" || parsed=""
    check "a record with no admin parses" "$([ -n "$parsed" ] && echo ok || echo FAIL)"
    check "an account address is used as the source" \
        "$(printf '%s\n' "$parsed" | grep -qxF "source"$'\t'"GC6A5FQOXIVRYYOQACUVFIK4BUQCC3LCR75XKJFQOD5HFL6IWD3MVHRS" && echo ok || echo FAIL)"

    # A contract without a wasm hash must fail the parse, loudly: a record that
    # silently verifies one field and skips another is worse than no record.
    cat >"$tmp/broken.json" <<'JSON'
{
  "network": "testnet",
  "contracts": {
    "mock_vault": {
      "contract_id": "CDWJS65BA26QBTA4L6LBX76B2XPGAQHY25USI6Z3MSQ73T5NQTXARSQ6"
    }
  }
}
JSON
    set +e
    parse_record "$tmp/broken.json" >/dev/null 2>&1
    local broken_rc=$?
    set -e
    check "a contract without a hash is rejected" "$([ "$broken_rc" -ne 0 ] && echo ok || echo FAIL)"

    # Invalid JSON and a missing file both fail rather than reporting zero
    # contracts, which would otherwise look like a pass.
    printf 'not json\n' >"$tmp/garbage.json"
    set +e
    parse_record "$tmp/garbage.json" >/dev/null 2>&1
    local garbage_rc=$?
    parse_record "$tmp/absent.json" >/dev/null 2>&1
    local absent_rc=$?
    set -e
    check "invalid JSON is rejected" "$([ "$garbage_rc" -ne 0 ] && echo ok || echo FAIL)"
    check "a missing record is rejected" "$([ "$absent_rc" -ne 0 ] && echo ok || echo FAIL)"

    # Every committed record must parse. This is what stops a hand-edited
    # record from going unverified.
    local record count=0
    for record in "$RECORD_DIR"/testnet*.json; do
        [ -f "$record" ] || continue
        count=$((count + 1))
        parsed="$(parse_record "$record" 2>/dev/null)" || parsed=""
        check "$record parses" "$([ -n "$parsed" ] && echo ok || echo FAIL)"
        check "$record is testnet" \
            "$(printf '%s\n' "$parsed" | grep -qxF "network"$'\t'"testnet" && echo ok || echo FAIL)"
        check "$record lists at least one contract" \
            "$(printf '%s\n' "$parsed" | grep -q "^contract"$'\t' && echo ok || echo FAIL)"
    done
    check "at least one deployment record exists" "$([ "$count" -gt 0 ] && echo ok || echo FAIL)"

    # The view mapping is a promise about side-effect-free functions.
    check "mock_vault -> total_assets" "$([ "$(view_for mock_vault)" = total_assets ] && echo ok || echo FAIL)"
    check "blend_adapter -> total_assets" "$([ "$(view_for blend_adapter)" = total_assets ] && echo ok || echo FAIL)"
    check "epoch_manager -> current_epoch" "$([ "$(view_for epoch_manager)" = current_epoch ] && echo ok || echo FAIL)"
    check "an unknown contract has no view" "$(view_for unknown_contract >/dev/null 2>&1 && echo FAIL || echo ok)"

    # Distinct failure classifications. These are the two the issue asks to
    # tell apart: "the deployment is gone" and "the deployment is not this code".
    local cid="CDWJS65BA26QBTA4L6LBX76B2XPGAQHY25USI6Z3MSQ73T5NQTXARSQ6"
    local good_hash="a3599c7022dc90c5df2fe009e49e25de6fb087536f872c588efc5cc732346759"
    local wrong_hash="0000000000000000000000000000000000000000000000000000000000000000"

    set +e
    classify_hash_check "probe" "$cid" "$good_hash" 0 "$good_hash" >/dev/null 2>&1
    local match_rc=$?
    local missing_msg
    missing_msg="$(classify_hash_check "probe" "$cid" "$good_hash" 1 "error: Contract not found" 2>&1)"
    local missing_rc=$?
    local mismatch_msg
    mismatch_msg="$(classify_hash_check "probe" "$cid" "$good_hash" 0 "$wrong_hash" 2>&1)"
    local mismatch_rc=$?
    set -e

    check "a matching on-chain hash is accepted" "$([ "$match_rc" -eq 0 ] && echo ok || echo FAIL)"
    check "a missing contract exits 2" "$([ "$missing_rc" -eq 2 ] && echo ok || echo FAIL)"
    check "a missing contract says 'contract not found'" \
        "$(printf '%s' "$missing_msg" | grep -q 'contract not found' && echo ok || echo FAIL)"
    check "a hash mismatch exits 3" "$([ "$mismatch_rc" -eq 3 ] && echo ok || echo FAIL)"
    check "a hash mismatch says 'hash mismatch'" \
        "$(printf '%s' "$mismatch_msg" | grep -q 'hash mismatch' && echo ok || echo FAIL)"

    # Local artefact comparison, with no wasm needed.
    local probe_file="$tmp/probe.bin"
    printf 'probe\n' >"$probe_file"
    local probe_hash
    probe_hash="$(sha256_of "$probe_file")"
    check "sha256_of works" "$([ "${#probe_hash}" -eq 64 ] && echo ok || echo FAIL)"
    set +e
    check_local_hash "probe" "$probe_file" "$probe_hash" >/dev/null 2>&1
    local local_match_rc=$?
    check_local_hash "probe" "$probe_file" "$wrong_hash" >/dev/null 2>&1
    local local_mismatch_rc=$?
    set -e
    check "a matching local artefact is accepted" "$([ "$local_match_rc" -eq 0 ] && echo ok || echo FAIL)"
    check "a local mismatch exits 3" "$([ "$local_mismatch_rc" -eq 3 ] && echo ok || echo FAIL)"

    if [ "$failures" -ne 0 ]; then
        echo "self-test failed: $failures check(s)"
        exit 1
    fi
    echo "verify-deployment self-test passed"
    exit 0
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    usage
    exit 0
fi

PY="$(find_json_parser)" || die "no working python3 (or python) on PATH.
The deployment records are JSON and this script parses them rather than grepping
them, so that a nested \"contract_id\" is never mistaken for a deployment. See
docs/deployment.md for the prerequisites."

if [ "${1:-}" = "--self-test" ]; then
    self_test
fi

NETWORK="testnet"
REBUILD=0
SOURCE_ACCOUNT=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --rebuild) REBUILD=1 ;;
        --source-account)
            [ "$#" -ge 2 ] || die "--source-account needs an address"
            SOURCE_ACCOUNT="$2"
            shift
            ;;
        --*) die "unexpected flag '$1'. See --help." ;;
        *) NETWORK="$1" ;;
    esac
    shift
done

# The guard runs first, before the CLI check and before any record is read, so
# asking for mainnet fails on the network regardless of what else is wrong.
assert_network_is_allowed "$NETWORK" || die "network '$NETWORK' is not allowed.
Strata is unaudited and verify-deployment.sh only checks testnet. Allowed: ${ALLOWED_NETWORKS[*]}"

records=()
for candidate in "$RECORD_DIR"/testnet*.json; do
    [ -f "$candidate" ] && records+=("$candidate")
done
[ "${#records[@]}" -gt 0 ] || die "no deployment records matching $RECORD_DIR/testnet*.json.
Run scripts/deploy-testnet.sh first, or check out the branch that records the
deployment."

command -v stellar >/dev/null 2>&1 ||
    die "stellar CLI not found. Install it, then re-run. See docs/deployment.md."

say "Verifying ${#records[@]} deployment record(s) against Stellar $NETWORK. UNAUDITED."

if [ "$REBUILD" -eq 1 ]; then
    step "building contracts locally (--rebuild)"
    stellar contract build
fi

for record in "${records[@]}"; do
    verify_record "$record"
done

step "done"
say "Every contract in ${#records[@]} record(s) is live on $NETWORK, every on-chain wasm hash matches the record, and every read-only call succeeded."
