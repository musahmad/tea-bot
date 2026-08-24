#!/usr/bin/env python3
"""Re-parse the #t archive and print summary counts.

Run after appending new data. If the totals move in a way you didn't expect,
the parse broke rather than the game changing. See HANDOVER.md.
"""
import collections, math, os, re, sys

HERE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")

# Every handle a person ever appeared under. Canonicalise before aggregating.
ALIAS = {
    "mumu": "musa", "mumu-the-bull": "musa", "mumu_the_bull": "musa",
    "m": "marcus", "beardo": "marcus",
    "wisdom": "jem", "retardio": "jem", "jem-will-fix-it": "jem",
    "alexander.stepanov": "sasha", "angry-sasha": "sasha", "ss": "sasha",
    "alexwilliams0712": "alex", "bigger-show": "alex",
    "stephen285": "stephen", "twmeggs": "tim", "martan": "martyn",
    "aaa": "aatif", "lawrence-magic": "laurence",
}
PEOPLE = {"musa", "marcus", "jem", "sasha", "alex", "stephen", "tim", "martyn",
          "aatif", "laurence", "megan", "tania", "adam", "laura", "eoin"}

def canon(token):
    """Handle -> person, or None if the token isn't a name."""
    t = re.sub(r"[^a-z0-9.\-]", "", token.strip().lower())
    t = ALIAS.get(t, t)
    return t if t in PEOPLE else None

def read(name):
    path = os.path.join(HERE, name)
    with open(path) as fh:
        rows = [l.rstrip("\n").split("\t") for l in fh if l.strip()]
    return rows[0], [r for r in rows[1:] if not r[0].startswith("#")]

# A round the bot actually drew a loser for, as opposed to one somebody
# volunteered for. Only these are meaningful for luck.
VOLUNTEERED = ("offered", "no takers", "declined", "bot ")

def rounds():
    out = []
    _, rolls = read("rolls.tsv")
    for r in rolls:
        players = [p for p in (canon(t) for t in re.split(r"[,\s]+", r[1])) if p]
        loser = canon(r[3])
        if loser and loser not in players:
            players.append(loser)
        out.append(dict(dt=r[0], era=1 if r[0] < "2025-06-09" else 2,
                        players=list(dict.fromkeys(players)), loser=loser,
                        cups=int(r[4]) if r[4].isdigit() else 0, bids=None,
                        drawn=not any(v in r[1] for v in VOLUNTEERED)))
    _, bids = read("rounds.tsv")
    for r in bids:
        b = {}
        for m in re.finditer(r"([A-Za-z][\w\-.]*)=(-?[\d.]+)", r[1]):
            p = canon(m.group(1))
            if p:
                b[p] = float(m.group(2))
        loser = canon(r[3])
        players = list(b) or ([loser] if loser else [])
        if loser and loser not in players:
            players.append(loser)
        out.append(dict(dt=r[0], era=3, players=players, loser=loser,
                        cups=int(r[5]) if r[5].isdigit() else 0, bids=b,
                        drawn=len(b) >= 2))
    out.sort(key=lambda x: x["dt"])
    return out

def main():
    R = rounds()
    contested = [x for x in R if x["drawn"] and len(x["players"]) >= 2]
    print(f"rounds            {len(R):>6}   "
          f"era I {sum(1 for x in R if x['era']==1)} / "
          f"II {sum(1 for x in R if x['era']==2)} / "
          f"III {sum(1 for x in R if x['era']==3)}")
    print(f"contested         {len(contested):>6}   (a real draw between 2+ people)")
    print(f"cups brewed       {sum(x['cups'] for x in R):>6}")
    print(f"span              {R[0]['dt']}  ->  {R[-1]['dt']}")

    # Every die face the bot printed, excluding the duplicate-entry bug rows —
    # those rolled six dice, so they aren't a fair 3d6 sample.
    faces = collections.Counter()
    _, rolls = read("rolls.tsv")
    for r in rolls:
        for m in re.finditer(r"=((?:\d\+)+\d)=", r[2]):
            dice = m.group(1).split("+")
            if len(dice) != 3:
                continue
            for d in dice:
                faces[int(d)] += 1
    n = sum(faces.values())
    chi = sum((faces[f] - n / 6) ** 2 / (n / 6) for f in range(1, 7))
    print(f"dice faces        {n:>6}   chi-square {chi:.2f} on 5 df "
          f"(5% critical 11.07)")

    print("\nloss vs expectation, contested rounds only")
    print(f"  {'player':<10}{'rounds':>7}{'lost':>6}{'expected':>10}{'obs/exp':>9}{'z':>7}")
    for p in PEOPLE:
        mine = [x for x in contested if p in x["players"]]
        if len(mine) < 25:
            continue
        obs = sum(1 for x in mine if x["loser"] == p)
        exp = sum(1 / len(x["players"]) for x in mine)
        var = sum((1 / len(x["players"])) * (1 - 1 / len(x["players"])) for x in mine)
        z = (obs - exp) / math.sqrt(var) if var else 0
        print(f"  {p:<10}{len(mine):>7}{obs:>6}{exp:>10.1f}{obs/exp:>9.2f}{z:>+7.2f}")

    # chain side
    _, tr = read("transfers.tsv")
    _, st = read("settlements.tsv")
    _, bl = read("balances_daily.tsv")
    print(f"\ntransfers         {len(tr):>6}   last block {tr[-1][1]}")
    print(f"settlements       {len(st):>6}")
    print(f"balance days      {len(bl):>6}   last {bl[-1][0]}")

    hdr, _ = read("balances_daily.tsv")
    total = sum(float(v) for v in bl[-1][1:])
    print(f"TEA held          {total:>9.2f}   across {len(hdr)-1} players")

if __name__ == "__main__":
    main()
