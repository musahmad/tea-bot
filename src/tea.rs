use reqwest::Url;
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::contract::ContractInterface;
use crate::preferences::{london_now_minutes, PreferenceStore};
use crate::rounds::{now_unix, RoundStore, RoundSummary};
use crate::slack::{SlackAction, UserCommand};
use crate::{FirestoreConfig, User};

pub struct TeaRound {
    pub bids: HashMap<User, u8>,
    pub start_time: Instant,
    /// Wall-clock start (Unix seconds), for persistence — `start_time` is a
    /// monotonic `Instant` and can't be turned into a timestamp.
    pub started_at_unix: u64,
    /// The user who opened the round by placing the first bid.
    pub starter: User,
    /// Slack id → end of their latest completed round, fetched once at round start.
    /// Only one round runs at a time, so it cannot go stale mid-round.
    pub cooldowns: HashMap<String, u64>,
}

pub struct Tea {
    pub message_tx: mpsc::UnboundedSender<SlackAction>,
    pub command_rx: mpsc::UnboundedReceiver<UserCommand>,
    pub tea_round: Option<TeaRound>,
    pub contract: ContractInterface,
    pub rounds: RoundStore,
    pub prefs: Arc<PreferenceStore>,
    /// Seconds a player MUST wait after a completed round before bidding again.
    /// 0 disables the cooldown.
    pub cooldown_secs: u64,
}

impl Tea {
    pub fn new(
        message_tx: mpsc::UnboundedSender<SlackAction>,
        command_rx: mpsc::UnboundedReceiver<UserCommand>,
        contract: ContractInterface,
        firestore: Option<FirestoreConfig>,
        prefs: Arc<PreferenceStore>,
        cooldown_secs: u64,
    ) -> Self {
        Self {
            message_tx,
            command_rx,
            tea_round: None,
            contract,
            rounds: RoundStore::new(firestore),
            prefs,
            cooldown_secs,
        }
    }

    /// Reject the bid if `user` is still cooling down from a recent round.
    fn reject_if_cooling_down(
        &self,
        user: &User,
        cooldowns: &HashMap<String, u64>,
        response_url: &Url,
    ) -> bool {
        let Some(&ended_at) = cooldowns.get(&user.id) else {
            return false;
        };
        let Some(remaining) = cooldown_remaining(ended_at, now_unix(), self.cooldown_secs) else {
            return false;
        };
        SlackAction::RejectBid(
            format!(
                "☕️ You're on cooldown — you can join another round in {}m {:02}s 🚨",
                remaining / 60,
                remaining % 60
            ),
            response_url.clone(),
        )
        .send(&self.message_tx);
        true
    }

