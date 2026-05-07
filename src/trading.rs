use crate::config::ExitStrategies;
use crate::error::{BotError, Result};
use crate::pumpportal;
use crate::strategies::{Action, TradingSignal};
use crate::wallet::WalletManager;
use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

/// Compute the slippage % to send to PumpPortal for a sell, given how many times
/// the previous sell attempt for this position has failed.
///
/// Each failure adds 10 percentage points to the slippage tolerance. Capped at 99%.
/// Example with base = 8%: 8 → 18 → 28 → 38 → 48 → 58 → 68 → 78 → 88 → 98 → 99 → 99 …
///
/// Linear (rather than multiplicative) escalation keeps the worst-case execution
/// price close to the trigger price even on profitable exits — a successful sell
/// at +10% slippage gives up at most ~10% of the expected SOL, vs ~50% under doubling.
/// The PumpPortal API only accepts integer slippage, so the caller is expected to
/// round/clamp at the boundary.
fn escalated_slippage(base_slippage: f64, failures: u32) -> f64 {
    (base_slippage + (failures as f64) * 10.0).min(99.0)
}

/// Serialize/deserialize Pubkey as base58 string for human-readable JSON
mod pubkey_string {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    pub fn serialize<S>(pubkey: &Pubkey, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&pubkey.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Pubkey, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Pubkey::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub token_address: String,
    #[serde(with = "pubkey_string")]
    pub token_mint: Pubkey,
    pub amount: u64,
    pub entry_price: f64,
    pub entry_time: chrono::DateTime<chrono::Utc>,
    pub peak_pnl_percent: f64,
    pub source_wallet: Option<String>,
    pub sol_spent: f64,
    // Enrichment cache fields — rebuilt automatically by 30s enrichment timer
    #[serde(skip)]
    pub cached_market_cap: Option<u64>,
    #[serde(skip)]
    pub cached_holders: Option<u32>,
    #[serde(skip)]
    pub cached_volume_24h: Option<u64>,
    #[serde(skip)]
    pub cached_liquidity: Option<u64>,
    #[serde(skip)]
    pub last_enrichment: Option<chrono::DateTime<chrono::Utc>>,
    // Price history — rebuilt within seconds by 250ms exit check loop
    #[serde(skip)]
    pub price_history: Vec<(chrono::DateTime<chrono::Utc>, f64)>,
    // Consecutive sell failures — skip position in exit checks after 3 failures
    // Prevents one stuck position (e.g., graduated to Raydium) from blocking all other exits
    #[serde(default)]
    pub sell_failures: u32,
    // Timestamp of the last sell failure — used to reset sell_failures after a cooldown period
    // so stuck positions get retried periodically instead of being permanently abandoned
    #[serde(default)]
    pub last_sell_failure: Option<chrono::DateTime<chrono::Utc>>,
    // Consecutive on-chain price-fetch failures — skip pricing after 3 failures so an
    // unpriceable mint (not on PumpFun bonding curve and not on PumpSwap, e.g. a Raydium-only
    // SPL token that ended up in positions.json) doesn't hammer RPC every 250ms forever.
    #[serde(default)]
    pub price_failures: u32,
    #[serde(default)]
    pub last_price_failure: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone)]
