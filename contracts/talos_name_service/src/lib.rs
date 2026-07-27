//! TalosNameService — Soroban smart contract for human-readable Talos names.
//!
//! Handles:
//! - Name registration (e.g., "marketbot" → Talos ID)
//! - Name resolution (name → Talos ID)
//! - Name availability checks
//! - Validation: 3-32 chars, lowercase alphanumeric + hyphens

#![no_std]

#[cfg(all(test, not(target_arch = "wasm32")))]
extern crate std;

#[cfg(all(test, not(target_arch = "wasm32")))]
use std::string::ToString;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Env, IntoVal, String, Symbol,
};

// ── Data Types ──────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    NameRecord(String), // name → talos_id
    TalosName(u32),     // talos_id → name
    RegistryContract,
    Admin,              // protocol admin for name-service level (ProtocolWallet clone / admin)
    NameFeeAmount,     // i128 — default fee for name registration
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    AlreadyInitialized = 1,
    UnauthorizedCaller = 2,
}

// ── Events ──────────────────────────────────────────────────────────
//
// Event schema (topics → data):
//   name_reg : (symbol, talos_id: u32) → (name: String, owner: Address)
//   nmf_paid : (symbol, talos_id: u32) → (payer: Address, asset: Address, amount: i128)

fn emit_name_registered(env: &Env, talos_id: u32, name: String, owner: Address) {
    let topics = (symbol_short!("name_reg"), talos_id);
    env.events().publish(topics, (name, owner));
}

fn emit_name_fee_paid(env: &Env, talos_id: u32, payer: &Address, asset: &Address, amount: i128) {
    env.events()
        .publish((symbol_short!("nmf_paid"), talos_id), (payer.clone(), asset.clone(), amount));
}

// ── Validation ──────────────────────────────────────────────────────
fn validate_name(name: &String) -> bool {
    let len = name.len();
    if len < 3 || len > 32 {
        return false;
    }

    let name_string = name.to_string();
    let bytes = name_string.as_bytes();
    if bytes.first().is_none() || bytes.last().is_none() {
        return false;
    }

    let mut prev_hyphen = false;
    for &b in bytes {
        if (b'a'..=b'z').contains(&b) || (b'0'..=b'9').contains(&b) {
            prev_hyphen = false;
            continue;
        }
        if b == b'-' {
            if prev_hyphen || bytes.first() == Some(&b) || bytes.last() == Some(&b) {
                return false;
            }
            prev_hyphen = true;
            continue;
        }
        return false;
    }

    true
}

// ── Contract ────────────────────────────────────────────────────────

/// Compile-time interface version of TalosNameService.
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

#[contract]
pub struct TalosNameService;

#[contractimpl]
impl TalosNameService {
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

    /// Register a name for a Talos.
    ///
    /// # Arguments
    /// * `e` - Soroban environment
    /// * `owner` - The address authorizing this name registration
    /// * `talos_id` - The Talos ID to associate with the name
    /// * `name` - Human-readable name (3-32 chars, lowercase alphanumeric + hyphens)
    pub fn register_name(e: Env, owner: Address, talos_id: u32, name: String) {
        owner.require_auth();

        if !validate_name(&name) {
            panic!("Invalid name. Must be 3-32 chars, lowercase alphanumeric + hyphens, no consecutive hyphens.");
        }

        if e.storage()
            .persistent()
            .get::<_, u32>(&DataKey::NameRecord(name.clone()))
            .is_some()
        {
            panic!("Name already taken");
        }

        let registry_contract: Address = e
            .storage()
            .persistent()
            .get(&DataKey::RegistryContract)
            .expect("Registry contract not initialized");

        let creator: Option<Address> = e.invoke_contract(
            &registry_contract,
            &Symbol::new(&e, "creator_of"),
            soroban_sdk::vec![&e, talos_id.into_val(&e)],
        );

        if creator != Some(owner.clone()) {
            panic_with_error!(&e, ContractError::UnauthorizedCaller);
        }

        // Retrieve the old name via TalosName(talos_id) and delete NameRecord(old_name)
        // to prevent dangling records when changing names.
        if let Some(old_name) = e
            .storage()
            .persistent()
            .get::<_, String>(&DataKey::TalosName(talos_id))
        {
            e.storage()
                .persistent()
                .remove(&DataKey::NameRecord(old_name));
        }

        // Store mappings
        e.storage()
            .persistent()
            .set(&DataKey::NameRecord(name.clone()), &talos_id);
        e.storage()
            .persistent()
            .set(&DataKey::TalosName(talos_id), &name);

        emit_name_registered(&e, talos_id, name, owner);
    }

