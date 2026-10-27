//! Property tests for the Blend adapter's accounting.
//!
//! The adapter sits between the epoch manager and a lending pool, and every
//! number it handles passes through two rounds of floor division: the pool
//! floors the bTokens it mints, and the adapter floors the shares it mints.
//! The properties here are the ones that make those roundings safe:
//!
//! 1. a deposit cannot create value — the position is worth no more after the
//!    deposit than before it, plus the assets paid in;
//! 2. a redemption cannot create value — what is paid plus what remains is no
//!    more than the position was worth before the redemption;
//! 3. redeeming a deposit returns at most the principal plus the yield the
//!    rate actually accrued;
//! 4. the share ledger moves only by the shares a call reports, and in the
//!    direction that call implies.
//!
//! These use the pool double, not a network. The double reproduces Blend's
//! rounding (floor on supply, ceil on withdrawal) from the design note; it does
//! not accrue interest, so a test moves the rate itself. What is proved here is
//! the adapter's arithmetic around the pool, not anything about the real pool.
//!
//! Run with more cases (up to the documented cap):
//!
//! ```text
//! PROPTEST_CASES=512 cargo test -p strata-tests blend_adapter_props
//! ```

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestCaseError, TestRunner};
use soroban_sdk::{testutils::Address as _, token::StellarAssetClient, Address, Env};
use strata_blend_adapter::{BlendAdapter, BlendAdapterClient};
use strata_blend_pool_mock::{MockPool, MockPoolClient};

/// Blend v2's `b_rate` scale. The adapter reads it from the reserve, so this is
/// only the value the fixture gives the pool.
const SCALAR: i128 = 1_000_000_000_000;

/// Cases run by default. Every case deploys a token, a pool and an adapter and
/// drives at least one cross-contract call, so this is deliberately smaller
/// than the pure-arithmetic waterfalls suite. The generators include the
/// rounding edges, and proptest shrinks any failure to a minimal case.
const CASES: u32 = 128;
/// Upper bound even when `PROPTEST_CASES` is large. CI runs the whole suite
/// with a 20_000-case budget for the waterfall properties; applying that here
/// would spend tens of minutes deploying test contracts for no extra coverage.
const MAX_CASES: u32 = 512;

struct World {
    env: Env,
    adapter: Address,
    pool: Address,
    asset: Address,
    manager: Address,
}

fn world(initial_rate: i128) -> World {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool = env.register(MockPool, (&asset, &initial_rate));
    let manager = Address::generate(&env);
    let adapter = env.register(BlendAdapter, (&pool, &asset, &manager));
    World {
        env,
        adapter,
        pool,
        asset,
        manager,
    }
}

impl World {
    fn adapter(&self) -> BlendAdapterClient<'_> {
        BlendAdapterClient::new(&self.env, &self.adapter)
    }

    fn pool(&self) -> MockPoolClient<'_> {
        MockPoolClient::new(&self.env, &self.pool)
    }

    fn mint(&self, who: &Address, amount: i128) {
        StellarAssetClient::new(&self.env, &self.asset).mint(who, &amount);
    }

    /// Deposits as the manager, funding the manager first. The adapter's only
    /// permitted depositor is an address in these tests, not a real manager
    /// contract; the manager integration lives in `blend_adapter_integration`.
    fn deposit(&self, assets: i128) -> i128 {
        self.mint(&self.manager, assets);
        self.adapter().deposit(&assets, &self.manager)
    }

    /// Puts spare tokens in the pool, standing in for assets borrowers repaid.
    /// Raising `b_rate` is accounting only: without this the pool would owe
    /// more than it holds and could not pay a redemption.
    fn fund_pool(&self, amount: i128) {
        self.mint(&self.pool, amount);
    }

    fn total_assets(&self) -> i128 {
        self.adapter().total_assets()
    }

    fn total_shares(&self) -> i128 {
        self.adapter().total_shares()
    }
}

/// A deposit amount with realistic magnitudes and the small edges where floor
/// division matters. The upper bound keeps `amount * 10^12` far inside `i128`.
fn amount() -> impl Strategy<Value = i128> {
    prop_oneof![
        10i128..=1_000i128,
        10i128..=1_000_000i128,
        10i128..=1_000_000_000i128,
        Just(10),
        Just(1_000_000_000),
    ]
}

