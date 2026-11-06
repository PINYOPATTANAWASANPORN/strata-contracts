# Settlement ordering and front-running around maturity

**Strata is unaudited and testnet-only.** This is an analysis of one codebase as
it stands, not an audit. It closes issue #9 and it changed no contract, no spec
and no payout arithmetic. Every figure below was measured by a test in
`tests/src/settlement_ordering.rs`; nothing is estimated.

The question: around maturity, the epoch manager's transitions are separated by a
clock and by who may call them. What can a transaction ordering inside that
window change, who can profit from it, and which parts of it are real rather than
properties of the mock vault?

## The only clock the manager reads

`contracts/epoch-manager/src/lib.rs` reads `Env::ledger().timestamp()` in exactly
four places — `deposit` (line 323), `settle` (419), `create_epoch` (620) and the
`seconds_to_maturity` view (753). It never reads `ledger().sequence()`. There is
no per-transaction clock and no sub-ledger ordering: **two calls in the same
ledger see the same time**, and the granularity of every ordering question below
is one ledger (about 5 seconds on testnet).

That gives two boundaries, not one:

- deposits are accepted while `now < maturity_ts`, and refused at
  `now >= maturity_ts` (`Error::DepositsClosed`);
- settlement is refused while `now < maturity_ts` (`Error::NotMature`), so the
  maturity ledger itself is the first ledger at which both "deposits closed" and
  "settlement possible" are true.

`maturity_ts` is fixed at `start_ts + term_seconds` when the admin opens the
epoch, and `term_seconds` is stored on the epoch. A later `settle` does not
shorten, extend or re-price the term: it applies the waterfall to `V` with the
original `term_seconds`.

## The transitions

| Transition | Who may call | Constraints | Clock / state it reads | What it can change |
| --- | --- | --- | --- | --- |
| `create_epoch` | admin (`require_auth`) | no epoch, or the last one is `Closed`; term, rate and ratio within spec bounds | timestamp, once, into `start_ts`; `maturity_ts = start_ts + term` | opens the window; snapshots rate and ratio for the whole epoch |
| `deposit` | the depositor (`from.require_auth`) | status `Open`; `now < maturity_ts`; amount bounds; senior gate on post-deposit totals, junior ungated | timestamp; vault's live share price | adds principal at full weight, however late |
| `settle` | **anyone**, no authorisation | status `Open`; `now >= maturity_ts`; vault shares > 0; token balance ≥ owed | timestamp; vault `balance_of` + `redeem` return | fixes `V` and both payouts for good |
| `claim` | the claimant (`claimant.require_auth`) | status `Settled`; principal > 0 | no clock; epoch totals only | pays one depositor; the last claim in a tranche takes the remainder |
| `close_epoch` | **anyone**, no authorisation | status `Settled`; both `*_unclaimed == 0` | no clock | frees the manager for the next epoch |
| `position_of`, `senior_room`, `project`, `seconds_to_maturity` | anyone, read-only | — | `position_of` reads the vault's live valuation while `Open`; `seconds_to_maturity` reads the timestamp | nothing |

Two consequences are worth stating plainly, because they are the whole analysis:

1. **No transition pays its caller.** `settle` and `close_epoch` have no
   identity parameter and no reward. There is no fee to compete for, so the
   "front-running" question is not "who collects the MEV" but "whose valuation
   moment is used".
2. **The deposit window closes at the same ledger that settlement opens.** There
   is no gap in which the composition of the pool is fixed but the outcome is
   not.

## A. A deposit in the final ledger

Deposits are accepted in the last second before maturity and carry full principal
weight into the waterfall. That is by design — `deposit` documents that the
tokens are locked into the vault immediately, so a position earns from the moment
it is made — but the *senior target* is computed from the epoch's fixed term, not
from how long the principal was present.

Measured with a 10%/yr senior target and a 10%/yr vault, one-year term
(`a_final_ledger_senior_deposit_earns_a_whole_terms_target_interest`):

