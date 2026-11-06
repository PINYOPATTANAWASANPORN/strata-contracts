//! Hostile and broken vault tests against [`EpochManager`].
//!
//! Addresses Issue #8: `test(security): analyse hostile-vault reentrancy and
//! add a malicious-vault test`.
//!
//! # Re-entrancy model in Soroban
//!
//! The Soroban host runtime forbids contract re-entrancy at the VM boundary:
//! any cross-contract call that attempts to invoke a contract already present
//! on the active call stack traps in the host before the callee's function body
//! executes. See the official Stellar Soroban security documentation:
//! <https://developers.stellar.org/docs/build/security-docs>
//!
//! These tests drive a malicious [` VaultInterface`] implementation
//! ([`HostileVault`]) through every callback vector and faulty accounting mode:
//!
//! | Scenario | Callback / Fault | Observed outcome |
//! | --- | --- | --- |
//! | `deposit` -> `deposit` | Re-enter `manager.deposit` inside `vault.deposit` | Rejected by Soroban host (re-entrancy trap); tx rolls back |
//! | `deposit` -> `settle` | Re-enter `manager.settle` inside `vault.deposit` | Rejected by Soroban host (re-entrancy trap); tx rolls back |
//! | `deposit` -> `claim` | Re-enter `manager.claim` inside `vault.deposit` | Rejected by Soroban host (re-entrancy trap); tx rolls back |
//! | `redeem` -> `deposit` | Re-enter `manager.deposit` inside `vault.redeem` | Rejected by Soroban host (re-entrancy trap); epoch stays `Open` |
//! | `redeem` -> `settle` | Re-enter `manager.settle` inside `vault.redeem` | Rejected by Soroban host (re-entrancy trap); epoch stays `Open` |
//! | `redeem` -> `claim` | Re-enter `manager.claim` inside `vault.redeem` | Rejected by Soroban host (re-entrancy trap); epoch stays `Open` |
//! | Inflated/deflated valuation | `total_assets` / `convert_to_assets` lie (100x or 0) | Only view projections move; `settle` uses real `redeem` output |
//! | Inflated `redeem` return | `redeem` reports 2x value without transferring tokens | Rejected by manager (`Error::InvalidAmount`, `held < owed`) |
//! | Underpaid `redeem` transfer | `redeem` transfers 0.5x tokens but reports 1x value | Rejected by manager (`Error::InvalidAmount`, `held < owed`) |
//! | Dishonest partial `redeem` | `redeem` transfers and reports 0.5x (withholds 0.5x) | Indistinguishable from strategy loss (R6); 0.5x stuck in vault |
//! | Vault traps on `deposit` | `vault.deposit` panics | Tx rolls back; depositor keeps full token balance |
//! | Vault traps on `redeem` | `vault.redeem` panics | `settle` rolls back; epoch stays `Open` and can retry (R8) |

use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};
use strata_epoch_manager::{EpochManager, EpochManagerClient, Error, Status, Tranche};
use strata_vault_interface::VaultInterface;
use strata_waterfall::SECONDS_PER_YEAR;

const TERM: u64 = SECONDS_PER_YEAR;
const RATE: u32 = 500; // 5%/yr senior target
const RATIO: u32 = 10_000; // 1.0x senior cap
const STARTING_BALANCE: i128 = 1_000_000_000;
const DEPOSIT_AMOUNT: i128 = 100_000_000;

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostileMode {
    Normal,
    ReenterDepositOnDeposit,
    ReenterSettleOnDeposit,
    ReenterClaimOnDeposit,
    ReenterDepositOnRedeem,
    ReenterSettleOnRedeem,
    ReenterClaimOnRedeem,
    InflateValuation,
    DeflateValuation,
    InflateRedeemReturn,
    UnderpayRedeemTransfer,
    DishonestPartialRedeem,
    TrapOnDeposit,
    TrapOnRedeem,
}

#[contracttype]
#[derive(Clone)]
pub enum HostileDataKey {
    Asset,
    Manager,
    TargetUser,
    Mode,
    Shares(Address),
    TotalAssets,
    TotalShares,
}

#[contract]
pub struct HostileVault;

#[contractimpl]
impl HostileVault {
    pub fn __constructor(env: Env, asset: Address, manager: Address, target_user: Address) {
        env.storage()
            .persistent()
            .set(&HostileDataKey::Asset, &asset);
        env.storage()
            .persistent()
            .set(&HostileDataKey::Manager, &manager);
        env.storage()
            .persistent()
            .set(&HostileDataKey::TargetUser, &target_user);
        env.storage()
            .persistent()
            .set(&HostileDataKey::Mode, &HostileMode::Normal);
        env.storage()
            .persistent()
            .set(&HostileDataKey::TotalAssets, &0i128);
        env.storage()
            .persistent()
            .set(&HostileDataKey::TotalShares, &0i128);
    }

