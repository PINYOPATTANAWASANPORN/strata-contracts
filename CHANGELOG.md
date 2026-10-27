# Changelog

Notable changes to this repository, newest first. Dates are the date the change
landed, not a release date. Strata is unaudited and testnet-only; nothing in this
file is a production-readiness claim.

## 2026-10-27

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