| | no late depositor | 200 000 senior at `maturity - 1s` |
| --- | --- | --- |
| junior principal | 400 000 | 400 000 |
| senior principal | 200 000 | 400 000 |
| `V` | 660 000 | 859 999 |
| senior payout | 220 000 | 440 000 |
| junior payout | 440 000 | **419 999** |
| the late depositor's claim | — | **220 000** |

The mechanics are worth separating, because they locate the defect. The vault is
*fair*: 600 000 has accrued 59 999 by `maturity - 1`, so the late 200 000 buys
181 818 shares at the correct 659 999/600 000 price, and redeeming the whole
781 818-share position at maturity returns 859 999. Nothing is mispriced. The
transfer happens in the waterfall: `I` is computed on 400 000 of senior principal
over the full term, so the late depositor's 200 000 earns 20 000 for one second
of exposure, and `V` cannot cover it — the junior's payout falls by 20 001 (one
stroop of the difference is the mock vault's accrual truncation).

The junior side is the same economics without a gate
(`a_final_ledger_junior_deposit_dilutes_the_earlier_juniors_share`). Instead of
target interest, the dilution is pro-rata pooling (R5): 400 000 junior at `t0`
plus 400 000 at `maturity - 1` gives a junior tranche of 839 999 over 800 000 of
principal, so the junior who funded the year takes 419 999 and the one who
arrived for the last second takes 420 000 — 20 001 of yield moved for one
ledger of exposure. Junior deposits are ungated on purpose; nothing here
contradicts the spec, which is exactly why it needs a decision rather than a
patch.

**Bound.** Per epoch: the senior gate still caps senior principal at
`J * ratio / 10_000` with ratio ≤ 1.0x, and the rate is capped at 100%/yr. The
late depositor must lock the principal until settlement, so the cost is one
ledger of exposure plus the wait for everyone else to claim before `close_epoch`
allows another epoch. It can be repeated once per epoch. It is not bounded by
anything else: there is no minimum holding period, no per-depositor cap and no
time-weighting.

**Verdict: real, measured, unfixed.** Filed as **issue #32** and recorded as
**R19** in `docs/risks.md`. Any fix is a spec or design change — close deposits
before settlement, impose a holding period, or time-weight the senior target.

## B. Settling at the first eligible ledger, or later

Settlement is permissionless and there is no deadline. `V` is whatever the vault
returns in the settlement ledger, so the party who submits `settle` chooses the
valuation moment for everyone. Measured
(`settling_later_moves_the_junior_and_leaves_the_senior_where_it_was`), same
epochs, settled at maturity versus thirty days later:

| 10%/yr vault | at maturity | 30 days late |
| --- | --- | --- |
| `V` | 660 000 | 664 931 |
| senior payout | 220 000 | 220 000 |
| junior payout | 440 000 | 444 931 |

| −10%/yr vault | at maturity | 30 days late |
| --- | --- | --- |
| `V` | 540 000 | 535 069 |
| senior payout | 220 000 | 220 000 |
| junior payout | 320 000 | 315 069 |

The senior is capped, so every stroop of post-maturity gain or loss lands on the
junior until the cushion is gone. Settling late in a gain is good for the junior;
settling late in a loss is bad for it. Since anyone can settle at maturity, a
junior who fears a post-maturity loss can protect itself by settling promptly,
but it cannot capture a post-maturity gain if somebody else settles first. That
asymmetry is real, and it favours the earliest caller. It grants no outsider a
profit: the caller receives nothing, and `claim` pays only positions.

The genuinely exploitable version of this is not the caller, it is `V`. If the
vault's price can be moved inside the settlement ledger — a manipulable oracle, a
pool whose utilisation can be pushed, a paused or illiquid pool — then whoever can
move it picks the valuation. That is R6 and B1/B2, not a property of the
manager. On the mock vault the only mover is the admin (R10), and the mock's rate
is forward-only: `set_yield_rate` accrues at the old rate before storing the new
one, so it cannot re-price the past.

**Verdict: real but not attacker-profitable for a Strata caller; bounded for the
mock; the real vector is a manipulable `V`, which is already tracked.**

## C. Claim order

