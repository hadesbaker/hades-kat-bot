# hades-kat-bot

A Rust-based Solana trading bot for PumpFun tokens. Copy trades from target wallets in real-time via WebSocket, or discover tokens through Birdeye/GeckoTerminal feeds. All execution goes through PumpPortal's `/api/trade-local` endpoint (unsigned transactions signed locally). Positions are monitored using **on-chain pricing only** (bonding curve for active PumpFun tokens, PumpSwap pool vaults for graduated tokens), with all prices denominated in **SOL**. Exit conditions include hard stop-loss, hard profit target, and a multi-tier dynamic trailing stop system. Every buy/sell is logged to a JSONL trade journal.

## Features

- **Copy Trading** — Monitor target wallets via RPC WebSocket (`logsSubscribe`), detect PumpFun buys, and mirror them automatically
- **Strategy-Based Discovery** — Poll Birdeye and GeckoTerminal for trending tokens, enrich via DexScreener, apply filters and strategy analysis
- **On-Chain Pricing** — All price monitoring uses direct RPC reads (bonding curve PDAs and PumpSwap pool vaults), no external API dependency in the pricing path
- **Dynamic Trailing Stops** — Multi-tier trailing stop system with configurable gain/trail percentage pairs
- **Trade Journaling** — Append-only JSONL logs with full trade metadata per session
- **Risk Management** — Configurable stop-loss, take-profit, position limits, cooldowns, and slippage
- **Dry Run Mode** — Test the full pipeline without executing real trades
- **5 Trading Strategies** — Momentum, Mean Reversion, Breakout, Volume Spike, and Holder Growth

## Architecture

```
main.rs (CLI entry point)
  └── TradingBot (bot.rs) — orchestrator, event loop
        ├── WalletManager (wallet.rs) — keypair, SOL/token balances
        ├── StrategyEngine (strategies.rs) — 5 analysis strategies
        ├── TradingEngine (trading.rs) — position mgmt, buy/sell execution, exit checks
        ├── MonitoringSystem (monitoring.rs) — metrics, alerts, webhook
        ├── TradeJournal (bot.rs) — JSONL logging
        └── pumpportal.rs — all external data:
              ├── On-chain price reads (bonding curve, PumpSwap)
              ├── PumpPortal trade-local API (tx construction)
              ├── DexScreener enrichment (metadata only)
              ├── Birdeye trending feed
              ├── GeckoTerminal trending feed
              └── WebSocket copy-trade listener
```

### Event Loop

The main loop (`bot.rs:run`) uses `tokio::select!` with 4 concurrent arms:

| Arm        | Interval                   | Purpose                                                |
| ---------- | -------------------------- | ------------------------------------------------------ |
| Discovery  | `birdeye.poll_interval_ms` | Poll trending feeds, enrich, filter, analyze, trade    |
| Exit Check | `exit_check_interval_ms`   | On-chain price check for open positions, trigger sells |
| Copy Trade | Channel receive            | Process detected target wallet buys                    |
| Status     | 60s                        | Log metrics and check alerts                           |

### Trade Execution Flow

```
Signal (copy trade or strategy)
  → PumpPortal /api/trade-local (returns unsigned VersionedTransaction)
    → Bot signs with local keypair
      → send_and_confirm_transaction via RPC
        → Post-buy: read on-chain price for entry, query token balance
        → Position tracked in TradingEngine
          → Exit check loop monitors PnL via on-chain reads
            → Sell triggered → PumpPortal sell → journal logged
```

### On-Chain Pricing

Two-tier system, no external APIs:

1. **Bonding Curve** (active PumpFun tokens) — Derive PDA from mint, read `virtual_sol_reserves` / `virtual_token_reserves`, calculate `(vsol/1e9) / (vtok/1e6)` = SOL per token
2. **PumpSwap Pools** (graduated tokens, `complete=1`) — Discover pool via `getTokenLargestAccounts` + owner inspection, cache vault addresses, read base/quote vault balances, calculate `(quote_lamports/1e9) / (base_raw/1e6)`

Pool discovery is expensive (~3-4 RPC calls) but cached per token after first lookup.

## File Summary

| File                | Lines | Purpose                                                           |
| ------------------- | ----- | ----------------------------------------------------------------- |
| `src/main.rs`       | 65    | CLI entry point (`--config`, `--debug`, `--dry-run`)              |
| `src/lib.rs`        | 11    | Module declarations                                               |
| `src/error.rs`      | 37    | `BotError` enum (10 variants) via `thiserror`                     |
| `src/config.rs`     | 241   | Config structs, TOML parsing, trailing stop parser                |
| `src/wallet.rs`     | 74    | Keypair wrapper, SOL/token balance queries                        |
| `src/strategies.rs` | 243   | 5 entry strategies with configurable parameters                   |
| `src/trading.rs`    | 773   | Position management, buy/sell execution, exit condition checks    |
| `src/pumpportal.rs` | 876   | On-chain pricing, API integrations, WebSocket copy-trade listener |
| `src/bot.rs`        | 808   | Orchestrator, event loop, trade journaling, filters               |
| `src/monitoring.rs` | 299   | Metrics tracking, alert system, webhook support                   |

