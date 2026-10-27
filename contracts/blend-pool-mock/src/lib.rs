//! A test-only stand-in for a Blend v2 lending pool.
//!
//! **This is a mock. It is not the Blend pool and must never be deployed.**
//! Its whole purpose is that the adapter's behaviour can be driven through
//! every branch of the real pool's arithmetic and of its failure modes, on a
//! ledger the test controls, without a network.
//!
//! # What it reproduces, and from where
//!
//! Every behaviour below is taken from
//! [`docs/design/blend-adapter.md`](../../docs/design/blend-adapter.md) §4,
//! which read it from Blend's source. The point of the double is to be faithful
//! where the adapter depends on it, so the interesting parts are the ones that
//! are *surprising*:
//!
//! | Behaviour | Why the adapter cares |
//! | --- | --- |
//! | `b_tokens = floor(amount * 10^scalar / b_rate)` on supply | rounding is against the depositor on the way in |
//! | `to_burn = ceil(amount * 10^scalar / b_rate)` on withdrawal | and against the depositor on the way out |
//! | an over-sized withdrawal is **clamped, not rejected** | a short payment arrives with no error and no signal |
//! | a withdrawal over the available liquidity **reverts** (`1207`) | settlement can be blocked by borrowers, not by the vault |
//! | an expired position reads back **empty**, not as an error | the position can vanish while the epoch still needs it |
//! | supply is gated by reserve `enabled`, supply cap and pool status | a frozen pool blocks new epochs, not settlement |
//!
//! # What it does not reproduce
//!
//! Interest accrual. `b_rate` here is a number a test sets, not one that moves
//! with time — a test that wants a rising rate raises it, and one that wants the
//! loss path of a bad-debt socialisation lowers it. That is a deliberate
//! simplification: the real pool's accrual is lazy and depends on borrowers who
//! do not exist here, and a settable rate is what makes an epoch's outcome
//! reproducible.
//!
//! There is also no borrow side, so utilisation is modelled as a single
//! settable ceiling — [`MockPool::set_available_liquidity`] — rather than
//! derived from `b_supply` and `d_supply`. It stands in for the unborrowed
//! fraction of the reserve.
//!
//! # One knob that is deliberately unfaithful
//!
//! [`MockPool::set_underpay`] makes a withdrawal pay less than it was asked to
//! while reporting success. No such thing exists in Blend: it is there so the
//! adapter's short-payment check can be driven at all, because a correct
//! adapter asking for what the pool itself reported can never trigger the
//! clamp. It is labelled as a lie everywhere it appears, including in the
//! adapter's tests.
//!
//! # Auth
//!
//! The setters are permissionless. This is a test double with no value in it;
//! requiring an admin key would only make tests noisier. `submit` does enforce
//! the pool's real rule — `spender` authorises, and `from` authorises when it
//! is not the spender — because that is load-bearing for the adapter.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, Address, Env,
    Map, Vec,
};
use strata_blend_pool_interface::{
    BlendPool, Positions, Request, Reserve, ReserveConfig, ReserveData, REQUEST_SUPPLY,
    REQUEST_WITHDRAW,
};

/// Blend's own error codes, so a test can tell which failure it is looking at
/// and so the adapter's behaviour under each one matches the real pool.
///
/// The values are Blend's, not Strata's. Where the design note quotes a code,
/// that code is used here.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// Someone tried to act as the pool itself.
    InvalidPoolParticipant = 1200,
    /// The pool is frozen or in setup, so supply is blocked.
    InvalidPoolStatus = 1206,
    /// The withdrawal would take utilisation to 100%.
    InvalidUtilRate = 1207,
    /// The pool has no such reserve.
    InternalReserveNotFound = 1209,
    /// A supply would mint nothing, which would take the assets for free.
    InvalidBTokenMintAmount = 1216,
    /// Nothing to burn: the position is empty.
    InvalidBTokenBurnAmount = 1217,
    /// The reserve is at its supply cap.
    ExceededSupplyCap = 1220,
    /// The reserve is disabled, so supply is blocked.
    ReserveDisabled = 1223,
    /// Not constructed.
    NotInitialized = 1300,
    /// An amount or rate that cannot be used.
    InvalidAmount = 1301,
    /// A request type this pool does not implement.
    UnknownRequestType = 1302,
}

/// Pool statuses that block supply. 4 is frozen; 5 and 6 are setup states.
const SUPPLY_BLOCKED_STATUSES: [u32; 3] = [4, 5, 6];