    /// Group a round's participants by the tea each one wants *right now*
    /// (Europe/London), for the post-round order. Larger groups first. Anyone
    /// with no saved preference defaults to Normal (see `TeaPreference::resolve`).
    async fn tea_orders(&self, users: &[User]) -> Vec<(String, Vec<User>)> {
        let now = london_now_minutes();
        let mut groups: Vec<(String, Vec<User>)> = Vec::new();

        for user in users {
            let tea = self.prefs.get(&user.id).await.resolve(now).to_string();
            match groups.iter_mut().find(|(t, _)| *t == tea) {
                Some((_, members)) => members.push(user.clone()),
                None => groups.push((tea, vec![user.clone()])),
            }
        }

        groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        groups
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

    fn calculate_transfers(payments: &HashMap<User, f64>) -> HashMap<(User, User), f64> {
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
                if let Some(tea_round) = self.tea_round.as_ref() {
                    if self.reject_if_cooling_down(&user, &tea_round.cooldowns, &response_url) {
                        return;
                    }
                }
                if let Some(tea_round) = self.tea_round.as_mut() {
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
                    let cooldowns = if self.cooldown_secs == 0 {
                        HashMap::new()
                    } else {
                        self.rounds
                            .recent_participants(now_unix().saturating_sub(self.cooldown_secs))
                            .await
                    };
                    if self.reject_if_cooling_down(&user, &cooldowns, &response_url) {
                        return;
                    }
                    self.tea_round = Some(TeaRound {
                        bids: HashMap::from([(user.clone(), bid)]),
                        start_time: Instant::now(),
                        started_at_unix: now_unix(),
                        starter: user.clone(),
                        cooldowns,
                    });

                    SlackAction::StartTeaRound.send(&self.message_tx);
                    SlackAction::StartTimer {
                        title: "Bidding closes in".to_string(),
                        duration_secs: 45,
                        completion_message: None,
                    }
                    .send(&self.message_tx);
                    SlackAction::ConfirmBid(response_url).send(&self.message_tx);
                }
            }
            UserCommand::ShowBoard(response_url) => {
                if let Err(e) = self.contract.refresh_balances().await {
                    tracing::error!("Failed to refresh balances 🚨: {}", e);
                    SlackAction::RespondEphemeral(
                        "☕️ Couldn't reach the bank to refresh balances — try again in a moment. 🚨"
                            .to_string(),
                        response_url,
                    )
                    .send(&self.message_tx);
                    return;
                }
                SlackAction::RespondTeaderboard(self.contract.balances().clone(), response_url)
                    .send(&self.message_tx);
            }
            UserCommand::Donate {
                from,
                to,
                amount,
                message,
                response_url,
            } => {
                self.handle_donation(from, to, amount, message, response_url)
                    .await;
            }
        }
    }

    /// Settle a `/t donate` transfer. Refreshes balances, rejects an
    /// over-balance gift privately, then moves the TEA on-chain and announces it
    /// along with the donor's `message`, when they wrote one.
    async fn handle_donation(
        &mut self,
        from: User,
        to: User,
        amount: f64,
        message: Option<String>,
        response_url: Url,
    ) {
        if let Err(e) = self.contract.refresh_balances().await {
            tracing::error!("Failed to refresh balances 🚨: {}", e);
            SlackAction::RespondEphemeral(
                "☕️ Couldn't reach the bank to refresh balances — try again in a moment. 🚨"
                    .to_string(),
                response_url,
            )
            .send(&self.message_tx);
            return;
        }

        let balance = self.contract.get_balance(from.id.clone()).unwrap_or(0.0);
        if balance < amount {
            SlackAction::RespondEphemeral(
                format!(
                    "☕️ You only have {:.1} TEA — can't donate {:.1}. 🚨",
                    balance, amount
                ),
                response_url,
            )
            .send(&self.message_tx);
            return;
        }

        let transfer = vec![(
            from.address.parse().unwrap(),
            to.address.parse().unwrap(),
            amount,
        )];

        match self.contract.transfer(transfer).await {
            Ok(outcome) => {
                let settled = outcome.settled.first().copied().unwrap_or(amount);
                SlackAction::RespondEphemeral(
                    format!("✅ Donated {:.1} TEA to {}!", settled, to),
                    response_url,
                )
                .send(&self.message_tx);
                let mut announcement = format!(
                    "💸 *{} donated {:.1} TEA to {}!* What a legend.",
                    from, settled, to
                );
                if let Some(message) = message {
                    // Quote it so multi-line notes stay visually attached.
                    announcement += &format!("\n> {}", message.replace('\n', "\n> "));
                }
                SlackAction::SendMessage(announcement).send(&self.message_tx);
            }
            Err(e) => {
                tracing::error!("Donation transfer failed 🚨: {}", e);
                SlackAction::RespondEphemeral(
                    format!("☕️ Donation failed 🚨: {}", e),
                    response_url,
                )
                .send(&self.message_tx);
            }
        }
    }

    async fn end_tea_round(&mut self) {
        if let Some(tea_round) = self.tea_round.take() {
            let bids = tea_round.bids.clone();
            if bids.len() == 1 {
                let (user, bid) = bids.iter().next().unwrap();
                SlackAction::SendMessage(format!(
                    "☕️ No one joined your tea round, {}! You bid {} TEA. Go and treat yourself to a lonely tea.",
                    user, bid
                ))
                .send(&self.message_tx);
                let orders = self
                    .tea_orders(&bids.keys().cloned().collect::<Vec<_>>())
                    .await;
                SlackAction::ShowTeaOrders(orders.clone()).send(&self.message_tx);
                self.rounds
                    .record(RoundSummary::lonely(
                        tea_round.started_at_unix,
                        now_unix(),
                        &tea_round.starter,
                        &bids,
                        &orders,
                    ))
                    .await;
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

            // Captures every re-roll of a tie-break for the round record; stays
            // empty when there was a single lowest bidder.
            let mut rolloff_history: Vec<Vec<(User, Vec<u8>)>> = Vec::new();

            let lowest_bidder = if lowest_bidders.len() > 1 {
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

                    rolloff_history.push(rolls.clone());
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

            let payments = self.calculate_payments(&bids, &lowest_bidder, penalty);

            // Debt and transfer caps both use these balances; a donation since bidding changes them.
            // On failure the bid-time balances stand in.
            if let Err(e) = self.contract.refresh_balances().await {
                tracing::error!("Failed to refresh balances before settling 🚨: {}", e);
            }

            // A payer who cannot cover their payment would otherwise lose only their balance.
            // The deepest debtor makes the tea; the lowest bidder still pays the penalty.
            let debts = debtors(&payments, self.contract.balances());
            let tea_maker = match debts.first() {
                Some((debtor, debt)) if *debtor != lowest_bidder => {
                    SlackAction::SendMessage(format!(
                        "\n\n🚨 *{} bid beyond their means and is {:.1} TEA in debt. Debtors make the tea, whatever they bid!*\n",
                        debtor, debt
                    ))
                    .send(&self.message_tx);
                    debtor.clone()
                }
                _ => lowest_bidder,
            };
            let affordable = affordable_payments(&payments, &debts);
            let shortfall = shortfall_notice(&payments, &affordable, &debts);
            let transfers: HashMap<(User, User), f64> = Self::calculate_transfers(&affordable);

            SlackAction::AnnounceTeaMaker((tea_maker.clone(), bids[&tea_maker], bids.len()))
                .send(&self.message_tx);
            let orders = self
                .tea_orders(&bids.keys().cloned().collect::<Vec<_>>())
                .await;
            SlackAction::AnnouncePenalty {
                dice,
                players: bids.len(),
                penalty,
            }
            .send(&self.message_tx);
            SlackAction::AnnouncePayments(payments.clone()).send(&self.message_tx);
            SlackAction::StartTimer {
                title: format!("{} is brewing tea", tea_maker),
                duration_secs: 5 * 60,
                completion_message: Some(format!(
                    "\n☕️ *Tea should be ready! Brewed by {}.*\n",
                    tea_maker
                )),
            }
            .send(&self.message_tx);

            // Fix an order so `settled` lines up with each transfer in the record.
            let transfer_list: Vec<((User, User), f64)> = transfers.into_iter().collect();

            let (settled, tx_hash): (Vec<f64>, Option<String>) = if !transfer_list.is_empty() {
                let payment_args = transfer_list
                    .iter()
                    .map(|((from, to), amount)| {
                        (
                            from.address.parse().unwrap(),
                            to.address.parse().unwrap(),
                            *amount,
                        )
                    })
                    .collect();
                match self.contract.transfer(payment_args).await {
                    Ok(outcome) => {
                        SlackAction::SendMessage(
                            shortfall
                                .unwrap_or_else(|| "☕️ *All transfers successful ✅*".to_string()),
                        )
                        .send(&self.message_tx);
                        (outcome.settled, Some(outcome.tx_hash))
                    }
                    Err(e) => {
                        SlackAction::SendMessage(format!("☕️ *Failed to transfer 🚨:* {}", e))
                            .send(&self.message_tx);
                        (Vec::new(), None)
                    }
                }
            } else {
                SlackAction::SendMessage(
                    shortfall.unwrap_or_else(|| "☕️ *No transfers to be made ✅*".to_string()),
                )
                .send(&self.message_tx);
                (Vec::new(), None)
            };

            SlackAction::ShowTeaOrders(orders.clone()).send(&self.message_tx);

            // The cache still holds the balances from before the transfers.
            let previous = self.contract.balances().clone();
            let balances_after = match self.contract.refresh_balances().await {
                Ok(new_balances) => {
                    SlackAction::ShowTeaderboard {
                        balances: new_balances.clone(),
                        previous,
                    }
                    .send(&self.message_tx);
                    Some(new_balances)
                }
                Err(e) => {
                    tracing::error!("Failed to refresh balances 🚨: {}", e);
                    SlackAction::SendMessage(format!("☕️ *Failed to refresh balances 🚨:* {}", e))
                        .send(&self.message_tx);
                    None
                }
            };

            self.rounds
                .record(RoundSummary {
                    started_at_unix: tea_round.started_at_unix,
                    ended_at_unix: now_unix(),
                    status: "completed",
                    starter: &tea_round.starter,
                    bids: &bids,
                    teas: &orders,
                    lowest_bid: *lowest_bid,
                    rolloff: &rolloff_history,
                    loser: Some(&tea_maker),
                    penalty_dice: Some(dice),
                    penalty_amount: penalty,
                    payments: Some(&payments),
                    transfers: &transfer_list,
                    settled: &settled,
                    tx_hash,
                    balances_after: balances_after.as_ref(),
                })
                .await;
        }
    }
}

/// Payers whose payment exceeds their balance, deepest debt first.
/// A payer with no known balance is never in debt.
fn debtors(payments: &HashMap<User, f64>, balances: &HashMap<User, f64>) -> Vec<(User, f64)> {
    let mut debtors: Vec<(User, f64)> = payments
        .iter()
        .filter_map(|(user, &amount)| {
            let debt = -amount - balances.get(user)?;
            (debt > 1e-9).then(|| (user.clone(), debt))
        })
        .collect();
    debtors.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    debtors
}

/// Payments scaled to what can actually be paid. Debtors pay their whole balance
/// and every winner takes the same fraction of their winnings, so a shortfall
/// falls on all winners rather than only those paired with the short payer.
fn affordable_payments(payments: &HashMap<User, f64>, debts: &[(User, f64)]) -> HashMap<User, f64> {
    if debts.is_empty() {
        return payments.clone();
    }

    let mut affordable = payments.clone();
    for (debtor, debt) in debts {
        *affordable.get_mut(debtor).unwrap() += debt;
    }
    let collected = affordable
        .values()
        .filter(|&&a| a < 0.0)
        .fold(0.0, |total, a| total - a);
    let winnings: f64 = payments.values().filter(|&&a| a > 0.0).sum();
    for amount in affordable.values_mut().filter(|a| **a > 0.0) {
        *amount *= collected / winnings;
    }
    affordable
}

/// The Slack notice for a round where some payers couldn't pay in full.
fn shortfall_notice(
    payments: &HashMap<User, f64>,
    affordable: &HashMap<User, f64>,
    debts: &[(User, f64)],
) -> Option<String> {
    if debts.is_empty() {
        return None;
    }

    let mut winners: Vec<(&User, f64)> = payments
        .iter()
        .filter(|(_, &amount)| amount > 0.0)
        .map(|(user, &amount)| (user, amount))
        .collect();
    winners.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));

    let short_payers: Vec<String> = debts
        .iter()
        .map(|(payer, debt)| {
            let owed = -payments[payer];
            format!(
                "{} owed {:.1} TEA but only had {:.1}",
                payer,
                owed,
                owed - debt
            )
        })
        .collect();
    let winner_lines: Vec<String> = winners
        .iter()
        .map(|(winner, amount)| {
            format!(
                "{} gets {:.1} of {:.1} TEA",
                winner, affordable[*winner], amount
            )
        })
        .collect();
    let won: f64 = winners.iter().map(|(_, amount)| amount).sum();
    let paid: f64 = winners.iter().map(|(winner, _)| affordable[*winner]).sum();

    Some(format!(
        "☕️ *Not everyone could pay in full 🚨*\n{}\nAll winners get {:.0}% of their winnings:\n{}",
        short_payers.join("\n"),
        paid / won * 100.0,
        winner_lines.join("\n")
    ))
}

