// Usage:
//   cargo run --bin transfer -- <from> <to> <amount>
//
// Examples:
//   cargo run --bin transfer -- Musa Marcus 5.0
//   cargo run --bin transfer -- 0x208f10e8dc6dba5cb4f26279020cd572c8ec6242 0x1318d7f15f8d9824b88aeaf94fc2f9afafe1a53e 2.5

use alloy::{
    network::EthereumWallet,
    primitives::{Address, U256},
    providers::{DynProvider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use serde::Deserialize;

alloy::sol!(
    #[sol(rpc)]
    TeaBot,
    "abi.json"
);

#[derive(Deserialize)]
struct Config {
    private_key: String,
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
    if args.len() != 4 {
        eprintln!("Usage: transfer <from_address_or_name> <to_address_or_name> <amount>");
        std::process::exit(1);
    }

    dotenv::dotenv().ok();
    let config: Config = load_config()?;

    let (from_label, from_addr) = resolve_address(&args[1], &config.users)?;
    let (to_label, to_addr) = resolve_address(&args[2], &config.users)?;
    let amount: f64 = args[3].parse()?;
    let amount_wei = (amount * 1e18).round() as u128;

    let signer: PrivateKeySigner = config.private_key.parse()?;
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .with_simple_nonce_management()
        .connect_http(config.provider_url.parse()?);
    let contract = TeaBot::new(
        config.wallet_address.parse()?,
        DynProvider::new(provider),
    );

    println!(
        "Transferring {:.4} TEA from {} ({}) to {} ({})...",
        amount, from_label, from_addr, to_label, to_addr
    );

    let receipt = contract
        .mass_transfer(vec![(to_addr, from_addr, U256::from(amount_wei))])
        .send()
        .await?
        .get_receipt()
        .await?;

    println!("Transaction hash: {}", receipt.transaction_hash);
    println!("Status: {}", if receipt.status() { "success" } else { "failed" });

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