#[contracttype]
pub enum DataKey {
    Asset,
    BRate,
    Scalar,
    Index,
    Decimals,
    Enabled,
    SupplyCap,
    AvailableLiquidity,
    PoolStatus,
    Underpay,
    BSupply,
    Position(Address),
}

#[contract]
pub struct MockPool;

#[contractimpl]
impl MockPool {
    /// Builds a pool with one reserve: `asset`, at `b_rate`, index 0, 12
    /// decimals of scale, enable, unlimited cap, and unlimited liquidity.
    pub fn __constructor(env: Env, asset: Address, b_rate: i128) {
        if env.storage().persistent().has(&DataKey::Asset) {
            panic!("already initialized");
        }
        if b_rate <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        let s = env.storage().persistent();
        s.set(&DataKey::Asset, &asset);
        s.set(&DataKey::BRate, &b_rate);
        // 12, as Blend v2 moved to. The adapter reads this rather than assuming
        // it, so a test can set 9 and prove the adapter follows the field.
        s.set(&DataKey::Scalar, &12_u32);
        s.set(&DataKey::Index, &0_u32);
        s.set(&DataKey::Decimals, &7_u32);
        s.set(&DataKey::Enabled, &true);
        s.set(&DataKey::SupplyCap, &i128::MAX);
        s.set(&DataKey::AvailableLiquidity, &i128::MAX);
        s.set(&DataKey::PoolStatus, &0_u32);
        s.set(&DataKey::Underpay, &0_i128);
        s.set(&DataKey::BSupply, &0_i128);
    }

    // --- test knobs --------------------------------------------------------

    /// Sets the bToken exchange rate directly. Rises for interest, falls for a
    /// bad-debt socialisation. Any positive value, in either direction.
    pub fn set_b_rate(env: Env, b_rate: i128) {
        if b_rate <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        env.storage().persistent().set(&DataKey::BRate, &b_rate);
    }

    /// The most the pool will pay out in one withdrawal, in asset base units.
    ///
    /// Stands in for the unborrowed fraction of the reserve: a withdrawal
    /// larger than this reverts with `InvalidUtilRate`, exactly as the real
    /// pool does at 100% utilisation. It is a ceiling, not an accounting
    /// figure: it is not increased by a supply, so a test that sets a limit
    /// keeps that limit.
    pub fn set_available_liquidity(env: Env, available: i128) {
        env.storage()
            .persistent()
            .set(&DataKey::AvailableLiquidity, &available);
    }

    /// Enables or disables the reserve. A disabled reserve blocks supply and
    /// still allows withdrawal, as the real pool does.
    pub fn set_reserve_enabled(env: Env, enabled: bool) {
        env.storage().persistent().set(&DataKey::Enabled, &enabled);
    }

    /// Sets the reserve's supply cap, in asset base units.
    pub fn set_supply_cap(env: Env, cap: i128) {
        env.storage().persistent().set(&DataKey::SupplyCap, &cap);
    }

    /// Sets the pool status. 4, 5 and 6 block supply.
    pub fn set_status(env: Env, status: u32) {
        env.storage()
            .persistent()
            .set(&DataKey::PoolStatus, &status);
    }

    /// Sets the decimal exponent for `b_rate`.
    pub fn set_scalar(env: Env, scalar: u32) {
        env.storage().persistent().set(&DataKey::Scalar, &scalar);
    }

    /// **Deliberately unfaithful.** Makes a withdrawal pay `underpay` fewer
    /// assets than the pool computed, with no error.
    ///
    /// Blend has no such behaviour. This exists so the adapter's short-payment
    /// check can be exercised: a correct adapter asks for exactly what the pool
    /// reported its position was worth, so it can never trigger the real clamp.
    /// The adapter's test that uses this says so.
    pub fn set_underpay(env: Env, underpay: i128) {
        env.storage()
            .persistent()
            .set(&DataKey::Underpay, &underpay);
    }

    /// Removes `account`'s position entry while leaving `b_supply` alone.
    ///
    /// This is what an expired TTL looks like from outside: the tokens are
    /// still accounted for in the reserve's total, and the account's claim on
    /// them reads as empty rather than as an error.
    pub fn expire_position(env: Env, account: Address) {
        env.storage()
            .persistent()
            .remove(&DataKey::Position(account));
    }

    // --- views -------------------------------------------------------------

    pub fn b_rate(env: Env) -> i128 {
        load_b_rate(&env)
    }

