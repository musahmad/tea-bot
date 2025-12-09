use std::collections::HashMap;

use alloy::{
    network::EthereumWallet,
    primitives::{Address, U256},
    providers::{DynProvider, ProviderBuilder},
    rpc::types::TransactionReceipt,
    signers::local::PrivateKeySigner,
};
use anyhow::Error;

use crate::User;

pub struct ContractInterface {
    instance: TeaBot::TeaBotInstance<DynProvider>,
    balances: HashMap<User, f64>,
    users: Vec<User>,
}

alloy::sol!(
    #[sol(rpc)]
    #[derive(Debug)]
    TeaBot,
    "abi.json"
);

impl ContractInterface {
    pub fn new(
        private_key: String,
        wallet_address: String,
        provider_url: String,
        users: Vec<User>,
    ) -> Self {
        let signer: PrivateKeySigner = private_key.parse().unwrap();

        let wallet = EthereumWallet::from(signer);

        let wallet_client = ProviderBuilder::new()
            .wallet(wallet.clone())
            .with_simple_nonce_management()
            .connect_http(provider_url.parse().unwrap());

        let tea_bot = TeaBot::new(
            wallet_address.parse().unwrap(),
            DynProvider::new(wallet_client.clone()),
        );
        Self {
            instance: tea_bot,
            balances: HashMap::new(),
            users,
        }
    }

    pub async fn refresh_balances(&mut self) -> Result<HashMap<User, f64>, Error> {
        let addresses = self
            .users
            .iter()
            .map(|user| user.address.parse().unwrap())
            .collect::<Vec<Address>>();

        let balances = self
            .instance
            .mass_balance(addresses.clone())
            .call()
            .await?
            .to_vec();

        for (i, balance) in balances.iter().enumerate() {
            let balance_f64 = balance.to::<u128>() as f64 / 1e18;
            self.balances.insert(self.users[i].clone(), balance_f64);
        }
        tracing::info!("Balances: {:?}", self.balances);
        Ok(self.balances.clone())
    }

    pub fn get_balance(&self, id: String) -> Option<f64> {
        let balance = *self.balances.get(
            &self
                .users
                .iter()
                .find(|user| user.id == id)
                .unwrap()
                .clone(),
        )?;

        Some(balance)
    }

    pub async fn transfer(
        &self,
        payments: Vec<(Address, Address, f64)>,
    ) -> Result<TransactionReceipt, Error> {
        let payments = payments
            .into_iter()
            .map(|(from, to, amount)| {
                let amount_wei = (amount * 1e18).round() as u128;
                (to, from, U256::from(amount_wei))
            })
            .collect::<Vec<(Address, Address, U256)>>();

        Ok(self
            .instance
            .mass_transfer(payments)
            .send()
            .await?
            .get_receipt()
            .await?)
    }
}
