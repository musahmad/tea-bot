// Usage:
//   cargo run --bin check_balance -- <address_or_name> [address_or_name ...]
//
// Examples:
//   cargo run --bin check_balance -- Musa
//   cargo run --bin check_balance -- Musa Marcus Tim
//   cargo run --bin check_balance -- 0x208f10e8dc6dba5cb4f26279020cd572c8ec6242

use alloy::{
    primitives::Address,
    providers::{DynProvider, ProviderBuilder},
};
use serde::Deserialize;

alloy::sol!(
    #[sol(rpc)]
    TeaBot,
    "abi.json"
);

#[derive(Deserialize)]
struct Config {
    wallet_address: String,
    provider_url: String,
    users: Vec<UserEntry>,
}

#[derive(Deserialize)]
struct UserEntry {
    name: String,
    address: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: check_balance <address_or_name> [address_or_name ...]");
        std::process::exit(1);
    }

    dotenv::dotenv().ok();
    let config: Config = load_config()?;

    let provider = ProviderBuilder::new()
        .connect_http(config.provider_url.parse()?);
    let contract = TeaBot::new(
        config.wallet_address.parse()?,
        DynProvider::new(provider),
    );

    let lookups: Vec<(String, Address)> = args[1..]
        .iter()
        .map(|arg| resolve_address(arg, &config.users))
        .collect::<Result<_, _>>()?;

    let addresses: Vec<Address> = lookups.iter().map(|(_, addr)| *addr).collect();
    let balances = contract.mass_balance(addresses).call().await?.to_vec();

    for ((label, _), balance) in lookups.iter().zip(balances.iter()) {
        let balance_f64 = balance.to::<u128>() as f64 / 1e18;
        println!("{}: {:.4} TEA", label, balance_f64);
    }

    Ok(())
}

fn resolve_address(input: &str, users: &[UserEntry]) -> anyhow::Result<(String, Address)> {
    if let Ok(addr) = input.parse::<Address>() {
        return Ok((input.to_string(), addr));
    }
    let lower = input.to_lowercase();
    users
        .iter()
        .find(|u| u.name.to_lowercase() == lower)
        .map(|u| Ok((u.name.clone(), u.address.parse::<Address>()?)))
        .unwrap_or_else(|| anyhow::bail!("'{}' is not a valid address or known user name", input))
}

fn load_config() -> anyhow::Result<Config> {
    if let Ok(json) = std::env::var("CONFIG_JSON") {
        Ok(serde_json5::from_str(&json)?)
    } else {
        let path = std::env::var("CONFIG_PATH")
            .unwrap_or_else(|_| "config.json5".to_string());
        let mut file = std::fs::File::open(&path)?;
        Ok(serde_json5::from_reader(&mut file)?)
    }
}