Senior and junior claims are independent: each tranche pays out of its own fixed
`senior_payout` / `junior_payout`, so claiming in one tranche cannot change the
other. Within a tranche, every claim but the last is
`floor(payout * principal / tranche_total)`, and the last claim in the tranche is
paid `payout - paid`, which absorbs the rounding remainder.

Measured (`claim_order_only_decides_who_takes_the_rounding_remainder`): a senior
payout of 105 010 over principals of 33 333 and 66 667 pays 35 002 then 70 008;
in the reverse order, 70 007 then 35 003. The last claimer takes one stroop more
than its own floored pro-rata share, and the tranche total is exact either way.

**Verdict: bounded dust, not a risk.** The remainder is at most
`(number of depositors in the tranche - 1)` stroops, only a claimant can take it,
and a claimant must hold a position. It is a fairness curiosity, not a lever.

## D. A rate change in the final ledger

The mock vault's `set_yield_rate` is an admin power with no market behind it
(R10), and it is the only way to move `V` in this test environment. Because it
accrues before changing the rate, a change in the final second can only re-price
that second. Measured
(`a_rate_change_in_the_final_ledger_moves_the_vault_by_one_stroop`): with 800 000
in the vault and a −1000%/yr rate set one second before maturity, `V` at
settlement is 879 999, against 880 000 with no change. One stroop, and it comes
from the vault flooring the `maturity - 1` accrual, not from the rate change,
which accrued zero in the remaining second.

**Verdict: mock artefact for ordering.** It shows the mock cannot be manipulated
into a retrospective re-pricing, which is worth knowing for the tests, but it
says nothing about a vault with a real, movable price. That case is B1/B2 and R6.

## What is real, what is bounded, what is an artefact

| Finding | Classification | Where it is tracked |
| --- | --- | --- |
| A senior deposit in the final ledger earns a full term's target, paid by the junior (20 000 on 200 000 for one second at 10%/yr) | **Real**, measured, unfixed, repeatable once per epoch | issue #32, R19, test names above |
| A junior deposit in the final ledger dilutes the earlier junior by the same mechanism | **Real**, inherent to pooled yield; no gate by design | R5, R19 |
| Settlement timing moves post-maturity P&L to or from the junior, and the caller chooses the moment by submitting first | **Real**, but no caller is paid and early settlement is always available | R19, section B |
| A vault whose `V` can be moved in the settlement ledger lets its mover choose the valuation | **Real for such a vault**; the ordering half is this document, the market half is untested | R6, B1, B2 |
| Claim order decides who takes the rounding remainder | **Bounded dust** (≤ depositors − 1 stroops) | section C |
| A final-ledger rate change on the mock vault moves `V` by one stroop of truncation | **Artefact** of the mock's admin-set yield | R7, R10, section D |
| No transition pays its caller; `settle` and `close_epoch` are permissionless and free | Design property, verified | `settling_pays_the_caller_nothing` |

## What this does not cover

- **A manipulable vault.** The analysis assumes `V` is whatever the vault
  honestly returns at that ledger. Whether a real Blend pool can be pushed at
  settlement is B2 territory and has not been measured.
- **Sub-ledger ordering.** Soroban applies transactions sequentially and the
  manager reads only `timestamp()`, so there is nothing to analyse below one
  ledger. Two deposits in the same ledger are ordered, but neither is closer to
  maturity than the other.
- **Multi-epoch strategies.** One epoch at a time is enforced by `close_epoch`,
  but an actor can repeat the deposit pattern across epochs; this document
  measures one instance, not a strategy over many.
- **Gas, fees and failed transactions.** No attempt is made to price the cost of
  the ordering, or to model a mempool that does not exist in the same form on
  Stellar.
- **The junior's other asymmetries.** R5, R3 and R4 are separate economic
  questions; this document only measures the ordering channel into them.

## Reproducing

```bash
cargo test -p strata-tests settlement_ordering -- --nocapture
```

The eight tests print the figures quoted above. The settlement amounts are
recomputed with `strata_waterfall::settle` from the `V` the vault actually
returned, so the contract's payout is never used as its own expectation, and the
vault values are derived from the mock's documented accrual rule rather than read
back.
