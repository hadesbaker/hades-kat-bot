use crate::config::Config;
use crate::error::{BotError, Result};
use crate::monitoring::MonitoringSystem;
use crate::pumpportal::{self, TokenInfo};
use crate::strategies::{Action, StrategyConfig, StrategyEngine, TradingStrategy};
use crate::tokens::NewTokenMonitor;
use crate::trading::{Trade, TradingEngine};
use crate::wallet::WalletManager;
use log::{error, info, warn};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::EncodableKey;
use std::collections::HashMap;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::Duration;

/// Per-wallet quality scoring: adjusts position sizes based on historical P&L.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WalletScoreEntry {
    score: f64,
    trades: u32,
    total_pnl_sol: f64,
}

struct WalletScorer {
    scores: HashMap<String, WalletScoreEntry>,
    path: String,
}

impl WalletScorer {
    fn load(initial_score: f64) -> Self {
        let path = "wallet_scores.json".to_string();
        let scores = if let Ok(data) = std::fs::read_to_string(&path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        };
        let mut scorer = Self { scores, path };
        // Initialize any target wallets not yet in the file
        let wallets: Vec<String> = std::env::var("TARGET_WALLETS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .collect();
        for w in wallets {
            scorer.scores.entry(w).or_insert(WalletScoreEntry {
                score: initial_score,
                trades: 0,
                total_pnl_sol: 0.0,
            });
        }
        scorer.save();
        scorer
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.scores) {
            let tmp = format!("{}.tmp", self.path);
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    fn get_score(&self, wallet: &str) -> Option<f64> {
        self.scores.get(wallet).map(|e| e.score)
    }

    fn update(&mut self, wallet: &str, pnl_sol: f64, config: &crate::config::WalletScoring) {
        let entry = self
            .scores
            .entry(wallet.to_string())
            .or_insert(WalletScoreEntry {
                score: config.initial_score,
                trades: 0,
                total_pnl_sol: 0.0,
            });
        entry.trades += 1;
        entry.total_pnl_sol += pnl_sol;
        entry.score =
            (entry.score + pnl_sol * config.sensitivity).clamp(config.min_score, config.max_score);
        info!(
            "Wallet score updated: {} → {:.3} (trades={}, total_pnl={:+.4} SOL, this_trade={:+.4} SOL)",
            wallet, entry.score, entry.trades, entry.total_pnl_sol, pnl_sol
        );
        self.save();
    }

    fn remove_wallet(&mut self, wallet: &str) {
        // Remove from .env file
        if let Ok(env_content) = std::fs::read_to_string(".env") {
            let new_content = env_content
                .lines()
                .map(|line| {
                    if line.starts_with("TARGET_WALLETS=") {
                        let prefix = "TARGET_WALLETS=";
                        let wallets: Vec<&str> = line[prefix.len()..]
                            .split(',')
                            .filter(|w| w.trim() != wallet && !w.trim().is_empty())
                            .collect();
                        format!("{}{}", prefix, wallets.join(","))
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let _ = std::fs::write(".env", new_content);
        }
    }
}

/// Append-only JSONL trade journal.
struct TradeJournal {
    path: String,
}

impl TradeJournal {
    fn new() -> Self {
        // Create journals/ directory if it doesn't exist
        let _ = std::fs::create_dir_all("journals");
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let path = format!("journals/{}.jsonl", timestamp);
        Self { path }
    }

    fn write_entry(&self, entry: &serde_json::Value) {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{}", entry) {
                    error!("Failed to write trade journal: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to open trade journal {}: {}", self.path, e);
            }
        }
    }

    fn log_buy(
        &self,
        token: &TokenInfo,
        trade: &Trade,
        sol_spent: f64,
        source_wallet: Option<&str>,
    ) {
        let entry = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "action": "BUY",
            "token_address": token.address,
            "symbol": token.symbol,
            "name": token.name,
            "price_usd": token.price_usd,
            "market_cap": token.market_cap,
            "liquidity": token.liquidity,
            "holders": token.holders,
            "volume_24h": token.volume_24h,
            "price_change_24h": token.price_change_24h,
            "age_hours": token.age_hours,
            "sol_spent": sol_spent,
            "tokens_received": trade.amount,
            "tx_signature": trade.signature,
            "source_wallet": source_wallet,
        });
        info!(
            "JOURNAL BUY: {} ({}) | price=${:.10} mcap={} liq={} holders={} vol={} chg={:.2}% source={}",
            token.symbol, token.address, token.price_usd, token.market_cap,
            token.liquidity, token.holders, token.volume_24h, token.price_change_24h,
            source_wallet.unwrap_or("strategy")
        );
        self.write_entry(&entry);
    }

    fn log_sell(
        &self,
        token_address: &str,
        entry_price_sol: f64,
        exit_token: &TokenInfo,
        trade: &Trade,
        sol_spent: f64,
        peak_pnl: f64,
        exit_reason: &str,
        source_wallet: Option<&str>,
    ) {
        let pnl_percent = trade
            .sol_received
            .map(|recv| ((recv - sol_spent) / sol_spent) * 100.0)
            .unwrap_or(0.0);
        let entry = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "action": "SELL",
            "token_address": token_address,
            "symbol": exit_token.symbol,
            "name": exit_token.name,
            "entry_price_sol": entry_price_sol,
            "exit_price_sol": exit_token.price_usd,
            "market_cap": exit_token.market_cap,
            "liquidity": exit_token.liquidity,
            "holders": exit_token.holders,
            "volume_24h": exit_token.volume_24h,
            "price_change_24h": exit_token.price_change_24h,
            "pnl_percent": pnl_percent,
            "peak_pnl_percent": peak_pnl,
            "exit_reason": exit_reason,
            "tokens_sold": trade.amount,
            "profit_loss_sol": trade.profit_loss,
            "sol_spent": sol_spent,
            "sol_received": trade.sol_received,
            "tx_signature": trade.signature,
            "source_wallet": source_wallet,
        });
        info!(
            "JOURNAL SELL: {} ({}) | exit={:.12} SOL mcap={} liq={} holders={} vol={} pnl={:+.2}% peak={:+.2}% sol_recv={} source={} reason={}",
            exit_token.symbol, token_address, exit_token.price_usd, exit_token.market_cap,
            exit_token.liquidity, exit_token.holders, exit_token.volume_24h, pnl_percent, peak_pnl,
            trade.sol_received.map_or("N/A".to_string(), |s| format!("{:.6}", s)),
            source_wallet.unwrap_or("strategy"),
            exit_reason
        );
        self.write_entry(&entry);
    }
}

