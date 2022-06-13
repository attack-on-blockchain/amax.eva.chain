// Substrate
use sc_service::{ChainType, Properties};
// Local
use primitives_core::{AccountId, Balance};
use wall_e_runtime::{AuraId, GenesisConfig, GrandpaId, SS58Prefix, WASM_BINARY};
use wall_e_runtime_constants::currency::UNITS;

use super::key_helper::{authority_keys_from_seed, generate_dev_accounts};

// The URL for the telemetry server.
// const STAGING_TELEMETRY_URL: &str = "wss://telemetry.polkadot.io/submit/";

/// Specialized `ChainSpec`. This is a specialization of the general Substrate ChainSpec type.
pub type ChainSpec = sc_service::GenericChainSpec<GenesisConfig>;

fn properties() -> Properties {
    let mut properties = Properties::new();
    properties.insert("tokenSymbol".into(), "AMAX".into());
    properties.insert("tokenDecimals".into(), 18.into());
    properties.insert("ss58Format".into(), SS58Prefix::get().into());
    properties
}

pub fn development_config() -> Result<ChainSpec, String> {
    let wasm_binary = WASM_BINARY.ok_or_else(|| "Development wasm not available".to_string())?;

    let accounts = generate_dev_accounts(10);

    Ok(ChainSpec::from_genesis(
        // Name
        "Wall-e Development",
        // ID
        "wall_e_dev",
        ChainType::Development,
        move || {
            let endowed = accounts.clone().into_iter().map(|k| (k, 100000 * UNITS)).collect();
            genesis(
                wasm_binary,
                // Initial PoA authorities
                vec![authority_keys_from_seed("Alice")],
                // Sudo account
                accounts[0],
                // Pre-funded accounts
                endowed,
                true,
            )
        },
        // Bootnodes
        vec![],
        // Telemetry
        None,
        // Protocol ID
        None,
        None,
        // Properties
        Some(properties()),
        // Extensions
        None,
    ))
}

pub fn local_testnet_config() -> Result<ChainSpec, String> {
    let wasm_binary = WASM_BINARY.ok_or_else(|| "Development wasm not available".to_string())?;

    let accounts = generate_dev_accounts(10);

    Ok(ChainSpec::from_genesis(
        // Name
        "Wall-e Local Testnet",
        // ID
        "wall_e_local_testnet",
        ChainType::Local,
        move || {
            let endowed = accounts.clone().into_iter().map(|k| (k, 100000 * UNITS)).collect();
            genesis(
                wasm_binary,
                // Initial PoA authorities
                vec![authority_keys_from_seed("Alice"), authority_keys_from_seed("Bob")],
                // Sudo account
                accounts[0], // Alith
                // Pre-funded accounts
                endowed,
                true,
            )
        },
        // Bootnodes
        vec![],
        // Telemetry
        None,
        // Protocol ID
        None,
        // Properties
        None,
        Some(properties()),
        // Extensions
        None,
    ))
}

/// Configure initial storage state for FRAME modules.
fn genesis(
    wasm_binary: &[u8],
    initial_authorities: Vec<(AuraId, GrandpaId)>,
    root_key: AccountId,
    endowed: Vec<(AccountId, Balance)>,
    _enable_println: bool,
) -> GenesisConfig {
    use wall_e_runtime::{AuraConfig, BalancesConfig, GrandpaConfig, SudoConfig, SystemConfig};
    GenesisConfig {
        // System && Utility.
        system: SystemConfig {
            // Add Wasm runtime to storage.
            code: wasm_binary.to_vec(),
        },
        // Monetary.
        balances: BalancesConfig { balances: endowed },
        transaction_payment: Default::default(),
        // Consesnsus.
        aura: AuraConfig {
            authorities: initial_authorities.iter().map(|x| (x.0.clone())).collect(),
        },
        grandpa: GrandpaConfig {
            authorities: initial_authorities.iter().map(|x| (x.1.clone(), 1)).collect(),
        },
        technical_committee: Default::default(),
        technical_committee_membership: Default::default(),
        // Evm compatibility.
        evm: Default::default(),
        ethereum: Default::default(),
        base_fee: Default::default(),
        sudo: SudoConfig {
            // Assign network admin rights.
            key: Some(root_key),
        },
    }
}