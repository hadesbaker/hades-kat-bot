use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// RPC URL. Sourced from the `SOLANA_RPC_URL` environment variable in `.env`,
    /// not from `config.toml`. Defaulted/overridden in `Config::load`.
    #[serde(default = "default_rpc_url")]
    pub rpc_url: String,
    pub wallet_path: String,
    pub birdeye_token_feed: bool,
    pub coingecko_token_feed: bool,
    pub target_wallet_token_feed: bool,
    #[serde(default)]
    pub new_token_event_feed: bool,
    #[serde(default = "default_true")]
    pub pumpfun_enabled: bool,
    #[serde(default = "default_true")]
    pub pumpswap_enabled: bool,
    pub trading: TradingConfig,
    pub birdeye: BirdeyeConfig,
    pub trendingtokenfilters: TrendingTokenFilters,
    #[serde(default)]
    pub newtokenfilters: NewTokenFilters,
    #[serde(default)]
    pub copybotfilters: CopyBotFilters,
    #[serde(default)]
    pub walletscoring: WalletScoring,
    #[serde(default)]
    pub exitstrategies: ExitStrategies,
    #[serde(default)]
    pub strategies: StrategyToggles,
    pub params: StrategyParams,
    pub monitoring: MonitoringConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub max_slippage: f64,
    pub max_buy_amount: u64,
    pub profit_target_percent: f64,
    pub stop_loss_percent: f64,
    pub cooldown_seconds: u64,
    pub max_positions: usize,
    pub exit_check_interval_ms: u64,
    #[serde(default)]
    pub dynamic_trailing_stop_thresholds: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BirdeyeConfig {
    pub api_key: String,
    pub trending_limit: u32,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendingTokenFilters {
    pub min_market_cap: u64,
    pub max_market_cap: u64,
    pub min_holders: u32,
    pub max_age_hours: u32,
    pub min_liquidity: u64,
    #[serde(default)]
    pub min_volume_24h: u64,
    #[serde(default)]
    pub min_holder_density: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyParams {
    // Momentum
    pub min_price_change: f64,
    pub min_volume_ratio: f64,
    // MeanReversion
    pub max_price_change: f64,
    pub min_liquidity_ratio: f64,
    // Breakout
    pub min_volume_spike: f64,
    pub min_price_momentum: f64,
    // VolumeSpike
    pub min_volume_multiplier: f64,
    pub min_holder_count: f64,
    // HolderGrowth
    pub min_growth_holders: f64,
    pub min_growth_market_cap: f64,
    // LiquidityDepth
    #[serde(default = "default_liq_mcap_ratio")]
    pub min_liq_mcap_ratio: f64,
    #[serde(default = "default_liq_min_usd")]
    pub min_liquidity_usd: f64,
    #[serde(default = "default_liq_max_mcap")]
    pub liq_max_market_cap: f64,
    // PumpSwapSniper — max age (seconds since graduation) to still fire a buy
    #[serde(default = "default_ps_sniper_freshness_secs")]
    pub ps_sniper_freshness_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringConfig {
    pub webhook_url: Option<String>,
    pub alert_thresholds: AlertThresholds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertThresholds {
    pub max_drawdown_percent: f64,
    pub min_daily_profit_percent: f64,
    pub max_daily_loss_percent: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            wallet_path: "wallet.json".to_string(),
            birdeye_token_feed: true,
            coingecko_token_feed: true,
            target_wallet_token_feed: false,
            new_token_event_feed: false,
            pumpfun_enabled: true,
            pumpswap_enabled: true,
            trading: TradingConfig::default(),
            birdeye: BirdeyeConfig::default(),
            trendingtokenfilters: TrendingTokenFilters::default(),
            newtokenfilters: NewTokenFilters::default(),
            copybotfilters: CopyBotFilters::default(),
            walletscoring: WalletScoring::default(),
            exitstrategies: ExitStrategies::default(),
            strategies: StrategyToggles::default(),
            params: StrategyParams::default(),
            monitoring: MonitoringConfig::default(),
        }
    }
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            max_slippage: 5.0,
            max_buy_amount: 1_000_000_000,  // 1 SOL
            profit_target_percent: 20.0,
            stop_loss_percent: 10.0,
            cooldown_seconds: 60,
            max_positions: 5,
            exit_check_interval_ms: 5_000, // 5 Seconds
            dynamic_trailing_stop_thresholds: String::new(),
        }
    }
}

impl Default for BirdeyeConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            trending_limit: 100,
            poll_interval_ms: 300_000, // 5 minutes
        }
    }
}

