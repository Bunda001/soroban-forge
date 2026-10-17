use super::*;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, Env};

const YES: bool = true;
const NO: bool = false;

/// Ledger time the market is deployed at, in seconds.
const START: u64 = 100;
/// Resolution deadline, in seconds. From this instant on the market is closed
/// to staking and resolving, and open to refunds if it was never resolved.
const DEADLINE: u64 = 1_000;

struct Setup<'a> {
    market: PredictionMarketContractClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    oracle: Address,
}

fn setup(env: &Env) -> Setup<'_> {
    env.mock_all_auths();
    env.ledger().with_mut(|li| li.timestamp = START);

    let oracle = Address::generate(env);
    let token_issuer = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(token_issuer);
    let token_address = sac.address();

    let market_id = env.register(
        PredictionMarketContract,
        (oracle.clone(), token_address.clone(), DEADLINE),
    );

    Setup {
        market: PredictionMarketContractClient::new(env, &market_id),
        token: TokenClient::new(env, &token_address),
        token_admin: StellarAssetClient::new(env, &token_address),
        oracle,
    }
}

/// Mint `amount` to a fresh address and stake it all on `outcome`.
fn staker(env: &Env, s: &Setup, outcome: bool, amount: i128) -> Address {
    let who = Address::generate(env);
    s.token_admin.mint(&who, &amount);
    s.market.stake(&who, &outcome, &amount);
    who
}

/// Move the ledger clock to `timestamp`.
fn set_time(env: &Env, timestamp: u64) {
    env.ledger().with_mut(|li| li.timestamp = timestamp);
}

/// Move the ledger clock to the deadline instant, which already counts as
/// expired.
fn advance_to_deadline(env: &Env) {
    set_time(env, DEADLINE);
}

/// The error a `try_*` client call reports for one of this contract's errors.
///
/// The entrypoints fail with `panic_with_error!` rather than returning
/// `Result<_, Error>`, so the generated `try_*` methods surface failures as
/// `soroban_sdk::Error`, not as this crate's own `Error`.
fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from(e)
}

#[test]
fn constructor_stores_the_deadline() {
    let env = Env::default();
    let s = setup(&env);

    assert_eq!(s.market.get_deadline(), DEADLINE);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn constructor_rejects_a_deadline_that_is_not_in_the_future() {
    let env = Env::default();
    set_time(&env, START);

    let oracle = Address::generate(&env);
    let token_issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(token_issuer);

    // A deadline equal to the current ledger time is already expired.
    let _ = env.register(PredictionMarketContract, (oracle, sac.address(), START));
}

#[test]
fn stake_escrows_tokens_and_records_position() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, NO, 400);

    // The stake left the staker and is held by the market.
    assert_eq!(s.token.balance(&alice), 0);
    assert_eq!(s.token.balance(&bob), 0);
    assert_eq!(s.token.balance(&s.market.address), 1_000);

    // Positions and pools track each side separately.
    assert_eq!(s.market.get_stake(&alice, &YES), 600);
    assert_eq!(s.market.get_stake(&alice, &NO), 0);
    assert_eq!(s.market.get_pool(&YES), 600);
    assert_eq!(s.market.get_pool(&NO), 400);
    assert_eq!(s.market.get_total_pool(), 1_000);
}

#[test]
fn repeated_stakes_accumulate() {
    let env = Env::default();
    let s = setup(&env);

    let alice = Address::generate(&env);
    s.token_admin.mint(&alice, &500);
    s.market.stake(&alice, &YES, &200);
    s.market.stake(&alice, &YES, &300);

    assert_eq!(s.market.get_stake(&alice, &YES), 500);
    assert_eq!(s.market.get_pool(&YES), 500);
}

#[test]
fn oracle_resolves_and_outcome_is_readable() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    assert_eq!(s.market.get_outcome(), None);
    s.market.resolve(&s.oracle, &YES);
    assert_eq!(s.market.get_outcome(), Some(YES));
}

