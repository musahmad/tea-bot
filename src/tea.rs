use std::{collections::HashMap, time::Instant};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use tokio::sync::broadcast;

use crate::contract::ContractInterface;
use crate::slack::{SlackAction, UserCommand};
use crate::tv::TvEvent;
use crate::User;

pub struct TeaRound {
    pub bids: HashMap<User, u8>,
    pub start_time: Instant,
}

pub struct Tea {
    pub message_tx: mpsc::UnboundedSender<SlackAction>,
    pub command_rx: mpsc::UnboundedReceiver<UserCommand>,
    pub tea_round: Option<TeaRound>,
    pub contract: ContractInterface,
    pub tv_tx: broadcast::Sender<TvEvent>,
}

impl Tea {
    pub fn new(
        message_tx: mpsc::UnboundedSender<SlackAction>,
        command_rx: mpsc::UnboundedReceiver<UserCommand>,
        contract: ContractInterface,
        tv_tx: broadcast::Sender<TvEvent>,
    ) -> Self {
        Self {
            message_tx,
            command_rx,
            tea_round: None,
            contract,
            tv_tx,
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

    fn calculate_payments(
        &self,
        bids: &HashMap<User, u8>,
        lowest_bidder: &User,
        penalty: f64,
    ) -> HashMap<User, f64> {
        let sum = bids.values().sum::<u8>() as f64;

        let mut distribution = bids
            .iter()
            .map(|(user, bid)| {
                (
                    user.clone(),
                    ((sum - *bid as f64) / (bids.len() - 1) as f64) - *bid as f64,
                )
            })
            .collect::<HashMap<User, f64>>();

        for (user, amount) in distribution.iter_mut() {
            if user == lowest_bidder {
                *amount -= penalty;
            } else {
                *amount += penalty / (bids.len() - 1) as f64;
            }
        }

        distribution
    }

    fn calculate_transfers(&self, payments: &HashMap<User, f64>) -> HashMap<(User, User), f64> {
        let mut sorted_payments: Vec<(User, f64)> = payments
            .iter()
            .map(|(user, amount)| (user.clone(), *amount))
            .collect();
        sorted_payments.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let mut transfers = HashMap::new();
        let mut i = 0;
        let mut j = sorted_payments.len() - 1;

        while i < j {
            let receiver = sorted_payments[i].0.clone();
            let receiver_amount = sorted_payments[i].1;

            let payer = sorted_payments[j].0.clone();
            let payer_amount = sorted_payments[j].1.abs();

            if receiver_amount == 0.0 {
                i += 1;
                continue;
            }
            if payer_amount == 0.0 {
                j -= 1;
                continue;
            }

            let transfer_amount = receiver_amount.min(payer_amount);

            transfers.insert((payer.clone(), receiver.clone()), transfer_amount);

            sorted_payments[i].1 -= transfer_amount;
            sorted_payments[j].1 += transfer_amount;

            if sorted_payments[i].1 == 0.0 {
                i += 1;
            }
            if sorted_payments[j].1 == 0.0 {
                j -= 1;
            }
        }

        transfers
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

                    SlackAction::ConfirmBid(response_url).send(&self.message_tx);
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
                    let _ = self.tv_tx.send(TvEvent::TeaRoundStarted {
                        started_by: user.to_string(),
                    });
                    SlackAction::StartTimer {
                        title: "Bidding closes in".to_string(),
                        duration_secs: 45,
                        completion_message: None,
                    }
                    .send(&self.message_tx);
                    SlackAction::ConfirmBid(response_url).send(&self.message_tx);
                }
            }
            UserCommand::CancelTeaRound => {
                tracing::info!("Cancelled tea round");
                self.tea_round = None;
                SlackAction::CancelTeaRound.send(&self.message_tx);
                let _ = self.tv_tx.send(TvEvent::TeaRoundCancelled);
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

            let penalty = (rand::random::<u8>() % 6 + 1) as f64;

            let payments = self.calculate_payments(&bids, &tea_maker, penalty);
            let transfers: HashMap<(User, User), f64> = self.calculate_transfers(&payments);

            SlackAction::AnnounceTeaMaker((tea_maker.clone(), *lowest_bid, bids.len()))
                .send(&self.message_tx);
            let _ = self.tv_tx.send(TvEvent::TeaMakerAnnounced {
                maker: tea_maker.to_string(),
                bid: *lowest_bid,
                cups: bids.len(),
            });
            SlackAction::AnnouncePenalty(penalty).send(&self.message_tx);
            SlackAction::AnnouncePayments(payments).send(&self.message_tx);
            SlackAction::StartTimer {
                title: format!("{} is brewing tea", tea_maker),
                duration_secs: 5 * 60,
                completion_message: Some(format!(
                    "\n🍵 *Tea should be ready! Brewed by {}.*\n",
                    tea_maker
                )),
            }
            .send(&self.message_tx);

            if transfers.len() > 0 {
                match self
                    .contract
                    .transfer(
                        transfers
                            .iter()
                            .map(|((from, to), amount)| {
                                (
                                    from.address.parse().unwrap(),
                                    to.address.parse().unwrap(),
                                    *amount,
                                )
                            })
                            .collect(),
                    )
                    .await
                {
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
                    let balances: Vec<(User, f64)> = new_balances.into_iter().collect();
                    SlackAction::ShowTeaderboard(balances.clone()).send(&self.message_tx);
                    let _ = self.tv_tx.send(TvEvent::Teaderboard {
                        entries: balances
                            .iter()
                            .map(|(u, b)| (u.to_string(), *b))
                            .collect(),
                    });
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
