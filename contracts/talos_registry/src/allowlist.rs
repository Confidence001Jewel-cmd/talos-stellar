use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

#[contracttype]
#[derive(Clone)]
pub enum AllowlistDataKey {
    AllowedAsset(Address),
    Admin,
}

#[contract]
pub struct AssetAllowlist;

#[contractimpl]
impl AssetAllowlist {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&AllowlistDataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&AllowlistDataKey::Admin, &admin);
    }

    pub fn add_asset(env: Env, asset: Address) {
        let admin: Address = env.storage().instance().get(&AllowlistDataKey::Admin).expect("not initialized");
        admin.require_auth();
        env.storage().instance().set(&AllowlistDataKey::AllowedAsset(asset.clone()), &true);
    }

    pub fn remove_asset(env: Env, asset: Address) {
        let admin: Address = env.storage().instance().get(&AllowlistDataKey::Admin).expect("not initialized");
        admin.require_auth();
        env.storage().instance().remove(&AllowlistDataKey::AllowedAsset(asset));
    }

    pub fn is_allowed(env: Env, asset: Address) -> bool {
        env.storage().instance().get(&AllowlistDataKey::AllowedAsset(asset)).unwrap_or(false)
    }
}
