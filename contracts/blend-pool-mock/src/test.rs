//! Tests for the pool test double itself.
//!
//! A double that is wrong in a way the adapter depends on makes the adapter's
//! tests worthless, so the surprising parts of the real pool — rounding
//! directions, the silent clamp, the utilisation gate, the empty read of an
//! expired position — are pinned here rather than assumed.

extern crate std;

use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{token, vec, Address, Env, Error as SdkError, InvokeError};

const ONE_TO_ONE: i128 = 1_000_000_000_000;

/// The value `try_*` reports for a trap with `code`. `submit` returns a value
/// rather than a `Result`, so the client has no typed error for it.
fn trap(code: Error) -> Result<SdkError, InvokeError> {
    Ok(SdkError::from_contract_error(code as u32))
}

struct World {
    env: Env,
    pool: Address,
    asset: Address,
    user: Address,
}

fn world_with_rate(rate: i128) -> World {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool = env.register(MockPool, (&asset, &rate));
    let user = Address::generate(&env);
    World {
        env,
        pool,
        asset,
        user,
    }
}

fn world() -> World {
    world_with_rate(ONE_TO_ONE)
}

impl World {
    fn pool(&self) -> MockPoolClient<'_> {
        MockPoolClient::new(&self.env, &self.pool)
    }

    fn token(&self) -> token::TokenClient<'_> {
        token::TokenClient::new(&self.env, &self.asset)
    }

    fn give(&self, who: &Address, amount: i128) {
        token::StellarAssetClient::new(&self.env, &self.asset).mint(who, &amount);
    }

    fn supply(&self, amount: i128) -> Positions {
        self.pool().submit(
            &self.user,
            &self.user,
            &self.user,
            &vec![
                &self.env,
                Request {
                    request_type: REQUEST_SUPPLY,
                    address: self.asset.clone(),
                    amount,
                },
            ],
        )
    }

    fn withdraw(&self, amount: i128) -> Positions {
        self.pool().submit(
            &self.user,
            &self.user,
            &self.user,
            &vec![
                &self.env,
                Request {
                    request_type: REQUEST_WITHDRAW,
                    address: self.asset.clone(),
                    amount,
                },
            ],
        )
    }

    /// `try_submit` can also fail converting its own arguments, so its success
    /// case is itself a `Result`. A valid request vec cannot fail to convert, so
    /// the two are flattened here and the tests assert on the contract error.
    fn try_withdraw(&self, amount: i128) -> Result<Positions, Result<SdkError, InvokeError>> {
        self.pool()
            .try_submit(
                &self.user,
                &self.user,
                &self.user,
                &vec![
                    &self.env,
                    Request {
                        request_type: REQUEST_WITHDRAW,
                        address: self.asset.clone(),
                        amount,
                    },
                ],
            )
            .map(|inner| inner.expect("a valid request vec converts"))
    }

    fn try_supply(&self, amount: i128) -> Result<Positions, Result<SdkError, InvokeError>> {
        self.pool()
            .try_submit(
                &self.user,
                &self.user,
                &self.user,
                &vec![
                    &self.env,
                    Request {
                        request_type: REQUEST_SUPPLY,
                        address: self.asset.clone(),
                        amount,
                    },
                ],
            )
            .map(|inner| inner.expect("a valid request vec converts"))
    }
}

#[test]
fn a_supply_mints_b_tokens_rounded_down() {
    // 1 bToken worth 2 assets: 100 assets in, 50 bTokens out.
    let w = world_with_rate(2_000_000_000_000);
    w.give(&w.user, 100_000_000);
    let positions = w.supply(100_000_000);
    assert_eq!(positions.supply.get(0).unwrap(), 50_000_000);
    assert_eq!(w.pool().token_balance(), 100_000_000);

    // And the rounding is against the depositor: a third of a bToken is
    // discarded, not rounded up.
    let w2 = world_with_rate(3_000_000_000_000);
    w2.give(&w2.user, 100_000_000);
    let positions = w2.supply(100_000_000);
    assert_eq!(positions.supply.get(0).unwrap(), 33_333_333);
}

#[test]
fn a_withdrawal_burns_b_tokens_rounded_up_and_leaves_dust_behind() {
    let w = world_with_rate(2_000_000_000_000);
    w.give(&w.user, 100_000_000);
    w.supply(100_000_000);
    assert_eq!(w.pool().stored_b_tokens(&w.user), 50_000_000);

    // The rate rises, as it does when borrowers pay interest.
    w.pool().set_b_rate(&2_500_000_000_000);

    // Withdrawing the same token amount now burns fewer bTokens than were
    // minted, which is the dust the design note measured on testnet.
    w.withdraw(100_000_000);

    assert_eq!(w.pool().stored_b_tokens(&w.user), 10_000_000);
    assert_eq!(w.token().balance(&w.user), 100_000_000);
    assert_eq!(
        w.pool().token_balance(),
        0,
        "the pool paid out everything it held"
    );
}

