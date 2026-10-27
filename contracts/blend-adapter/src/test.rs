//! Tests for the Blend adapter, against the pool test double.
//!
//! Two kinds of test live here and they are deliberately separate:
//!
//! - the ordinary ones, which use `mock_all_auths` so that the arithmetic and
//!   the accounting can be driven without ceremony;
//! - `the_adapter_authorises_the_pools_token_transfer`, which does **not**, and
//!   asserts the resulting auth tree directly. `mock_all_auths` would pass
//!   whether or not the adapter's `authorize_as_current_contract` entry is
//!   right, and that entry is the single most likely place for this design to
//!   fail in practice.

extern crate std;

use super::*;
use soroban_sdk::auth::InvokerContractAuthEntry;
use soroban_sdk::testutils::{
    Address as _, AuthorizedFunction, AuthorizedInvocation, Events, MockAuth, MockAuthInvoke,
};
use soroban_sdk::xdr::ContractEventBody;
use soroban_sdk::{
    token, Address, Env, Error as SdkError, IntoVal, InvokeError, Map, Symbol, TryFromVal, Val, Vec,
};
use strata_blend_pool_mock::{MockPool, MockPoolClient};

/// The value `try_*` reports for a trap with `code`.
///
/// The adapter's methods return a value rather than a `Result` — the vault
/// interface is fixed and cannot be widened — so `panic_with_error!` is the
/// only way to fail with a specific code, and the generated client has no typed
/// error to map that code back to. The code is therefore the identity, and this
/// is what the host reports.
fn trap(code: Error) -> Result<SdkError, InvokeError> {
    Ok(SdkError::from_contract_error(code as u32))
}

/// The contract, function and arguments of an authorisation, or a panic.
///
/// A panic rather than an `Option`: an authorisation that is not a contract
/// call means this test is looking at something it did not expect, and that
/// should stop it.
fn contract_fn(invocation: &AuthorizedInvocation) -> (Address, Symbol, Vec<Val>) {
    match &invocation.function {
        AuthorizedFunction::Contract((contract, fn_name, args)) => {
            (contract.clone(), fn_name.clone(), args.clone())
        }
        other => panic!("expected an authorisation of a contract call, got {other:?}"),
    }
}

/// 1e12: one bToken per asset unit, Blend v2's starting rate.
const ONE_TO_ONE: i128 = 1_000_000_000_000;

struct World {
    env: Env,
    adapter: Address,
    pool: Address,
    asset: Address,
    manager: Address,
}

fn build(mock_auths: bool) -> World {
    let env = Env::default();
    if mock_auths {
        env.mock_all_auths();
    }
    let admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool = env.register(MockPool, (&asset, &ONE_TO_ONE));
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

fn world() -> World {
    build(true)
}

impl World {
    fn adapter(&self) -> BlendAdapterClient<'_> {
        BlendAdapterClient::new(&self.env, &self.adapter)
    }

    fn pool(&self) -> MockPoolClient<'_> {
        MockPoolClient::new(&self.env, &self.pool)
    }

    fn token(&self) -> token::TokenClient<'_> {
        token::TokenClient::new(&self.env, &self.asset)
    }

    fn mint(&self, who: &Address, amount: i128) {
        token::StellarAssetClient::new(&self.env, &self.asset).mint(who, &amount);
    }

    /// The data of the named event published by the most recent top-level
    /// invocation, if it published one.
    ///
    /// Found by name rather than by position, so a change in how many events a
    /// call emits cannot make this pass for the wrong reason.
    fn event_data(&self, name: Symbol) -> Option<Map<Symbol, i128>> {
        self.env.events().all().events().iter().find_map(|event| {
            let ContractEventBody::V0(body) = &event.body;
            let first = body.topics.first()?;
            if Symbol::try_from_val(&self.env, first).ok()? != name {
                return None;
            }
            Map::<Symbol, i128>::try_from_val(&self.env, &body.data).ok()
        })
    }

    /// The three amounts every event in this contract carries.
    fn amounts(&self, shares: i128, assets: i128, b_tokens: i128) -> Map<Symbol, i128> {
        let mut map = Map::new(&self.env);
        map.set(Symbol::new(&self.env, "shares"), shares);
        map.set(Symbol::new(&self.env, "assets"), assets);
        map.set(Symbol::new(&self.env, "b_tokens"), b_tokens);
        map
    }

    /// Puts spare tokens in the pool, standing in for the money borrowers
    /// would have paid in interest.
    ///
    /// `set_b_rate` is accounting only: raising it makes the pool *say* a
    /// position is worth more without creating any asset to pay it with. The
    /// real pool has that asset because someone borrowed and repaid. Same
    /// convention as the mock vault's strategy reserve, and the same
    /// consequence: the pool's raw token balance is not a meaningful assertion
    /// target, only its payouts are.
    fn fund_pool(&self, amount: i128) {
        self.mint(&self.pool, amount);
    }

    /// Deposits `assets` as the manager, with the manager funded first.
    fn deposit(&self, assets: i128) -> i128 {
        self.mint(&self.manager, assets);
        self.adapter().deposit(&assets, &self.manager)
    }
}

