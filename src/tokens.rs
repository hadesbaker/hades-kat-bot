use crate::config::NewTokenFilters;
use crate::pumpportal::{self, MigrationEvent, TokenInfo};
use log::{debug, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use std::collections::HashMap;
use std::sync::Arc;

/// Entry in the new token watchlist.
#[derive(Debug, Clone)]
struct WatchlistEntry {
    added_at: std::time::Instant,
    /// Peak bonding curve price observed (SOL per token)
    peak_price_sol: f64,
    /// Last known bonding curve price
    last_price_sol: f64,
    /// Last time this token was polled
    last_poll: std::time::Instant,
    /// Enriched TokenInfo from DexScreener (populated async)
    enriched: Option<TokenInfo>,
    /// Last time enrichment was attempted
    last_enrichment: Option<std::time::Instant>,
    /// Whether this token has already been sent for strategy analysis
    signal_sent: bool,
}

/// Monitors newly created tokens from PumpPortal, tracks their bonding curve
/// prices, and emits tokens that pass filters for strategy analysis.
pub struct NewTokenMonitor {
    watchlist: HashMap<String, WatchlistEntry>,
    filters: NewTokenFilters,
    rpc_client: Arc<AsyncRpcClient>,
    /// Optional sender for graduation events. When set, the monitor forwards a
    /// MigrationEvent into this channel for any watchlist token whose bonding-curve
    /// price suddenly stops returning (i.e., the curve flipped to complete=1).
    /// This is the watchlist-transition safety net for the PumpSwapSniper strategy.
    migration_tx: Option<tokio::sync::mpsc::UnboundedSender<MigrationEvent>>,
}

impl NewTokenMonitor {
    pub fn new(filters: NewTokenFilters, rpc_client: Arc<AsyncRpcClient>) -> Self {
        Self {
            watchlist: HashMap::new(),
            filters,
            rpc_client,
            migration_tx: None,
        }
    }

    pub fn set_migration_tx(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<MigrationEvent>,
    ) {
        self.migration_tx = Some(tx);
    }

    /// Add a new token to the watchlist if it passes the pre-filter.
    /// Returns true if the token was added.
    pub fn add_token(&mut self, mint: String, _bonding_curve_key: String, market_cap_sol: f64) -> bool {
        // Pre-filter: minimum market cap in SOL
        if market_cap_sol < self.filters.min_initial_market_cap_sol {
            debug!(
                "New token {} pre-filtered: mcap_sol={:.4} < min {:.4}",
                mint, market_cap_sol, self.filters.min_initial_market_cap_sol
            );
            return false;
        }

        // Skip if already in watchlist
        if self.watchlist.contains_key(&mint) {
            return false;
        }

        // Enforce watchlist cap — drop oldest token if full
        if self.watchlist.len() >= self.filters.watchlist_cap {
            if let Some(oldest_key) = self
                .watchlist
                .iter()
                .min_by_key(|(_, e)| e.added_at)
                .map(|(k, _)| k.clone())
            {
                debug!("Watchlist full, dropping oldest token: {}", oldest_key);
                self.watchlist.remove(&oldest_key);
            }
        }

        let now = std::time::Instant::now();
        self.watchlist.insert(
            mint.clone(),
            WatchlistEntry {
                added_at: now,
                peak_price_sol: 0.0,
                last_price_sol: 0.0,
                last_poll: now,
                enriched: None,
                last_enrichment: None,
                signal_sent: false,
            },
        );

        true
    }

    /// Tick the monitor: batch-read bonding curve prices, enrich tokens that need it,
    /// apply filters, and return tokens ready for strategy analysis.
    /// Call this on every monitor interval tick from the event loop.
    pub async fn tick(&mut self) -> Vec<TokenInfo> {
        if self.watchlist.is_empty() {
            return Vec::new();
        }

        let now = std::time::Instant::now();
        let monitor_duration =
            std::time::Duration::from_secs(self.filters.monitor_duration_minutes * 60);
        let poll_interval = std::time::Duration::from_secs(self.filters.poll_interval_secs);
        let reduced_poll_interval =
            std::time::Duration::from_secs(self.filters.reduced_poll_interval_secs);
        let reduced_poll_after =
            std::time::Duration::from_secs(self.filters.reduced_poll_after_minutes * 60);
        let rug_threshold = self.filters.rug_drop_percent / 100.0;

        // Step 1: Remove expired tokens
        let expired: Vec<String> = self
            .watchlist
            .iter()
            .filter(|(_, e)| now.duration_since(e.added_at) >= monitor_duration)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &expired {
            debug!("Watchlist token {} expired ({}m elapsed)", key, self.filters.monitor_duration_minutes);
            self.watchlist.remove(key);
        }

        if self.watchlist.is_empty() {
            return Vec::new();
        }

        // Step 2: Determine which tokens need polling based on tiered intervals
        let tokens_to_poll: Vec<String> = self
            .watchlist
            .iter()
            .filter(|(_, e)| {
                if e.signal_sent {
                    return false; // Already sent for analysis
                }
                let age = now.duration_since(e.added_at);
                let interval = if age >= reduced_poll_after {
                    reduced_poll_interval
                } else {
                    poll_interval
                };
                now.duration_since(e.last_poll) >= interval
            })
            .map(|(k, _)| k.clone())
            .collect();

        if tokens_to_poll.is_empty() {
            return Vec::new();
        }

        // Step 3: Batch bonding curve price read
        let prices = match pumpportal::fetch_prices_bonding_curve_batch(
            &self.rpc_client,
            &tokens_to_poll,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!("Batch bonding curve read error: {}", e);
                Vec::new()
            }
        };

        let price_map: HashMap<String, f64> = prices.into_iter().collect();

        // Graduation detection: any token we polled that previously had a price but
        // didn't come back from this batch read has very likely graduated to PumpSwap
        // (fetch_prices_bonding_curve_batch silently skips complete=1 accounts).
        // Forward a synthetic MigrationEvent for the sniper strategy. Bot-level dedup
        // prevents collisions with the WS listener.
        let mut graduated: Vec<String> = Vec::new();
        if self.migration_tx.is_some() {
            for mint in &tokens_to_poll {
                if let Some(entry) = self.watchlist.get(mint) {
                    if entry.last_price_sol > 0.0 && !price_map.contains_key(mint) {
                        graduated.push(mint.clone());
                    }
                }
            }
            if let Some(tx) = &self.migration_tx {
                for mint in &graduated {
                    info!(
                        "Watchlist transition: {} bonding curve disappeared (likely graduated)",
                        mint
                    );
                    let _ = tx.send(MigrationEvent {
                        mint: mint.clone(),
                        tx_type: "watchlist_transition".to_string(),
                        signature: String::new(),
                        pool: String::new(),
                    });
                }
            }
        }

        // Update prices and detect rugs
        let mut rugged: Vec<String> = Vec::new();
        for mint in &tokens_to_poll {
            if let Some(entry) = self.watchlist.get_mut(mint) {
                entry.last_poll = now;

                if let Some(&price) = price_map.get(mint) {
                    entry.last_price_sol = price;
                    if price > entry.peak_price_sol {
                        entry.peak_price_sol = price;
                    }

                    // Rug detection: if price dropped rug_threshold% from peak
                    if entry.peak_price_sol > 0.0 {
                        let drop_pct =
                            (entry.peak_price_sol - price) / entry.peak_price_sol;
                        if drop_pct >= rug_threshold {
                            info!(
                                "Token {} rugged: price dropped {:.1}% from peak ({:.12} → {:.12} SOL)",
                                mint,
                                drop_pct * 100.0,
                                entry.peak_price_sol,
                                price
                            );
                            rugged.push(mint.clone());
                        }
                    }
                }
            }
        }

        for key in &rugged {
            self.watchlist.remove(key);
        }

        // Drop graduated tokens — they'll continue to return no bonding-curve price
        // every tick and are now the sniper's responsibility, not the watchlist's.
        for key in &graduated {
            self.watchlist.remove(key);
        }

        // Step 4: Enrich tokens that haven't been enriched recently (every 30s)
        let enrichment_interval = std::time::Duration::from_secs(30);
        let tokens_to_enrich: Vec<String> = self
            .watchlist
            .iter()
            .filter(|(_, e)| {
                !e.signal_sent
                    && e.last_price_sol > 0.0
                    && e.last_enrichment
                        .map(|t| now.duration_since(t) >= enrichment_interval)
                        .unwrap_or(true)
            })
            .map(|(k, _)| k.clone())
            .collect();

        if !tokens_to_enrich.is_empty() {
            match pumpportal::enrich_tokens_dexscreener(&tokens_to_enrich).await {
                Ok(enriched_tokens) => {
                    for token_info in enriched_tokens {
                        if let Some(entry) = self.watchlist.get_mut(&token_info.address) {
                            entry.enriched = Some(token_info);
                            entry.last_enrichment = Some(now);
                        }
                    }
                }
                Err(e) => {
                    warn!("DexScreener enrichment for watchlist failed: {}", e);
                }
            }

            // Mark tokens that got no DexScreener data as attempted
            for mint in &tokens_to_enrich {
                if let Some(entry) = self.watchlist.get_mut(mint) {
                    if entry.last_enrichment.is_none()
                        || entry.last_enrichment.unwrap() != now
                    {
                        entry.last_enrichment = Some(now);
                    }
                }
            }
        }

        // Step 5: Apply filters and collect tokens ready for strategy analysis.
        // Tokens are eligible once they have a bonding curve price; we prefer DexScreener
        // data when available, but fall back to a synthesized TokenInfo built from on-chain
        // data after a short delay — DexScreener typically takes minutes to index brand-new
        // pump.fun tokens, and without this fallback no fresh token ever reaches analysis.
        let mut ready_tokens: Vec<TokenInfo> = Vec::new();

        let bonding_curve_fallback_delay = std::time::Duration::from_secs(15);

        let candidates: Vec<String> = self
            .watchlist
            .iter()
            .filter(|(_, e)| {
                if e.signal_sent || e.last_price_sol == 0.0 {
                    return false;
                }
                e.enriched.is_some()
                    || now.duration_since(e.added_at) >= bonding_curve_fallback_delay
            })
            .map(|(k, _)| k.clone())
            .collect();

        for mint in candidates {
            let entry = match self.watchlist.get(&mint) {
                Some(e) => e,
                None => continue,
            };
            let (token, source) = match &entry.enriched {
                Some(t) => (t.clone(), "dexscreener"),
                None => (synthesize_from_bonding_curve(&mint, entry), "bonding_curve"),
            };

            if !self.passes_filters(&token) {
                continue;
            }

            info!(
                "Watchlist token {} ({}) passed filters: mcap={} holders={} liq={} vol={} (monitored {:.0}s, source={})",
                token.symbol,
                mint,
                token.market_cap,
                token.holders,
                token.liquidity,
                token.volume_24h,
                now.duration_since(entry.added_at).as_secs_f64(),
                source
            );

            ready_tokens.push(token);

            // Mark as sent so we don't re-analyze
            if let Some(entry) = self.watchlist.get_mut(&mint) {
                entry.signal_sent = true;
            }
        }

        ready_tokens
    }

    /// Check if a token passes the new token filters.
    fn passes_filters(&self, token: &TokenInfo) -> bool {
        let f = &self.filters;

        if f.min_market_cap > 0 && token.market_cap < f.min_market_cap {
            debug!(
                "New token {} filtered: mcap {} < min {}",
                token.symbol, token.market_cap, f.min_market_cap
            );
            return false;
        }
        if f.max_market_cap > 0 && token.market_cap > f.max_market_cap {
            debug!(
                "New token {} filtered: mcap {} > max {}",
                token.symbol, token.market_cap, f.max_market_cap
            );
            return false;
        }
        if f.min_holders > 0 && token.holders < f.min_holders {
            debug!(
                "New token {} filtered: holders {} < min {}",
                token.symbol, token.holders, f.min_holders
            );
            return false;
        }
        if f.min_liquidity > 0 && token.liquidity < f.min_liquidity {
            debug!(
                "New token {} filtered: liq {} < min {}",
                token.symbol, token.liquidity, f.min_liquidity
            );
            return false;
        }
        if f.min_volume_24h > 0 && token.volume_24h < f.min_volume_24h {
            debug!(
                "New token {} filtered: vol {} < min {}",
                token.symbol, token.volume_24h, f.min_volume_24h
            );
            return false;
        }

        true
    }

    /// Current number of tokens being monitored.
    pub fn watchlist_len(&self) -> usize {
        self.watchlist.len()
    }
}

