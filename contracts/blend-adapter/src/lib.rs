//! A Strata vault that supplies to a Blend lending pool.
//!
//! This is the other implementation of [`VaultInterface`], alongside
//! `strata-mock-vault`. Where the mock pays a rate an admin types in, this one
//! holds a real position in a real lending pool on Stellar testnet: it supplies
//! the underlying to Blend and its claim is a count of bTokens whose value moves
//! with the pool's `b_rate`.
//!
//! The epoch manager is not changed by any of this. The adapter implements the
//! existing interface exactly, so the manager cannot tell the two apart, which
//! is the point of the interface existing at all. The design and its
//! consequences are in
//! [`docs/design/blend-adapter.md`](../../docs/design/blend-adapter.md).
//!
//! **Unaudited, testnet only.** This holds real, if valueless, positions in
//! somebody else's contract.
//!
//! # The three things worth understanding
//!
//! **The share price is the pool's, not ours.** The adapter keeps its own share
//! ledger so depositors have a claim to hand in, but a share is worth what the
//! pool says the underlying position is worth:
//!
//! ```text
//! position value = floor(b_tokens * b_rate / 10^scalar)
//! share price    = position value / adapter shares
//! ```
//!
//! `b_rate` rises as borrowers pay interest and **falls when the pool
//! socialises bad debt**, so a deposit here can be worth less when it is
//! redeemed. That is not a bug in the adapter; it is the loss reaching `V`, and
//! the waterfall already handles a `V` below the principal.
//!
//! **A withdrawal is validated by measurement, not by trust.** Blend clamps an
//! over-sized withdrawal instead of rejecting it: it burns what is there, pays
//! what that is worth, and reports success. So the adapter measures its own
//! token balance before and after and reverts if it received less than it asked
//! for. Soroban rolls the whole invocation back, so a failed check leaves no
//! half-withdrawn position behind.
//!
//! **A position can expire.** A Blend user entry that goes untouched for
//! roughly 120 days reads back as *empty* rather than as an error, and a Strata
//! epoch may run for 365 days. The adapter therefore keeps a record of what the
//! pool last told it it held, and refuses to value or redeem a position the
//! pool has forgotten. [`Error::PositionMissing`] is that refusal. Making the
//! position survive is a separate, unsolved problem — see `docs/risks.md`.
//!
//! # Deliberately absent
//!
//! No admin, no upgrade path, no fees, no pause. The pool, the asset and the one
//! permitted depositor are fixed at construction and cannot be changed, which
//! means there is no key that can redirect the position. The costs of that
//! choice are recorded as risks rather than hidden behind an escape hatch.

#![no_std]

use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error,
    symbol_short, token, vec, Address, Env, IntoVal, Symbol, Vec,
};
use strata_blend_pool_interface::{BlendPoolClient, Request, REQUEST_SUPPLY, REQUEST_WITHDRAW};
use strata_vault_interface::VaultInterface;

/// Minimum remaining life before a storage entry is worth extending, in
/// ledgers. Same values as the epoch manager, for the same reason.
const BUMP_THRESHOLD: u32 = 100_000;
/// How much life to add, in ledgers. Generous relative to a one-year maximum
/// term, because the cost of over-extending is a little rent and the cost of
/// losing the position is the whole epoch.
///
/// **This extends the adapter's own entries, and cannot extend the pool's.**
/// A contract cannot extend another contract's persistent entries, and the
/// adapter's claim on the pool is an entry in the pool's storage. Nothing here
/// solves that.
const BUMP_AMOUNT: u32 = 4_000_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// The contract is already initialised. A constructor cannot be re-run.
    AlreadyInitialized = 1,
    /// An amount was zero or negative where zero is meaningless.
    ZeroAmount = 2,
    /// A deposit would have to mint zero shares, so the assets would be taken
    /// for nothing.
    ZeroShares = 3,
    /// The owner does not hold that many adapter shares.
    InsufficientShares = 4,
    /// The caller is not the configured manager, or is asking on behalf of
    /// somebody who is not.
    Unauthorized = 5,
    /// The pool does not list the asset this adapter was built over. Raised at
    /// construction, so a misconfigured adapter fails at deployment rather than
    /// at the first deposit.
    ReserveNotFound = 6,
    /// The adapter recorded a position and the pool now reports none for it.
    ///
    /// This is what an expired Blend entry looks like from the outside, and it
    /// is the reason this contract keeps its own record: the pool's own
    /// behaviour is to report an empty position, not an error.
    PositionMissing = 7,
    /// The pool paid less than the adapter asked for and did not say so.
    InsufficientWithdrawal = 8,
    /// A supply succeeded but the pool reported no position for the reserve
    /// afterwards, so the assets went somewhere this contract cannot see.
    SupplyNotRegistered = 9,
    /// Arithmetic exceeded the documented bounds.
    ArithmeticOverflow = 10,
}

