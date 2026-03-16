use std::{collections::HashMap, time::Instant};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::contract::ContractInterface;
use crate::slack::{SlackAction, UserCommand};
use crate::User;

const SASHA_NAME: &str = "sasha";
const MARTYN_NAME: &str = "martyn";

pub struct TeaRound {
    pub bids: HashMap<User, u8>,
    pub start_time: Instant,
}

pub struct Tea {
    pub message_tx: mpsc::UnboundedSender<SlackAction>,
    pub command_rx: mpsc::UnboundedReceiver<UserCommand>,
    pub tea_round: Option<TeaRound>,
    pub contract: ContractInterface,
}

impl Tea {
    pub fn new(
        message_tx: mpsc::UnboundedSender<SlackAction>,
        command_rx: mpsc::UnboundedReceiver<UserCommand>,
        contract: ContractInterface,
    ) -> Self {
        Self {
            message_tx,
            command_rx,
            tea_round: None,
            contract,
        }
    }

    pub async fn run(&mut self) {
        loop {
            if let Some(ref tea_round) = self.tea_round {
                let elapsed = tea_round.start_time.elapsed();

                tokio::select! {
                    Some(command) = self.command_rx.recv() => {
                        self.handle_command(command).await;
                    }
                    _ = sleep(Duration::from_secs(45).saturating_sub(elapsed)) => {
                        let _ = self.end_tea_round().await;
                    }
                }
            } else {
                if let Some(command) = self.command_rx.recv().await {
                    self.handle_command(command).await;
                }
            }
        }
    }

    fn calculate_forced_payments(
        &self,
        from_user: &User,
        to_user: &User,
        amount: f64,
    ) -> HashMap<User, f64> {
        HashMap::from([(from_user.clone(), -amount), (to_user.clone(), amount)])
    }

    async fn handle_command(&mut self, command: UserCommand) {
        match command {
            UserCommand::Bid(user, bid, response_url) => {
                if let Some(tea_round) = self.tea_round.as_mut() {
                    if let Some(balance) = self.contract.get_balance(user.id.clone()) {
                        if balance < bid.into() {
                            SlackAction::RejectBid(
                                format!("☕️ Insufficient balance. You have {} TEA 🚨", balance),
                                response_url,
                            )
                            .send(&self.message_tx);
                            return;
                        }
                    }
                    if let Some(bid) = tea_round.bids.get(&user) {
                        SlackAction::RejectBid(
                            format!(
                                "☕️ You have already bid {:.1} TEA. That's locked in now! 🚨",
                                bid
                            ),
                            response_url,
                        )
                        .send(&self.message_tx);
                        return;
                    }

                    tea_round.bids.insert(user.clone(), bid);

                    SlackAction::ConfirmBid(user.clone(), response_url).send(&self.message_tx);
                } else {
                    if let Err(e) = self.contract.refresh_balances().await {
                        tracing::error!("Failed to refresh balances 🚨: {}", e);

                        SlackAction::RejectBid(
                            "☕️ Failed to refresh balances 🚨".to_string(),
                            response_url,
                        )
                        .send(&self.message_tx);
                        return;
                    }
                    if let Some(balance) = self.contract.get_balance(user.id.clone()) {
                        if balance < bid.into() {
                            SlackAction::RejectBid(
                                format!("☕️ Insufficient balance. You have {} TEA 🚨", balance),
                                response_url,
                            )
                            .send(&self.message_tx);
                            return;
                        }
                    }

                    self.tea_round = Some(TeaRound {
                        bids: HashMap::from([(user.clone(), bid)]),
                        start_time: Instant::now(),
                    });

                    SlackAction::StartTeaRound(user.clone()).send(&self.message_tx);
                    SlackAction::ConfirmBid(user.clone(), response_url).send(&self.message_tx);
                }
            }
            UserCommand::CancelTeaRound => {
                tracing::info!("Cancelled tea round");
                self.tea_round = None;
                SlackAction::CancelTeaRound.send(&self.message_tx);
            }
        }
    }