/// Build a TokenInfo from bonding curve data alone, for tokens DexScreener has not indexed yet.
/// pump.fun tokens have a fixed 1B display-token supply, so mcap_sol = price_sol * 1e9.
/// USD-denominated fields use a rough SOL price constant — strategy filters compare against
/// these, but the bot's PnL math is SOL-denominated end-to-end so the constant only needs to
/// be in the right ballpark for thresholds, not exact.
fn synthesize_from_bonding_curve(mint: &str, entry: &WatchlistEntry) -> TokenInfo {
    const SOL_USD_APPROX: f64 = 185.0;
    const PUMP_FUN_SUPPLY_DISPLAY: f64 = 1_000_000_000.0;

    let mcap_sol = entry.last_price_sol * PUMP_FUN_SUPPLY_DISPLAY;
    let market_cap_usd = (mcap_sol * SOL_USD_APPROX) as u64;
    // Bonding-curve effective liquidity is roughly 30% of mcap when fresh
    let liquidity_usd = ((mcap_sol * SOL_USD_APPROX) * 0.30) as u64;

    TokenInfo {
        address: mint.to_string(),
        symbol: mint[..8.min(mint.len())].to_string(),
        name: "BONDING_CURVE".to_string(),
        decimals: 6,
        market_cap: market_cap_usd.max(1),
        // Real holder count isn't readable cheaply from bonding curve; placeholder above the
        // hardcoded VolumeSpike default of 5 so that strategy can fire too
        holders: 100,
        age_hours: 0,
        liquidity: liquidity_usd.max(1),
        // Codebase convention: TokenInfo.price_usd carries SOL price, not USD
        price_usd: entry.last_price_sol,
        price_change_24h: 0.0,
        volume_24h: 0,
        created_at: chrono::Utc::now().to_rfc3339(),
        just_graduated_at_secs: None,
    }
}
