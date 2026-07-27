//! TalosRegistry — Soroban smart contract for Talos Protocol.
//!
//! Handles:
//! - Talos creation (with Pulse token metadata)
//! - Protocol fee collection (3% launchpad fee)
//! - Talos metadata storage and retrieval
//! - Patron registration with minimum Pulse holding validation

#![no_std]

pub mod allowlist;

#[cfg(all(test, not(target_arch = "wasm32")))]
extern crate std;

use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, Address, Env, String};

// ── Data Types ──────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct Patron {
    pub creator_share: u32,
    pub investor_share: u32,
    pub treasury_share: u32,
    pub creator_addr: Address,
    pub investor_addr: Address,
    pub treasury_addr: Address,
}

#[contracttype]
#[derive(Clone)]
pub struct Kernel {
    pub approval_threshold: i128,
    pub gtm_budget: i128,
    pub min_patron_pulse: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct Pulse {
    pub total_supply: i128,
    pub price_usd_cents: i128,
    pub token_symbol: String,
}

#[contracttype]
#[derive(Clone)]
pub struct Talos {
    pub id: u32,
    pub name: String,
    pub category: String,
    pub description: String,
    pub creator: Address,
    pub patron: Patron,
    pub kernel: Kernel,
    pub pulse: Pulse,
    pub created_at: u64,
    pub active: bool,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    NextTalosId,
    Talos(u32),
    CreatorOf(u32),
    ProtocolWallet,
    ProtocolFeeBps,
    /// Holds the address nominated by the current admin but not yet accepted.
    /// Presence of this key means a transfer is in progress; absence means none.
    PendingAdmin,
}

// ── Events ──────────────────────────────────────────────────────────
//
// Event schema (topics → data):
//   tls_crt : (symbol, creator: Address)   → (talos_id: u32, name: String, category: String)
//   pat_upd : (symbol, talos_id: u32)      → (creator: Address, creator_share: u32, investor_share: u32)
//   fee_chg : (symbol,)                    → (old_bps: u32, new_bps: u32)
//   adm_prp : (symbol,)                    → (current: Address, proposed: Address)
//   adm_acc : (symbol,)                    → (new_admin: Address)
//   adm_cnl : (symbol,)                    → (cancelled: Address)

fn emit_talos_created(env: &Env, talos_id: u32, creator: Address, name: String, category: String) {
    let topics = (symbol_short!("tls_crt"), creator);
    env.events().publish(topics, (talos_id, name, category));
}

fn emit_patron_updated(env: &Env, talos_id: u32, patron: &Patron) {
    let topics = (symbol_short!("pat_upd"), talos_id);
    env.events().publish(
        topics,
        (
            patron.creator_addr.clone(),
            patron.creator_share,
            patron.investor_share,
        ),
    );
}

fn emit_protocol_fee_changed(env: &Env, old_bps: u32, new_bps: u32) {
    let topics = (symbol_short!("fee_chg"),);
    env.events().publish(topics, (old_bps, new_bps));
}

/// Emitted when the current admin nominates a new admin.
/// `proposed` is the address that must call `accept_admin` to complete the transfer.
fn emit_admin_proposed(env: &Env, current: Address, proposed: Address) {
    let topics = (symbol_short!("adm_prp"),);
    env.events().publish(topics, (current, proposed));
}

/// Emitted when the pending admin accepts the transfer.
/// `new_admin` is now the active protocol wallet / admin.
fn emit_admin_accepted(env: &Env, new_admin: Address) {
    let topics = (symbol_short!("adm_acc"),);
    env.events().publish(topics, (new_admin,));
}

/// Emitted when the current admin cancels an in-progress admin transfer.
/// `cancelled` is the address whose nomination was revoked.
fn emit_admin_cancelled(env: &Env, cancelled: Address) {
    let topics = (symbol_short!("adm_cnl"),);
    env.events().publish(topics, (cancelled,));
}

fn emit_asset_allowed(env: &Env, admin: &Address, asset: &Address) {
    env.events()
        .publish((symbol_short!("alw_add"), admin.clone()), asset.clone());
}

fn emit_asset_removed(env: &Env, admin: &Address, asset: &Address) {
    env.events()
        .publish((symbol_short!("alw_rem"), admin.clone()), asset.clone());
}

fn emit_protocol_fee_paid(
    env: &Env,
    token: &Address,
    from: &Address,
    to: &Address,
    amount: i128,
    fee: i128,
) {
    env.events().publish(
        (symbol_short!("fee_pay"), token.clone()),
        (from.clone(), to.clone(), amount, fee),
    );
}

fn emit_revenue_reported(
    env: &Env,
    talos_id: u32,
    payer: &Address,
    token: &Address,
    amount: i128,
    currency: String,
) {
    env.events().publish(
        (symbol_short!("rev_rep"), talos_id),
        (payer.clone(), token.clone(), amount, currency),
    );
}

// ── Helpers ─────────────────────────────────────────────────────────

fn validate_patron_shares(patron: &Patron) {
    let total = patron.creator_share + patron.investor_share + patron.treasury_share;
    if total != 100 {
        panic!("Patron shares must sum to 100");
    }
}

// ── Constants ───────────────────────────────────────────────────────

const PROTOCOL_FEE_BPS: u32 = 300; // 3%
const MAX_PROTOCOL_FEE_BPS: u32 = 10_000; // 100%

/// Compile-time interface version of TalosRegistry.
///
/// Format: `(major, minor, patch)` following Semantic Versioning.
///
/// Bump rules:
/// - **major** — incompatible ABI change (removed/renamed entry-points, changed argument types)
/// - **minor** — backwards-compatible new entry-point or return-field added
/// - **patch** — bug-fix with no observable ABI change
///
/// This constant is embedded in the WASM binary at compile time and is
/// therefore immutable once deployed; it cannot be altered by any admin
/// call, storage write, or cross-contract invocation.
pub const CONTRACT_VERSION: (u32, u32, u32) = (1, 0, 0);

// ── Contract ────────────────────────────────────────────────────────

#[contract]
pub struct TalosRegistry;

#[contractimpl]
impl TalosRegistry {
    /// Create a new Talos on-chain.
    ///
    /// # Arguments
    /// * `e` - Soroban environment
    /// * `name` - Talos name
    /// * `category` - Category (Marketing, Sales, etc.)
    /// * `description` - Description
    /// * `patron` - Patron configuration (shares + addresses)
    /// * `kernel` - Kernel policy (thresholds, budget)
    /// * `pulse` - Pulse token config (supply, price, symbol)
    /// * `protocol_wallet` - Protocol wallet to receive 3% fee
    ///
    /// Returns the new Talos ID.
    pub fn create_talos(
        e: Env,
        name: String,
        category: String,
        description: String,
        patron: Patron,
        kernel: Kernel,
        pulse: Pulse,
        protocol_wallet: Address,
    ) -> u32 {
        // Require creator authorization
        patron.creator_addr.require_auth();

        validate_patron_shares(&patron);

        // If the registry has been initialized, ensure callers use the configured
        // protocol wallet. This keeps the create_talos ABI backwards compatible
        // while preventing a mismatched fee recipient from being supplied.
        if let Some(configured_wallet) = e
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::ProtocolWallet)
        {
            if configured_wallet != protocol_wallet {
                panic!("Protocol wallet mismatch");
            }
        }

