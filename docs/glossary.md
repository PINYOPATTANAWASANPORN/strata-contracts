# Glossary

Plain-English definitions of the terms these documents use. Where a term has a
precise meaning in the settlement rules, the entry links to the document that
defines it; the entries here are summaries, not a second source of truth.

**Strata is unaudited and testnet-only.** Nothing in this glossary is a claim
that the contracts are safe.

### Basis point (bps)

One hundredth of a percentage point, so 10 000 bps is 100%. The senior target
rate and the deposit gating ratio are both quoted in bps.

### bToken

Blend's receipt for assets supplied to a reserve, similar in role to a vault
share. One bToken is worth `floor(b_rate / 10^12)` units of the underlying, and
`b_rate` rises as borrowers pay interest and falls when the pool socialises bad
debt. See [architecture.md](architecture.md#the-blend-adapter).

### Claim

The per-depositor payout made after settlement. Each claim is pro-rata by
principal, and the last claim in a tranche is paid the remainder so the tranche
pays out exactly its settled amount. Claims open once the epoch is `Settled`.

### Close epoch

The final, permissionless transition. It is allowed only when settlement has
happened and every position has been claimed, and it frees the manager to open
another epoch. See [architecture.md](architecture.md#the-state-machine).

### Cushion

The loss the junior tranche can absorb before the senior is touched: `J - I`,
where `J` is junior principal and `I` is the senior's target interest. It is
**not** `J` — the senior's own target interest sits inside the buffer, so a
buffer exactly equal to `I` leaves a cushion of zero. See
[waterfall-spec.md §3](waterfall-spec.md#why-these-bounds).

### Deposit gating ratio

The per-epoch cap on senior principal, as a fraction of junior principal, fixed
when the epoch is created and never above 1.0x. A senior deposit is accepted
only if the totals *after* it still satisfy `S * 10_000 <= J * ratio`; junior
deposits are never gated. See
[waterfall-spec.md §7](waterfall-spec.md#7-deposit-gating).

### Dust

The base units left over when a pro-rata claim is rounded down. It is at most
`n - 1` units for `n` depositors in a tranche, and the last claim in the tranche
absorbs it. See
[waterfall-spec.md §8](waterfall-spec.md#8-distribution-to-depositors).

### Epoch

One fixed-term round of the protocol: deposits are accepted while the epoch is
`Open`, settlement splits the redeemed value, claims pay the depositors, and
closing frees the manager. There is exactly one active epoch at a time.

### ERC-4626

The standard interface for tokenised vaults. Strata implements the small
ERC-4626-style subset it needs in `vault-interface`, which is what lets the
epoch manager wrap different vaults without changing.

### Junior

The tranche that absorbs losses first and receives everything above the senior's
target. It is paid nothing unless the senior has been made whole.

### Keeper

An operator that submits the protocol's permissionless calls — settlement,
adapter refresh, TTL extension — on a schedule. A keeper has no special
authority and can only move an epoch forward.

### Ledger

One entry in Stellar's sequence of ledgers, with a close time and a sequence
number. Soroban contracts read the close time as `ledger().timestamp()`, and the
epoch's deposit and settlement boundaries are defined on it.

### Maturity

The end of the epoch's term, fixed at `start_ts + term_seconds` when the epoch
opens. Deposits are refused at or after maturity and settlement is allowed from
then on; nothing later changes the maturity time itself.

### Oracle

A price feed. The testnet Blend pool is driven by a mock oracle, so the prices
it reports are test inputs rather than market prices. See
[risks.md](risks.md) B7.

### Position

A depositor's principal in one tranche of one epoch. It is recorded as
principal rather than vault shares, and it is not transferable.

### Principal

The amount a depositor put in. The waterfall and the pro-rata claim split are
both defined over principal, while the manager's position at the vault is
tracked separately in vault shares.

### Pro-rata

In proportion to principal. A normal claim is
`payout * principal / tranche_total`, rounded down, and the last claim in a
tranche takes what is left.

### SAC (Stellar Asset Contract)

The token contract Stellar provides for an asset; XLM's SAC is the contract for
the native asset. Strata moves the underlying asset through its SAC using
SEP-41 token calls.

### Senior

The tranche paid its principal plus a fixed target rate first, capped at that
target. "Senior" means paid first, not safe: a senior depositor can lose
principal in a wipeout.

### Senior due

The most the senior tranche can be paid: `S + I`, principal plus target
interest. See [waterfall-spec.md §4](waterfall-spec.md#4-senior-due).

### Senior target interest (I)

The senior's promised interest, `S * r * t / (10_000 * 31_536_000)`, truncated.
The discarded fraction stays in `V` and goes to the junior. See
[waterfall-spec.md §4](waterfall-spec.md#4-senior-due).

### Senior target rate (r)

The fixed annual rate promised to the senior tranche, in bps, chosen when the
epoch is created and capped at 10 000 bps (100%/yr). See
[waterfall-spec.md §3](waterfall-spec.md#3-constants).

### Settlement

The one-way transition that redeems the manager's whole vault position and
splits the proceeds between the tranches. It is permissionless once the epoch is
mature, and it fixes `V` and both payouts for good. See
[architecture.md](architecture.md#the-state-machine).

### Soroban

Stellar's smart-contract platform. The contracts in this repository are Soroban
contracts, compiled to Wasm and invoked on chain.

### Stroop

One ten-millionth (0.0000001) of an XLM, and the base unit for 7-decimal
Stellar assets. Amounts in these documents are base units, so 451 stroops is
0.0000451 XLM.

### Testnet

Stellar's public test network. It carries no real value and SDF resets it
periodically, so the addresses recorded here are only meaningful until the next
reset.

### Tranche

One of the two claims on an epoch's result: senior or junior. Each tranche has
its own settled payout and its own pool of pro-rata claims.

### TTL (time to live)

How many ledgers a Soroban storage entry stays live before it expires. A
persistent entry that expires is archived rather than deleted, and it can be
restored by paying rent. See [architecture.md](architecture.md#storage-layout).

### Underlying vault

The contract that holds the assets and produces the value settlement redeems. It
implements the vault interface; `mock-vault` is the admin-set test double and
`blend-adapter` supplies to a real Blend pool.

### Unaudited

Not reviewed by an independent third party. Everything in this repository is
testnet-only for that reason; see the disclaimer in the
[README](../README.md#disclaimer).

### Utilisation

The share of a Blend reserve that is currently lent out. At or near 100%, a full
withdrawal may not be possible, which can block settlement. See
[risks.md](risks.md) B2.

### V (value redeemed)

The amount the vault actually returns when the manager redeems its whole
position at settlement. The waterfall splits `V`; it is not a balance the vault
merely reports. See [waterfall-spec.md §2](waterfall-spec.md#2-inputs).

### Vault share

A claim on the vault's pooled position, as distinct from a depositor's recorded
principal. The share price moves as the vault earns or loses, so a later
depositor receives fewer shares for the same assets; the manager tracks shares
for redemption and principal for payouts.

### Wasm

The bytecode Soroban executes. A deployable contract must be built by
`stellar contract build` for `wasm32v1-none`, and the resulting `wasm_sha256` is
what the deployment records pin.

### Waterfall

The ordered split of `V`: the senior is paid first and capped at its target, and
the junior receives the remainder. See
[waterfall-spec.md §5](waterfall-spec.md#5-the-waterfall).

## Sources

- [waterfall-spec.md](waterfall-spec.md) — the settlement math and its five
  invariants. Source of truth.
- [architecture.md](architecture.md) — state machine, storage, authorisation,
  and the adapter.
- [risks.md](risks.md) — the working risk register, including what is unresolved.
- [analysis/settlement-ordering.md](analysis/settlement-ordering.md) — what
  transaction ordering around maturity is worth.
