# #t channel archive — handover

A complete, structured record of every message the tea bot ever posted in the
lo-tech Slack channel `#t`, cross-checked against the TEA token contract on
Arbitrum One.

**Coverage:** 2024-12-12 16:20 GMT (channel creation) → 2026-08-24 11:56 BST.
Nothing before that exists; the first message in the channel is Musa joining it.

The archive is finished up to that timestamp. To continue it, read
[Resuming](#resuming) — you only ever append forward.

---

## What's here

| File | Rows | One row is |
|---|---|---|
| `data/rolls.tsv` | 810 | a dice round (eras I–II) |
| `data/rounds.tsv` | 621 | a sealed-bid round (era III) |
| `data/events.tsv` | 161 | a rule change, outage, bug, join, donation, dispute |
| `data/transfers.tsv` | 1,518 | one ERC-20 `Transfer` event |
| `data/settlements.tsv` | 577 | one settlement transaction (groups the transfers) |
| `data/balances_daily.tsv` | 263 | per-player end-of-day balance, forward-filled |
| `data/names.tsv` | 15 | Slack handle / emoji name → person |

All files are tab-separated, UTF-8, sorted ascending by the first column, with a
single header line. `rolls`/`rounds`/`events` use a leading `#` on the header;
the three chain files do not (they came out of a different pipeline). Empty or
not-applicable cells are `-`.

### Schemas

**`rolls.tsv`** — `datetime, participants, rolls, loser(made tea), cups, notes`
- `datetime` — local channel time as Slack rendered it (GMT or BST, **not**
  normalised; see [Caveats](#caveats)).
- `participants` — comma-separated canonical names. Suffixes carry meaning and
  are load-bearing when parsing: `(offered)` = this person typed `ot` and
  volunteered, `(no takers)` = requested and nobody joined, `(offer declined)` =
  offered and nobody accepted, `(dupe)` = the duplicate-entry bug, `(bot silent)`
  / `(bot failed)` = the bot never responded.
- `rolls` — `name=d+d+d=total` joined by commas. Rerolls are appended after
  ` | REROLL ` (and ` | REROLL2 `). Before 2025-06-14 the bot only printed
  totals, so these rows read `name=total` with the note `(totals only)`.
- `loser(made tea)` — who brewed. For volunteered/no-taker rounds this is the
  volunteer, which is **not** a random draw — exclude these when measuring luck.
- `cups` — cups brewed, integer, `-` if the round was cancelled before it landed.

**`rounds.tsv`** — `datetime, bids, rolls, loser, loser_bid, cups, penalty, payments, teaderboard_after`
- `bids` — `name=amount` comma-separated. Amounts are TEA; fractional bids occur
  only in December 2025.
- `rolls` — populated only when a tie went to dice, same format as above.
- `penalty` — e.g. `d6x2=12`, or `(none)` before the loser penalty shipped
  (first seen 2026-04-07).
- `payments` — `from->to amount` pairs separated by `;` in early rows, later
  `name+amount` / `name-amount` deltas separated by commas, mirroring the change
  in how the bot printed settlements.
- `teaderboard_after` — the balance line the bot posted after the round, or `-`
  once the chain data made it redundant.

**`transfers.tsv`** — `utc_datetime, block, tx, from, to, amount_tea`
`from`/`to` are canonical names where the address is known, `MINT/BURN` for the
zero address, `TREASURY` for the deployer, otherwise the raw `0x…` prefix.
Amounts are decimal TEA (18 dp divided out). **UTC**, unlike the Slack files.

**`settlements.tsv`** — `utc_datetime, tx, n_transfers, total_tea, detail`
One row per `mass_transfer` call; `detail` is `from->to:amount` pairs joined by
`;`.

**`balances_daily.tsv`** — `date` plus one column per player, forward-filled.
A player's column is `0.00` for every day before they were first funded — that
is "not yet playing", not "broke". Compute troughs from first funding onward.

---

## Names

Everyone appears under several handles across the three eras. `data/names.tsv`
is the map; canonicalise before aggregating or you will double-count people.

```
mumu, :mumu_the_bull:           → musa
m, beardo, :beardo:             → marcus
:wisdom:, :retardio:            → jem
alexander.stepanov, :angry-sasha:, :ss:  → sasha
alexwilliams0712, :bigger-show: → alex
stephen285                      → stephen
twmeggs                         → tim
martan, :saint-martyn-of-afreetea: → martyn
aaa                             → aatif
:lawrence-magic:                → laurence
```

Era III switches from real names to emoji handles partway through (Martyn's
changes again on 2026-05-14). `rounds.tsv` has already been normalised to
canonical names in the `bids`, `rolls` and `loser` columns; the raw handles
survive in `payments` and `teaderboard_after`.

---

## Eras, and why they matter for parsing

The bot's output format changed twice. Any parser needs all three shapes.

| | Dates | Selection | Bot output |
|---|---|---|---|
| **I** | 2024-12-12 → 2025-06-09 | lowest 3d6 | `Dice roll results:` block, totals only, ties broken by list order |
| **II** | 2025-06-09 → 2025-12-09 | lowest 3d6 | per-die `:dice-N:` messages, tie rerolls, Teaderboard, king/bitch |
| **III** | 2025-12-09 → present | lowest sealed bid, dice on a tie | bid list + settlement, balances on-chain |

Dates that will bite you:
- **2025-06-09 15:52** — stats tracking begins (first Teaderboard).
- **2025-06-14 11:48** — dice output switches from totals to per-die.
- **2025-07-25** — the Teaderboard is **wiped**. Era II statistics are not
  continuous across this date; treat as IIa / IIb.
- **2025-12-09 13:46** — first bidding round.
- **2026-04-07** — the dice-based loser penalty appears (PR #10). Median bid
  jumps from 1 to 5 TEA; any before/after comparison should cut here.
- **~2026-04-30 14:40** — bot stops printing per-player "X has joined" lines.

---

## How it was collected

### Slack
- Channel `#t` = `C084Y1ASX9A`; bot user `t` = `U084VFPKR0S`.
- Read backwards from newest with the Slack MCP `slack_read_channel`,
  `limit: 100`, `response_format: concise`, following `pagination_info.cursor`.
  ~100 messages ≈ 1–4 days of history; the full backfill took ~120 pages.
- Rounds routinely straddle a page boundary. Write a placeholder row and patch
  it when the next page arrives, rather than dropping the round.

### Chain
- TEA is a plain ERC-20 at `0x7eab07a82f20c296aac0025e93296efce21d8be1`,
  Arbitrum One (chainId `0xa4b1`). The address is **not** in the repo —
  `config.json5` is gitignored — it was recovered from an Arbiscan link Marcus
  posted in-channel on 2025-12-05.
- `eth_getLogs` over the full range, topic
  `0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef`.
- Batched JSON-RPC returns **HTTP 403** on `arb1.arbitrum.io`. Use individual
  `eth_getBlockByNumber` calls with a `user-agent` header, rotating across
  `arb1.arbitrum.io/rpc`, `arbitrum.drpc.org`, `arbitrum-one.publicnode.com`.
- Settlements are `mass_transfer`, selector `0x4a1175a3`.

### Verification
Final on-chain balances match the last Teaderboard the bot posted to within the
1.0 TEA jem→stephen donation at 11:56 on the last day. If a future pull
disagrees with the bot's own numbers by more than a known donation, trust the
chain and record the discrepancy in `events.tsv` as `type=anomaly`.

---

## Resuming

Everything is append-only and sorted, so the last row of each file is the
resume point.

| File | Resume from |
|---|---|
| `rolls.tsv` | complete — era I/II ended `2025-12-09 11:09`, nothing further to add |
| `rounds.tsv` | `2026-08-24 11:39` |
| `events.tsv` | `2026-08-24 11:56` |
| `transfers.tsv` | block `497868208` (`2026-08-24 10:56:50` UTC) |
| `settlements.tsv` | tx `0x55fc3937…b949fdd` |
| `balances_daily.tsv` | `2026-08-24` |

1. Read `#t` forward from the last recorded timestamp.
2. Append new rounds to `rounds.tsv` and anything notable to `events.tsv`.
3. Re-run `eth_getLogs` from block `497868209` to head; append to
   `transfers.tsv`, regroup into `settlements.tsv`, extend
   `balances_daily.tsv`.
4. Run `python3 archive/verify.py` — it re-parses every file, checks the
   canonicalisation, and prints the summary counts. If the totals move in a way
   you didn't expect, the parse broke, not the game.

If the bot's output format changes again, add a row to the era table above
before you add data. The next person will need it.

## Caveats

- **Timestamps in the Slack files are channel-local and unnormalised** (GMT
  before ~30 Mar 2025 and after 26 Oct 2025, BST between). The chain files are
  UTC. Don't join the two on time without converting.
- Rounds cancelled with `c` are kept, with `cups = -`. They are real rounds and
  the bot counted them; exclude them yourself if you don't want them.
- The duplicate-entry bug (`(dupe)` rows) gave one player two entries and rolled
  the second one **six** dice. Those totals are real bot output and are left in;
  filter on the suffix if you're modelling dice.
- Era I `rolls` are totals only — there are no individual dice to test.
- `cups` for a "selfish"/"lonely" tea is 1 where the bot confirmed it and `-`
  where the round was cancelled first.
- A handful of era-II rounds were never credited on the Teaderboard because the
  bot silently dropped the command; these are marked in `notes` and will make
  reconstructed stats disagree slightly with the bot's own board.