#[test]
#[should_panic]
fn only_the_oracle_can_resolve() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    // A perfectly ordinary address — authorized, but not the designated
    // oracle — must not be able to report an outcome.
    let impostor = Address::generate(&env);
    s.market.resolve(&impostor, &YES);
}

#[test]
fn winners_split_the_pool_proportionally() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, YES, 400);
    let carol = staker(&env, &s, NO, 500);
    assert_eq!(s.market.get_total_pool(), 1_500);

    s.market.resolve(&s.oracle, &YES);

    // Each winner takes stake * total_pool / winning_pool:
    //   alice 600 * 1500 / 1000 = 900
    //   bob   400 * 1500 / 1000 = 600
    assert_eq!(s.market.claim(&alice), 900);
    assert_eq!(s.market.claim(&bob), 600);

    assert_eq!(s.token.balance(&alice), 900);
    assert_eq!(s.token.balance(&bob), 600);
    assert_eq!(s.token.balance(&carol), 0);

    // The losing side's stakes are exactly what funded the winners' profit,
    // and the market is left empty.
    assert_eq!(s.token.balance(&s.market.address), 0);
    assert!(s.market.has_claimed(&alice));
    assert!(s.market.has_claimed(&bob));
}

#[test]
#[should_panic]
fn double_claim_panics() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    staker(&env, &s, NO, 400);
    s.market.resolve(&s.oracle, &YES);

    s.market.claim(&alice);
    s.market.claim(&alice);
}

#[test]
#[should_panic]
fn loser_cannot_claim() {
    let env = Env::default();
    let s = setup(&env);

    staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, NO, 400);
    s.market.resolve(&s.oracle, &YES);

    s.market.claim(&bob);
}

#[test]
#[should_panic]
fn claim_before_resolution_panics() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);

    s.market.claim(&alice);
}

#[test]
#[should_panic]
fn resolve_twice_panics() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    s.market.resolve(&s.oracle, &YES);
    s.market.resolve(&s.oracle, &NO);
}

#[test]
#[should_panic]
fn stake_after_resolution_panics() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    s.market.resolve(&s.oracle, &YES);

    staker(&env, &s, NO, 100);
}

#[test]
#[should_panic]
fn zero_amount_stake_panics() {
    let env = Env::default();
    let s = setup(&env);

    let alice = Address::generate(&env);
    s.token_admin.mint(&alice, &100);
    s.market.stake(&alice, &YES, &0);
}

// --- deadline and refunds ---

#[test]
fn unresolved_market_refunds_every_participant_after_the_deadline() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, NO, 400);
    assert_eq!(s.token.balance(&s.market.address), 1_000);

    // The oracle never resolves.
    advance_to_deadline(&env);
    assert_eq!(s.market.get_outcome(), None);

    // Each participant gets back exactly what they staked, whichever side
    // they backed — no payout formula, no dust.
    assert_eq!(s.market.refund(&alice), 600);
    assert_eq!(s.market.refund(&bob), 400);

    assert_eq!(s.token.balance(&alice), 600);
    assert_eq!(s.token.balance(&bob), 400);
    assert_eq!(s.token.balance(&s.market.address), 0);
    assert!(s.market.has_claimed(&alice));
    assert!(s.market.has_claimed(&bob));
}

#[test]
fn refund_returns_stakes_on_both_sides() {
    let env = Env::default();
    let s = setup(&env);

    let carol = Address::generate(&env);
    s.token_admin.mint(&carol, &150);
    s.market.stake(&carol, &YES, &100);
    s.market.stake(&carol, &NO, &50);

    advance_to_deadline(&env);

    assert_eq!(s.market.refund(&carol), 150);
    assert_eq!(s.token.balance(&carol), 150);
    assert_eq!(s.token.balance(&s.market.address), 0);
}