#[contracttype]
pub enum DataKey {
    /// The Blend pool. Immutable.
    Pool,
    /// The underlying token. Immutable, and pinned by the manager at epoch
    /// creation.
    Asset,
    /// The one address allowed to deposit and redeem. Immutable.
    Manager,
    /// The last position size the pool reported for this adapter, in bTokens.
    ///
    /// A canary, not an accounting figure: the live position is always read from
    /// the pool. Its only job is to notice the difference between "the pool says
    /// the position is worth nothing" and "the pool has forgotten the position
    /// exists".
    RecordedBTokens,
    /// Adapter shares outstanding.
    TotalShares,
    /// Adapter shares held by one account.
    ShareBalance(Address),
}

/// Topics: `deposit`, the receiver. Data: the amounts moved.
#[contractevent]
pub struct DepositEvent {
    #[topic]
    pub receiver: Address,
    /// Underlying assets supplied.
    pub assets: i128,
    /// Adapter shares minted.
    pub shares: i128,
    /// The pool's reported position after the supply, in bTokens.
    pub b_tokens: i128,
}

/// Topics: `redeem`, the owner, the receiver. Data: the amounts moved.
#[contractevent]
pub struct RedeemEvent {
    #[topic]
    pub owner: Address,
    #[topic]
    pub receiver: Address,
    /// Adapter shares burned.
    pub shares: i128,
    /// Underlying assets actually paid out — the number the waterfall receives
    /// as `V`, not a report of what was asked for.
    pub assets: i128,
    /// The pool's reported position after the withdrawal, in bTokens.
    pub b_tokens: i128,
}

#[contract]
pub struct BlendAdapter;

#[contractimpl]
impl BlendAdapter {
    /// Builds an adapter over `pool` for `asset`, with `manager` as the only
    /// account allowed to deposit and redeem.
    ///
    /// Verifies that the pool actually lists `asset` before storing anything.
    /// A pool that does not answer, or does not know the asset, fails the
    /// deployment — which is the cheapest possible place to find a
    /// misconfiguration.
    pub fn __constructor(env: Env, pool: Address, asset: Address, manager: Address) {
        if env.storage().persistent().has(&DataKey::Pool) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }

        match BlendPoolClient::new(&env, &pool).try_get_reserve(&asset) {
            Ok(_) => {}
            Err(_) => panic_with_error!(&env, Error::ReserveNotFound),
        }

        let s = env.storage().persistent();
        s.set(&DataKey::Pool, &pool);
        s.set(&DataKey::Asset, &asset);
        s.set(&DataKey::Manager, &manager);
        s.set(&DataKey::RecordedBTokens, &0_i128);
        s.set(&DataKey::TotalShares, &0_i128);
        for key in [
            DataKey::Pool,
            DataKey::Asset,
            DataKey::Manager,
            DataKey::RecordedBTokens,
            DataKey::TotalShares,
        ] {
            s.extend_ttl(&key, BUMP_THRESHOLD, BUMP_AMOUNT);
        }
    }

    /// The Blend pool this adapter supplies to.
    pub fn pool(env: Env) -> Address {
        load_pool(&env)
    }

    /// The one address permitted to deposit and redeem.
    pub fn manager(env: Env) -> Address {
        load_manager(&env)
    }

    /// The last position the pool reported, in bTokens.
    ///
    /// For diagnostics and for the test that pins the expiry behaviour. The
    /// live position is read from the pool; this is only what the adapter was
    /// last told.
    pub fn recorded_b_tokens(env: Env) -> i128 {
        load_recorded(&env)
    }

    /// Adapter shares outstanding.
    pub fn total_shares(env: Env) -> i128 {
        total_shares(&env)
    }
}

#[contractimpl]
impl VaultInterface for BlendAdapter {
    fn asset(env: Env) -> Address {
        load_asset(&env)
    }