## Trading Strategies

Six strategies are available (configured in `[strategies]` and `[params]`). Only **Momentum** is enabled by default.

| Strategy             | Trigger                                                                                  | Confidence Based On    |
| -------------------- | ---------------------------------------------------------------------------------------- | ---------------------- |
| **Momentum**         | `price_change_24h >= min_price_change` AND `volume/liquidity >= min_volume_ratio`        | Price change magnitude |
| **Mean Reversion**   | `price_change_24h <= max_price_change` (dip) AND `liquidity/mcap >= min_liquidity_ratio` | Dip magnitude          |
| **Breakout**         | `volume/liquidity >= min_volume_spike` AND `price_change >= min_price_momentum`          | Volume spike magnitude |
| **Volume Spike**     | `volume/liquidity >= min_volume_multiplier` AND `holders >= min_holder_count`            | Volume multiplier      |
| **Holder Growth**    | `holders >= min_growth_holders` AND `mcap >= min_growth_market_cap`                      | Holder-to-mcap ratio   |
| **PumpSwap Sniper**  | Token has just graduated from PumpFun to PumpSwap, within `ps_sniper_freshness_secs`     | Always 1.0             |

**PumpSwap Sniper** is fed by two sources running in parallel: PumpPortal's `subscribeMigration` WebSocket channel (push-based, lowest latency), and a transition detector inside the new-token watchlist (any monitored token whose bonding curve disappears is treated as graduated). Events from both sources are deduplicated by mint. No filters from `[newtokenfilters]` apply — graduation alone is the entire signal.

## Exit Conditions

Checked every `exit_check_interval_ms` (default 250ms) via on-chain price reads:

1. **Hard Stop-Loss** (always active) — Exit if loss exceeds `stop_loss_percent`
2. **Dynamic Trailing Stop** (if thresholds configured) — When peak PnL crosses a gain tier, exit if current PnL drops `trail%` below peak. Highest crossed tier is active.
3. **Hard Profit Target** (fallback only) — Exit if PnL exceeds `profit_target_percent`. Only fires when no trailing thresholds are configured.

Example trailing stop config `"5:3,12:4,20:5,35:6,50:7,80:8,120:9"`:

- Peak hits +5% → exit if drops to +2% (5-3)
- Peak hits +35% → exit if drops to +29% (35-6)
- Peak hits +120% → exit if drops to +111% (120-9)

## Quick Start

### Prerequisites

- Rust 1.70+
- A Solana RPC endpoint (QuickNode, Helius, etc.)
- SOL in a wallet for trading

### Installation

```bash
git clone <repository-url>
cd hades-kat-bot
cargo build --release
```

### Configuration

Two files. Both are git-ignored — copy from the templates and tune locally.

```bash
cp config.toml.example config.toml   # operator settings (slippage, strategies, filters, ...)
cp .env.example .env                  # secrets (RPC URL, API keys, webhooks, target wallets)
```

1. **`.env`** — fill in real values:

```
# Solana RPC URL — used for all on-chain reads, transaction submission, and
# WebSocket subscriptions (the bot derives wss:// from this URL).
SOLANA_RPC_URL=https://api.mainnet-beta.solana.com

# Birdeye API key (required only when birdeye_token_feed = true)
BIRDEYE_API_KEY=

# Discord webhook URL for monitoring alerts (optional)
DISCORD_WEBHOOK_URL=

# Comma-separated target wallets for copy trading (leave empty if not used)
TARGET_WALLETS=WalletPubkey1,WalletPubkey2

# PumpPortal API base URL — used for /api/trade-local (HTTP) and /api/data (WS, derived)
PUMPPORTAL_API_URL=https://pumpportal.fun

# PumpPortal API key — reserved for premium features (not currently sent on any request)
PUMPPORTAL_API_KEY=
```

2. **`config.toml`** — open it and tune trading parameters, strategy toggles, and filter thresholds. The example ships with everything off; flip the strategies you want and start with a small `max_buy_amount`.

`SOLANA_RPC_URL` is required for live trading. The free `api.mainnet-beta.solana.com` endpoint is heavily rate-limited; use a paid provider (QuikNode, Helius, etc.) in production. `PUMPPORTAL_API_URL` defaults to `https://pumpportal.fun` if unset; override only if PumpPortal moves the endpoint or you front it with a proxy.