pub struct TradingBot {
    config: Config,
    wallet: WalletManager,
    strategy_engine: StrategyEngine,
    trading_engine: TradingEngine,
    monitoring: MonitoringSystem,
    journal: TradeJournal,
    dry_run: bool,
    running: bool,
    // Copy trading fields
    target_wallets: Vec<Pubkey>,
    wss_url: String,
    // Loss cooldown: token_address → time of last loss sell
    loss_cooldown: HashMap<String, std::time::Instant>,
    // Wallet quality scoring
    wallet_scorer: WalletScorer,
    // Runtime blocklist for wallets that hit removal threshold (takes effect immediately)
    blocked_wallets: std::collections::HashSet<String>,
    // New token event receiver from PumpPortal WebSocket
    new_tokens_rx: tokio::sync::mpsc::UnboundedReceiver<pumpportal::NewTokenEvent>,
    // Hold sender so the channel doesn't close when we move it
    _new_tokens_tx: tokio::sync::mpsc::UnboundedSender<pumpportal::NewTokenEvent>,
    // Migration event receiver (PumpFun → PumpSwap graduations)
    migrations_rx: tokio::sync::mpsc::UnboundedReceiver<pumpportal::MigrationEvent>,
    // Hold sender for cloning into the WS listener and the watchlist detector
    migrations_tx: tokio::sync::mpsc::UnboundedSender<pumpportal::MigrationEvent>,
    // Mints we've already dispatched to the sniper, to dedup WS + watchlist events
    sniped_mints: std::collections::HashSet<String>,
    // New token watchlist monitor
    new_token_monitor: NewTokenMonitor,
}

