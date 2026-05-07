use crate::error::Result;
use crate::pumpportal::TokenInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradingStrategy {
    Momentum,
    MeanReversion,
    Breakout,
    VolumeSpike,
    HolderGrowth,
    LiquidityDepth,
    PumpSwapSniper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub strategy: TradingStrategy,
    pub parameters: HashMap<String, f64>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct TradingSignal {
    pub token: TokenInfo,
    pub action: Action,
    pub confidence: f64,
    pub reason: String,
    pub expected_price: Option<f64>,
    /// If set, overrides the confidence-based buy amount calculation (in lamports).
    pub override_buy_amount: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Buy,
    Sell,
    Hold,
}

pub struct StrategyEngine {
    strategies: Vec<StrategyConfig>,
}

impl StrategyEngine {
    pub fn new(strategies: Vec<StrategyConfig>) -> Self {
        Self { strategies }
    }

    pub fn analyze_token(&mut self, token: &TokenInfo) -> Result<Vec<TradingSignal>> {
        let mut signals = Vec::new();

        for strategy_config in &self.strategies {
            if !strategy_config.enabled {
                continue;
            }

            match strategy_config.strategy {
                TradingStrategy::Momentum => {
                    if let Some(signal) =
                        self.momentum_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
                TradingStrategy::MeanReversion => {
                    if let Some(signal) =
                        self.mean_reversion_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
                TradingStrategy::Breakout => {
                    if let Some(signal) =
                        self.breakout_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
                TradingStrategy::VolumeSpike => {
                    if let Some(signal) =
                        self.volume_spike_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
                TradingStrategy::HolderGrowth => {
                    if let Some(signal) =
                        self.holder_growth_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
                TradingStrategy::LiquidityDepth => {
                    if let Some(signal) =
                        self.liquidity_depth_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
                TradingStrategy::PumpSwapSniper => {
                    if let Some(signal) =
                        self.ps_sniper_strategy(token, &strategy_config.parameters)?
                    {
                        signals.push(signal);
                    }
                }
            }
        }

        Ok(signals)
    }

    /// Fires Buy with confidence 1.0 when the token's `just_graduated_at_secs` is
    /// set and within the configured freshness window. Strictly graduation-driven —
    /// any token whose TokenInfo lacks the graduation marker is silently skipped.
    fn ps_sniper_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let max_age_secs = *params.get("ps_sniper_freshness_secs").unwrap_or(&30.0) as u64;

        let age_secs = match token.just_graduated_at_secs {
            Some(s) => s,
            None => return Ok(None),
        };

        if age_secs > max_age_secs {
            return Ok(None);
        }

        Ok(Some(TradingSignal {
            token: token.clone(),
            action: Action::Buy,
            confidence: 1.0,
            reason: format!(
                "PumpSwapSniper: token graduated {}s ago (window {}s)",
                age_secs, max_age_secs
            ),
            expected_price: None,
            override_buy_amount: None,
        }))
    }

    fn momentum_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let min_price_change = params.get("min_price_change").unwrap_or(&5.0);
        let min_volume_ratio = params.get("min_volume_ratio").unwrap_or(&2.0);

        // Check if price is increasing significantly
        if token.price_change_24h >= *min_price_change {
            // Check if volume is also increasing
            let volume_ratio = token.volume_24h as f64 / token.liquidity as f64;
            if volume_ratio >= *min_volume_ratio {
                return Ok(Some(TradingSignal {
                    token: token.clone(),
                    action: Action::Buy,
                    confidence: (token.price_change_24h / 100.0).min(1.0),
                    reason: format!(
                        "Momentum: Price up {:.2}%, Volume ratio {:.2}",
                        token.price_change_24h, volume_ratio
                    ),
                    expected_price: Some(token.price_usd * 1.1),
                    override_buy_amount: None,
                }));
            }
        }

        Ok(None)
    }

    fn mean_reversion_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let max_price_change = params.get("max_price_change").unwrap_or(&-5.0);
        let min_liquidity_ratio = params.get("min_liquidity_ratio").unwrap_or(&0.05);

        // Check if price has dropped significantly but liquidity is still good
        if token.price_change_24h <= *max_price_change {
            let liquidity_ratio = token.liquidity as f64 / token.market_cap as f64;
            if liquidity_ratio >= *min_liquidity_ratio {
                return Ok(Some(TradingSignal {
                    token: token.clone(),
                    action: Action::Buy,
                    confidence: (-token.price_change_24h / 100.0).min(1.0),
                    reason: format!(
                        "Mean Reversion: Price down {:.2}%, Liquidity ratio {:.2}",
                        token.price_change_24h, liquidity_ratio
                    ),
                    expected_price: Some(token.price_usd * 1.05),
                    override_buy_amount: None,
                }));
            }
        }

        Ok(None)
    }

    fn breakout_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let min_volume_spike = params.get("min_volume_spike").unwrap_or(&1.5);
        let min_price_momentum = params.get("min_price_momentum").unwrap_or(&1.0);

        // Check for volume spike with price momentum
        let volume_spike = token.volume_24h as f64 / token.liquidity as f64;
        if volume_spike >= *min_volume_spike && token.price_change_24h >= *min_price_momentum {
            return Ok(Some(TradingSignal {
                token: token.clone(),
                action: Action::Buy,
                confidence: (volume_spike / 10.0).min(1.0),
                reason: format!(
                    "Breakout: Volume spike {:.2}x, Price momentum {:.2}%",
                    volume_spike, token.price_change_24h
                ),
                expected_price: Some(token.price_usd * 1.15),
                override_buy_amount: None,
            }));
        }

        Ok(None)
    }

    fn volume_spike_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let min_volume_multiplier = params.get("min_volume_multiplier").unwrap_or(&2.0);
        let min_holders = *params.get("min_holders").unwrap_or(&5.0) as u32;

        // Check for sudden volume increase with decent holder count
        let volume_multiplier = token.volume_24h as f64 / token.liquidity as f64;
        if volume_multiplier >= *min_volume_multiplier && token.holders >= min_holders {
            return Ok(Some(TradingSignal {
                token: token.clone(),
                action: Action::Buy,
                confidence: (volume_multiplier / 20.0).min(1.0),
                reason: format!(
                    "Volume Spike: {:.2}x volume, {} holders",
                    volume_multiplier, token.holders
                ),
                expected_price: Some(token.price_usd * 1.2),
                override_buy_amount: None,
            }));
        }

        Ok(None)
    }

    fn holder_growth_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let min_holders = *params.get("min_growth_holders").unwrap_or(&10.0) as u32;
        let min_market_cap = *params.get("min_growth_market_cap").unwrap_or(&3_000.0) as u64;

        // Look for tokens with growing holder base and decent market cap
        if token.holders >= min_holders && token.market_cap >= min_market_cap {
            let holder_ratio = token.holders as f64 / (token.market_cap as f64 / 1_000_000.0);
            if holder_ratio >= 0.1 {
                // At least 0.1 holders per $1M market cap
                return Ok(Some(TradingSignal {
                    token: token.clone(),
                    action: Action::Buy,
                    confidence: (holder_ratio / 2.0).min(1.0),
                    reason: format!(
                        "Holder Growth: {} holders, ${:.0}M market cap",
                        token.holders,
                        token.market_cap as f64 / 1_000_000.0
                    ),
                    expected_price: Some(token.price_usd * 1.08),
                    override_buy_amount: None,
                }));
            }
        }

        Ok(None)
    }

    fn liquidity_depth_strategy(
        &self,
        token: &TokenInfo,
        params: &HashMap<String, f64>,
    ) -> Result<Option<TradingSignal>> {
        let min_liq_mcap_ratio = params.get("min_liq_mcap_ratio").unwrap_or(&0.03);
        let min_liquidity = *params.get("min_liquidity_usd").unwrap_or(&5_000.0) as u64;
        let max_market_cap = *params.get("max_market_cap").unwrap_or(&5_000_000.0) as u64;

        if token.market_cap == 0 || token.liquidity < min_liquidity {
            return Ok(None);
        }
        if token.market_cap > max_market_cap {
            return Ok(None);
        }

        let liq_ratio = token.liquidity as f64 / token.market_cap as f64;
        if liq_ratio >= *min_liq_mcap_ratio {
            let confidence = (liq_ratio / 0.2).min(1.0);
            return Ok(Some(TradingSignal {
                token: token.clone(),
                action: Action::Buy,
                confidence,
                reason: format!(
                    "LiquidityDepth: liq/mcap ratio {:.4}, liquidity ${}, mcap ${}",
                    liq_ratio, token.liquidity, token.market_cap
                ),
                expected_price: Some(token.price_usd * 1.1),
                override_buy_amount: None,
            }));
        }

        Ok(None)
    }
}
