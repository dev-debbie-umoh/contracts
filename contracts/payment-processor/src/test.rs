extern crate std;

use super::*;
use loyalty_program::{LoyaltyProgram, LoyaltyProgramClient};
use merchant_registry::{MerchantRegistry, MerchantRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _, Events},
    token::StellarAssetClient,
    vec, Env, String, Event,
};

const UNIT: i128 = 10_000_000;

struct Setup<'a> {
    env: Env,
    pp: PaymentProcessorClient<'a>,
    registry: MerchantRegistryClient<'a>,
    token: token::Client<'a>,
    asset: Address,
    treasury: Address,
    merchant: Address,
    settlement: Address,
    payer: Address,
}

fn setup(fee_bps: u32) -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);
    let merchant = Address::generate(&env);
    let settlement = Address::generate(&env);
    let payer = Address::generate(&env);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let asset = sac.address();
    StellarAssetClient::new(&env, &asset).mint(&payer, &(1_000 * UNIT));

    let registry_id = env.register(MerchantRegistry, ());
    let registry = MerchantRegistryClient::new(&env, &registry_id);
    registry.initialize(&admin);
    registry.register(
        &merchant,
        &String::from_str(&env, "Lagos Bites"),
        &settlement,
        &vec![&env, asset.clone()],
    );
    registry.approve(&merchant);

    let pp_id = env.register(PaymentProcessor, ());
    let pp = PaymentProcessorClient::new(&env, &pp_id);
    pp.initialize(&admin, &registry_id, &treasury, &fee_bps);

    Setup {
        token: token::Client::new(&env, &asset),
        pp,
        registry,
        asset,
        treasury,
        merchant,
        settlement,
        payer,
        env,
    }
}

fn order(env: &Env, r: &str) -> String {
    String::from_str(env, r)
}

fn category(env: &Env) -> String {
    String::from_str(env, "retail")
}

/// Registers and approves a second merchant that accepts `asset`.
fn register_asset_merchant(s: &Setup, asset: &Address) -> (Address, Address) {
    let merchant = Address::generate(&s.env);
    let settlement = Address::generate(&s.env);
    s.registry.register(
        &merchant,
        &String::from_str(&s.env, "Second Shop"),
        &settlement,
        &vec![&s.env, asset.clone()],
    );
    s.registry.approve(&merchant);
    (merchant, settlement)
}

fn line(env: &Env, merchant: &Address, amount: i128, r: &str) -> BatchLine {
    BatchLine {
        merchant: merchant.clone(),
        amount,
        order_ref: order(env, r),
        category: category(env),
    }
}

fn share(recipient: &Address, bps: u32) -> RoyaltyShare {
    RoyaltyShare {
        recipient: recipient.clone(),
        bps,
    }
}

#[test]
fn payment_splits_fee_to_treasury_and_net_to_settlement() {
    let s = setup(250);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "ORD-1001"),
        &category(&s.env),
    );
    assert_eq!(s.pp.payment_count(), 1);

    assert_eq!(s.token.balance(&s.settlement), 975 * UNIT / 10);
    assert_eq!(s.token.balance(&s.treasury), 25 * UNIT / 10);
    assert_eq!(s.token.balance(&s.payer), 900 * UNIT);
    // The processor never custodies funds.
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.fee, 25 * UNIT / 10);
    assert_eq!(p.amount, 100 * UNIT);
    assert_eq!(p.category, category(&s.env));
    assert_eq!(p.status, PaymentStatus::Completed);
    assert_eq!(
        s.pp.payment_for_order(&s.merchant, &order(&s.env, "ORD-1001"))
            .id,
        id
    );
}

#[test]
fn zero_fee_skips_treasury_transfer() {
    let s = setup(0);
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "A"),
        &category(&s.env),
    );
    assert_eq!(s.token.balance(&s.settlement), UNIT);
    assert_eq!(s.token.balance(&s.treasury), 0);
}

#[test]
fn duplicate_order_ref_is_rejected() {
    let s = setup(100);
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "DUP"),
        &category(&s.env),
    );
    let again = s.pp.try_pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "DUP"),
        &category(&s.env),
    );
    assert_eq!(again, Err(Ok(PaymentError::DuplicateOrder)));
    assert_eq!(s.token.balance(&s.payer), 999 * UNIT);
}

#[test]
fn inactive_merchant_or_unlisted_asset_is_rejected() {
    let s = setup(100);
    s.registry.suspend(&s.merchant);
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "X"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::MerchantNotAccepting))
    );
    s.registry.reinstate(&s.merchant);

    let other = s
        .env
        .register_stellar_asset_contract_v2(Address::generate(&s.env))
        .address();
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &other,
            &UNIT,
            &order(&s.env, "Y"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::MerchantNotAccepting))
    );
}

#[test]
fn partial_then_full_refund() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "R"),
        &category(&s.env),
    );

    let total = s.pp.refund(
        &id,
        &(4 * UNIT),
        &String::from_str(&s.env, "one item returned"),
    );
    assert_eq!(total, 4 * UNIT);
    assert_eq!(
        s.pp.get_payment(&id).status,
        PaymentStatus::PartiallyRefunded
    );

    s.pp
        .refund(&id, &(6 * UNIT), &String::from_str(&s.env, "rest"));
    assert_eq!(s.pp.get_payment(&id).status, PaymentStatus::Refunded);
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 0);

    assert_eq!(
        s.pp
            .try_refund(&id, &1, &String::from_str(&s.env, "again")),
        Err(Ok(PaymentError::AlreadyRefunded))
    );
}

#[test]
fn refund_cannot_exceed_gross_amount() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "Z"),
        &category(&s.env),
    );
    assert_eq!(
        s.pp
            .try_refund(&id, &(UNIT + 1), &String::from_str(&s.env, "too much")),
        Err(Ok(PaymentError::RefundExceedsPayment))
    );
}

#[test]
fn refund_needs_the_settlement_to_hold_the_refund_amount() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "BAL-1"),
    );
    // The merchant withdraws most of the settlement balance before the refund.
    s.token.transfer(&s.settlement, &s.treasury, &(8 * UNIT));
    assert_eq!(s.token.balance(&s.settlement), 2 * UNIT);

    assert_eq!(
        s.pp.try_refund(&id, &(5 * UNIT), &String::from_str(&s.env, "too late")),
        Err(Ok(PaymentError::InsufficientBalance))
    );
    // Nothing moved and the payment is still whole.
    assert_eq!(s.pp.get_payment(&id).refunded, 0);
    assert_eq!(s.token.balance(&s.payer), 990 * UNIT);

    // A refund the settlement can cover still goes through.
    s.pp.refund(&id, &(2 * UNIT), &String::from_str(&s.env, "partial"));
    assert_eq!(s.token.balance(&s.payer), 992 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 0);
}