impl TradingBot {
    pub async fn new(config: Config, dry_run: bool) -> Result<Self> {
        // Initialize wallet with the configured RPC URL
        let wallet = if std::path::Path::new(&config.wallet_path).exists() {
            let keypair = solana_sdk::signature::Keypair::read_from_file(&config.wallet_path)
                .map_err(|e| BotError::Wallet(format!("Failed to read wallet file: {}", e)))?;
            WalletManager::new(keypair, config.rpc_url.clone())
        } else {
            warn!("Wallet file not found, generating new wallet");
            let keypair = solana_sdk::signature::Keypair::new();
            keypair
                .write_to_file(&config.wallet_path)
                .map_err(|e| BotError::Wallet(format!("Failed to save wallet: {}", e)))?;
            WalletManager::new(keypair, config.rpc_url.clone())
        };

        info!("Wallet address: {}", wallet.get_address());

        // Initialize strategy engine — populate parameters from config.params
        let p = &config.params;

        let mut momentum_params = HashMap::new();
        momentum_params.insert("min_price_change".to_string(), p.min_price_change);
        momentum_params.insert("min_volume_ratio".to_string(), p.min_volume_ratio);

        let mut mean_reversion_params = HashMap::new();
        mean_reversion_params.insert("max_price_change".to_string(), p.max_price_change);
        mean_reversion_params.insert("min_liquidity_ratio".to_string(), p.min_liquidity_ratio);

        let mut breakout_params = HashMap::new();
        breakout_params.insert("min_volume_spike".to_string(), p.min_volume_spike);
        breakout_params.insert("min_price_momentum".to_string(), p.min_price_momentum);

        let mut volume_spike_params = HashMap::new();
        volume_spike_params.insert("min_volume_multiplier".to_string(), p.min_volume_multiplier);
        volume_spike_params.insert("min_holder_count".to_string(), p.min_holder_count);

        let mut holder_growth_params = HashMap::new();
        holder_growth_params.insert("min_growth_holders".to_string(), p.min_growth_holders);
        holder_growth_params.insert("min_growth_market_cap".to_string(), p.min_growth_market_cap);

        let mut liquidity_depth_params = HashMap::new();
        liquidity_depth_params.insert("min_liq_mcap_ratio".to_string(), p.min_liq_mcap_ratio);
        liquidity_depth_params.insert("min_liquidity_usd".to_string(), p.min_liquidity_usd);
        liquidity_depth_params.insert("max_market_cap".to_string(), p.liq_max_market_cap);

        let mut ps_sniper_params = HashMap::new();
        ps_sniper_params.insert(
            "ps_sniper_freshness_secs".to_string(),
            p.ps_sniper_freshness_secs,
        );

        let st = &config.strategies;
        let strategies = vec![
            StrategyConfig {
                strategy: TradingStrategy::Momentum,
                parameters: momentum_params,
                enabled: st.momentum,
            },
            StrategyConfig {
                strategy: TradingStrategy::MeanReversion,
                parameters: mean_reversion_params,
                enabled: st.mean_reversion,
            },
            StrategyConfig {
                strategy: TradingStrategy::Breakout,
                parameters: breakout_params,
                enabled: st.breakout,
            },
            StrategyConfig {
                strategy: TradingStrategy::VolumeSpike,
                parameters: volume_spike_params,
                enabled: st.volume_spike,
            },
            StrategyConfig {
                strategy: TradingStrategy::HolderGrowth,
                parameters: holder_growth_params,
                enabled: st.holder_growth,
            },
            StrategyConfig {
                strategy: TradingStrategy::LiquidityDepth,
                parameters: liquidity_depth_params,
                enabled: st.liquidity_depth,
            },
            StrategyConfig {
                strategy: TradingStrategy::PumpSwapSniper,
                parameters: ps_sniper_params,
                enabled: st.ps_sniper,
            },
        ];
        for s in &strategies {
            info!(
                "{:?} Strategy: {}",
                s.strategy,
                if s.enabled { "enabled" } else { "disabled" }
            );
        }
        let strategy_engine = StrategyEngine::new(strategies);

        // Initialize trading engine
        let trailing_thresholds = config.trading.parsed_trailing_thresholds();
        let trading_engine = TradingEngine::new(
            &wallet,
            &config.rpc_url,
            config.trading.max_positions,
            config.trading.max_buy_amount,
            config.trading.max_slippage,
            config.trading.profit_target_percent,
            config.trading.stop_loss_percent,
            config.trading.cooldown_seconds,
            trailing_thresholds,
            config.exitstrategies.clone(),
        );

        // Initialize monitoring system
        let monitoring = MonitoringSystem::new(
            config.monitoring.webhook_url.clone(),
            config.monitoring.alert_thresholds.clone(),
        );

        // Parse target wallets from env
        let raw_wallets = std::env::var("TARGET_WALLETS").unwrap_or_default();
        let wallet_parts: Vec<&str> = raw_wallets
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .collect();
        let target_wallets: Vec<Pubkey> = wallet_parts
            .iter()
            .filter_map(|s| {
                let trimmed = s.trim();
                match Pubkey::from_str(trimmed) {
                    Ok(pk) => Some(pk),
                    Err(e) => {
                        warn!("Invalid wallet address '{}': {}", trimmed, e);
                        None
                    }
                }
            })
            .collect();
        info!(
            "Parsed {}/{} target wallets from .env",
            target_wallets.len(),
            wallet_parts.len()
        );

        // Derive WSS URL
        let wss_url = config
            .rpc_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");

        let journal = TradeJournal::new();
        info!("Trade journal: {}", journal.path);

        // Create channel for new token events
        let (new_tokens_tx, new_tokens_rx) =
            tokio::sync::mpsc::unbounded_channel::<pumpportal::NewTokenEvent>();

        // Create channel for migration events (PumpFun → PumpSwap graduations).
        // Both the WS subscribeMigration listener and the watchlist transition detector
        // push into this channel; the receiver dedups by mint.
        let (migrations_tx, migrations_rx) =
            tokio::sync::mpsc::unbounded_channel::<pumpportal::MigrationEvent>();

        let wallet_scorer = WalletScorer::load(config.walletscoring.initial_score);
        if config.walletscoring.enabled {
            info!(
                "Wallet scoring: enabled (sensitivity={}, removal_at={})",
                config.walletscoring.sensitivity, config.walletscoring.wallet_removal_score
            );
        }

        // Initialize new token monitor with async RPC client
        let async_rpc_for_monitor = Arc::new(
            solana_client::nonblocking::rpc_client::RpcClient::new(config.rpc_url.clone()),
        );
        let mut new_token_monitor = NewTokenMonitor::new(
            config.newtokenfilters.clone(),
            async_rpc_for_monitor,
        );
        // Hook the watchlist's transition-detection events into the migration channel
        // when the sniper strategy is enabled.
        if config.strategies.ps_sniper {
            new_token_monitor.set_migration_tx(migrations_tx.clone());
        }

        Ok(Self {
            config,
            wallet: wallet,
            strategy_engine,
            trading_engine,
            monitoring,
            journal,
            dry_run,
            running: false,
            target_wallets,
            wss_url,
            loss_cooldown: HashMap::new(),
            wallet_scorer,
            blocked_wallets: std::collections::HashSet::new(),
            new_tokens_rx,
            _new_tokens_tx: new_tokens_tx,
            migrations_rx,
            migrations_tx,
            sniped_mints: std::collections::HashSet::new(),
            new_token_monitor,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        self.running = true;
        info!("Starting trading bot (dry_run: {})", self.dry_run);

        let mut discovery_interval =
            tokio::time::interval(Duration::from_millis(self.config.birdeye.poll_interval_ms));
        let mut exit_interval = tokio::time::interval(Duration::from_millis(
            self.config.trading.exit_check_interval_ms,
        ));
        let mut status_interval = tokio::time::interval(Duration::from_secs(60));
        let mut enrichment_interval = tokio::time::interval(Duration::from_millis(
            self.config.exitstrategies.enrichment_interval_ms,
        ));

        // Create channel for copy trade mints
        let (mint_tx, mut mint_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, Option<String>)>();

        // Spawn WS listener if target wallet feed enabled
        if self.config.target_wallet_token_feed && !self.target_wallets.is_empty() {
            let wss = self.wss_url.clone();
            let wallets = self.target_wallets.clone();
            let rpc = Arc::new(RpcClient::new(self.config.rpc_url.clone()));
            let tx = mint_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = pumpportal::listen_target_wallets(wss, wallets, rpc, tx).await {
                    error!("Target wallet listener error: {}", e);
                }
            });
            info!(
                "Target wallet feed: monitoring {} wallets via WSS",
                self.target_wallets.len()
            );
            for (i, w) in self.target_wallets.iter().enumerate() {
                info!("  Target #{}: {}", i + 1, w);
            }
        }

        // Spawn PumpPortal new-token WS listener if enabled
        if self.config.new_token_event_feed {
            let tx = self._new_tokens_tx.clone();
            tokio::spawn(async move {
                pumpportal::listen_new_tokens(tx).await;
            });
            info!(
                "New token event feed: monitoring PumpPortal WS (watchlist cap={}, monitor={}m, poll={}s/{}s)",
                self.config.newtokenfilters.watchlist_cap,
                self.config.newtokenfilters.monitor_duration_minutes,
                self.config.newtokenfilters.poll_interval_secs,
                self.config.newtokenfilters.reduced_poll_interval_secs,
            );
        }

        // Spawn PumpPortal migration WS listener if PumpSwapSniper is enabled.
        // We also feed the watchlist's transition-detection events into the same channel
        // so a missed WS event doesn't lose us the snipe.
        if self.config.strategies.ps_sniper {
            let tx = self.migrations_tx.clone();
            tokio::spawn(async move {
                pumpportal::listen_migrations(tx).await;
            });
            info!(
                "PumpSwapSniper: enabled (freshness window {}s)",
                self.config.params.ps_sniper_freshness_secs
            );
        }

        // Monitor tick interval — uses the faster poll interval since tiered
        // logic inside NewTokenMonitor handles per-token scheduling
        let mut monitor_interval = tokio::time::interval(Duration::from_secs(
            self.config.newtokenfilters.poll_interval_secs,
        ));

        // Run discovery once immediately on startup
        discovery_interval.tick().await;
        if let Err(e) = self.get_trending_tokens().await {
            error!("Error fetching trending tokens on startup: {}", e);
        }

        while self.running {
            tokio::select! {
                _ = discovery_interval.tick() => {
                    if let Err(e) = self.get_trending_tokens().await {
                        error!("Error fetching trending tokens: {}", e);
                    }
                }
                _ = exit_interval.tick() => {
                    if !self.trading_engine.get_positions().is_empty() {
                        if let Err(e) = self.check_exit_conditions().await {
                            error!("Error checking exit conditions: {}", e);
                        }
                    }
                }
                Some((mint_address, target_wallet)) = mint_rx.recv() => {
                    if let Err(e) = self.process_copy_trade(mint_address, target_wallet).await {
                        error!("Copy trade error: {}", e);
                    }
                }
                Some(ev) = self.new_tokens_rx.recv() => {
                    if self.config.new_token_event_feed {
                        if self.new_token_monitor.add_token(
                            ev.mint.clone(),
                            ev.bonding_curve_key.clone(),
                            ev.market_cap_sol,
                        ) {
                            info!(
                                "Watchlist +{} (mcap_sol={:.4}, watchlist={})",
                                &ev.mint[..8.min(ev.mint.len())],
                                ev.market_cap_sol,
                                self.new_token_monitor.watchlist_len()
                            );
                        }
                    }
                }
                Some(ev) = self.migrations_rx.recv() => {
                    if self.config.strategies.ps_sniper {
                        if let Err(e) = self.process_graduation(ev).await {
                            error!("Graduation processing error: {}", e);
                        }
                    }
                }
                _ = monitor_interval.tick() => {
                    if self.config.new_token_event_feed && self.new_token_monitor.watchlist_len() > 0 {
                        let ready_tokens = self.new_token_monitor.tick().await;
                        for token in ready_tokens {
                            if let Err(e) = self.analyze_and_trade_token(token).await {
                                error!("Error analyzing watchlist token: {}", e);
                            }
                        }
                    }
                }
                _ = enrichment_interval.tick() => {
                    if !self.trading_engine.get_positions().is_empty() {
                        self.trading_engine.enrich_positions().await;
                    }
                }
                _ = status_interval.tick() => {
                    self.update_monitoring().await;
                    self.log_status().await;
                }
            }
        }

        info!("Trading bot stopped");
        Ok(())
    }

