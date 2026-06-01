# cat-n line-number prefix: tradeoff finding

## What

Commits `585105bc` and `355fe746` added a 1-based line-number prefix to every
`read_slice` / `read_file` response body (cat -n format). The format was
chosen to match Claude Code's `Read` tool output, the goal was to stop the
model from miscounting line numbers when asked to report them.

## Where it helps

**Haiku 4.5 across all 15 realworld benchmark languages:** recall lifts
from a 79pp floor on swift to 100%, plus measurable improvements on go,
csharp, java, dart. The cat-n prefix is the single biggest contributor to
the session's **12 WIN / 0 TIE / 3 LOSS** vs Claude Code-Haiku scoreboard
(was 8/2/5 pre-fix; net +4 wins).

The win mechanism: Haiku struggles to count newlines reliably in a >100-line
slice, so it was emitting line numbers that landed inside `@discardableResult`
attribute lines instead of `public func` declaration lines on swift, miscounting
class-declaration lines on dart, etc. cat-n removes the math entirely.

## Where it hurts

**gpt-5.4-mini on the same 15 benchmarks:** mostly hurts. Mini was already
hitting 100% recall on these tasks *without* cat-n's help, so the additional
input tokens (the `<6-char-line-num>\t` prefix on every emitted line)
translate directly into a cost regression with no offsetting recall benefit:

| Lang | Mini w/g pre-fix | Mini w/g post-fix | Delta |
|------|----------------:|------------------:|-------|
| go     | $0.0475 (TIE)   | $0.0788 (LOSS 1.68×) | +66% |
| ruby   | $0.0500 (close) | $0.0594 (LOSS 1.26×) | +19% |
| scala  | $0.0329 (close) | $0.0540 (LOSS 1.76×) | +64% |
| php    | $0.0424 (TIE)   | $0.0533 (LOSS 1.52×) | +26% |

The cost shape is roughly proportional: a 12–15% size bump in every
read response, amplified by however many reads the model issues per
turn. Tasks that need many file reads (kotlin's rule-class enumeration,
ruby/scala's fixture audit) take the hit hardest.

## Pricing context

| Tier | Input | Cached input | Output |
|------|------:|------------:|-------:|
| Haiku 4.5 | $1.00/M | $0.10/M | $5.00/M |
| gpt-5.4-mini | $0.75/M | $0.075/M | $4.50/M |

Input pricing is similar; the asymmetry isn't in the price column but in
what the model *needs*. Haiku needs cat-n to land line numbers correctly,
so paying the bytes is rational. Mini doesn't need it (its arithmetic is
already reliable), so it's pure overhead.

## Options for the user

1. **Keep cat-n on for everyone** (status quo). Accepts the mini-side
   cost regression as the price of universal Haiku correctness.
2. **Gate cat-n on model tier.** Skip the prefix when the model is in a
   strong-arithmetic tier (mini, sonnet, opus, gpt-4*). Adds a model-tier
   lookup at the squeezy-tools layer.
3. **Gate cat-n on `start_line`.** The model already gets `start_line` as
   a separate field in the response; on prompts where line numbers are
   downstream-load-bearing the model could be coached (via spec
   description) to use `start_line + offset` arithmetic.
4. **Use a shorter prefix.** The cat-n format we copied from Claude Code
   uses 6 chars + tab = ~7 bytes per line. A 3-digit zero-padded prefix
   would cut that to 4 bytes per line — ~40% smaller overhead.

Option 2 is the targeted fix but adds a per-tier branch. Option 4 is
half-credit on the mini side without breaking Haiku. Option 3 is the
cheapest change but trusts the model to do the math the prefix exists
to bypass — fragile.

Recommendation: option 4 (shorter prefix) as a default, with option 2 as
a follow-up if mini-side cost is still too high. Both keep the universal
format and stay model-agnostic at the call site.

## Status

This finding is surfaced in the session-end commits but not acted on —
the per-tier or shorter-prefix decision is a product/architecture call,
not a mechanical fix. The Haiku-side wins are kept; the mini-side
regression is documented.
