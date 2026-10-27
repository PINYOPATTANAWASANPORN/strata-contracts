//! End-to-end epochs through the real epoch manager and the Blend adapter.
//!
//! The adapter's own tests prove each function against the pool double. These
//! tests prove the thing that actually matters to a depositor: an epoch manager
//! whose vault is the adapter, taking senior and junior deposits, supplying
//! them to a pool, and settling through the waterfall. There is no mock vault
//! anywhere in this fixture.
//!
//! Every expected payout is recomputed in this file from
//! [`docs/waterfall-spec.md`](../../docs/waterfall-spec.md), including the
//! senior interest expression and its truncation. The contract's fields are
//! compared to those numbers; they are never used as their own expected value.
//! The pool's position is recomputed independently as well, from the bToken
//! formula in the adapter's design note rather than from a contract read.
//!
//! Four scenarios:
//!
//! | Test | Pool outcome | What it shows |
//! | --- | --- | --- |
//! | good | +10% | senior target, junior gets the rest |
//! | flat | 0% | the senior target is still paid, from the junior |
//! | loss | -5% | the junior absorbs the shortfall first |
//! | liquidity-limited | cannot pay at maturity | settlement fails loudly and can be retried |
//!
//! The pool is the test double and its rate is set by the test. It proves the
//! plumbing and the settlement arithmetic at the boundary; it proves nothing
//! about Blend's real liquidity, and the adapter has still never run against a
//! real pool. See `docs/risks.md` B1–B7.

use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};
use strata_blend_adapter::{BlendAdapter, BlendAdapterClient};
use strata_blend_pool_mock::{MockPool, MockPoolClient};
use strata_epoch_manager::{EpochManager, EpochManagerClient, Status, Tranche};

/// The waterfall spec's `SECONDS_PER_YEAR`, restated here so the expected
/// values depend on the document, not on an import from the implementation.
const SECONDS_PER_YEAR: u64 = 31_536_000;
/// The waterfall spec's basis-point denominator.
const BPS_DENOMINATOR: i128 = 10_000;
/// The senior target used by every epoch in this file: 5% per year.
const RATE_BPS: u32 = 500;
/// The senior cap used by every epoch: 1.0x, so senior never exceeds junior.
const RATIO_BPS: u32 = 10_000;
/// Blend v2's starting bToken exchange rate: 1 bToken = 1 asset.
const ONE_TO_ONE: i128 = 1_000_000_000_000;
const STARTING_BALANCE: i128 = 1_000_000_000;

struct World {
    env: Env,
    manager: Address,
    adapter: Address,
    pool: Address,
    asset: Address,
    senior: Address,
    junior: Address,
}

fn world() -> World {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    // The adapter's immutable `manager` is the epoch manager, and the epoch
    // manager's immutable `vault` is the adapter. That is a cycle at
    // construction time, so both contract IDs are chosen up front and each
    // contract is registered at its chosen address. This is a test-only
    // convenience; on a network the manager is deployed after the adapter.
    let manager_id = Address::generate(&env);
    let adapter_id = Address::generate(&env);

    let pool = env.register(MockPool, (&asset, &ONE_TO_ONE));
    let adapter = env.register_at(&adapter_id, BlendAdapter, (&pool, &asset, &manager_id));
    let manager = env.register_at(&manager_id, EpochManager, (&admin, &adapter));

    let senior = Address::generate(&env);
    let junior = Address::generate(&env);
    for who in [&senior, &junior] {
        StellarAssetClient::new(&env, &asset).mint(who, &STARTING_BALANCE);
    }

    World {
        env,
        manager,
        adapter,
        pool,
        asset,
        senior,
        junior,
    }
}

impl World {
    fn mgr(&self) -> EpochManagerClient<'_> {
        EpochManagerClient::new(&self.env, &self.manager)
    }

    fn adapter(&self) -> BlendAdapterClient<'_> {
        BlendAdapterClient::new(&self.env, &self.adapter)
    }

    fn pool(&self) -> MockPoolClient<'_> {
        MockPoolClient::new(&self.env, &self.pool)
    }

    fn token(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.asset)
    }

    fn balance(&self, who: &Address) -> i128 {
        self.token().balance(who)
    }

    fn open_epoch(&self) {
        self.mgr()
            .create_epoch(&SECONDS_PER_YEAR, &RATE_BPS, &RATIO_BPS);
    }

    fn deposit(&self, who: &Address, tranche: Tranche, amount: i128) {
        self.mgr().deposit(who, &tranche, &amount);
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp() + seconds;
        self.env.ledger().set_timestamp(now);
    }

    fn set_rate(&self, rate: i128) {
        self.pool().set_b_rate(&rate);
    }

    /// Adds the tokens that stand in for borrower interest. Without this the
    /// pool would report a higher position than it could pay.
    fn fund_pool(&self, amount: i128) {
        StellarAssetClient::new(&self.env, &self.asset).mint(&self.pool, &amount);
    }
}