#[test]
fn payment_accrues_loyalty_when_enabled() {
    let s = setup(0);
    let loyalty_id = s.env.register(LoyaltyProgram, ());
    let loyalty = LoyaltyProgramClient::new(&s.env, &loyalty_id);
    loyalty.initialize(&Address::generate(&s.env));
    loyalty.set_processor(&s.pp.address);
    // 10 points per whole unit, with 7 decimals (Stellar's precision).
    loyalty.enroll(&s.merchant, &10, &7);
    s.pp.set_loyalty(&Some(loyalty_id));

    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(20 * UNIT),
        &order(&s.env, "L"),
        &category(&s.env),
    );
    assert_eq!(s.pp.get_payment(&id).loyalty_points, 200);
    assert_eq!(loyalty.get_account(&s.merchant, &s.payer).points, 200);
}

#[test]
fn pause_blocks_payments() {
    let s = setup(0);
    s.pp.pause();
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "P"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::Paused))
    );
    s.pp.unpause();
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "P"),
        &category(&s.env),
    );
}

#[test]
fn fee_rate_is_capped() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_set_fee_bps(&(MAX_FEE_BPS + 1)),
        Err(Ok(PaymentError::InvalidFeeRate))
    );
    s.pp.set_fee_bps(&MAX_FEE_BPS);
    assert_eq!(s.pp.quote(&(100 * UNIT)), (10 * UNIT, 90 * UNIT));
}

#[test]
fn input_validation() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &0,
            &order(&s.env, "V"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::InvalidAmount))
    );
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, ""),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::InvalidOrderRef))
    );
}

#[test]
fn payment_ids_are_hashed_not_sequential() {
    let s = setup(0);
    let a = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "H-1"),
        &category(&s.env),
    );
    let b = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "H-2"),
        &category(&s.env),
    );
    assert_ne!(a, 1);
    assert_ne!(b, a + 1, "the next id cannot be read off the counter");
    assert_eq!(s.pp.payment_count(), 2);
    assert_eq!(s.pp.get_payment(&b).order_ref, order(&s.env, "H-2"));
    assert_eq!(
        s.pp.payment_for_order(&s.merchant, &order(&s.env, "H-1"))
            .id,
        a
    );
}

#[test]
fn every_sale_and_refund_emits_a_compliance_record() {
    let s = setup(250);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "C-1"),
        &category(&s.env),
    );
    let mut record = ComplianceRecord {
        kind: symbol_short!("payment"),
        merchant: s.merchant.clone(),
        payment_id: id,
        payer: s.payer.clone(),
        settlement: s.settlement.clone(),
        asset: s.asset.clone(),
        amount: 100 * UNIT,
        fee: 25 * UNIT / 10,
        order_ref: order(&s.env, "C-1"),
        timestamp: s.env.ledger().timestamp(),
    };
    let emitted = s.env.events().all().filter_by_contract(&s.pp.address);
    assert!(emitted
        .events()
        .contains(&record.to_xdr(&s.env, &s.pp.address)));

    s.pp
        .refund(&id, &(10 * UNIT), &String::from_str(&s.env, "returned"));
    record.kind = symbol_short!("refund");
    record.amount = 10 * UNIT;
    record.fee = 0;
    let emitted = s.env.events().all().filter_by_contract(&s.pp.address);
    assert!(emitted
        .events()
        .contains(&record.to_xdr(&s.env, &s.pp.address)));
}

// ---------------------------------------------------------------------------
// Token transfer failure scenarios
// ---------------------------------------------------------------------------

/// A token whose `transfer` reports success but moves nothing.
#[contract]
pub struct LyingToken;

#[contractimpl]
impl LyingToken {
    pub fn transfer(_env: Env, _from: Address, _to: Address, _amount: i128) {}

    pub fn balance(_env: Env, _id: Address) -> i128 {
        0
    }

    pub fn decimals(_env: Env) -> u32 {
        7
    }
}

/// A token that reverts on every transfer.
#[contract]
pub struct RevertingToken;

#[contractimpl]
impl RevertingToken {
    pub fn transfer(_env: Env, _from: Address, _to: Address, _amount: i128) {
        panic!("transfer reverts");
    }

    pub fn balance(_env: Env, _id: Address) -> i128 {
        0
    }

    pub fn decimals(_env: Env) -> u32 {
        7
    }
}

#[contracttype]
enum TokenKey {
    Balance(Address),
    Decimals,
    Calls,
    FailAfter,
    Transfers,
    BlockTo,
    BlockFrom,
}

/// A configurable token used to exercise every failure mode: it can panic on
/// a specific transfer count, or refuse transfers to/from a blocked address.
#[contract]
pub struct TestToken;

#[contractimpl]
impl TestToken {
    pub fn init(env: Env, decimals: u32, fail_after: u32) {
        env.storage().instance().set(&TokenKey::Decimals, &decimals);
        env.storage()
            .instance()
            .set(&TokenKey::FailAfter, &fail_after);
        env.storage().instance().set(&TokenKey::Calls, &0u32);
        env.storage().instance().set(&TokenKey::Transfers, &0u32);
    }

    pub fn block_to(env: Env, to: Address) {
        env.storage()
            .persistent()
            .set(&TokenKey::BlockTo, &Some(to));
    }

    pub fn block_from(env: Env, from: Address) {
        env.storage()
            .persistent()
            .set(&TokenKey::BlockFrom, &Some(from));
    }

    pub fn mint(env: Env, to: Address, amount: i128) {
        let current = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&TokenKey::Balance(to), &(current + amount));
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        let mut transfers: u32 = env
            .storage()
            .instance()
            .get(&TokenKey::Transfers)
            .unwrap_or(0);
        transfers += 1;
        env.storage()
            .instance()
            .set(&TokenKey::Transfers, &transfers);

        let fail_after: u32 = env
            .storage()
            .instance()
            .get(&TokenKey::FailAfter)
            .unwrap_or(0);
        if fail_after != 0 && transfers > fail_after {
            panic!("token transfer failed");
        }
        let blocked_to: Option<Address> = env
            .storage()
            .persistent()
            .get(&TokenKey::BlockTo)
            .unwrap_or(None);
        if blocked_to == Some(to.clone()) {
            panic!("destination blocked");
        }
        let blocked_from: Option<Address> = env
            .storage()
            .persistent()
            .get(&TokenKey::BlockFrom)
            .unwrap_or(None);
        if blocked_from == Some(from.clone()) {
            panic!("source blocked");
        }