/// A bToken exchange rate around 1:1, with the extremes a test pool can
/// reasonably have. Every value is positive and far below `i128::MAX`.
fn rate() -> impl Strategy<Value = i128> {
    prop_oneof![
        (SCALAR / 2)..=(SCALAR * 2),
        Just(SCALAR),
        Just(SCALAR / 2),
        Just(SCALAR * 2),
    ]
}

fn growth_rate() -> impl Strategy<Value = i128> {
    SCALAR..=(SCALAR * 2)
}

/// Runs one property with its own runner.
///
/// Deliberately not the `proptest!` macro: the macro calls
/// `contextualize_config` after the configuration is supplied, so CI's global
/// `PROPTEST_CASES=20000` would override the cap below and turn four
/// cross-contract properties into tens of minutes of contract deployment. A
/// direct `TestRunner` keeps the documented case count, while still shrinking
/// and persisting counterexamples.
fn check<S>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>)
where
    S: Strategy,
    S::Value: core::fmt::Debug,
{
    let requested = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CASES);
    let config = ProptestConfig {
        cases: requested.min(MAX_CASES),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("proptest-regressions"),
        )),
        source_file: Some(file!()),
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    runner
        .run(&strategy, test)
        .unwrap_or_else(|error| panic!("{error}"));
}

/// A deposit adds the assets paid in and no more. The pool floors the bTokens
/// it mints, so the position can gain less than the deposit; it must never
/// gain more.
#[test]
fn deposit_rounding_never_creates_value() {
    check(
        (amount(), amount(), growth_rate()),
        |(first, second, new_rate)| {
            let w = world(SCALAR);
            let shares_before = w.deposit(first);
            prop_assert_eq!(shares_before, first, "the first deposit is 1:1");

            // Re-price the existing position before the second deposit, so the
            // share price is not 1:1 and the share calculation actually
            // divides. The value must be read *after* the rate move: the move
            // itself is yield, and this property is about what the deposit
            // adds.
            w.pool().set_b_rate(&new_rate);
            let value_before = w.total_assets();
            prop_assert!(value_before > 0);

            let shares = w.deposit(second);
            let value_after = w.total_assets();

            prop_assert_eq!(
                w.total_shares(),
                shares_before + shares,
                "total shares must equal the sum of every deposit's shares"
            );
            prop_assert!(
                value_after <= value_before + second,
                "deposit of {} created value: {} -> {} (deposit before rate move {})",
                second,
                value_before,
                value_after,
                first
            );

            let new_shares_value = w.adapter().convert_to_assets(&shares);
            prop_assert!(
                new_shares_value <= second,
                "a deposit of {} bought shares worth {}",
                second,
                new_shares_value
            );
            Ok(())
        },
    );
}

/// A redemption pays out of the position and cannot leave more value behind
/// than it started with. The pool rounds the burned bTokens up, so the
/// remaining position can only shrink by at least what was paid.
#[test]
fn redeem_rounding_never_creates_value() {
    check(
        (amount(), amount(), rate(), 1u32..=10_000u32),
        |(first, second, new_rate, share_bps)| {
            let w = world(SCALAR);
            w.deposit(first);
            w.deposit(second);

            w.pool().set_b_rate(&new_rate);
            // Back the repricing with tokens, otherwise the pool cannot pay
            // even though its accounting says the position is worth more.
            w.fund_pool(w.total_assets());

            let value_before = w.total_assets();
            let shares_before = w.total_shares();
            prop_assert!(value_before > 0);
            prop_assert!(shares_before > 0);

            // A non-zero fraction of the position. `max(1)` keeps the share
            // count non-zero for tiny generated totals; `min` keeps it within
            // the ledger.
            let shares = ((shares_before * share_bps as i128) / 10_000)
                .max(1)
                .min(shares_before);
            let paid = w.adapter().redeem(&shares, &w.manager, &w.manager);
            let value_after = w.total_assets();

            prop_assert!(paid >= 0);
            prop_assert!(
                paid + value_after <= value_before,
                "redeem created value: before {}, paid {}, after {}",
                value_before,
                paid,
                value_after
            );
            prop_assert_eq!(
                w.total_shares(),
                shares_before - shares,
                "burning {} shares must reduce the ledger by exactly that",
                shares
            );
            Ok(())
        },
    );
}