/// The value of a position whose deposits all arrived at the pool's starting
/// 1:1 rate, at `final_rate`. This is Blend's own conversion rounded down:
/// `floor(b_tokens * b_rate / 10^12)`.
fn position_value(deposited: i128, final_rate: i128) -> i128 {
    deposited * final_rate / ONE_TO_ONE
}

/// The waterfall, recomputed from `docs/waterfall-spec.md` §4 and §5.
///
/// Deliberately not `strata_waterfall::settle`: importing the implementation
/// would let a wrong implementation agree with itself.
struct ExpectedSplit {
    senior_due: i128,
    senior_payout: i128,
    junior_payout: i128,
}

fn spec_split(senior: i128, value: i128) -> ExpectedSplit {
    let interest = senior * (RATE_BPS as i128) * (SECONDS_PER_YEAR as i128)
        / (BPS_DENOMINATOR * (SECONDS_PER_YEAR as i128));
    let senior_due = senior + interest;
    let senior_payout = if value >= senior_due {
        senior_due
    } else {
        value
    };
    ExpectedSplit {
        senior_due,
        senior_payout,
        junior_payout: value - senior_payout,
    }
}

// --- a good epoch -----------------------------------------------------------

#[test]
fn a_good_epoch_through_the_adapter_pays_the_waterfall_from_the_real_redemption() {
    let w = world();
    w.open_epoch();

    let senior_principal = 300_000;
    let junior_principal = 500_000;
    w.deposit(&w.junior, Tranche::Junior, junior_principal);
    w.deposit(&w.senior, Tranche::Senior, senior_principal);

    // The pool earns 10% over the term: 800_000 becomes 880_000.
    let final_rate = ONE_TO_ONE + ONE_TO_ONE * 1_000 / 10_000;
    w.set_rate(final_rate);
    let expected_value = position_value(senior_principal + junior_principal, final_rate);
    w.fund_pool(expected_value - senior_principal - junior_principal);

    w.advance(SECONDS_PER_YEAR);
    w.mgr().settle();

    let epoch = w.mgr().current_epoch().unwrap();
    let expected = spec_split(senior_principal, expected_value);

    assert_eq!(epoch.status, Status::Settled);
    assert_eq!(
        epoch.value_redeemed, expected_value,
        "V must be what the pool actually paid, recomputed here from its rate"
    );
    assert_eq!(epoch.senior_payout, expected.senior_payout);
    assert_eq!(epoch.junior_payout, expected.junior_payout);
    assert_eq!(
        epoch.senior_payout + epoch.junior_payout,
        epoch.value_redeemed,
        "conservation: the two payouts account for every base unit"
    );

    // The senior target for 300_000 at 5%/yr for one year is 15_000.
    assert_eq!(expected.senior_due, 315_000);
    assert_eq!(epoch.senior_payout, 315_000);
    // Everything else, including the yield above the senior target, is junior.
    assert_eq!(epoch.junior_payout, 565_000);

    let senior_claim = w.mgr().claim(&w.senior, &Tranche::Senior);
    let junior_claim = w.mgr().claim(&w.junior, &Tranche::Junior);
    assert_eq!(senior_claim, expected.senior_payout);
    assert_eq!(junior_claim, expected.junior_payout);
    assert_eq!(
        w.balance(&w.senior),
        STARTING_BALANCE - senior_principal + senior_claim
    );
    assert_eq!(
        w.balance(&w.junior),
        STARTING_BALANCE - junior_principal + junior_claim
    );
    assert_eq!(w.balance(&w.manager), 0, "every claim was paid");
}

// --- a flat epoch -----------------------------------------------------------

#[test]
fn a_flat_epoch_pays_the_senior_target_out_of_the_junior_buffer() {
    let w = world();
    w.open_epoch();

    let senior_principal = 400_000;
    let junior_principal = 400_000;
    w.deposit(&w.junior, Tranche::Junior, junior_principal);
    w.deposit(&w.senior, Tranche::Senior, senior_principal);

    // No rate move and no funding: the pool earns exactly nothing.
    let expected_value = senior_principal + junior_principal;
    w.advance(SECONDS_PER_YEAR);
    w.mgr().settle();

    let epoch = w.mgr().current_epoch().unwrap();
    let expected = spec_split(senior_principal, expected_value);

    assert_eq!(epoch.value_redeemed, expected_value);
    assert_eq!(epoch.senior_payout, expected.senior_payout);
    assert_eq!(epoch.junior_payout, expected.junior_payout);

    // The flat case is not break-even for the junior: the senior still earns
    // its 5%, so the junior pays that 20_000 out of its own principal.
    assert_eq!(epoch.senior_payout, 420_000);
    assert_eq!(epoch.junior_payout, 380_000);

    assert_eq!(w.mgr().claim(&w.senior, &Tranche::Senior), 420_000);
    assert_eq!(w.mgr().claim(&w.junior, &Tranche::Junior), 380_000);
}