    pub fn set_mode(env: Env, mode: HostileMode) {
        env.storage().persistent().set(&HostileDataKey::Mode, &mode);
    }
}

#[contractimpl]
impl VaultInterface for HostileVault {
    fn asset(env: Env) -> Address {
        env.storage()
            .persistent()
            .get(&HostileDataKey::Asset)
            .unwrap()
    }

    fn deposit(env: Env, assets: i128, receiver: Address) -> i128 {
        let mode: HostileMode = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Mode)
            .unwrap_or(HostileMode::Normal);
        let asset: Address = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Asset)
            .unwrap();
        let manager: Address = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Manager)
            .unwrap();
        let user: Address = env
            .storage()
            .persistent()
            .get(&HostileDataKey::TargetUser)
            .unwrap();

        match mode {
            HostileMode::TrapOnDeposit => {
                panic!("HostileVault: intentional trap on deposit");
            }
            HostileMode::ReenterDepositOnDeposit => {
                EpochManagerClient::new(&env, &manager).deposit(&user, &Tranche::Junior, &1_000);
            }
            HostileMode::ReenterSettleOnDeposit => {
                EpochManagerClient::new(&env, &manager).settle();
            }
            HostileMode::ReenterClaimOnDeposit => {
                EpochManagerClient::new(&env, &manager).claim(&user, &Tranche::Junior);
            }
            _ => {}
        }

        let me = env.current_contract_address();
        TokenClient::new(&env, &asset).transfer(&receiver, &me, &assets);

        let prev_shares: i128 = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Shares(receiver.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&HostileDataKey::Shares(receiver), &(prev_shares + assets));

        let tot_assets: i128 = env
            .storage()
            .persistent()
            .get(&HostileDataKey::TotalAssets)
            .unwrap_or(0);
        let tot_shares: i128 = env
            .storage()
            .persistent()
            .get(&HostileDataKey::TotalShares)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&HostileDataKey::TotalAssets, &(tot_assets + assets));
        env.storage()
            .persistent()
            .set(&HostileDataKey::TotalShares, &(tot_shares + assets));

        assets
    }

    fn redeem(env: Env, shares: i128, receiver: Address, owner: Address) -> i128 {
        let mode: HostileMode = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Mode)
            .unwrap_or(HostileMode::Normal);
        let asset: Address = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Asset)
            .unwrap();
        let manager: Address = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Manager)
            .unwrap();
        let user: Address = env
            .storage()
            .persistent()
            .get(&HostileDataKey::TargetUser)
            .unwrap();

        let held_shares: i128 = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Shares(owner.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&HostileDataKey::Shares(owner), &(held_shares - shares));

        let me = env.current_contract_address();
        let token = TokenClient::new(&env, &asset);

        match mode {
            HostileMode::TrapOnRedeem => {
                panic!("HostileVault: intentional trap on redeem");
            }
            HostileMode::ReenterDepositOnRedeem => {
                EpochManagerClient::new(&env, &manager).deposit(&user, &Tranche::Junior, &1_000);
                token.transfer(&me, &receiver, &shares);
                shares
            }
            HostileMode::ReenterSettleOnRedeem => {
                EpochManagerClient::new(&env, &manager).settle();
                token.transfer(&me, &receiver, &shares);
                shares
            }
            HostileMode::ReenterClaimOnRedeem => {
                EpochManagerClient::new(&env, &manager).claim(&user, &Tranche::Junior);
                token.transfer(&me, &receiver, &shares);
                shares
            }
            HostileMode::InflateRedeemReturn => {
                // Transfers 1x shares in tokens, but claims 2x was redeemed.
                token.transfer(&me, &receiver, &shares);
                shares * 2
            }
            HostileMode::UnderpayRedeemTransfer => {
                // Transfers only half the tokens, but claims full shares were redeemed.
                let half = shares / 2;
                token.transfer(&me, &receiver, &half);
                shares
            }
            HostileMode::DishonestPartialRedeem => {
                // Withholds half the pool inside the vault and reports the smaller figure.
                let half = shares / 2;
                token.transfer(&me, &receiver, &half);
                half
            }
            _ => {
                token.transfer(&me, &receiver, &shares);
                shares
            }
        }
    }

    fn total_assets(env: Env) -> i128 {
        let mode: HostileMode = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Mode)
            .unwrap_or(HostileMode::Normal);
        let actual: i128 = env
            .storage()
            .persistent()
            .get(&HostileDataKey::TotalAssets)
            .unwrap_or(0);
        match mode {
            HostileMode::InflateValuation => actual.saturating_mul(100),
            HostileMode::DeflateValuation => 0,
            _ => actual,
        }
    }

    fn convert_to_assets(env: Env, shares: i128) -> i128 {
        let mode: HostileMode = env
            .storage()
            .persistent()
            .get(&HostileDataKey::Mode)
            .unwrap_or(HostileMode::Normal);
        match mode {
            HostileMode::InflateValuation => shares.saturating_mul(100),
            HostileMode::DeflateValuation => 0,
            _ => shares,
        }
    }

    fn balance_of(env: Env, account: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&HostileDataKey::Shares(account))
            .unwrap_or(0)
    }
}