    pub fn initialize(e: Env, registry_id: Address, admin: Address, name_fee: i128) {
        if e.storage()
            .persistent()
            .get::<_, Address>(&DataKey::RegistryContract)
            .is_some()
        {
            panic_with_error!(&e, ContractError::AlreadyInitialized);
        }
        if name_fee < 0 {
            panic!("name_fee must be non-negative");
        }

        e.storage()
            .persistent()
            .set(&DataKey::RegistryContract, &registry_id);
        e.storage().persistent().set(&DataKey::Admin, &admin);
        e.storage().persistent().set(&DataKey::NameFeeAmount, &name_fee);
    }

    /// Return the current name registration fee amount, or 0 if unconfigured.
    pub fn name_fee(e: Env) -> i128 {
        e.storage().persistent().get(&DataKey::NameFeeAmount).unwrap_or(0)
    }

    /// Return the configured admin, if any.
    pub fn admin(e: Env) -> Option<Address> {
        e.storage().persistent().get(&DataKey::Admin)
    }

    /// Update the name-registration fee. Only the configured admin may call this.
    pub fn set_name_fee(e: Env, new_fee: i128) {
        if new_fee < 0 {
            panic!("name_fee must be non-negative");
        }
        let admin: Address = e.storage().persistent().get(&DataKey::Admin).expect("admin not set");
        admin.require_auth();
        e.storage().persistent().set(&DataKey::NameFeeAmount, &new_fee);
    }

    /// Register a name AND pay the registration fee with an allowlisted asset.
    ///
    /// In addition to the authorization and registry checks in `register_name`, this
    /// entry-point enforces that:
    ///   1. `asset` is allowlisted in the registry (cross-contract `is_asset_allowed`)
    ///   2. `payer` authorizes a transfer of exactly `name_fee` tokens → Admin
    ///   3. `owner` still authorizes the NAME update itself
    ///
    /// If the asset was removed from the registry allowlist after initialization,
    /// this call panics before any funds move — so an attacker cannot pay with a
    /// mintable/bogus token.
    pub fn register_name_with_fee(
        e: Env,
        owner: Address,
        talos_id: u32,
        name: String,
        payer: Address,
        asset: Address,
    ) {
        // First run the plain name registration logic.
        Self::register_name(e.clone(), owner.clone(), talos_id, name.clone());

        let fee: i128 = e.storage().persistent().get(&DataKey::NameFeeAmount).unwrap_or(0);
        if fee <= 0 {
            return;
        }

        let registry_contract: Address = e
            .storage()
            .persistent()
            .get(&DataKey::RegistryContract)
            .expect("Registry contract not initialized");
        let admin: Address = e.storage().persistent().get(&DataKey::Admin).expect("admin not set");

        // Cross-contract call: verify asset is allowlisted in TalosRegistry.
        let allowed: bool = e.invoke_contract(
            &registry_contract,
            &Symbol::new(&e, "is_asset_allowed"),
            soroban_sdk::vec![&e, asset.clone().into_val(&e)],
        );
        if !allowed {
            panic!("Asset not in registry allowlist; cannot pay name fee");
        }

        payer.require_auth();

        let token = soroban_sdk::token::TokenClient::new(&e, &asset);
        token.transfer(&payer, &admin, &fee);

        emit_name_fee_paid(&e, talos_id, &payer, &asset, fee);
    }

    /// Resolve a name to a Talos ID.
    /// Returns None if the name doesn't exist.
    pub fn resolve_name(e: Env, name: String) -> Option<u32> {
        e.storage().persistent().get(&DataKey::NameRecord(name))
    }

    /// Get the name associated with a Talos ID.
    /// Returns None if the Talos has no name.
    pub fn name_of(e: Env, talos_id: u32) -> Option<String> {
        e.storage().persistent().get(&DataKey::TalosName(talos_id))
    }

    /// Check if a name is available.
    pub fn is_name_available(e: Env, name: String) -> bool {
        if !validate_name(&name) {
            return false;
        }
        e.storage()
            .persistent()
            .get::<_, u32>(&DataKey::NameRecord(name))
            .is_none()
    }