// --- construction ----------------------------------------------------------

#[test]
fn the_constructor_records_the_configuration() {
    let w = world();
    let a = w.adapter();

    assert_eq!(a.asset(), w.asset);
    assert_eq!(a.pool(), w.pool);
    assert_eq!(a.manager(), w.manager);
    assert_eq!(a.total_shares(), 0);
    assert_eq!(a.recorded_b_tokens(), 0);
    assert_eq!(
        a.total_assets(),
        0,
        "an adapter that owns nothing is worth nothing"
    );
}

#[test]
fn a_pool_that_does_not_list_the_asset_fails_construction() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    // The pool is built over one asset...
    let pool_asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    // ...and the adapter is pointed at a different one.
    let other_asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool = env.register(MockPool, (&pool_asset, &ONE_TO_ONE));
    let manager = Address::generate(&env);

    // Construction runs the constructor, and it fails on the pool's refusal,
    // which is the cheapest place to find a misconfiguration. A constructor
    // revert is a panic in the host, so it is caught rather than returned.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.register(BlendAdapter, (&pool, &other_asset, &manager))
    }));
    assert!(
        result.is_err(),
        "constructing over a reserve the pool does not list must fail"
    );
}

// --- deposit ---------------------------------------------------------------

#[test]
fn a_first_deposit_mints_shares_one_to_one_and_supplies_to_the_pool() {
    let w = world();
    w.mint(&w.manager, 1_000);

    let shares = w.adapter().deposit(&1_000, &w.manager);

    assert_eq!(shares, 1_000, "the first depositor sets the price at 1:1");
    assert_eq!(w.adapter().balance_of(&w.manager), 1_000);
    assert_eq!(w.adapter().total_shares(), 1_000);

    // The assets left the manager, passed through the adapter and are now the
    // pool's, held against the adapter's bToken claim.
    assert_eq!(w.token().balance(&w.manager), 0);
    assert_eq!(w.token().balance(&w.adapter), 0);
    assert_eq!(w.token().balance(&w.pool), 1_000);
    assert_eq!(w.pool().stored_b_tokens(&w.adapter), 1_000);
    assert_eq!(w.adapter().recorded_b_tokens(), 1_000);

    assert_eq!(w.adapter().total_assets(), 1_000);
    assert_eq!(w.adapter().convert_to_assets(&500), 500);
}

#[test]
fn a_second_deposit_gets_fewer_shares_after_the_rate_rises() {
    let w = world();
    w.deposit(1_000);

    // 1 bToken is now worth 1.25 assets.
    w.pool().set_b_rate(&1_250_000_000_000);
    assert_eq!(w.adapter().total_assets(), 1_250);

    let shares = w.deposit(1_000);

    // 1_000 assets at a share price of 1.25 buys 800 shares.
    assert_eq!(shares, 800);
    assert_eq!(w.adapter().total_shares(), 1_800);
    assert_eq!(w.adapter().balance_of(&w.manager), 1_800);
    // 2_000 assets in, 2_250 of value: the new depositor gets a fair slice and
    // no more.
    assert_eq!(w.adapter().total_assets(), 2_250);
}