struct Fixture {
    env: Env,
    manager: Address,
    vault: Address,
    asset: Address,
    senior: Address,
    junior: Address,
}

fn fixture() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let senior = Address::generate(&env);
    let junior = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let manager_id = Address::generate(&env);
    let vault_id = Address::generate(&env);

    let vault = env.register_at(&vault_id, HostileVault, (&asset, &manager_id, &junior));
    let manager = env.register_at(&manager_id, EpochManager, (&admin, &vault));

    for who in [&senior, &junior] {
        StellarAssetClient::new(&env, &asset).mint(who, &STARTING_BALANCE);
    }

    EpochManagerClient::new(&env, &manager).create_epoch(&TERM, &RATE, &RATIO);

    Fixture {
        env,
        manager,
        vault,
        asset,
        senior,
        junior,
    }
}

impl Fixture {
    fn mgr(&self) -> EpochManagerClient<'_> {
        EpochManagerClient::new(&self.env, &self.manager)
    }

    fn hostile(&self) -> HostileVaultClient<'_> {
        HostileVaultClient::new(&self.env, &self.vault)
    }

    fn token(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.asset)
    }

    fn seed_both_tranches(&self) {
        self.mgr()
            .deposit(&self.junior, &Tranche::Junior, &DEPOSIT_AMOUNT);
        self.mgr()
            .deposit(&self.senior, &Tranche::Senior, &DEPOSIT_AMOUNT);
    }

    fn advance_past_maturity(&self) {
        let now = self.env.ledger().timestamp() + TERM;
        self.env.ledger().set_timestamp(now);
    }
}

// --- re-entrancy during vault.deposit ---------------------------------------

#[test]
fn reentering_manager_from_vault_deposit_is_rejected_by_the_host() {
    for mode in [
        HostileMode::ReenterDepositOnDeposit,
        HostileMode::ReenterSettleOnDeposit,
        HostileMode::ReenterClaimOnDeposit,
    ] {
        let f = fixture();
        f.hostile().set_mode(&mode);

        // The Soroban host aborts any cross-contract call that re-enters a
        // contract already on the call stack (`Err(Err(InvokeError::Abort))`).
        let res = f
            .mgr()
            .try_deposit(&f.junior, &Tranche::Junior, &DEPOSIT_AMOUNT);
        assert!(
            matches!(res, Err(Err(_))),
            "re-entry mode {mode:?} during deposit must be rejected by the Soroban host"
        );

        // Full rollback: the depositor kept their tokens and the epoch is empty.
        assert_eq!(f.token().balance(&f.junior), STARTING_BALANCE);
        let epoch = f.mgr().current_epoch().unwrap();
        assert_eq!(epoch.junior_total, 0);
        assert_eq!(epoch.vault_shares, 0);
    }
}

// --- re-entrancy during vault.redeem ----------------------------------------

#[test]
fn reentering_manager_from_vault_redeem_is_rejected_by_the_host() {
    for mode in [
        HostileMode::ReenterDepositOnRedeem,
        HostileMode::ReenterSettleOnRedeem,
        HostileMode::ReenterClaimOnRedeem,
    ] {
        let f = fixture();
        f.seed_both_tranches();
        f.advance_past_maturity();

        f.hostile().set_mode(&mode);

        let res = f.mgr().try_settle();
        assert!(
            matches!(res, Err(Err(_))),
            "re-entry mode {mode:?} during redeem must be rejected by the Soroban host"
        );

        // Full rollback: the epoch remains Open and vault shares are untouched.
        let epoch = f.mgr().current_epoch().unwrap();
        assert_eq!(epoch.status, Status::Open);
        assert_eq!(epoch.vault_shares, DEPOSIT_AMOUNT * 2);
        assert_eq!(f.token().balance(&f.vault), DEPOSIT_AMOUNT * 2);
    }
}

// --- inflated / deflated valuation views ------------------------------------