The values in `BIRDEYE_API_KEY` and `DISCORD_WEBHOOK_URL` override the corresponding fields in `config.toml` (`[birdeye] api_key`, `[monitoring] webhook_url`) if set, so you can keep `config.toml` free of secrets.

### Running

```bash
# Dry run (recommended first)
cargo run --release -- --dry-run

# Debug logging
cargo run --release -- --debug

# Custom config path
cargo run --release -- --config my-config.toml

# Live trading
cargo run --release
```

To save logs to a file while still seeing them in the terminal, force `env_logger` to keep ANSI colors enabled (it disables them by default when stdout is piped):

```bash
RUST_LOG_STYLE=always cargo run --release 2>&1 | tee logs/run.log
```

The terminal renders the colors; the file stores the raw ANSI escape codes (view with `less -R` or `cat`).

To keep colors in the terminal but strip them from the saved file:

```bash
RUST_LOG_STYLE=always cargo run --release 2>&1 | tee >(sed 's/\x1b\[[0-9;]*m//g' > logs/run.log)
```

### Wallet Setup

The bot auto-generates `wallet.json` on first run if it doesn't exist. To import into Phantom:

1. Open `wallet.json` — it contains a JSON array of numbers (the keypair bytes)
2. In Phantom: Add Account -> Import Private Key
3. Paste the array and name the wallet
4. Send SOL to the wallet address (logged on bot startup)

## Configuration Reference

### Root Level

| Variable                   | Default                               | Purpose                            |
| -------------------------- | ------------------------------------- | ---------------------------------- |
| `wallet_path`              | `"wallet.json"`                       | Path to keypair file               |
| `birdeye_token_feed`       | `true`                                | Enable Birdeye trending feed       |
| `coingecko_token_feed`     | `true`                                | Enable GeckoTerminal trending feed |
| `target_wallet_token_feed` | `false`                               | Enable copy trading via WebSocket  |

The Solana RPC URL is read from the `SOLANA_RPC_URL` environment variable in `.env`, not from `config.toml`.

### `[trading]`

| Variable                           | Default                 | Purpose                                                        |
| ---------------------------------- | ----------------------- | -------------------------------------------------------------- |
| `max_slippage`                     | `5.0`                   | Slippage tolerance (%) sent to PumpPortal                      |
| `min_liquidity`                    | `100,000,000` (100 SOL) | Minimum liquidity filter for strategy discovery (lamports)     |
| `max_buy_amount`                   | `1,000,000,000` (1 SOL) | Max spend per strategy buy (lamports)                          |
| `profit_target_percent`            | `20.0`                  | Hard take-profit %, only fires if no trailing stops configured |
| `stop_loss_percent`                | `10.0`                  | Hard stop-loss %, always active                                |
| `cooldown_seconds`                 | `60`                    | Min seconds between trades on same token                       |
| `max_positions`                    | `5`                     | Max concurrent open positions                                  |
| `exit_check_interval_ms`           | `5,000`                 | How often to poll on-chain prices for exit checks              |
| `copy_trade_amount_sol`            | `0.1`                   | SOL amount per copy trade                                      |
| `dynamic_trailing_stop_thresholds` | `""`                    | Tiered trailing stops: `"gain:trail,gain:trail,..."`           |

### PumpPortal (loaded from `.env`, not `config.toml`)