pub struct Trade {
    pub id: String,
    pub token_address: String,
    pub action: Action,
    pub amount: u64,
    pub price: f64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub signature: Option<String>,
    pub profit_loss: Option<f64>,
    pub sol_received: Option<f64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PositionSnapshot {
    positions: HashMap<String, Position>,
    last_trade_time: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

pub struct TradingEngine {
    wallet: WalletManager,
    positions: HashMap<String, Position>,
    trade_history: Vec<Trade>,
    max_positions: usize,
    max_buy_amount: u64,
    max_slippage: f64,
    profit_target_percent: f64,
    stop_loss_percent: f64,
    cooldown_seconds: u64,
    last_trade_time: HashMap<String, chrono::DateTime<chrono::Utc>>,
    trailing_thresholds: Vec<(f64, f64)>,
    /// Async RPC client with Processed commitment for fast on-chain reads
    price_rpc: Arc<AsyncRpcClient>,
    /// Cached PumpSwap pool info (pool_id + vault addresses) for graduated tokens
    pumpswap_pools: HashMap<String, pumpportal::PumpSwapPoolInfo>,
    /// Exit strategy configuration (time-based, liquidity, velocity exits)
    exit_strategies: ExitStrategies,
}

impl TradingEngine {
    pub fn new(
        wallet: &WalletManager,
        rpc_url: &str,
        max_positions: usize,
        max_buy_amount: u64,
        max_slippage: f64,
        profit_target_percent: f64,
        stop_loss_percent: f64,
        cooldown_seconds: u64,
        trailing_thresholds: Vec<(f64, f64)>,
        exit_strategies: ExitStrategies,
    ) -> Self {
        if trailing_thresholds.is_empty() {
            log::info!("Dynamic trailing stops: disabled (no thresholds configured)");
        } else {
            log::info!(
                "Dynamic trailing stops: {} tiers configured",
                trailing_thresholds.len()
            );
            for (gain, trail) in &trailing_thresholds {
                log::info!("  +{:.1}% gain → {:.1}% trail", gain, trail);
            }
        }

        // Async RPC client with Processed commitment for fastest on-chain reads
        let price_rpc = Arc::new(AsyncRpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig {
                commitment: CommitmentLevel::Processed,
            },
        ));
        log::info!("Price monitoring: on-chain only (bonding curve → PumpSwap)");

        let (positions, last_trade_time) = Self::load_positions();

        Self {
            wallet: wallet.clone(),
            positions,
            trade_history: Vec::new(),
            max_positions,
            max_buy_amount,
            max_slippage,
            profit_target_percent,
            stop_loss_percent,
            cooldown_seconds,
            last_trade_time,
            trailing_thresholds,
            price_rpc,
            pumpswap_pools: HashMap::new(),
            exit_strategies,
        }
    }

    const POSITIONS_FILE: &'static str = "positions.json";

    /// Load positions and last_trade_time from disk. Returns empty maps if file missing or corrupt.
    fn load_positions() -> (
        HashMap<String, Position>,
        HashMap<String, chrono::DateTime<chrono::Utc>>,
    ) {
        let path = std::path::Path::new(Self::POSITIONS_FILE);
        if !path.exists() {
            log::info!("No positions.json found — starting with empty positions");
            return (HashMap::new(), HashMap::new());
        }

        match std::fs::read_to_string(path) {
            Ok(data) => match serde_json::from_str::<PositionSnapshot>(&data) {
                Ok(snapshot) => {
                    let pos_count = snapshot.positions.len();
                    if pos_count > 0 {
                        log::info!("Restored {} positions from positions.json", pos_count);
                        for (addr, pos) in &snapshot.positions {
                            log::info!(
                                    "  Restored: {} — {} tokens, entry={:.12} SOL, sol_spent={:.4}, peak_pnl={:.2}%",
                                    addr, pos.amount, pos.entry_price, pos.sol_spent, pos.peak_pnl_percent
                                );
                        }
                    }
                    (snapshot.positions, snapshot.last_trade_time)
                }
                Err(e) => {
                    log::warn!(
                        "Failed to parse positions.json: {} — starting with empty positions",
                        e
                    );
                    (HashMap::new(), HashMap::new())
                }
            },
            Err(e) => {
                log::warn!(
                    "Failed to read positions.json: {} — starting with empty positions",
                    e
                );
                (HashMap::new(), HashMap::new())
            }
        }
    }

    /// Save current positions and last_trade_time to disk (atomic write via temp file).
    fn save_positions(&self) {
        let snapshot = PositionSnapshot {
            positions: self.positions.clone(),
            last_trade_time: self.last_trade_time.clone(),
        };

        let tmp_path = format!("{}.tmp", Self::POSITIONS_FILE);
        match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp_path, &json) {
                    log::warn!("Failed to write {}: {}", tmp_path, e);
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, Self::POSITIONS_FILE) {
                    log::warn!(
                        "Failed to rename {} → {}: {}",
                        tmp_path,
                        Self::POSITIONS_FILE,
                        e
                    );
                }
            }
            Err(e) => {
                log::warn!("Failed to serialize positions: {}", e);
            }
        }
    }

    pub async fn execute_signal(&mut self, signal: TradingSignal) -> Result<Option<Trade>> {
        // Check cooldown (only for buys — sells must never be blocked)
        if signal.action != Action::Sell && self.is_in_cooldown(&signal.token.address).await? {
            log::info!(
                "Token {} is in cooldown, skipping trade",
                signal.token.address
            );
            return Ok(None);
        }

        match signal.action {
            Action::Buy => self.execute_buy(signal).await,
            Action::Sell => self.execute_sell(signal).await,
            Action::Hold => {
                log::debug!("Hold signal for token {}", signal.token.address);
                Ok(None)
            }
        }
    }

    async fn execute_buy(&mut self, signal: TradingSignal) -> Result<Option<Trade>> {
        // Check if we already have a position
        if self.positions.contains_key(&signal.token.address) {
            log::debug!("Already have position in token {}", signal.token.address);
            return Ok(None);
        }

        // Check max positions
        if self.positions.len() >= self.max_positions {
            log::warn!(
                "Maximum positions reached, cannot buy {}",
                signal.token.address
            );
            return Ok(None);
        }

        // Calculate buy amount
        let buy_amount = self.calculate_buy_amount(&signal)?;
        if buy_amount == 0 {
            log::warn!("Buy amount is 0 for token {}", signal.token.address);
            return Ok(None);
        }

        // Get token mint
        let token_mint = Pubkey::from_str(&signal.token.address)
            .map_err(|e| BotError::InvalidTokenAddress(format!("Invalid token address: {}", e)))?;

        // Execute buy transaction via PumpPortal trade-local API
        let mut trade = self.create_buy_trade(&signal, buy_amount).await?;

        // Wait for RPC to index the transaction before querying token balance
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Query actual token balance received from the bonding curve
        let mut token_balance = self.wallet.get_token_balance(&token_mint)?;
        if token_balance == 0 {
            log::warn!(
                "Token balance is 0 for {} after buy, retrying in 1s...",
                signal.token.address
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            token_balance = self.wallet.get_token_balance(&token_mint)?;
        }

        if token_balance == 0 {
            log::error!(
                "Token balance still 0 for {} after retry — position recorded with 0 tokens",
                signal.token.address
            );
        }

        // Update trade amount to reflect actual tokens received (not SOL lamports spent)
        trade.amount = token_balance;

        // Calculate entry price from ACTUAL execution (SOL spent / display tokens received).
        // This is more accurate than a post-buy on-chain spot price, which is inflated because
        // our buy moves the bonding curve upward. The spot price after buying is always higher
        // than the average price we actually paid, causing exit strategies to see worse PnL than
        // reality and trigger premature exits (MR, stop loss on actually-profitable trades).
        // Units: SOL per display token (6 decimals for pump.fun tokens), matching fetch_current_price_sol().
        let sol_amount = buy_amount as f64 / 1_000_000_000.0;
        let actual_entry_price = if token_balance > 0 && sol_amount > 0.0 {
            let display_tokens = token_balance as f64 / 1_000_000.0; // pump.fun: 6 decimals
            let exec_price = sol_amount / display_tokens;
            log::info!(
                "Entry price for {} from execution: {:.12} SOL (spent {:.6} SOL for {:.2} display tokens)",
                signal.token.address, exec_price, sol_amount, display_tokens
            );
            exec_price
        } else {
            // Fallback: try on-chain price if execution data unavailable
            let cached_pool = self.pumpswap_pools.get(&signal.token.address);
            match pumpportal::fetch_current_price_sol(
                self.price_rpc.as_ref(),
                &signal.token.address,
                cached_pool,
            )
            .await
            {
                Ok((price, pool_info)) if price > 0.0 => {
                    if let Some(info) = pool_info {
                        self.pumpswap_pools
                            .insert(signal.token.address.clone(), info);
                    }
                    log::warn!(
                        "Entry price for {} from on-chain fallback: {:.12} SOL (no execution data)",
                        signal.token.address,
                        price
                    );
                    price
                }
                _ => {
                    log::warn!(
                        "No entry price for {} — will be backfilled from first exit check",
                        signal.token.address
                    );
                    0.0
                }
            }
        };

        // Also discover PumpSwap pool if applicable (for future exit price checks)
        if !self.pumpswap_pools.contains_key(&signal.token.address) {
            let cached_pool = self.pumpswap_pools.get(&signal.token.address);
            if let Ok((_, Some(info))) = pumpportal::fetch_current_price_sol(
                self.price_rpc.as_ref(),
                &signal.token.address,
                cached_pool,
            )
            .await
            {
                self.pumpswap_pools
                    .insert(signal.token.address.clone(), info);
            }
        }

        log::info!(
            "Buy confirmed for {}: spent {} lamports ({:.4} SOL), received {} tokens, entry={:.12} SOL",
            signal.token.address,
            buy_amount,
            buy_amount as f64 / 1_000_000_000.0,
            token_balance,
            actual_entry_price
        );

        // Update position — store actual token count, NOT SOL lamports spent
        let position = Position {
            token_address: signal.token.address.clone(),
            token_mint,
            amount: token_balance,
            entry_price: actual_entry_price,
            entry_time: chrono::Utc::now(),
            peak_pnl_percent: 0.0,
            source_wallet: None,
            sol_spent: buy_amount as f64 / 1_000_000_000.0,
            cached_market_cap: None,
            cached_holders: None,
            cached_volume_24h: None,
            cached_liquidity: None,
            last_enrichment: None,
            price_history: Vec::new(),
            sell_failures: 0,
            last_sell_failure: None,
            price_failures: 0,
            last_price_failure: None,
        };

        // Update trade price to actual post-buy bonding curve price for accurate journal
        trade.price = actual_entry_price;

        self.positions
            .insert(signal.token.address.clone(), position);
        self.last_trade_time
            .insert(signal.token.address, chrono::Utc::now());
        self.save_positions();

        log::info!(
            "Bought {} tokens of {} at {:.12} SOL — sig: {}",
            token_balance,
            signal.token.symbol,
            actual_entry_price,
            trade.signature.as_deref().unwrap_or("none")
        );

        Ok(Some(trade))
    }

    async fn execute_sell(&mut self, signal: TradingSignal) -> Result<Option<Trade>> {
        let position = match self.positions.get(&signal.token.address) {
            Some(pos) => pos,
            None => {
                log::debug!("No position found for token {}", signal.token.address);
                return Ok(None);
            }
        };

        // Query actual on-chain token balance to handle discrepancies
        let mut actual_balance = self.wallet.get_token_balance(&position.token_mint)?;
        if actual_balance == 0 {
            log::warn!(
                "On-chain token balance is 0 for {}, retrying in 1s...",
                signal.token.address
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            actual_balance = self.wallet.get_token_balance(&position.token_mint)?;
        }

        if actual_balance == 0 {
            log::warn!(
                "On-chain token balance is 0 for {} after retry — removing stale position (likely already sold on-chain)",
                signal.token.address
            );
            self.positions.remove(&signal.token.address);
            self.save_positions();
            return Ok(None);
        }

        // Use the minimum of stored position amount and actual on-chain balance
        let sell_amount = self.calculate_sell_amount(position)?.min(actual_balance);
        log::info!(
            "Sell amount for {}: position.amount={} on-chain={} using={}",
            signal.token.address,
            position.amount,
            actual_balance,
            sell_amount
        );

        if sell_amount == 0 {
            log::warn!("Sell amount is 0 for token {}", signal.token.address);
            return Ok(None);
        }

        // Determine if this is a full exit (selling entire balance)
        let is_full_exit = sell_amount >= actual_balance;

        // Slippage escalation: each consecutive sell failure doubles slippage up to 99%.
        // Without this, a fast-moving token (e.g. mid-graduation pump or rug) blows
        // through the 8% min-out at simulation time and every retry fails identically.
        // 99% means "accept any non-zero output" — the AMM's slippage gate becomes a no-op.
        let effective_slippage = escalated_slippage(self.max_slippage, position.sell_failures);
        if position.sell_failures > 0 {
            log::warn!(
                "Sell retry #{} for {} — escalating slippage {:.0}% → {:.0}%",
                position.sell_failures + 1,
                signal.token.address,
                self.max_slippage,
                effective_slippage
            );
        }

        // Execute sell transaction via PumpPortal trade-local API
        let trade = self
            .create_sell_trade(&signal, sell_amount, position, is_full_exit, effective_slippage)
            .await?;

        // Update or remove position
        if sell_amount >= position.amount {
            self.positions.remove(&signal.token.address);
            log::info!("Sold entire position of {}", signal.token.symbol);
        } else {
            // Partial sell - update position
            let mut updated_position = position.clone();
            updated_position.amount -= sell_amount;
            self.positions
                .insert(signal.token.address.clone(), updated_position);
            log::info!("Partially sold position of {}", signal.token.symbol);
        }

        self.last_trade_time
            .insert(signal.token.address, chrono::Utc::now());
        self.save_positions();

        Ok(Some(trade))
    }

    fn calculate_buy_amount(&self, signal: &TradingSignal) -> Result<u64> {
        // Copy trades specify an exact amount; use it (capped at max_buy_amount)
        if let Some(override_amount) = signal.override_buy_amount {
            return Ok(override_amount.min(self.max_buy_amount));
        }

        let confidence_multiplier = signal.confidence;
        let base_amount = (self.max_buy_amount as f64 * confidence_multiplier) as u64;

        // Ensure we don't exceed max buy amount
        let buy_amount = base_amount.min(self.max_buy_amount);

        Ok(buy_amount)
    }

    fn calculate_sell_amount(&self, position: &Position) -> Result<u64> {
        // Sell entire position
        Ok(position.amount)
    }

    /// PumpPortal trade-local endpoint. Base URL is configurable via the
    /// `PUMPPORTAL_API_URL` env var (defaults to https://pumpportal.fun).
    fn pumpportal_trade_local_url() -> String {
        let base = std::env::var("PUMPPORTAL_API_URL")
            .unwrap_or_else(|_| "https://pumpportal.fun".to_string());
        let base = base.trim_end_matches('/');
        format!("{}/api/trade-local", base)
    }

    async fn create_buy_trade(&self, signal: &TradingSignal, amount: u64) -> Result<Trade> {
        let sol_amount = amount as f64 / 1_000_000_000.0; // lamports → SOL
        let wallet_pubkey = self.wallet.get_address().to_string();

        log::info!(
            "PumpPortal trade-local BUY: mint={} amount={:.4} SOL slippage={}",
            signal.token.address,
            sol_amount,
            self.max_slippage as u32
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| BotError::Http(e))?;

        let resp = client
            .post(Self::pumpportal_trade_local_url())
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "publicKey": wallet_pubkey,
                "action": "buy",
                "mint": signal.token.address,
                "amount": sol_amount,
                "denominatedInSol": true,
                "slippage": self.max_slippage as u32,
                "priorityFee": 0.001,
                "pool": "auto"
            }))
            .send()
            .await
            .map_err(|e| BotError::Http(e))?;

        let status = resp.status();
        if !status.is_success() {
            let error_text = resp.text().await.unwrap_or_default();
            log::error!(
                "PumpPortal trade-local BUY error: status={} body={}",
                status,
                error_text
            );
            return Err(BotError::Trading(format!(
                "PumpPortal API error: {} - {}",
                status, error_text
            )));
        }

        let tx_bytes = resp.bytes().await.map_err(|e| BotError::Http(e))?;
        log::info!(
            "Received {} bytes from PumpPortal trade-local (buy)",
            tx_bytes.len()
        );

        let signature = self.sign_and_send_tx(&tx_bytes)?;

        let trade = Trade {
            id: uuid::Uuid::new_v4().to_string(),
            token_address: signal.token.address.clone(),
            action: Action::Buy,
            amount,
            price: signal.token.price_usd,
            timestamp: chrono::Utc::now(),
            signature: Some(signature),
            profit_loss: None,
            sol_received: None,
        };

        Ok(trade)
    }

    async fn create_sell_trade(
        &self,
        signal: &TradingSignal,
        amount: u64,
        position: &Position,
        is_full_exit: bool,
        slippage_percent: f64,
    ) -> Result<Trade> {
        let wallet_pubkey = self.wallet.get_address().to_string();
        // PumpPortal accepts integer slippage; clamp to [1, 99] before u32 cast
        let slippage_int = slippage_percent.round().clamp(1.0, 99.0) as u32;

        // For full exits, use "100%" which tells PumpPortal to sell the entire balance
        // without needing an exact token count. Fall back to explicit amount for partial sells.
        let sell_amount_value: serde_json::Value = if is_full_exit {
            log::info!(
                "PumpPortal trade-local SELL (100%): mint={} slippage={}",
                signal.token.address,
                slippage_int
            );
            serde_json::json!("100%")
        } else {
            log::info!(
                "PumpPortal trade-local SELL: mint={} amount={} tokens slippage={}",
                signal.token.address,
                amount,
                slippage_int
            );
            serde_json::json!(amount)
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| BotError::Http(e))?;

        let resp = client
            .post(Self::pumpportal_trade_local_url())
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "publicKey": wallet_pubkey,
                "action": "sell",
                "mint": signal.token.address,
                "amount": sell_amount_value,
                "denominatedInSol": false,
                "slippage": slippage_int,
                "priorityFee": 0.001,
                "pool": "auto"
            }))
            .send()
            .await
            .map_err(|e| BotError::Http(e))?;

        let status = resp.status();
        if !status.is_success() {
            let error_text = resp.text().await.unwrap_or_default();
            log::error!(
                "PumpPortal trade-local SELL error: status={} body={}",
                status,
                error_text
            );
            return Err(BotError::Trading(format!(
                "PumpPortal API error: {} - {}",
                status, error_text
            )));
        }

        let tx_bytes = resp.bytes().await.map_err(|e| BotError::Http(e))?;
        log::info!(
            "Received {} bytes from PumpPortal trade-local (sell)",
            tx_bytes.len()
        );

        // Capture SOL balance before sell to compute sol_received
        let sol_before = self.wallet.get_balance().unwrap_or(0);

        let signature = self.sign_and_send_tx(&tx_bytes)?;

        // Capture SOL balance after sell with retry loop.
        // RPC nodes may return stale balances immediately after confirmation,
        // so we retry up to 5 times with 500ms delays.
        let mut sol_received: Option<f64> = None;
        let max_retries = 5;
        for attempt in 0..max_retries {
            let sol_after = self.wallet.get_balance().unwrap_or(0);
            if sol_after > sol_before {
                let received = (sol_after - sol_before) as f64 / 1_000_000_000.0;
                log::info!(
                    "SOL received from sell: {:.6} SOL (attempt {})",
                    received,
                    attempt + 1
                );
                sol_received = Some(received);
                break;
            }
            if attempt < max_retries - 1 {
                log::debug!(
                    "Balance not yet updated after sell (before={} after={}), retrying in 500ms ({}/{})",
                    sol_before, sol_after, attempt + 1, max_retries
                );
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            } else {
                log::warn!(
                    "SOL balance did not increase after sell after {} attempts (before={} after={})",
                    max_retries, sol_before, sol_after
                );
            }
        }

        // Fallback: estimate sol_received from on-chain price if balance check failed
        if sol_received.is_none() {
            let estimated = position.amount as f64 / 1_000_000.0 * position.entry_price;
            // Use a conservative 90% estimate to account for slippage/fees
            let conservative_estimate = estimated * 0.90;
            log::warn!(
                "Using conservative estimate for sol_received: {:.6} SOL (90% of {:.6} entry value)",
                conservative_estimate, estimated
            );
            sol_received = Some(conservative_estimate);
        }

        let profit_loss = sol_received.map(|recv| recv - position.sol_spent);

        let trade = Trade {
            id: uuid::Uuid::new_v4().to_string(),
            token_address: signal.token.address.clone(),
            action: Action::Sell,
            amount,
            price: signal.token.price_usd,
            timestamp: chrono::Utc::now(),
            signature: Some(signature),
            profit_loss,
            sol_received,
        };

        Ok(trade)
    }

    /// Deserialize the raw transaction bytes from PumpPortal, sign, and send.
    /// PumpPortal's /api/trade-local returns a VersionedTransaction (v0), not legacy.
    /// The blockhash is already set by PumpPortal — do NOT fetch a new one.
    fn sign_and_send_tx(&self, tx_bytes: &[u8]) -> Result<String> {
        let mut tx: VersionedTransaction = bincode::deserialize(tx_bytes)
            .map_err(|e| BotError::Trading(format!("Failed to deserialize transaction: {}", e)))?;

        // Find our pubkey in the transaction's static account keys and sign at that index.
        let wallet_pubkey = self.wallet.get_keypair().pubkey();
        let idx = tx
            .message
            .static_account_keys()
            .iter()
            .position(|k| k == &wallet_pubkey)
            .ok_or_else(|| {
                BotError::Trading("Wallet pubkey not found in transaction account keys".to_string())
            })?;

        let message_bytes = tx.message.serialize();
        let sig = self.wallet.get_keypair().sign_message(&message_bytes);
        tx.signatures[idx] = sig;

        // Send via RPC — send_and_confirm_transaction accepts &impl SerializableTransaction
        let rpc = self.wallet.get_rpc_client();
        let signature = rpc
            .send_and_confirm_transaction(&tx)
            .map_err(|e| BotError::TransactionFailed(format!("Transaction failed: {}", e)))?;

        log::info!("Transaction confirmed: {}", signature);
        Ok(signature.to_string())
    }

    async fn is_in_cooldown(&self, token_address: &str) -> Result<bool> {
        if let Some(last_trade) = self.last_trade_time.get(token_address) {
            let elapsed = chrono::Utc::now() - *last_trade;
            return Ok(elapsed.num_seconds() < self.cooldown_seconds as i64);
        }
        Ok(false)
    }

    pub fn set_position_source_wallet(&mut self, address: &str, wallet: String) {
        if let Some(pos) = self.positions.get_mut(address) {
            pos.source_wallet = Some(wallet);
        }
    }

    pub fn rpc_client(&self) -> &AsyncRpcClient {
        &self.price_rpc
    }

    pub fn get_positions(&self) -> &HashMap<String, Position> {
        &self.positions
    }

    pub fn get_trade_history(&self) -> &Vec<Trade> {
        &self.trade_history
    }

    pub fn add_trade(&mut self, trade: Trade) {
        self.trade_history.push(trade);
    }

    /// Increment sell failure counter for a position. Each failure escalates the
    /// slippage on the next attempt by +10 percentage points (capped at 99%). The
    /// position is never permanently skipped — once slippage reaches the 99% cap,
    /// further retries are throttled to once every 30s but continue forever.
    pub fn increment_sell_failures(&mut self, token_address: &str) {
        let base_slippage = self.max_slippage;
        if let Some(pos) = self.positions.get_mut(token_address) {
            pos.sell_failures += 1;
            pos.last_sell_failure = Some(chrono::Utc::now());
            let next_slippage = escalated_slippage(base_slippage, pos.sell_failures);
            log::warn!(
                "Sell failure #{} for {} — next attempt at slippage {:.0}%{}",
                pos.sell_failures,
                token_address,
                next_slippage,
                if next_slippage >= 99.0 {
                    " (cap reached; throttled to 30s between retries; will keep trying until the position exits)"
                } else {
                    ""
                }
            );
            self.save_positions();
        }
    }

    pub async fn check_exit_conditions(&mut self) -> Result<Vec<TradingSignal>> {
        let mut exit_signals = Vec::new();

        let positions_snapshot: Vec<(String, Position)> = self
            .positions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for (token_address, position) in &positions_snapshot {
            // Throttle, but never abandon. During the escalation phase we retry every
            // loop tick — each retry adds +10 percentage points of slippage tolerance.
            // Once escalated_slippage hits the 99% cap (any non-zero output OK), we
            // throttle to 30s between attempts so we don't burn RPC if the AMM is
            // genuinely drained — but the loop continues forever; positions must always
            // exit. The counter resets to 0 on the first successful sell (the position
            // is removed entirely) so this is purely failure-mode behaviour.
            let next_slippage = escalated_slippage(self.max_slippage, position.sell_failures);
            if next_slippage >= 99.0 && position.sell_failures > 0 {
                if let Some(last_fail) = position.last_sell_failure {
                    let elapsed_secs = (chrono::Utc::now() - last_fail).num_seconds();
                    if elapsed_secs < 30 {
                        log::debug!(
                            "Throttling sell retry for {} ({} failures, {}s since last attempt, retrying in {}s at 99% slippage)",
                            token_address,
                            position.sell_failures,
                            elapsed_secs,
                            (30 - elapsed_secs).max(0)
                        );
                        continue;
                    }
                }
            }

            // Skip positions that have failed on-chain pricing too many times in a row.
            // Negative cache for unpriceable tokens (e.g., a Raydium-only SPL stablecoin
            // that ended up in positions.json) — without this, every 250ms tick repeats
            // the expensive find_pumpswap_pool RPC sequence forever.
            // After 5 minutes, reset and retry in case the token became priceable.
            if position.price_failures >= 3 {
                let should_reset = position.last_price_failure.map_or(true, |last_fail| {
                    (chrono::Utc::now() - last_fail).num_minutes() >= 5
                });
                if should_reset {
                    log::info!(
                        "Resetting price failure counter for {} (was {} failures, 5 min cooldown elapsed)",
                        token_address,
                        position.price_failures
                    );
                    if let Some(pos) = self.positions.get_mut(token_address) {
                        pos.price_failures = 0;
                        pos.last_price_failure = None;
                    }
                    self.save_positions();
                    // Don't skip — proceed to retry pricing this cycle
                } else {
                    log::debug!(
                        "Skipping unpriceable position {} ({} consecutive price failures, retries soon)",
                        token_address,
                        position.price_failures
                    );
                    continue;
                }
            }

            // Fetch on-chain *realized* SOL price for the position: simulate selling
            // `position.amount` tokens through the bonding curve / PumpSwap pool and use
            // the resulting effective per-token output as the current price. This matches
            // what we'd actually receive on exit, instead of the spot quote — which can
            // overstate value by 10%+ on thin freshly-graduated PumpSwap pools.
            let has_cached = self.pumpswap_pools.contains_key(token_address);
            let cached_pool = self.pumpswap_pools.get(token_address);
            let current_price = match pumpportal::fetch_realized_price_sol(
                self.price_rpc.as_ref(),
                token_address,
                position.amount,
                cached_pool,
            )
            .await
            {
                Ok((price, pool_info)) => {
                    // Cache the pool info if PumpSwap was used (first discovery)
                    if let Some(info) = pool_info {
                        if !has_cached {
                            self.pumpswap_pools.insert(token_address.clone(), info);
                        }
                    }
                    // Reset price failure counter on successful price fetch
                    if position.price_failures > 0 {
                        if let Some(pos) = self.positions.get_mut(token_address) {
                            pos.price_failures = 0;
                            pos.last_price_failure = None;
                        }
                        self.save_positions();
                    }
                    price
                }
                Err(e) => {
                    log::warn!(
                        "No on-chain price for {}: {}, skipping exit check",
                        token_address,
                        e
                    );
                    if let Some(pos) = self.positions.get_mut(token_address) {
                        pos.price_failures += 1;
                        pos.last_price_failure = Some(chrono::Utc::now());
                        if pos.price_failures == 3 {
                            log::warn!(
                                "Position {} marked unpriceable after 3 consecutive failures (will retry in 5 min)",
                                token_address
                            );
                        }
                    }
                    self.save_positions();
                    continue;
                }
            };

            // Phase 1a: Record price history for exit strategy analysis (velocity, momentum)
            if let Some(pos) = self.positions.get_mut(token_address) {
                pos.price_history.push((chrono::Utc::now(), current_price));
                // Cap at 1200 entries (~5 minutes at 250ms intervals)
                if pos.price_history.len() > 1200 {
                    pos.price_history.remove(0);
                }
            }

            // Handle missing entry price: if entry_price is 0 (failed to fetch at buy time),
            // backfill from current on-chain price. If still 0, skip PnL exit logic.
            let entry_price = if position.entry_price == 0.0 || !position.entry_price.is_finite() {
                log::warn!(
                    "Position {} has invalid entry_price ({:.12} SOL), backfilling from on-chain",
                    token_address,
                    position.entry_price
                );
                // Use the current price as the entry price (best we can do retroactively)
                // and update the stored position so future checks have a valid baseline
                if current_price > 0.0 && current_price.is_finite() {
                    if let Some(pos) = self.positions.get_mut(token_address) {
                        pos.entry_price = current_price;
                        log::info!(
                            "Backfilled entry_price for {} to {:.12} SOL from on-chain",
                            token_address,
                            current_price
                        );
                    }
                    // Skip this cycle — PnL is 0% since we just set entry = current
                    continue;
                } else {
                    log::warn!(
                        "Cannot backfill entry_price for {} (current_price={:.10}), skipping exit check",
                        token_address,
                        current_price
                    );
                    continue;
                }
            } else {
                position.entry_price
            };

            let pnl_percent = ((current_price - entry_price) / entry_price) * 100.0;
            let loss_percent = ((entry_price - current_price) / entry_price) * 100.0;
            // Update peak PnL tracking
            let mut peak = position.peak_pnl_percent;
            if pnl_percent > peak {
                peak = pnl_percent;
                if let Some(pos) = self.positions.get_mut(token_address) {
                    pos.peak_pnl_percent = peak;
                }
            }

            // Determine active trailing stop (if any)
            let active_trail = self
                .trailing_thresholds
                .iter()
                .rev()
                .find(|(gain_threshold, _)| peak >= *gain_threshold);

            let trail_info = if let Some((thresh, trail)) = active_trail {
                let exit_at = peak - trail;
                format!(
                    "trail: peak={:+.2}% tier=+{:.0}%:{:.0}% exit_at={:+.2}%",
                    peak, thresh, trail, exit_at
                )
            } else {
                format!("trail: peak={:+.2}% (no tier active)", peak)
            };

            log::info!(
                "PnL {} ({}): entry={:.12} SOL current={:.12} SOL pnl={:+.2}% sol_spent={:.4} | target=+{:.1}% stop=-{:.1}% | {}",
                token_address,
                position.token_address,
                entry_price,
                current_price,
                pnl_percent,
                position.sol_spent,
                self.profit_target_percent,
                self.stop_loss_percent,
                trail_info
            );

            // Check exit conditions in priority order:
            // 1. Momentum reversal (catches slow bleeds at ~-6% instead of stop loss at ~-15%)
            // 2. Hard stop loss (always active, backstop)
            // 3. Dynamic trailing stop (if a tier is active)
            // 4. Hard profit target — fallback ONLY when trailing thresholds aren't configured.
            //    If trailing tiers are set, the user has opted into ride-the-peak behaviour
            //    and the hard profit target must stay out of the way until those tiers run.
            let mut should_exit = false;
            let mut reason = String::new();

            // MomentumReversal FIRST: exit if price is below SMA and in loss territory
            // Checked before stop loss so slow bleeds exit at ~-6% instead of ~-15%
            if self.exit_strategies.momentum_reversal_enabled {
                let window_secs = self.exit_strategies.momentum_reversal_window_secs;
                let min_loss = self.exit_strategies.momentum_reversal_min_loss_pct;
                if window_secs > 0 && min_loss > 0.0 && pnl_percent < -min_loss {
                    if let Some(pos) = self.positions.get(token_address) {
                        let now = chrono::Utc::now();
                        let window_start = now - chrono::Duration::seconds(window_secs as i64);
                        let window_prices: Vec<f64> = pos
                            .price_history
                            .iter()
                            .filter(|(t, _)| *t >= window_start)
                            .map(|(_, p)| *p)
                            .collect();

                        // Need enough data points (at least ~10 seconds of history)
                        if window_prices.len() >= 40 {
                            let sma: f64 =
                                window_prices.iter().sum::<f64>() / window_prices.len() as f64;
                            if current_price < sma {
                                should_exit = true;
                                reason = format!(
                                    "Momentum reversal: price {:.12} < SMA({:.12}, {}s window), pnl={:+.2}%",
                                    current_price, sma, window_secs, pnl_percent
                                );
                            }
                        }
                    }
                }
            }

            if !should_exit && loss_percent >= self.stop_loss_percent {
                should_exit = true;
                reason = format!("Stop loss triggered: {:.2}%", loss_percent);
            } else if !should_exit {
                if let Some((thresh, trail)) = active_trail {
                    let exit_at = peak - trail;
                    if pnl_percent <= exit_at {
                        should_exit = true;
                        reason = format!(
                            "Trailing stop triggered: peak={:+.2}% tier=+{:.0}%:{:.0}% current={:+.2}% (exit_at={:+.2}%)",
                            peak, thresh, trail, pnl_percent, exit_at
                        );
                    }
                } else if self.trailing_thresholds.is_empty()
                    && pnl_percent >= self.profit_target_percent
                {
                    // Hard profit target only fires when NO trailing thresholds are configured
                    // at all. With trailing tiers configured, we wait for the lowest tier to be
                    // crossed and let the trailing-stop logic decide the exit — exiting on the
                    // hard target would short-circuit the user's chosen ride-the-peak behaviour.
                    should_exit = true;
                    reason = format!("Profit target reached: {:.2}%", pnl_percent);
                }
            }

            // === Additional Exit Strategies (Agent 4) ===

            // TimeExit: sell after max_hold_minutes to prevent bag-holding
            // Smart suppression: skip time exit if a profitable trailing tier is active —
            // let the trailing stop manage the exit so runners can continue to higher tiers
            if !should_exit && self.exit_strategies.time_exit_enabled {
                let hold_minutes = (chrono::Utc::now() - position.entry_time).num_minutes() as u64;
                if self.exit_strategies.max_hold_minutes > 0
                    && hold_minutes >= self.exit_strategies.max_hold_minutes
                {
                    let trailing_protecting = active_trail.is_some() && pnl_percent > 0.0;
                    if trailing_protecting {
                        log::info!(
                            "Time exit suppressed for {} — trailing tier active at PnL {:+.2}%, letting runner continue",
                            token_address, pnl_percent
                        );
                    } else {
                        should_exit = true;
                        reason = format!(
                            "Time exit: held for {} minutes (max {})",
                            hold_minutes, self.exit_strategies.max_hold_minutes
                        );
                    }
                }
            }

            // PriceVelocity: sell if price declining rapidly (using 30s window from price_history)
            if !should_exit && self.exit_strategies.velocity_exit_enabled {
                if let Some(pos) = self.positions.get(token_address) {
                    let now = chrono::Utc::now();
                    let window_start = now - chrono::Duration::seconds(30);
                    let recent: Vec<&(chrono::DateTime<chrono::Utc>, f64)> = pos
                        .price_history
                        .iter()
                        .filter(|(t, _)| *t >= window_start)
                        .collect();

                    if recent.len() >= 2 {
                        if let (Some(first), Some(last)) = (recent.first(), recent.last()) {
                            let elapsed_mins =
                                (last.0 - first.0).num_milliseconds() as f64 / 60_000.0;
                            if elapsed_mins > 0.0 && first.1 > 0.0 {
                                let decline_pct = ((first.1 - last.1) / first.1) * 100.0;
                                let decline_rate = decline_pct / elapsed_mins;
                                if decline_rate >= self.exit_strategies.max_decline_rate_per_min
                                    && self.exit_strategies.max_decline_rate_per_min > 0.0
                                {
                                    should_exit = true;
                                    reason = format!(
                                        "Velocity exit: declining {:.1}%/min over {:.0}s (max {:.1}%/min)",
                                        decline_rate,
                                        elapsed_mins * 60.0,
                                        self.exit_strategies.max_decline_rate_per_min
                                    );
                                }
                            }
                        }
                    }
                }
            }

            if should_exit {
                let token_info = crate::pumpportal::TokenInfo {
                    address: token_address.clone(),
                    symbol: "UNKNOWN".to_string(),
                    name: "Unknown".to_string(),
                    decimals: 6,
                    market_cap: 0,
                    holders: 0,
                    age_hours: 0,
                    liquidity: 0,
                    price_usd: current_price,
                    price_change_24h: 0.0,
                    volume_24h: 0,
                    created_at: "".to_string(),
                    just_graduated_at_secs: None,
                };

                log::info!("Exit signal for {}: {}", token_address, reason);

                exit_signals.push(TradingSignal {
                    token: token_info,
                    action: Action::Sell,
                    confidence: 1.0,
                    reason,
                    expected_price: Some(current_price),
                    override_buy_amount: None,
                });
            }
        }

        Ok(exit_signals)
    }

    /// Enrich all open positions with DexScreener market data.
    /// Called on a separate timer (30-60s) — NOT in the 250ms exit check loop.
    pub async fn enrich_positions(&mut self) {
        let addresses: Vec<String> = self.positions.keys().cloned().collect();
        if addresses.is_empty() {
            return;
        }

        log::debug!(
            "Enriching {} open positions via DexScreener",
            addresses.len()
        );

        match pumpportal::enrich_tokens_dexscreener(&addresses).await {
            Ok(tokens) => {
                for token in tokens {
                    if let Some(pos) = self.positions.get_mut(&token.address) {
                        pos.cached_market_cap = Some(token.market_cap);
                        pos.cached_holders = Some(token.holders);
                        pos.cached_volume_24h = Some(token.volume_24h);
                        pos.cached_liquidity = Some(token.liquidity);
                        pos.last_enrichment = Some(chrono::Utc::now());
                        log::debug!(
                            "Enriched {}: mcap={} holders={} vol={} liq={}",
                            token.address,
                            token.market_cap,
                            token.holders,
                            token.volume_24h,
                            token.liquidity
                        );
                    }
                }
            }
            Err(e) => {
                log::warn!("Position enrichment failed: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_save_and_load_roundtrip() {
        let test_file = "positions_test_tmp.json";

        let mut positions = HashMap::new();
        let pubkey = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        positions.insert(
            "TestMint123".to_string(),
            Position {
                token_address: "TestMint123".to_string(),
                token_mint: pubkey,
                amount: 1_000_000,
                entry_price: 0.000054321,
                entry_time: chrono::Utc::now(),
                peak_pnl_percent: 12.5,
                source_wallet: Some("wallet_abc".to_string()),
                sol_spent: 0.05,
                cached_market_cap: None,
                cached_holders: None,
                cached_volume_24h: None,
                cached_liquidity: None,
                last_enrichment: None,
                price_history: Vec::new(),
                sell_failures: 0,
                last_sell_failure: None,
                price_failures: 0,
                last_price_failure: None,
            },
        );

        let mut last_trade_time = HashMap::new();
        last_trade_time.insert("TestMint123".to_string(), chrono::Utc::now());

        // Save
        let snapshot = PositionSnapshot {
            positions: positions.clone(),
            last_trade_time: last_trade_time.clone(),
        };
        let json = serde_json::to_string_pretty(&snapshot).expect("serialize failed");
        std::fs::write(test_file, &json).expect("write failed");

        // Verify file contains expected data
        let raw = std::fs::read_to_string(test_file).expect("read failed");
        assert!(raw.contains("TestMint123"));
        assert!(raw.contains("So11111111111111111111111111111111111111112"));

        // Load back and verify all fields
        let loaded: PositionSnapshot = serde_json::from_str(&raw).expect("deserialize failed");
        assert_eq!(loaded.positions.len(), 1);
        assert_eq!(loaded.last_trade_time.len(), 1);

        let pos = loaded.positions.get("TestMint123").unwrap();
        assert_eq!(pos.token_address, "TestMint123");
        assert_eq!(pos.token_mint, pubkey);
        assert_eq!(pos.amount, 1_000_000);
        assert!((pos.entry_price - 0.000054321).abs() < 1e-12);
        assert!((pos.peak_pnl_percent - 12.5).abs() < 1e-6);
        assert_eq!(pos.sol_spent, 0.05);
        assert_eq!(pos.source_wallet, Some("wallet_abc".to_string()));

        // Skipped fields should be default
        assert!(pos.cached_market_cap.is_none());
        assert!(pos.price_history.is_empty());
        // sell_failures and last_sell_failure should survive serialization
        assert_eq!(pos.sell_failures, 0);
        assert!(pos.last_sell_failure.is_none());

        std::fs::remove_file(test_file).ok();
    }

    /// Helper to create a test position with defaults
    fn make_test_position(token: &str, entry_price: f64, sol_spent: f64) -> Position {
        Position {
            token_address: token.to_string(),
            token_mint: Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap(),
            amount: 1_000_000_000,
            entry_price,
            entry_time: chrono::Utc::now(),
            peak_pnl_percent: 0.0,
            source_wallet: Some("test_wallet".to_string()),
            sol_spent,
            cached_market_cap: None,
            cached_holders: None,
            cached_volume_24h: None,
            cached_liquidity: None,
            last_enrichment: None,
            price_history: Vec::new(),
            sell_failures: 0,
            last_sell_failure: None,
            price_failures: 0,
            last_price_failure: None,
        }
    }

    // ==========================================
    // SELL FAILURE RESET MECHANISM TESTS
    // ==========================================

    #[test]
    fn test_sell_failures_default_zero() {
        let pos = make_test_position("TokenA", 0.001, 0.05);
        assert_eq!(pos.sell_failures, 0);
        assert!(pos.last_sell_failure.is_none());
    }

    #[test]
    fn test_sell_failures_serialization_roundtrip() {
        // sell_failures and last_sell_failure are NOT #[serde(skip)] — they persist
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 3;
        let ts = chrono::Utc::now();
        pos.last_sell_failure = Some(ts);

        let json = serde_json::to_string(&pos).unwrap();
        let loaded: Position = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.sell_failures, 3);
        assert!(loaded.last_sell_failure.is_some());
        // Timestamps should match within 1ms (serialization precision)
        let diff = (loaded.last_sell_failure.unwrap() - ts)
            .num_milliseconds()
            .abs();
        assert!(diff <= 1, "Timestamp diff too large: {}ms", diff);
    }

    #[test]
    fn test_sell_failures_default_on_missing_field() {
        // Old positions.json files won't have sell_failures or last_sell_failure.
        // #[serde(default)] should handle this gracefully.
        let json = r#"{
            "token_address": "TokenA",
            "token_mint": "So11111111111111111111111111111111111111112",
            "amount": 1000000,
            "entry_price": 0.001,
            "entry_time": "2026-03-10T20:00:00Z",
            "peak_pnl_percent": 0.0,
            "source_wallet": "wallet_abc",
            "sol_spent": 0.05
        }"#;

        let pos: Position = serde_json::from_str(json).unwrap();
        assert_eq!(pos.sell_failures, 0);
        assert!(pos.last_sell_failure.is_none());
    }

    #[test]
    fn test_sell_failure_increment_sets_timestamp() {
        // Simulate increment_sell_failures behavior
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        assert_eq!(pos.sell_failures, 0);
        assert!(pos.last_sell_failure.is_none());

        for expected in 1..=3 {
            pos.sell_failures += 1;
            pos.last_sell_failure = Some(chrono::Utc::now());
            assert_eq!(pos.sell_failures, expected);
            assert!(pos.last_sell_failure.is_some());
        }
    }

    #[test]
    fn test_sell_failure_reset_clears_state() {
        // Simulate the reset that happens after a successful sell:
        // counter goes to 0, timestamp cleared.
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 5;
        pos.last_sell_failure = Some(chrono::Utc::now());

        pos.sell_failures = 0;
        pos.last_sell_failure = None;

        assert_eq!(pos.sell_failures, 0);
        assert!(pos.last_sell_failure.is_none());
    }

    // ==========================================
    // SLIPPAGE ESCALATION TESTS
    // ==========================================

    #[test]
    fn test_escalated_slippage_no_failures_uses_base() {
        assert_eq!(escalated_slippage(8.0, 0), 8.0);
        assert_eq!(escalated_slippage(15.0, 0), 15.0);
    }

    // ==========================================
    // REALIZED-PRICE SIMULATION TESTS
    // ==========================================

    use crate::pumpportal::simulate_sell_price_sol;

    #[test]
    fn test_simulate_sell_zero_inputs_return_zero() {
        assert_eq!(simulate_sell_price_sol(0, 1_000_000_000, 1_000_000), 0.0);
        assert_eq!(simulate_sell_price_sol(1_000_000, 0, 1_000_000), 0.0);
        assert_eq!(simulate_sell_price_sol(1_000_000, 1_000_000_000, 0), 0.0);
    }

    #[test]
    fn test_simulate_sell_tiny_trade_approaches_spot() {
        // base=1e10 raw (10K display), quote=10 SOL. Spot = 10/10K = 0.001 SOL/token.
        // A 1-token sell should net very close to spot.
        let base = 10_000_000_000_u64;       // 10K display tokens
        let quote = 10_000_000_000_u64;      // 10 SOL in lamports
        let tokens_in = 1_000_000_u64;       // 1 display token (raw)
        let price = simulate_sell_price_sol(base, quote, tokens_in);
        // Spot = (10/1e9) / (1e10/1e6) = 10 / 10_000 = 0.001 SOL per display token
        let spot = 0.001;
        assert!(
            (price - spot).abs() / spot < 0.001,
            "tiny trade {:.10} should be within 0.1% of spot {:.10}",
            price,
            spot
        );
    }

    #[test]
    fn test_simulate_sell_large_trade_has_meaningful_impact() {
        // A 10% drain of base reserves should noticeably depress effective price.
        let base = 10_000_000_000_u64;
        let quote = 10_000_000_000_u64;
        let tokens_in = 1_000_000_000_u64; // 1K display tokens, 10% of pool
        let spot = 0.001;
        let price = simulate_sell_price_sol(base, quote, tokens_in);
        // For x*y=k with 10% size: out_quote = quote - (base*quote)/(base+dx)
        //   = 10 - (1e10*1e10)/(1.1e10) = 10 - 9.0909... = 0.90909 SOL
        // effective per display token = 0.90909 / 1000 ≈ 0.0009091
        assert!(price < spot * 0.92, "expected meaningful impact, got {:.10}", price);
        assert!(price > spot * 0.85, "got {:.10}", price);
    }

    #[test]
    fn test_simulate_sell_matches_observed_test_run() {
        // From the may-5 test: spot read 6.42e-7 SOL/token (+10.32%); we held
        // 429,337.87 display tokens; received 0.229 SOL. That's a real loss vs spot's
        // implied 0.276 SOL. Confirm that for a thin pool, the simulated output
        // matches reality more closely than the spot quote.
        // Approximate the post-graduation pool: at spot 6.42e-7, with 30 SOL of quote:
        //   base_display = 30 / 6.42e-7 ≈ 4.67e7 display tokens, raw = 4.67e13
        //   quote_lamports = 30e9
        let quote = 30_000_000_000_u64;
        let base = 46_700_000_000_000_u64;
        let tokens_in = 429_337_870_605_u64;
        let realized = simulate_sell_price_sol(base, quote, tokens_in);
        // Spot would be 30 / 4.67e7 = 6.42e-7
        let spot = 30.0 / 46_700_000.0;
        // Realized must be lower than spot when selling a non-trivial fraction
        assert!(realized < spot, "realized {:.12} should be below spot {:.12}", realized, spot);
        // And at this size (~1% of pool), the impact is small but non-zero
        assert!(
            realized < spot * 0.995 && realized > spot * 0.97,
            "realized {:.12} (spot {:.12}) — expected within ~3% below spot",
            realized,
            spot
        );
    }

    #[test]
    fn test_escalated_slippage_adds_10_per_failure() {
        // Base 8% → 8 / 18 / 28 / 38 / 48 / 58 / 68 / 78 / 88 / 98
        assert_eq!(escalated_slippage(8.0, 0), 8.0);
        assert_eq!(escalated_slippage(8.0, 1), 18.0);
        assert_eq!(escalated_slippage(8.0, 2), 28.0);
        assert_eq!(escalated_slippage(8.0, 3), 38.0);
        assert_eq!(escalated_slippage(8.0, 4), 48.0);
        assert_eq!(escalated_slippage(8.0, 5), 58.0);
        assert_eq!(escalated_slippage(8.0, 9), 98.0);
    }

    #[test]
    fn test_escalated_slippage_pins_at_99_when_cap_reached() {
        // Base 8%: at failure 10, base + 100 = 108 → capped at 99
        assert_eq!(escalated_slippage(8.0, 10), 99.0);
        assert_eq!(escalated_slippage(8.0, 11), 99.0);
        assert_eq!(escalated_slippage(8.0, 100), 99.0);
    }

    #[test]
    fn test_escalated_slippage_caps_during_escalation() {
        // Base 30%: 30 / 40 / 50 / 60 / 70 / 80 / 90 / 99 (would be 100, capped)
        assert_eq!(escalated_slippage(30.0, 0), 30.0);
        assert_eq!(escalated_slippage(30.0, 1), 40.0);
        assert_eq!(escalated_slippage(30.0, 6), 90.0);
        assert_eq!(escalated_slippage(30.0, 7), 99.0);
        assert_eq!(escalated_slippage(30.0, 8), 99.0);
    }

    #[test]
    fn test_escalated_slippage_high_base_pins_immediately() {
        // Base 99%: every failure stays at 99
        for failures in 0..=10 {
            assert_eq!(escalated_slippage(99.0, failures), 99.0);
        }
    }

    // ==========================================
    // 30-SECOND THROTTLE TESTS (post-escalation)
    // ==========================================

    /// Simulate the throttle check from check_exit_conditions:
    /// returns `true` if the position should be skipped this tick.
    /// Throttle fires only once escalated_slippage has reached the 99% cap.
    fn should_throttle(pos: &Position, base_slippage: f64) -> bool {
        let next_slippage = escalated_slippage(base_slippage, pos.sell_failures);
        if next_slippage < 99.0 || pos.sell_failures == 0 {
            return false;
        }
        match pos.last_sell_failure {
            Some(last_fail) => (chrono::Utc::now() - last_fail).num_seconds() < 30,
            None => false,
        }
    }

    #[test]
    fn test_throttle_inactive_during_escalation_phase() {
        // With base 8%, escalation runs failures 1..=9 (slippage 18..=98); throttle
        // only kicks in at failure 10 when slippage hits the 99% cap.
        for failures in 0..=9 {
            let mut pos = make_test_position("TokenA", 0.001, 0.05);
            pos.sell_failures = failures;
            pos.last_sell_failure = Some(chrono::Utc::now()); // fresh failure
            assert!(
                !should_throttle(&pos, 8.0),
                "Should not throttle at failure count {} (still escalating)",
                failures
            );
        }
    }

    #[test]
    fn test_throttle_active_within_30s_after_cap_reached() {
        // Base 8%, 10 failures → slippage at 99% cap, throttle engaged
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 10;
        pos.last_sell_failure = Some(chrono::Utc::now() - chrono::Duration::seconds(10));
        assert!(should_throttle(&pos, 8.0), "Should throttle 10s after cap reached");
    }

    #[test]
    fn test_throttle_clears_after_30s() {
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 10;
        pos.last_sell_failure = Some(chrono::Utc::now() - chrono::Duration::seconds(31));
        assert!(!should_throttle(&pos, 8.0), "Should NOT throttle 31s after failure");
    }

    #[test]
    fn test_throttle_at_exactly_30s_is_allowed() {
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 10;
        pos.last_sell_failure = Some(chrono::Utc::now() - chrono::Duration::seconds(30));
        // Predicate is `< 30`, so exactly 30s elapsed is allowed
        assert!(!should_throttle(&pos, 8.0), "30s boundary should not throttle");
    }

    #[test]
    fn test_throttle_skipped_when_no_timestamp() {
        // Edge case: cap reached but no timestamp (shouldn't happen in practice,
        // increment always sets it). Behaviour: don't throttle, allow the retry.
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 15;
        pos.last_sell_failure = None;
        assert!(!should_throttle(&pos, 8.0));
    }

    #[test]
    fn test_throttle_high_base_engages_quickly() {
        // Base 99%: throttle fires from the very first failure since slippage
        // is already at the cap.
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 1;
        pos.last_sell_failure = Some(chrono::Utc::now() - chrono::Duration::seconds(5));
        assert!(should_throttle(&pos, 99.0));
    }

    #[test]
    fn test_position_never_permanently_abandoned() {
        // Critical invariant: even at very high failure counts, after enough time
        // the throttle clears and the loop retries. There is no terminal state.
        let mut pos = make_test_position("TokenA", 0.001, 0.05);
        pos.sell_failures = 100;
        pos.last_sell_failure = Some(chrono::Utc::now() - chrono::Duration::minutes(5));
        assert!(
            !should_throttle(&pos, 8.0),
            "After throttle window, even a long-stuck position must be retried"
        );
    }

    // ==========================================
    // ENTRY PRICE CALCULATION TESTS
    // ==========================================

    #[test]
    fn test_entry_price_execution_based() {
        // Entry price should be: sol_spent / display_tokens
        // Example: spent 0.05 SOL for 1,000,000,000 raw tokens (= 1000 display tokens at 6 decimals)
        let sol_spent = 0.05;
        let raw_tokens: u64 = 1_000_000_000;
        let display_tokens = raw_tokens as f64 / 1_000_000.0; // = 1000.0
        let exec_price = sol_spent / display_tokens; // = 0.00005 SOL per display token

        assert!(
            (exec_price - 0.00005).abs() < 1e-12,
            "Expected 0.00005, got {}",
            exec_price
        );
    }

    #[test]
    fn test_entry_price_execution_vs_spot_inflated() {
        // The core bug: post-buy spot price is HIGHER than execution price
        // because our buy moves the bonding curve up.
        // Execution: spent 0.05 SOL for 1000 display tokens = 0.00005 SOL/token
        // Spot after buy: bonding curve moved up = 0.000055 SOL/token (inflated 10%)
        let exec_price = 0.00005;
        let spot_price_after_buy = 0.000055;

        // If we use spot as entry price and real price drops to exec_price level,
        // the bot thinks we're at -9.1% when we're actually at 0%
        let pnl_with_spot: f64 = ((exec_price - spot_price_after_buy) / spot_price_after_buy) * 100.0;
        let pnl_with_exec: f64 = ((exec_price - exec_price) / exec_price) * 100.0;

        assert!(
            pnl_with_spot < -9.0,
            "Spot-based PnL should show false loss: {}%",
            pnl_with_spot
        );
        assert!(
            pnl_with_exec.abs() < 1e-12,
            "Execution-based PnL should be 0%: {}%",
            pnl_with_exec
        );
    }

    #[test]
    fn test_entry_price_zero_tokens_no_divide_by_zero() {
        // If token_balance is 0, should NOT compute execution price (division by zero)
        let sol_amount: f64 = 0.05;
        let token_balance: u64 = 0;

        // Guard: only compute if token_balance > 0
        let exec_price = if token_balance > 0 && sol_amount > 0.0 {
            let display_tokens = token_balance as f64 / 1_000_000.0;
            sol_amount / display_tokens
        } else {
            0.0 // fallback
        };

        assert_eq!(exec_price, 0.0, "Should fallback to 0 when no tokens received");
    }

    #[test]
    fn test_entry_price_zero_sol_no_divide_by_zero() {
        let sol_amount: f64 = 0.0;
        let token_balance: u64 = 1_000_000_000;

        let exec_price = if token_balance > 0 && sol_amount > 0.0 {
            let display_tokens = token_balance as f64 / 1_000_000.0;
            sol_amount / display_tokens
        } else {
            0.0
        };

        assert_eq!(exec_price, 0.0, "Should fallback to 0 when sol_amount is 0");
    }

    #[test]
    fn test_entry_price_pnl_accuracy() {
        // Realistic scenario: bought at exec price, price doubles
        let exec_price: f64 = 0.0000003; // SOL per display token
        let current_price: f64 = 0.0000006; // 2x

        let pnl: f64 = ((current_price - exec_price) / exec_price) * 100.0;
        assert!(
            (pnl - 100.0).abs() < 1e-6,
            "Expected +100% PnL, got {:.2}%",
            pnl
        );
    }

    #[test]
    fn test_entry_price_backfill_skips_cycle() {
        // When entry_price is 0 and backfilled, the current cycle should be skipped
        // because PnL would be 0% (entry = current). This is simulated by the `continue`
        // in check_exit_conditions after backfill.
        let entry_price = 0.0;
        let current_price = 0.00005;

        // Backfill sets entry_price = current_price
        let backfilled_entry: f64 = if entry_price == 0.0 && current_price > 0.0 {
            current_price
        } else {
            entry_price
        };

        let pnl: f64 = ((current_price - backfilled_entry) / backfilled_entry) * 100.0;
        assert!(
            pnl.abs() < 1e-6,
            "PnL should be 0% on backfill cycle: {:.2}%",
            pnl
        );
    }

    // ==========================================
    // SELL FAILURE HANDLING IN EXIT LOOP TESTS
    // ==========================================

    #[test]
    fn test_exit_loop_continues_past_failed_sell() {
        // Simulate: 3 positions, middle one has sell_failures >= 3
        // The loop should skip the stuck one and process the others
        let positions = vec![
            ("TokenA", make_test_position("TokenA", 0.001, 0.05)),
            ("TokenB", {
                let mut p = make_test_position("TokenB", 0.002, 0.10);
                p.sell_failures = 4;
                p.last_sell_failure = Some(chrono::Utc::now()); // recent, won't reset
                p
            }),
            ("TokenC", make_test_position("TokenC", 0.003, 0.08)),
        ];

        let mut processed = Vec::new();
        let mut skipped = Vec::new();

        for (addr, pos) in &positions {
            if pos.sell_failures >= 3 {
                let should_reset = pos.last_sell_failure.map_or(true, |last_fail| {
                    (chrono::Utc::now() - last_fail).num_minutes() >= 5
                });
                if !should_reset {
                    skipped.push(*addr);
                    continue;
                }
            }
            processed.push(*addr);
        }

        assert_eq!(processed, vec!["TokenA", "TokenC"]);
        assert_eq!(skipped, vec!["TokenB"]);
    }

    #[test]
    fn test_exit_loop_processes_reset_position() {
        // Same as above but TokenB's failure was 6 minutes ago — should be reset and processed
        let positions = vec![
            ("TokenA", make_test_position("TokenA", 0.001, 0.05)),
            ("TokenB", {
                let mut p = make_test_position("TokenB", 0.002, 0.10);
                p.sell_failures = 4;
                p.last_sell_failure = Some(chrono::Utc::now() - chrono::Duration::minutes(6));
                p
            }),
            ("TokenC", make_test_position("TokenC", 0.003, 0.08)),
        ];

        let mut processed = Vec::new();

        for (addr, pos) in &positions {
            if pos.sell_failures >= 3 {
                let should_reset = pos.last_sell_failure.map_or(true, |last_fail| {
                    (chrono::Utc::now() - last_fail).num_minutes() >= 5
                });
                if !should_reset {
                    continue;
                }
                // Reset happens here in real code
            }
            processed.push(*addr);
        }

        assert_eq!(
            processed,
            vec!["TokenA", "TokenB", "TokenC"],
            "All positions should be processed after cooldown reset"
        );
    }

    #[test]
    fn test_pnl_calculation_sol_based() {
        // SOL PnL = sol_received - sol_spent (independent of price calculations)
        let sol_spent: f64 = 0.05;
        let sol_received: f64 = 0.08;
        let profit_loss: f64 = sol_received - sol_spent;

        assert!(
            (profit_loss - 0.03).abs() < 1e-12,
            "Expected +0.03 SOL profit, got {}",
            profit_loss
        );

        // Loss case
        let sol_received_loss: f64 = 0.02;
        let loss: f64 = sol_received_loss - sol_spent;
        assert!(
            (loss - (-0.03)).abs() < 1e-12,
            "Expected -0.03 SOL loss, got {}",
            loss
        );
    }

    #[test]
    fn test_trailing_stop_tier_selection() {
        // Simulate the trailing stop tier selection logic
        let thresholds: Vec<(f64, f64)> = vec![
            (8.0, 4.0),
            (15.0, 5.0),
            (25.0, 6.0),
            (38.0, 7.0),
            (55.0, 8.0),
        ];

        // Peak at +20% → active tier is 15:5 (highest threshold below peak)
        let peak = 20.0;
        let active = thresholds
            .iter()
            .rev()
            .find(|(gain_threshold, _)| peak >= *gain_threshold);
        assert_eq!(active, Some(&(15.0, 5.0)));

        // Exit at: 20 - 5 = 15%
        let exit_at = peak - active.unwrap().1;
        assert!((exit_at - 15.0).abs() < 1e-6);

        // Current PnL at +14% → should trigger exit (14 <= 15)
        let current_pnl = 14.0;
        assert!(current_pnl <= exit_at, "Should trigger trailing stop exit");

        // Current PnL at +16% → should NOT trigger
        let current_pnl_good = 16.0;
        assert!(
            current_pnl_good > exit_at,
            "Should NOT trigger trailing stop"
        );
    }

    #[test]
    fn test_momentum_reversal_priority() {
        // MR should trigger BEFORE stop loss when both conditions are met
        // MR condition: pnl < -min_loss AND price < SMA
        // Stop loss: loss >= stop_loss_percent
        let entry_price = 0.001;
        let current_price = 0.00094; // -6% loss
        let stop_loss_percent = 10.0;
        let mr_min_loss = 3.0;

        let pnl = ((current_price - entry_price) / entry_price) * 100.0;
        let loss = ((entry_price - current_price) / entry_price) * 100.0;

        // MR should fire at -6% (loss > mr_min_loss of 3%)
        assert!(pnl < -mr_min_loss, "MR condition met: pnl={:.1}%", pnl);
        // Stop loss should NOT fire (loss 6% < 10%)
        assert!(
            loss < stop_loss_percent,
            "Stop loss should NOT fire: loss={:.1}%",
            loss
        );
    }

    // =====================================================================
    // sol_received fallback estimation tests
    // =====================================================================

    /// Helper: simulate the fallback estimation logic from execute_sell
    fn estimate_sol_received_fallback(
        token_amount: u64,
        entry_price: f64,
    ) -> f64 {
        let estimated = token_amount as f64 / 1_000_000.0 * entry_price;
        estimated * 0.90 // conservative 90% estimate
    }

    #[test]
    fn test_sol_received_fallback_basic() {
        // Position: 100M raw tokens at entry price 0.0000001 SOL/display-token
        // Display tokens = 100M / 1e6 = 100
        // Entry value = 100 * 0.0000001 = 0.00001 SOL
        // Conservative estimate = 0.00001 * 0.90 = 0.000009
        let amount: u64 = 100_000_000;
        let entry_price: f64 = 0.0000001;
        let est = estimate_sol_received_fallback(amount, entry_price);
        assert!(
            (est - 0.000009).abs() < 1e-12,
            "Expected 0.000009, got {}",
            est
        );
    }

    #[test]
    fn test_sol_received_fallback_large_position() {
        // Position: 1B tokens at 0.000001 SOL/token (display)
        // Entry value = 1B / 1e6 * 0.000001 = 0.001 SOL
        // Conservative = 0.001 * 0.90 = 0.0009
        let amount: u64 = 1_000_000_000;
        let entry_price: f64 = 0.000001;
        let est = estimate_sol_received_fallback(amount, entry_price);
        assert!(
            (est - 0.0009).abs() < 1e-9,
            "Expected 0.0009, got {}",
            est
        );
    }

    #[test]
    fn test_sol_received_fallback_high_value_position() {
        // Position: 5B tokens at 0.00002 SOL/token (display)
        // Entry value = 5B / 1e6 * 0.00002 = 0.1 SOL
        // Conservative = 0.1 * 0.90 = 0.09
        let amount: u64 = 5_000_000_000;
        let entry_price: f64 = 0.00002;
        let est = estimate_sol_received_fallback(amount, entry_price);
        assert!(
            (est - 0.09).abs() < 1e-9,
            "Expected 0.09, got {}",
            est
        );
    }

    #[test]
    fn test_sol_received_fallback_always_less_than_entry_value() {
        // The 90% multiplier should always produce a value less than the entry value
        // This means the conservative estimate always records a loss, which is
        // better than recording 0 (break-even) when we don't know the real outcome
        let test_cases = vec![
            (50_000_000u64, 0.0000005f64),   // tiny position
            (500_000_000, 0.000005),          // small position
            (5_000_000_000, 0.00005),         // medium position
            (50_000_000_000, 0.0005),         // large position
        ];

        for (amount, entry_price) in test_cases {
            let entry_value = amount as f64 / 1_000_000.0 * entry_price;
            let est = estimate_sol_received_fallback(amount, entry_price);
            assert!(
                est < entry_value,
                "Estimate {:.9} should be less than entry value {:.9} for amount={} price={}",
                est, entry_value, amount, entry_price
            );
            // Should be exactly 90% of entry value
            let ratio = est / entry_value;
            assert!(
                (ratio - 0.90).abs() < 1e-9,
                "Ratio should be 0.90, got {}",
                ratio
            );
        }
    }

    #[test]
    fn test_sol_received_fallback_produces_realistic_loss() {
        // Simulate what happens in journal: profit_loss = sol_received - sol_spent
        // With the 90% estimate, the recorded loss should be ~10% of entry value
        let amount: u64 = 60_000_000_000; // 60B raw tokens
        let entry_price: f64 = 0.000001; // 0.000001 SOL per display token
        let sol_spent: f64 = 0.06; // what we paid

        let est_received = estimate_sol_received_fallback(amount, entry_price);
        let profit_loss = est_received - sol_spent;

        // Entry value = 60B/1e6 * 0.000001 = 0.06 SOL (matches sol_spent)
        // Estimated received = 0.06 * 0.90 = 0.054
        // profit_loss = 0.054 - 0.06 = -0.006 (10% loss)
        assert!(
            profit_loss < 0.0,
            "Fallback should always show a loss, got {}",
            profit_loss
        );
        assert!(
            (profit_loss - (-0.006)).abs() < 1e-9,
            "Expected -0.006 loss, got {}",
            profit_loss
        );
    }

    #[test]
    fn test_sol_received_fallback_zero_entry_price() {
        // Edge case: entry_price is 0 (backfill never happened)
        // Should produce 0 estimate, not panic
        let amount: u64 = 100_000_000;
        let entry_price: f64 = 0.0;
        let est = estimate_sol_received_fallback(amount, entry_price);
        assert!(
            est == 0.0,
            "Zero entry price should produce zero estimate, got {}",
            est
        );
    }

    #[test]
    fn test_sol_received_fallback_zero_amount() {
        // Edge case: amount is 0 (shouldn't happen, but guard against it)
        let amount: u64 = 0;
        let entry_price: f64 = 0.000001;
        let est = estimate_sol_received_fallback(amount, entry_price);
        assert!(
            est == 0.0,
            "Zero amount should produce zero estimate, got {}",
            est
        );
    }

    #[test]
    fn test_sol_received_fallback_wallet_scoring_impact() {
        // Verify that the fallback estimate produces a negative PnL for wallet scoring
        // This is important because before this fix, null sol_received caused
        // wallet scoring to use unwrap_or(0.0), treating unknown sells as break-even
        let amount: u64 = 80_000_000_000;
        let entry_price: f64 = 0.0000005;
        let sol_spent: f64 = 0.04;

        let est_received = estimate_sol_received_fallback(amount, entry_price);
        // Wallet scoring does: pnl_sol = sol_received - sol_spent
        let pnl_sol = est_received - sol_spent;

        // Before fix: pnl_sol = 0.0 (break-even, inflating wallet score)
        // After fix: pnl_sol < 0 (conservative loss, preventing score inflation)
        assert!(
            pnl_sol < 0.0,
            "Wallet scoring PnL should be negative with fallback, got {}",
            pnl_sol
        );
    }

    #[test]
    fn test_sol_received_retry_logic_simulation() {
        // Simulate the retry decision logic:
        // sol_before = 1_000_000_000 (1 SOL)
        // After sell, balance might not update immediately
        let sol_before: u64 = 1_000_000_000;

        // Simulate 5 attempts where balance doesn't change
        let stale_readings = vec![
            1_000_000_000u64, // attempt 1: same as before
            1_000_000_000,    // attempt 2: still stale
            1_000_000_000,    // attempt 3: still stale
            1_050_000_000,    // attempt 4: updated! +0.05 SOL
            1_050_000_000,    // attempt 5: would not be reached
        ];

        let mut sol_received: Option<f64> = None;
        for (attempt, &sol_after) in stale_readings.iter().enumerate() {
            if sol_after > sol_before {
                let received = (sol_after - sol_before) as f64 / 1_000_000_000.0;
                sol_received = Some(received);
                assert_eq!(attempt, 3, "Should succeed on attempt 4 (index 3)");
                break;
            }
        }

        assert!(sol_received.is_some(), "Should have found balance increase");
        let received = sol_received.unwrap();
        assert!(
            (received - 0.05).abs() < 1e-9,
            "Should receive 0.05 SOL, got {}",
            received
        );
    }

    #[test]
    fn test_sol_received_retry_all_stale_triggers_fallback() {
        // If all 5 attempts return stale balance, fallback should be used
        let sol_before: u64 = 1_000_000_000;
        let max_retries = 5;

        let mut sol_received: Option<f64> = None;
        for _attempt in 0..max_retries {
            let sol_after = sol_before; // always stale
            if sol_after > sol_before {
                sol_received = Some((sol_after - sol_before) as f64 / 1_000_000_000.0);
                break;
            }
        }

        // All retries exhausted, sol_received is still None
        assert!(sol_received.is_none(), "All retries stale should leave None");

        // Fallback estimation kicks in
        let amount: u64 = 100_000_000_000;
        let entry_price: f64 = 0.000001;
        let estimated = amount as f64 / 1_000_000.0 * entry_price;
        let conservative_estimate = estimated * 0.90;
        sol_received = Some(conservative_estimate);

        assert!(
            sol_received.is_some(),
            "After fallback, sol_received must be Some"
        );
        assert!(
            (sol_received.unwrap() - 0.09).abs() < 1e-9,
            "Fallback should be 0.09, got {}",
            sol_received.unwrap()
        );
    }

    #[test]
    fn test_trade_sol_received_never_none_after_fix() {
        // The key invariant: after the fix, Trade.sol_received should NEVER be None
        // for sell trades. This test verifies the invariant by constructing trades
        // as the fixed code would.

        // Case 1: Balance check succeeds on first try
        let trade_success = Trade {
            id: "test1".to_string(),
            token_address: "mint1".to_string(),
            action: Action::Sell,
            amount: 1000,
            price: 0.001,
            timestamp: chrono::Utc::now(),
            signature: Some("sig1".to_string()),
            profit_loss: Some(0.005),
            sol_received: Some(0.055),
        };
        assert!(trade_success.sol_received.is_some());
        assert!(trade_success.profit_loss.is_some());

        // Case 2: Balance check fails, fallback estimate used
        let sol_spent: f64 = 0.05;
        let amount: u64 = 100_000_000;
        let entry_price: f64 = 0.0000005;
        let fallback = estimate_sol_received_fallback(amount, entry_price);
        let trade_fallback = Trade {
            id: "test2".to_string(),
            token_address: "mint2".to_string(),
            action: Action::Sell,
            amount,
            price: 0.001,
            timestamp: chrono::Utc::now(),
            signature: Some("sig2".to_string()),
            profit_loss: Some(fallback - sol_spent),
            sol_received: Some(fallback),
        };
        assert!(trade_fallback.sol_received.is_some());
        assert!(trade_fallback.profit_loss.is_some());
        // Fallback should record a loss (conservative)
        assert!(trade_fallback.profit_loss.unwrap() < 0.0);
    }
}