    /// Supplies `assets` to the pool and mints adapter shares to `receiver`.
    ///
    /// Manager only. Shares are priced before the deposit arrives, so a new
    /// depositor cannot dilute the ones already outstanding.
    ///
    /// Effects are written before the pool is called: the share ledger is
    /// updated first, so a re-entrant call from a hostile token sees a
    /// consistent contract rather than one mid-update. Soroban rolls back the
    /// whole invocation on failure, so nothing partial can persist either way.
    fn deposit(env: Env, assets: i128, receiver: Address) -> i128 {
        load_manager(&env).require_auth();
        if assets <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        let supply_before = total_shares(&env);
        let snapshot = position_snapshot(&env);
        let recorded = load_recorded(&env);

        // With no shareholders outstanding there is nobody to protect, so a
        // stale record is corrected rather than treated as a lost position. The
        // pool's own report overwrites it below.
        let assets_before = if supply_before == 0 {
            0
        } else {
            live_assets(&env, recorded, &snapshot)
        };

        let shares = if supply_before == 0 {
            // First depositor defines the share price at 1:1.
            assets
        } else if assets_before <= 0 {
            // Shares exist and are worth nothing: a deposit would mint nothing
            // for a real transfer, so refuse rather than take the assets.
            0
        } else {
            mul_div(&env, assets, supply_before, assets_before)
        };
        if shares <= 0 {
            panic_with_error!(&env, Error::ZeroShares);
        }

        credit_shares(&env, &receiver, shares);

        let asset = load_asset(&env);
        let me = env.current_contract_address();

        // The manager declared this exact transfer before it called us: it is
        // the manager's own tokens moving into this contract, and the manager
        // is not the invoker of this call.
        token::Client::new(&env, &asset).transfer(&load_manager(&env), &me, &assets);

        // The pool, one frame below us, will move our tokens into itself. That
        // token call is two frames below us, so it needs declaring here.
        env.authorize_as_current_contract(vec![
            &env,
            supply_auth_entry(&env, &me, &asset, &load_pool(&env), assets),
        ]);

        let positions = BlendPoolClient::new(&env, &load_pool(&env)).submit(
            &me,
            &me,
            &me,
            &supply_request(&env, &asset, assets),
        );
        let b_tokens_after = positions.supply.get(snapshot.index).unwrap_or(0);
        if b_tokens_after <= 0 {
            panic_with_error!(&env, Error::SupplyNotRegistered);
        }
        store_recorded(&env, b_tokens_after);

        DepositEvent {
            receiver,
            assets,
            shares,
            b_tokens: b_tokens_after,
        }
        .publish(&env);

        shares
    }

    /// Burns `shares` from `owner`, withdraws the matching value from the pool,
    /// and pays what actually arrived to `receiver`.
    ///
    /// Manager only, and only for its own position.
    ///
    /// The return value is the assets **measured to have arrived**, not the
    /// amount requested. The epoch manager treats it as `V`, the waterfall's
    /// input, so a figure this contract reported rather than held would be a
    /// lie the settlement then split.
    ///
    /// A position worth nothing is redeemed without calling the pool: asking
    /// Blend to burn zero bTokens reverts, and a wiped-out epoch still has to
    /// be settleable.
    fn redeem(env: Env, shares: i128, receiver: Address, owner: Address) -> i128 {
        let manager = load_manager(&env);
        manager.require_auth();
        if owner != manager {
            panic_with_error!(&env, Error::Unauthorized);
        }
        if shares <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }
        let supply_before = total_shares(&env);
        if supply_before <= 0 || share_balance(&env, &owner) < shares {
            panic_with_error!(&env, Error::InsufficientShares);
        }

        let snapshot = position_snapshot(&env);
        let total = live_assets(&env, load_recorded(&env), &snapshot);
        let assets = mul_div(&env, shares, total, supply_before);

        // Effects first: the shares are gone before anything leaves.
        debit_shares(&env, &owner, shares);

        let asset = load_asset(&env);
        let me = env.current_contract_address();
        let token = token::Client::new(&env, &asset);
        let mut received: i128 = 0;

        if assets > 0 {
            let before = token.balance(&me);

            let positions = BlendPoolClient::new(&env, &load_pool(&env)).submit(
                &me,
                &me,
                &me,
                &withdraw_request(&env, &asset, assets),
            );

            // Measure rather than trust. Blend clamps an over-sized withdrawal
            // and reports success, so the balance is the only honest answer to
            // "did we get what we asked for".
            received = token.balance(&me) - before;
            if received < assets {
                panic_with_error!(&env, Error::InsufficientWithdrawal);
            }

            // Recorded before the outbound transfer, so the contract's state is
            // consistent before control leaves it.
            store_recorded(&env, positions.supply.get(snapshot.index).unwrap_or(0));
        }

