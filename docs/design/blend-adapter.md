# A Blend vault adapter: research and go / no-go

**Status:** research only. No contract code was written for this note.
**Date:** 2026-10-25.

The design below was built on 2026-10-27 as `contracts/blend-adapter`, with
`contracts/blend-pool-interface` for the pool ABI subset and
`contracts/blend-pool-mock` as the test double. Property tests for its
accounting and full-epoch integration tests through the real epoch manager were
added the same day. That work is tested against the double only: the adapter has
not been deployed, has never run against a real pool, and the TTL finding in
§4.7 is still unaddressed. Everything measured in §5 and every risk in §8 stands
as written. See [`risks.md`](../risks.md) B1–B7 for where each one stands now.
**Scope:** can Strata's epoch manager run against a real Stellar yield source on
testnet — a Blend lending pool — through an adapter that implements the existing
`vault-interface` unchanged?

**Unaudited. Testnet only. Nothing here has been reviewed by a third party, and
nothing in this note is a security assessment of Blend.**

---

## 1. Recommendation

**Go — build the adapter for testnet, with the conditions in §9.**

A usable Blend deployment exists on Stellar testnet today, and it already
supports the token the existing epoch manager uses as its underlying: the
native XLM Stellar Asset Contract. Every difference between what the manager
expects and what a Blend pool does can be absorbed by the adapter. None of the
three components the task forbids touching — the waterfall crate, the waterfall
spec, the epoch manager — needs to change.

Two findings materially shape the design rather than merely constraining it:

1. **A Blend withdrawal larger than the position is silently clamped, not
   rejected.** Verified on testnet (§5). The adapter therefore cannot rely on
   the pool to enforce an exact withdrawal; it has to measure what it actually
   received and fail loudly if it is short.
2. **A Blend user position entry expires after roughly 120 days without being
   touched, and an expired position reads back as empty.** Established from
   Blend's source, not tested (a 120-day test is not possible in this session).
   A Strata epoch may run for up to 365 days, so an epoch can outlive its own
   Blend position unless something refreshes the entry.

The first is a design detail. The second is a risk to depositor funds and is
treated as such in §8.

---

## 2. Sources

Everything about Blend below comes from one of these. Where a fact was verified
on chain, the read is listed in §5. "Checked" means the page or file was read on
that date.

| # | Source | What it established | Checked |
| --- | --- | --- | --- |
| S1 | <https://docs.blend.capital/llms.txt> and the `tech-docs/core-contracts/lending-pool/*` pages | the documented model: `Request` structs, `submit()`, b/d tokens, pool status, bad debt | 2026-10-25 |
| S2 | <https://github.com/blend-capital/blend-contracts-v2> at `ba22b487b2c5057a4ecc28b05b5193c28e4bd117` — `pool/src/pool/{actions,submit,reserve,user,status,pool}.rs`, `pool/src/{storage,errors,constants}.rs` | the actual behaviour, including rounding, clamping, status gating, TTL | 2026-10-25 |
| S3 | <https://github.com/blend-capital/blend-utils> at `b05242df30b6b6caf9d317646f754541824a5a8b` — `testnet.contracts.json` | the official testnet contract addresses and the recorded wasm hashes | 2026-10-25 |
| S4 | `stellar contract info interface` / `info hash` / `contract invoke` against Stellar testnet | which of those addresses exist, what the deployed spec is, and live state | 2026-10-25 |
| S5 | This repository: `contracts/vault-interface`, `contracts/mock-vault`, `contracts/epoch-manager`, `docs/waterfall-spec.md`, `docs/risks.md` | what the manager needs from a vault | 2026-10-25 |

S2 is `blend-contracts-v2`, not `blend-capital/blend-contracts`. The latter is
the v1 implementation and is **not** the code deployed at the testnet address
below: the deployed spec has functions v1 does not (`propose_admin`,
`get_reserve_list`, `flash_loan`, `submit_with_allowance`) and a different
`Reserve` shape (`{asset, config, data, scalar}` in v2). Anyone reading the
wrong repository will get the rounding scale wrong by a factor of a thousand,
because v2 moved `b_rate` and `d_rate` from 9 to 12 decimals.

