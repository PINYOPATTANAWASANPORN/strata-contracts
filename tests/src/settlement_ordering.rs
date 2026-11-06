//! Settlement-ordering analysis: who can call each lifecycle transition, which
//! ledger time it reads, and what transaction ordering around maturity can
//! change.
//!
//! Companion to `docs/analysis/settlement-ordering.md`. Nothing here changes a
//! contract; the point is to measure the ordering effects rather than argue
//! about them.
//!
//! Every expected payout is recomputed with `strata_waterfall::settle` from the
//! `V` the vault actually returned, and every expected vault value is derived
//! from the mock vault's documented accrual rule rather than read back as its
//! own expectation.

use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};
use strata_epoch_manager::{EpochManager, EpochManagerClient, Error, Status, Tranche};
use strata_mock_vault::{MockVault, MockVaultClient};
use strata_waterfall::{settle, SECONDS_PER_YEAR};

/// One year, so the senior target is a whole year's rate.
const TERM: u64 = SECONDS_PER_YEAR;
/// 10%/yr senior target.
const RATE: u32 = 1_000;
/// 1.0x, so the senior cap is exactly the junior principal.
const RATIO: u32 = 10_000;
/// 10%/yr vault yield, one accrual window for the whole term.
const VAULT_RATE: i32 = 1_000;

const STARTING_BALANCE: i128 = 1_000_000;
/// Unbacked accounting needs a funded strategy reserve to pay redemptions.
const STRATEGY_RESERVE: i128 = 1_000_000_000_000_000;

const THIRTY_DAYS: u64 = 30 * 24 * 60 * 60;

struct World {
    env: Env,
    manager: Address,
    vault: Address,
    asset: Address,
    senior_a: Address,
    senior_b: Address,
    junior: Address,
    bystander: Address,
}

fn world() -> World {
    let env = Env::default();
    let admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let vault = env.register(MockVault, (admin.clone(), asset.clone()));
    let manager = env.register(EpochManager, (admin.clone(), vault.clone()));

    let senior_a = Address::generate(&env);
    let senior_b = Address::generate(&env);
    let junior = Address::generate(&env);
    let bystander = Address::generate(&env);

    env.mock_all_auths();
    for who in [&senior_a, &senior_b, &junior, &bystander] {
        StellarAssetClient::new(&env, &asset).mint(who, &STARTING_BALANCE);
    }
    StellarAssetClient::new(&env, &asset).mint(&vault, &STRATEGY_RESERVE);

    World {
        env,
        manager,
        vault,
        asset,
        senior_a,
        senior_b,
        junior,
        bystander,
    }
}

impl World {
    fn mgr(&self) -> EpochManagerClient<'_> {
        EpochManagerClient::new(&self.env, &self.manager)
    }

    fn vault_client(&self) -> MockVaultClient<'_> {
        MockVaultClient::new(&self.env, &self.vault)
    }

    fn token(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.asset)
    }

    fn balance(&self, who: &Address) -> i128 {
        self.token().balance(who)
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp() + seconds;
        self.env.ledger().set_timestamp(now);
    }

    /// Opens an epoch at the current ledger time and sets the vault's yield
    /// from that same moment, so one accrual window covers the whole term.
    fn open(&self, vault_rate: i32) {
        self.mgr().create_epoch(&TERM, &RATE, &RATIO);
        self.vault_client().set_yield_rate(&vault_rate);
    }

    fn deposit_senior(&self, who: &Address, amount: i128) -> i128 {
        self.mgr().deposit(who, &Tranche::Senior, &amount)
    }

    fn deposit_junior(&self, who: &Address, amount: i128) -> i128 {
        self.mgr().deposit(who, &Tranche::Junior, &amount)
    }
}

// --- the deposit boundary ----------------------------------------------------

