use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};
use reqwest::Url;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::contract::ContractInterface;
use crate::slack::{SlackAction, UserCommand};
use crate::User;

pub struct TeaRound {
    pub bids: HashMap<User, u8>,
    pub start_time: Instant,
}

/// A round that has finished settling, kept around so the slow-tea button can charge the loser.
pub struct SettledRound {
    pub participants: Vec<User>,
    pub loser: User,
    /// Slack ids of non-loser participants who have reported slow tea so far.
    pub voters: HashSet<String>,
}

impl SettledRound {
    /// Number of reports needed to charge the loser: 50% of the non-losers, floored, min 1.
    fn required_votes(&self) -> usize {
        let non_losers = self.participants.len().saturating_sub(1);
        (non_losers / 2).max(1)
    }
}

pub struct Tea {
    pub message_tx: mpsc::UnboundedSender<SlackAction>,
    pub command_rx: mpsc::UnboundedReceiver<UserCommand>,
    pub tea_round: Option<TeaRound>,
    pub settled: Option<SettledRound>,
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
            settled: None,
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
            }
            UserCommand::SlowTea {
                clicker,
                loser_id,
                response_url,
            } => {
                self.handle_slow_tea(clicker, loser_id, response_url).await;
            }
        }
    }

    async fn handle_slow_tea(&mut self, clicker: User, loser_id: String, response_url: Url) {
        let settled = match self.settled.as_mut() {
            Some(settled) => settled,
            None => {
                SlackAction::SlowTeaReject(
                    "☕️ There's no tea round to report as slow right now 🚨".to_string(),
                    response_url,
                )
                .send(&self.message_tx);
                return;
            }
        };

        if settled.loser.id != loser_id {
            SlackAction::SlowTeaReject(
                "🐢 That slow-tea report has already been claimed!".to_string(),
                response_url,
            )
            .send(&self.message_tx);
            return;
        }
        if clicker == settled.loser {
            SlackAction::SlowTeaReject(
                "🐢 You can't report your own slow tea! Get brewing!".to_string(),
                response_url,
            )
            .send(&self.message_tx);
            return;
        }
        if !settled.participants.iter().any(|user| user == &clicker) {
            SlackAction::SlowTeaReject(
                "☕️ Only people who were in the round can report slow tea 🚨".to_string(),
                response_url,
            )
            .send(&self.message_tx);
            return;
        }

        // Record this report; a slow tea is only charged once enough people agree.
        let newly_added = settled.voters.insert(clicker.id.clone());
        let required = settled.required_votes();
        let votes = settled.voters.len();
        let loser_display = settled.loser.to_string();

        if votes < required {
            let remaining = required - votes;
            let people = if remaining == 1 { "person" } else { "people" };
            let prefix = match newly_added {
                true => "🐢 Slow tea reported!",
                false => "🐢 You've already reported slow tea.",
            };
            SlackAction::SlowTeaReject(
                format!(
                    "{} {} more {} needed to charge {}.",
                    prefix, remaining, people, loser_display
                ),
                response_url,
            )
            .send(&self.message_tx);
            return;
        }

        // Enough people agree — take the round so it can only ever be charged once.
        let settled = self.settled.take().unwrap();
        let loser = settled.loser;
        let others: Vec<User> = settled
            .participants
            .into_iter()
            .filter(|user| user != &loser)
            .collect();

        SlackAction::SlowTeaResolved {
            response_url,
            voters: votes,
            loser: loser.clone(),
            count: others.len(),
        }
        .send(&self.message_tx);

        match self
            .contract
            .transfer(
                others
                    .iter()
                    .map(|to| (loser.address.parse().unwrap(), to.address.parse().unwrap(), 1.0))
                    .collect(),
            )
            .await
        {
            Ok(_) => {
                SlackAction::SendMessage("☕️ *Slow tea penalty transferred ✅*".to_string())
                    .send(&self.message_tx);
            }
            Err(e) => {
                SlackAction::SendMessage(format!(
                    "☕️ *Failed to transfer slow tea penalty 🚨:* {}",
                    e
                ))
                .send(&self.message_tx);
            }
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

            let dice = rand::random::<u8>() % 6 + 1;
            let penalty = dice as f64 * 0.5 * (bids.len() - 1) as f64;

            let payments = self.calculate_payments(&bids, &tea_maker, penalty);
            let transfers: HashMap<(User, User), f64> = self.calculate_transfers(&payments);

            SlackAction::AnnounceTeaMaker((tea_maker.clone(), *lowest_bid, bids.len()))
                .send(&self.message_tx);
            SlackAction::AnnouncePenalty {
                dice,
                players: bids.len(),
                penalty,
            }
            .send(&self.message_tx);
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
                    SlackAction::ShowTeaderboard(new_balances.into_iter().collect())
                        .send(&self.message_tx);
                }
                Err(e) => {
                    tracing::error!("Failed to refresh balances 🚨: {}", e);
                    SlackAction::SendMessage(format!("☕️ *Failed to refresh balances 🚨:* {}", e))
                        .send(&self.message_tx);
                }
            }

            let others = bids.keys().filter(|user| **user != tea_maker).count();
            self.settled = Some(SettledRound {
                participants: bids.keys().cloned().collect(),
                loser: tea_maker.clone(),
                voters: HashSet::new(),
            });
            SlackAction::OfferSlowTea {
                loser: tea_maker,
                others,
            }
            .send(&self.message_tx);
        }
    }
}
