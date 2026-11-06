# Changelog

Notable changes to this repository, newest first. Dates are the date the change
landed, not a release date. Strata is unaudited and testnet-only; nothing in this
file is a production-readiness claim.

## 2026-11-06

- **feat:** the Blend adapter is deployed against the real testnet Blend pool
  (`CCEBVDYM…`) with its own epoch manager, recorded in
  `deployments/testnet-blend.json` and produced by
  `scripts/deploy-testnet-blend.sh`. One 300-second epoch settled end to end:
  2 XLM in, 20 000 451 stroops out, so **451 stroops of yield**; the senior took
  its 4-stroop target and the junior the remaining 447, matching
  `docs/waterfall-spec.md` recomputed independently. The adapter was built on
  2026-11-05, before `refresh()` merged, so it has no `refresh()` and a build of
  the current tree is a different Wasm; the record states what ran, not what the
  tree builds today. `README.md` and risk entries B1–B7 now say that one testnet
  epoch has run, and what it does and does not prove.
- **feat:** `scripts/verify-deployment.sh` now verifies **every**
  `deployments/testnet*.json` record, not only `testnet.json`: each recorded
  contract must exist on chain, the ledger's Wasm hash must match the recorded
  `wasm_sha256`, and one read-only view call per contract must answer. The
  records are parsed as JSON, so the `contract_id` nested under `token` or
  `blend_pool` is not mistaken for a deployment, and `--rebuild` compares the
  local artefacts with every record. Today it checks `testnet.json`,
  `testnet-blend-refresh.json` and `testnet-blend.json`; a new record needs no
  change here.
- **ci:** the `docs are current` job runs `verify-deployment.sh --self-test`,
  which was an acceptance criterion of issue #5 that PR #22 did not wire up.
  The self-test is extended to cover the parser and every committed record.

## 2026-11-05

- **feat:** add a permissionless `refresh()` to the Blend adapter that reads the
  pool's position — the read that renews the pool's own user entry — and records
  the ledger in `last_refreshed`; `deposit` and `redeem` use the same path. Adds
  `scripts/refresh-adapter-testnet.sh` and three tests: the call needs no
  authorisation, it moves no funds, and the pool double's entry TTL advances.
  On testnet on 2026-11-05 the call succeeded from a non-manager identity and
  `last_refreshed` moved 5 569 344 → 5 569 359, but the entry's live-until
  ledger did not (7 642 944 before and after): the entry was still ~120 days
  from expiry, above Blend's ~100-day extension threshold. **TTL extension is
  therefore not proven**, and risk B4 remains open. Adds
  `deployments/testnet-blend-refresh.json` for the probe.
- **fix:** declare the full `Reserve` shape the deployed Blend pool returns. The
  interface carried only the supplier-relevant subset (9 config fields and 4
  data fields); the pool's structs have 13 and 7. Soroban decodes a struct as an
  exact field set, so pointing an adapter at the real testnet pool failed in the
  constructor with `UnexpectedSize`. The double now returns the same shape. No
  accounting behaviour changed.
- **fix:** value the Blend adapter's position with Blend v2's fixed `1e12`
  `b_rate` scale instead of `Reserve::scalar`. `scalar` is the underlying
  asset's `10^decimals` (1e7 for XLM), so the previous divisor under-reported a
  position against a real pool by five orders of magnitude. The pool double now
  reports the same `scalar` a real reserve does, and its conversions use the
  fixed scale. The adapter still has not run against a real pool; no on-chain
  claim is made here.

## 2026-10-27

- **test:** property-test the Blend adapter's accounting (rounding cannot
  create value at a deposit or a redemption, a deposit/redeem round trip is
  bounded by principal plus accrued yield, and share accounting is monotonic)
  and run four full epochs through the real epoch manager with the adapter as
  its vault: good, flat, loss and liquidity-limited settlement. Expected
  payouts are recomputed from `docs/waterfall-spec.md`, not read back from the
  contract, and the pool position is recomputed independently. 8 new tests, no
  contract code changed. Still tested only against the pool double; the adapter
  has not been deployed and has never run against a real pool.
- **feat:** add a Blend-backed vault, `contracts/blend-adapter`, implementing
  `VaultInterface` unchanged so the epoch manager cannot tell it from the mock
  vault. Adds `contracts/blend-pool-interface` for the pool ABI subset and
  `contracts/blend-pool-mock` as a test double. 34 new tests. Risks B1–B7 are
  recorded in `docs/risks.md`. The adapter has **not** been deployed and has
  never been run against a real pool; keeping its pool position alive across a
  long epoch is still unsolved.
- **docs:** state the project's Stellar relevance in the first line of
  `README.md`: these are Soroban smart contracts on Stellar that split a vault's
  yield into senior and junior tranches, and they are unaudited and testnet-only.
  No code changed.