#[test]
fn deposits_are_open_in_the_last_ledger_before_maturity_and_closed_at_it() {
    let w = world();
    w.open(0);
    w.deposit_junior(&w.junior, 400_000);

    // The last second before maturity is still inside the window.
    w.advance(TERM - 1);
    assert!(w
        .mgr()
        .try_deposit(&w.senior_a, &Tranche::Senior, &1_000)
        .is_ok());

    // The maturity ledger itself is not.
    w.advance(1);
    assert_eq!(
        w.mgr().try_deposit(&w.senior_b, &Tranche::Senior, &1_000),
        Err(Ok(Error::DepositsClosed)),
    );

    // The manager reads `ledger().timestamp()`, not a sequence number, and the
    // boundary is `now >= maturity_ts`. Two calls in the same ledger therefore
    // see the same clock.
    let e = w.mgr().current_epoch().unwrap();
    assert_eq!(e.maturity_ts, e.start_ts + TERM);
    assert_eq!(w.env.ledger().timestamp(), e.maturity_ts);
}

// --- the scenario that matters ----------------------------------------------

#[test]
fn a_final_ledger_senior_deposit_earns_a_whole_terms_target_interest() {
    let w = world();
    w.open(VAULT_RATE);
    w.deposit_junior(&w.junior, 400_000);
    w.deposit_senior(&w.senior_a, 200_000);

    // One second of exposure, at the last moment deposits are accepted.
    w.advance(TERM - 1);
    w.deposit_senior(&w.senior_b, 200_000);
    w.advance(1);

    w.mgr().settle();
    let e = w.mgr().current_epoch().unwrap();

    // The vault: 600 000 accrues 59 999 over TERM-1, the late 200 000 buys
    // 181 818 shares at that price, and the final second rounds to zero.
    assert_eq!(e.value_redeemed, 859_999);

    // The split is recomputed from the spec, never taken from the contract.
    let expected = settle(e.senior_total, e.junior_total, RATE, TERM, e.value_redeemed).unwrap();
    assert_eq!(e.senior_payout, expected.senior_payout);
    assert_eq!(e.junior_payout, expected.junior_payout);
    assert_eq!((e.senior_payout, e.junior_payout), (440_000, 419_999));

    // Had the late depositor not arrived, the same epoch paid the junior
    // 440 000: the full-year target on 200 000 senior principal is 20 000,
    // and that is where it came from.
    let without = settle(200_000, 400_000, RATE, TERM, 660_000).unwrap();
    assert_eq!(without.junior_payout, 440_000);

    // The late depositor's own claim: 200 000 principal plus 20 000, for one
    // second of exposure.
    let late_claim = w.mgr().claim(&w.senior_b, &Tranche::Senior);
    assert_eq!(late_claim, 220_000);
    assert_eq!(late_claim - 200_000, 20_000);

    eprintln!(
        "ordering: senior deposited 200000 at maturity-1s and claimed {late_claim} (+20000); \
         the junior received {} instead of 440000 ({} less)",
        e.junior_payout,
        440_000 - e.junior_payout
    );
}

#[test]
fn a_final_ledger_junior_deposit_dilutes_the_earlier_juniors_share() {
    let w = world();
    w.open(VAULT_RATE);
    w.deposit_junior(&w.junior, 400_000);
    w.deposit_senior(&w.senior_a, 200_000);

    w.advance(TERM - 1);
    // A second junior, one second before the window closes. Junior deposits
    // are ungated by design, so nothing stops this.
    w.deposit_junior(&w.senior_b, 400_000);
    w.advance(1);

    w.mgr().settle();
    let e = w.mgr().current_epoch().unwrap();
    assert_eq!(e.value_redeemed, 1_059_999);

    let expected = settle(e.senior_total, e.junior_total, RATE, TERM, e.value_redeemed).unwrap();
    assert_eq!(e.senior_payout, expected.senior_payout);
    assert_eq!(e.junior_payout, expected.junior_payout);

    // The junior tranche pays 839 999 for 800 000 of principal. The first
    // junior, who funded the whole term, takes 419 999 of it; the depositor
    // who arrived in the final second takes the other 420 000.
    let early_claim = w.mgr().claim(&w.junior, &Tranche::Junior);
    assert_eq!(early_claim, 419_999);
    assert_eq!(440_000 - early_claim, 20_001);

    eprintln!(
        "ordering: junior deposited 400000 at t0 and claimed {early_claim}; \
         440000 without the final-second depositor ({} less)",
        440_000 - early_claim
    );
}