#[test]
fn an_over_sized_withdrawal_is_clamped_and_reported_as_success() {
    let w = world();
    w.give(&w.user, 100_000_000);
    w.supply(100_000_000);

    // 1000x the position. The real pool clamps this rather than refusing it,
    // and that is the behaviour the adapter's balance check exists for.
    let positions = w.withdraw(100_000_000_000);

    assert_eq!(w.token().balance(&w.user), 100_000_000);
    assert_eq!(w.pool().stored_b_tokens(&w.user), 0);
    assert_eq!(
        positions.supply.get(0),
        None,
        "the position is emptied and the request is not mentioned in the answer"
    );
}

#[test]
fn a_withdrawal_beyond_the_liquidity_limit_reverts() {
    let w = world();
    w.give(&w.user, 100_000_000);
    w.supply(100_000_000);

    // Only 60 of the 100 is unborrowed.
    w.pool().set_available_liquidity(&60_000_000);

    assert_eq!(
        w.try_withdraw(100_000_000),
        Err(trap(Error::InvalidUtilRate)),
        "the pool refuses rather than paying short"
    );
    // Nothing moved.
    assert_eq!(w.pool().stored_b_tokens(&w.user), 100_000_000);
    assert_eq!(w.token().balance(&w.user), 0);
}

#[test]
fn an_expired_position_reads_as_empty_and_the_reserve_still_counts_it() {
    let w = world();
    w.give(&w.user, 100_000_000);
    w.supply(100_000_000);

    w.pool().expire_position(&w.user);

    let positions = w.pool().get_positions(&w.user);
    assert_eq!(
        positions.supply.get(0),
        None,
        "an expired entry reads as empty, not as an error"
    );
    assert_eq!(
        w.pool().b_supply(),
        100_000_000,
        "the reserve still counts the bTokens; they are not lost, they are unreachable"
    );
    assert_eq!(w.pool().stored_b_tokens(&w.user), 0);
}

#[test]
fn a_disabled_reserve_blocks_supply_and_still_allows_withdrawal() {
    let w = world();
    w.give(&w.user, 100_000_000);
    w.supply(100_000_000);

    w.pool().set_reserve_enabled(&false);

    assert_eq!(
        w.try_supply(50_000_000),
        Err(trap(Error::ReserveDisabled)),
        "supply is blocked"
    );
    // Withdrawal is not.
    w.withdraw(100_000_000);
    assert_eq!(w.token().balance(&w.user), 100_000_000);
}

#[test]
fn a_frozen_pool_blocks_supply() {
    let w = world();
    w.give(&w.user, 100_000_000);
    w.pool().set_status(&4);
    assert_eq!(
        w.try_supply(100_000_000),
        Err(trap(Error::InvalidPoolStatus))
    );
    assert_eq!(w.token().balance(&w.user), 100_000_000);
}

#[test]
fn withdrawing_with_no_position_reverts_rather_than_doing_nothing() {
    let w = world();
    assert_eq!(
        w.try_withdraw(1_000_000),
        Err(trap(Error::InvalidBTokenBurnAmount))
    );
}

#[test]
fn the_scale_is_read_from_the_reserve_so_v1s_nine_decimals_work() {
    let w = world();
    // v1 scaled rates by 9, not 12.
    w.pool().set_scalar(&9);
    w.pool().set_b_rate(&1_000_000_000);
    w.give(&w.user, 100_000_000);

    let positions = w.supply(100_000_000);
    assert_eq!(positions.supply.get(0).unwrap(), 100_000_000);
    assert_eq!(w.pool().get_reserve(&w.asset).scalar, 9);
}

#[test]
fn a_supply_beyond_the_cap_reverts() {
    let w = world();
    w.give(&w.user, 100_000_000);
    w.pool().set_supply_cap(&50_000_000);
    assert_eq!(
        w.try_supply(100_000_000),
        Err(trap(Error::ExceededSupplyCap))
    );
}

#[test]
fn submit_needs_the_spenders_authorisation() {
    let w = world();
    w.give(&w.user, 100_000_000);
    // Replaces the blanket mock with an empty list, so `require_auth` is
    // actually enforced from here on.
    w.env.mock_auths(&[]);

    assert!(
        w.try_supply(100_000_000).is_err(),
        "an unauthorised supply must be refused"
    );
}

#[test]
fn a_reserve_the_pool_does_not_have_is_an_error() {
    let w = world();
    let other = Address::generate(&w.env);
    assert_eq!(
        w.pool()
            .try_get_reserve(&other)
            .map(|inner| inner.expect("the reserve converts")),
        Err(trap(Error::InternalReserveNotFound))
    );
}
