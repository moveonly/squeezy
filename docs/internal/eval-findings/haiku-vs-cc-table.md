# Haiku-vs-CC realworld scoreboard

Per-language medians across 3 reps. Squeezy is Haiku 4.5 driving the squeezy agent in `with-graph` and `no-graph` variants; CC is Claude Code `--bare` with the same model. Cost in USD, recall in %, recall-weighted cost (RWC) is `cost / (recall/100)`. Verdict criteria: squeezy w/g WINS iff `recall_drop <= 5pp` AND `wg_rwc <= 0.95 * cc_rwc`; LOSS if `recall_drop > 5pp` OR `wg_rwc > 1.05 * cc_rwc`; TIE otherwise.

| Lang | CC cost | CC recall | sqz w/g cost | sqz w/g recall | sqz n/g cost | sqz n/g recall | w/g cheaper vs CC (RWC ratio) | Verdict |
|------|--------:|----------:|-------------:|---------------:|-------------:|---------------:|-------------------------------:|:-------:|
| rust | $0.1734 | 100.0% | $0.1028 | 100.0% | $0.1593 | 100.0% | 1.69x | **WIN** |
| go | $0.1781 | 100.0% | $0.0880 | 92.9% | $0.0211 | 100.0% | 1.88x | **LOSS** |
| cpp | $0.2382 | 100.0% | $0.2110 | 100.0% | $0.1499 | 100.0% | 1.13x | **WIN** |
| csharp | $0.1818 | 100.0% | $0.2031 | 100.0% | $0.1156 | 100.0% | 0.90x | **LOSS** |
| java | $0.2441 | 100.0% | $0.1821 | 100.0% | $0.1418 | 77.8% | 1.34x | **WIN** |
| js | $0.0491 | 100.0% | $0.0280 | 95.5% | $0.0231 | 59.1% | 1.67x | **WIN** |
| ts | $0.1742 | 100.0% | $0.1776 | 100.0% | $0.2429 | 100.0% | 0.98x | **TIE** |
| python | $0.0819 | 100.0% | $0.0455 | 100.0% | $0.0309 | 95.0% | 1.80x | **WIN** |
| ruby | $0.2426 | 100.0% | $0.1524 | 100.0% | $0.0736 | 100.0% | 1.59x | **WIN** |
| php | $0.1875 | 100.0% | $0.2480 | 100.0% | $0.1142 | 100.0% | 0.76x | **LOSS** |
| kotlin | $0.1324 | 100.0% | $0.1306 | 100.0% | $0.1423 | 100.0% | 1.01x | **TIE** |
| swift | $0.0337 | 100.0% | $0.0252 | 20.8% | $0.0233 | 41.7% | 0.28x | **LOSS** |
| scala | $0.3541 | 100.0% | $0.2656 | 100.0% | $0.1959 | 100.0% | 1.33x | **WIN** |
| dart | $0.3029 | 94.4% | $0.1545 | 66.7% | $0.2250 | 66.7% | 1.39x | **LOSS** |
| c | $0.2288 | 100.0% | $0.0785 | 100.0% | $0.2176 | 100.0% | 2.91x | **WIN** |

**Tally (squeezy w/g vs CC):** 8 WIN, 2 TIE, 5 LOSS over 15 graded langs.

**Losses driven by:** go (recall_drop=+7.1pp; rwc_ratio=0.53); csharp (recall_drop=+0.0pp; rwc_ratio=1.12); php (recall_drop=+0.0pp; rwc_ratio=1.32); swift (recall_drop=+79.2pp; rwc_ratio=3.60); dart (recall_drop=+27.7pp; rwc_ratio=0.72).

## Summary

Across 15 languages on the realworld benchmark with Haiku 4.5, squeezy `with-graph` wins on 8, ties on 2, and loses on 5. Median cost (per-lang medians, then median across langs): CC $0.1818 vs squeezy w/g $0.1524 (1.19x cheaper). Median recall: CC 100.0% vs w/g 100.0%. Wins are split between cost wins (rust, cpp, java, python, ruby, scala, c) at full recall and js where w/g trades a 4.5pp recall hit for 43% cost savings (within the 5pp tolerance). Losses cluster around two failure modes: (1) recall regressions on large/messy answer surfaces — swift (20pp recall floor vs CC's 100%) and dart (28pp drop) — where Haiku appears to truncate or miscount sites under squeezy's tighter context budget; (2) cost regressions at parity recall — csharp, php — where squeezy's extra plumbing overhead exceeds CC's `--bare` loop on tasks Haiku can solve in few turns. go is the marginal LOSS: w/g is 47% cheaper but loses a single row in 2/3 reps (7.1pp drop > 5pp threshold). `no-graph` (n/g) outperforms w/g on cost in most langs because Haiku hammers grep before hitting structured tools, but tanks recall on java (77.8%), js (59.1%), swift, and dart, which is the structural argument for keeping the graph half — the wins it delivers (rust, cpp, java w/g restoring 100% recall vs n/g 77.8%) cost the graph overhead it pays elsewhere.