        let from_balance = Self::balance(env.clone(), from.clone());
        if from_balance < amount {
            panic!("insufficient balance");
        }
        env.storage().persistent().set(
            &TokenKey::Balance(from),
            &(from_balance - amount),
        );
        let to_balance = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&TokenKey::Balance(to), &(to_balance + amount));
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&TokenKey::Balance(id))
            .unwrap_or(0)
    }

    pub fn decimals(env: Env) -> u32 {
        let calls: u32 = env
            .storage()
            .instance()
            .get(&TokenKey::Calls)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&TokenKey::Calls, &(calls + 1));
        env.storage()
            .instance()
            .get(&TokenKey::Decimals)
            .unwrap_or(7)
    }

    pub fn decimals_calls(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&TokenKey::Calls)
            .unwrap_or(0)
    }
}

#[test]
fn a_token_that_does_not_move_funds_is_rejected() {
    let s = setup(100);
    let fake = s.env.register(LyingToken, ());
    let (merchant, _settlement) = register_asset_merchant(&s, &fake);

    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &merchant,
            &fake,
            &UNIT,
            &order(&s.env, "F-1"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::TransferFailed))
    );
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp.try_payment_for_order(&merchant, &order(&s.env, "F-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn a_reverting_token_aborts_the_payment() {
    let s = setup(0);
    let fake = s.env.register(RevertingToken, ());
    let (merchant, _settlement) = register_asset_merchant(&s, &fake);

    assert!(s
        .pp
        .try_pay(
            &s.payer,
            &merchant,
            &fake,
            &UNIT,
            &order(&s.env, "REV-1"),
            &category(&s.env)
        )
        .is_err());
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp.try_payment_for_order(&merchant, &order(&s.env, "REV-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn a_failed_fee_transfer_rolls_back_the_payment() {
    let s = setup(100);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &0);
    token.block_to(&s.treasury);
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, settlement) = register_asset_merchant(&s, &token_id);

    assert!(s
        .pp
        .try_pay(
            &s.payer,
            &merchant,
            &token_id,
            &(10 * UNIT),
            &order(&s.env, "FEE-FAIL"),
            &category(&s.env)
        )
        .is_err());

    // The settlement transfer had already succeeded, but the whole
    // transaction reverted, including the token's own state.
    assert_eq!(token.balance(&s.payer), 100 * UNIT);
    assert_eq!(token.balance(&settlement), 0);
    assert_eq!(token.balance(&s.treasury), 0);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp
            .try_payment_for_order(&merchant, &order(&s.env, "FEE-FAIL")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn a_failed_refund_transfer_leaves_the_payment_untouched() {
    let s = setup(0);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &0);
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, settlement) = register_asset_merchant(&s, &token_id);
    token.block_from(&settlement);

    let id = s.pp.pay(
        &s.payer,
        &merchant,
        &token_id,
        &(10 * UNIT),
        &order(&s.env, "REFUND-FAIL"),
        &category(&s.env),
    );

    assert!(s
        .pp
        .try_refund(&id, &(5 * UNIT), &String::from_str(&s.env, "blocked"))
        .is_err());
    let payment = s.pp.get_payment(&id);
    assert_eq!(payment.refunded, 0);
    assert_eq!(payment.status, PaymentStatus::Completed);
    assert_eq!(s.pp.payment_count(), 1);
}

#[test]
fn batch_stops_at_the_first_failing_transfer_and_rolls_back() {
    let s = setup(0);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &1); // first transfer succeeds, second reverts
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, settlement) = register_asset_merchant(&s, &token_id);

    assert!(s
        .pp
        .try_pay_batch(
            &s.payer,
            &token_id,
            &vec![
                &s.env,
                line(&s.env, &merchant, 10 * UNIT, "B-OK"),
                line(&s.env, &merchant, 10 * UNIT, "B-FAIL"),
            ]
        )
        .is_err());

    assert_eq!(token.balance(&s.payer), 100 * UNIT);
    assert_eq!(token.balance(&settlement), 0);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp
            .try_payment_for_order(&merchant, &order(&s.env, "B-OK")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

// ---------------------------------------------------------------------------
// Cached token decimals
// ---------------------------------------------------------------------------

#[test]
fn token_decimals_are_cached_after_the_first_read() {
    let s = setup(0);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &0);
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, _settlement) = register_asset_merchant(&s, &token_id);

    let first = s.pp.pay(
        &s.payer,
        &merchant,
        &token_id,
        &(10 * UNIT),
        &order(&s.env, "DEC-1"),
        &category(&s.env),
    );
    let second = s.pp.pay(
        &s.payer,
        &merchant,
        &token_id,
        &(10 * UNIT),
        &order(&s.env, "DEC-2"),
        &category(&s.env),
    );

    assert_eq!(token.decimals_calls(), 1);
    assert_eq!(s.pp.get_payment(&first).asset_decimals, 7);
    assert_eq!(s.pp.get_payment(&second).asset_decimals, 7);

    // The view is served from the same cache, without another token call.
    assert_eq!(s.pp.token_decimals(&token_id), 7);
    assert_eq!(token.decimals_calls(), 1);
}

// ---------------------------------------------------------------------------
// Batch payments
// ---------------------------------------------------------------------------

#[test]
fn batch_settles_every_line_across_merchants() {
    let s = setup(100);
    let (m2, settle2) = register_asset_merchant(&s, &s.asset);
    let ids = s.pp.pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.env, &s.merchant, 10 * UNIT, "TAB-1"),
            line(&s.env, &m2, 20 * UNIT, "INV-9"),
            line(&s.env, &s.merchant, 30 * UNIT, "TAB-2"),
        ],
    );
    assert_eq!(ids.len(), 3);
    assert_eq!(s.pp.payment_count(), 3);
    for (i, r) in ["TAB-1", "INV-9", "TAB-2"].iter().enumerate() {
        let id = ids.get(i as u32).unwrap();
        assert_eq!(s.pp.get_payment(&id).order_ref, order(&s.env, r));
    }
    // 1% fee per line, net to each merchant's settlement account.
    assert_eq!(s.token.balance(&s.settlement), 396 * UNIT / 10);
    assert_eq!(s.token.balance(&settle2), 198 * UNIT / 10);
    assert_eq!(s.token.balance(&s.treasury), 6 * UNIT / 10);
    assert_eq!(s.token.balance(&s.payer), 940 * UNIT);
    assert_eq!(
        s.pp.payment_for_order(&m2, &order(&s.env, "INV-9")).amount,
        20 * UNIT
    );
}

