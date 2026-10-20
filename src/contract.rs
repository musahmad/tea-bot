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
    /// Exact on-chain balances, for capping transfers without float rounding.
    balances_wei: HashMap<Address, U256>,
    users: Vec<User>,
}

alloy::sol!(
    #[sol(rpc)]
    #[derive(Debug)]
    TeaBot,
    "abi.json"
);

/// Result of settling a batch of transfers on-chain.
pub struct TransferOutcome {
    /// Hash of the `mass_transfer` transaction.
    pub tx_hash: String,
    /// Amounts actually submitted, parallel to the input `payments`.
    /// A payer's transfers draw down their balance in order,
    /// so a transfer can be below the requested amount or zero.
    pub settled: Vec<f64>,
}

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
            balances_wei: HashMap::new(),
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
            self.balances
                .insert(self.users[i].clone(), wei_to_tea(*balance));
            self.balances_wei.insert(addresses[i], *balance);
        }
        tracing::info!("Balances: {:?}", self.balances);
        Ok(self.balances.clone())
    }

    /// Balances as of the last refresh.
    pub fn balances(&self) -> &HashMap<User, f64> {
        &self.balances
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
    ) -> Result<TransferOutcome, Error> {
        let capped = cap_to_balances(&payments, &self.balances_wei);
        let settled = capped.iter().map(|&wei| wei_to_tea(wei)).collect();
        let payments = payments
            .iter()
            .zip(capped)
            .map(|(&(from, to, _), wei)| (to, from, wei))
            .collect::<Vec<(Address, Address, U256)>>();

        let receipt: TransactionReceipt = self
            .instance
            .mass_transfer(payments)
            .send()
            .await?
            .get_receipt()
            .await?;

        Ok(TransferOutcome {
            tx_hash: format!("{:?}", receipt.transaction_hash),
            settled,
        })
    }
}

fn wei_to_tea(wei: U256) -> f64 {
    wei.to::<u128>() as f64 / 1e18
}

/// Caps payments so a payer never sends more than their balance in total.
/// A payer who cannot cover everything pays each recipient the same fraction
/// (balance / total owed) and is drained; the batch would revert otherwise.
/// A payer with no known balance is not capped.
fn cap_to_balances(
    payments: &[(Address, Address, f64)],
    balances_wei: &HashMap<Address, U256>,
) -> Vec<U256> {
    let wanted: Vec<U256> = payments
        .iter()
        .map(|&(_, _, amount)| U256::from((amount * 1e18).round() as u128))
        .collect();

    let mut owed: HashMap<Address, U256> = HashMap::new();
    for (&(from, _, _), &wei) in payments.iter().zip(&wanted) {
        *owed.entry(from).or_default() += wei;
    }

    let mut capped: Vec<U256> = payments
        .iter()
        .zip(&wanted)
        .map(|(&(from, _, _), &wei)| match balances_wei.get(&from) {
            Some(&balance) if owed[&from] > balance => wei * balance / owed[&from],
            _ => wei,
        })
        .collect();

    // Flooring leaves a few wei behind; the payer's last payment takes them.
    for (from, total) in owed {
        let Some(&balance) = balances_wei.get(&from).filter(|&&b| total > b) else {
            continue;
        };
        let indices: Vec<usize> = (0..payments.len())
            .filter(|&i| payments[i].0 == from)
            .collect();
        let paid: U256 = indices.iter().map(|&i| capped[i]).sum();
        capped[*indices.last().unwrap()] += balance - paid;
    }
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOSER: Address = Address::repeat_byte(1);
    const A: Address = Address::repeat_byte(2);
    const B: Address = Address::repeat_byte(3);

    fn tea(amount: f64) -> U256 {
        U256::from((amount * 1e18).round() as u128)
    }

    #[test]
    fn short_payer_pays_proportionally_and_is_drained() {
        // The 6 TEA loser owing 10.3 across several winners.
        let payments = [(LOSER, A, 2.5), (LOSER, B, 1.3), (LOSER, A, 6.5)];
        let balances = HashMap::from([(LOSER, tea(6.0))]);
        let capped = cap_to_balances(&payments, &balances);

        assert_eq!(capped.iter().sum::<U256>(), tea(6.0));
        for (&(_, _, owed), &paid) in payments.iter().zip(&capped) {
            let expected = owed * 6.0 / 10.3;
            assert!(
                (wei_to_tea(paid) - expected).abs() < 1e-12,
                "{owed} -> {paid}"
            );
        }
    }

    #[test]
    fn never_exceeds_exact_wei_balance() {
        let balance = U256::from(6_000_000_000_000_000_001u128);
        let capped = cap_to_balances(&[(LOSER, A, 6.1)], &HashMap::from([(LOSER, balance)]));
        assert_eq!(capped, vec![balance]);
    }

    #[test]
    fn covered_payments_are_unchanged() {
        let payments = [(LOSER, A, 1.3), (B, A, 0.2)];
        let balances = HashMap::from([(LOSER, tea(10.0)), (B, tea(0.2))]);
        assert_eq!(
            cap_to_balances(&payments, &balances),
            vec![tea(1.3), tea(0.2)]
        );
    }
}