        if received > 0 {
            token.transfer(&me, &receiver, &received);
        }

        RedeemEvent {
            owner,
            receiver,
            shares,
            assets: received,
            b_tokens: load_recorded(&env),
        }
        .publish(&env);

        received
    }

    /// The current value of the adapter's whole position, read live from the
    /// pool.
    ///
    /// Cannot be a stored number: `b_rate` moves whenever anyone touches the
    /// reserve, without this contract doing anything.
    ///
    /// Traps with [`Error::PositionMissing`] rather than reporting zero when the
    /// pool has stopped recognising a position this contract recorded. A zero
    /// here would be indistinguishable from a position that genuinely lost
    /// everything, and those two need different responses.
    fn total_assets(env: Env) -> i128 {
        live_total_assets(&env)
    }

    /// The value of `shares` at the current position price.
    ///
    /// Read-only, and never used for payout: settlement uses what `redeem`
    /// actually returned.
    fn convert_to_assets(env: Env, shares: i128) -> i128 {
        let supply = total_shares(&env);
        if shares <= 0 || supply <= 0 {
            return 0;
        }
        mul_div(&env, shares, live_total_assets(&env), supply)
    }

    /// The adapter shares `account` holds.
    fn balance_of(env: Env, account: Address) -> i128 {
        share_balance(&env, &account)
    }
}

// --- the pool, read and written --------------------------------------------

/// Everything needed to value a position, in one pass over the pool.
///
/// Two reads, because the pool has no single "what is my position worth" view:
/// the size lives in the account's position and the price lives in the reserve.
pub struct Snapshot {
    /// bTokens the pool reports for this adapter.
    pub reported: i128,
    /// bTokens per asset unit, scaled by `scalar`.
    pub b_rate: i128,
    /// The scale `b_rate` is expressed in.
    pub scalar: u32,
    /// The reserve key the position appears under.
    pub index: u32,
}

fn position_snapshot(env: &Env) -> Snapshot {
    let pool = BlendPoolClient::new(env, &load_pool(env));
    let reserve = pool.get_reserve(&load_asset(env));
    let positions = pool.get_positions(&env.current_contract_address());
    let index = reserve.config.index;
    let reported = positions.supply.get(index).unwrap_or(0);
    Snapshot {
        reported,
        b_rate: reserve.data.b_rate,
        scalar: reserve.scalar,
        index,
    }
}

/// The position's value, and the check that it is still ours.
fn live_assets(env: &Env, recorded: i128, snapshot: &Snapshot) -> i128 {
    if recorded > 0 && snapshot.reported <= 0 {
        panic_with_error!(env, Error::PositionMissing);
    }
    b_token_value(env, snapshot.reported, snapshot.b_rate, snapshot.scalar)
}

fn live_total_assets(env: &Env) -> i128 {
    let snapshot = position_snapshot(env);
    live_assets(env, load_recorded(env), &snapshot)
}

fn supply_request(env: &Env, asset: &Address, amount: i128) -> Vec<Request> {
    let mut requests = Vec::new(env);
    requests.push_back(Request {
        request_type: REQUEST_SUPPLY,
        address: asset.clone(),
        amount,
    });
    requests
}

fn withdraw_request(env: &Env, asset: &Address, amount: i128) -> Vec<Request> {
    let mut requests = Vec::new(env);
    requests.push_back(Request {
        request_type: REQUEST_WITHDRAW,
        address: asset.clone(),
        amount,
    });
    requests
}

/// The function name a token `transfer` is authorised under.
pub const TRANSFER_FN: Symbol = symbol_short!("transfer");