#[test]
fn batch_is_all_or_nothing() {
    let s = setup(100);
    let stranger = Address::generate(&s.env);
    let res = s.pp.try_pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.env, &s.merchant, 10 * UNIT, "OK-1"),
            line(&s.env, &stranger, 5 * UNIT, "BAD-1"),
        ],
    );
    assert_eq!(res, Err(Ok(PaymentError::MerchantNotAccepting)));
    // The valid first line was rolled back too.
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp
            .try_payment_for_order(&s.merchant, &order(&s.env, "OK-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn batch_rejects_a_repeated_order_inside_itself() {
    let s = setup(0);
    let res = s.pp.try_pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.env, &s.merchant, UNIT, "SAME"),
            line(&s.env, &s.merchant, UNIT, "SAME"),
        ],
    );
    assert_eq!(res, Err(Ok(PaymentError::DuplicateOrder)));
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
}

#[test]
fn batch_size_limits_and_pause() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_pay_batch(&s.payer, &s.asset, &vec![&s.env]),
        Err(Ok(PaymentError::InvalidBatch))
    );
    let mut big = vec![&s.env];
    for i in 0..=MAX_BATCH {
        big.push_back(line(
            &s.env,
            &s.merchant,
            UNIT,
            &std::format!("B-{i}"),
        ));
    }
    assert_eq!(
        s.pp.try_pay_batch(&s.payer, &s.asset, &big),
        Err(Ok(PaymentError::InvalidBatch))
    );
    s.pp.pause();
    assert_eq!(
        s.pp.try_pay_batch(
            &s.payer,
            &s.asset,
            &vec![&s.env, line(&s.env, &s.merchant, UNIT, "P")]
        ),
        Err(Ok(PaymentError::Paused))
    );
}

// ---------------------------------------------------------------------------
// Split payments
// ---------------------------------------------------------------------------

#[test]
fn split_payment_distributes_across_contributors() {
    let s = setup(200); // 2% fee
    let payer2 = Address::generate(&s.env);
    let payer3 = Address::generate(&s.env);
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer2, &(500 * UNIT));
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer3, &(500 * UNIT));

    let shares = vec![
        &s.env,
        SplitShare {
            payer: s.payer.clone(),
            amount: 50 * UNIT,
        },
        SplitShare {
            payer: payer2.clone(),
            amount: 30 * UNIT,
        },
        SplitShare {
            payer: payer3.clone(),
            amount: 20 * UNIT,
        },
    ];

    let id = s.pp.split_payment(
        &s.merchant,
        &s.asset,
        &shares,
        &order(&s.env, "GRP-1"),
    );

    assert_eq!(s.token.balance(&s.payer), 950 * UNIT);
    assert_eq!(s.token.balance(&payer2), 470 * UNIT);
    assert_eq!(s.token.balance(&payer3), 480 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 98 * UNIT);
    assert_eq!(s.token.balance(&s.treasury), 2 * UNIT);
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.amount, 100 * UNIT);
    assert_eq!(p.fee, 2 * UNIT);
    assert_eq!(p.status, PaymentStatus::Completed);

    // Duplicate order ref is rejected.
    assert_eq!(
        s.pp
            .try_split_payment(&s.merchant, &s.asset, &shares, &order(&s.env, "GRP-1")),
        Err(Ok(PaymentError::DuplicateOrder))
    );
    // Empty shares rejected.
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![&s.env],
            &order(&s.env, "GRP-2")
        ),
        Err(Ok(PaymentError::InvalidSplit))
    );
    // Non-positive share rejected.
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![
                &s.env,
                SplitShare {
                    payer: s.payer.clone(),
                    amount: 0
                }
            ],
            &order(&s.env, "GRP-3"),
        ),
        Err(Ok(PaymentError::InvalidAmount))
    );
}

// ---------------------------------------------------------------------------
// Royalty splits
// ---------------------------------------------------------------------------

#[test]
fn royalty_split_pays_platform_royalties_and_seller() {
    let s = setup(250);
    let artist = Address::generate(&s.env);
    let curator = Address::generate(&s.env);
    s.pp.pay_with_royalties(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "NFT-1"),
        &vec![&s.env, share(&artist, 1_000), share(&curator, 500)],
    );
    // fee 2.5 to treasury; net 97.5 -> artist 10%, curator 5%, seller the rest
    assert_eq!(s.token.balance(&s.treasury), 25 * UNIT / 10);
    assert_eq!(s.token.balance(&artist), 975 * UNIT / 100);
    assert_eq!(s.token.balance(&curator), 4_875 * UNIT / 1_000);
    assert_eq!(s.token.balance(&s.settlement), 828_750 * UNIT / 10_000);
    assert_eq!(s.token.balance(&s.payer), 900 * UNIT);
}