| Env var              | Default                    | Purpose                                                                  |
| -------------------- | -------------------------- | ------------------------------------------------------------------------ |
| `PUMPPORTAL_API_URL` | `https://pumpportal.fun`   | Base URL — `/api/trade-local` for HTTP, `/api/data` (wss://) for WS feed |
| `PUMPPORTAL_API_KEY` | unset                      | Reserved for premium features (not currently sent on any request)        |

### `[birdeye]`

| Variable           | Default           | Purpose                                         |
| ------------------ | ----------------- | ----------------------------------------------- |
| `api_key`          | `""`              | Birdeye API key (get from birdeye.so)           |
| `trending_limit`   | `100`             | Max trending tokens to fetch per cycle          |
| `poll_interval_ms` | `300,000` (5 min) | Discovery polling interval (used for all feeds) |

### `[filters]`

| Variable         | Default      | Purpose                                 |
| ---------------- | ------------ | --------------------------------------- |
| `min_market_cap` | `2,800`      | Skip tokens below this market cap (USD) |
| `max_market_cap` | `10,000,000` | Skip tokens above this market cap (USD) |
| `min_holders`    | `10`         | Minimum holder count                    |
| `max_age_hours`  | `48`         | Maximum token age in hours              |

Note: A PumpFun-only filter and a `min_liquidity` filter (from `[trading]`) are also applied during token discovery.

### `[monitoring]`

| Variable      | Default | Purpose                              |
| ------------- | ------- | ------------------------------------ |
| `webhook_url` | `None`  | Discord/Slack webhook URL for alerts |

### `[monitoring.alert_thresholds]`

| Variable                   | Default | Purpose                            |
| -------------------------- | ------- | ---------------------------------- |
| `max_drawdown_percent`     | `20.0`  | Alert if max drawdown exceeds this |
| `min_daily_profit_percent` | `5.0`   | Daily profit alert threshold       |
| `max_daily_loss_percent`   | `15.0`  | Alert if daily loss exceeds this   |

### `[params]` (Strategy Parameters)

| Variable                | Default   | Strategy                                               |
| ----------------------- | --------- | ------------------------------------------------------ |
| `min_price_change`      | `5.0`     | Momentum — min 24h price change %                      |
| `min_volume_ratio`      | `2.0`     | Momentum — min volume/liquidity ratio                  |
| `max_price_change`      | `-5.0`    | Mean Reversion — max 24h price change (negative = dip) |
| `min_liquidity_ratio`   | `0.05`    | Mean Reversion — min liquidity/mcap ratio              |
| `min_volume_spike`      | `1.5`     | Breakout — min volume/liquidity ratio                  |
| `min_price_momentum`    | `1.0`     | Breakout — min price change %                          |
| `min_volume_multiplier` | `2.0`     | Volume Spike — min volume/liquidity multiplier         |
| `min_holder_count`      | `5.0`     | Volume Spike — min holders                             |
| `min_growth_holders`    | `10.0`    | Holder Growth — min holders                            |
| `min_growth_market_cap` | `3,000.0` | Holder Growth — min market cap (USD)                   |

### Environment Variables

Loaded from `.env` at startup (via `dotenvy`); shell-exported values also work.

| Variable              | Required     | Default                                | Purpose                                                                  |
| --------------------- | ------------ | -------------------------------------- | ------------------------------------------------------------------------ |
| `SOLANA_RPC_URL`      | yes          | `https://api.mainnet-beta.solana.com`  | RPC endpoint for all on-chain reads, tx submission, and WS subscriptions |
| `BIRDEYE_API_KEY`     | for Birdeye  | unset                                  | Birdeye API key — overrides `[birdeye] api_key` in `config.toml`         |
| `DISCORD_WEBHOOK_URL` | no           | unset                                  | Webhook URL for alerts — overrides `[monitoring] webhook_url`            |
| `TARGET_WALLETS`      | for copy     | —                                      | Comma-separated Solana pubkeys to copy trade from                        |
| `PUMPPORTAL_API_URL`  | no           | `https://pumpportal.fun`               | Base URL for `/api/trade-local` (HTTP) and `/api/data` (WS, derived)     |
| `PUMPPORTAL_API_KEY`  | no           | unset                                  | Reserved for premium features; not currently sent on any request         |

## Trade Journal

Each bot session creates a JSONL file in `journals/` (e.g., `journals/2026-03-04_14-30-00.jsonl`).

**BUY entry fields**: timestamp, token address/symbol/name, price, market cap, liquidity, holders, volume, sol_spent, tokens_received, tx_signature, source_wallet

**SELL entry fields**: entry_price_sol, exit_price_sol, pnl_percent, peak_pnl_percent, profit_loss_sol, sol_spent, sol_received, exit_reason, tx_signature, source_wallet

## Monitoring

### Metrics Tracked

- Total trades, win/loss count, win rate
- Total P&L (SOL), average profit/loss per trade
- Max drawdown
- Current position count
- Uptime

### Alert Levels

| Level    | Triggers                                                 |
| -------- | -------------------------------------------------------- |
| Warning  | Max drawdown exceeded, win rate < 30% (after 10+ trades) |
| Error    | Daily loss threshold exceeded                            |
| Critical | System failures                                          |

## Key Constants

| Constant         | Value                                         |
| ---------------- | --------------------------------------------- |
| PumpFun Program  | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` |
| PumpSwap Program | `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` |
| SOL Mint         | `So11111111111111111111111111111111111111112` |
| Priority Fee     | 0.001 SOL                                     |
| Token Graduation | ~$69K market cap (bonding curve `complete=1`) |

## Security

- **Wallet**: Keypair stored locally in `wallet.json` — never share this file
- **API Keys**: PumpPortal keys live in `.env` (gitignored); the Birdeye key is in `config.toml`
- **Local Signing**: PumpPortal returns unsigned transactions; the bot signs locally — your private key never leaves your machine
- **Dry Run**: Always test with `--dry-run` before live trading

## Author

Taki Hades Baker Alyasri

## License

MIT