/// Builds the authorisation entry that lets the pool move this adapter's own
/// tokens into itself.
///
/// The adapter calls the pool, and the pool calls the token contract. That token
/// call sits two frames below the adapter, and a deeper call made on a
/// contract's behalf is *not* implicitly authorised — hence the explicit entry.
/// Without it the supply is rejected on chain, which is the right failure mode:
/// a missing entry stops a deposit rather than permitting an unauthorised pull.
///
/// Same pattern, same argument list, as the epoch manager's own entry, except
/// that `source` is a parameter rather than `env.current_contract_address()`.
/// That is the same choice the epoch manager makes about `from`: the scope of
/// what is being authorised should be visible in the code that asks for it, and
/// a function that reads the current contract cannot be tested outside a
/// contract call.
pub fn supply_auth_entry(
    env: &Env,
    source: &Address,
    asset: &Address,
    pool: &Address,
    assets: i128,
) -> InvokerContractAuthEntry {
    let me = source.clone();
    InvokerContractAuthEntry::Contract(SubContractInvocation {
        context: ContractContext {
            contract: asset.clone(),
            fn_name: TRANSFER_FN,
            args: vec![
                env,
                me.into_val(env),
                pool.clone().into_val(env),
                assets.into_val(env),
            ],
        },
        sub_invocations: vec![env],
    })
}

// --- arithmetic ------------------------------------------------------------

/// `10^scalar`, with a trap rather than a wrap.
fn scale(env: &Env, scalar: u32) -> i128 {
    let mut acc: i128 = 1;
    for _ in 0..scalar {
        acc = acc
            .checked_mul(10)
            .unwrap_or_else(|| panic_with_error!(env, Error::ArithmeticOverflow));
    }
    acc
}

/// `floor(b_tokens * b_rate / 10^scalar)`.
fn b_token_value(env: &Env, b_tokens: i128, b_rate: i128, scalar: u32) -> i128 {
    if b_tokens <= 0 || b_rate <= 0 {
        return 0;
    }
    b_tokens
        .checked_mul(b_rate)
        .unwrap_or_else(|| panic_with_error!(env, Error::ArithmeticOverflow))
        / scale(env, scalar)
}

/// `a * b / c`, floored, trapping on overflow.
fn mul_div(env: &Env, a: i128, b: i128, c: i128) -> i128 {
    if c <= 0 {
        panic_with_error!(env, Error::ArithmeticOverflow);
    }
    a.checked_mul(b)
        .unwrap_or_else(|| panic_with_error!(env, Error::ArithmeticOverflow))
        / c
}

// --- storage ---------------------------------------------------------------

fn load_pool(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&DataKey::Pool)
        .unwrap_or_else(|| panic_with_error!(env, Error::AlreadyInitialized))
}

fn load_asset(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&DataKey::Asset)
        .unwrap_or_else(|| panic_with_error!(env, Error::AlreadyInitialized))
}

fn load_manager(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&DataKey::Manager)
        .unwrap_or_else(|| panic_with_error!(env, Error::AlreadyInitialized))
}

fn load_recorded(env: &Env) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::RecordedBTokens)
        .unwrap_or(0)
}

fn store_recorded(env: &Env, b_tokens: i128) {
    env.storage()
        .persistent()
        .set(&DataKey::RecordedBTokens, &b_tokens);
    bump(env, &DataKey::RecordedBTokens);
}

fn total_shares(env: &Env) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::TotalShares)
        .unwrap_or(0)
}

fn share_balance(env: &Env, account: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::ShareBalance(account.clone()))
        .unwrap_or(0)
}

fn credit_shares(env: &Env, account: &Address, shares: i128) {
    let held = share_balance(env, account);
    write_shares(
        env,
        account,
        held.checked_add(shares)
            .unwrap_or_else(|| panic_with_error!(env, Error::ArithmeticOverflow)),
    );
}

fn debit_shares(env: &Env, account: &Address, shares: i128) {
    let held = share_balance(env, account);
    let after = held
        .checked_sub(shares)
        .unwrap_or_else(|| panic_with_error!(env, Error::InsufficientShares));
    if after < 0 {
        panic_with_error!(env, Error::InsufficientShares);
    }
    write_shares(env, account, after);
}

fn write_shares(env: &Env, account: &Address, shares: i128) {
    let total = total_shares(env);
    let delta = shares - share_balance(env, account);
    env.storage()
        .persistent()
        .set(&DataKey::ShareBalance(account.clone()), &shares);
    env.storage().persistent().set(
        &DataKey::TotalShares,
        &total
            .checked_add(delta)
            .unwrap_or_else(|| panic_with_error!(env, Error::ArithmeticOverflow)),
    );
    bump(env, &DataKey::ShareBalance(account.clone()));
    bump(env, &DataKey::TotalShares);
}

fn bump(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, BUMP_THRESHOLD, BUMP_AMOUNT);
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test;
