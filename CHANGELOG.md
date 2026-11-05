# Changelog

Notable changes to this repository, newest first. Dates are the date the change
landed, not a release date. Strata is unaudited and testnet-only; nothing in this
file is a production-readiness claim.

## 2026-11-05

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
