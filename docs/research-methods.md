# Research methods and data requirements

This project separates short-window crypto pricing from sports structural pricing.
The collector provides evidence for testing hypotheses; it does not establish that
an unpublished strategy has been reproduced or that a signal is profitable.

## Short-window crypto

Read each contract's typed asset, reference index, window start/end, price-to-beat,
rounding precision and full rules. Preserve request/arrival times alongside the
exchange timestamp and reference-history response. Never replace a named settlement
index with an unrelated exchange's spot price.

The implemented baselines study late direction and last-minute probability changes.
Where the captured rule specifies a 60-second pre-boundary statistic, the model uses
that exact window and its rounding event. First-arrival samples govern what was
known at a decision time; later history cannot be backdated. Gap-filled reference
candles are excluded from modeling.

The model is a driftless Brownian baseline, not a calibrated production estimator.
A potential trade must cover configured fees, depth and price reserves. A signal
is followed by a delayed, distinct snapshot before a simulated FOK-like entry or
exit. Missing later liquidity cannot be treated as a fill. Official final index
values and payout responses are archived separately; reconstruction disagreements
must remain unresolved rather than being explained away.

## Sports structural pricing

Under compatible normal-settlement rules, some outcomes imply others. For example,
if a baseball team trailing 0-3 ultimately wins, the final combined score is at
least seven. The logical implication alone does not establish a trade: combo and
hedge quotations must exist for the same payout quantity, costs must be included,
and both legs must actually execute.

Preserve full rules, line values, overtime/draw/cancellation provisions, fee evidence,
combo eligibility, scores/period/status and both sides of the order book. Cancellation
or last-fair-market-price settlement may invalidate a normal-settlement equivalence.
A FOK instruction for one order does not make two orders atomic.

The public collector has no authenticated RFQ quote/lifecycle feed. Last-trade stats
are not a complete trade tape, and public event status is not a licensed low-latency
scoreboard. Volume admission can also exclude a low-turnover relationship leg.
Consequently, the current scanner cannot establish executable combo arbitrage,
market-making profitability, real queue priority or reliable stop execution.

## Evaluation discipline

- Keep the two strategy families and their PnL/accounting separate.
- Preserve the historical watchlist; never select past observations using future volume.
- Separate local receive time, source update time and HTTP round-trip duration.
- Charge captured/configured fees conservatively; the market coefficient does not
  prove an account's fee tier. Check current fees before interpreting a result.
- Use delayed, advancing source snapshots and shared depth consumption. Duplicate
  snapshots do not replenish liquidity.
- Report rejected fills, unresolved outcomes and missing data, not only signals.
- Compare future time periods and sizes after freezing parameters. A brief run does
  not establish statistical significance or justify annualizing returns.

## Public API references

- [Polymarket US API documentation](https://docs.polymarket.us/api-reference/introduction)
- [Market books and stats](https://docs.polymarket.us/api-reference/markets/get-market-book)
- [Reference asset prices](https://docs.polymarket.us/api-reference/asset-prices/overview)
- [Current fees](https://docs.polymarket.us/fees)
- [Combo settlement](https://docs.polymarket.us/faqs/combos-faqs)

Verify current access and the terms of each actual market. API behavior, permissions
and product availability may change independently of a saved source revision.