    /// Check if a Talos has a registered name.
    pub fn has_name(e: Env, talos_id: u32) -> bool {
        e.storage()
            .persistent()
            .get::<_, String>(&DataKey::TalosName(talos_id))
            .is_some()
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
    use talos_registry::{Kernel, Patron, Pulse, TalosRegistry, TalosRegistryClient};

    fn setup() -> (
        Env,
        Address,
        Address,
        Address,
        TalosRegistryClient<'static>,
        TalosNameServiceClient<'static>,
    ) {
        let _env = Env::default();
        let env = _env.clone();
        let registry_contract = env.register_contract(None, TalosRegistry);
        let name_service_contract = env.register_contract(None, TalosNameService);
        let name_service_client = TalosNameServiceClient::new(&env, &name_service_contract);
        let admin = Address::generate(&env);
        name_service_client.initialize(&registry_contract, &admin, &0i128);
        let registry_client = TalosRegistryClient::new(&env, &registry_contract);
        (
            env,
            registry_contract,
            name_service_contract,
            admin,
            registry_client,
            name_service_client,
        )
    }

    fn setup_with_fee(
        name_fee: i128,
    ) -> (
        Env,
        Address,
        Address,
        Address,
        TalosRegistryClient<'static>,
        TalosNameServiceClient<'static>,
    ) {
        let _env = Env::default();
        let env = _env.clone();
        let registry_contract = env.register_contract(None, TalosRegistry);
        let name_service_contract = env.register_contract(None, TalosNameService);
        let name_service_client = TalosNameServiceClient::new(&env, &name_service_contract);
        let admin = Address::generate(&env);
        name_service_client.initialize(&registry_contract, &admin, &name_fee);
        let registry_client = TalosRegistryClient::new(&env, &registry_contract);
        (
            env,
            registry_contract,
            name_service_contract,
            admin,
            registry_client,
            name_service_client,
        )
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

    fn register_name_with_auth(
        env: &Env,
        client: &TalosNameServiceClient,
        contract_id: &Address,
        registry_contract: &Address,
        owner: &Address,
        talos_id: u32,
        name: &String,
    ) {
        client
            .mock_auths(&[MockAuth {
                address: owner,
                invoke: &MockAuthInvoke {
                    contract: contract_id,
                    fn_name: "register_name",
                    args: (owner.clone(), talos_id, name.clone()).into_val(env),
                    sub_invokes: &[MockAuthInvoke {
                        contract: registry_contract,
                        fn_name: "creator_of",
                        args: (talos_id,).into_val(env),
                        sub_invokes: &[],
                    }],
                },
            }])
            .register_name(owner, &talos_id, name);
    }

    // ── version() tests ──────────────────────────────────────────────

    #[test]
    fn version_returns_compile_time_constant() {
        let (_env, _registry_contract, _contract_id, _admin, _registry_client, client) = setup();
        assert_eq!(client.version(), (1u32, 0u32, 0u32));
    }

    #[test]
    fn version_is_idempotent() {
        let (_env, _registry_contract, _contract_id, _admin, _registry_client, client) = setup();
        // Calling version() multiple times must always return the same value.
        assert_eq!(client.version(), client.version());
    }

    #[test]
    fn version_is_unaffected_by_state_changes() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name = s(&env, "vega");

        let before = client.version();

        // Register a name — a storage write must not affect the version constant.
        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );
        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name,
        );