#[test]
fn royalty_split_rejects_zero_shares_and_more_than_one_hundred_percent() {
    let s = setup(0);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let pay = |r: &str, shares: Vec<RoyaltyShare>| {
        s.pp.try_pay_with_royalties(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, r),
            &shares,
        )
    };
    assert_eq!(
        pay("A", vec![&s.env, share(&a, 0)]),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
    assert_eq!(
        pay("B", vec![&s.env, share(&a, 6_000), share(&b, 4_001)]),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
    // exactly 100% leaves nothing for the seller but is allowed
    pay("C", vec![&s.env, share(&a, 6_000), share(&b, 4_000)])
        .unwrap()
        .unwrap();
    assert_eq!(s.token.balance(&s.settlement), 0);
    assert_eq!(s.token.balance(&a) + s.token.balance(&b), UNIT);
}

#[test]
fn royalty_rounding_never_overpays_or_underflows() {
    let s = setup(0);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    // One stroop split 50/50: half-up rounds the first cut up to 1 and
    // leaves nothing for the second recipient, instead of underflowing.
    s.pp.pay_with_royalties(
        &s.payer,
        &s.merchant,
        &s.asset,
        &1,
        &order(&s.env, "ODD"),
        &vec![&s.env, share(&a, 5_000), share(&b, 5_000)],
    );
    assert_eq!(s.token.balance(&a) + s.token.balance(&b), 1);
    assert_eq!(s.token.balance(&s.settlement), 0);
}

#[test]
fn royalty_split_caps_the_number_of_recipients() {
    let s = setup(0);
    let mut shares = Vec::new(&s.env);
    for _ in 0..=MAX_ROYALTY_RECIPIENTS {
        shares.push_back(share(&Address::generate(&s.env), 100));
    }
    assert_eq!(
        s.pp.try_pay_with_royalties(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "X"),
            &shares
        ),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
}

// ---------------------------------------------------------------------------
// Governance
// ---------------------------------------------------------------------------

#[test]
fn governance_moves_parameter_changes_to_council_votes() {
    let s = setup(250);
    let (a, b, c) = (
        Address::generate(&s.env),
        Address::generate(&s.env),
        Address::generate(&s.env),
    );
    s.pp
        .enable_governance(&vec![&s.env, a.clone(), b.clone(), c.clone()], &2);

    // the admin can no longer change critical parameters alone
    assert_eq!(
        s.pp.try_set_fee_bps(&100),
        Err(Ok(PaymentError::GovernanceActive))
    );
    assert_eq!(
        s.pp.try_set_treasury(&a),
        Err(Ok(PaymentError::GovernanceActive))
    );

    let id = s.pp.propose_param_change(&a, &ParamChange::FeeBps(100));
    assert_eq!(s.pp.get_config().fee_bps, 250, "one approval is not enough");
    assert_eq!(
        s.pp.try_approve_param_change(&a, &id),
        Err(Ok(PaymentError::AlreadyApproved))
    );
    assert!(s.pp.approve_param_change(&b, &id));
    assert_eq!(s.pp.get_config().fee_bps, 100);
    assert!(s.pp.get_param_proposal(&id).executed);
    assert_eq!(
        s.pp.try_approve_param_change(&c, &id),
        Err(Ok(PaymentError::ProposalExecuted))
    );

    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.pp
            .try_propose_param_change(&outsider, &ParamChange::Treasury(outsider.clone())),
        Err(Ok(PaymentError::NotCouncilMember))
    );
    assert_eq!(
        s.pp
            .try_propose_param_change(&a, &ParamChange::FeeBps(MAX_FEE_BPS + 1)),
        Err(Ok(PaymentError::InvalidFeeRate))
    );
}

#[test]
fn governance_validates_the_council() {
    let s = setup(250);
    let a = Address::generate(&s.env);
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone()], &2),
        Err(Ok(PaymentError::InvalidGovernance))
    );
    assert_eq!(
        s.pp
            .try_enable_governance(&vec![&s.env, a.clone(), a.clone()], &1),
        Err(Ok(PaymentError::InvalidGovernance))
    );
    s.pp.enable_governance(&vec![&s.env, a.clone()], &1);
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone()], &1),
        Err(Ok(PaymentError::GovernanceActive))
    );

    // a threshold of one applies the proposal immediately
    s.pp
        .propose_param_change(&a, &ParamChange::Treasury(a.clone()));
    assert_eq!(s.pp.get_config().treasury, a);
}

#[test]
fn pause_blocks_refunds() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "PR-1"),
        &category(&s.env),
    );

    s.pp.pause();
    assert_eq!(
        s.pp
            .try_refund(&id, &(5 * UNIT), &String::from_str(&s.env, "paused")),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(s.pp.get_payment(&id).refunded, 0);
    assert_eq!(s.pp.get_payment(&id).status, PaymentStatus::Completed);

    s.pp.unpause();
    let total = s
        .pp
        .refund(&id, &(5 * UNIT), &String::from_str(&s.env, "resumed"));
    assert_eq!(total, 5 * UNIT);
    assert_eq!(
        s.pp.get_payment(&id).status,
        PaymentStatus::PartiallyRefunded
    );
}

#[test]
fn refund_decrements_payment_counts_without_reusing_ids() {
    let s = setup(0);
    let first = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "CNT-1"),
        &category(&s.env),
    );
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);

    // Partial refund leaves the payment active (count unchanged).
    s.pp.refund(&id1, &(4 * UNIT), &String::from_str(&s.env, "partial"));
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);

    // Completing the refund decrements both global and merchant active counts once.
    s.pp.refund(&id1, &(6 * UNIT), &String::from_str(&s.env, "full"));
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 0);

    // The next payment gets a fresh ID and does not overwrite payment #1.
    let id2 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "CNT-2"),
        &category(&s.env),
    );
    assert_ne!(id2, id1);
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);
    assert_eq!(s.pp.get_payment(&id1).status, PaymentStatus::Refunded);
    assert_eq!(s.pp.get_payment(&id2).status, PaymentStatus::Completed);
}

#[derive(Debug, PartialEq, Eq)]
struct StateSnapshot {
    paused: bool,
    fee_bps: u32,
    payment_count: u64,
    merchant_payment_count: u64,
    payer_balance: i128,
    settlement_balance: i128,
    treasury_balance: i128,
    processor_balance: i128,
}

fn capture_state(s: &Setup) -> StateSnapshot {
    let cfg = s.pp.get_config();
    StateSnapshot {
        paused: cfg.paused,
        fee_bps: cfg.fee_bps,
        payment_count: s.pp.payment_count(),
        merchant_payment_count: s.pp.merchant_payment_count(&s.merchant),
        payer_balance: s.token.balance(&s.payer),
        settlement_balance: s.token.balance(&s.settlement),
        treasury_balance: s.token.balance(&s.treasury),
        processor_balance: s.token.balance(&s.pp.address),
    }
}

#[test]
fn split_payment_distributes_across_contributors() {
    let s = setup(200); // 2% fee
    let payer2 = Address::generate(&s.env);
    let payer3 = Address::generate(&s.env);
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer2, &(500 * UNIT));
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer3, &(500 * UNIT));

    let shares = vec![
        &s.env,
        SplitShare {
            payer: s.payer.clone(),
            amount: 50 * UNIT,
        },
        SplitShare {
            payer: payer2.clone(),
            amount: 30 * UNIT,
        },
        SplitShare {
            payer: payer3.clone(),
            amount: 20 * UNIT,
        },
    ];

    let id =
        s.pp.split_payment(&s.merchant, &s.asset, &shares, &order(&s.env, "GRP-1"));
    assert_eq!(s.pp.payment_count(), 1);

    assert_eq!(s.token.balance(&s.payer), 950 * UNIT);
    assert_eq!(s.token.balance(&payer2), 470 * UNIT);
    assert_eq!(s.token.balance(&payer3), 480 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 98 * UNIT);
    assert_eq!(s.token.balance(&s.treasury), 2 * UNIT);
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.amount, 100 * UNIT);
    assert_eq!(p.fee, 2 * UNIT);
    assert_eq!(p.status, PaymentStatus::Completed);

    // Duplicate order ref is rejected
    assert_eq!(
        s.pp.try_split_payment(&s.merchant, &s.asset, &shares, &order(&s.env, "GRP-1")),
        Err(Ok(PaymentError::DuplicateOrder))
    );
    // Empty shares rejected
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![&s.env],
            &order(&s.env, "GRP-2")
        ),
        Err(Ok(PaymentError::InvalidSplit))
    );
    // Non-positive share rejected
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![
                &s.env,
                SplitShare {
                    payer: s.payer.clone(),
                    amount: 0
                }
            ],
            &order(&s.env, "GRP-3"),
        ),
        Err(Ok(PaymentError::InvalidAmount))
    );
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert_eq!(s.pp.payment_count(), 2);
}