    pub fn b_supply(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::BSupply)
            .unwrap_or(0)
    }

    /// `account`'s bToken position, read directly from the entry rather than
    /// through `get_positions`, so a test can see the difference between "no
    /// entry" and "no tokens" without the pool's own defaulting in the way.
    pub fn stored_b_tokens(env: Env, account: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Position(account))
            .unwrap_or(0)
    }

    /// The pool's real token balance.
    pub fn token_balance(env: Env) -> i128 {
        token::Client::new(&env, &load_asset(&env)).balance(&env.current_contract_address())
    }
}

#[contractimpl]
impl BlendPool for MockPool {
    fn submit(
        env: Env,
        from: Address,
        spender: Address,
        to: Address,
        requests: Vec<Request>,
    ) -> Positions {
        let pool = env.current_contract_address();
        // The pool will not let anyone act as the pool.
        if from == pool || spender == pool || to == pool {
            panic_with_error!(&env, Error::InvalidPoolParticipant);
        }

        // Exactly the pool's rule: the spender authorises, and the from also
        // authorises when it is not the spender. `to` does not.
        spender.require_auth();
        if from != spender {
            from.require_auth();
        }

        let asset = load_asset(&env);

        for request in requests.iter() {
            if request.address != asset {
                panic_with_error!(&env, Error::InternalReserveNotFound);
            }
            match request.request_type {
                REQUEST_SUPPLY => supply(&env, &spender, &to, request.amount),
                REQUEST_WITHDRAW => withdraw(&env, &from, &to, request.amount),
                _ => panic_with_error!(&env, Error::UnknownRequestType),
            }
        }

        positions_of(&env, &to)
    }

    fn get_positions(env: Env, account: Address) -> Positions {
        positions_of(&env, &account)
    }

    fn get_reserve(env: Env, asset: Address) -> Reserve {
        if asset != load_asset(&env) {
            panic_with_error!(&env, Error::InternalReserveNotFound);
        }
        let s = env.storage().persistent();
        Reserve {
            asset,
            config: ReserveConfig {
                index: s.get(&DataKey::Index).unwrap_or(0),
                decimals: s.get(&DataKey::Decimals).unwrap_or(7),
                enabled: s.get(&DataKey::Enabled).unwrap_or(true),
                c_factor: 9_000,
                l_factor: 9_000,
                util: 5_000,
                max_util: 9_500,
                supply_cap: s.get(&DataKey::SupplyCap).unwrap_or(i128::MAX),
            },
            data: ReserveData {
                b_rate: load_b_rate(&env),
                b_supply: s.get(&DataKey::BSupply).unwrap_or(0),
                d_rate: 0,
                d_supply: 0,
            },
            scalar: s.get(&DataKey::Scalar).unwrap_or(12),
        }
    }
}

// --- internals -------------------------------------------------------------

fn supply(env: &Env, spender: &Address, to: &Address, amount: i128) {
    if amount <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
    let s = env.storage().persistent();
    let enabled: bool = s.get(&DataKey::Enabled).unwrap_or(true);
    if !enabled {
        panic_with_error!(env, Error::ReserveDisabled);
    }
    let status: u32 = s.get(&DataKey::PoolStatus).unwrap_or(0);
    if SUPPLY_BLOCKED_STATUSES.contains(&status) {
        panic_with_error!(env, Error::InvalidPoolStatus);
    }
    let cap: i128 = s.get(&DataKey::SupplyCap).unwrap_or(i128::MAX);
    let b_supply: i128 = s.get(&DataKey::BSupply).unwrap_or(0);
    let rate = load_b_rate(env);
    let minted = to_b_token_down(env, amount, rate);

    // The real pool refuses a supply that would mint nothing: taking the assets
    // for no claim is worse than failing.
    if minted <= 0 {
        panic_with_error!(env, Error::InvalidBTokenMintAmount);
    }
    let supplied_value = b_token_value(env, b_supply, rate);
    if supplied_value.checked_add(amount).unwrap_or(i128::MAX) > cap {
        panic_with_error!(env, Error::ExceededSupplyCap);
    }

    // transfer(spender -> pool)
    token::Client::new(env, &load_asset(env)).transfer(
        spender,
        env.current_contract_address(),
        &amount,
    );

    let key = DataKey::Position(to.clone());
    let held: i128 = s.get(&key).unwrap_or(0);
    s.set(
        &key,
        &held
            .checked_add(minted)
            .unwrap_or_else(|| panic_with_error!(env, Error::InvalidAmount)),
    );
    s.set(&DataKey::BSupply, &(b_supply + minted));
}