    async fn end_tea_round(&mut self) {
        if let Some(tea_round) = self.tea_round.take() {
            let bids = tea_round.bids.clone();
            if bids.len() == 1 {
                SlackAction::SendMessage(format!(
                    "No one joined your tea round, {}! Go and treat yourself to a lonely tea.",
                    bids.keys().next().unwrap()
                ))
                .send(&self.message_tx);
                return;
            }

            let lowest_bid = bids
                .values()
                .min_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap();

            let lowest_bidders = tea_round
                .bids
                .iter()
                .filter(|(_, bid)| *bid == lowest_bid)
                .map(|(user, _)| user)
                .collect::<Vec<_>>();

            SlackAction::RevealBids(bids.clone().into_iter().collect()).send(&self.message_tx);

            let tea_maker = if lowest_bidders.len() > 1 {
                let mut rollers: Vec<User> = lowest_bidders.iter().map(|u| (*u).clone()).collect();
                SlackAction::AnnounceDiceRoll(rollers.clone(), *lowest_bid).send(&self.message_tx);

                loop {
                    let rolls: Vec<(User, Vec<u8>)> = rollers
                        .iter()
                        .map(|user| {
                            (
                                user.clone(),
                                (0..3)
                                    .map(|_| rand::random::<u8>() % 6 + 1)
                                    .collect::<Vec<u8>>(),
                            )
                        })
                        .collect();

                    SlackAction::RollDice(rolls.clone()).send(&self.message_tx);

                    let lowest_score_sum: u8 = rolls
                        .iter()
                        .map(|(_, dice)| dice.iter().sum::<u8>())
                        .min()
                        .unwrap();

                    let lowest_rollers: Vec<User> = rolls
                        .iter()
                        .filter(|(_, dice)| dice.iter().sum::<u8>() == lowest_score_sum)
                        .map(|(user, _)| user.clone())
                        .collect();

                    rollers = lowest_rollers;

                    if rollers.len() == 1 {
                        break;
                    } else {
                        SlackAction::AnnounceDiceRollTie(rollers.clone()).send(&self.message_tx);
                    }
                }

                rollers[0].clone()
            } else {
                (*lowest_bidders[0]).clone()
            };

            SlackAction::AnnounceTeaMaker((tea_maker, *lowest_bid, bids.len()))
                .send(&self.message_tx);

            let mut payments = HashMap::new();
            let mut transfers = Vec::new();

            if self.contract.refresh_balances().await.is_ok() {
                let sasha = self.contract.get_user_by_name(SASHA_NAME);
                let martyn = self.contract.get_user_by_name(MARTYN_NAME);

                if let (Some(sasha), Some(martyn)) = (sasha, martyn) {
                    let amount = self.contract.get_balance_by_name(SASHA_NAME).unwrap_or(0.0);
                    payments = self.calculate_forced_payments(&sasha, &martyn, amount);

                    if amount > 0.0 {
                        transfers.push((
                            sasha.address.parse().unwrap(),
                            martyn.address.parse().unwrap(),
                            amount,
                        ));
                    }
                }
            }

            SlackAction::AnnouncePayments(payments).send(&self.message_tx);

            if !transfers.is_empty() {
                match self.contract.transfer(transfers).await {
                    Ok(_) => {
                        SlackAction::SendMessage("☕️ *All transfers successful ✅*".to_string())
                            .send(&self.message_tx);
                    }
                    Err(e) => {
                        SlackAction::SendMessage(format!("☕️ *Failed to transfer 🚨:* {}", e))
                            .send(&self.message_tx);
                    }
                }
            } else {
                SlackAction::SendMessage("☕️ *No transfers to be made ✅*".to_string())
                    .send(&self.message_tx);
            }

            match self.contract.refresh_balances().await {
                Ok(new_balances) => {
                    SlackAction::ShowTeaderboard(new_balances.into_iter().collect())
                        .send(&self.message_tx);
                }
                Err(e) => {
                    tracing::error!("Failed to refresh balances 🚨: {}", e);
                    SlackAction::SendMessage(format!("☕️ *Failed to refresh balances 🚨:* {}", e))
                        .send(&self.message_tx);
                }
            }
        }
    }
}