        let after = client.version();
        assert_eq!(before, after);
    }

    #[test]
    fn version_matches_contract_version_constant() {
        // Verify that the public CONTRACT_VERSION constant and the on-chain
        // entry-point are in sync, so tooling that reads the constant directly
        // agrees with what the deployed WASM reports.
        let (_env, _registry_contract, _contract_id, _admin, _registry_client, client) = setup();
        let (maj, min, patch) = client.version();
        assert_eq!((maj, min, patch), CONTRACT_VERSION);
    }

    #[test]
    fn register_name_success() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name = s(&env, "marketbot");

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        assert!(client.is_name_available(&name));
        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name,
        );

        assert!(!client.is_name_available(&name));
        assert!(client.has_name(&talos_id));
        assert_eq!(client.resolve_name(&name), Some(talos_id));
        assert_eq!(client.name_of(&talos_id), Some(name));
    }

    #[test]
    fn duplicate_name_rejected() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let second_owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name = s(&env, "marketbot");

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name,
        );

        let duplicate_result = client
            .mock_auths(&[MockAuth {
                address: &second_owner,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "register_name",
                    args: (second_owner.clone(), talos_id, name.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_register_name(&second_owner, &talos_id, &name);

        assert!(duplicate_result.is_err());
    }

    #[test]
    fn unauthorized_caller_rejected() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let creator = Address::generate(&env);
        let unauthorized = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name = s(&env, "marketbot");

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &creator,
            &protocol_wallet,
        );

        let result = client
            .mock_auths(&[MockAuth {
                address: &unauthorized,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "register_name",
                    args: (unauthorized.clone(), talos_id, name.clone()).into_val(&env),
                    sub_invokes: &[MockAuthInvoke {
                        contract: &registry_contract,
                        fn_name: "creator_of",
                        args: (talos_id,).into_val(&env),
                        sub_invokes: &[],
                    }],
                },
            }])
            .try_register_name(&unauthorized, &talos_id, &name);

        assert!(result.is_err());
    }

    #[test]
    fn initialize_guard_rejects_reinitialization() {
        let (_env, registry_contract, _contract_id, admin, _registry_client, client) = setup();
        assert!(client.try_initialize(&registry_contract, &admin, &0i128).is_err());
    }

    #[test]
    fn lookup_by_name_returns_correct_talos_id() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name = s(&env, "atlas-agent");

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name,
        );

        assert_eq!(client.resolve_name(&name), Some(talos_id));
        assert_eq!(client.name_of(&talos_id), Some(name));
    }

    #[test]
    fn invalid_name_rejected() {
        let (env, _registry_contract, contract_id, _admin, _registry_client, client) = setup();
        let invalid_name = s(&env, "ab");
        let owner = Address::generate(&env);

        let result = client
            .mock_auths(&[MockAuth {
                address: &owner,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "register_name",
                    args: (owner.clone(), 1u32, invalid_name.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_register_name(&owner, &1, &invalid_name);

        assert!(result.is_err());
    }

    #[test]
    fn accepts_valid_name_patterns() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let valid_name = s(&env, "alpha-1");
        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        let result = client
            .mock_auths(&[MockAuth {
                address: &owner,
                invoke: &MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "register_name",
                    args: (owner.clone(), talos_id, valid_name.clone()).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_register_name(&owner, &talos_id, &valid_name);

        assert!(result.is_ok());
    }

    #[test]
    fn rejects_invalid_name_patterns() {
        let (env, _registry_contract, contract_id, _admin, _registry_client, client) = setup();
        let owner = Address::generate(&env);
        let invalid_names = [
            s(&env, "Alpha"),
            s(&env, "bad--name"),
            s(&env, "-bad"),
            s(&env, "bad-"),
        ];

        for invalid_name in invalid_names {
            let result = client
                .mock_auths(&[MockAuth {
                    address: &owner,
                    invoke: &MockAuthInvoke {
                        contract: &contract_id,
                        fn_name: "register_name",
                        args: (owner.clone(), 1u32, invalid_name.clone()).into_val(&env),
                        sub_invokes: &[],
                    },
                }])
                .try_register_name(&owner, &1, &invalid_name);

            assert!(
                result.is_err(),
                "expected invalid name {:?} to be rejected",
                invalid_name.to_string()
            );
        }
    }

    #[test]
    fn register_name_emits_name_reg_event() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name = s(&env, "marketbot");

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name,
        );

        let all_events = env.events().all();
        let events = all_events
            .iter()
            .filter(|e| e.0 == contract_id)
            .collect::<std::vec::Vec<_>>();
        assert_eq!(events.len(), 1);
        let (_addr, topics, data) = events.get(0).unwrap();
        assert_eq!(topics.len() as u32, 2);
        let t0: Symbol = TryFromVal::try_from_val(&env, &topics.get(0).unwrap()).unwrap();
        let t1: u32 = TryFromVal::try_from_val(&env, &topics.get(1).unwrap()).unwrap();
        let (got_name, got_owner): (String, Address) =
            TryFromVal::try_from_val(&env, data).unwrap();
        assert_eq!(got_name, name);
        assert_eq!(got_owner, owner);
    }

    #[test]
    fn update_name_removes_old_record() {
        let (env, registry_contract, contract_id, _admin, registry_client, client) = setup();
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let name1 = s(&env, "name1");
        let name2 = s(&env, "name2");

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        // Register first name
        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name1,
        );

        assert_eq!(client.resolve_name(&name1), Some(talos_id));

        // Register second name
        register_name_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            talos_id,
            &name2,
        );

        assert_eq!(client.resolve_name(&name2), Some(talos_id));
        // Verify old name is cleared
        assert_eq!(client.resolve_name(&name1), None);
        assert!(client.is_name_available(&name1));
    }
    #[test]
    fn has_name_returns_false_for_unknown_talos_id() {
        let (env, registry_contract, contract_id, _admin, _registry_client, client) = setup();

        // talos_id = 999 does not exist
        assert!(!client.has_name(&999));
    }

    #[test]
    fn name_of_returns_none_for_unknown_talos_id() {
        let (env, _registry_contract, _contract_id, _admin, _registry_client, client) = setup();

        assert!(client.name_of(&999).is_none());
    }

    fn create_and_mint_token(env: &Env, admin: &Address) -> Address {
        let id = env.register_contract_wasm(None, soroban_sdk::token::StellarAssetWASM::new(env));
        let token = soroban_sdk::token::TokenClient::new(env, &id);
        token.initialize(admin, &7u32, &s(env, "TEST"), &s(env, "Test USD"));
        id
    }

    fn mint(env: &Env, asset: &Address, from: &Address, to: &Address, amount: i128) {
        soroban_sdk::token::TokenClient::new(env, asset)
            .mock_all_auths()
            .mint(from, to, &amount);
    }

    fn balance_of(env: &Env, asset: &Address, addr: &Address) -> i128 {
        soroban_sdk::token::TokenClient::new(env, asset).balance(addr)
    }

    fn mock_allow(
        env: &Env,
        client: &TalosRegistryClient,
        contract_id: &Address,
        admin: &Address,
        asset: &Address,
    ) {
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

    fn register_name_with_fee_with_auth(
        env: &Env,
        client: &TalosNameServiceClient,
        contract_id: &Address,
        registry_contract: &Address,
        owner: &Address,
        payer: &Address,
        talos_id: u32,
        name: &String,
        asset: &Address,
    ) {
        client
            .mock_auths(&[
                MockAuth {
                    address: owner,
                    invoke: &MockAuthInvoke {
                        contract: contract_id,
                        fn_name: "register_name_with_fee",
                        args: (
                            owner.clone(),
                            talos_id,
                            name.clone(),
                            payer.clone(),
                            asset.clone(),
                        )
                            .into_val(env),
                        sub_invokes: &[MockAuthInvoke {
                            contract: registry_contract,
                            fn_name: "creator_of",
                            args: (talos_id,).into_val(env),
                            sub_invokes: &[],
                        }],
                    },
                },
                MockAuth {
                    address: payer,
                    invoke: &MockAuthInvoke {
                        contract: contract_id,
                        fn_name: "register_name_with_fee",
                        args: (
                            owner.clone(),
                            talos_id,
                            name.clone(),
                            payer.clone(),
                            asset.clone(),
                        )
                            .into_val(env),
                        sub_invokes: &[],
                    },
                },
            ])
            .register_name_with_fee(owner, &talos_id, name, payer, asset);
    }

    #[test]
    fn register_name_with_fee_allowed_asset_transfers_fee() {
        let (env, registry_contract, contract_id, ns_admin, registry_client, client) =
            setup_with_fee(1_000i128);
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let payer = Address::generate(&env);
        let name = s(&env, "marketbot");

        registry_client.initialize(&protocol_wallet);
        let asset = create_and_mint_token(&env, &protocol_wallet);
        mock_allow(&env, &registry_client, &registry_contract, &protocol_wallet, &asset);
        mint(&env, &asset, &protocol_wallet, &payer, 10_000);

        let admin_before = balance_of(&env, &asset, &ns_admin);
        let payer_before = balance_of(&env, &asset, &payer);

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        register_name_with_fee_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            &payer,
            talos_id,
            &name,
            &asset,
        );

        assert_eq!(client.name_of(&talos_id), Some(name));
        assert_eq!(balance_of(&env, &asset, &payer), payer_before - 1_000);
        assert_eq!(balance_of(&env, &asset, &ns_admin), admin_before + 1_000);
    }

    #[test]
    fn register_name_with_fee_denied_asset_panics_before_transfer() {
        let (env, registry_contract, contract_id, ns_admin, registry_client, client) =
            setup_with_fee(1_000i128);
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let payer = Address::generate(&env);
        let name = s(&env, "marketbot");

        registry_client.initialize(&protocol_wallet);
        let asset = create_and_mint_token(&env, &protocol_wallet);
        mint(&env, &asset, &protocol_wallet, &payer, 10_000);

        let admin_before = balance_of(&env, &asset, &ns_admin);
        let payer_before = balance_of(&env, &asset, &payer);

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        let result = client
            .mock_auths(&[
                MockAuth {
                    address: &owner,
                    invoke: &MockAuthInvoke {
                        contract: &contract_id,
                        fn_name: "register_name_with_fee",
                        args: (
                            owner.clone(),
                            talos_id,
                            name.clone(),
                            payer.clone(),
                            asset.clone(),
                        )
                            .into_val(&env),
                        sub_invokes: &[MockAuthInvoke {
                            contract: &registry_contract,
                            fn_name: "creator_of",
                            args: (talos_id,).into_val(&env),
                            sub_invokes: &[],
                        }],
                    },
                },
                MockAuth {
                    address: &payer,
                    invoke: &MockAuthInvoke {
                        contract: &contract_id,
                        fn_name: "register_name_with_fee",
                        args: (
                            owner.clone(),
                            talos_id,
                            name.clone(),
                            payer.clone(),
                            asset.clone(),
                        )
                            .into_val(&env),
                        sub_invokes: &[],
                    },
                },
            ])
            .try_register_name_with_fee(&owner, &talos_id, &name, &payer, &asset);

        assert!(result.is_err());
        assert_eq!(balance_of(&env, &asset, &payer), payer_before);
        assert_eq!(balance_of(&env, &asset, &ns_admin), admin_before);
    }

    #[test]
    fn set_name_fee_unauthorized_update_panics() {
        let (_env, _registry_contract, _contract_id, admin, _registry_client, client) = setup();
        let impostor = Address::generate(&_env);

        assert_eq!(client.name_fee(), 0i128);

        let result = client
            .mock_auths(&[MockAuth {
                address: &impostor,
                invoke: &MockAuthInvoke {
                    contract: &_contract_id,
                    fn_name: "set_name_fee",
                    args: (5_000i128,).into_val(&_env),
                    sub_invokes: &[],
                },
            }])
            .try_set_name_fee(&5_000i128);

        assert!(result.is_err());
        assert_eq!(client.name_fee(), 0i128);

        client
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &_contract_id,
                    fn_name: "set_name_fee",
                    args: (5_000i128,).into_val(&_env),
                    sub_invokes: &[],
                },
            }])
            .set_name_fee(&5_000i128);
        assert_eq!(client.name_fee(), 5_000i128);
    }

    #[test]
    fn register_name_with_fee_emits_nmf_paid_event() {
        let (env, registry_contract, contract_id, ns_admin, registry_client, client) =
            setup_with_fee(500i128);
        let owner = Address::generate(&env);
        let protocol_wallet = Address::generate(&env);
        let payer = Address::generate(&env);
        let name = s(&env, "marketbot");

        registry_client.initialize(&protocol_wallet);
        let asset = create_and_mint_token(&env, &protocol_wallet);
        mock_allow(&env, &registry_client, &registry_contract, &protocol_wallet, &asset);
        mint(&env, &asset, &protocol_wallet, &payer, 10_000);

        let talos_id = create_talos_with_auth(
            &env,
            &registry_client,
            &registry_contract,
            &owner,
            &protocol_wallet,
        );

        register_name_with_fee_with_auth(
            &env,
            &client,
            &contract_id,
            &registry_contract,
            &owner,
            &payer,
            talos_id,
            &name,
            &asset,
        );

        let events = env.events().all();
        let nmf_events: std::vec::Vec<_> = events
            .iter()
            .filter(|(addr, topics, _data)| {
                if addr != &contract_id {
                    return false;
                }
                if topics.len() < 2 {
                    return false;
                }
                let t0: Symbol = TryFromVal::try_from_val(&env, &topics.get(0).unwrap()).unwrap();
                t0 == symbol_short!("nmf_paid")
            })
            .collect();

        assert_eq!(nmf_events.len(), 1);
        let (_addr, topics, data) = nmf_events.get(0).unwrap();
        let t1: u32 = TryFromVal::try_from_val(&env, &topics.get(1).unwrap()).unwrap();
        let (got_payer, got_asset, got_amount): (Address, Address, i128) =
            TryFromVal::try_from_val(&env, data).unwrap();
        assert_eq!(t1, talos_id);
        assert_eq!(got_payer, payer);
        assert_eq!(got_asset, asset);
        assert_eq!(got_amount, 500i128);
    }
}