fn withdraw(env: &Env, from: &Address, to: &Address, amount: i128) {
    if amount <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
    let s = env.storage().persistent();
    let rate = load_b_rate(env);
    let key = DataKey::Position(from.clone());
    let cur: i128 = s.get(&key).unwrap_or(0);
    if cur <= 0 {
        panic_with_error!(env, Error::InvalidBTokenBurnAmount);
    }

    // Rounding up on the way out, as the real pool does.
    let mut to_burn = to_b_token_up(env, amount, rate);
    let mut tokens_out = amount;
    if to_burn > cur {
        // The clamp. No error, no signal: burn what there is and pay what that
        // is worth. This is the behaviour the adapter's balance check exists
        // for.
        to_burn = cur;
        tokens_out = b_token_value(env, cur, rate);
    }

    // Utilisation gate: the pool will not pay out more than it has unborrowed.
    let available: i128 = s.get(&DataKey::AvailableLiquidity).unwrap_or(i128::MAX);
    if tokens_out > available {
        panic_with_error!(env, Error::InvalidUtilRate);
    }

    // Deliberately unfaithful hostile knob. See `set_underpay`.
    let underpay: i128 = s.get(&DataKey::Underpay).unwrap_or(0);
    if underpay > 0 {
        tokens_out = (tokens_out - underpay).max(0);
    }

    let b_supply: i128 = s.get(&DataKey::BSupply).unwrap_or(0);
    s.set(&key, &(cur - to_burn));
    s.set(&DataKey::BSupply, &(b_supply - to_burn));
    s.set(&DataKey::AvailableLiquidity, &(available - tokens_out));

    if tokens_out > 0 {
        // transfer(pool -> to): the pool is the source and the invoker, so it
        // needs no separate authorisation.
        token::Client::new(env, &load_asset(env)).transfer(
            &env.current_contract_address(),
            to,
            &tokens_out,
        );
    }
}

fn positions_of(env: &Env, account: &Address) -> Positions {
    let mut supply = Map::new(env);
    let tokens: i128 = env
        .storage()
        .persistent()
        .get(&DataKey::Position(account.clone()))
        .unwrap_or(0);
    // A zero position is stored as no key, matching the pool's own behaviour of
    // dropping empty entries.
    if tokens > 0 {
        supply.set(
            env.storage().persistent().get(&DataKey::Index).unwrap_or(0),
            tokens,
        );
    }
    Positions {
        collateral: Map::new(env),
        liabilities: Map::new(env),
        supply,
    }
}

/// `10^scalar`, the scale `b_rate` is expressed in.
fn scale(env: &Env, scalar: u32) -> i128 {
    let mut acc: i128 = 1;
    for _ in 0..scalar {
        acc = acc
            .checked_mul(10)
            .unwrap_or_else(|| panic_with_error!(env, Error::InvalidAmount));
    }
    acc
}

/// `floor(amount * 10^scalar / b_rate)`. Rounding is against the depositor.
fn to_b_token_down(env: &Env, amount: i128, b_rate: i128) -> i128 {
    let scalar: u32 = env
        .storage()
        .persistent()
        .get(&DataKey::Scalar)
        .unwrap_or(12);
    amount
        .checked_mul(scale(env, scalar))
        .unwrap_or_else(|| panic_with_error!(env, Error::InvalidAmount))
        / b_rate
}

/// `ceil(amount * 10^scalar / b_rate)`. Rounding is against the withdrawer.
fn to_b_token_up(env: &Env, amount: i128, b_rate: i128) -> i128 {
    let scalar: u32 = env
        .storage()
        .persistent()
        .get(&DataKey::Scalar)
        .unwrap_or(12);
    let scale = scale(env, scalar);
    let numerator = amount
        .checked_mul(scale)
        .unwrap_or_else(|| panic_with_error!(env, Error::InvalidAmount));
    (numerator + b_rate - 1) / b_rate
}

/// `floor(b_tokens * b_rate / 10^scalar)`.
fn b_token_value(env: &Env, b_tokens: i128, b_rate: i128) -> i128 {
    let scalar: u32 = env
        .storage()
        .persistent()
        .get(&DataKey::Scalar)
        .unwrap_or(12);
    b_tokens
        .checked_mul(b_rate)
        .unwrap_or_else(|| panic_with_error!(env, Error::InvalidAmount))
        / scale(env, scalar)
}

fn load_asset(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&DataKey::Asset)
        .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
}

fn load_b_rate(env: &Env) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::BRate)
        .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
}

#[cfg(test)]
mod test;