// --- settlement timing ------------------------------------------------------

#[test]
fn settling_later_moves_the_junior_and_leaves_the_senior_where_it_was() {
    // Settled at the first eligible ledger.
    let on_time = world();
    on_time.open(VAULT_RATE);
    on_time.deposit_junior(&on_time.junior, 400_000);
    on_time.deposit_senior(&on_time.senior_a, 200_000);
    on_time.advance(TERM);
    on_time.mgr().settle();
    let on_time = on_time.mgr().current_epoch().unwrap();

    // Settled thirty days later, by anyone who cares to wait.
    let late = world();
    late.open(VAULT_RATE);
    late.deposit_junior(&late.junior, 400_000);
    late.deposit_senior(&late.senior_a, 200_000);
    late.advance(TERM + THIRTY_DAYS);
    late.mgr().settle();
    let late = late.mgr().current_epoch().unwrap();

    // 800 000 at 10%/yr: 80 000 over the year, 4 931 more over the extra
    // thirty days (truncated once, because the vault accrues lazily).
    assert_eq!(on_time.value_redeemed, 660_000);
    assert_eq!(late.value_redeemed, 664_931);

    // The senior is capped at its target, so the extra yield is the junior's.
    assert_eq!(on_time.senior_payout, 220_000);
    assert_eq!(late.senior_payout, 220_000);
    assert_eq!(on_time.junior_payout, 440_000);
    assert_eq!(late.junior_payout, 444_931);

    eprintln!(
        "ordering: settling {}s late moved V from {} to {}, and the junior from {} to {}; \
         the senior was unchanged at {}",
        THIRTY_DAYS,
        on_time.value_redeemed,
        late.value_redeemed,
        on_time.junior_payout,
        late.junior_payout,
        late.senior_payout
    );
}

#[test]
fn a_loss_accruing_after_maturity_is_borne_by_the_junior_until_the_cushion() {
    let on_time = world();
    on_time.open(-VAULT_RATE);
    on_time.deposit_junior(&on_time.junior, 400_000);
    on_time.deposit_senior(&on_time.senior_a, 200_000);
    on_time.advance(TERM);
    on_time.mgr().settle();
    let on_time = on_time.mgr().current_epoch().unwrap();

    let late = world();
    late.open(-VAULT_RATE);
    late.deposit_junior(&late.junior, 400_000);
    late.deposit_senior(&late.senior_a, 200_000);
    late.advance(TERM + THIRTY_DAYS);
    late.mgr().settle();
    let late = late.mgr().current_epoch().unwrap();

    assert_eq!(on_time.value_redeemed, 540_000);
    assert_eq!(late.value_redeemed, 535_069);
    assert_eq!(on_time.senior_payout, 220_000);
    assert_eq!(late.senior_payout, 220_000);
    assert_eq!(on_time.junior_payout, 320_000);
    assert_eq!(late.junior_payout, 315_069);

    eprintln!(
        "ordering: settling {}s late during a loss moved the junior from {} to {}; \
         the senior was unchanged at {}",
        THIRTY_DAYS, on_time.junior_payout, late.junior_payout, late.senior_payout
    );
}

// --- the mock vault's rate knob ---------------------------------------------

