# Mini w/g vs Codex realworld scoreboard

Squeezy gpt-5.4-mini with-graph vs Codex CLI baseline. Medians over 3 reps. WIN iff recall ≥ 95% AND cost ≤ 0.95× codex.

| Lang | sqz w/g recall | sqz w/g cost | codex cost | w/g vs codex | sqz n/g recall | sqz n/g cost | Verdict |
|------|--------------:|-------------:|-----------:|-------------:|--------------:|-------------:|:-------:|
| swift | 100.0% | $0.0167 | $0.0281 | 0.59× | 100.0% | $0.0188 | **WIN** |
| go | 100.0% | $0.0788 | $0.0250 | 3.15× | 100.0% | $0.0478 | LOSS |
| cpp | 100.0% | $0.0500 | $0.0541 | 0.92× | 100.0% | $0.0610 | **WIN** |
| csharp | 100.0% | $0.0719 | $0.0341 | 2.11× | 100.0% | $0.0629 | LOSS |
| java | 100.0% | $0.1026 | $0.0497 | 2.06× | 83.3% | $0.0564 | LOSS |
| js | 100.0% | $0.0337 | $0.0212 | 1.59× | 0.0% | $0.0000 | LOSS |
| python | 0.0% | $0.0137 | $0.0209 | 0.66× | 0.0% | $0.0126 | LOSS |
| ruby | 100.0% | $0.0313 | $0.0473 | 0.66× | 100.0% | $0.0315 | **WIN** |
| php | 100.0% | $0.0533 | $0.0351 | 1.52× | 100.0% | $0.0336 | LOSS |
| kotlin | 100.0% | $0.0257 | $0.0248 | 1.04× | 100.0% | $0.0464 | TIE |
| scala | 100.0% | $0.0540 | $0.0307 | 1.76× | 100.0% | $0.0595 | LOSS |
| dart | 100.0% | $0.1710 | $0.0233 | 7.34× | 94.4% | $0.1371 | LOSS |
| rust | 96.9% | $0.0309 | $0.0354 | 0.87× | 100.0% | $0.0159 | **WIN** |

**Tally:** 4 WIN / 1 TIE / 8 LOSS over 13 langs (c + ts not yet sweep-graded on mini).

**Mini-side trade-off:** see `cat-n-tradeoff.md`. Cat-n line-number prefix lifts Haiku recall (12/15 WIN vs Claude Code) but inflates mini cost because mini already had 100% recall without it. Post-fix mini data on ruby/scala/php/python/kotlin confirms the regression.

**Open LOSS root causes:** see `adversarial-diagnoses-mini-losses.md`. Three subagent-driven diagnoses on java / scala / js. All single-cause hypotheses refuted; each cell has a distinct mechanism (java=decl_search wire weight, scala=runaway grep, js=haiku-delegate enumeration). Follow-up PRs will land medium-risk fixes.

**Fresh post-fix data:** go, csharp, ruby (flipped to WIN), scala, php, python (grader mismatch — separate issue), kotlin. The remaining 8 langs use prior pre-fix sweep data.