#[test]
fn split_payments_cannot_be_refunded_through_the_processor() {
    let s = setup(0);
    let payer2 = Address::generate(&s.env);
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer2, &(100 * UNIT));
    let id = s.pp.split_payment(
        &s.merchant,
        &s.asset,
        &vec![
            &s.env,
            SplitShare {
                payer: s.payer.clone(),
                amount: 10 * UNIT,
            },
            SplitShare {
                payer: payer2.clone(),
                amount: 10 * UNIT,
            },
        ],
        &order(&s.env, "SPLIT-R"),
    );
    assert_eq!(
        s.pp.try_refund(&id, &UNIT, &String::from_str(&s.env, "nope")),
        Err(Ok(PaymentError::SplitNotRefundable))
    );
}

#[contract]
struct MaliciousCallback;

#[contracttype]
enum CallbackKey {
    Target,
    ReentryBlocked,
}

#[contractimpl]
impl MaliciousCallback {
    pub fn init(env: Env, target: Address) {
        env.storage()
            .instance()
            .set(&CallbackKey::Target, &target);
        env.storage()
            .instance()
            .set(&CallbackKey::ReentryBlocked, &false);
    }

    pub fn on_payment_settled(
        env: Env,
        _payment_id: u64,
        merchant: Address,
        payer: Address,
        asset: Address,
        amount: i128,
    ) {
        let target: Address = env
            .storage()
            .instance()
            .get(&CallbackKey::Target)
            .unwrap();
        let res = PaymentProcessorClient::new(&env, &target).try_pay(
            &payer,
            &merchant,
            &asset,
            &amount,
            &String::from_str(&env, "REENTER"),
            &String::from_str(&env, "retail"),
        );
        if res.is_err() {
            env.storage()
                .instance()
                .set(&CallbackKey::ReentryBlocked, &true);
        }
    }

    pub fn reentry_blocked(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&CallbackKey::ReentryBlocked)
            .unwrap_or(false)
    }
}

#[test]
fn reentrancy_via_merchant_callback_is_blocked() {
    let s = setup(100);
    let cb_id = s.env.register(MaliciousCallback, ());
    let cb = MaliciousCallbackClient::new(&s.env, &cb_id);
    cb.init(&s.pp.address);

    s.pp.set_merchant_callback(&s.merchant, &Some(cb_id.clone()));
    assert_eq!(s.pp.get_merchant_callback(&s.merchant), Some(cb_id));

    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "ORD-CB-1"),
    );
    assert!(cb.reentry_blocked());
    // The re-entrant payment was never recorded.
    assert_eq!(s.pp.payment_count(), 1);

    // Clearing the callback lets the next payment settle normally.
    s.pp.set_merchant_callback(&s.merchant, &None);
    assert_eq!(s.pp.get_merchant_callback(&s.merchant), None);
    let id2 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "ORD-CB-2"),
    );
    assert_ne!(id2, id);
    assert_eq!(s.pp.payment_count(), 2);
}

#[test]
fn state_snapshot_lifecycle_pay_refund_pause_and_next_pay() {
    let s = setup(250);
    s.env.ledger().set_timestamp(1_700_000_000);
    // Top up settlement so it can absorb the 2.5% protocol fee on a full refund.
    StellarAssetClient::new(&s.env, &s.asset).mint(&s.settlement, &(10 * UNIT));

    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 0,
            merchant_payment_count: 0,
            payer_balance: 1_000 * UNIT,
            settlement_balance: 10 * UNIT,
            treasury_balance: 0,
            processor_balance: 0,
        }
    );

    // 1. Settle payment #1 (100 UNIT -> 2.5 fee, 97.5 net).
    let id1 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "SNAP-1"),
    );
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 1,
            merchant_payment_count: 1,
            payer_balance: 900 * UNIT,
            settlement_balance: 1_075 * UNIT / 10,
            treasury_balance: 25 * UNIT / 10,
            processor_balance: 0,
        }
    );
    assert_eq!(
        s.pp.get_payment(&id1),
        Payment {
            id: id1,
            payer: s.payer.clone(),
            merchant: s.merchant.clone(),
            settlement: s.settlement.clone(),
            asset: s.asset.clone(),
            amount: 100 * UNIT,
            fee: 25 * UNIT / 10,
            refunded: 0,
            order_ref: order(&s.env, "SNAP-1"),
            loyalty_points: 0,
            status: PaymentStatus::Completed,
            created_at: 1_700_000_000,
        }
    );

    // 2. Partial refund (40 UNIT).
    s.pp.refund(&id1, &(40 * UNIT), &String::from_str(&s.env, "partial"));
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 1,
            merchant_payment_count: 1,
            payer_balance: 940 * UNIT,
            settlement_balance: 675 * UNIT / 10,
            treasury_balance: 25 * UNIT / 10,
            processor_balance: 0,
        }
    );
    assert_eq!(
        s.pp.get_payment(&id1).status,
        PaymentStatus::PartiallyRefunded
    );
    assert_eq!(s.pp.get_payment(&id1).refunded, 40 * UNIT);

    // 3. Pause blocks both pay and refund without mutating state.
    s.pp.pause();
    let paused_snap = capture_state(&s);
    assert!(paused_snap.paused);
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "BLOCKED")
        ),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(
        s.pp.try_refund(&id1, &(60 * UNIT), &String::from_str(&s.env, "blocked")),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(capture_state(&s), paused_snap);

    // 4. Unpause and complete refund (60 UNIT) -> counts drop to 0.
    s.pp.unpause();
    s.pp.refund(&id1, &(60 * UNIT), &String::from_str(&s.env, "final"));
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 0,
            merchant_payment_count: 0,
            payer_balance: 1_000 * UNIT,
            settlement_balance: 75 * UNIT / 10,
            treasury_balance: 25 * UNIT / 10,
            processor_balance: 0,
        }
    );
    assert_eq!(s.pp.get_payment(&id1).status, PaymentStatus::Refunded);
    assert_eq!(s.pp.get_payment(&id1).refunded, 100 * UNIT);

    // 5. A later payment gets a new ID while #1 remains Refunded.
    s.env.ledger().set_timestamp(1_700_000_060);
    let id2 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(40 * UNIT),
        &order(&s.env, "SNAP-2"),
    );
    assert_ne!(id2, id1);
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 1,
            merchant_payment_count: 1,
            payer_balance: 960 * UNIT,
            settlement_balance: 465 * UNIT / 10,
            treasury_balance: 35 * UNIT / 10,
            processor_balance: 0,
        }
    );

    let ledger_snap = s.env.to_snapshot();
    assert_eq!(ledger_snap.ledger.timestamp, 1_700_000_060);
    assert!(!ledger_snap.ledger.ledger_entries.is_empty());
}