#[test]
fn inflated_or_deflated_total_assets_and_convert_to_assets_cannot_corrupt_settlement() {
    for mode in [HostileMode::InflateValuation, HostileMode::DeflateValuation] {
        let f = fixture();
        f.seed_both_tranches();
        f.hostile().set_mode(&mode);

        // Pre-settlement view `position_of` consults `convert_to_assets`, so a
        // lying vault can distort the UI estimate while the epoch is Open.
        let est = f
            .mgr()
            .position_of(&f.junior, &Tranche::Junior)
            .estimated_payout;
        match mode {
            HostileMode::InflateValuation => assert!(est > DEPOSIT_AMOUNT),
            HostileMode::DeflateValuation => assert_eq!(est, 0),
            _ => unreachable!(),
        }

        // `settle` ignores `total_assets` and `convert_to_assets` and uses the
        // actual redemption return verified against the manager's token balance.
        f.advance_past_maturity();
        let split = f.mgr().settle();
        assert_eq!(split.senior_payout, 105_000_000);
        assert_eq!(split.junior_payout, 95_000_000);

        assert_eq!(f.mgr().claim(&f.senior, &Tranche::Senior), 105_000_000);
        assert_eq!(f.mgr().claim(&f.junior, &Tranche::Junior), 95_000_000);
    }
}

// --- inflated or deflated redeem results ------------------------------------

#[test]
fn inflated_redeem_return_or_underpaid_transfer_is_blocked_by_balance_check() {
    for mode in [
        HostileMode::InflateRedeemReturn,
        HostileMode::UnderpayRedeemTransfer,
    ] {
        let f = fixture();
        f.seed_both_tranches();
        f.advance_past_maturity();
        f.hostile().set_mode(&mode);

        // Both lies make `owed > held`, which `settle` rejects with
        // `Error::InvalidAmount`, rolling back the entire transaction.
        let res = f.mgr().try_settle();
        assert_eq!(res, Err(Ok(Error::InvalidAmount)));

        let epoch = f.mgr().current_epoch().unwrap();
        assert_eq!(epoch.status, Status::Open);
        assert_eq!(f.token().balance(&f.vault), DEPOSIT_AMOUNT * 2);
    }
}

#[test]
fn dishonest_partial_redeem_is_treated_as_a_loss_and_leaves_withheld_funds_in_vault() {
    let f = fixture();
    f.seed_both_tranches();
    f.advance_past_maturity();

    // The vault holds 200_000_000 but transfers and reports only 100_000_000.
    // Because `held (100_000_000) == owed (100_000_000)`, the manager cannot
    // distinguish this from a real 50% strategy loss (Risk R6).
    f.hostile().set_mode(&HostileMode::DishonestPartialRedeem);
    let split = f.mgr().settle();

    assert_eq!(split.senior_due, 105_000_000);
    assert_eq!(split.senior_payout, 100_000_000);
    assert_eq!(split.junior_payout, 0);

    assert_eq!(f.mgr().claim(&f.senior, &Tranche::Senior), 100_000_000);
    assert_eq!(f.mgr().claim(&f.junior, &Tranche::Junior), 0);

    // The withheld 100_000_000 remains stranded inside the hostile vault.
    assert_eq!(f.token().balance(&f.vault), DEPOSIT_AMOUNT);
}

// --- vault that traps -------------------------------------------------------

#[test]
fn vault_trapping_on_deposit_or_redeem_rolls_back_and_allows_retry_when_fixed() {
    let f = fixture();

    // 1. Trap on deposit: depositor loses nothing.
    f.hostile().set_mode(&HostileMode::TrapOnDeposit);
    assert!(f
        .mgr()
        .try_deposit(&f.junior, &Tranche::Junior, &DEPOSIT_AMOUNT)
        .is_err());
    assert_eq!(f.token().balance(&f.junior), STARTING_BALANCE);

    // 2. Recover to Normal, deposit, advance to maturity, then trap on redeem.
    f.hostile().set_mode(&HostileMode::Normal);
    f.seed_both_tranches();
    f.advance_past_maturity();

    f.hostile().set_mode(&HostileMode::TrapOnRedeem);
    assert!(f.mgr().try_settle().is_err());
    assert_eq!(f.mgr().current_epoch().unwrap().status, Status::Open);

    // 3. Once the vault stops trapping, permissionless `settle` succeeds and
    // depositors recover their full payouts (Risk R8).
    f.hostile().set_mode(&HostileMode::Normal);
    f.mgr().settle();
    assert_eq!(f.mgr().claim(&f.senior, &Tranche::Senior), 105_000_000);
    assert_eq!(f.mgr().claim(&f.junior, &Tranche::Junior), 95_000_000);
}
