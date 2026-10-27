//! The subset of a Blend v2 lending pool that a Strata Blend adapter uses.
//!
//! This crate has no logic. It declares the types and the three functions the
//! adapter calls, and `#[contractclient]` generates a `BlendPoolClient` from
//! them. Splitting it out mirrors `strata-vault-interface`: the adapter depends
//! on a pool *interface*, so the test double in `strata-blend-pool-mock` and
//! the real pool on Stellar testnet are interchangeable behind it.
//!
//! # Provenance, and what is not verified here
//!
//! These shapes are transcribed from the research in
//! [`docs/design/blend-adapter.md`](../../docs/design/blend-adapter.md) §4,
//! which read them from `blend-capital/blend-contracts-v2` at
//! `ba22b487b2c5057a4ecc28b05b5193c28e4bd117` and from the deployed testnet
//! spec.
//!
//! **They have not been checked against a deployed pool by this crate.** The
//! task that points an adapter at the real testnet pool is the one that can do
//! that, and until it does, the only implementation that has been run against
//! this interface is the test double. Treat the field order and the widths as
//! the record of a reading, not as a verified ABI.
//!
//! # Two things worth knowing before using it
//!
//! - **`b_rate` and `d_rate` are scaled, and the scale is a field, not a
//!   constant.** v2 uses 12 decimals and v1 used 9. The interface carries
//!   `Reserve::scalar` so a caller can use `10^scalar` instead of hard-coding
//!   `1e12`. The design note records that reading v1 by mistake gets this wrong
//!   by a factor of a thousand.
//! - **A withdrawal larger than the position is clamped, not rejected.** The
//!   pool burns what there is and pays what that is worth. Nothing in the
//!   return value says the request was short. A caller that needs an exact
//!   withdrawal has to measure its own balance, which is what the adapter does.
//!
//! Deliberately absent: `flash_loan`, `borrow`, `repay`, `liquidate`,
//! `get_config`, `get_reserve_list`, the admin queue, and every b/d-token
//! surface. A pure supplier needs none of them, and leaving them out keeps the
//! interface small enough to read in full.

#![no_std]

use soroban_sdk::{contractclient, contracttype, Address, Env, Map, Vec};

/// `Request::request_type` for a supply. Matches Blend's own discriminant.
pub const REQUEST_SUPPLY: u32 = 0;
/// `Request::request_type` for a withdrawal of uncollateralised supply.
pub const REQUEST_WITHDRAW: u32 = 1;

/// One instruction inside a `submit` call.
///
/// `address` is the asset, not a recipient: the destination is the `to`
/// argument of `submit` itself.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// [`REQUEST_SUPPLY`] or [`REQUEST_WITHDRAW`].
    pub request_type: u32,
    /// The reserve's underlying asset address.
    pub address: Address,
    /// Base units of the asset.
    pub amount: i128,
}

/// An account's position, keyed by reserve index.
///
/// Only `supply` is ever populated for a pure supplier. `collateral` and
/// `liabilities` are declared because they are part of the returned struct and
/// because a position that unexpectedly had collateral is worth being able to
/// see.
///
/// **A missing key and a zero are the same thing to the pool.** An expired
/// entry reads back as a default struct with empty maps, not as an error —
/// which is why the adapter keeps its own record of what it put there and
/// compares.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Positions {
    pub collateral: Map<u32, i128>,
    pub liabilities: Map<u32, i128>,
    pub supply: Map<u32, i128>,
}

/// A reserve's parameters.
///
/// A supplier's only meaningful fields here are `index`, `enabled` and
/// `supply_cap`; the collateral and liquidation factors are recorded for
/// completeness and are never read by the adapter.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveConfig {
    /// The key this reserve's position appears under in [`Positions`].
    pub index: u32,
    /// Decimals of the underlying asset, for display only. XLM is 7.
    pub decimals: u32,
    /// Whether supply is currently permitted. A disabled reserve still allows
    /// withdrawal.
    pub enabled: bool,
    pub c_factor: u32,
    pub l_factor: u32,
    /// Target utilisation, in basis points.
    pub util: u32,
    /// Borrowing ceiling, in basis points.
    pub max_util: u32,
    /// Maximum total supply, in base units of the asset.
    pub supply_cap: i128,
}

/// A reserve's live state.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveData {
    /// bTokens per unit of the asset, scaled by [`Reserve::scalar`]. Rises with
    /// accrued interest and **falls when bad debt is socialised**, which is the
    /// only way a supplier's principal can shrink on the pool's own terms.
    pub b_rate: i128,
    /// bTokens outstanding across all suppliers.
    pub b_supply: i128,
    /// Debt tokens per unit borrowed, scaled by [`Reserve::scalar`].
    pub d_rate: i128,
    pub d_supply: i128,
}

/// A reserve: which asset, its parameters, its live state, and the scale.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reserve {
    pub asset: Address,
    pub config: ReserveConfig,
    pub data: ReserveData,
    /// The decimal exponent `b_rate` and `d_rate` are scaled by. 12 in v2, 9 in
    /// v1. Read it; do not assume it.
    pub scalar: u32,
}

/// The pool surface the adapter depends on.
///
/// `Env` is the first argument of each function because `#[contractclient]`
/// interface traits take it explicitly. The generated client hides it, so calls
/// read as `client.submit(&from, &spender, &to, &requests)`.
#[contractclient(name = "BlendPoolClient")]
pub trait BlendPool {
    /// Supplies or withdraws in one call, and returns the resulting position of
    /// `to`.
    ///
    /// Authorisation, as implemented upstream: `spender` always authorises, and
    /// `from` also authorises when `from != spender`. `to` is not required to
    /// authorise. Nobody may be the pool's own address.
    ///
    /// Assets move on supply as `transfer(spender -> pool)` and on withdrawal
    /// as `transfer(pool -> to)`.
    fn submit(
        env: Env,
        from: Address,
        spender: Address,
        to: Address,
        requests: Vec<Request>,
    ) -> Positions;

    /// `account`'s position, or a default empty one if its entry has expired.
    ///
    /// The read itself refreshes the entry's TTL, which is why a keeper needs no
    /// contract change to do that job.
    fn get_positions(env: Env, account: Address) -> Positions;

    /// The reserve for `asset`. Errors if the pool lists no such reserve.
    fn get_reserve(env: Env, asset: Address) -> Reserve;
}