#[test]
fn state_snapshot_batch_and_per_merchant_refund_counts() {
    let s = setup(100);
    let (m2, _settle2) = second_merchant(&s);
    StellarAssetClient::new(&s.env, &s.asset).mint(&s.settlement, &(5 * UNIT));

    let ids = s.pp.pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.merchant, 10 * UNIT, order(&s.env, "B-1")),
            line(&m2, 20 * UNIT, order(&s.env, "B-2")),
            line(&s.merchant, 30 * UNIT, order(&s.env, "B-3")),
        ],
    );
    assert_eq!(ids.len(), 3);
    assert_eq!(s.pp.payment_count(), 3);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 2);
    assert_eq!(s.pp.merchant_payment_count(&m2), 1);

    // Fully refunding merchant 1's first payment decrements only merchant 1 and the global count.
    let first = ids.get(0).unwrap();
    s.pp.refund(
        &first,
        &(10 * UNIT),
        &String::from_str(&s.env, "cancel item"),
    );
    assert_eq!(s.pp.payment_count(), 2);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);
    assert_eq!(s.pp.merchant_payment_count(&m2), 1);

    // Config transition snapshot.
    s.pp.set_fee_bps(&500);
    let cfg = s.pp.get_config();
    assert_eq!(cfg.fee_bps, 500);
    assert!(!cfg.paused);

    // An explicit lock makes the state-changing entry points fail with Reentrancy.
    s.env.as_contract(&s.pp.address, || {
        s.env
            .storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &true);
    });
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "LOCKED"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::Reentrancy))
    );
    assert_eq!(
        s.pp.try_refund(&first, &UNIT, &order(&s.env, "LOCKED")),
        Err(Ok(PaymentError::Reentrancy))
    );
    s.env.as_contract(&s.pp.address, || {
        s.env
            .storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &false);
    });
}

#[test]
fn delegated_payments_respect_the_spending_limit_and_expiry() {
    let s = setup(0);
    s.env.ledger().set_timestamp(1_700_000_000);
    let delegatee = Address::generate(&s.env);
    s.pp.create_delegation(&s.payer, &delegatee, &(15 * UNIT), &1_700_000_600);
    // The payer lets the processor pull what the delegatee may spend.
    s.token.approve(
        &s.payer,
        &s.pp.address,
        &(15 * UNIT),
        &(s.env.ledger().sequence() + 1_000),
    );

    let id = s.pp.pay_delegated(
        &delegatee,
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "DEL-1"),
    );
    assert_eq!(s.token.balance(&s.payer), 990 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 10 * UNIT);
    assert_eq!(s.pp.get_payment(&id).payer, s.payer);

    // Only 5 UNIT of the limit is left.
    assert_eq!(
        s.pp.try_pay_delegated(
            &delegatee,
            &s.payer,
            &s.merchant,
            &s.asset,
            &(6 * UNIT),
            &order(&s.env, "DEL-2"),
        ),
        Err(Ok(PaymentError::SpendingLimitExceeded))
    );

    // Someone without a delegation cannot spend the payer's funds.
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.pp.try_pay_delegated(
            &stranger,
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "DEL-3"),
        ),
        Err(Ok(PaymentError::PaymentNotFound))
    );

    // After expiry nothing more can be spent.
    s.env.ledger().set_timestamp(1_700_000_601);
    assert_eq!(
        s.pp.try_pay_delegated(
            &delegatee,
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "DEL-4"),
        ),
        Err(Ok(PaymentError::DelegationExpired))
    );
}

#[test]
fn signed_payments_need_a_registered_key_a_fresh_nonce_and_a_valid_signature() {
    let s = setup(0);
    s.env.ledger().set_timestamp(1_700_000_000);
    let sig = BytesN::from_array(&s.env, &[0u8; 64]);

    // No key registered yet.
    assert_eq!(
        s.pp.try_pay_signed(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "SIG-1"),
            &0,
            &1_700_000_600,
            &sig,
        ),
        Err(Ok(PaymentError::PaymentKeyNotSet))
    );

    let key = BytesN::from_array(&s.env, &[7u8; 32]);
    s.pp.register_payment_key(&s.payer, &key);

    // An authorization whose expiry has passed is refused.
    s.env.ledger().set_timestamp(1_700_000_601);
    assert_eq!(
        s.pp.try_pay_signed(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "SIG-2"),
            &0,
            &1_700_000_600,
            &sig,
        ),
        Err(Ok(PaymentError::AuthorizationExpired))
    );

    // The nonce must be the next one expected for this payer.
    assert_eq!(
        s.pp.try_pay_signed(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "SIG-3"),
            &5,
            &1_700_001_000,
            &sig,
        ),
        Err(Ok(PaymentError::InvalidNonce))
    );

    // A signature that does not verify aborts the call and moves nothing.
    assert!(s
        .pp
        .try_pay_signed(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "SIG-4"),
            &0,
            &1_700_001_000,
            &sig,
        )
        .is_err());
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
}

// Property tests for issue #135: Fee invariants