#[test]
fn a_rate_change_in_the_final_ledger_moves_the_vault_by_one_stroop() {
    let w = world();
    w.open(VAULT_RATE);
    w.deposit_junior(&w.junior, 400_000);
    w.deposit_senior(&w.senior_a, 400_000);

    w.advance(TERM - 1);
    // The admin's testnet-only power: a -1000%/yr rate, one second before
    // maturity. `set_yield_rate` accrues at the old rate first, so only the
    // remaining second can be re-priced.
    w.vault_client().set_yield_rate(&-100_000);
    w.advance(1);
    w.mgr().settle();

    let e = w.mgr().current_epoch().unwrap();
    // 800 000 at 10% for TERM-1 is 79 999. One second at -1000%/yr is 0.0003
    // stroops, which floors to zero: the last-second change is worth nothing.
    // Without the change the vault would be worth 880 000.
    assert_eq!(e.value_redeemed, 879_999);
    assert_eq!(880_000 - e.value_redeemed, 1);

    let expected = settle(e.senior_total, e.junior_total, RATE, TERM, e.value_redeemed).unwrap();
    assert_eq!(e.senior_payout, expected.senior_payout);
    assert_eq!(e.junior_payout, expected.junior_payout);

    eprintln!(
        "ordering: a -100000bps rate change in the final ledger left V at {} \
         (880000 without it); one stroop, from truncation, not from the change",
        e.value_redeemed
    );
}

// --- claim ordering ---------------------------------------------------------

#[test]
fn claim_order_only_decides_who_takes_the_rounding_remainder() {
    // Senior payout 105 010 over 33 333 and 66 667. The pro-rata shares are
    // 35 002.98 and 70 007.01, so the last claimer absorbs one stroop.
    let w = world();
    w.mgr().create_epoch(&TERM, &501, &RATIO);
    w.vault_client().set_yield_rate(&VAULT_RATE);
    w.deposit_junior(&w.junior, 100_000);
    w.deposit_senior(&w.senior_a, 33_333);
    w.deposit_senior(&w.senior_b, 66_667);
    w.advance(TERM);
    w.mgr().settle();

    let e = w.mgr().current_epoch().unwrap();
    assert_eq!(e.senior_payout, 105_010);

    // Across tranches the order is irrelevant.
    let junior = w.mgr().claim(&w.junior, &Tranche::Junior);
    let first = w.mgr().claim(&w.senior_a, &Tranche::Senior);
    let last = w.mgr().claim(&w.senior_b, &Tranche::Senior);

    assert_eq!(junior, 114_990);
    assert_eq!(first, 35_002);
    assert_eq!(last, 70_008);
    assert_eq!(first + last, e.senior_payout);
    // The last claimer's excess over its own floored pro-rata share is one
    // stroop, and that is the only thing order decides.
    assert_eq!(last - 70_007, 1);

    // Reversed: the extra stroop follows whoever claims last.
    let other = world();
    other.mgr().create_epoch(&TERM, &501, &RATIO);
    other.vault_client().set_yield_rate(&VAULT_RATE);
    other.deposit_junior(&other.junior, 100_000);
    other.deposit_senior(&other.senior_a, 33_333);
    other.deposit_senior(&other.senior_b, 66_667);
    other.advance(TERM);
    other.mgr().settle();

    let b_first = other.mgr().claim(&other.senior_b, &Tranche::Senior);
    let a_last = other.mgr().claim(&other.senior_a, &Tranche::Senior);
    assert_eq!((b_first, a_last), (70_007, 35_003));
    assert_eq!(a_last - 35_002, 1);

    eprintln!(
        "ordering: senior payout {} over 33333/66667 pays 35002/70008 (or 70007/35003); \
         the last claimer takes one stroop",
        e.senior_payout
    );
}

// --- the caller's incentive -------------------------------------------------

#[test]
fn settling_pays_the_caller_nothing() {
    let w = world();
    w.open(VAULT_RATE);
    w.deposit_junior(&w.junior, 400_000);
    w.deposit_senior(&w.senior_a, 400_000);
    w.advance(TERM);

    let before = w.balance(&w.bystander);
    // No authorisation is required, and no identity is passed: `settle` has no
    // caller to pay.
    w.mgr().settle();
    assert_eq!(w.balance(&w.bystander), before);
    assert_eq!(w.mgr().current_epoch().unwrap().status, Status::Settled);
}