### A documented difference worth knowing about

S1 says the `from` and `to` arguments of `submit()` must authorise the call. S2
shows the implementation requires `spender.require_auth()`, and additionally
`from.require_auth()` only when `from != spender`. `to` is not required to
authorise. The adapter relies on the source, not the documentation, and the test
double in Phase 2 should implement the source behaviour.

---

## 3. What the epoch manager needs from a vault

From S5. The manager calls five functions; the interface declares a sixth that it
never calls:

| Call | Where | What the manager assumes |
| --- | --- | --- |
| `asset()` | `create_epoch`/`__constructor`, `lib.rs:275` | returns the underlying token address, which is then **pinned for the life of the manager**. A vault cannot change its asset later. |
| `deposit(assets, receiver)` | `deposit`, `lib.rs:361` | pulls exactly `assets` of the pinned token from the manager (`transfer(from → vault)` authorised by the manager via `authorize_as_current_contract`), and returns the share count minted to `receiver`. The manager records that number in `epoch.vault_shares` and never re-derives it. |
| `balance_of(account)` | `settle`, `lib.rs:432` | returns the manager's share balance. `settle` refuses if it is **less** than the recorded `epoch.vault_shares` (`Error::NoVaultShares`). |
| `redeem(shares, receiver, owner)` | `settle`, `lib.rs:440` | burns exactly `shares` from `owner`, sends the assets to `receiver`, and **returns the assets actually transferred**. That return value is the spec's `V`. A negative return is rejected. |
| `convert_to_assets(shares)` | `position_of` projection, `lib.rs:850` | a read-only valuation used only before settlement. Never used for payout. |
| `total_assets()` | — | **declared in the interface, never called by the manager.** Only the integration test harness calls it. It must still exist for interface completeness. |

Consequences that matter for an adapter:

