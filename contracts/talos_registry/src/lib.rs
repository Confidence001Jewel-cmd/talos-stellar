//! TalosRegistry — Soroban smart contract for Talos Protocol.
//!
//! Handles:
//! - Talos creation (with Pulse token metadata)
//! - Protocol fee collection (3% launchpad fee)
//! - Talos metadata storage and retrieval
//! - Patron registration with minimum Pulse holding validation

#![no_std]

#[cfg(all(test, not(target_arch = "wasm32")))]
extern crate std;

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, Address, Env, String, Vec,
};

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminAction {
    SetProtocolFee(u32),
    ProposeAdmin(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProposalStatus {
    Scheduled,
    Executed,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelockProposal {
    pub id: u64,
    pub action: AdminAction,
    pub eta: u64,
    pub status: ProposalStatus,
    pub scheduled_at: u64,
    pub scheduled_by: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelockConfig {
    pub min_delay: u64,
    pub grace_period: u64,
}

/// A narrowly-scoped write path that can be independently paused.
///
/// Reads are never gated by any pause domain — `get_talos`, `is_active`,
/// `next_talos_id`, etc. always reflect current state regardless of pause
/// status. Administrative entry-points (`set_protocol_fee`, admin transfer,
/// timelock management) are intentionally **not** pausable: they already
/// carry their own admin-auth (and optional timelock) protection, and
/// making them pausable would risk an admin permanently locking itself out.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PauseDomain {
    TalosCreation,
    PatronUpdates,
    KernelUpdates,
    PulseUpdates,
    Deactivation,
}

/// Persisted record of an active pause on a single [`PauseDomain`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused_by: Address,
    pub paused_at: u64,
    /// Unix timestamp after which the pause automatically lifts.
    /// `0` means indefinite — only settable by the admin, never by a guardian.
    pub expires_at: u64,
}

/// Computed, read-friendly view of a domain's pause status.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseInfo {
    pub active: bool,
    pub paused_by: Option<Address>,
    pub paused_at: Option<u64>,
    /// `None` when not paused, or when paused indefinitely by the admin.
    pub expires_at: Option<u64>,
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
    TimelockConfig,
    TimelockProposal(u64),
    NextTimelockId,
    /// Active pause record for a domain, if any. Absence means unpaused.
    PauseState(PauseDomain),
    /// Bounded list of guardian addresses authorized to trigger (but not lift) pauses.
    Guardians,
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
//   tl_sch  : (symbol, proposal_id: u64)   → (action: AdminAction, eta: u64, proposer: Address)
//   tl_exec : (symbol, proposal_id: u64)   → (action: AdminAction, executor: Address)
//   tl_cnl  : (symbol, proposal_id: u64)   → (action: AdminAction, canceller: Address)
//   tl_cfg  : (symbol,)                    → (old_min_delay: u64, new_min_delay: u64, grace_period: u64)
//   pause_on  : (symbol, domain: PauseDomain) → (actor: Address, expires_at: u64)
//   pause_off : (symbol, domain: PauseDomain) → (actor: Address,)
//   guard_add : (symbol,)                     → (guardian: Address,)
//   guard_rem : (symbol,)                     → (guardian: Address,)

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

/// Emitted when the current admin cancels an in-progress transfer.
/// `cancelled` is the address whose nomination was revoked.
fn emit_admin_cancelled(env: &Env, cancelled: Address) {
    let topics = (symbol_short!("adm_cnl"),);
    env.events().publish(topics, (cancelled,));
}

fn emit_timelock_scheduled(
    env: &Env,
    proposal_id: u64,
    action: &AdminAction,
    eta: u64,
    proposer: Address,
) {
    let topics = (symbol_short!("tl_sch"), proposal_id);
    env.events().publish(topics, (action.clone(), eta, proposer));
}

fn emit_timelock_executed(
    env: &Env,
    proposal_id: u64,
    action: &AdminAction,
    executor: Address,
) {
    let topics = (symbol_short!("tl_exec"), proposal_id);
    env.events().publish(topics, (action.clone(), executor));
}

fn emit_timelock_cancelled(
    env: &Env,
    proposal_id: u64,
    action: &AdminAction,
    canceller: Address,
) {
    let topics = (symbol_short!("tl_cnl"), proposal_id);
    env.events().publish(topics, (action.clone(), canceller));
}

fn emit_timelock_config_changed(
    env: &Env,
    old_min_delay: u64,
    new_min_delay: u64,
    grace_period: u64,
) {
    let topics = (symbol_short!("tl_cfg"),);
    env.events()
        .publish(topics, (old_min_delay, new_min_delay, grace_period));
}

/// Emitted when a domain is paused (or an existing pause is refreshed/extended).
fn emit_pause_set(env: &Env, domain: &PauseDomain, actor: &Address, expires_at: u64) {
    let topics = (symbol_short!("pause_on"), domain.clone());
    env.events().publish(topics, (actor.clone(), expires_at));
}

/// Emitted when an admin lifts an active pause on a domain.
fn emit_pause_cleared(env: &Env, domain: &PauseDomain, actor: &Address) {
    let topics = (symbol_short!("pause_off"), domain.clone());
    env.events().publish(topics, (actor.clone(),));
}

fn emit_guardian_added(env: &Env, guardian: Address) {
    let topics = (symbol_short!("guard_add"),);
    env.events().publish(topics, (guardian,));
}

fn emit_guardian_removed(env: &Env, guardian: Address) {
    let topics = (symbol_short!("guard_rem"),);
    env.events().publish(topics, (guardian,));
}

// ── Helpers ─────────────────────────────────────────────────────────

fn validate_patron_shares(patron: &Patron) {
    let total = patron.creator_share + patron.investor_share + patron.treasury_share;
    if total != 100 {
        panic!("Patron shares must sum to 100");
    }
}

// ── Emergency Pause Helpers ────────────────────────────────────────

fn get_guardians(e: &Env) -> Vec<Address> {
    e.storage()
        .persistent()
        .get(&DataKey::Guardians)
        .unwrap_or(Vec::new(e))
}

fn is_guardian_internal(e: &Env, addr: &Address) -> bool {
    get_guardians(e).iter().any(|g| g == *addr)
}

/// Evaluate whether `domain` is currently paused, lazily treating an expired
/// (non-indefinite) pause record as inactive without mutating storage.
fn is_domain_paused(e: &Env, domain: &PauseDomain) -> bool {
    match e
        .storage()
        .persistent()
        .get::<_, PauseState>(&DataKey::PauseState(domain.clone()))
    {
        None => false,
        Some(state) => state.expires_at == 0 || e.ledger().timestamp() < state.expires_at,
    }
}

fn require_not_paused(e: &Env, domain: PauseDomain) {
    if is_domain_paused(e, &domain) {
        panic!("Write path paused");
    }
}

// ── Constants ───────────────────────────────────────────────────────

const PROTOCOL_FEE_BPS: u32 = 300; // 3%
const MAX_PROTOCOL_FEE_BPS: u32 = 10_000; // 100%
const DEFAULT_GRACE_PERIOD: u64 = 604_800; // 7 days in seconds
const MAX_MIN_DELAY: u64 = 2_592_000; // 30 days in seconds
const MAX_GUARDIAN_PAUSE_SECS: u64 = 604_800; // 7 days — bounds guardian blast radius
const MAX_ADMIN_PAUSE_SECS: u64 = 2_592_000; // 30 days; 0 (indefinite) is also allowed for admin
const MAX_GUARDIANS: u32 = 10; // bounds unbounded storage growth

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
pub const CONTRACT_VERSION: (u32, u32, u32) = (1, 2, 0);

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
        require_not_paused(&e, PauseDomain::TalosCreation);

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
        require_not_paused(&e, PauseDomain::PatronUpdates);

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
        require_not_paused(&e, PauseDomain::KernelUpdates);

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
        require_not_paused(&e, PauseDomain::PulseUpdates);

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
        require_not_paused(&e, PauseDomain::Deactivation);

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

    fn set_protocol_fee_internal(e: &Env, fee_bps: u32) {
        if fee_bps > MAX_PROTOCOL_FEE_BPS {
            panic!("Protocol fee cannot exceed 100%");
        }

        let old_bps: u32 = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(PROTOCOL_FEE_BPS);

        e.storage()
            .persistent()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);

        emit_protocol_fee_changed(e, old_bps, fee_bps);
    }

    fn propose_admin_internal(e: &Env, current: Address, new_admin: Address) {
        e.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin);

        emit_admin_proposed(e, current, new_admin);
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

        let config = Self::get_timelock_config(e.clone());
        if config.min_delay > 0 {
            panic!("Timelock enabled: action must be scheduled");
        }

        Self::set_protocol_fee_internal(&e, fee_bps);
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

        let config = Self::get_timelock_config(e.clone());
        if config.min_delay > 0 {
            panic!("Timelock enabled: action must be scheduled");
        }

        Self::propose_admin_internal(&e, current, new_admin);
    }

    // ── Timelock Administration ─────────────────────────────────────

    /// Configure timelock parameter settings (`min_delay` and `grace_period`).
    ///
    /// # Authorization
    /// Requires current protocol wallet (admin) authorization.
    pub fn set_timelock_config(e: Env, min_delay: u64, grace_period: u64) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        if grace_period == 0 {
            panic!("Grace period must be positive");
        }
        if min_delay > MAX_MIN_DELAY {
            panic!("Min delay exceeds maximum limit");
        }

        let old_config = Self::get_timelock_config(e.clone());

        let new_config = TimelockConfig {
            min_delay,
            grace_period,
        };

        e.storage()
            .persistent()
            .set(&DataKey::TimelockConfig, &new_config);

        emit_timelock_config_changed(&e, old_config.min_delay, min_delay, grace_period);
    }

    /// Retrieve active timelock configuration.
    pub fn get_timelock_config(e: Env) -> TimelockConfig {
        e.storage()
            .persistent()
            .get(&DataKey::TimelockConfig)
            .unwrap_or(TimelockConfig {
                min_delay: 0,
                grace_period: DEFAULT_GRACE_PERIOD,
            })
    }

    /// Schedule a high-impact administrative action for future execution.
    ///
    /// # Authorization
    /// Requires current protocol wallet (admin) authorization.
    pub fn schedule_action(e: Env, action: AdminAction, delay: u64) -> u64 {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let config = Self::get_timelock_config(e.clone());
        if delay < config.min_delay {
            panic!("Delay less than minimum required delay");
        }

        let id: u64 = e
            .storage()
            .persistent()
            .get(&DataKey::NextTimelockId)
            .unwrap_or(1);

        let now = e.ledger().timestamp();
        let eta = now.saturating_add(delay);

        let proposal = TimelockProposal {
            id,
            action: action.clone(),
            eta,
            status: ProposalStatus::Scheduled,
            scheduled_at: now,
            scheduled_by: admin.clone(),
        };

        e.storage()
            .persistent()
            .set(&DataKey::TimelockProposal(id), &proposal);
        e.storage()
            .persistent()
            .set(&DataKey::NextTimelockId, &(id + 1));

        emit_timelock_scheduled(&e, id, &action, eta, admin);
        id
    }

    /// Execute a scheduled administrative action after its timelock ETA has matured.
    pub fn execute_action(e: Env, proposal_id: u64) {
        let mut proposal: TimelockProposal = e
            .storage()
            .persistent()
            .get(&DataKey::TimelockProposal(proposal_id))
            .expect("Timelock proposal not found");

        if proposal.status != ProposalStatus::Scheduled {
            panic!("Proposal not active");
        }

        let now = e.ledger().timestamp();
        if now < proposal.eta {
            panic!("Timelock delay not met");
        }

        let config = Self::get_timelock_config(e.clone());
        if now > proposal.eta.saturating_add(config.grace_period) {
            panic!("Proposal expired");
        }

        match &proposal.action {
            AdminAction::SetProtocolFee(fee_bps) => {
                Self::set_protocol_fee_internal(&e, *fee_bps);
            }
            AdminAction::ProposeAdmin(new_admin) => {
                let current: Address = e
                    .storage()
                    .persistent()
                    .get(&DataKey::ProtocolWallet)
                    .expect("Contract not initialized");
                Self::propose_admin_internal(&e, current, new_admin.clone());
            }
        }

        proposal.status = ProposalStatus::Executed;
        e.storage()
            .persistent()
            .set(&DataKey::TimelockProposal(proposal_id), &proposal);

        let executor = proposal.scheduled_by.clone();
        emit_timelock_executed(&e, proposal_id, &proposal.action, executor);
    }

    /// Cancel a scheduled administrative proposal before it is executed.
    ///
    /// # Authorization
    /// Requires current protocol wallet (admin) authorization.
    pub fn cancel_action(e: Env, proposal_id: u64) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let mut proposal: TimelockProposal = e
            .storage()
            .persistent()
            .get(&DataKey::TimelockProposal(proposal_id))
            .expect("Timelock proposal not found");

        if proposal.status != ProposalStatus::Scheduled {
            panic!("Proposal not active");
        }

        proposal.status = ProposalStatus::Cancelled;
        e.storage()
            .persistent()
            .set(&DataKey::TimelockProposal(proposal_id), &proposal);

        emit_timelock_cancelled(&e, proposal_id, &proposal.action, admin);
    }

    /// Retrieve a timelock proposal by ID.
    pub fn get_timelock_proposal(e: Env, proposal_id: u64) -> Option<TimelockProposal> {
        e.storage()
            .persistent()
            .get(&DataKey::TimelockProposal(proposal_id))
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

    // ── Emergency Pause Controls ──────────────────────────────────────

    /// Pause a single write-path domain, blocking it while leaving all
    /// reads and every other domain fully functional.
    ///
    /// # Authorization
    /// `caller` must be either the current admin (`ProtocolWallet`) or a
    /// registered guardian, and must sign the transaction.
    ///
    /// # Duration semantics
    /// - Admin: `duration = 0` pauses indefinitely (only the admin can lift
    ///   it later via `unpause`). A non-zero `duration` must not exceed
    ///   [`MAX_ADMIN_PAUSE_SECS`].
    /// - Guardian: `duration` must be non-zero and at most
    ///   [`MAX_GUARDIAN_PAUSE_SECS`] — guardians can never impose an
    ///   indefinite pause, bounding the damage a compromised guardian key
    ///   can do.
    ///
    /// Calling `pause` again on an already-paused domain refreshes
    /// (overwrites) the existing record — this is intentionally idempotent
    /// so a retried or duplicated transaction is safe. The one exception:
    /// a guardian may never overwrite a pause that the admin established
    /// (see `Panics`), preventing a guardian from weakening an admin lock.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    /// - `"Caller is not admin or guardian"` — unauthorized caller.
    /// - `"Domain locked by admin; guardians cannot modify"` — a guardian
    ///   attempted to overwrite an admin-established pause.
    /// - `"Duration exceeds admin maximum"` / `"Guardian pause duration out of bounds"`.
    pub fn pause(e: Env, caller: Address, domain: PauseDomain, duration: u64) {
        caller.require_auth();

        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");

        let is_admin_caller = caller == admin;
        if !is_admin_caller && !is_guardian_internal(&e, &caller) {
            panic!("Caller is not admin or guardian");
        }

        let key = DataKey::PauseState(domain.clone());

        if !is_admin_caller {
            if let Some(existing) = e.storage().persistent().get::<_, PauseState>(&key) {
                if existing.paused_by == admin {
                    panic!("Domain locked by admin; guardians cannot modify");
                }
            }
        }

        let now = e.ledger().timestamp();
        let expires_at = if is_admin_caller {
            if duration == 0 {
                0
            } else {
                if duration > MAX_ADMIN_PAUSE_SECS {
                    panic!("Duration exceeds admin maximum");
                }
                now.saturating_add(duration)
            }
        } else {
            if duration == 0 || duration > MAX_GUARDIAN_PAUSE_SECS {
                panic!("Guardian pause duration out of bounds");
            }
            now.saturating_add(duration)
        };

        e.storage().persistent().set(
            &key,
            &PauseState {
                paused_by: caller.clone(),
                paused_at: now,
                expires_at,
            },
        );

        emit_pause_set(&e, &domain, &caller, expires_at);
    }

    /// Lift an active pause on `domain`.
    ///
    /// Idempotent: if the domain has no active pause record (never paused,
    /// already lifted, or already expired), this is a deterministic no-op —
    /// it does not panic, so duplicate or retried `unpause` calls are safe.
    ///
    /// # Authorization
    /// Only the current admin (`ProtocolWallet`) may unpause, regardless of
    /// whether the pause was originally set by the admin or a guardian.
    ///
    /// # Panics
    /// - `"Contract not initialized"` — if `initialize` has not been called.
    pub fn unpause(e: Env, domain: PauseDomain) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let key = DataKey::PauseState(domain.clone());
        if e.storage().persistent().get::<_, PauseState>(&key).is_none() {
            return;
        }

        e.storage().persistent().remove(&key);
        emit_pause_cleared(&e, &domain, &admin);
    }

    /// Returns `true` if `domain` is currently paused (and its pause has not
    /// yet expired). Never requires authorization — this is a read.
    pub fn is_paused(e: Env, domain: PauseDomain) -> bool {
        is_domain_paused(&e, &domain)
    }

    /// Full observability view of a domain's pause status: who paused it,
    /// when, and when it expires (or `None` for indefinite/unpaused).
    pub fn pause_info(e: Env, domain: PauseDomain) -> PauseInfo {
        match e
            .storage()
            .persistent()
            .get::<_, PauseState>(&DataKey::PauseState(domain.clone()))
        {
            None => PauseInfo {
                active: false,
                paused_by: None,
                paused_at: None,
                expires_at: None,
            },
            Some(state) => {
                let active = state.expires_at == 0 || e.ledger().timestamp() < state.expires_at;
                PauseInfo {
                    active,
                    paused_by: Some(state.paused_by),
                    paused_at: Some(state.paused_at),
                    expires_at: if state.expires_at == 0 {
                        None
                    } else {
                        Some(state.expires_at)
                    },
                }
            }
        }
    }

    /// Register a guardian address authorized to trigger (but never lift)
    /// bounded-duration pauses. Bounded to [`MAX_GUARDIANS`] entries.
    ///
    /// Idempotent: adding an already-registered guardian is a no-op.
    ///
    /// # Authorization
    /// Requires current admin (`ProtocolWallet`) authorization.
    pub fn add_guardian(e: Env, guardian: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let mut guardians = get_guardians(&e);
        if guardians.iter().any(|g| g == guardian) {
            return;
        }
        if guardians.len() >= MAX_GUARDIANS {
            panic!("Guardian limit reached");
        }

        guardians.push_back(guardian.clone());
        e.storage().persistent().set(&DataKey::Guardians, &guardians);
        emit_guardian_added(&e, guardian);
    }

    /// Revoke a guardian's pause authority.
    ///
    /// Idempotent: removing an address that is not a guardian is a no-op.
    /// Does not affect any pause the guardian already set — use `unpause`
    /// (admin-only) to lift it explicitly.
    ///
    /// # Authorization
    /// Requires current admin (`ProtocolWallet`) authorization.
    pub fn remove_guardian(e: Env, guardian: Address) {
        let admin: Address = e
            .storage()
            .persistent()
            .get(&DataKey::ProtocolWallet)
            .expect("Contract not initialized");
        admin.require_auth();

        let guardians = get_guardians(&e);
        let mut remaining = Vec::new(&e);
        let mut found = false;
        for g in guardians.iter() {
            if g == guardian {
                found = true;
                continue;
            }
            remaining.push_back(g);
        }

        if !found {
            return;
        }

        e.storage().persistent().set(&DataKey::Guardians, &remaining);
        emit_guardian_removed(&e, guardian);
    }

    /// Check whether `addr` currently holds guardian pause authority.
    pub fn is_guardian(e: Env, addr: Address) -> bool {
        is_guardian_internal(&e, &addr)
    }

    /// List all currently registered guardians (bounded to `MAX_GUARDIANS`).
    pub fn list_guardians(e: Env) -> Vec<Address> {
        get_guardians(&e)
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
        assert_eq!(client.version(), (1u32, 2u32, 0u32));
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

    // ── Timelock unit tests ──────────────────────────────────────────

    use soroban_sdk::testutils::Ledger as _;

    #[test]
    fn timelock_config_defaults_and_updates() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let default_cfg = client.get_timelock_config();
        assert_eq!(default_cfg.min_delay, 0);
        assert_eq!(default_cfg.grace_period, 604_800);

        // Non-admin cannot set config
        let impostor = Address::generate(&env);
        let err_res = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_timelock_config",
                    args: (3600u64, 86400u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_set_timelock_config(&3600, &86400);
        assert!(err_res.is_err());

        // Admin updates config
        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_timelock_config",
                    args: (3600u64, 86400u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_timelock_config(&3600, &86400);

        let updated = client.get_timelock_config();
        assert_eq!(updated.min_delay, 3600);
        assert_eq!(updated.grace_period, 86400);
    }

    #[test]
    fn timelock_schedule_execute_happy_path() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_timelock_config",
                    args: (3600u64, 86400u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_timelock_config(&3600, &86400);

        let action = AdminAction::SetProtocolFee(500);

        let proposal_id = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "schedule_action",
                    args: (action.clone(), 3600u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .schedule_action(&action, &3600);

        assert_eq!(proposal_id, 1);

        let prop = client.get_timelock_proposal(&proposal_id).unwrap();
        assert_eq!(prop.status, ProposalStatus::Scheduled);
        assert_eq!(prop.eta, env.ledger().timestamp() + 3600);

        // Advance ledger timestamp to ETA
        env.ledger().with_mut(|li| {
            li.timestamp += 3600;
        });

        client.execute_action(&proposal_id);

        let executed_prop = client.get_timelock_proposal(&proposal_id).unwrap();
        assert_eq!(executed_prop.status, ProposalStatus::Executed);
        assert_eq!(client.protocol_fee_bps(), Some(500));
    }

    #[test]
    fn timelock_schedule_propose_admin_happy_path() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize(&admin);

        let action = AdminAction::ProposeAdmin(new_admin.clone());

        let proposal_id = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "schedule_action",
                    args: (action.clone(), 0u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .schedule_action(&action, &0);

        client.execute_action(&proposal_id);
        assert_eq!(client.pending_admin(), Some(new_admin.clone()));

        mock_accept(&env, &client, &contract_id, &new_admin);
        assert_eq!(client.protocol_wallet(), Some(new_admin));
    }

    #[test]
    fn timelock_early_execution_fails() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_timelock_config",
                    args: (3600u64, 86400u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_timelock_config(&3600, &86400);

        let action = AdminAction::SetProtocolFee(400);
        let proposal_id = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "schedule_action",
                    args: (action.clone(), 3600u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .schedule_action(&action, &3600);

        // Attempt execution immediately before ETA
        let res = client.try_execute_action(&proposal_id);
        assert!(res.is_err());
    }

    #[test]
    fn timelock_expired_execution_fails() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_timelock_config",
                    args: (100u64, 500u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_timelock_config(&100, &500);

        let action = AdminAction::SetProtocolFee(400);
        let proposal_id = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "schedule_action",
                    args: (action.clone(), 100u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .schedule_action(&action, &100);

        // Advance ledger timestamp beyond eta + grace_period (100 + 500 + 1 = 601)
        env.ledger().with_mut(|li| {
            li.timestamp += 601;
        });

        let res = client.try_execute_action(&proposal_id);
        assert!(res.is_err());
    }

    #[test]
    fn timelock_cancellation_clears_proposal() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let action = AdminAction::SetProtocolFee(400);
        let proposal_id = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "schedule_action",
                    args: (action.clone(), 0u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .schedule_action(&action, &0);

        // Admin cancels action
        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "cancel_action",
                    args: (proposal_id,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .cancel_action(&proposal_id);

        let cancelled = client.get_timelock_proposal(&proposal_id).unwrap();
        assert_eq!(cancelled.status, ProposalStatus::Cancelled);

        // Execution of cancelled proposal must fail
        assert!(client.try_execute_action(&proposal_id).is_err());
    }

    #[test]
    fn timelock_direct_admin_calls_rejected_when_min_delay_active() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_timelock_config",
                    args: (3600u64, 86400u64).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_timelock_config(&3600, &86400);

        // Direct set_protocol_fee call should fail because min_delay > 0
        let res = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "set_protocol_fee",
                    args: (500u32,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_set_protocol_fee(&500);
        assert!(res.is_err());

        // Direct propose_admin call should fail because min_delay > 0
        let new_admin = Address::generate(&env);
        let res_prop = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "propose_admin",
                    args: (new_admin.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_propose_admin(&new_admin);
        assert!(res_prop.is_err());
    }

    // ── Emergency Pause: helpers ───────────────────────────────────────

    fn mock_pause(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        caller: &Address,
        domain: &PauseDomain,
        duration: u64,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: caller,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "pause",
                    args: (caller.clone(), domain.clone(), duration).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .pause(caller, domain, &duration);
    }

    /// Returns `true` if the mocked `pause` call succeeded, `false` if it errored.
    fn try_mock_pause(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        caller: &Address,
        domain: &PauseDomain,
        duration: u64,
    ) -> bool {
        client
            .mock_auths(&[MockAuth {
                address: caller,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "pause",
                    args: (caller.clone(), domain.clone(), duration).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .try_pause(caller, domain, &duration)
            .is_ok()
    }

    fn mock_unpause(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        admin: &Address,
        domain: &PauseDomain,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: admin,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "unpause",
                    args: (domain.clone(),).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .unpause(domain);
    }

    fn mock_add_guardian(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        admin: &Address,
        guardian: &Address,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: admin,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "add_guardian",
                    args: (guardian.clone(),).into_val(env),
                    sub_invokes: &[],
                },
            }])
            .add_guardian(guardian);
    }

    // ── Emergency Pause: scoping ────────────────────────────────────────

    #[test]
    fn pause_blocks_only_the_targeted_domain() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        client.initialize(&admin);

        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);

        mock_pause(
            &env,
            &client,
            &contract_id,
            &admin,
            &PauseDomain::TalosCreation,
            0,
        );

        // Talos creation is blocked...
        let ok = try_mock_pause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation, 0);
        assert!(ok); // re-pausing (refresh) is allowed
        assert!(client.is_paused(&PauseDomain::TalosCreation));
        assert!(client
            .mock_auths(&[MockAuth {
                address: &creator,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "create_talos",
                    args: (
                        s(&env, "Second"),
                        s(&env, "Marketing"),
                        s(&env, "desc"),
                        patron(&env, &creator),
                        kernel(),
                        pulse(&env),
                        admin.clone(),
                    )
                        .into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_create_talos(
                &s(&env, "Second"),
                &s(&env, "Marketing"),
                &s(&env, "desc"),
                &patron(&env, &creator),
                &kernel(),
                &pulse(&env),
                &admin,
            )
            .is_err());

        // ...but every other write path and all reads remain functional.
        assert!(!client.is_paused(&PauseDomain::PatronUpdates));
        let updated_patron = Patron {
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
                    args: (id, updated_patron.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .update_patron(&id, &updated_patron);

        assert!(client.get_talos(&id).is_some());
        assert!(client.is_active(&id));
    }

    #[test]
    fn reads_are_never_gated_by_pause() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        client.initialize(&admin);
        let id = create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);

        for domain in [
            PauseDomain::TalosCreation,
            PauseDomain::PatronUpdates,
            PauseDomain::KernelUpdates,
            PauseDomain::PulseUpdates,
            PauseDomain::Deactivation,
        ] {
            mock_pause(&env, &client, &contract_id, &admin, &domain, 0);
        }

        assert!(client.get_talos(&id).is_some());
        assert!(client.is_active(&id));
        assert_eq!(client.creator_of(&id), Some(creator));
        assert_eq!(client.next_talos_id(), 2);
    }

    // ── Emergency Pause: roles & duration bounds ────────────────────────

    #[test]
    fn admin_can_pause_indefinitely_with_zero_duration() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        mock_pause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation, 0);

        let info = client.pause_info(&PauseDomain::TalosCreation);
        assert!(info.active);
        assert_eq!(info.expires_at, None);
        assert_eq!(info.paused_by, Some(admin));
    }

    #[test]
    fn admin_pause_duration_exceeding_max_is_rejected() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let ok = try_mock_pause(
            &env,
            &client,
            &contract_id,
            &admin,
            &PauseDomain::TalosCreation,
            MAX_ADMIN_PAUSE_SECS + 1,
        );
        assert!(!ok);
    }

    #[test]
    fn guardian_can_pause_within_bounded_duration() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            3600,
        );

        let info = client.pause_info(&PauseDomain::TalosCreation);
        assert!(info.active);
        assert_eq!(info.paused_by, Some(guardian));
        assert!(info.expires_at.is_some());
    }

    #[test]
    fn guardian_pause_with_zero_duration_is_rejected() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        // duration = 0 would mean "indefinite" — guardians may never set that.
        let ok = try_mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            0,
        );
        assert!(!ok);
    }

    #[test]
    fn guardian_pause_exceeding_max_duration_is_rejected() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        let ok = try_mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            MAX_GUARDIAN_PAUSE_SECS + 1,
        );
        assert!(!ok);
    }

    #[test]
    fn non_admin_non_guardian_cannot_pause() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let stranger = Address::generate(&env);
        client.initialize(&admin);

        let ok = try_mock_pause(
            &env,
            &client,
            &contract_id,
            &stranger,
            &PauseDomain::TalosCreation,
            3600,
        );
        assert!(!ok);
    }

    #[test]
    fn pause_requires_caller_auth() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        // No mock_auths supplied → auth check must fail.
        let res = client.try_pause(&admin, &PauseDomain::TalosCreation, &0);
        assert!(res.is_err());
    }

    // ── Emergency Pause: guardian containment ───────────────────────────

    #[test]
    fn guardian_cannot_override_admin_established_pause() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        mock_pause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation, 0);

        let ok = try_mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            3600,
        );
        assert!(!ok);

        // The admin's indefinite pause is untouched.
        let info = client.pause_info(&PauseDomain::TalosCreation);
        assert_eq!(info.paused_by, Some(admin));
        assert_eq!(info.expires_at, None);
    }

    #[test]
    fn admin_can_override_guardian_pause() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            3600,
        );
        mock_pause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation, 0);

        let info = client.pause_info(&PauseDomain::TalosCreation);
        assert_eq!(info.paused_by, Some(admin));
        assert_eq!(info.expires_at, None);
    }

    #[test]
    fn only_admin_can_unpause_a_guardian_set_pause() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            3600,
        );

        // Guardian cannot unpause its own pause.
        let res = client
            .mock_auths(&[MockAuth {
                address: &guardian,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "unpause",
                    args: (PauseDomain::TalosCreation,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_unpause(&PauseDomain::TalosCreation);
        assert!(res.is_err());

        // Admin can.
        mock_unpause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation);
        assert!(!client.is_paused(&PauseDomain::TalosCreation));
    }

    // ── Emergency Pause: expiration & idempotency ───────────────────────

    #[test]
    fn guardian_pause_auto_expires() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        let creator = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        mock_pause(
            &env,
            &client,
            &contract_id,
            &guardian,
            &PauseDomain::TalosCreation,
            3600,
        );
        assert!(client.is_paused(&PauseDomain::TalosCreation));

        env.ledger().with_mut(|li| {
            li.timestamp += 3601;
        });

        assert!(!client.is_paused(&PauseDomain::TalosCreation));

        // The domain is writable again without any explicit unpause call.
        create_talos_with_auth(&env, &client, &contract_id, &creator, &admin);
    }

    #[test]
    fn unpause_on_a_domain_with_no_active_pause_is_a_deterministic_noop() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        // Never paused — must not panic.
        mock_unpause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation);

        // Pause, unpause, then unpause again (duplicate/retried call) — still must not panic.
        mock_pause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation, 0);
        mock_unpause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation);
        mock_unpause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation);

        assert!(!client.is_paused(&PauseDomain::TalosCreation));
    }

    // ── Emergency Pause: guardian roster management ─────────────────────

    #[test]
    fn add_guardian_is_idempotent_and_admin_only() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        let stranger = Address::generate(&env);
        client.initialize(&admin);

        let res = client
            .mock_auths(&[MockAuth {
                address: &stranger,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "add_guardian",
                    args: (guardian.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_add_guardian(&guardian);
        assert!(res.is_err());

        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);
        assert!(client.is_guardian(&guardian));
        assert_eq!(client.list_guardians().len(), 1);

        // Re-adding is a no-op, not an error, and does not duplicate the entry.
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);
        assert_eq!(client.list_guardians().len(), 1);
    }

    #[test]
    fn remove_guardian_is_idempotent_and_admin_only() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);
        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);

        let res = client
            .mock_auths(&[MockAuth {
                address: &guardian,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "remove_guardian",
                    args: (guardian.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_remove_guardian(&guardian);
        assert!(res.is_err());

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "remove_guardian",
                    args: (guardian.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .remove_guardian(&guardian);
        assert!(!client.is_guardian(&guardian));

        // Removing again is a deterministic no-op.
        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "remove_guardian",
                    args: (guardian.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .remove_guardian(&guardian);
    }

    #[test]
    fn guardian_roster_is_bounded() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        for _ in 0..MAX_GUARDIANS {
            let g = Address::generate(&env);
            mock_add_guardian(&env, &client, &contract_id, &admin, &g);
        }

        let one_too_many = Address::generate(&env);
        let res = client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "add_guardian",
                    args: (one_too_many.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_add_guardian(&one_too_many);
        assert!(res.is_err());
        assert_eq!(client.list_guardians().len(), MAX_GUARDIANS);
    }

    // ── Emergency Pause: events ──────────────────────────────────────────

    #[test]
    fn pause_and_unpause_emit_events() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        mock_pause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation, 0);
        let events = env.events().all();
        let (_, topics, data) = events.get(events.len() - 1).unwrap();
        assert_topic_symbol(&env, &topics, 0, symbol_short!("pause_on"));
        let (actor, expires_at): (Address, u64) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(actor, admin);
        assert_eq!(expires_at, 0);

        mock_unpause(&env, &client, &contract_id, &admin, &PauseDomain::TalosCreation);
        let events = env.events().all();
        let (_, topics, data) = events.get(events.len() - 1).unwrap();
        assert_topic_symbol(&env, &topics, 0, symbol_short!("pause_off"));
        let (actor,): (Address,) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(actor, admin);
    }

    #[test]
    fn guardian_management_emits_events() {
        let (env, contract_id) = setup();
        let client = TalosRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        client.initialize(&admin);

        mock_add_guardian(&env, &client, &contract_id, &admin, &guardian);
        let events = env.events().all();
        let (_, topics, data) = events.get(events.len() - 1).unwrap();
        assert_topic_symbol(&env, &topics, 0, symbol_short!("guard_add"));
        let (got,): (Address,) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got, guardian);

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "remove_guardian",
                    args: (guardian.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .remove_guardian(&guardian);
        let events = env.events().all();
        let (_, topics, data) = events.get(events.len() - 1).unwrap();
        assert_topic_symbol(&env, &topics, 0, symbol_short!("guard_rem"));
        let (got,): (Address,) = TryFromVal::try_from_val(&env, &data).unwrap();
        assert_eq!(got, guardian);
    }
}