#[test]
fn a_deposit_of_zero_or_less_is_refused() {
    let w = world();
    assert_eq!(
        w.adapter().try_deposit(&0, &w.manager),
        Err(trap(Error::ZeroAmount))
    );
    assert_eq!(
        w.adapter().try_deposit(&-1, &w.manager),
        Err(trap(Error::ZeroAmount))
    );
}

#[test]
fn a_deposit_that_would_mint_nothing_is_refused() {
    let w = world();
    w.deposit(1_000);
    // The position is now worth nothing but shares exist.
    w.pool().set_b_rate(&1);
    assert_eq!(w.adapter().total_assets(), 0);

    assert_eq!(
        w.adapter().try_deposit(&1, &w.manager),
        Err(trap(Error::ZeroShares)),
        "taking assets for zero shares would destroy them"
    );
}

#[test]
fn deposit_is_manager_only() {
    // Funded with auth mocked, because minting needs the admin's signature.
    let w = world();
    w.mint(&w.manager, 1_000);

    // From here on nothing is mocked, so `require_auth` is actually enforced.
    // An empty list replaces the blanket mock rather than adding to it.
    w.env.mock_auths(&[]);

    assert!(
        w.adapter().try_deposit(&1_000, &w.manager).is_err(),
        "a deposit with no authorisation must be refused"
    );
    assert_eq!(w.token().balance(&w.manager), 1_000, "nothing was taken");
}

#[test]
fn a_deposit_into_a_frozen_pool_reverts() {
    let w = world();
    w.mint(&w.manager, 1_000);
    w.pool().set_status(&4);

    assert!(
        w.adapter().try_deposit(&1_000, &w.manager).is_err(),
        "the pool blocks supply, so the deposit must fail rather than take the assets"
    );
    // Nothing was taken.
    assert_eq!(w.token().balance(&w.manager), 1_000);
    assert_eq!(w.adapter().total_shares(), 0);
}

// --- redeem ----------------------------------------------------------------

#[test]
fn redeem_pays_the_positions_value_and_burns_the_shares() {
    let w = world();
    w.deposit(1_000);

    // 10% up, backed by tokens standing in for repaid interest.
    w.pool().set_b_rate(&1_100_000_000_000);
    w.fund_pool(100);

    let paid = w.adapter().redeem(&1_000, &w.manager, &w.manager);

    assert_eq!(paid, 1_100, "the whole position, including the yield");
    assert_eq!(w.token().balance(&w.manager), 1_100);
    assert_eq!(w.adapter().total_shares(), 0);
    assert_eq!(w.adapter().balance_of(&w.manager), 0);
    assert_eq!(w.adapter().total_assets(), 0);
}

#[test]
fn redeem_pays_a_share_of_a_shared_position() {
    let w = world();
    w.deposit(1_000);

    // The manager is the only depositor by construction, so a partial
    // redemption is the interesting case: half the shares, half the value.
    w.pool().set_b_rate(&1_200_000_000_000);
    let paid = w.adapter().redeem(&400, &w.manager, &w.manager);

    assert_eq!(paid, 480, "400/1000 of 1_200");
    assert_eq!(w.adapter().total_shares(), 600);
    assert_eq!(w.adapter().total_assets(), 720);
}