    /// Process a copy trade signal from the target wallet WS listener.
    /// Uses PumpPortal trade-local API ("pool": "auto") which handles all routing
    /// (bonding curve, PumpSwap, Raydium) automatically — no manual pool discovery needed.
    /// Positions are monitored by the existing check_exit_conditions loop.
    async fn process_copy_trade(
        &mut self,
        mint_address: String,
        target_wallet: Option<String>,
    ) -> Result<()> {
        info!("Copy trade detected: {}", mint_address);

        // Skip if source wallet is blocked (score dropped to removal threshold)
        if let Some(ref tw) = target_wallet {
            if self.blocked_wallets.contains(tw) {
                info!(
                    "Copy trade {} skipped: source wallet {} is blocked (score below removal threshold)",
                    mint_address, tw
                );
                return Ok(());
            }
        }

        // Skip if already holding
        if self
            .trading_engine
            .get_positions()
            .contains_key(&mint_address)
        {
            info!("Already holding {}, skipping copy trade", mint_address);
            return Ok(());
        }

        // Skip if token was recently sold at a loss (loss cooldown)
        let loss_cooldown_mins = self.config.copybotfilters.loss_cooldown_minutes;
        if loss_cooldown_mins > 0 {
            if let Some(loss_time) = self.loss_cooldown.get(&mint_address) {
                let elapsed = loss_time.elapsed();
                let cooldown = Duration::from_secs(loss_cooldown_mins as u64 * 60);
                if elapsed < cooldown {
                    info!(
                        "Copy trade {} skipped: loss cooldown ({:.0}s remaining)",
                        mint_address,
                        (cooldown - elapsed).as_secs_f64()
                    );
                    return Ok(());
                }
            }
        }

        // Skip if at max positions
        if self.trading_engine.get_positions().len() >= self.config.trading.max_positions {
            warn!(
                "Max positions reached, skipping copy trade for {}",
                mint_address
            );
            return Ok(());
        }

        // Attempt DexScreener enrichment (optional — new tokens often missing)
        let tokens = pumpportal::enrich_tokens_dexscreener(&[mint_address.clone()]).await?;
        let enriched = tokens.into_iter().next();

        // Build TokenInfo — use DexScreener data if available, otherwise fetch price
        // from bonding curve / PumpFun API for accurate PnL tracking
        let token_info = if let Some(t) = enriched {
            info!(
                "Copy trade {} enriched: price=${:.8} mcap={} vol={}",
                t.symbol, t.price_usd, t.market_cap, t.volume_24h
            );
            t
        } else {
            info!(
                "No DexScreener data for {} — using on-chain price",
                mint_address
            );

            pumpportal::TokenInfo {
                address: mint_address.clone(),
                symbol: mint_address[..8.min(mint_address.len())].to_string(),
                name: "COPY_TRADE".to_string(),
                decimals: 6,
                market_cap: 0,
                holders: 0,
                age_hours: 0,
                liquidity: 0,
                price_usd: 0.0, // Entry price will be set post-buy from on-chain data
                price_change_24h: 0.0,
                volume_24h: 0,
                created_at: chrono::Utc::now().to_rfc3339(),
                just_graduated_at_secs: None,
            }
        };

        // Apply copy bot filters
        if !self.passes_copybot_filters(&token_info) {
            info!(
                "Copy trade {} skipped: failed copybot filters",
                mint_address
            );
            return Ok(());
        }

        // Check if token is on bonding curve (pumpfun) or graduated (pumpswap)
        // and gate based on config flags
        if !self.config.pumpfun_enabled || !self.config.pumpswap_enabled {
            let is_bonding_curve = pumpportal::fetch_price_bonding_curve_rpc(
                self.trading_engine.rpc_client(),
                &mint_address,
            )
            .await
            .is_ok();

            if is_bonding_curve && !self.config.pumpfun_enabled {
                info!(
                    "Copy trade {} skipped: token is on bonding curve (pumpfun_enabled=false)",
                    mint_address
                );
                return Ok(());
            }
            if !is_bonding_curve && !self.config.pumpswap_enabled {
                info!(
                    "Copy trade {} skipped: token has graduated to PumpSwap (pumpswap_enabled=false)",
                    mint_address
                );
                return Ok(());
            }
        }

        if self.dry_run {
            info!(
                "DRY RUN: Would copy trade {} ({}) via PumpPortal trade-local",
                token_info.symbol, mint_address
            );
            return Ok(());
        }

        // If analysis_step is enabled, run strategy analysis and only buy on signal.
        // If disabled, buy immediately at max_buy_amount without strategy gate.
        if self.config.copybotfilters.analysis_step {
            let signals = self.strategy_engine.analyze_token(&token_info)?;

            if signals.is_empty() {
                info!(
                    "Copy trade {} skipped: no strategy signals for {}",
                    mint_address, token_info.symbol
                );
                return Ok(());
            }

            for signal in signals {
                if signal.confidence < 0.5 {
                    log::debug!(
                        "Copy trade {}: low confidence signal {:.2}, skipping",
                        token_info.symbol, signal.confidence
                    );
                    continue;
                }

                let sol_amount = signal
                    .override_buy_amount
                    .unwrap_or(self.config.trading.max_buy_amount) as f64
                    / 1_000_000_000.0;

                info!(
                    "Copy trade strategy signal for {} ({}): {} (confidence: {:.2}, amount: {:.4} SOL, source: {})",
                    token_info.symbol, mint_address, signal.reason, signal.confidence, sol_amount,
                    target_wallet.as_deref().unwrap_or("unknown")
                );

                let token_info_for_journal = signal.token.clone();
                match self.trading_engine.execute_signal(signal).await {
                    Ok(Some(trade)) => {
                        info!(
                            "Copy trade executed via PumpPortal: {} | sig={} | source={}",
                            mint_address,
                            trade.signature.as_deref().unwrap_or("none"),
                            target_wallet.as_deref().unwrap_or("unknown")
                        );
                        if let Some(ref tw) = target_wallet {
                            self.trading_engine
                                .set_position_source_wallet(&mint_address, tw.clone());
                        }
                        self.journal.log_buy(
                            &token_info_for_journal,
                            &trade,
                            sol_amount,
                            target_wallet.as_deref(),
                        );
                        self.trading_engine.add_trade(trade);
                    }
                    Ok(None) => {
                        warn!(
                            "Copy trade signal for {} was not executed (cooldown/max positions/zero amount)",
                            mint_address
                        );
                    }
                    Err(e) => {
                        error!("Copy trade execution failed for {}: {}", mint_address, e);
                        return Err(e);
                    }
                }
            }
        } else {
            // No analysis — buy immediately at max_buy_amount
            let buy_amount = self.config.trading.max_buy_amount;
            let sol_amount = buy_amount as f64 / 1_000_000_000.0;

            info!(
                "Copy trade {} ({}): buying immediately (analysis_step=false), amount={:.4} SOL, source={}",
                token_info.symbol, mint_address, sol_amount,
                target_wallet.as_deref().unwrap_or("unknown")
            );

            let signal = crate::strategies::TradingSignal {
                token: token_info.clone(),
                action: crate::strategies::Action::Buy,
                confidence: 1.0,
                reason: "Copy trade (no analysis)".to_string(),
                expected_price: None,
                override_buy_amount: Some(buy_amount),
            };

            match self.trading_engine.execute_signal(signal).await {
                Ok(Some(trade)) => {
                    info!(
                        "Copy trade executed via PumpPortal: {} | sig={} | source={}",
                        mint_address,
                        trade.signature.as_deref().unwrap_or("none"),
                        target_wallet.as_deref().unwrap_or("unknown")
                    );
                    if let Some(ref tw) = target_wallet {
                        self.trading_engine
                            .set_position_source_wallet(&mint_address, tw.clone());
                    }
                    self.journal.log_buy(
                        &token_info,
                        &trade,
                        sol_amount,
                        target_wallet.as_deref(),
                    );
                    self.trading_engine.add_trade(trade);
                }
                Ok(None) => {
                    warn!(
                        "Copy trade for {} was not executed (cooldown/max positions/zero amount)",
                        mint_address
                    );
                }
                Err(e) => {
                    error!("Copy trade execution failed for {}: {}", mint_address, e);
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// Poll trending token feeds, enrich via DexScreener, filter, analyze, and trade.
    /// Feeds are conditionally enabled via config flags.
    async fn get_trending_tokens(&mut self) -> Result<()> {
        // Step 1: Fetch trending token addresses from Birdeye
        let birdeye_addresses = if self.config.birdeye_token_feed {
            let api_key = &self.config.birdeye.api_key;
            if api_key.is_empty() {
                log::warn!("Birdeye API key is empty, skipping trending token fetch");
                Vec::new()
            } else {
                let addrs =
                    pumpportal::fetch_birdeye_trending(api_key, self.config.birdeye.trending_limit)
                        .await?;
                info!("Birdeye trending: fetched {} token addresses", addrs.len());
                for (i, addr) in addrs.iter().enumerate() {
                    info!("  Birdeye #{}: {}", i + 1, addr);
                }
                addrs
            }
        } else {
            log::info!("Birdeye token feed disabled, skipping");
            Vec::new()
        };

        // Step 2: Fetch trending token addresses from GeckoTerminal (free, no API key)
        let gecko_addresses = if self.config.coingecko_token_feed {
            match pumpportal::fetch_geckoterminal_trending(5).await {
                Ok(addrs) => {
                    info!(
                        "GeckoTerminal trending: fetched {} token addresses",
                        addrs.len()
                    );
                    for (i, addr) in addrs.iter().enumerate() {
                        info!("  Gecko #{}: {}", i + 1, addr);
                    }
                    addrs
                }
                Err(e) => {
                    error!("GeckoTerminal fetch failed: {}", e);
                    Vec::new()
                }
            }
        } else {
            log::info!("GeckoTerminal token feed disabled, skipping");
            Vec::new()
        };

        // Step 3: Merge and deduplicate addresses from all feeds
        let mut all_addresses = birdeye_addresses;
        for addr in gecko_addresses {
            if !all_addresses.contains(&addr) {
                all_addresses.push(addr);
            }
        }

        if all_addresses.is_empty() {
            log::debug!("No trending tokens returned from any feed");
            return Ok(());
        }

        info!(
            "Combined trending: {} unique token addresses",
            all_addresses.len()
        );

        // Step 4: Enrich via DexScreener batch API
        let tokens = pumpportal::enrich_tokens_dexscreener(&all_addresses).await?;
        info!(
            "DexScreener enriched {}/{} tokens with pair data",
            tokens.len(),
            all_addresses.len()
        );

        // Step 5: Filter and analyze each token
        for token in tokens {
            // Skip tokens we already have a position in
            if self
                .trading_engine
                .get_positions()
                .contains_key(&token.address)
            {
                log::debug!("Already holding position in {}, skipping", token.symbol);
                continue;
            }

            // Apply config filters
            if !self.passes_trending_filters(&token) {
                continue;
            }

            // Analyze with strategies and potentially trade
            if let Err(e) = self.analyze_and_trade_token(token).await {
                error!("Error analyzing token: {}", e);
            }
        }

        Ok(())
    }

    fn passes_trending_filters(&self, token: &TokenInfo) -> bool {
        let cfg = &self.config.trendingtokenfilters;

        // PumpFun filter — PumpPortal API only supports PumpFun tokens
        if !token.address.ends_with("pump") {
            info!("Token {} FILTERED OUT: not a PumpFun token", token.symbol);
            return false;
        }

        // Market cap filter
        if token.market_cap < cfg.min_market_cap {
            info!(
                "Token {} FILTERED OUT: market_cap {} < min {}",
                token.symbol, token.market_cap, cfg.min_market_cap
            );
            return false;
        }
        if token.market_cap > cfg.max_market_cap {
            info!(
                "Token {} FILTERED OUT: market_cap {} > max {}",
                token.symbol, token.market_cap, cfg.max_market_cap
            );
            return false;
        }

        // Holders filter
        if token.holders < cfg.min_holders {
            info!(
                "Token {} FILTERED OUT: holders {} < min {}",
                token.symbol, token.holders, cfg.min_holders
            );
            return false;
        }

        // Age filter
        if token.age_hours > cfg.max_age_hours {
            info!(
                "Token {} FILTERED OUT: age {}h > max {}h",
                token.symbol, token.age_hours, cfg.max_age_hours
            );
            return false;
        }

        // Liquidity filter
        if token.liquidity < self.config.trendingtokenfilters.min_liquidity {
            info!(
                "Token {} FILTERED OUT: liquidity {} < min {}",
                token.symbol, token.liquidity, self.config.trendingtokenfilters.min_liquidity
            );
            return false;
        }

        // Volume filter
        if cfg.min_volume_24h > 0 && token.volume_24h < cfg.min_volume_24h {
            info!(
                "Token {} FILTERED OUT: volume_24h {} < min {}",
                token.symbol, token.volume_24h, cfg.min_volume_24h
            );
            return false;
        }

        // Holder density filter: holders per $10K market cap
        if cfg.min_holder_density > 0 && token.market_cap > 0 {
            let density = (token.holders as u64 * 10_000) / token.market_cap;
            if density < cfg.min_holder_density as u64 {
                info!(
                    "Token {} FILTERED OUT: holder_density {}/10K < min {}/10K (holders={}, mcap={})",
                    token.symbol, density, cfg.min_holder_density, token.holders, token.market_cap
                );
                return false;
            }
        }

        info!(
            "Token {} PASSED filters: mcap={} holders={} age={}h price={:.10} liq={} vol={}",
            token.symbol,
            token.market_cap,
            token.holders,
            token.age_hours,
            token.price_usd,
            token.liquidity,
            token.volume_24h
        );
        true
    }

    /// Check if a token passes the copy bot filter criteria.
    /// Returns true if filters are disabled OR if the token passes all enabled filters.
    /// 0 means "no filter" (disabled) for numeric thresholds.
    fn passes_copybot_filters(&self, token: &TokenInfo) -> bool {
        let cfg = &self.config.copybotfilters;
        if !cfg.enabled {
            return true;
        }

        // Reject unenriched tokens — DexScreener enrichment failed, all metadata is 0.
        // These trades have unknown quality and historically produce heavy losses.
        if token.market_cap == 0 && token.liquidity == 0 && token.holders == 0 {
            info!(
                "Copy trade {} FILTERED: unenriched (no DexScreener data available)",
                token.symbol
            );
            return false;
        }

        if cfg.min_market_cap > 0 && token.market_cap < cfg.min_market_cap {
            info!(
                "Copy trade {} FILTERED: market_cap {} < min {}",
                token.symbol, token.market_cap, cfg.min_market_cap
            );
            return false;
        }
        if cfg.max_market_cap > 0 && token.market_cap > cfg.max_market_cap {
            info!(
                "Copy trade {} FILTERED: market_cap {} > max {}",
                token.symbol, token.market_cap, cfg.max_market_cap
            );
            return false;
        }
        if cfg.min_holders > 0 && token.holders < cfg.min_holders {
            info!(
                "Copy trade {} FILTERED: holders {} < min {}",
                token.symbol, token.holders, cfg.min_holders
            );
            return false;
        }
        if cfg.min_liquidity > 0 && token.liquidity < cfg.min_liquidity {
            info!(
                "Copy trade {} FILTERED: liquidity {} < min {}",
                token.symbol, token.liquidity, cfg.min_liquidity
            );
            return false;
        }
        if cfg.max_age_hours > 0 && token.age_hours > cfg.max_age_hours {
            info!(
                "Copy trade {} FILTERED: age {}h > max {}h",
                token.symbol, token.age_hours, cfg.max_age_hours
            );
            return false;
        }
        if cfg.min_volume_24h > 0 && token.volume_24h < cfg.min_volume_24h {
            info!(
                "Copy trade {} FILTERED: volume_24h {} < min {}",
                token.symbol, token.volume_24h, cfg.min_volume_24h
            );
            return false;
        }
        // Holder density filter: blocks tokens with artificially inflated mcap relative to holders.
        // Density = holders per $10K mcap. Low density (e.g., 335 holders at $134K mcap = 25/10K)
        // indicates wash trading or botted volume — these tokens crash after copy-buy.
        if cfg.min_holder_density > 0 && token.market_cap > 0 {
            let density = (token.holders as u64 * 10_000) / token.market_cap;
            if density < cfg.min_holder_density as u64 {
                info!(
                    "Copy trade {} FILTERED: holder_density {}/10K < min {}/10K (holders={}, mcap={})",
                    token.symbol, density, cfg.min_holder_density, token.holders, token.market_cap
                );
                return false;
            }
        }

        info!(
            "Copy trade {} PASSED filters: mcap={} holders={} liq={} vol={}",
            token.symbol, token.market_cap, token.holders, token.liquidity, token.volume_24h
        );
        true
    }

    /// Handle a PumpFun → PumpSwap graduation. Builds a TokenInfo with the
    /// `just_graduated_at_secs` marker set to 0 (we caught it on the first WS push
    /// or watchlist tick) and forwards it to `analyze_and_trade_token`. The
    /// PumpSwapSniper strategy is the only consumer that fires on this marker.
    /// No filters are applied — graduation alone is the entire signal, per design.
    /// Dedups via `sniped_mints` so the WS listener and watchlist transition detector
    /// don't both buy the same token.
    async fn process_graduation(
        &mut self,
        ev: pumpportal::MigrationEvent,
    ) -> Result<()> {
        if !self.sniped_mints.insert(ev.mint.clone()) {
            log::debug!("Graduation for {} already processed; skipping dup", ev.mint);
            return Ok(());
        }

        info!(
            "Graduation detected: mint={} sig={}",
            ev.mint,
            if ev.signature.is_empty() { "?" } else { &ev.signature }
        );

        let token = pumpportal::TokenInfo {
            address: ev.mint.clone(),
            symbol: ev.mint[..8.min(ev.mint.len())].to_string(),
            name: "GRADUATED".to_string(),
            decimals: 6,
            market_cap: 0,
            holders: 0,
            age_hours: 0,
            liquidity: 0,
            price_usd: 0.0,
            price_change_24h: 0.0,
            volume_24h: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            just_graduated_at_secs: Some(0),
        };

        self.analyze_and_trade_token(token).await
    }

    async fn analyze_and_trade_token(&mut self, token: TokenInfo) -> Result<()> {
        info!("Analyzing token: {} ({})", token.symbol, token.address);

        // Get trading signals from strategy engine
        let signals = self.strategy_engine.analyze_token(&token)?;

        if signals.is_empty() {
            log::debug!("No trading signals for token {}", token.symbol);
            return Ok(());
        }

        // Execute signals
        for signal in signals {
            if signal.confidence < 0.5 {
                log::debug!(
                    "Low confidence signal for {}: {:.2}",
                    token.symbol,
                    signal.confidence
                );
                continue;
            }

            info!(
                "Executing {:#?} signal for {}: {} (confidence: {:.2})",
                signal.action, token.symbol, signal.reason, signal.confidence
            );

            if !self.dry_run {
                let token_for_journal = signal.token.clone();
                let sol_amount = signal
                    .override_buy_amount
                    .unwrap_or(self.config.trading.max_buy_amount)
                    as f64
                    / 1_000_000_000.0;
                if let Some(trade) = self.trading_engine.execute_signal(signal).await? {
                    if trade.action == Action::Buy {
                        self.journal
                            .log_buy(&token_for_journal, &trade, sol_amount, None);
                    }
                    self.trading_engine.add_trade(trade);
                    info!("Trade executed successfully");
                }
            } else {
                info!("DRY RUN: Would execute trade");
            }
        }

        Ok(())
    }

    async fn check_exit_conditions(&mut self) -> Result<()> {
        let exit_signals = self.trading_engine.check_exit_conditions().await?;

        for signal in exit_signals {
            info!("Exit condition triggered: {}", signal.reason);

            // Snapshot position state before the sell removes it
            let position_snapshot = self
                .trading_engine
                .get_positions()
                .get(&signal.token.address)
                .cloned();

            let exit_reason = signal.reason.clone();
            let token_address = signal.token.address.clone();

            if !self.dry_run {
                // Capture the on-chain SOL exit price from the signal BEFORE executing
                let exit_price_sol = signal.token.price_usd; // price_usd field carries SOL price from exit check

                match self.trading_engine.execute_signal(signal).await {
                    Ok(Some(trade)) => {
                        // Enrich with DexScreener for metadata (holders, volume, etc.) but override price with on-chain SOL price
                        let exit_token =
                            match pumpportal::enrich_tokens_dexscreener(&[token_address.clone()])
                                .await
                            {
                                Ok(tokens) => {
                                    let mut t =
                                        tokens.into_iter().next().unwrap_or_else(|| TokenInfo {
                                            address: token_address.clone(),
                                            symbol: "UNKNOWN".to_string(),
                                            name: "Unknown".to_string(),
                                            decimals: 6,
                                            market_cap: 0,
                                            holders: 0,
                                            age_hours: 0,
                                            liquidity: 0,
                                            price_usd: exit_price_sol,
                                            price_change_24h: 0.0,
                                            volume_24h: 0,
                                            created_at: String::new(),
                                            just_graduated_at_secs: None,
                                        });
                                    // Override DexScreener price with on-chain SOL exit price
                                    t.price_usd = exit_price_sol;
                                    t
                                }
                                Err(_) => TokenInfo {
                                    address: token_address.clone(),
                                    symbol: "UNKNOWN".to_string(),
                                    name: "Unknown".to_string(),
                                    decimals: 6,
                                    market_cap: 0,
                                    holders: 0,
                                    age_hours: 0,
                                    liquidity: 0,
                                    price_usd: exit_price_sol,
                                    price_change_24h: 0.0,
                                    volume_24h: 0,
                                    created_at: String::new(),
                                    just_graduated_at_secs: None,
                                },
                            };

                        if let Some(pos) = position_snapshot {
                            self.journal.log_sell(
                                &token_address,
                                pos.entry_price,
                                &exit_token,
                                &trade,
                                pos.sol_spent,
                                pos.peak_pnl_percent,
                                &exit_reason,
                                pos.source_wallet.as_deref(),
                            );
                            // Track loss sells for loss cooldown filter
                            // Use actual SOL received vs spent (not price comparison) to account for slippage
                            let is_loss = trade
                                .sol_received
                                .map(|received| received < pos.sol_spent)
                                .unwrap_or(exit_price_sol < pos.entry_price);
                            if is_loss {
                                self.loss_cooldown
                                    .insert(token_address.clone(), std::time::Instant::now());
                            }

                            // Update wallet quality score after every sell
                            if self.config.walletscoring.enabled {
                                if let Some(ref source_wallet) = pos.source_wallet {
                                    let pnl_sol = trade
                                        .sol_received
                                        .map(|recv| recv - pos.sol_spent)
                                        .unwrap_or(0.0);
                                    self.wallet_scorer.update(
                                        source_wallet,
                                        pnl_sol,
                                        &self.config.walletscoring,
                                    );
                                    // Check if wallet should be removed
                                    let removal_score =
                                        self.config.walletscoring.wallet_removal_score;
                                    if removal_score > 0.0 {
                                        if let Some(score) =
                                            self.wallet_scorer.get_score(source_wallet)
                                        {
                                            if score <= removal_score {
                                                warn!(
                                                    "Wallet {} score {:.3} hit removal threshold {:.3} — removing from .env and blocking",
                                                    source_wallet, score, removal_score
                                                );
                                                self.wallet_scorer.remove_wallet(source_wallet);
                                                self.blocked_wallets.insert(source_wallet.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        self.trading_engine.add_trade(trade);
                        info!("Exit trade executed successfully");
                    }
                    Ok(None) => {
                        // Signal executed but no trade returned (e.g., position already gone)
                        info!(
                            "Exit signal for {} processed but no trade returned",
                            token_address
                        );
                    }
                    Err(e) => {
                        // Sell failed — increment failure counter instead of aborting the loop
                        warn!(
                            "Sell failed for {}: {} — incrementing failure counter",
                            token_address, e
                        );
                        self.trading_engine.increment_sell_failures(&token_address);
                        continue;
                    }
                }
            } else {
                info!("DRY RUN: Would execute exit trade");
            }
        }

        Ok(())
    }

    async fn update_monitoring(&mut self) {
        let positions = self.trading_engine.get_positions().clone();
        let trades = self.trading_engine.get_trade_history().clone();

        self.monitoring.update_metrics(&trades, &positions);

        // Check for unacknowledged alerts
        let unacknowledged = self.monitoring.get_unacknowledged_alerts();
        for alert in unacknowledged {
            match alert.level {
                crate::monitoring::AlertLevel::Critical => {
                    error!("CRITICAL ALERT: {}", alert.message);
                }
                crate::monitoring::AlertLevel::Error => {
                    error!("ERROR: {}", alert.message);
                }
                crate::monitoring::AlertLevel::Warning => {
                    warn!("WARNING: {}", alert.message);
                }
                crate::monitoring::AlertLevel::Info => {
                    info!("INFO: {}", alert.message);
                }
            }
        }
    }

    async fn log_status(&self) {
        let metrics = self.monitoring.get_metrics();
        let positions = self.trading_engine.get_positions();

        let balance_sol = self
            .wallet
            .get_balance()
            .map(|b| b as f64 / 1_000_000_000.0)
            .unwrap_or(0.0);

        info!(
            "Status - Balance: {:.4} SOL | Trades: {} | Win Rate: {:.1}% | P&L: {:+.6} SOL | Positions: {} | Uptime: {}s",
            balance_sol,
            metrics.total_trades,
            metrics.win_rate,
            metrics.total_profit_loss,
            positions.len(),
            metrics.uptime_seconds
        );

        if !positions.is_empty() {
            info!("Current positions:");
            for (address, position) in positions {
                info!(
                    "  {}: {} tokens @ {:.8}",
                    address, position.amount, position.entry_price
                );
            }
        }
    }
}
