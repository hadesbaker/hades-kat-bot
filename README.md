# hades-kat-bot

A Rust trading bot for Solana pump.fun tokens. It runs up to four independent discovery feeds — **Birdeye** trending, **GeckoTerminal** trending, **copy-trading** of target wallets, and brand-new **pump.fun token creations** — enriches each candidate with market data, scores it through a pluggable engine of seven buy strategies, and trades the ones that signal. Buys and sells are built by PumpPortal's `/api/trade-local` endpoint, signed locally, and submitted through your own RPC. Every open position is monitored on a fast on-chain pricing loop and exited by a layered strategy: hard stop-loss, take-profit, a multi-tier trailing stop, and optional time / velocity / momentum-reversal exits. Copy-traded wallets are graded by realized P&L, so unprofitable ones drop themselves out.

> **Warning — this bot trades real funds on Solana mainnet.** It takes full custody of the wallet at `wallet_path` and buys and sells tokens on its own. Always test with `--dry-run` first, and go live with a small `max_buy_amount` and `max_positions = 1`.
>
> **⚠️ Educational use only — see the [Disclaimer](#disclaimer) before running this software.**

## Features

- **Four discovery feeds**, each independently toggleable:
  - **Birdeye** trending tokens
  - **GeckoTerminal** trending pools
  - **Copy trading** — mirrors buys from target wallets via RPC `logsSubscribe`
  - **New-token feed** — brand-new pump.fun launches via PumpPortal's WebSocket, plus a graduation/migration sniper
- **7 pluggable buy strategies** — Momentum, Mean Reversion, Breakout, Volume Spike, Holder Growth, Liquidity Depth, and PumpSwap Sniper — each toggleable with its own tunable parameters
- **On-chain, SOL-denominated pricing** — bonding-curve reserves for pre-graduation tokens, PumpSwap pool vaults for graduated ones; exits are priced on the *realized* sell by simulating the swap
- **Layered exit engine** — hard stop-loss, hard take-profit, a multi-tier dynamic trailing stop, and optional time, velocity, and momentum-reversal exits
- **Wallet quality scoring** — copy-traded wallets are scored by realized SOL P&L; a wallet that stays unprofitable is automatically dropped from the target list
- **Local signing** — PumpPortal returns unsigned transactions; the bot signs with your local keypair and submits through your own RPC, so the private key never leaves the machine
- **JSONL trade journal** — every buy and sell appended to a timestamped per-session journal file
- **In-memory monitoring** — win rate, P&L, drawdown, and threshold-based alerts
- **Dry-run mode** — exercises the entire pipeline without sending a single real transaction
- **Single-instance lock** — a PID lockfile prevents two copies of the bot running against the same wallet

## Prerequisites

| Tool | Version / type | Why |
| ---- | -------------- | --- |
| Rust | edition 2021 — a recent stable toolchain | Solana SDK 1.18 typically needs Rust 1.75+ |
| A Solana mainnet RPC | HTTP endpoint (the WebSocket URL is derived from it) | All on-chain reads, transaction submission, and the copy-trade `logsSubscribe` stream. A paid provider (Helius, QuickNode, Triton, …) is strongly recommended — the public RPC will not reliably serve `logsSubscribe` or fast reads. |
| A funded Solana wallet | keypair JSON at `wallet_path` | Acts as trader and signer — needs SOL for buys, fees, and the 0.001 SOL priority fee per trade |
| A Birdeye API key | optional | Required only if the Birdeye feed is enabled |
| PumpPortal | reachable endpoint (default `https://pumpportal.fun`) | Builds every buy/sell transaction and serves the new-token / migration WebSocket; no key required |

## Setup

```bash
# 1. Clone
git clone https://github.com/hadesbaker/hades-kat-bot.git
cd hades-kat-bot

# 2. Create the two config files (both are git-ignored — copy the templates)
cp config.toml.example config.toml   # operator settings: strategies, filters, exits
cp .env.example .env                  # secrets: RPC URL, API keys, target wallets

# 3. Edit .env and config.toml — see "Configuration" below

# 4. Build
cargo build --release
```

### Wallet

The bot loads a Solana keypair from `wallet_path` (default `wallet.json`). If that file does not exist it **auto-generates a fresh keypair** on first run — so make sure you fund the address it prints at startup, and that you are pointing at the wallet you intend. To create one yourself instead:

```bash
solana-keygen new --outfile wallet.json
```

`wallet.json` is git-ignored — never commit it.

## Running

```bash
# Dry run — runs the full pipeline (feeds, discovery, strategy analysis, exit
# checks) but submits no real buys or sells. Recommended first.
cargo run --release -- --dry-run

# Live
cargo run --release
```

Flags:

- `--dry-run` — simulate only; logs "DRY RUN: Would…" instead of sending transactions
- `-d`, `--debug` — set the log level to `debug` instead of `info`
- `-c`, `--config <FILE>` — use an alternate config file (default `config.toml`)

### Logging

Logging goes through `env_logger`. Verbosity is `info` by default, `debug` with `--debug`, and the standard `RUST_LOG` environment variable overrides both. The bot writes no log file of its own — to keep one, redirect output:

```bash
cargo run --release 2>&1 | tee logs/run.log
```

The one structured file the bot does write is the per-session trade journal (see [How it works](#how-it-works)).

## Configuration

The bot reads two files. **Secrets** live in `.env`; **operator tuning** lives in `config.toml`. Both are git-ignored — copy them from `.env.example` / `config.toml.example`. Where the two overlap, `.env` wins.

### `.env`

| Variable | Purpose |
| -------- | ------- |
| `SOLANA_RPC_URL` | RPC endpoint for all on-chain reads and transaction submission; the WebSocket URL is derived from it. Overrides the config's RPC URL. |
| `BIRDEYE_API_KEY` | Birdeye API key — required only when the Birdeye feed is on. Overrides `[birdeye] api_key`. |
| `DISCORD_WEBHOOK_URL` | Webhook URL for monitoring alerts. Overrides `[monitoring] webhook_url`. |
| `TARGET_WALLETS` | Comma-separated base58 wallet addresses to copy-trade |
| `PUMPPORTAL_API_URL` | PumpPortal base URL — `/api/trade-local` (HTTP) and `/api/data` (WebSocket). Defaults to `https://pumpportal.fun`. |
| `PUMPPORTAL_API_KEY` | Reserved for PumpPortal premium features; currently unused |

> Keep API keys in `.env`, not `config.toml` — the `config.toml.example` header says as much. `.env` is the intended home for every secret.

### `config.toml`

`config.toml.example` is fully commented; the tables below summarize each section.

**Top level**

| Key | Example | Purpose |
| --- | ------- | ------- |
| `wallet_path` | `"wallet.json"` | Path to the keypair file |
| `birdeye_token_feed` | `false` | Enable the Birdeye trending feed |
| `coingecko_token_feed` | `false` | Enable the GeckoTerminal trending feed |
| `target_wallet_token_feed` | `false` | Enable copy trading from `TARGET_WALLETS` |
| `new_token_event_feed` | `false` | Enable the new-pump.fun-token feed |
| `pumpfun_enabled` / `pumpswap_enabled` | `true` | Allow trading bonding-curve / graduated tokens |

**`[trading]`**

| Key | Example | Purpose |
| --- | ------- | ------- |
| `max_slippage` | `5.0` | Base sell slippage %; escalates +10pp per retry, capped at 99% |
| `max_buy_amount` | `50_000_000` | Maximum spend per buy, in lamports (here 0.05 SOL) |
| `profit_target_percent` | `20.0` | Hard take-profit %; suppressed when trailing stops are configured |
| `stop_loss_percent` | `15.0` | Hard stop-loss %, always active |
| `cooldown_seconds` | `60` | Minimum seconds between trades on the same token |
| `max_positions` | `1` | Maximum concurrently open positions |
| `exit_check_interval_ms` | `1_000` | How often open positions are re-priced and exit-checked |
| `dynamic_trailing_stop_thresholds` | `""` | `gain%:trail%` pairs — see [Strategies](#strategies); empty disables trailing |

**`[birdeye]`**

| Key | Example | Purpose |
| --- | ------- | ------- |
| `api_key` | `""` | Leave blank — set `BIRDEYE_API_KEY` in `.env` instead |
| `trending_limit` | `20` | Trending tokens fetched per cycle |
| `poll_interval_ms` | `300_000` | Discovery-loop interval (drives all trending feeds) |

**`[trendingtokenfilters]`** — applied to Birdeye / GeckoTerminal candidates

| Key | Example | Purpose |
| --- | ------- | ------- |
| `min_market_cap` / `max_market_cap` | `50_000` / `100_000_000` | Accepted market-cap band (USD) |
| `min_holders` | `50` | Minimum holder count |
| `max_age_hours` | `168` | Maximum token age |
| `min_liquidity` | `50_000` | Minimum liquidity (USD) |
| `min_volume_24h` | `100_000` | Minimum 24h volume (USD) |
| `min_holder_density` | `25` | Minimum holders per $10K market cap |

**`[newtokenfilters]`** — applied to the new-token watchlist

| Key | Example | Purpose |
| --- | ------- | ------- |
| `min_initial_market_cap_sol` | `20.0` | Pre-filter: minimum market cap (SOL) on arrival |
| `min_market_cap` / `max_market_cap` | `30_000` / `50_000` | Market-cap band once DexScreener data arrives (USD; `0` disables) |
| `min_holders` / `min_liquidity` / `min_volume_24h` | `100` / `0` / `0` | Additional thresholds (`0` disables each) |
| `monitor_duration_minutes` | `60` | How long each new token stays on the watchlist |
| `watchlist_cap` | `200` | Maximum watchlist size (oldest dropped when full) |
| `poll_interval_secs` / `reduced_poll_interval_secs` / `reduced_poll_after_minutes` | `15` / `60` / `15` | Price-poll cadence, and when it slows down |
| `rug_drop_percent` | `80.0` | Drop from peak price that marks a token dead |

**`[copybotfilters]`** — applied to copy-trade candidates

| Key | Example | Purpose |
| --- | ------- | ------- |
| `enabled` | `false` | Master switch for copy-trade filtering |
| `min_market_cap` / `min_holders` / `min_liquidity` / `min_volume_24h` | `1_000` / `1` / `1` / `1` | Minimum thresholds (`0` disables each) |
| `loss_cooldown_minutes` | `60` | After a losing sell, block re-buying that token for this long |
| `min_holder_density` | `0` | Minimum holders per $10K market cap |
| `analysis_step` | `true` | `true`: run strategy analysis before copying · `false`: copy immediately |

**`[exitstrategies]`** — optional exits layered on top of stop-loss / trailing

| Key | Example | Purpose |
| --- | ------- | ------- |
| `enrichment_interval_ms` | `30_000` | How often DexScreener metadata is refreshed for open positions |
| `time_exit_enabled` / `max_hold_minutes` | `false` / `60` | Sell after holding a position this long |
| `liquidity_exit_enabled` / `min_liquidity_sol` | `false` / `1.0` | Sell if on-chain liquidity falls below the threshold |
| `velocity_exit_enabled` / `max_decline_rate_per_min` | `false` / `20.0` | Sell if price falls faster than this %/minute |
| `momentum_reversal_enabled` / `momentum_reversal_window_secs` / `momentum_reversal_min_loss_pct` | `false` / `30` / `5.0` | Sell on a momentum reversal while in loss territory |

**`[walletscoring]`** — copy-trading only

| Key | Example | Purpose |
| --- | ------- | ------- |
| `enabled` | `true` | Enable wallet scoring |
| `initial_score` | `1.0` | Score assigned to a newly seen target wallet |
| `sensitivity` | `5.0` | Score adjustment factor: `score += pnl_sol × sensitivity` |
| `min_score` / `max_score` | `0.1` / `2.0` | Score clamp range |
| `min_trades_for_scoring` | `3` | Minimum trades before a wallet's score is acted on |
| `wallet_removal_score` | `0.0` | When a wallet's score falls to this, it is removed from `TARGET_WALLETS` (`0` disables removal) |

**`[monitoring]`** and **`[monitoring.alert_thresholds]`**

| Key | Example | Purpose |
| --- | ------- | ------- |
| `webhook_url` | `""` | Alert webhook; overridden by `DISCORD_WEBHOOK_URL` |
| `max_drawdown_percent` | `20.0` | Raise an alert above this drawdown |
| `min_daily_profit_percent` | `5.0` | Daily-profit alert threshold |
| `max_daily_loss_percent` | `15.0` | Raise an alert above this daily loss |

**`[strategies]`** — seven booleans, one per strategy: `momentum`, `mean_reversion`, `breakout`, `volume_spike`, `holder_growth`, `liquidity_depth`, `ps_sniper`. Start with everything `false` and enable one at a time.

**`[params]`** — numeric thresholds for the strategies. Each key is named in the [Strategies](#strategies) table below.

## How it works

```
main()
  acquire bot.lock (single-instance guard) -> load .env + config.toml
  -> build wallet, strategy engine, trading engine, monitoring, trade journal

  four feeds run concurrently (each enabled in config.toml):
    - Birdeye trending        poll every birdeye.poll_interval_ms
    - GeckoTerminal trending       "          "
    - copy trading            RPC logsSubscribe on each TARGET_WALLETS address
    - new-token feed          PumpPortal subscribeNewToken (+ migration sniper)

  a candidate token appears
    -> enrich it with DexScreener market data
    -> apply the matching filter set (trending / new-token / copy-bot)
    -> StrategyEngine scores it with every enabled strategy
    -> any buy signal with confidence >= 0.5?
         no  -> skip
         yes -> TradingEngine.execute_buy
                  guards: cooldown, max_positions, duplicate
                  -> PumpPortal /api/trade-local builds the buy transaction
                  -> sign locally, submit + confirm via your RPC
                  -> record the Position, save positions.json, journal the buy

  exit loop  (every exit_check_interval_ms, for each open position)
    -> price the realized sell on-chain (bonding curve or PumpSwap pool)
    -> compute PnL %, update peak PnL
    -> sell the FULL position as soon as any exit fires:
         - hard stop-loss            PnL <= -stop_loss_percent
         - dynamic trailing stop     PnL fell trail% below peak
         - hard take-profit          PnL >= profit_target_percent
         - time / velocity / momentum-reversal exit   (if enabled)
    -> PumpPortal builds the sell, sign + submit, journal the sell
    -> copy trades: update the source wallet's quality score
```

Notes:

- **Confidence gate.** Every strategy emits only *buy* signals; a signal below 0.5 confidence is dropped. Exits are never strategy-driven — they come entirely from the exit loop.
- **On-chain, realized pricing.** Entry price is what the buy actually paid (SOL spent ÷ tokens received). Exit PnL is measured against a *simulated realized sell* through the curve or pool, not a spot quote — so logged PnL reflects what you would actually receive and will differ from a naive price chart.
- **Two venues.** Pre-graduation tokens price off the pump.fun bonding curve; once a token graduates (`complete = 1`) it prices off its PumpSwap pool vaults. Pool discovery is cached per token.
- **Positions persist.** Open positions are saved to `positions.json` and reloaded on restart, so the bot resumes managing them.
- **Trade journal.** Each run opens a fresh `journals/<timestamp>.jsonl`; every buy and sell is appended as one JSON line (token metadata, SOL in/out, PnL, peak PnL, exit reason, signature).
- **Single instance.** `bot.lock` holds the running bot's PID; a second instance refuses to start while the first is alive. After a hard crash you may need to delete a stale `bot.lock`.
- **Wallet self-pruning.** With scoring enabled, a copy-traded wallet whose score falls to `wallet_removal_score` is removed from `TARGET_WALLETS` in `.env` automatically — the bot edits `.env` at runtime.
- **Sell slippage escalates.** A failing sell is retried with slippage rising +10pp each attempt, up to 99%, to force a stuck position out.

## Strategies

Each strategy is toggled in `[strategies]` and tuned in `[params]`. Every strategy emits only buy signals, and a signal must reach 0.5 confidence to trade. Start with a single strategy enabled.

| Strategy | `[strategies]` key | Buys when | Confidence from |
| -------- | ------------------ | --------- | --------------- |
| Momentum | `momentum` | 24h price change ≥ `min_price_change` **and** volume/liquidity ≥ `min_volume_ratio` | price-change magnitude |
| Mean Reversion | `mean_reversion` | 24h price change ≤ `max_price_change` (a dip) **and** liquidity/market-cap ≥ `min_liquidity_ratio` | dip magnitude |
| Breakout | `breakout` | volume/liquidity ≥ `min_volume_spike` **and** price change ≥ `min_price_momentum` | volume-spike magnitude |
| Volume Spike | `volume_spike` | volume/liquidity ≥ `min_volume_multiplier` **and** holder count is sufficient | volume multiplier |
| Holder Growth | `holder_growth` | holders ≥ `min_growth_holders` **and** market cap ≥ `min_growth_market_cap` | holders-to-market-cap ratio |
| Liquidity Depth | `liquidity_depth` | liquidity ≥ `min_liquidity_usd`, market cap ≤ `liq_max_market_cap`, **and** liquidity/market-cap ≥ `min_liq_mcap_ratio` | liquidity/market-cap ratio |
| PumpSwap Sniper | `ps_sniper` | a token has just graduated to PumpSwap, within `ps_sniper_freshness_secs` | always 1.0 |

The PumpSwap Sniper is fed by PumpPortal's `subscribeMigration` channel and by a transition detector in the new-token watchlist (a monitored token whose bonding curve disappears is treated as graduated); the two sources are deduplicated by mint.

### Dynamic trailing stop

`dynamic_trailing_stop_thresholds` is a comma-separated list of `gain%:trail%` pairs. When a position's peak PnL crosses a gain tier, the bot exits if PnL then falls `trail%` below the peak; the highest crossed tier wins. When trailing is configured, the hard `profit_target_percent` is suppressed.

Example — `"5:3,12:4,20:5,35:6,50:7,80:8,120:9"`:

- peak hits +5% → exit if it drops to +2%
- peak hits +35% → exit if it drops to +29%
- peak hits +120% → exit if it drops to +111%

## Project structure

```
src/
  main.rs          entry point: CLI parsing, logging, the bot.lock single-instance
                   guard, config load, builds and runs TradingBot
  lib.rs           crate root; module declarations
  error.rs         BotError type and the crate Result alias
  config.rs        all config structs, .env + config.toml loading, defaults
  bot.rs           TradingBot orchestrator: feed wiring, the event loop, copy-trade
                   and graduation handling, candidate filters, journal, wallet scorer
  wallet.rs        keypair wrapper; SOL and SPL token balance reads
  strategies.rs    StrategyEngine and the seven buy strategies
  trading.rs       TradingEngine: buy/sell execution, position state, exit checks,
                   positions.json persistence
  tokens.rs        new-token watchlist: bonding-curve polling, rug/graduation
                   detection, DexScreener enrichment, filter gate
  pumpportal.rs    all external I/O: on-chain price reads, PumpPortal trade API,
                   Birdeye / GeckoTerminal / DexScreener HTTP, the WebSocket feeds
  monitoring.rs    MonitoringSystem: in-memory metrics and threshold alerts
```

## Disclaimer

**This software is provided for educational and informational purposes only.**

- **Not financial advice.** Nothing in this repository — code, comments, documentation, or examples — constitutes financial, investment, trading, legal, or tax advice. It is a technical demonstration of automated trading concepts.
- **Use entirely at your own risk.** Cryptocurrency trading is extremely high risk. Automated trading of newly graduated pump.fun tokens is especially speculative and you should assume you can lose **100% of any funds the bot has access to**. Never run this bot with money you cannot afford to lose entirely.
- **No warranty.** This software is provided "AS IS", without warranty of any kind, express or implied. It may contain bugs, may execute trades incorrectly, may fail to sell a position, and may lose money — including through software defects, network or RPC failures, slippage, or adverse market conditions.
- **No liability.** To the maximum extent permitted by law, the author(s) and contributors shall not be liable for any claim, damages, or other liability — including but not limited to any financial losses, lost profits, or lost funds — arising from or in connection with the use of, or inability to use, this software.
- **You are solely responsible** for how you use this software, for securing your wallet and private keys, for any funds placed at risk, and for complying with all laws, regulations, and third-party terms of service (including those of pump.fun, PumpSwap, PumpPortal, and your RPC provider) applicable in your jurisdiction.

By using, running, modifying, or distributing this software, you acknowledge that you have read and understood this disclaimer and accept full responsibility for the outcomes.

## Author

**Taki Hades Baker Alyasri**

## License

MIT — see the [Disclaimer](#disclaimer) above. The MIT license's "AS IS", no-warranty, and no-liability terms apply to all use of this software.