#[test]
fn a_short_payment_from_the_pool_is_refused() {
    let w = world();
    w.deposit(1_000);

    // Deliberately unfaithful: the pool pays 10 stroops less than it says the
    // position is worth, and reports success. Blend has no such behaviour; this
    // is the only way to drive the adapter's balance check, because a correct
    // adapter asks for exactly what the pool itself reported.
    w.pool().set_underpay(&10);

    assert_eq!(
        w.adapter().try_redeem(&1_000, &w.manager, &w.manager),
        Err(trap(Error::InsufficientWithdrawal)),
        "a silent short payment must revert the whole withdrawal"
    );

    // The revert took the whole invocation with it: the shares are intact and
    // the pool paid nothing.
    assert_eq!(w.adapter().total_shares(), 1_000);
    assert_eq!(w.token().balance(&w.pool), 1_000);
    assert_eq!(w.token().balance(&w.manager), 0);
}

#[test]
fn redeeming_more_shares_than_held_is_refused() {
    let w = world();
    w.deposit(1_000);
    assert_eq!(
        w.adapter().try_redeem(&1_001, &w.manager, &w.manager),
        Err(trap(Error::InsufficientShares))
    );
    // Redeeming from empty is the same refusal, not a zero.
    let w2 = world();
    assert_eq!(
        w2.adapter().try_redeem(&1, &w2.manager, &w2.manager),
        Err(trap(Error::InsufficientShares))
    );
}

#[test]
fn redeeming_for_someone_other_than_the_manager_is_refused() {
    let w = world();
    w.deposit(1_000);
    let stranger = Address::generate(&w.env);
    assert_eq!(
        w.adapter().try_redeem(&1_000, &stranger, &stranger),
        Err(trap(Error::Unauthorized))
    );
}

#[test]
fn a_redeem_of_zero_is_refused() {
    let w = world();
    w.deposit(1_000);
    assert_eq!(
        w.adapter().try_redeem(&0, &w.manager, &w.manager),
        Err(trap(Error::ZeroAmount))
    );
}

#[test]
fn a_wiped_out_position_redeems_zero_without_calling_the_pool() {
    let w = world();
    w.deposit(1_000);

    // Total loss: 1_000 bTokens are now worth nothing, which is what a
    // socialised bad-debt event looks like from here.
    w.pool().set_b_rate(&1);

    let paid = w.adapter().redeem(&1_000, &w.manager, &w.manager);

    assert_eq!(paid, 0);
    assert_eq!(w.adapter().total_shares(), 0);
    // Asking Blend to burn zero bTokens reverts, so the adapter must not ask.
    // The bTokens are still at the pool, worth zero, and the epoch can settle.
    assert_eq!(w.pool().stored_b_tokens(&w.adapter), 1_000);
    assert_eq!(w.token().balance(&w.pool), 1_000);
}

#[test]
fn a_position_the_pool_has_forgotten_is_refused_rather_than_valued_at_zero() {
    let w = world();
    w.deposit(1_000);

    // What an expired Blend entry looks like: the position reads as empty, the
    // tokens are still in the reserve, and nothing raises an error.
    w.pool().expire_position(&w.adapter);
    assert_eq!(w.pool().stored_b_tokens(&w.adapter), 0);
    assert_eq!(
        w.pool().b_supply(),
        1_000,
        "the reserve still counts the bTokens"
    );

    assert_eq!(
        w.adapter().try_total_assets(),
        Err(trap(Error::PositionMissing))
    );
    assert_eq!(
        w.adapter().try_redeem(&1_000, &w.manager, &w.manager),
        Err(trap(Error::PositionMissing))
    );
}

#[test]
fn a_liquidity_limited_redeem_fails_rather_than_shrinking_the_payout() {
    let w = world();
    w.deposit(1_000);
    // The pool can only pay 900 of the 1_000 it owes: 90% utilisation.
    w.pool().set_available_liquidity(&900);

    assert!(
        w.adapter()
            .try_redeem(&1_000, &w.manager, &w.manager)
            .is_err(),
        "the pool refuses, so the redemption fails rather than paying short"
    );
    assert_eq!(w.adapter().total_shares(), 1_000);
}

// --- views -----------------------------------------------------------------