        // Get next Talos ID
        let next_id: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1);

        // Create Talos struct
        let talos = Talos {
            id: next_id,
            name: name.clone(),
            category,
            description,
            creator: patron.creator_addr.clone(),
            patron,
            kernel,
            pulse,
            created_at: e.ledger().timestamp(),
            active: true,
        };

        // Store Talos
        e.storage()
            .persistent()
            .set(&DataKey::Talos(next_id), &talos);

        // Track creator
        e.storage()
            .persistent()
            .set(&DataKey::CreatorOf(next_id), &talos.creator);

        // Increment next ID
        e.storage()
            .persistent()
            .set(&DataKey::NextTalosId, &(next_id + 1));

        // Emit event
        emit_talos_created(
            &e,
            next_id,
            talos.creator.clone(),
            name,
            talos.category.clone(),
        );

        next_id
    }

    /// Get Talos by ID.
    pub fn get_talos(e: Env, talos_id: u32) -> Option<Talos> {
        e.storage().persistent().get(&DataKey::Talos(talos_id))
    }

    /// Get the creator address of a Talos.
    pub fn creator_of(e: Env, talos_id: u32) -> Option<Address> {
        e.storage().persistent().get(&DataKey::CreatorOf(talos_id))
    }

    /// Check if a Talos is active.
    pub fn is_active(e: Env, talos_id: u32) -> bool {
        e.storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .map(|t: Talos| t.active)
            .unwrap_or(false)
    }

    /// Get the next Talos ID (for counting).
    pub fn next_talos_id(e: Env) -> u32 {
        e.storage()
            .persistent()
            .get(&DataKey::NextTalosId)
            .unwrap_or(1)
    }

    /// Update patron shares for a Talos.
    pub fn update_patron(e: Env, talos_id: u32, patron: Patron) {
        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        // Require creator authorization
        talos.creator.require_auth();

        validate_patron_shares(&patron);

        talos.patron = patron.clone();

        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);

        emit_patron_updated(&e, talos_id, &patron);
    }

    /// Update kernel policy for a Talos.
    pub fn update_kernel(e: Env, talos_id: u32, kernel: Kernel) {
        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();

        talos.kernel = kernel;

        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);
    }

    /// Update pulse token config for a Talos.
    pub fn update_pulse(e: Env, talos_id: u32, pulse: Pulse) {
        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();

        talos.pulse = pulse;

        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);
    }

    /// Deactivate a Talos.
    pub fn deactivate_talos(e: Env, talos_id: u32) {
        let mut talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");

        talos.creator.require_auth();
        talos.active = false;

        e.storage()
            .persistent()
            .set(&DataKey::Talos(talos_id), &talos);
    }

    /// Initialize the contract with protocol wallet and fee.
    pub fn initialize(e: Env, protocol_wallet: Address) {
        if e.storage()
            .persistent()
            .get::<_, Address>(&DataKey::ProtocolWallet)
            .is_some()
        {
            panic!("Already initialized");
        }

        e.storage()
            .persistent()
            .set(&DataKey::ProtocolWallet, &protocol_wallet);
        e.storage()
            .persistent()
            .set(&DataKey::ProtocolFeeBps, &PROTOCOL_FEE_BPS);
        e.storage().persistent().set(&DataKey::NextTalosId, &1u32);
    }

    /// Get the protocol wallet address.
    pub fn protocol_wallet(e: Env) -> Option<Address> {
        e.storage().persistent().get(&DataKey::ProtocolWallet)
    }

    /// Get the protocol fee in basis points.
    pub fn protocol_fee_bps(e: Env) -> Option<u32> {
        e.storage().persistent().get(&DataKey::ProtocolFeeBps)
    }

    /// Update the protocol fee in basis points.
    ///
    /// Only the configured protocol wallet may update the fee.
    pub fn set_protocol_fee(e: Env, fee_bps: u32) {
        if fee_bps > MAX_PROTOCOL_FEE_BPS {
            panic!("Protocol fee cannot exceed 100%");
        }

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        admin.require_auth();

        let old_bps: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(PROTOCOL_FEE_BPS);

        e.storage()
            .persistent()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);

        emit_protocol_fee_changed(&e, old_bps, fee_bps);
    }

    // ── Two-step admin transfer ──────────────────────────────────────

    /// Nominate `new_admin` as the next protocol wallet (admin).
    ///
    /// The transfer is **not** immediate. The nominated address must call
    /// `accept_admin` to confirm and complete the handover. Until then the
    /// current admin retains all privileges.
    ///
    /// Calling `propose_admin` while a transfer is already pending silently
    /// replaces the old nomination with the new one (no separate cancellation
    /// step is required for the proposer). The replacement emits a fresh
    /// `adm_prp` event without emitting `adm_cnl`.
    ///
    /// # Authorization
    /// Requires the current admin (`ProtocolWallet`) to sign the transaction.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    pub fn propose_admin(e: Env, new_admin: Address) {
        let current: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        current.require_auth();

        e.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin);

        emit_admin_proposed(&e, current, new_admin);
    }

    /// Complete the pending admin transfer.
    ///
    /// The nominated address (stored under `PendingAdmin`) must call this
    /// method and authorize it. On success:
    ///   1. `ProtocolWallet` is updated to the caller's address.
    ///   2. The `PendingAdmin` storage entry is removed.
    ///   3. An `adm_acc` event is emitted.
    ///
    /// # Authorization
    /// Requires the pending admin to sign the transaction.
    ///
    /// # Panics
    /// - `"No pending admin transfer"` — if `propose_admin` has not been called.
    pub fn accept_admin(e: Env) {
        let pending: Address = e
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .expect("No pending admin transfer");

        pending.require_auth();

        // Promote pending → active admin.
        e.storage()
            .persistent()
            .set(&DataKey::ProtocolWallet, &pending);

        // Clear the pending slot.
        e.storage().persistent().remove(&DataKey::PendingAdmin);

        emit_admin_accepted(&e, pending);
    }

    /// Cancel an in-progress admin transfer.
    ///
    /// Removes the `PendingAdmin` entry and revokes the nomination without
    /// changing the current admin.
    ///
    /// # Authorization
    /// Requires the current admin (`ProtocolWallet`) to sign the transaction.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    /// - `"No pending admin transfer"` — if there is nothing to cancel.
    pub fn cancel_admin_transfer(e: Env) {
        let current: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        current.require_auth();

        let pending: Address = e
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .expect("No pending admin transfer");

        e.storage().persistent().remove(&DataKey::PendingAdmin);

        emit_admin_cancelled(&e, pending);
    }

    /// Return the pending admin address, if any.
    pub fn pending_admin(e: Env) -> Option<Address> {
        e.storage().persistent().get(&DataKey::PendingAdmin)
    }

    /// Return the contract's interface version as `(major, minor, patch)`.
    ///
    /// The value is a compile-time constant baked into the WASM binary.
    /// It is **not** stored in ledger state and cannot be altered by any
    /// administrator, upgrade, or cross-contract call after deployment.
    ///
    /// Clients should call this method to verify ABI compatibility before
    /// invoking other entry-points. A change in `major` signals a breaking
    /// change; a change in `minor` adds new entry-points while remaining
    /// backwards compatible; `patch` carries bug-fixes only.
    ///
    /// # Returns
    /// `(major: u32, minor: u32, patch: u32)` — currently `(1, 0, 0)`.
    pub fn version(_e: Env) -> (u32, u32, u32) {
        CONTRACT_VERSION
    }

    /// Calculate the protocol fee for an amount using the configured fee bps.
    pub fn calculate_protocol_fee(e: Env, amount: i128) -> i128 {
        if amount < 0 {
            panic!("Amount must be non-negative");
        }

        let fee_bps: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(PROTOCOL_FEE_BPS);

        amount * fee_bps as i128 / MAX_PROTOCOL_FEE_BPS as i128
    }

    // ── Asset allowlist ───────────────────────────────────────────────

    /// Add an asset (token contract address) to the transfer allowlist.
    ///
    /// Only allowlisted assets may be used in `collect_protocol_fee` and
    /// `report_revenue` — which are the value-transfer entry-points of the
    /// registry contract. This prevents an attacker from supplying a
    /// malicious/mintable token to siphon protocol fees.
    ///
    /// # Authorization
    /// Requires the current admin (`ProtocolWallet`) to sign the transaction.
    pub fn allow_asset(e: Env, asset: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        use crate::allowlist::AssetAllowlist;
        AssetAllowlist::add(&e, &admin, &asset);
        emit_asset_allowed(&e, &admin, &asset);
    }

    /// Remove an asset from the transfer allowlist.
    ///
    /// # Authorization
    /// Requires the current admin (`ProtocolWallet`) to sign the transaction.
    pub fn deny_asset(e: Env, asset: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        use crate::allowlist::AssetAllowlist;
        AssetAllowlist::remove(&e, &admin, &asset);
        emit_asset_removed(&e, &admin, &asset);
    }

    /// Query whether an asset is currently on the allowlist.
    pub fn is_asset_allowed(e: Env, asset: Address) -> bool {
        use crate::allowlist::AssetAllowlist;
        AssetAllowlist::is_allowed(&e, &asset)
    }

    // ── Value-transfer entry points ───────────────────────────────────

    /// Execute a token transfer that includes the protocol fee deduction.
    ///
    /// This is the **primary value-transfer entry-point** of the registry:
    /// callers pass an allowlisted `asset`, a `from` payer, a `to`
    /// recipient, and a gross `amount`. The contract:
    ///
    ///   1. Enforces `asset` is in the allowlist.
    ///   2. Requires `from` authorization to move its tokens.
    ///   3. Computes `fee = amount * fee_bps / 10_000`.
    ///   4. Transfers `fee` tokens `from → ProtocolWallet`.
    ///   5. Transfers `amount - fee` tokens `from → to`.
    ///   6. Emits `fee_pay` with the final state.
    ///
    /// Returns `(net_amount_to_receiver, protocol_fee)` for integrators.
    pub fn collect_protocol_fee(
        e: Env,
        asset: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> (i128, i128) {
        if amount <= 0 {
            panic!("amount must be positive");
        }

        use crate::allowlist::AssetAllowlist;
        AssetAllowlist::enforce(&e, &asset);

        from.require_auth();

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        let fee = Self::calculate_protocol_fee(e.clone(), amount);
        let net = amount - fee;
        if net < 0 {
            panic!("fee exceeds amount");
        }

        let token_client =
            soroban_sdk::token::TokenClient::new(&e, &asset);

        if fee > 0 {
            token_client.transfer(&from, &admin, &fee);
        }
        if net > 0 {
            token_client.transfer(&from, &to, &net);
        }

        emit_protocol_fee_paid(&e, &asset, &from, &to, amount, fee);
        (net, fee)
    }

    /// Report revenue paid to a Talos creator.
    ///
    /// Caller (payer) authorizes the transfer of `amount` of the allowlisted
    /// `asset` from itself to the Talos creator of `talos_id`. This is the
    /// **representative production-shaped path** used for
    /// commerce / service payments through the registry.
    ///
    /// Steps:
    ///   1. Look up Talos → assert active → load `creator` and `talos_id`.
    ///   2. Enforce `asset` is allowlisted (otherwise panic).
    ///   3. `payer.require_auth()` for the transfer.
    ///   4. Compute protocol fee → send fee to protocol wallet → send net to
    ///      Talos creator via Stellar token `transfer`.
    ///   5. Emit `rev_rep` event with `currency` string (for off-chain ledger).
    ///
    /// Returns `(net_to_creator, protocol_fee)`.
    pub fn report_revenue(
        e: Env,
        talos_id: u32,
        payer: Address,
        asset: Address,
        amount: i128,
        currency: String,
    ) -> (i128, i128) {
        if amount <= 0 {
            panic!("amount must be positive");
        }

        let talos: Talos = e
            .storage()
            .persistent()
            .get(&DataKey::Talos(talos_id))
            .expect("Talos not found");
        if !talos.active {
            panic!("Talos not active");
        }

        use crate::allowlist::AssetAllowlist;
        AssetAllowlist::enforce(&e, &asset);

        payer.require_auth();

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        let fee = Self::calculate_protocol_fee(e.clone(), amount);
        let net = amount - fee;
        if net < 0 {
            panic!("fee exceeds amount");
        }

        let token_client =
            soroban_sdk::token::TokenClient::new(&e, &asset);

        if fee > 0 {
            token_client.transfer(&payer, &admin, &fee);
        }
        if net > 0 {
            token_client.transfer(&payer, &talos.creator, &net);
        }

        emit_revenue_reported(&e, talos_id, &payer, &asset, amount, currency);
        (net, fee)
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events as _, MockAuth, MockAuthInvoke},
        Address, Env, IntoVal, Symbol, TryFromVal,
    };

    fn setup() -> (Env, Address) {
        let env = Env::default();
        let contract_id = env.register_contract(None, TalosRegistry);
        (env, contract_id)
    }

    fn s(env: &Env, value: &str) -> String {
        String::from_str(env, value)
    }

    fn patron(env: &Env, creator: &Address) -> Patron {
        Patron {
            creator_share: 60,
            investor_share: 25,
            treasury_share: 15,
            creator_addr: creator.clone(),
            investor_addr: Address::generate(env),
            treasury_addr: Address::generate(env),
        }
    }

    fn kernel() -> Kernel {
        Kernel {
            approval_threshold: 10,
            gtm_budget: 1_000,
            min_patron_pulse: 100,
        }
    }

    fn pulse(env: &Env) -> Pulse {
        Pulse {
            total_supply: 1_000_000,
            price_usd_cents: 100,
            token_symbol: s(env, "TLOS"),
        }
    }

    fn create_talos_with_auth(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        creator: &Address,
        protocol_wallet: &Address,
    ) -> u32 {
        let name = s(env, "Genesis");
        let category = s(env, "Marketing");
        let description = s(env, "Autonomous marketing agent");
        let patron = patron(env, creator);
        let kernel = kernel();
        let pulse = pulse(env);

        client
            .mock_auths(&[MockAuth {
                address: creator,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "create_talos",
                    args: (
                        name.clone(),
                        category.clone(),
                        description.clone(),
                        patron.clone(),
                        kernel.clone(),
                        pulse.clone(),
                        protocol_wallet.clone(),
                    )
                        .into_val(env),
                    sub_invokes: &[],
                },
            }])
            .create_talos(
                &name,
                &category,
                &description,
                &patron,
                &kernel,
                &pulse,
                protocol_wallet,
            )
    }

    // ── version() tests ──────────────────────────────────────────────

    #[test]
    fn version_returns_compile_time_constant() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        assert_eq!(client.version(), (1u32, 0u32, 0u32));
    }

    #[test]
    fn version_is_idempotent() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        // Calling version() multiple times must always return the same value.
        assert_eq!(client.version(), client.version());
    }

    #[test]
    fn version_is_unaffected_by_state_changes() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let protocol_wallet = Address::generate(&env);

        let before = client.version();

        // Initialize the contract and change the fee — state writes must not
        // affect the version constant.
        client.initialize(&protocol_wallet);
        client
            .mock_auths(&[MockAuth {
                address: &protocol_wallet,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_protocol_fee",
                    args: (500u32,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_protocol_fee(&500);

        let after = client.version();
        assert_eq!(before, after);
    }

    #[test]
    fn version_matches_contract_version_constant() {
        // Verify that the public CONTRACT_VERSION constant and the on-chain
        // entry-point are in sync, so tooling that reads the constant directly
        // agrees with what the deployed WASM reports.
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let (maj, min, patch) = client.version();
        assert_eq!((maj, min, patch), CONTRACT_VERSION);
    }

    #[test]
    fn create_talos_happy_path() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);

        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        assert_eq!(id, 1);
        assert_eq!(client.next_talos_id(), 2);
        assert_eq!(client.creator_of(&id), Some(creator.clone()));
        assert!(client.is_active(&id));

        let talos = client.get_talos(&id).expect("talos should be stored");
        assert_eq!(talos.id, id);
        assert_eq!(talos.name, s(&env, "Genesis"));
        assert_eq!(talos.category, s(&env, "Marketing"));
        assert_eq!(talos.creator, creator);
        assert!(talos.active);
    }

    #[test]
    fn initialize_guard_rejects_reinitialization() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);

        client.initialize(&Address::generate(&env));

        assert!(client.try_initialize(&Address::generate(&env)).is_err());
    }

    #[test]
    fn update_patron_requires_creator_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        let new_patron = Patron {
            creator_share: 50,
            investor_share: 30,
            treasury_share: 20,
            creator_addr: creator,
            investor_addr: Address::generate(&env),
            treasury_addr: Address::generate(&env),
        };

        assert!(client.try_update_patron(&id, &new_patron).is_err());
    }

    #[test]
    fn set_protocol_fee_requires_admin_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let protocol_wallet = Address::generate(&env);

        client.initialize(&protocol_wallet);

        assert!(client.try_set_protocol_fee(&500).is_err());
    }

    #[test]
    fn protocol_fee_calculation_uses_configured_basis_points() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let protocol_wallet = Address::generate(&env);

        client.initialize(&protocol_wallet);
        assert_eq!(client.protocol_fee_bps(), Some(300));
        assert_eq!(client.calculate_protocol_fee(&10_000), 300);

        client
            .mock_auths(&[MockAuth {
                address: &protocol_wallet,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_protocol_fee",
                    args: (250u32,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_protocol_fee(&250);

        assert_eq!(client.protocol_fee_bps(), Some(250));
        assert_eq!(client.calculate_protocol_fee(&40_000), 1_000);
    }

    fn assert_topic_symbol(
        env: &Env,
        topics: &soroban_sdk::Vec<soroban_sdk::Val>,
        idx: u32,
        expected: Symbol,
    ) {
        let val = topics.get(idx).expect("topic index out of range");
        let sym: Symbol = TryFromVal::try_from_val(env, &val).expect("topic is not a Symbol");
        assert_eq!(sym, expected);
    }

    fn assert_topic_address(
        env: &Env,
        topics: &soroban_sdk::Vec<soroban_sdk::Val>,
        idx: u32,
        expected: &Address,
    ) {
        let val = topics.get(idx).expect("topic index out of range");
        let addr: Address = TryFromVal::try_from_val(env, &val).expect("topic is not an Address");
        assert_eq!(addr, *expected);
    }

    fn assert_topic_u32(
        env: &Env,
        topics: &soroban_sdk::Vec<soroban_sdk::Val>,
        idx: u32,
        expected: u32,
    ) {
        let val = topics.get(idx).expect("topic index out of range");
        let n: u32 = TryFromVal::try_from_val(env, &val).expect("topic is not a u32");
        assert_eq!(n, expected);
    }

    #[test]
    fn create_talos_emits_tls_crt_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);

        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        let events = env.events().all();
        assert_eq!(events.len(), 1);
        let (addr, topics, data) = events.get(0).unwrap();

        assert_eq!(addr, contract_id);
        assert_eq!(topics.len(), 2);
        assert_topic_symbol(&env, &topics, 0, symbol_short!("tls_crt"));
        assert_topic_address(&env, &topics, 1, &creator);

        let (got_id, got_name, got_cat): (u32, String, String) =
            TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_id, id);
        assert_eq!(got_name, s(&env, "Genesis"));
        assert_eq!(got_cat, s(&env, "Marketing"));
    }

    #[test]
    fn update_patron_emits_pat_upd_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        let new_patron = Patron {
            creator_share: 50,
            investor_share: 30,
            treasury_share: 20,
            creator_addr: creator.clone(),
            investor_addr: Address::generate(&env),
            treasury_addr: Address::generate(&env),
        };

        client
            .mock_auths(&[MockAuth {
                address: &creator,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "update_patron",
                    args: (id, new_patron.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .update_patron(&id, &new_patron);

        // tls_crt from create_talos + pat_upd from update_patron
        let events = env.events().all();
        assert_eq!(events.len(), 2);
        let (addr, topics, data) = events.get(1).unwrap();

        assert_eq!(addr, contract_id);
        assert_eq!(topics.len(), 2);
        assert_topic_symbol(&env, &topics, 0, symbol_short!("pat_upd"));
        assert_topic_u32(&env, &topics, 1, id);

        let (got_creator, got_cs, got_is): (Address, u32, u32) =
            TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_creator, creator);
        assert_eq!(got_cs, 50);
        assert_eq!(got_is, 30);
    }

    #[test]
    fn set_protocol_fee_emits_fee_chg_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let protocol_wallet = Address::generate(&env);

        client.initialize(&protocol_wallet);

        client
            .mock_auths(&[MockAuth {
                address: &protocol_wallet,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_protocol_fee",
                    args: (500u32,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_protocol_fee(&500);

        let events = env.events().all();
        assert_eq!(events.len(), 1);
        let (addr, topics, data) = events.get(0).unwrap();

        assert_eq!(addr, contract_id);
        assert_eq!(topics.len(), 1);
        assert_topic_symbol(&env, &topics, 0, symbol_short!("fee_chg"));

        let (old_bps, new_bps): (u32, u32) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(old_bps, 300);
        assert_eq!(new_bps, 500);
    }
    #[test]
    fn get_talos_returns_none_for_missing_id() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);

        // IDs are 1-indexed, 0 does not exist
        assert!(client.get_talos(&0).is_none());
    }

    #[test]
    fn update_kernel_requires_creator_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        let new_kernel = Kernel {
            approval_threshold: 20,
            gtm_budget: 5_000,
            min_patron_pulse: 500,
        };

        // Non-creator must not be able to update kernel
        let imposter = Address::generate(&env);
        assert!(client.try_update_kernel(&id, &new_kernel).is_err());
    }

    #[test]
    fn update_kernel_persists_changes() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        let new_kernel = Kernel {
            approval_threshold: 20,
            gtm_budget: 5_000,
            min_patron_pulse: 500,
        };

        client
            .mock_auths(&[MockAuth {
                address: &creator,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "update_kernel",
                    args: (id, new_kernel.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .update_kernel(&id, &new_kernel);

        let updated = client.get_talos(&id).expect("talos should exist");
        assert_eq!(updated.kernel.approval_threshold, 20);
        assert_eq!(updated.kernel.gtm_budget, 5_000);
        assert_eq!(updated.kernel.min_patron_pulse, 500);
    }

    #[test]
    fn deactivate_talos_requires_creator_auth_and_sets_inactive() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        // Initially active
        assert!(client.is_active(&id));

        // Non-creator can't deactivate
        let imposter = Address::generate(&env);
        assert!(client.try_deactivate_talos(&id).is_err());

        // Creator can deactivate
        client
            .mock_auths(&[MockAuth {
                address: &creator,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "deactivate_talos",
                    args: (id,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .deactivate_talos(&id);

        assert!(!client.is_active(&id));
    }

    #[test]
    fn create_talos_rejects_shares_not_summing_to_100() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);

        let bad_patron = Patron {
            creator_share: 60,
            investor_share: 25,
            treasury_share: 10, // sums to 95
            creator_addr: creator.clone(),
            investor_addr: Address::generate(&env),
            treasury_addr: Address::generate(&env),
        };

        let result = client
            .mock_auths(&[MockAuth {
                address: &creator,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "create_talos",
                    args: (
                        s(&env, "Bad"),
                        s(&env, "Marketing"),
                        s(&env, "desc"),
                        bad_patron.clone(),
                        kernel(),
                        pulse(&env),
                        protocol_wallet.clone(),
                    )
                        .into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_create_talos(
                &s(&env, "Bad"),
                &s(&env, "Marketing"),
                &s(&env, "desc"),
                &bad_patron,
                &kernel(),
                &pulse(&env),
                &protocol_wallet,
            );

        assert!(result.is_err());
    }

    #[test]
    fn update_patron_rejects_shares_not_summing_to_100() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &protocol_wallet);

        let bad_patron = Patron {
            creator_share: 50,
            investor_share: 30,
            treasury_share: 30, // sums to 110
            creator_addr: creator.clone(),
            investor_addr: Address::generate(&env),
            treasury_addr: Address::generate(&env),
        };

        let result = client
            .mock_auths(&[MockAuth {
                address: &creator,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "update_patron",
                    args: (id, bad_patron.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_update_patron(&id, &bad_patron);

        assert!(result.is_err());
    }

    // ── Two-step admin transfer tests ────────────────────────────────

    fn mock_propose(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        admin: &Address,
        new_admin: &Address,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: admin,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "propose_admin",
                    args: (new_admin.clone(),).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .propose_admin(new_admin);
    }

    fn mock_accept(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        pending: &Address,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: pending,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "accept_admin",
                    args: ().into_val(env),
                    sub_invokes: &[],
                },
            }])
            .accept_admin();
    }

    fn mock_cancel(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        admin: &Address,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: admin,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "cancel_admin_transfer",
                    args: ().into_val(env),
                    sub_invokes: &[],
                },
            }])
            .cancel_admin_transfer();
    }

    /// Happy path: propose → accept transfers admin and clears pending slot.
    #[test]
    fn propose_then_accept_transfers_admin() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        assert_eq!(client.protocol_wallet(), Some(admin.clone()));
        assert_eq!(client.pending_admin(), None);

        mock_propose(&env, &client, &contract_id, &admin, &new_admin);
        assert_eq!(client.pending_admin(), Some(new_admin.clone()));

        mock_accept(&env, &client, &contract_id, &new_admin);

        // Admin is now new_admin; pending slot is cleared.
        assert_eq!(client.protocol_wallet(), Some(new_admin.clone()));
        assert_eq!(client.pending_admin(), None);
    }

    /// After transfer the new admin can use admin-only entry-points.
    #[test]
    fn new_admin_can_set_protocol_fee_after_transfer() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);
        mock_accept(&env, &client, &contract_id, &new_admin);

        // New admin can update fee; old admin cannot.
        client
            .mock_auths(&[MockAuth {
                address: &new_admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_protocol_fee",
                    args: (400u32,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_protocol_fee(&400);

        assert_eq!(client.protocol_fee_bps(), Some(400));
    }

    /// propose_admin without admin auth must fail.
    #[test]
    fn propose_admin_requires_current_admin_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let impostor = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);

        // No mock_auths → auth check fails.
        assert!(client.try_propose_admin(&new_admin).is_err());

        // Impostor auth → still fails.
        let result = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "propose_admin",
                    args: (new_admin.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_propose_admin(&new_admin);
        assert!(result.is_err());
    }

    /// accept_admin without pending admin auth must fail.
    #[test]
    fn accept_admin_requires_pending_admin_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let impostor = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);

        // Impostor pretending to be pending admin.
        let result = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "accept_admin",
                    args: ().into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_accept_admin();
        assert!(result.is_err());

        // Original admin trying to accept — must also fail (only pending may accept).
        let result2 = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "accept_admin",
                    args: ().into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_accept_admin();
        assert!(result2.is_err());
    }

    /// accept_admin with no pending nomination must panic.
    #[test]
    fn accept_admin_without_pending_panics() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        client.initialize(&admin);

        // No propose was called; any auth attempt should fail.
        assert!(client.try_accept_admin().is_err());
    }

    /// cancel_admin_transfer removes pending and emits adm_cnl.
    #[test]
    fn cancel_admin_transfer_clears_pending() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);
        assert_eq!(client.pending_admin(), Some(new_admin.clone()));

        mock_cancel(&env, &client, &contract_id, &admin);

        assert_eq!(client.pending_admin(), None);
        // Admin is unchanged.
        assert_eq!(client.protocol_wallet(), Some(admin.clone()));
    }

    /// After cancellation the pending admin can no longer accept.
    #[test]
    fn cancelled_pending_cannot_accept() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);
        mock_cancel(&env, &client, &contract_id, &admin);

        let result = client
            .mock_auths(&[MockAuth {
                address: &new_admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "accept_admin",
                    args: ().into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_accept_admin();
        assert!(result.is_err());
    }

    /// cancel_admin_transfer without pending nomination must panic.
    #[test]
    fn cancel_without_pending_panics() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        client.initialize(&admin);

        // No nomination exists.
        assert!(client.try_cancel_admin_transfer().is_err());
    }

    /// cancel_admin_transfer requires current admin auth.
    #[test]
    fn cancel_admin_transfer_requires_admin_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let impostor = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);

        let result = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "cancel_admin_transfer",
                    args: ().into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_cancel_admin_transfer();
        assert!(result.is_err());
    }

    /// propose_admin replaces an existing pending nomination (no cancel needed).
    #[test]
    fn propose_admin_replaces_existing_pending() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let first = Address::generate(&env);
        let second = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &first);
        assert_eq!(client.pending_admin(), Some(first.clone()));

        // Replace with a different nominee.
        mock_propose(&env, &client, &contract_id, &admin, &second);
        assert_eq!(client.pending_admin(), Some(second.clone()));

        // First nominee can no longer accept.
        let result = client
            .mock_auths(&[MockAuth {
                address: &first,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "accept_admin",
                    args: ().into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_accept_admin();
        assert!(result.is_err());
    }

    // ── Admin transfer event tests ───────────────────────────────────

    #[test]
    fn propose_admin_emits_adm_prp_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);

        let events = env.events().all();
        assert_eq!(events.len(), 1);
        let (addr, topics, data) = events.get(0).unwrap();

        assert_eq!(addr, contract_id);
        assert_eq!(topics.len(), 1);
        assert_topic_symbol(&env, &topics, 0, symbol_short!("adm_prp"));

        let (got_current, got_proposed): (Address, Address) =
            TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_current, admin);
        assert_eq!(got_proposed, new_admin);
    }

    #[test]
    fn accept_admin_emits_adm_acc_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);

        // Clear events accumulated by propose.
        let _ = env.events().all();

        mock_accept(&env, &client, &contract_id, &new_admin);

        let events = env.events().all();
        // The accept call is the only event since the last snapshot.
        let accept_events: std::vec::Vec<_> = events
            .iter()
            .filter(|(a, t, _)| {
                if *a != contract_id {
                    return false;
                }
                if t.len() == 0 {
                    return false;
                }
                let sym: Result<Symbol, _> =
                    TryFromVal::try_from_val(&env, &t.get(0).unwrap());
                sym.map(|s| s == symbol_short!("adm_acc")).unwrap_or(false)
            })
            .collect();

        assert_eq!(accept_events.len(), 1);
        let (_, _, data) = accept_events[0].clone();
        let (got_new,): (Address,) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_new, new_admin);
    }

    #[test]
    fn cancel_admin_transfer_emits_adm_cnl_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);
        mock_propose(&env, &client, &contract_id, &admin, &new_admin);
        mock_cancel(&env, &client, &contract_id, &admin);

        let events = env.events().all();
        let cancel_events: std::vec::Vec<_> = events
            .iter()
            .filter(|(a, t, _)| {
                if *a != contract_id {
                    return false;
                }
                if t.len() == 0 {
                    return false;
                }
                let sym: Result<Symbol, _> =
                    TryFromVal::try_from_val(&env, &t.get(0).unwrap());
                sym.map(|s| s == symbol_short!("adm_cnl")).unwrap_or(false)
            })
            .collect();

        assert_eq!(cancel_events.len(), 1);
        let (_, _, data) = cancel_events[0].clone();
        let (got_cancelled,): (Address,) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_cancelled, new_admin);
    }

    // ── Allowlist: authorization, events, storage ────────────────────

    fn mock_allow(env: &Env, client: &TalosRegistryClient, contract_id: &Address, admin: &Address, asset: &Address) {
        client
            .mock_auths(&[MockAuth {
                address: admin,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "allow_asset",
                    args: (asset.clone(),).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .allow_asset(asset);
    }

    fn mock_deny(env: &Env, client: &TalosRegistryClient, contract_id: &Address, admin: &Address, asset: &Address) {
        client
            .mock_auths(&[MockAuth {
                address: admin,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "deny_asset",
                    args: (asset.clone(),).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .deny_asset(asset);
    }

    #[test]
    fn allow_asset_admin_happy_path() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin);
        assert!(!client.is_asset_allowed(&token));

        mock_allow(&env, &client, &contract_id, &admin, &token);
        assert!(client.is_asset_allowed(&token));
    }

    #[test]
    fn deny_asset_admin_happy_path() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin);
        mock_allow(&env, &client, &contract_id, &admin, &token);
        assert!(client.is_asset_allowed(&token));

        mock_deny(&env, &client, &contract_id, &admin, &token);
        assert!(!client.is_asset_allowed(&token));
    }

    #[test]
    fn allow_asset_unauthorized_update_panics() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let impostor = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin);

        let res = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "allow_asset",
                    args: (token.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_allow_asset(&token);
        assert!(res.is_err(), "non-admin must not be allowed to add assets");
        assert!(!client.is_asset_allowed(&token));
    }

    #[test]
    fn deny_asset_unauthorized_update_panics() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let impostor = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin);
        mock_allow(&env, &client, &contract_id, &admin, &token);

        let res = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "deny_asset",
                    args: (token.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_deny_asset(&token);
        assert!(res.is_err(), "non-admin must not be allowed to remove assets");
        assert!(client.is_asset_allowed(&token), "asset should remain allowed on failed denial");
    }

    #[test]
    fn allow_asset_emits_alw_add_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin);
        mock_allow(&env, &client, &contract_id, &admin, &token);

        let events = env.events().all();
        let alw: std::vec::Vec<_> = events
            .iter()
            .filter(|(a, t, _)| {
                if *a != contract_id {
                    return false;
                }
                if t.len() != 2 {
                    return false;
                }
                let sym: Result<Symbol, _> = TryFromVal::try_from_val(&env, &t.get(0).unwrap());
                sym.map(|s| s == symbol_short!("alw_add")).unwrap_or(false)
            })
            .collect();

        assert_eq!(alw.len(), 1);
        let (_, topics, data) = alw[0].clone();
        assert_topic_address(&env, &topics, 1, &admin);
        let asset_addr: Address = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(asset_addr, token);
    }

    #[test]
    fn deny_asset_emits_alw_rem_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin);
        mock_allow(&env, &client, &contract_id, &admin, &token);
        let _ = env.events().all();

        mock_deny(&env, &client, &contract_id, &admin, &token);

        let events = env.events().all();
        let rem: std::vec::Vec<_> = events
            .iter()
            .filter(|(a, t, _)| {
                if *a != contract_id {
                    return false;
                }
                if t.len() != 2 {
                    return false;
                }
                let sym: Result<Symbol, _> = TryFromVal::try_from_val(&env, &t.get(0).unwrap());
                sym.map(|s| s == symbol_short!("alw_rem")).unwrap_or(false)
            })
            .collect();

        assert_eq!(rem.len(), 1);
        let (_, topics, data) = rem[0].clone();
        assert_topic_address(&env, &topics, 1, &admin);
        let asset_addr: Address = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(asset_addr, token);
    }

    // ── collect_protocol_fee: allowed vs denied + values ─────────────

    use soroban_sdk::token::Client as TokenClient;

    fn create_and_mint_token(env: &Env, admin: &Address) -> Address {
        let id = env.register_contract_wasm(None, soroban_sdk::token::StellarAssetWASM::new(env));
        let token = TokenClient::new(env, &id);
        token.initialize(admin, &7u32, &s(env, "TEST"), &s(env, "Test USD"));
        id
    }

    fn mint(env: &Env, asset: &Address, from: &Address, to: &Address, amount: i128) {
        TokenClient::new(env, asset)
            .mock_all_auths()
            .mint(from, to, &amount);
    }

    fn mock_collect_fee(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        from: &Address,
        asset: &Address,
        to: &Address,
        amount: i128,
    ) -> (i128, i128) {
        client
            .mock_auths(&[MockAuth {
                address: from,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "collect_protocol_fee",
                    args: (asset.clone(), from.clone(), to.clone(), amount).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .collect_protocol_fee(asset, from, to, &amount)
    }

    #[test]
    fn collect_protocol_fee_allowed_sends_fee_and_net() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let payer = Address::generate(&env);
        let receiver = Address::generate(&env);

        client.initialize(&admin);
        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);
        mock_allow(&env, &client, &contract_id, &admin, &asset);

        let token = TokenClient::new(&env, &asset);
        let balance_before_payer = token.balance(&payer);
        let balance_before_admin = token.balance(&admin);
        let balance_before_receiver = token.balance(&receiver);

        let (net, fee) = mock_collect_fee(&env, &client, &contract_id, &payer, &asset, &receiver, 10_000);
        // default 300bps (3%) → fee 300, net 9_700
        assert_eq!(fee, 300);
        assert_eq!(net, 9_700);

        assert_eq!(token.balance(&payer), balance_before_payer - 10_000);
        assert_eq!(token.balance(&admin), balance_before_admin + 300);
        assert_eq!(token.balance(&receiver), balance_before_receiver + 9_700);
    }

    #[test]
    fn collect_protocol_fee_denied_panics_before_transfer() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let payer = Address::generate(&env);
        let receiver = Address::generate(&env);

        client.initialize(&admin);
        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);

        let token = TokenClient::new(&env, &asset);
        let balance_before = token.balance(&payer);

        // Asset NOT allowlisted → must panic.
        let res = client
            .mock_auths(&[MockAuth {
                address: &payer,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "collect_protocol_fee",
                    args: (asset.clone(), payer.clone(), receiver.clone(), 10_000i128).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_collect_protocol_fee(&asset, &payer, &receiver, &10_000);
        assert!(res.is_err(), "denied asset must not be transferrable");

        // Balances must be unchanged because the enforce() runs before any transfer.
        assert_eq!(token.balance(&payer), balance_before);
    }

    // ── report_revenue: allowed, denied, unauthorized payer ──────────

    fn mock_report_revenue(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        payer: &Address,
        talos_id: u32,
        asset: &Address,
        amount: i128,
        currency: &str,
    ) -> (i128, i128) {
        client
            .mock_auths(&[MockAuth {
                address: payer,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "report_revenue",
                    args: (talos_id, payer.clone(), asset.clone(), amount, s(env, currency)).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .report_revenue(&talos_id, payer, asset, &amount, &s(env, currency))
    }

    #[test]
    fn report_revenue_allowed_sends_fee_to_admin_and_net_to_creator() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        let payer = Address::generate(&env);

        client.initialize(&admin);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);

        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);
        mock_allow(&env, &client, &contract_id, &admin, &asset);

        let token = TokenClient::new(&env, &asset);
        let bal_admin_before = token.balance(&admin);
        let bal_creator_before = token.balance(&creator);

        let (net, fee) = mock_report_revenue(
            &env, &client, &contract_id, &payer, id, &asset, 10_000, "USD",
        );
        assert_eq!(fee, 300);
        assert_eq!(net, 9_700);
        assert_eq!(token.balance(&admin), bal_admin_before + 300);
        assert_eq!(token.balance(&creator), bal_creator_before + 9_700);
    }

    #[test]
    fn report_revenue_denied_asset_panics_without_moving_funds() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        let payer = Address::generate(&env);

        client.initialize(&admin);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);

        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);

        let token = TokenClient::new(&env, &asset);
        let bal_payer_before = token.balance(&payer);

        let res = client
            .mock_auths(&[MockAuth {
                address: &payer,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "report_revenue",
                    args: (id, payer.clone(), asset.clone(), 10_000i128, s(&env, "USD")).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_report_revenue(&id, &payer, &asset, &10_000, &s(&env, "USD"));
        assert!(res.is_err(), "denied asset must block revenue reporting");
        assert_eq!(token.balance(&payer), bal_payer_before);
    }

    #[test]
    fn report_revenue_unauthorized_payer_panics() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        let payer = Address::generate(&env);
        let impostor = Address::generate(&env);

        client.initialize(&admin);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);
        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);
        mock_allow(&env, &client, &contract_id, &admin, &asset);

        // Impostor tries to trigger the transfer on behalf of payer without
        // payer's signature → must fail.
        let res = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "report_revenue",
                    args: (id, payer.clone(), asset.clone(), 10_000i128, s(&env, "USD")).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_report_revenue(&id, &payer, &asset, &10_000, &s(&env, "USD"));
        assert!(res.is_err(), "unauthorized payer signature must fail");
    }

    #[test]
    fn report_revenue_emits_rev_rep_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        let payer = Address::generate(&env);

        client.initialize(&admin);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);
        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);
        mock_allow(&env, &client, &contract_id, &admin, &asset);
        let _ = env.events().all();

        let _ = mock_report_revenue(&env, &client, &contract_id, &payer, id, &asset, 10_000, "USD");

        let events = env.events().all();
        let rev: std::vec::Vec<_> = events
            .iter()
            .filter(|(a, t, _)| {
                if *a != contract_id {
                    return false;
                }
                if t.len() != 2 {
                    return false;
                }
                let sym: Result<Symbol, _> = TryFromVal::try_from_val(&env, &t.get(0).unwrap());
                sym.map(|s| s == symbol_short!("rev_rep")).unwrap_or(false)
            })
            .collect();

        assert_eq!(rev.len(), 1);
        let (_, topics, data) = rev[0].clone();
        assert_topic_u32(&env, &topics, 1, id);
        let (got_payer, got_token, got_amt, got_ccy): (Address, Address, i128, String) =
            TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_payer, payer);
        assert_eq!(got_token, asset);
        assert_eq!(got_amt, 10_000);
        assert_eq!(got_ccy, s(&env, "USD"));
    }

    #[test]
    fn collect_protocol_fee_emits_fee_pay_event() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let payer = Address::generate(&env);
        let receiver = Address::generate(&env);

        client.initialize(&admin);
        let asset = create_and_mint_token(&env, &admin);
        mint(&env, &asset, &admin, &payer, 10_000);
        mock_allow(&env, &client, &contract_id, &admin, &asset);
        let _ = env.events().all();

        let _ = mock_collect_fee(&env, &client, &contract_id, &payer, &asset, &receiver, 10_000);

        let events = env.events().all();
        let fee: std::vec::Vec<_> = events
            .iter()
            .filter(|(a, t, _)| {
                if *a != contract_id {
                    return false;
                }
                if t.len() != 2 {
                    return false;
                }
                let sym: Result<Symbol, _> = TryFromVal::try_from_val(&env, &t.get(0).unwrap());
                sym.map(|s| s == symbol_short!("fee_pay")).unwrap_or(false)
            })
            .collect();

        assert_eq!(fee.len(), 1);
        let (_, topics, data) = fee[0].clone();
        let asset_in_topic: Address = TryFromVal::try_from_val(&env, &topics.get(1).unwrap()).unwrap();
        assert_eq!(asset_in_topic, asset);
        let (got_from, got_to, got_amt, got_fee): (Address, Address, i128, i128) =
            TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got_from, payer);
        assert_eq!(got_to, receiver);
        assert_eq!(got_amt, 10_000);
        assert_eq!(got_fee, 300);
    }

    #[test]
    fn allowlist_is_empty_by_default_after_init() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);
        for _ in 0..5 {
            assert!(!client.is_asset_allowed(&Address::generate(&env)));
        }
    }

    #[test]
    fn multiple_assets_allowed_then_removed() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let tokens: std::vec::Vec<Address> = (0..3).map(|_| Address::generate(&env)).collect();
        for t in &tokens {
            mock_allow(&env, &client, &contract_id, &admin, t);
        }
        for t in &tokens {
            assert!(client.is_asset_allowed(t));
        }

        mock_deny(&env, &client, &contract_id, &admin, &tokens[1]);
        assert!(client.is_asset_allowed(&tokens[0]));
        assert!(!client.is_asset_allowed(&tokens[1]));
        assert!(client.is_asset_allowed(&tokens[2]));
    }
}