/// Seconds left on a cooldown that started at `ended_at`, or `None` once it has passed.
fn cooldown_remaining(ended_at: u64, now: u64, window: u64) -> Option<u64> {
    let remaining = ended_at
        .saturating_add(window)
        .saturating_sub(now)
        .min(window);
    (remaining > 0).then_some(remaining)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{affordable_payments, cooldown_remaining, debtors, shortfall_notice, Tea};
    use crate::User;

    fn user(name: &str) -> User {
        User {
            id: name.to_string(),
            name: name.to_string(),
            address: String::new(),
            emoji: None,
            image_url: None,
        }
    }

    #[test]
    fn deepest_debtor_first() {
        let (a, b, c) = (user("a"), user("b"), user("c"));
        let payments = HashMap::from([(a.clone(), -70.0), (b.clone(), -10.0), (c.clone(), 80.0)]);
        let balances = HashMap::from([(a.clone(), 20.0), (b.clone(), 0.0), (c.clone(), 0.0)]);
        let found = debtors(&payments, &balances);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, a);
        assert!((found[0].1 - 50.0).abs() < 1e-9);
        assert_eq!(found[1].0, b);
        assert!((found[1].1 - 10.0).abs() < 1e-9);
    }

    #[test]
    fn covered_or_unknown_payers_are_not_debtors() {
        let (a, b) = (user("a"), user("b"));
        let payments = HashMap::from([(a.clone(), -20.0), (b.clone(), 20.0)]);
        assert!(debtors(&payments, &HashMap::from([(a.clone(), 20.0)])).is_empty());
        assert!(debtors(&payments, &HashMap::new()).is_empty());
    }

    #[test]
    fn shortfall_falls_on_all_winners() {
        let (rich, broke, a, b) = (user("rich"), user("broke"), user("a"), user("b"));
        let payments = HashMap::from([
            (rich.clone(), -10.0),
            (broke.clone(), -10.0),
            (a.clone(), 15.0),
            (b.clone(), 5.0),
        ]);
        let balances = HashMap::from([(rich.clone(), 50.0), (broke.clone(), 2.0)]);
        let debts = debtors(&payments, &balances);
        let affordable = affordable_payments(&payments, &debts);

        assert!((affordable[&rich] + 10.0).abs() < 1e-9);
        assert!((affordable[&broke] + 2.0).abs() < 1e-9);
        assert!((affordable[&a] - 9.0).abs() < 1e-9);
        assert!((affordable[&b] - 3.0).abs() < 1e-9);

        // However the payers get paired, each winner receives their full share.
        let transfers = Tea::calculate_transfers(&affordable);
        for (winner, expected) in [(&a, 9.0), (&b, 3.0)] {
            let received: f64 = transfers
                .iter()
                .filter(|((_, to), _)| to == winner)
                .map(|(_, amount)| amount)
                .sum();
            assert!((received - expected).abs() < 1e-9);
        }
        for (payer, expected) in [(&rich, 10.0), (&broke, 2.0)] {
            let sent: f64 = transfers
                .iter()
                .filter(|((from, _), _)| from == payer)
                .map(|(_, amount)| amount)
                .sum();
            assert!((sent - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn covered_round_is_unchanged() {
        let (a, b) = (user("a"), user("b"));
        let payments = HashMap::from([(a.clone(), -20.0), (b.clone(), 20.0)]);
        let debts = debtors(&payments, &HashMap::from([(a.clone(), 20.0)]));
        let affordable = affordable_payments(&payments, &debts);
        assert_eq!(affordable, payments);
        assert_eq!(shortfall_notice(&payments, &affordable, &debts), None);
    }

    #[test]
    fn broke_payers_leave_nothing_to_transfer_but_still_notify() {
        let (a, b, c) = (user("a"), user("b"), user("c"));
        let payments = HashMap::from([(a.clone(), -6.0), (b.clone(), -4.0), (c.clone(), 10.0)]);
        let balances = HashMap::from([(a.clone(), 0.0), (b.clone(), 0.0)]);
        let debts = debtors(&payments, &balances);
        let affordable = affordable_payments(&payments, &debts);

        assert!(affordable.values().all(|&amount| amount == 0.0));
        assert!(Tea::calculate_transfers(&affordable).is_empty());
        let notice = shortfall_notice(&payments, &affordable, &debts).unwrap();
        assert!(notice.contains("All winners get 0%"), "{notice}");
    }

    #[test]
    fn cooldown_counts_down_from_round_end() {
        assert_eq!(cooldown_remaining(1_000, 1_000, 900), Some(900));
        assert_eq!(cooldown_remaining(1_000, 1_899, 900), Some(1));
        assert_eq!(cooldown_remaining(1_000, 1_900, 900), None);
        assert_eq!(cooldown_remaining(1_000, 5_000, 900), None);
    }

    #[test]
    fn cooldown_clamps_future_round_end_to_window() {
        // Clock skew: the round "ended" after now. Still at most one window away.
        assert_eq!(cooldown_remaining(2_000, 1_000, 900), Some(900));
    }

    #[test]
    fn zero_window_disables_cooldown() {
        assert_eq!(cooldown_remaining(1_000, 1_000, 0), None);
    }
}