#[test]
fn total_assets_follows_the_pools_rate_in_both_directions() {
    let w = world();
    w.deposit(1_000);

    w.pool().set_b_rate(&1_050_000_000_000);
    assert_eq!(w.adapter().total_assets(), 1_050);

    // A falling rate is a real case: bad debt is socialised by lowering
    // b_rate, and the adapter must report the loss rather than remember the
    // peak.
    w.pool().set_b_rate(&900_000_000_000);
    assert_eq!(w.adapter().total_assets(), 900);
}

#[test]
fn convert_to_assets_is_pro_rata_and_reads_zero_when_there_is_nothing() {
    let w = world();
    assert_eq!(w.adapter().convert_to_assets(&1), 0);

    w.deposit(1_000);
    w.pool().set_b_rate(&1_250_000_000_000);

    assert_eq!(w.adapter().convert_to_assets(&0), 0);
    assert_eq!(w.adapter().convert_to_assets(&1), 1);
    assert_eq!(w.adapter().convert_to_assets(&400), 500);
    assert_eq!(w.adapter().convert_to_assets(&1_000), 1_250);

    let w2 = world();
    w2.deposit(1_000);
    w2.pool().set_b_rate(&1);
    assert_eq!(
        w2.adapter().convert_to_assets(&1_000),
        0,
        "a wiped-out position converts to zero, not to an error"
    );
}

#[test]
fn balance_of_is_the_share_ledger() {
    let w = world();
    let stranger = Address::generate(&w.env);
    assert_eq!(w.adapter().balance_of(&stranger), 0);
    w.deposit(1_000);
    assert_eq!(w.adapter().balance_of(&w.manager), 1_000);
    assert_eq!(w.adapter().balance_of(&stranger), 0);
}

// --- authorisation ---------------------------------------------------------

#[test]
fn the_adapter_authorises_the_pools_token_transfer() {
    // This is the gate the design note names: the pool moves the adapter's
    // tokens, two frames below the adapter, so the adapter has to declare that
    // transfer. `mock_all_auths` would let a missing or wrong entry pass, so
    // this test supplies only the *manager's* authorisation and lets the
    // adapter's own entry be the real one under test.
    let env = Env::default();
    let admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool = env.register(MockPool, (&asset, &ONE_TO_ONE));
    let manager = Address::generate(&env);
    let adapter = env.register(BlendAdapter, (&pool, &asset, &manager));

    // Minting needs the admin's signature, so it happens before the blanket
    // mock is replaced.
    env.mock_all_auths();
    token::StellarAssetClient::new(&env, &asset).mint(&manager, &1_000);

    // The manager authorises the deposit, and within it the transfer of its own
    // tokens into the adapter. That is exactly what the epoch manager declares.
    env.mock_auths(&[MockAuth {
        address: &manager,
        invoke: &MockAuthInvoke {
            contract: &adapter,
            fn_name: "deposit",
            args: (1_000_i128, manager.clone()).into_val(&env),
            sub_invokes: &[MockAuthInvoke {
                contract: &asset,
                fn_name: "transfer",
                args: (manager.clone(), adapter.clone(), 1_000_i128).into_val(&env),
                sub_invokes: &[],
            }],
        },
    }]);

    let shares = BlendAdapterClient::new(&env, &adapter).deposit(&1_000, &manager);
    assert_eq!(shares, 1_000);

    // What is asserted below is the tree the host actually demanded, not what
    // this test supplied: `env.auths()` reports the authenticated
    // authorisations for the last invocation.
    let auths = env.auths();
    assert_eq!(auths.len(), 1, "only the manager supplies an authorisation");
    let (who, root) = &auths[0];
    assert_eq!(*who, manager);

    // The manager authorised a call to the adapter with exactly the arguments
    // it was called with.
    let (contract, fn_name, args) = contract_fn(root);
    assert_eq!(contract, adapter);
    assert_eq!(fn_name, Symbol::new(&env, "deposit"));
    assert_eq!(args.len(), 2);
    assert_eq!(
        i128::try_from_val(&env, &args.get(0).unwrap()).unwrap(),
        1_000
    );
    assert_eq!(
        Address::try_from_val(&env, &args.get(1).unwrap()).unwrap(),
        manager,
        "the shares are minted to the manager that paid"
    );

    // Underneath it, the transfer of the manager's own tokens into the adapter:
    // the epoch manager declares exactly this entry before it calls its vault.
    assert_eq!(root.sub_invocations.len(), 1);
    let (sub_contract, sub_fn, sub_args) = contract_fn(root.sub_invocations.first().unwrap());
    assert_eq!(sub_contract, asset);
    assert_eq!(sub_fn, TRANSFER_FN);
    assert_eq!(
        Address::try_from_val(&env, &sub_args.get(0).unwrap()).unwrap(),
        manager
    );
    assert_eq!(
        Address::try_from_val(&env, &sub_args.get(1).unwrap()).unwrap(),
        adapter
    );
    assert_eq!(
        i128::try_from_val(&env, &sub_args.get(2).unwrap()).unwrap(),
        1_000
    );

    // The adapter's *own* authorisation — the one that lets the pool move the
    // adapter's tokens into itself — is an invoker-contract entry, and
    // `env.auths()` does not surface those. It is proven by this test running
    // at all: with no wildcard mocked, the SAC would refuse
    // `transfer(adapter -> pool)` if the entry were missing or wrong. Removing
    // `authorize_as_current_contract` from `deposit` makes this test fail,
    // which is the property that makes it worth having.
}