#[test]
fn refund_works_long_after_the_deadline() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 250);

    set_time(&env, DEADLINE + 1_000_000);

    assert_eq!(s.market.refund(&alice), 250);
    assert_eq!(s.token.balance(&alice), 250);
}

#[test]
fn refund_before_the_deadline_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);

    assert_eq!(
        s.market.try_refund(&alice),
        Err(Ok(err(Error::DeadlineNotReached)))
    );

    // One second before the deadline is still too early.
    set_time(&env, DEADLINE - 1);
    assert_eq!(
        s.market.try_refund(&alice),
        Err(Ok(err(Error::DeadlineNotReached)))
    );
    assert_eq!(s.token.balance(&alice), 0);
}

#[test]
fn double_refund_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);
    staker(&env, &s, NO, 400);

    advance_to_deadline(&env);
    assert_eq!(s.market.refund(&alice), 600);

    assert_eq!(
        s.market.try_refund(&alice),
        Err(Ok(err(Error::AlreadyClaimed)))
    );
    // Only the first refund moved tokens: alice's balance did not double.
    assert_eq!(s.token.balance(&alice), 600);
    assert_eq!(s.token.balance(&s.market.address), 400);
}

#[test]
fn refund_without_a_stake_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 600);

    advance_to_deadline(&env);

    let stranger = Address::generate(&env);
    assert_eq!(
        s.market.try_refund(&stranger),
        Err(Ok(err(Error::NothingToClaim)))
    );
}

#[test]
fn refund_is_rejected_once_the_market_is_resolved() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);
    staker(&env, &s, NO, 400);

    // Resolved in time, so the payout path applies and the refund path
    // stays closed even after the deadline.
    s.market.resolve(&s.oracle, &YES);
    advance_to_deadline(&env);

    assert_eq!(
        s.market.try_refund(&alice),
        Err(Ok(err(Error::AlreadyResolved)))
    );
}

#[test]
fn resolve_after_the_deadline_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);

    set_time(&env, DEADLINE + 1);

    assert_eq!(
        s.market.try_resolve(&s.oracle, &YES),
        Err(Ok(err(Error::DeadlinePassed)))
    );
    assert_eq!(s.market.get_outcome(), None);

    // The market stays unresolved, so the stake is recoverable.
    assert_eq!(s.market.refund(&alice), 600);
}

#[test]
fn resolve_exactly_at_the_deadline_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    advance_to_deadline(&env);

    assert_eq!(
        s.market.try_resolve(&s.oracle, &YES),
        Err(Ok(err(Error::DeadlinePassed)))
    );
}

#[test]
fn resolve_just_before_the_deadline_still_works() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    set_time(&env, DEADLINE - 1);
    s.market.resolve(&s.oracle, &YES);

    assert_eq!(s.market.get_outcome(), Some(YES));
}

#[test]
fn stake_after_the_deadline_is_rejected() {
    let env = Env::default();
    let s = setup(&env);

    let alice = Address::generate(&env);
    s.token_admin.mint(&alice, &100);
    advance_to_deadline(&env);

    assert_eq!(
        s.market.try_stake(&alice, &YES, &100),
        Err(Ok(err(Error::DeadlinePassed)))
    );
    assert_eq!(s.token.balance(&alice), 100);
    assert_eq!(s.market.get_total_pool(), 0);
}

#[test]
fn claim_on_an_expired_unresolved_market_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);

    advance_to_deadline(&env);

    // An expired, unresolved market pays out only through `refund`.
    assert_eq!(s.market.try_claim(&alice), Err(Ok(err(Error::NotResolved))));
}

#[test]
fn winners_can_still_claim_after_the_deadline() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);
    staker(&env, &s, NO, 400);

    s.market.resolve(&s.oracle, &YES);
    advance_to_deadline(&env);

    // The deadline only closes staking and resolving; a market resolved in
    // time keeps paying out.
    assert_eq!(s.market.claim(&alice), 1_000);
    assert_eq!(s.token.balance(&alice), 1_000);
}