// --- a loss epoch -----------------------------------------------------------

#[test]
fn a_loss_epoch_with_a_falling_rate_is_absorbed_by_the_junior() {
    let w = world();
    w.open_epoch();

    let senior_principal = 400_000;
    let junior_principal = 400_000;
    w.deposit(&w.junior, Tranche::Junior, junior_principal);
    w.deposit(&w.senior, Tranche::Senior, senior_principal);

    // A 5% fall in b_rate is what a bad-debt socialisation looks like to a
    // supplier: the position is written down in place, with no transfer.
    let final_rate = ONE_TO_ONE * 9_500 / 10_000;
    w.set_rate(final_rate);
    let expected_value = position_value(senior_principal + junior_principal, final_rate);

    w.advance(SECONDS_PER_YEAR);
    w.mgr().settle();

    let epoch = w.mgr().current_epoch().unwrap();
    let expected = spec_split(senior_principal, expected_value);

    assert_eq!(expected_value, 760_000);
    assert_eq!(epoch.value_redeemed, expected_value);
    assert_eq!(epoch.senior_payout, 420_000, "senior is made whole");
    assert_eq!(
        epoch.junior_payout, 340_000,
        "junior absorbs the whole loss"
    );
    assert_eq!(epoch.senior_payout, expected.senior_payout);
    assert_eq!(epoch.junior_payout, expected.junior_payout);

    assert_eq!(w.mgr().claim(&w.senior, &Tranche::Senior), 420_000);
    assert_eq!(w.mgr().claim(&w.junior, &Tranche::Junior), 340_000);
}

// --- liquidity --------------------------------------------------------------

#[test]
fn a_liquidity_limited_redeem_blocks_settlement_and_can_be_retried() {
    let w = world();
    w.open_epoch();

    let senior_principal = 400_000;
    let junior_principal = 400_000;
    w.deposit(&w.junior, Tranche::Junior, junior_principal);
    w.deposit(&w.senior, Tranche::Senior, senior_principal);

    // The position is worth 840_000, but borrowers have left the pool able to
    // pay only 800_000. This is the real failure B2 describes: settlement
    // asks for everything at once and the pool cannot produce it.
    let final_rate = ONE_TO_ONE * 10_500 / 10_000;
    w.set_rate(final_rate);
    let expected_value = position_value(senior_principal + junior_principal, final_rate);
    assert_eq!(expected_value, 840_000);
    w.fund_pool(expected_value - senior_principal - junior_principal);
    w.pool().set_available_liquidity(&800_000);

    w.advance(SECONDS_PER_YEAR);
    assert!(
        w.mgr().try_settle().is_err(),
        "the pool cannot pay 840_000, so settlement must fail rather than pay short"
    );

    // The failed invocation rolled back: the epoch is still open, the adapter
    // still holds the shares, and no payout was recorded or paid.
    let epoch = w.mgr().current_epoch().unwrap();
    assert_eq!(epoch.status, Status::Open);
    assert_eq!(epoch.vault_shares, senior_principal + junior_principal);
    assert_eq!(epoch.value_redeemed, 0);
    assert_eq!(w.balance(&w.manager), 0);
    assert_eq!(
        w.adapter().total_shares(),
        senior_principal + junior_principal
    );
    assert_eq!(
        w.adapter().balance_of(&w.manager),
        senior_principal + junior_principal
    );

    // Liquidity returning is the only thing that changes. Settlement is
    // permissionless, so anyone can retry it.
    w.pool().set_available_liquidity(&i128::MAX);
    w.mgr().settle();

    let epoch = w.mgr().current_epoch().unwrap();
    let expected = spec_split(senior_principal, expected_value);
    assert_eq!(epoch.status, Status::Settled);
    assert_eq!(epoch.value_redeemed, expected_value);
    assert_eq!(epoch.senior_payout, expected.senior_payout);
    assert_eq!(epoch.junior_payout, expected.junior_payout);
    assert_eq!(epoch.senior_payout, 420_000);
    assert_eq!(epoch.junior_payout, 420_000);

    assert_eq!(w.mgr().claim(&w.senior, &Tranche::Senior), 420_000);
    assert_eq!(w.mgr().claim(&w.junior, &Tranche::Junior), 420_000);
}