- **`V` is the redemption return value, not a reported figure.** `settle` also
  re-checks that the manager's token balance covers `senior_payout +
  junior_payout` and refuses otherwise (`Error::InvalidAmount`). A vault cannot
  report one number and pay another without the transaction failing.
- **One position, all-or-nothing.** The manager deposits every epoch's funds as
  a single account and redeems its entire share balance once. There are no
  partial redemptions and no per-depositor vault accounting.
- **Units are base units of one token.** `i128`, no decimals, no floating point.
  The manager reads decimals for display only.
- **Zero shares for a real transfer is a loss**, and the manager does not detect
  it: it records `shares` without checking that it is positive. The adapter must
  reject a deposit that would mint nothing.
- **A vault that reverts blocks settlement.** That is risk R8 in
  `docs/risks.md`, already accepted for the mock; a real pool makes it a live
  possibility rather than a hypothetical.

---

## 4. What Blend provides

### 4.1 The testnet deployment

`testnet.contracts.json` (S3) names a v2 pool `TestnetV2` and the shared
protocol contracts. Each address was read on chain (S4):

| Contract | Address | On-chain wasm hash | Matches S3? |
| --- | --- | --- | --- |
| Lending pool (`TestnetV2`) | `CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF` | `a41fc53d6753b6c04eb15b021c55052366a4c8e0e21bc72700f461264ec1350e` | yes (`hashes.lendingPoolV2`) |
| Pool factory v2 | `CDV6RX4CGPCOKGTBFS52V3LMWQGZN3LCQTXF5RVPOOCG4XVMHXQ4NTF6` | `31328050548831f63d2b72e37bcfd0bb7371b7907135755dbe09ed434d755ca9` | yes |
| Backstop v2 | `CBDVWXT433PRVTUNM56C3JREF3HIZHRBA64NB2C3B2UNCKIS65ZYCLZA` | `c1f4502a757e25c611f5a159bc1ab0eef64085adac6c68123dca66e87faffbc2` | yes |
| Oracle (a **mock** on testnet) | `CAZOKR2Y5E2OSWSIBRVZMJ47RUTQPIGVWSAQ2UISGAVC46XKPGDG5PKI` | built-in / per S3 named `oraclemock` | n/a |

Hash equality is evidence that the deployed bytecode is the build Blend's own
deployment record describes. I did **not** rebuild `blend-contracts-v2` from
source and compare, so this is not a source-to-bytecode attestation.

The pool's admin is `GATALTGTWIOT6BUDBCZM3Q4OQ4BO2COLOAZ7IYSKPLC2PMSOPPGF5V56`.
Testnet is periodically reset by SDF, so all of these are promises that expire.

### 4.2 Live state, read 2026-10-25

`get_config` on the pool:

```
status = 0 (Admin_Active)   bstop_rate = 1_000_000 (10%)   max_positions = 8
min_collateral = 0          oracle = CAZOKR2Y… (mock)
```

`get_reserve_list` returns four reserves, in index order:
`XLM` (index 0), `wETH`, `wBTC`, `USDC`.

**XLM is a supported reserve**, and it is the exact contract the existing
deployment uses as its underlying
(`CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`). A Strata epoch
manager can therefore point at this pool with no change of underlying token.

`get_reserve(XLM)`:

| Field | Value | Meaning |
| --- | --- | --- |
| `config.index` | 0 | the key used in `Positions.supply` |
| `config.decimals` | 7 | XLM stroops |
| `config.enabled` | true | supply permitted |
| `config.c_factor` / `l_factor` | 0.9 / 0.9 | irrelevant to a pure supplier |
| `config.util` / `max_util` | 0.50 / 0.95 | interest curve target and borrow ceiling |
| `config.supply_cap` | `i128::MAX` | no practical cap |
| `data.b_rate` | `2_469_494_061_307` | 12 decimals: 1 bToken ≈ 2.4695 XLM |
| `data.b_supply` | `527_376_541_649_015` | bTokens outstanding |
| `data.d_rate` | `3_308_070_577_227` | 12 decimals |
| `data.d_supply` | `368_423_166_654_221` | dTokens outstanding |

Applying Blend's own conversion (`floor(b_tokens · b_rate / 1e12)`, §4.4) to
that read:

```
total supplied   = 130_235_323.77 XLM
total borrowed   = 121_876_983.76 XLM
utilization      ≈ 93.58%          (borrow ceiling is 95%)
unborrowed       ≈   8_358_340 XLM (6.4% of supply)
```

This is a real, busy testnet pool, not an empty shell. For scale: a Strata
demo epoch of a few hundred XLM is nothing next to it, which is good for
liquidity and bad for signal — the pool's state moves for reasons unrelated to
the adapter.

### 4.3 Supplying and withdrawing

Both are `Request` structs passed to one function (S1, S2):

```rust
pub struct Request { pub request_type: u32, pub address: Address, pub amount: i128 }
fn submit(from: Address, spender: Address, to: Address, requests: Vec<Request>) -> Positions
```

| `request_type` | Name in source | `amount` means |
| --- | --- | --- |
| 0 | `Supply` | underlying tokens to deposit (uncollateralised) |
| 1 | `Withdraw` | underlying tokens to withdraw from the uncollateralised supply |

Auth, exactly as implemented: `spender.require_auth()`, plus `from.require_auth()`
when `from != spender`. Neither `from`, `spender` nor `to` may be the pool's own
address (`Error 1200`). Tokens move as `transfer(spender → pool)` on supply and
`transfer(pool → to)` on withdraw.

Supplying mints bTokens:

```
b_tokens_minted = floor(amount · 1e12 / b_rate)      // to_b_token_down
```

Withdrawing converts the requested token amount into bTokens, **rounding up**:

```
to_burn    = ceil(amount · 1e12 / b_rate)           // to_b_token_up
tokens_out = amount
```

If `to_burn` exceeds the position, the request is clamped:

```rust
if to_burn > cur_b_tokens {
    to_burn    = cur_b_tokens;
    tokens_out = floor(cur_b_tokens · b_rate / 1e12);
}
```

This is the silent clamp referred to in §1. **No error, no signal.** Verified in
§5.

Before the transfer the reserve must satisfy `utilization < 100%`
(`require_utilization_below_100`, `Error 1207`). Individual reserves and the pool
as a whole are also gated by status (§4.5).

### 4.4 How a position is accounted for

A supply position is a count of **bTokens** in `Positions.supply`, keyed by
reserve index. bTokens are not transferable and have no token contract. Their
value is

```
value = floor(b_tokens · b_rate / 1e12)
```

where `b_rate` starts at 1e12 and only ever moves in one direction: up as
interest accrues, down when bad debt is socialised (§4.6). Interest is not
pushed to holders; `b_rate` is recomputed lazily the next time the reserve is
loaded, and it is stored at 12 decimals (v2). `Reserve::load` short-circuits if
the reserve has already been updated in the current ledger.

Two reads are needed to value a position:
`get_positions(account)` → bTokens, and `get_reserve(asset)` → `b_rate`. There is
no single "value of my position" view.

Backstop take: 10% of borrower interest accrues to the backstop before `b_rate`
is credited, so a supplier earns roughly 90% of borrow interest × utilization.
There are no deposit, withdrawal or performance fees.

### 4.5 What makes a withdrawal fail or pay less

| Condition | Behaviour | Error |
| --- | --- | --- |
| Pool status 4, 5 or 6 (frozen / setup) | **supply** blocked; withdraw still allowed | `InvalidPoolStatus` 1206 |
| Pool status 2 or 3 (on ice) | supply still allowed; borrow/cancel blocked | — |
| Reserve `enabled = false` | supply blocked; withdraw still allowed | `ReserveDisabled` 1223 |
| Utilization would reach 100% after the withdrawal | the whole call reverts | `InvalidUtilRate` 1207 |
| `supply_cap` reached | supply reverts | `ExceededSupplyCap` 1220 |
| Request larger than the position | **clamped, no error** | — |
| Asset not in the pool | reverts | `InternalReserveNotFound` 1209 |

So there are exactly two ways a withdrawal can disappoint: it reverts (util 100%,
missing asset, or the token transfer itself failing), or it silently pays less
than asked because the request exceeded the position. The second only happens if
the adapter asks for more than it owns, which it should never do — but it must
still check, because a stale or mis-computed bToken count would produce exactly
that.

### 4.6 Bad debt, and whether principal can be lost

Yes. S1 describes the path: an underwater borrower is liquidated; when only debt
remains, `bad_debt()` moves it to the backstop; if a bad debt auction cannot
clear and the backstop is below 5% of its threshold, the remaining debt is
burned, which **socialises the loss across all suppliers of that asset**. In code
terms `b_rate` falls, so every holder's position is worth less in the same
proportion. There is no per-depositor protection and no way for a supplier to opt
out other than not supplying.

This is the same shape as a falling yield rate in the mock vault, which the
waterfall already handles: the loss lands on `V`, and the senior/junior split
does the rest. It is not tested — inducing bad debt on a shared testnet pool is
not something this research should attempt.

### 4.7 TTL, and who has to renew it

From S2, `pool/src/storage.rs` (ledger = 5s):

| Entry | Threshold | Bumped to | Bumped by |
| --- | --- | --- | --- |
| Pool instance | ~30 days | ~31 days | every pool entry point |
| Shared reserve data | ~45 days | ~46 days | pool reads/writes |
| **User positions** | **~100 days** | **~120 days** | `get_user_positions` (a read) and `set_user_positions` |

A contract cannot extend another contract's persistent entries. The pool extends
its own on access, so the only thing that keeps the adapter's `Positions` entry
alive is somebody touching it — the adapter on any call, or anyone at all
calling `get_positions(adapter)`, since the view itself bumps the TTL.

And if it does expire, the consequence is severe. `get_user_positions` falls back
to `Positions::env_default` — **an empty position, not an error**. The reserve's
`b_supply` total is unaffected, so the tokens are still accounted for in the
pool, but the adapter's claim on them reads as zero. A withdrawal would then
compute `to_burn = 0` and revert on `remove_supply(0)` (`InvalidBTokenBurnAmount`,
1217). Recovering would mean restoring an archived entry by key.

The manager's maximum term is one year. The Blend position's TTL is about four
months. **An epoch longer than roughly 120 days can outlive the position it
depends on.** This is the single most important operational finding in this note.

### 4.8 What the admin can do

The pool admin can change reserve parameters through a 7-day queue
(`queue_set_reserve` → `set_reserve`), freeze the pool (`set_status` 4), and
propose a new admin. For a supplier the relevant powers are: freezing supply,
disabling a reserve, and re-parameterising its supply cap. The admin cannot
withdraw user funds, and cannot change `b_rate` except through the bad-debt path
above. Trust in the admin is still trust in a party.

---

## 5. What was exercised on testnet

A throwaway identity was created for this research
(`strata-research-01`, `GDU25XLZRXPFMFLYS2G6E3NERIK5QOY7BBCU44LVSZEOMIK3B6EKNNIJ`,
funded by Friendbot). The demo identities were not used. Amounts are stroops
(1 XLM = 10 000 000).

| # | Action | Requested | Result | Transaction |
| --- | --- | --- | --- | --- |
| 1 | supply XLM | 100 000 000 | 40 493 809 bTokens minted, position `{0: 40493809}` | [`d4fe560f…`](https://stellar.expert/explorer/testnet/tx/d4fe560fcd7484075cf872d76808994174c768933d58762e2886aeb1b8d7b86e) |
| 2 | read position | — | `{"collateral":{},"liabilities":{},"supply":{"0":"40493809"}}` | — |
| 3 | withdraw | 100 000 000 | paid exactly 100 000 000, burned 40 493 749, **60 bTokens left over** | [`e0577f11…`](https://stellar.expert/explorer/testnet/tx/e0577f11194a0c5b03ddbb609f50e7efdbf6e3ad217ffbc4617688bc883a1aa8) |
| 4 | supply XLM | 50 000 000 | 20 246 837 bTokens minted | [`4d7cc484…`](https://stellar.expert/explorer/testnet/tx/4d7cc48427d0331b012b88ce09301823f8cc4e3169a66cdb0b633b5f63f35d90) |
| 5 | withdraw | 10 000 000 000 (1000 XLM, 200× the position) | **clamped**: burned all 20 246 897 bTokens, paid 50 000 175, position emptied, **no error** | [`9cd4c056…`](https://stellar.expert/explorer/testnet/tx/9cd4c05693b987b7a7f488439b1da03a8f4dbc473a238d85acffa199601e3726) |

Rows 4 and 5 are ten seconds apart (ledger close times 1792941717 and
1792941727). Row 4 minted 20 246 837 bTokens; row 5 burned 20 246 897, the
difference being the 60 bTokens of dust row 3 left behind.

Four things this establishes, which the source predicted:

1. **Supply and withdraw both work with the existing XLM SAC.** No token
   change is needed and no trustline was required.
2. **Rounding is against the depositor on the way in and in the pool's favour on
   the way out.** 100 000 000 stroops minted 40 493 809 bTokens; withdrawing the
   same 100 000 000 stroops burned 40 493 749 of them, leaving 60 bTokens (≈148
   stroops) behind. Withdrawing an exact token amount does not withdraw exactly
   the same bTokens.
3. **The over-withdrawal clamp is real and silent** (row 5). The API returned
   success, the event reported `tokens_out = 50 000 175` against a request of
   10 000 000 000, and the position was emptied. Nothing in the return value says
   "you asked for more than you had".
4. **The pool does accrue, and the measurement is too coarse to price it.**
   From the two mints and burns the reserve's `b_rate` was in
   `(2_469_521_413_664, 2_469_521_535_635]` at row 4 and at least
   `2_469_522_860_712` at row 5, so it rose over those ten seconds — the pool is
   genuinely holding and accruing a real lending position. Most of the 175
   stroops returned above is the 148 stroops of dust from row 3, not interest:
   at 12 decimals and this `b_supply`, one unit of `b_rate` is worth 527 stroops,
   so a ten-second window cannot be read as an annual rate. No rate is claimed
   here.

**Not exercised, and therefore not claimed:** a withdrawal against a pool at
100% utilization; a frozen pool; bad debt; TTL expiry; and — importantly — a
**contract** acting as the pool user. Rows 1–5 used a keypair. The adapter's own
authorisation path is analysed in §7 but has not been run.

---

## 6. Mismatch table

| # | The manager assumes | Blend actually does | Consequence | Handled by |
| --- | --- | --- | --- | --- |
| M1 | `deposit` returns shares that `redeem(shares)` will burn | bTokens, non-transferable, 12-decimal `b_rate` | the adapter needs its own share ledger; bTokens are not an ERC-4626 share | adapter |
| M2 | deposit and redeem are asset-exact | deposit floors, withdraw ceils, and an over-request is clamped | value can be lost in dust; a short withdrawal is silent | adapter, with a measured balance delta |
| M3 | `redeem` can withdraw the entire position at any time | blocked at utilization ≥ 100% (`1207`) | settlement can revert; funds wait for repayment | unchanged risk R8, now real |
| M4 | the vault holds the asset and pays it out | the pool lends it out; **6.4% of supply was unborrowed** at the time of reading | large redemptions depend on borrowers repaying, not on the vault being solvent | unchanged risk R8 |
| M5 | principal cannot be lost | bad debt is socialised; `b_rate` falls | `V` can arrive below `S + J`; the waterfall already handles this | waterfall, unchanged |
| M6 | the position persists | user entries expire after ~120 days idle and then read as **empty** | an epoch longer than ~120 days can orphan its own position | adapter + operations (§7.4) |
| M7 | `deposit` cannot mint zero shares | the pool panics on a zero-bToken supply (`1216`) | the pool protects the adapter here; the adapter should still check | either |
| M8 | one call, one asset, one amount | `submit` takes a vector of requests and moves tokens itself | adapter authorises a token pull it does not itself perform | adapter, `authorize_as_current_contract` |
| M9 | supply is always possible | status 4–6 blocks supply (`1206`) | a frozen pool blocks new epochs, not settlement | adapter surfaces, manager unchanged |
| M10 | a vault is a dumb holder | the pool has an admin, an oracle and a backstop | new trust assumptions enter the system | risks B1–B3 |

No mismatch requires a change to the waterfall, the spec, the manager or the
interface.

---

## 7. Proposed adapter

### 7.1 Shape

A new crate, `contracts/blend-adapter`, implementing `VaultInterface` as it
stands (`asset`, `deposit`, `redeem`, `total_assets`, `convert_to_assets`,
`balance_of`). Immutable configuration at construction: the pool address, the
underlying asset, and the one address allowed to deposit and redeem — the epoch
manager. No upgrade function, no admin, no fees.

Internally it keeps three numbers and one mapping:

```
pool, asset, manager        // immutable config
total_b_tokens              // the adapter's Blend position (mirror, checked on read)
total_shares                // adapter shares outstanding
shares_of[account]          // per-account share ledger
```

If the manager is the only permitted depositor, `total_shares == shares_of[manager]`
at all times; the ledger exists anyway so the contract is honest about what it
is and so a future second depositor does not require a redesign.

### 7.2 The six functions in plain English

- **`asset()`** — returns the pinned XLM SAC address. The manager stores it and
  will refuse anything else for the life of the epoch.
- **`deposit(assets, receiver)`** — only the manager may call. Pull `assets`
  from the manager, then supply them to the pool with
  `submit(from=adapter, spender=adapter, to=adapter, [Supply(asset, assets)])`.
  Mint adapter shares at the pre-deposit price. Return the shares minted.
- **`redeem(shares, receiver, owner)`** — only the manager may call, and only for
  its own `owner`. Compute `assets = floor(shares · total_assets / total_shares)`,
  record the token balance, call `submit(... [Withdraw(asset, assets)])`, then
  read the token balance again. Pay the manager; burn the shares.
- **`total_assets()`** — `floor(total_b_tokens · b_rate / 1e12)`, read live from
  the pool. `b_rate` moves without the adapter doing anything, so this cannot be
  a stored number.
- **`convert_to_assets(shares)`** — the same pro-rata expression, floored, read
  only.
- **`balance_of(account)`** — the share ledger.

### 7.3 The two decisions that matter

**The withdrawal is validated by measurement, not by trust.** After
`submit(Withdraw)`, the adapter reads its own token balance and compares the
delta with the `assets` it computed. If it received less, it returns a specific
error — `InsufficientWithdrawal { requested, received }` — and the whole
invocation reverts, so the pool call is undone too. This is what turns Blend's
silent clamp into a loud failure, as the brief requires. Because Soroban rolls
back the entire invocation on a returned error, there is no half-withdrawn state
to clean up.

**The supply needs an authorisation the adapter has to pre-declare.** The token
transfer on a supply is `pool → token.transfer(spender=adapter → pool)`, which
sits two frames below the adapter. The adapter must call
`authorize_as_current_contract` with an entry for that exact transfer before it
calls `submit` — the same pattern, with the same argument list, as the epoch
manager's `transfer_auth_entry`. This is the most likely place for the design to
fail in practice, and it is the first thing Phase 2 should test with the auth
tree inspected rather than `mock_all_auths`.

### 7.4 Operations: keeping the position alive

Because the Blend user entry expires after ~120 days idle (§4.7), the adapter
needs one of:

- a permissionless `refresh()` entry point that calls `get_positions(adapter)`
  on the pool, refreshing the TTL without changing state; or
- an external keeper doing the same thing, which needs no adapter change at all
  because the pool's own view bumps the entry; or
- a documented rule that epoch terms stay under ~100 days.

The first is cheap and belongs in the adapter. The third is a policy that will
be forgotten. Recommendation: implement `refresh()`, document the 100-day
constraint, and treat a missing keeper as a denominated risk (§8, B4).

### 7.5 Reentrancy and call ordering

The pool moves tokens via SEP-41 `transfer`, which has no receiver hook, so
there is no callback path into the adapter from a well-behaved token. The
adapter should still order its own effects first — decrement shares and record
the withdrawal before the outbound transfer — and treat any transfer failure as
a revert. That is a discipline, not a proof: a malicious token *could* call back,
and that is true of every contract in this repository today. Nothing in Phase 1
tested it.

---

## 8. Risks this adds

New entries for `docs/risks.md` in Phase 2. All are **open**; none is mitigated
by anything that exists yet.

**B1 — Blend pool risk — High, open.** The adapter delegates custody to a third
party's contract, its admin, its oracle and its backstop. A bug in the pool, a
freeze, a compromised admin, or a bad-debt event all reach Strata depositors
directly. The pool on testnet has a mock oracle and an admin key; neither is a
production arrangement.

**B2 — Liquidity at settlement — High, open.** Settlement redeems the entire
position at one moment. At the time of the read, 6.4% of the XLM reserve was
unborrowed. A larger position, a higher utilization, or a settlement during a
borrowing spike can revert `settle`, and `docs/risks.md` R8 already says a
reverting redeem blocks every depositor's claim. Permissionless settlement means
anyone can retry; it does not mean the liquidity appears.

**B3 — Silent partial withdrawal — Medium, open.** Blend clamps an over-sized
withdrawal instead of rejecting it (verified, §5). The adapter's balance-delta
check is designed to catch this, but it is unbuilt and untested, and a clamp that
lands exactly on the computed amount would be indistinguishable from success.

**B4 — Blend position TTL — High, open.** An idle Blend position entry expires
after ~120 days and reads back as empty, which would orphan the epoch's entire
position. Strata's maximum term is 365 days. The mitigation (`refresh()` plus a
keeper plus a term limit) is a plan, not a control, until it is tested.

**B5 — Rounding dust — Low, open.** Each deposit and withdrawal leaves a few
base units of bTokens behind (60 bTokens observed on a 100 000 000-stroop
round-trip). Dust accrues to remaining holders rather than being lost, so this is
a fairness wart, not a leak — but it means the adapter's accounting must never
assume "shares in equals shares out".

**B6 — Second asset to trust — Medium, open.** `asset()` is pinned at epoch
creation. If someone constructs the adapter over a pool that lists the wrong
contract for "XLM", nothing detects it. The adapter should verify at construction
that `get_reserve(asset)` succeeds, which turns a silent misconfiguration into a
failed deployment.

**B7 — Testnet economics are not economics — Low, documented.** The testnet pool
is driven by a mock oracle and test actors. Running an epoch through it proves
the plumbing; it proves nothing about yield, liquidity or loss behaviour under
real conditions.

---

## 9. Recommendation, and what it depends on

**Go**, subject to three conditions that should be met before the adapter is
described as anything more than a testnet experiment:

1. **The contract-as-pool-user path works.** The adapter's own
   `authorize_as_current_contract` entry must be verified by inspecting the auth
   tree in a test, not by `mock_all_auths`. If that cannot be made to work, the
   adapter design fails at the first deposit and the whole approach needs
   rethinking. This is the gate.
2. **TTL is answered in code, not prose.** `refresh()` exists, a test proves the
   adapter's position survives an idle period, and the term limit is enforced or
   documented as an operational requirement.
3. **The withdrawal check is proven against the real clamp.** A test double that
   reproduces Blend's clamp (not merely an error) plus a test that the adapter
   refuses a short payment.

What this research supports: a Blend-backed Strata epoch on testnet is buildable
without touching the settlement math or the manager, and the mismatches are
enumerable and small. What it does not support: any claim that Strata is safe
against a real lending pool, or that the testnet pool is representative.

The honest one-line summary for a reviewer: *the mock vault pays an
admin-chosen rate; the adapter's vault pays whatever a real lending pool's
bToken exchange rate did, including less than it started with.*

---

## 10. How it works, for explaining it to someone else

1. Depositors put XLM into the epoch manager and pick a tranche. The manager pulls
   the XLM and hands it to its vault — which, in this design, is the adapter.
2. The adapter is a contract that owns exactly one position: a Blend supply of
   XLM. It takes the manager's XLM, supplies it to the Blend pool, and writes down
   how much of itself the manager now owns.
3. Blend lends that XLM to borrowers. The adapter's claim is a count of bTokens,
   and one bToken is worth `b_rate` XLM, `b_rate` rising as borrowers pay interest
   and falling if the pool has to write off bad debt.
4. At maturity the manager asks the adapter for everything. The adapter asks the
   pool to withdraw, measures how much XLM actually arrived, and pays the manager
   that number. That number — not a report, not an estimate — is what the
   waterfall splits: the senior takes its fixed target first, the junior takes the
   rest, and if the pool lost money the junior is wiped out before the senior is
   touched.
5. Every awkward case is decided at the boundary: the pool's bToken rounding, its
   silent clamp, its utilization gate, its status gates, and the expiry of its
   storage entries. None of those reach the waterfall, because the waterfall only
   ever sees what arrived.