/// The headline round-trip bound: redeeming a deposit cannot return more than
/// the principal plus the yield the rate actually accrued. The principal is
/// charged a floor on the way in, so the bound also covers the case where the
/// initial position was worth slightly less than was paid.
#[test]
fn redeem_after_deposit_is_bounded_by_principal_plus_accrued_yield() {
    check(
        (amount(), (SCALAR / 2)..=SCALAR, growth_rate()),
        |(principal, initial_rate, final_rate)| {
            let w = world(initial_rate);
            let shares = w.deposit(principal);

            // Recompute the pool's position independently, from the formulas
            // in the design note, rather than reading the value back from
            // either contract under test.
            let b_tokens = principal * SCALAR / initial_rate;
            let initial_value = b_tokens * initial_rate / SCALAR;
            let final_value = b_tokens * final_rate / SCALAR;
            let accrued_yield = final_value - initial_value;
            prop_assert!(b_tokens > 0);
            prop_assert!(final_value >= initial_value);

            w.pool().set_b_rate(&final_rate);
            w.fund_pool(final_value);

            let paid = w.adapter().redeem(&shares, &w.manager, &w.manager);

            prop_assert_eq!(
                paid,
                final_value,
                "a sole holder redeems the whole position"
            );
            prop_assert!(
                paid <= principal + accrued_yield,
                "paid {} more than principal {} plus accrued yield {}",
                paid,
                principal,
                accrued_yield
            );
            prop_assert_eq!(w.total_shares(), 0);
            Ok(())
        },
    );
}

/// Share accounting is monotonic in the only sense the ledger promises:
/// deposits increase the total and a holder's balance by exactly the shares
/// returned, redemptions decrease both by exactly the shares burned, and a
/// balance can never exceed the total.
#[test]
fn share_accounting_is_monotonic() {
    check(
        (amount(), amount(), rate(), 1u32..=10_000u32),
        |(first, second, new_rate, redeem_bps)| {
            let w = world(SCALAR);
            let first_shares = w.deposit(first);
            let after_first = w.total_shares();
            prop_assert_eq!(after_first, first_shares);

            let second_shares = w.deposit(second);
            let after_second = w.total_shares();
            prop_assert!(second_shares > 0);
            prop_assert_eq!(
                after_second,
                after_first + second_shares,
                "a deposit must increase total shares by exactly what it minted"
            );
            prop_assert_eq!(
                w.adapter().balance_of(&w.manager),
                after_second,
                "with one depositor the ledger and the balance must agree"
            );

            // A rate move between the calls proves the ledger is about shares,
            // not about a cached asset value. It happens after both deposits
            // so the test is about the ledger, not about a deposit that
            // legitimately mints zero shares at a high price.
            w.pool().set_b_rate(&new_rate);
            w.fund_pool(w.total_assets());

            let shares = ((after_second * redeem_bps as i128) / 10_000)
                .max(1)
                .min(after_second);
            let paid = w.adapter().redeem(&shares, &w.manager, &w.manager);
            let after_redeem = w.total_shares();
            prop_assert!(paid >= 0);
            prop_assert_eq!(
                after_redeem,
                after_second - shares,
                "a redemption must decrease total shares by exactly what it burned"
            );
            prop_assert_eq!(w.adapter().balance_of(&w.manager), after_redeem);
            prop_assert!(after_redeem >= 0);
            prop_assert!(after_redeem < after_second);

            // The ledger cannot be spent past zero. Asking for one more share
            // than remains is refused, and the refusal does not change the
            // ledger.
            prop_assert!(w
                .adapter()
                .try_redeem(&(after_redeem + 1), &w.manager, &w.manager)
                .is_err());
            prop_assert_eq!(w.total_shares(), after_redeem);
            Ok(())
        },
    );
}