#[test]
fn the_declared_entry_is_exactly_the_transfer_the_pool_makes() {
    // The companion to the test above. The tree cannot be read back for an
    // invoker-contract entry, so the entry itself is pinned here: the asset
    // contract, the `transfer` function, the adapter as the source, the pool as
    // the destination, the exact amount, and no sub-invocations.
    let w = world();

    let entry = supply_auth_entry(&w.env, &w.adapter, &w.asset, &w.pool, 1_000);

    let sub = match entry {
        InvokerContractAuthEntry::Contract(sub) => sub,
        _ => panic!("expected a contract invocation entry, not a create-contract authorisation"),
    };
    assert_eq!(sub.context.contract, w.asset, "the token being moved");
    assert_eq!(sub.context.fn_name, TRANSFER_FN);
    assert_eq!(sub.context.args.len(), 3, "transfer(from, to, amount)");
    assert_eq!(
        Address::try_from_val(&w.env, &sub.context.args.get(0).unwrap()).unwrap(),
        w.adapter,
        "from: the adapter's own tokens"
    );
    assert_eq!(
        Address::try_from_val(&w.env, &sub.context.args.get(1).unwrap()).unwrap(),
        w.pool,
        "to: the pool"
    );
    assert_eq!(
        i128::try_from_val(&w.env, &sub.context.args.get(2).unwrap()).unwrap(),
        1_000,
        "amount: exactly what was supplied"
    );
    assert_eq!(sub.sub_invocations.len(), 0, "the transfer calls nothing");
}

// --- events ----------------------------------------------------------------

#[test]
fn deposit_and_redeem_publish_events() {
    let w = world();
    w.deposit(1_000);

    assert_eq!(
        w.event_data(Symbol::new(&w.env, "deposit_event")),
        Some(w.amounts(1_000, 1_000, 1_000)),
        "a deposit publishes the assets in, the shares minted and the pool position"
    );

    // A partial redemption at a moved price, so the three figures differ and
    // the assertion is about the payload rather than about the event existing.
    w.pool().set_b_rate(&1_100_000_000_000);
    w.fund_pool(100);
    w.adapter().redeem(&400, &w.manager, &w.manager);

    assert_eq!(
        w.event_data(Symbol::new(&w.env, "redeem_event")),
        Some(w.amounts(400, 440, 600)),
        "400 of 1_000 shares at 1.1 is 440 assets, leaving 600 bTokens at the pool"
    );
}