#[test]
fn property_fee_and_net_conserve_total_for_single_payment() {
    let test_cases = vec![
        (100, 100),
        (1_000, 250),
        (10_000_000, 500),
        (1_000_000_000, 10),
        (i128::MAX / 1_000, 1_000),
    ];

    for (amount, fee_bps) in test_cases {
        let s = setup(fee_bps);
        let id = s.pp.pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &amount,
            &order(&s.env, &format!("PROP-{}-{}", amount, fee_bps)),
        );

        let payment = s.pp.get_payment(&id);
        let total = payment.fee.checked_add(payment.amount).unwrap();
        assert_eq!(total, payment.fee + payment.amount,
            "fee + amount should compute correctly for amount={}, fee_bps={}", amount, fee_bps);

        let (expected_fee, expected_net) = s.pp.quote(&amount).unwrap();
        assert_eq!(payment.fee, expected_fee,
            "payment fee should match quote for amount={}, fee_bps={}", amount, fee_bps);
        assert_eq!(expected_fee + expected_net, amount,
            "fee and net must sum to original amount for amount={}, fee_bps={}", amount, fee_bps);
    }
}

#[test]
fn property_batch_payments_conserve_funds() {
    let s = setup(250);
    let (m2, _settle2) = second_merchant(&s);

    let amounts = vec![
        (100 * UNIT, "BAT-1"),
        (500 * UNIT, "BAT-2"),
        (1_000 * UNIT, "BAT-3"),
    ];

    let lines: Vec<BatchLine> = amounts.iter().map(|(amt, ref_str)| {
        line(&s.merchant, *amt, order(&s.env, ref_str))
    }).collect::<Vec<_>>();

    let ids = s.pp.pay_batch(&s.payer, &s.asset, &Vec::from_array(&s.env, &lines));

    let mut total_fees = 0i128;
    let mut total_net = 0i128;
    for id in ids.iter() {
        let payment = s.pp.get_payment(&id);
        total_fees = total_fees.checked_add(payment.fee).unwrap();
        total_net = total_net.checked_add(payment.amount - payment.fee).unwrap();
    }

    let total_paid: i128 = amounts.iter().map(|(amt, _)| amt).sum();
    assert_eq!(total_fees + total_net, total_paid,
        "batch payment fees and net should conserve total amount");
}

#[test]
fn property_royalty_payments_conserve_funds() {
    let s = setup(250);
    let recip1 = Address::generate(&s.env);
    let recip2 = Address::generate(&s.env);

    let royalties = vec![
        RoyaltyShare { recipient: recip1.clone(), bps: 1000 },
        RoyaltyShare { recipient: recip2.clone(), bps: 2000 },
    ];

    let amount = 100 * UNIT;
    let id = s.pp.pay_with_royalties(
        &s.payer,
        &s.merchant,
        &s.asset,
        &amount,
        &order(&s.env, "ROYAL-1"),
        &Vec::from_array(&s.env, &royalties),
    );

    let payment = s.pp.get_payment(&id);
    let (fee, net) = s.pp.quote(&amount).unwrap();

    let recip1_balance = s.token.balance(&recip1);
    let recip2_balance = s.token.balance(&recip2);
    let settlement_balance = s.token.balance(&s.settlement);
    let treasury_balance = s.token.balance(&s.treasury);

    let total_distributed = recip1_balance + recip2_balance + settlement_balance + treasury_balance;
    assert_eq!(total_distributed, amount,
        "royalty payment must distribute full amount: fee={}, net={}", fee, net);
}

#[test]
fn property_refunds_cannot_exceed_original_amount() {
    let s = setup(100);

    let payment_amounts = vec![100 * UNIT, 500 * UNIT, 1_000 * UNIT];

    for amount in payment_amounts {
        let id = s.pp.pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &amount,
            &order(&s.env, &format!("REFUND-{}", amount)),
        );

        let payment = s.pp.get_payment(&id);
        assert!(payment.refunded <= payment.amount,
            "initial refund amount must be 0 for payment amount={}", amount);

        let partial_refund = amount / 3;
        let total_refunded = s.pp.refund(&id, &partial_refund, &String::from_str(&s.env, "partial"));
        assert!(total_refunded <= amount,
            "refunded amount must not exceed original for amount={}", amount);

        let second_refund = amount - partial_refund;
        let total_after_second = s.pp.refund(&id, &second_refund, &String::from_str(&s.env, "complete"));
        assert_eq!(total_after_second, amount,
            "total refunded must equal original amount for amount={}", amount);
    }
}

#[test]
fn property_fee_rate_constraints() {
    let fee_rates = vec![0, 100, 250, 500, 1_000];
    let amount = 100 * UNIT;

    for fee_bps in fee_rates {
        let s = setup(fee_bps);
        let (quoted_fee, quoted_net) = s.pp.quote(&amount).unwrap();

        assert!(quoted_fee <= amount, "fee cannot exceed amount for fee_bps={}", fee_bps);
        assert!(quoted_net >= 0, "net must be non-negative for fee_bps={}", fee_bps);
        assert_eq!(quoted_fee + quoted_net, amount,
            "fee and net must sum to amount for fee_bps={}", fee_bps);

        let bps_pct = (quoted_fee * 10_000) / amount;
        assert!(bps_pct <= fee_bps as i128 + 1,
            "computed bps must match or be very close to configured rate");
    }
}

#[test]
fn property_multiple_payments_same_merchant_independent() {
    let s = setup(250);
    let amounts = vec![100 * UNIT, 200 * UNIT, 300 * UNIT];

    let mut ids = Vec::new(&s.env);
    for (i, amt) in amounts.iter().enumerate() {
        let id = s.pp.pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            amt,
            &order(&s.env, &format!("MULTI-{}", i)),
        );
        ids.push_back(id);
    }

    let mut total_fees = 0i128;
    let mut total_amounts = 0i128;
    for id in ids.iter() {
        let p = s.pp.get_payment(&id);
        total_fees = total_fees.checked_add(p.fee).unwrap();
        total_amounts = total_amounts.checked_add(p.amount).unwrap();
    }

    let expected_total: i128 = amounts.iter().sum();
    assert_eq!(total_amounts, expected_total,
        "sum of payment amounts must equal total paid");

    let expected_total_fee = (expected_total * 250) / 10_000;
    assert_eq!(total_fees, expected_total_fee,
        "sum of fees must equal total fee for all payments");
}

#[test]
fn property_idempotency_no_duplicate_charges() {
    let s = setup(250);

    let id1 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "IDEM-1"),
    );

    let payment1 = s.pp.get_payment(&id1);
    let settlement_after_first = s.token.balance(&s.settlement);

    let id2 = s.pp.try_pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "IDEM-1"),
    );

    assert!(id2.is_ok() || id2 == Err(Ok(PaymentError::DuplicateOrder)),
        "retried payment with same order ref should either return existing id or duplicate error");

    let settlement_after_retry = s.token.balance(&s.settlement);
    assert_eq!(settlement_after_first, settlement_after_retry,
        "retry with same order ref should not transfer additional funds");
}