impl Default for TrendingTokenFilters {
    fn default() -> Self {
        Self {
            min_market_cap: 2_800,
            max_market_cap: 10_000_000,
            min_holders: 10,
            max_age_hours: 48,
            min_liquidity: 100_000_000, // 0.1 SOL
            min_volume_24h: 0,
            min_holder_density: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTokenFilters {
    /// Minimum market cap in SOL from PumpPortal event (pre-filter on arrival, before watchlist)
    #[serde(default = "default_min_initial_mcap_sol")]
    pub min_initial_market_cap_sol: f64,
    /// Minimum market cap in USD (applied when enriched data available)
    #[serde(default)]
    pub min_market_cap: u64,
    /// Maximum market cap in USD
    #[serde(default)]
    pub max_market_cap: u64,
    /// Minimum number of holders
    #[serde(default)]
    pub min_holders: u32,
    /// Minimum liquidity in USD
    #[serde(default)]
    pub min_liquidity: u64,
    /// Minimum 24h volume in USD
    #[serde(default)]
    pub min_volume_24h: u64,
    /// How long to monitor each new token (minutes)
    #[serde(default = "default_monitor_duration")]
    pub monitor_duration_minutes: u64,
    /// Maximum tokens in the watchlist at once
    #[serde(default = "default_watchlist_cap")]
    pub watchlist_cap: usize,
    /// How often to check watchlist tokens (seconds) — initial frequency
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Reduced poll frequency after reduced_poll_after_minutes (seconds)
    #[serde(default = "default_reduced_poll_interval")]
    pub reduced_poll_interval_secs: u64,
    /// Switch to reduced polling after this many minutes
    #[serde(default = "default_reduced_poll_after")]
    pub reduced_poll_after_minutes: u64,
    /// Drop from peak percentage to consider a token rugged (e.g. 90 = 90% drop)
    #[serde(default = "default_rug_drop_percent")]
    pub rug_drop_percent: f64,
}

fn default_min_initial_mcap_sol() -> f64 { 0.5 }
fn default_monitor_duration() -> u64 { 60 }
fn default_watchlist_cap() -> usize { 200 }
fn default_poll_interval() -> u64 { 30 }
fn default_reduced_poll_interval() -> u64 { 120 }
fn default_reduced_poll_after() -> u64 { 15 }
fn default_rug_drop_percent() -> f64 { 90.0 }

impl Default for NewTokenFilters {
    fn default() -> Self {
        Self {
            min_initial_market_cap_sol: 0.5,
            min_market_cap: 0,
            max_market_cap: 0,
            min_holders: 0,
            min_liquidity: 0,
            min_volume_24h: 0,
            monitor_duration_minutes: 60,
            watchlist_cap: 200,
            poll_interval_secs: 30,
            reduced_poll_interval_secs: 120,
            reduced_poll_after_minutes: 15,
            rug_drop_percent: 90.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyBotFilters {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub min_market_cap: u64,
    #[serde(default)]
    pub max_market_cap: u64,
    #[serde(default)]
    pub min_holders: u32,
    #[serde(default)]
    pub min_liquidity: u64,
    #[serde(default)]
    pub max_age_hours: u32,
    #[serde(default)]
    pub min_volume_24h: u64,
    #[serde(default = "default_loss_cooldown")]
    pub loss_cooldown_minutes: u32,
    #[serde(default)]
    pub min_holder_density: u32,
    /// Run strategy analysis before buying copy trades (true = analyze, false = buy immediately)
    #[serde(default = "default_true")]
    pub analysis_step: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletScoring {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_initial_score")]
    pub initial_score: f64,
    #[serde(default = "default_sensitivity")]
    pub sensitivity: f64,
    #[serde(default = "default_min_score")]
    pub min_score: f64,
    #[serde(default = "default_max_score")]
    pub max_score: f64,
    #[serde(default = "default_min_trades")]
    pub min_trades_for_scoring: u32,
    #[serde(default)]
    pub wallet_removal_score: f64,
}

fn default_initial_score() -> f64 { 1.0 }
fn default_sensitivity() -> f64 { 5.0 }
fn default_min_score() -> f64 { 0.1 }
fn default_max_score() -> f64 { 2.0 }
fn default_min_trades() -> u32 { 3 }

impl Default for WalletScoring {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_score: 1.0,
            sensitivity: 5.0,
            min_score: 0.1,
            max_score: 2.0,
            min_trades_for_scoring: 3,
            wallet_removal_score: 0.0,
        }
    }
}

fn default_loss_cooldown() -> u32 { 15 }

fn default_true() -> bool {
    true
}

impl Default for CopyBotFilters {
    fn default() -> Self {
        Self {
            enabled: true,
            min_market_cap: 0,
            max_market_cap: 0,
            min_holders: 0,
            min_liquidity: 0,
            max_age_hours: 0,
            min_volume_24h: 0,
            loss_cooldown_minutes: 15,
            min_holder_density: 0,
            analysis_step: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitStrategies {
    #[serde(default = "default_enrichment_interval")]
    pub enrichment_interval_ms: u64,
    #[serde(default)]
    pub time_exit_enabled: bool,
    #[serde(default)]
    pub max_hold_minutes: u64,
    #[serde(default)]
    pub liquidity_exit_enabled: bool,
    #[serde(default)]
    pub min_liquidity_sol: f64,
    #[serde(default)]
    pub velocity_exit_enabled: bool,
    #[serde(default)]
    pub max_decline_rate_per_min: f64,
    #[serde(default)]
    pub momentum_reversal_enabled: bool,
    #[serde(default)]
    pub momentum_reversal_window_secs: u64,
    #[serde(default)]
    pub momentum_reversal_min_loss_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyToggles {
    #[serde(default)]
    pub momentum: bool,
    #[serde(default)]
    pub mean_reversion: bool,
    #[serde(default)]
    pub breakout: bool,
    #[serde(default)]
    pub volume_spike: bool,
    #[serde(default)]
    pub holder_growth: bool,
    #[serde(default)]
    pub liquidity_depth: bool,
    #[serde(default)]
    pub ps_sniper: bool,
}

impl Default for StrategyToggles {
    fn default() -> Self {
        Self {
            momentum: false,
            mean_reversion: false,
            breakout: false,
            volume_spike: false,
            holder_growth: false,
            liquidity_depth: false,
            ps_sniper: false,
        }
    }
}

fn default_enrichment_interval() -> u64 {
    30_000
}

impl Default for ExitStrategies {
    fn default() -> Self {
        Self {
            enrichment_interval_ms: 30_000,
            time_exit_enabled: false,
            max_hold_minutes: 0,
            liquidity_exit_enabled: false,
            min_liquidity_sol: 0.0,
            velocity_exit_enabled: false,
            max_decline_rate_per_min: 0.0,
            momentum_reversal_enabled: false,
            momentum_reversal_window_secs: 60,
            momentum_reversal_min_loss_pct: 3.0,
        }
    }
}

impl Default for StrategyParams {
    fn default() -> Self {
        Self {
            min_price_change: 5.0,
            min_volume_ratio: 2.0,
            max_price_change: -5.0,
            min_liquidity_ratio: 0.05,
            min_volume_spike: 1.5,
            min_price_momentum: 1.0,
            min_volume_multiplier: 2.0,
            min_holder_count: 5.0,
            min_growth_holders: 10.0,
            min_growth_market_cap: 3_000.0,
            min_liq_mcap_ratio: 0.03,
            min_liquidity_usd: 5_000.0,
            liq_max_market_cap: 5_000_000.0,
            ps_sniper_freshness_secs: 30.0,
        }
    }
}

fn default_liq_mcap_ratio() -> f64 { 0.03 }
fn default_liq_min_usd() -> f64 { 5_000.0 }
fn default_liq_max_mcap() -> f64 { 5_000_000.0 }
fn default_ps_sniper_freshness_secs() -> f64 { 30.0 }

fn default_rpc_url() -> String {
    "https://api.mainnet-beta.solana.com".to_string()
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            alert_thresholds: AlertThresholds::default(),
        }
    }
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            max_drawdown_percent: 20.0,
            min_daily_profit_percent: 5.0,
            max_daily_loss_percent: 15.0,
        }
    }
}

impl TradingConfig {
    /// Parse "6:4,12:5,20:6" into sorted Vec of (gain_threshold, trail_percent).
    pub fn parsed_trailing_thresholds(&self) -> Vec<(f64, f64)> {
        let mut thresholds: Vec<(f64, f64)> = self
            .dynamic_trailing_stop_thresholds
            .split(',')
            .filter_map(|pair| {
                let parts: Vec<&str> = pair.trim().split(':').collect();
                if parts.len() == 2 {
                    if let (Ok(gain), Ok(trail)) =
                        (parts[0].trim().parse::<f64>(), parts[1].trim().parse::<f64>())
                    {
                        return Some((gain, trail));
                    }
                }
                None
            })
            .collect();
        thresholds.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        thresholds
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let config_str = std::fs::read_to_string(path).map_err(|e| {
            crate::error::BotError::Config(format!("Failed to read config file: {}", e))
        })?;
        let mut config: Config = toml::from_str(&config_str).map_err(|e| {
            crate::error::BotError::Config(format!("Failed to parse config: {}", e))
        })?;

        // Sensitive fields live in `.env` so they don't get committed alongside other
        // config. Each one falls back to whatever (if anything) is in config.toml.
        if let Ok(env_url) = std::env::var("SOLANA_RPC_URL") {
            if !env_url.trim().is_empty() {
                config.rpc_url = env_url;
            }
        }
        if config.rpc_url.trim().is_empty() {
            config.rpc_url = default_rpc_url();
        }

        if let Ok(env_key) = std::env::var("BIRDEYE_API_KEY") {
            if !env_key.trim().is_empty() {
                config.birdeye.api_key = env_key;
            }
        }

        if let Ok(env_webhook) = std::env::var("DISCORD_WEBHOOK_URL") {
            if !env_webhook.trim().is_empty() {
                config.monitoring.webhook_url = Some(env_webhook);
            }
        }

        Ok(config)
    }

}
