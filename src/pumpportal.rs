use crate::error::{BotError, Result};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
    pub market_cap: u64,
    pub holders: u32,
    pub age_hours: u32,
    pub liquidity: u64,
    pub price_usd: f64,
    pub price_change_24h: f64,
    pub volume_24h: u64,
    pub created_at: String,
    /// Seconds since this token graduated from PumpFun to PumpSwap. Set only when the
    /// TokenInfo originates from a graduation event (PumpPortal subscribeMigration WS or
    /// the watchlist transition detector). `None` for everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub just_graduated_at_secs: Option<u64>,
}

/// PumpFun program ID for bonding curve PDA derivation.
const PUMPFUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// PumpSwap AMM program ID — graduated PumpFun tokens trade here.
const PUMP_SWAP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// Native SOL mint address (wrapped SOL).
const SOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";
/// Derive the bonding curve PDA for a given token mint.
fn derive_bonding_curve_pda(mint: &Pubkey) -> Result<Pubkey> {
    let program_id = Pubkey::from_str(PUMPFUN_PROGRAM_ID)
        .map_err(|e| BotError::PumpPortal(format!("Invalid program ID: {}", e)))?;
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &program_id);
    Ok(pda)
}

/// Read the bonding curve account directly from RPC and calculate token price in SOL.
///
/// BondingCurveAccount layout (from pumpfun crate):
///   bytes  0-7:  discriminator (u64)
///   bytes  8-15: virtual_token_reserves (u64)
///   bytes 16-23: virtual_sol_reserves (u64)
///   bytes 24-31: real_token_reserves (u64)
///   bytes 32-39: real_sol_reserves (u64)
///   bytes 40-47: token_total_supply (u64)
///   byte  48:    complete (bool)
///   bytes 49-80: creator (Pubkey)
///
/// Price formula: price_sol = (virtual_sol_reserves / 1e9) / (virtual_token_reserves / 1e6)
///
/// Returns SOL price per display token. Err if the bonding curve is complete (graduated).
pub async fn fetch_price_bonding_curve_rpc(
    rpc_client: &AsyncRpcClient,
    token_address: &str,
) -> Result<f64> {
    let mint = Pubkey::from_str(token_address)
        .map_err(|e| BotError::InvalidTokenAddress(format!("Invalid mint: {}", e)))?;

    let bonding_curve_pda = derive_bonding_curve_pda(&mint)?;

    // Fetch with 500ms timeout
    let account = match tokio::time::timeout(
        Duration::from_millis(500),
        rpc_client.get_account(&bonding_curve_pda),
    )
    .await
    {
        Ok(Ok(acc)) => acc,
        Ok(Err(e)) => {
            return Err(BotError::PumpPortal(format!(
                "Bonding curve account read failed: {}",
                e
            )));
        }
        Err(_) => {
            return Err(BotError::PumpPortal(
                "Bonding curve RPC timed out (500ms)".to_string(),
            ));
        }
    };

    let data = &account.data;

    if data.len() < 49 {
        return Err(BotError::PumpPortal(format!(
            "Bonding curve data too short: {} bytes",
            data.len()
        )));
    }

    let virtual_token_reserves = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let virtual_sol_reserves = u64::from_le_bytes(data[16..24].try_into().unwrap());

    // Check complete flag at byte 48
    if data.len() > 48 && data[48] != 0 {
        return Err(BotError::PumpPortal(
            "Bonding curve complete — token graduated to PumpSwap".to_string(),
        ));
    }

    if virtual_token_reserves == 0 {
        return Err(BotError::PumpPortal(
            "virtual_token_reserves is zero".to_string(),
        ));
    }

    // Normalize reserves: lamports (9 decimals) → SOL, raw tokens (6 decimals) → display tokens
    let sol_reserves = virtual_sol_reserves as f64 / 1_000_000_000.0;
    let token_reserves = virtual_token_reserves as f64 / 1_000_000.0;
    let price_sol = sol_reserves / token_reserves;

    log::debug!(
        "Bonding curve price for {}: {:.12} SOL vsol={} vtok={}",
        token_address,
        price_sol,
        virtual_sol_reserves,
        virtual_token_reserves
    );

    Ok(price_sol)
}

/// Cached info for a PumpSwap pool — avoids repeated discovery and ATA derivation.
#[derive(Debug, Clone)]
pub struct PumpSwapPoolInfo {
    pub pool_id: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

/// Find the PumpSwap AMM pool for a graduated token.
/// Discovers pool_id, base_vault, and quote_vault so price reads only need 1 RPC call.
///
/// Uses getTokenLargestAccounts → getMultipleAccounts → getTokenAccountsByOwner.
/// Works on all RPC providers (unlike getProgramAccounts which QuickNode blocks).
/// Cache the result — this is a one-time discovery per token.
pub async fn find_pumpswap_pool(
    rpc_client: &AsyncRpcClient,
    token_address: &str,
) -> Result<PumpSwapPoolInfo> {
    let mint = Pubkey::from_str(token_address)
        .map_err(|e| BotError::InvalidTokenAddress(format!("Invalid mint: {}", e)))?;

    let pump_swap_program = Pubkey::from_str(PUMP_SWAP_PROGRAM_ID)
        .map_err(|e| BotError::PumpPortal(format!("Invalid PumpSwap program ID: {}", e)))?;
    let sol_mint = Pubkey::from_str(SOL_MINT_STR).unwrap();

    // Step 1: Get the largest token accounts for this mint
    let largest = match tokio::time::timeout(
        Duration::from_millis(3000),
        rpc_client.get_token_largest_accounts(&mint),
    )
    .await
    {
        Ok(Ok(accs)) => accs,
        Ok(Err(e)) => {
            return Err(BotError::PumpPortal(format!(
                "getTokenLargestAccounts failed for {}: {}",
                token_address, e
            )));
        }
        Err(_) => {
            return Err(BotError::PumpPortal(
                "getTokenLargestAccounts timed out (3s)".to_string(),
            ));
        }
    };

    if largest.is_empty() {
        return Err(BotError::PumpPortal(format!(
            "No token accounts found for {}",
            token_address
        )));
    }

    // Step 2: Read the top token accounts to extract their owners
    let token_account_pubkeys: Vec<Pubkey> = largest
        .iter()
        .take(10)
        .filter_map(|a| Pubkey::from_str(&a.address).ok())
        .collect();

    let token_accounts = rpc_client
        .get_multiple_accounts(&token_account_pubkeys)
        .await
        .map_err(|e| BotError::PumpPortal(format!("Failed to read token accounts: {}", e)))?;

    // Step 3: Find the token account whose owner is a PumpSwap pool
    // SPL token account layout: mint(0-31), owner(32-63), amount(64-71)
    let mut candidate_owners: Vec<(Pubkey, usize)> = Vec::new(); // (owner, index into token_account_pubkeys)
    for (idx, acc_opt) in token_accounts.iter().enumerate() {
        if let Some(acc) = acc_opt {
            if acc.data.len() >= 64 {
                let owner_bytes: [u8; 32] = acc.data[32..64].try_into().unwrap();
                candidate_owners.push((Pubkey::from(owner_bytes), idx));
            }
        }
    }

    if candidate_owners.is_empty() {
        return Err(BotError::PumpPortal(format!(
            "Could not extract owners from token accounts for {}",
            token_address
        )));
    }

    let owner_pubkeys: Vec<Pubkey> = candidate_owners.iter().map(|(pk, _)| *pk).collect();
    let owner_accounts = rpc_client
        .get_multiple_accounts(&owner_pubkeys)
        .await
        .map_err(|e| BotError::PumpPortal(format!("Failed to read owner accounts: {}", e)))?;

    let mut pool_id: Option<Pubkey> = None;
    let mut base_vault: Option<Pubkey> = None;

    for (i, acc_opt) in owner_accounts.iter().enumerate() {
        if let Some(acc) = acc_opt {
            if acc.owner == pump_swap_program {
                pool_id = Some(owner_pubkeys[i]);
                // The token account at this index IS the base vault
                base_vault = Some(token_account_pubkeys[candidate_owners[i].1]);
                break;
            }
        }
    }

    let pool_id = pool_id.ok_or_else(|| {
        BotError::PumpPortal(format!(
            "No PumpSwap pool found among top holders for {}",
            token_address
        ))
    })?;
    let base_vault = base_vault.unwrap(); // Safe: set whenever pool_id is set

    // Step 4: Find quote vault (WSOL token account owned by the pool)
    let sol_token_accounts = rpc_client
        .get_token_accounts_by_owner(&pool_id, TokenAccountsFilter::Mint(sol_mint))
        .await
        .map_err(|e| {
            BotError::PumpPortal(format!(
                "getTokenAccountsByOwner failed for pool {}: {}",
                pool_id, e
            ))
        })?;

    let quote_vault = sol_token_accounts
        .first()
        .and_then(|a| Pubkey::from_str(&a.pubkey).ok())
        .ok_or_else(|| {
            BotError::PumpPortal(format!(
                "No SOL vault found for PumpSwap pool {}",
                pool_id
            ))
        })?;

    log::info!(
        "PumpSwap pool for {}: pool={} base_vault={} quote_vault={}",
        token_address,
        pool_id,
        base_vault,
        quote_vault
    );

    Ok(PumpSwapPoolInfo {
        pool_id,
        base_vault,
        quote_vault,
    })
}

/// Fetch SOL-denominated price from a PumpSwap pool.
/// Reads the cached vault addresses directly — no ATA derivation.
/// Only 1 RPC call (getMultipleAccounts for 2 vaults).
pub async fn fetch_price_pumpswap_sol(
    rpc_client: &AsyncRpcClient,
    pool_info: &PumpSwapPoolInfo,
    token_address: &str,
) -> Result<f64> {
    // Read both vault accounts in a single RPC call
    let accounts = match tokio::time::timeout(
        Duration::from_millis(500),
        rpc_client.get_multiple_accounts(&[pool_info.base_vault, pool_info.quote_vault]),
    )
    .await
    {
        Ok(Ok(accs)) => accs,
        Ok(Err(e)) => {
            return Err(BotError::PumpPortal(format!(
                "PumpSwap vault read failed: {}",
                e
            )));
        }
        Err(_) => {
            return Err(BotError::PumpPortal(
                "PumpSwap vault read timed out (500ms)".to_string(),
            ));
        }
    };

    // Extract base token balance (SPL token account: amount at offset 64, u64 LE)
    let base_account = accounts[0].as_ref().ok_or_else(|| {
        BotError::PumpPortal(format!(
            "PumpSwap base vault {} does not exist",
            pool_info.base_vault
        ))
    })?;
    if base_account.data.len() < 72 {
        return Err(BotError::PumpPortal(format!(
            "PumpSwap base vault data too short: {} bytes",
            base_account.data.len()
        )));
    }
    let base_amount = u64::from_le_bytes(base_account.data[64..72].try_into().unwrap());

    // Extract quote SOL balance
    let quote_account = accounts[1].as_ref().ok_or_else(|| {
        BotError::PumpPortal(format!(
            "PumpSwap quote vault {} does not exist",
            pool_info.quote_vault
        ))
    })?;
    if quote_account.data.len() < 72 {
        return Err(BotError::PumpPortal(format!(
            "PumpSwap quote vault data too short: {} bytes",
            quote_account.data.len()
        )));
    }
    let quote_amount = u64::from_le_bytes(quote_account.data[64..72].try_into().unwrap());

    if base_amount == 0 {
        return Err(BotError::PumpPortal(
            "PumpSwap base reserve is zero".to_string(),
        ));
    }

    // Price in SOL per display token:
    // quote is in lamports (9 decimals), base is in raw token units (6 decimals)
    let quote_sol = quote_amount as f64 / 1_000_000_000.0;
    let base_tokens = base_amount as f64 / 1_000_000.0;
    let price_sol = quote_sol / base_tokens;

    log::debug!(
        "PumpSwap price for {}: {:.12} SOL (pool={} base={} quote={})",
        token_address,
        price_sol,
        pool_info.pool_id,
        base_amount,
        quote_amount
    );

    Ok(price_sol)
}

/// Batch-read bonding curve prices for multiple tokens in a single RPC call.
/// Uses getMultipleAccounts to read up to 100 bonding curve PDAs at once.
/// Returns a Vec of (token_address, price_sol) for tokens that have valid bonding curves.
/// Tokens that have graduated or have errors are silently skipped.
pub async fn fetch_prices_bonding_curve_batch(
    rpc_client: &AsyncRpcClient,
    token_addresses: &[String],
) -> Result<Vec<(String, f64)>> {
    if token_addresses.is_empty() {
        return Ok(Vec::new());
    }

    // Derive PDAs for all tokens
    let mut pda_to_address: Vec<(Pubkey, String)> = Vec::new();
    for addr in token_addresses {
        if let Ok(mint) = Pubkey::from_str(addr) {
            if let Ok(pda) = derive_bonding_curve_pda(&mint) {
                pda_to_address.push((pda, addr.clone()));
            }
        }
    }

    if pda_to_address.is_empty() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();

    // Process in chunks of 100 (RPC limit for getMultipleAccounts)
    for chunk in pda_to_address.chunks(100) {
        let pdas: Vec<Pubkey> = chunk.iter().map(|(pda, _)| *pda).collect();

        let accounts = match tokio::time::timeout(
            Duration::from_millis(2000),
            rpc_client.get_multiple_accounts(&pdas),
        )
        .await
        {
            Ok(Ok(accs)) => accs,
            Ok(Err(e)) => {
                log::warn!("Batch bonding curve read failed: {}", e);
                continue;
            }
            Err(_) => {
                log::warn!("Batch bonding curve read timed out (2s)");
                continue;
            }
        };

        for (i, acc_opt) in accounts.iter().enumerate() {
            if let Some(acc) = acc_opt {
                let data = &acc.data;
                if data.len() < 49 {
                    continue;
                }

                // Skip graduated tokens (complete flag)
                if data[48] != 0 {
                    continue;
                }

                let virtual_token_reserves = u64::from_le_bytes(data[8..16].try_into().unwrap());
                let virtual_sol_reserves = u64::from_le_bytes(data[16..24].try_into().unwrap());

                if virtual_token_reserves == 0 {
                    continue;
                }

                let sol_reserves = virtual_sol_reserves as f64 / 1_000_000_000.0;
                let token_reserves = virtual_token_reserves as f64 / 1_000_000.0;
                let price_sol = sol_reserves / token_reserves;

                results.push((chunk[i].1.clone(), price_sol));
            }
        }
    }

    Ok(results)
}

/// Simulate selling `tokens_in_raw` tokens through a constant-product pool
/// (`x * y = k`) and return the effective per-display-token SOL price.
///
/// Reserves are in their on-chain units:
///   - `reserve_base_raw`: pump.fun raw token units (6 decimals)
///   - `reserve_quote_lamports`: lamports (9 decimals)
///   - `tokens_in_raw`: raw token units to sell
///
/// Returns SOL per display token, or 0.0 if the simulation would consume the pool.
/// The same formula works for the PumpFun bonding curve (using virtual reserves)
/// and PumpSwap pool vaults.
pub fn simulate_sell_price_sol(
    reserve_base_raw: u64,
    reserve_quote_lamports: u64,
    tokens_in_raw: u64,
) -> f64 {
    if tokens_in_raw == 0 || reserve_base_raw == 0 || reserve_quote_lamports == 0 {
        return 0.0;
    }
    let base = reserve_base_raw as u128;
    let quote = reserve_quote_lamports as u128;
    let dx = tokens_in_raw as u128;

    // x*y=k:  new_quote = (base * quote) / (base + dx)
    let new_base = match base.checked_add(dx) {
        Some(v) => v,
        None => return 0.0,
    };
    let new_quote = base.saturating_mul(quote) / new_base;
    let sol_out_lamports = quote.saturating_sub(new_quote);

    if sol_out_lamports == 0 {
        return 0.0;
    }

    // SOL per display token:
    // (sol_out_lamports / 1e9) / (tokens_in_raw / 1e6) = (sol_out / tokens_in) * 1e-3
    let sol_out = sol_out_lamports as f64 / 1_000_000_000.0;
    let display_tokens = tokens_in_raw as f64 / 1_000_000.0;
    sol_out / display_tokens
}

/// Like `fetch_current_price_sol`, but returns the effective per-display-token price
/// you'd actually receive if you sold `tokens_in_raw` tokens *right now* — i.e., the
/// simulated AMM output, not the marginal spot quote.
///
/// Use this for exit-condition PnL on positions large enough to move the price.
/// For tiny test trades or strategy discovery, prefer `fetch_current_price_sol`.
pub async fn fetch_realized_price_sol(
    rpc_client: &AsyncRpcClient,
    token_address: &str,
    tokens_in_raw: u64,
    cached_pool: Option<&PumpSwapPoolInfo>,
) -> Result<(f64, Option<PumpSwapPoolInfo>)> {
    // --- Source 1: Active PumpFun bonding curve ---
    let mint = Pubkey::from_str(token_address)
        .map_err(|e| BotError::InvalidTokenAddress(format!("Invalid mint: {}", e)))?;
    if let Ok(pda) = derive_bonding_curve_pda(&mint) {
        if let Ok(Ok(account)) = tokio::time::timeout(
            Duration::from_millis(500),
            rpc_client.get_account(&pda),
        )
        .await
        {
            let data = &account.data;
            // Layout: disc(8) vtok(8) vsol(8) ... complete@48
            if data.len() >= 49 && data[48] == 0 {
                let vtok = u64::from_le_bytes(data[8..16].try_into().unwrap());
                let vsol = u64::from_le_bytes(data[16..24].try_into().unwrap());
                if vtok > 0 && vsol > 0 {
                    let price = simulate_sell_price_sol(vtok, vsol, tokens_in_raw);
                    if price > 0.0 {
                        return Ok((price, None));
                    }
                }
            }
        }
    }

    // --- Source 2: PumpSwap pool ---
    let pool_info = match cached_pool {
        Some(info) => info.clone(),
        None => {
            log::info!(
                "Discovering PumpSwap pool for {} (first time, realized-price)",
                token_address
            );
            find_pumpswap_pool(rpc_client, token_address).await?
        }
    };

    let accounts = match tokio::time::timeout(
        Duration::from_millis(500),
        rpc_client.get_multiple_accounts(&[pool_info.base_vault, pool_info.quote_vault]),
    )
    .await
    {
        Ok(Ok(accs)) => accs,
        Ok(Err(e)) => {
            return Err(BotError::PumpPortal(format!(
                "PumpSwap vault read failed: {}",
                e
            )));
        }
        Err(_) => {
            return Err(BotError::PumpPortal(
                "PumpSwap vault read timed out (500ms)".to_string(),
            ));
        }
    };

    let base_account = accounts[0].as_ref().ok_or_else(|| {
        BotError::PumpPortal(format!("PumpSwap base vault {} missing", pool_info.base_vault))
    })?;
    let quote_account = accounts[1].as_ref().ok_or_else(|| {
        BotError::PumpPortal(format!("PumpSwap quote vault {} missing", pool_info.quote_vault))
    })?;
    if base_account.data.len() < 72 || quote_account.data.len() < 72 {
        return Err(BotError::PumpPortal(
            "PumpSwap vault data too short".to_string(),
        ));
    }
    let base_amount = u64::from_le_bytes(base_account.data[64..72].try_into().unwrap());
    let quote_amount = u64::from_le_bytes(quote_account.data[64..72].try_into().unwrap());

    let price = simulate_sell_price_sol(base_amount, quote_amount, tokens_in_raw);
    if price <= 0.0 {
        return Err(BotError::PumpPortal(format!(
            "PumpSwap simulate_sell returned non-positive price (base={}, quote={}, in={})",
            base_amount, quote_amount, tokens_in_raw
        )));
    }
    Ok((price, Some(pool_info)))
}

/// Fetch the current SOL-denominated price of a token using on-chain data only.
/// 1. Bonding curve RPC read (active PumpFun tokens)
/// 2. PumpSwap pool vaults (graduated tokens)
///
/// Returns (sol_price, Some(pool_info)) if PumpSwap was used, (sol_price, None) if bonding curve.
/// The caller should cache the pool_info to avoid repeated discovery.
pub async fn fetch_current_price_sol(
    rpc_client: &AsyncRpcClient,
    token_address: &str,
    cached_pool: Option<&PumpSwapPoolInfo>,
) -> Result<(f64, Option<PumpSwapPoolInfo>)> {
    // --- Source 1: Direct bonding curve RPC (real-time, for active PumpFun tokens) ---
    match fetch_price_bonding_curve_rpc(rpc_client, token_address).await {
        Ok(price) => {
            return Ok((price, None));
        }
        Err(e) => {
            log::debug!(
                "Bonding curve unavailable for {}: {}, trying PumpSwap",
                token_address,
                e
            );
        }
    }

    // --- Source 2: PumpSwap pool (graduated tokens) ---
    let pool_info = match cached_pool {
        Some(info) => info.clone(),
        None => {
            log::info!(
                "Discovering PumpSwap pool for {} (first time)",
                token_address
            );
            find_pumpswap_pool(rpc_client, token_address).await?
        }
    };

    let price = fetch_price_pumpswap_sol(rpc_client, &pool_info, token_address).await?;
    Ok((price, Some(pool_info)))
}

/// Fetch trending token addresses from Birdeye's free-tier API.
/// Paginates in batches of 20 up to `limit` total.
/// Returns Vec of token mint addresses.
pub async fn fetch_birdeye_trending(api_key: &str, limit: u32) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| BotError::Http(e))?;

    let mut addresses = Vec::new();
    let mut offset: u32 = 0;
    let page_size: u32 = 20; // Birdeye max per request

    while (addresses.len() as u32) < limit {
        let url = format!(
            "https://public-api.birdeye.so/defi/token_trending?sort_by=rank&sort_type=asc&offset={}&limit={}",
            offset, page_size
        );

        let resp = client
            .get(&url)
            .header("accept", "application/json")
            .header("x-chain", "solana")
            .header("X-API-KEY", api_key)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                match r.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if let Some(tokens) = data["data"]["tokens"].as_array() {
                            if tokens.is_empty() {
                                break; // No more results
                            }
                            for token in tokens {
                                if let Some(addr) = token["address"].as_str() {
                                    addresses.push(addr.to_string());
                                }
                            }
                        } else {
                            log::warn!("Birdeye response missing data.tokens array");
                            break;
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to parse Birdeye response: {}", e);
                        break;
                    }
                }
            }
            Ok(r) => {
                log::warn!("Birdeye HTTP error: {}", r.status());
                break;
            }
            Err(e) => {
                log::warn!("Birdeye request failed: {}", e);
                break;
            }
        }

        offset += page_size;

        // Rate-limit between pagination requests
        if (addresses.len() as u32) < limit {
            sleep(Duration::from_millis(200)).await;
        }
    }

    // Truncate to exact limit
    addresses.truncate(limit as usize);
    Ok(addresses)
}

/// Parse a DexScreener pair JSON object into TokenInfo without requiring a NewTokenEvent.
/// Used for the Birdeye trending → DexScreener enrichment flow.
fn parse_dexscreener_pair_standalone(pair: &serde_json::Value) -> Option<TokenInfo> {
    let address = pair["baseToken"]["address"].as_str()?.to_string();
    let symbol = pair["baseToken"]["symbol"]
        .as_str()
        .unwrap_or(&address[..8.min(address.len())])
        .to_string();
    let name = pair["baseToken"]["name"]
        .as_str()
        .unwrap_or("Unknown")
        .to_string();

    let price_usd = pair["priceUsd"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let volume_24h = pair["volume"]["h24"].as_f64().unwrap_or(0.0) as u64;
    let liquidity = pair["liquidity"]["usd"].as_f64().unwrap_or(1.0).max(1.0) as u64;
    let market_cap = pair["marketCap"]
        .as_f64()
        .or_else(|| pair["fdv"].as_f64())
        .unwrap_or(0.0) as u64;
    let price_change_24h = pair["priceChange"]["h24"].as_f64().unwrap_or(0.0);

    // Estimate holders from 24h transaction delta (best available without on-chain query)
    let buys = pair["txns"]["h24"]["buys"].as_u64().unwrap_or(0);
    let sells = pair["txns"]["h24"]["sells"].as_u64().unwrap_or(0);
    let holders = buys.saturating_sub(sells).max(1) as u32;

    // Calculate age from pairCreatedAt (millisecond unix timestamp)
    let age_hours = pair["pairCreatedAt"]
        .as_i64()
        .map(|created_ms| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            ((now_ms - created_ms) / 3_600_000).max(0) as u32
        })
        .unwrap_or(0);

    Some(TokenInfo {
        address,
        symbol,
        name,
        decimals: 6,
        market_cap,
        holders,
        age_hours,
        liquidity,
        price_usd,
        price_change_24h,
        volume_24h,
        created_at: chrono::Utc::now().to_rfc3339(),
        just_graduated_at_secs: None,
    })
}

/// Fetch enriched TokenInfo for multiple tokens from DexScreener in batches of 30.
/// Deduplicates by base token address (keeps first/highest-liquidity pair per token).
pub async fn enrich_tokens_dexscreener(addresses: &[String]) -> Result<Vec<TokenInfo>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| BotError::Http(e))?;

    let mut tokens: Vec<TokenInfo> = Vec::new();
    let mut seen_addresses = std::collections::HashSet::new();

    for chunk in addresses.chunks(30) {
        let joined = chunk.join(",");
        let url = format!("https://api.dexscreener.com/tokens/v1/solana/{}", joined);

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        // Response is a JSON array of pair objects
                        if let Some(pairs) = data.as_array() {
                            for pair in pairs {
                                if let Some(token) = parse_dexscreener_pair_standalone(pair) {
                                    // Deduplicate: keep first pair per token address
                                    if seen_addresses.insert(token.address.clone()) {
                                        tokens.push(token);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to parse DexScreener batch response: {}", e);
                    }
                }
            }
            Ok(resp) => {
                log::warn!("DexScreener batch HTTP error: {}", resp.status());
            }
            Err(e) => {
                log::warn!("DexScreener batch request failed: {}", e);
            }
        }

        // Rate-limit between batch requests (300ms)
        if addresses.len() > 30 {
            sleep(Duration::from_millis(300)).await;
        }
    }

    Ok(tokens)
}

/// Fetch trending Solana token addresses from GeckoTerminal (free, no API key).
/// Paginates up to `max_pages` pages (20 pools per page).
/// Returns deduplicated Vec of token mint addresses.
pub async fn fetch_geckoterminal_trending(max_pages: u32) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| BotError::Http(e))?;

    let mut seen = std::collections::HashSet::new();
    let mut addresses = Vec::new();

    for page in 1..=max_pages {
        let url = format!(
            "https://api.geckoterminal.com/api/v2/networks/solana/trending_pools?page={}",
            page
        );

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if let Some(pools) = data["data"].as_array() {
                            if pools.is_empty() {
                                break;
                            }
                            for pool in pools {
                                if let Some(token_id) =
                                    pool["relationships"]["base_token"]["data"]["id"].as_str()
                                {
                                    let mint = token_id
                                        .strip_prefix("solana_")
                                        .unwrap_or(token_id)
                                        .to_string();
                                    if seen.insert(mint.clone()) {
                                        addresses.push(mint);
                                    }
                                }
                            }
                        } else {
                            log::warn!(
                                "GeckoTerminal response missing data array on page {}",
                                page
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to parse GeckoTerminal response page {}: {}",
                            page,
                            e
                        );
                        break;
                    }
                }
            }
            Ok(resp) => {
                log::warn!(
                    "GeckoTerminal HTTP error on page {}: {}",
                    page,
                    resp.status()
                );
                break;
            }
            Err(e) => {
                log::warn!("GeckoTerminal request failed on page {}: {}", page, e);
                break;
            }
        }

        // Rate-limit between page requests
        if page < max_pages {
            sleep(Duration::from_millis(300)).await;
        }
    }

    Ok(addresses)
}

// ---------------------------------------------------------------------------
// PumpPortal new-token WebSocket listener
// ---------------------------------------------------------------------------

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};

/// Event received from PumpPortal WebSocket when a new token is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTokenEvent {
    pub mint: String,
    #[serde(default, rename = "bondingCurveKey")]
    pub bonding_curve_key: String,
    #[serde(default, rename = "marketCapSol")]
    pub market_cap_sol: f64,
}

/// Event received from PumpPortal WebSocket when a token graduates from PumpFun
/// to PumpSwap. PumpPortal's subscribeMigration channel emits one of these per migration.
/// Field names follow PumpPortal's payload; the `mint` is the only field we strictly need.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationEvent {
    pub mint: String,
    #[serde(default, rename = "txType")]
    pub tx_type: String,
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub pool: String,
}

/// Connect to PumpPortal WebSocket and stream PumpFun → PumpSwap migration events.
/// Reconnects automatically with exponential backoff. Mirrors `listen_new_tokens`
/// but subscribes to the `subscribeMigration` channel.
pub async fn listen_migrations(
    tx: tokio::sync::mpsc::UnboundedSender<MigrationEvent>,
) {
    let base = std::env::var("PUMPPORTAL_API_URL")
        .unwrap_or_else(|_| "https://pumpportal.fun".to_string());
    let base = base.trim_end_matches('/');
    let ws_base = base.replace("https://", "wss://").replace("http://", "ws://");
    let url = format!("{}/api/data", ws_base);
    let mut attempt: u32 = 0;

    loop {
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                log::info!("PumpPortal migration WS connected");
                attempt = 0;
                let (mut write, mut read) = ws_stream.split();

                let sub = serde_json::json!({ "method": "subscribeMigration" });
                if let Err(e) = write.send(WsMessage::Text(sub.to_string())).await {
                    log::error!("PumpPortal migration WS subscribe failed: {:?}", e);
                    continue;
                }

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(WsMessage::Text(payload)) => {
                            if let Ok(ev) = serde_json::from_str::<MigrationEvent>(&payload) {
                                if !ev.mint.is_empty() {
                                    if tx.send(ev).is_err() {
                                        log::error!("Migration channel closed");
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(WsMessage::Ping(p)) => {
                            let _ = write.send(WsMessage::Pong(p)).await;
                        }
                        Ok(WsMessage::Close(_)) => {
                            log::warn!("PumpPortal migration WS closed by server");
                            break;
                        }
                        Err(e) => {
                            log::error!("PumpPortal migration WS error: {:?}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                log::error!("PumpPortal migration WS connect failed: {:?}", e);
            }
        }

        let delay = Duration::from_secs(2u64.pow(attempt.min(4)));
        log::info!("PumpPortal migration WS reconnecting in {:?}...", delay);
        sleep(delay).await;
        attempt += 1;
    }
}

/// Connect to PumpPortal WebSocket and stream new token creation events.
/// Reconnects automatically on disconnect with exponential backoff.
pub async fn listen_new_tokens(
    tx: tokio::sync::mpsc::UnboundedSender<NewTokenEvent>,
) {
    // Derive WS URL from PUMPPORTAL_API_URL (default https://pumpportal.fun → wss://pumpportal.fun/api/data)
    let base = std::env::var("PUMPPORTAL_API_URL")
        .unwrap_or_else(|_| "https://pumpportal.fun".to_string());
    let base = base.trim_end_matches('/');
    let ws_base = base.replace("https://", "wss://").replace("http://", "ws://");
    let url = format!("{}/api/data", ws_base);
    let mut attempt: u32 = 0;

    loop {
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                log::info!("PumpPortal new-token WS connected");
                attempt = 0;
                let (mut write, mut read) = ws_stream.split();

                // Subscribe to new token events
                let sub = serde_json::json!({
                    "method": "subscribeNewToken"
                });
                if let Err(e) = write.send(WsMessage::Text(sub.to_string())).await {
                    log::error!("PumpPortal new-token WS subscribe failed: {:?}", e);
                    continue;
                }

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(WsMessage::Text(payload)) => {
                            if let Ok(ev) = serde_json::from_str::<NewTokenEvent>(&payload) {
                                if !ev.mint.is_empty() {
                                    if tx.send(ev).is_err() {
                                        log::error!("New-token channel closed");
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(WsMessage::Ping(p)) => {
                            let _ = write.send(WsMessage::Pong(p)).await;
                        }
                        Ok(WsMessage::Close(_)) => {
                            log::warn!("PumpPortal new-token WS closed by server");
                            break;
                        }
                        Err(e) => {
                            log::error!("PumpPortal new-token WS error: {:?}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                log::error!("PumpPortal new-token WS connect failed: {:?}", e);
            }
        }

        // Exponential backoff: 2^attempt seconds, capped at 16s
        let delay = Duration::from_secs(2u64.pow(attempt.min(4)));
        log::info!("PumpPortal new-token WS reconnecting in {:?}...", delay);
        sleep(delay).await;
        attempt += 1;
    }
}

// ---------------------------------------------------------------------------
// Target wallet WebSocket listener (copy trading)
// ---------------------------------------------------------------------------

use solana_sdk::native_token::LAMPORTS_PER_SOL;

/// Parse logsSubscribe notification to extract tx signature
pub fn extract_signature_from_ws(msg: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(msg).ok()?;
    v.get("params")?
        .get("result")?
        .get("value")?
        .get("signature")?
        .as_str()
        .map(|s| s.to_string())
}

/// Parse confirmed transaction to detect a buy by target wallet.
/// Returns (target_wallet, token_mint, lamports_spent) if a buy was detected.
pub fn get_target_buy_info(
    signature: &str,
    rpc: &RpcClient,
    target_wallets: &[Pubkey],
) -> Result<Option<(String, String, u64)>> {
    use solana_transaction_status::UiTransactionEncoding;

    let sig_parsed = signature
        .parse()
        .map_err(|e| BotError::Trading(format!("Bad signature: {}", e)))?;

    let tx_with_meta = match rpc.get_transaction_with_config(
        &sig_parsed,
        solana_client::rpc_config::RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::JsonParsed),
            commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        },
    ) {
        Ok(tx) => tx,
        Err(_) => return Ok(None),
    };

    let meta = match tx_with_meta.transaction.meta {
        Some(ref m) => m,
        None => return Ok(None),
    };

    // Calculate SOL spent by the target wallet
    let lamports_spent = meta
        .pre_balances
        .first()
        .copied()
        .unwrap_or(0)
        .saturating_sub(meta.post_balances.first().copied().unwrap_or(0));

    if lamports_spent == 0 {
        return Ok(None);
    }

    // OptionSerializer -> Option via Into
    let pre_opt: Option<&Vec<_>> = meta.pre_token_balances.as_ref().into();
    let pre_token_balances = pre_opt.map(|v| v.as_slice()).unwrap_or(&[]);
    let post_opt: Option<&Vec<_>> = meta.post_token_balances.as_ref().into();
    let post_token_balances = post_opt.map(|v| v.as_slice()).unwrap_or(&[]);

    let target_wallets_strs: Vec<String> = target_wallets.iter().map(|k| k.to_string()).collect();

    for post in post_token_balances {
        let owner_opt: Option<&String> = post.owner.as_ref().into();
        let owner_str = match owner_opt {
            Some(o) => o.as_str(),
            None => continue,
        };

        if !target_wallets_strs
            .iter()
            .any(|tw| tw.as_str() == owner_str)
        {
            continue;
        }

        let pre_balance = pre_token_balances
            .iter()
            .find(|b| {
                if b.mint != post.mint {
                    return false;
                }
                let pre_owner_opt: Option<&String> = b.owner.as_ref().into();
                pre_owner_opt
                    .map(|o| o.as_str() == owner_str)
                    .unwrap_or(false)
            })
            .map(|b| b.ui_token_amount.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);

        let post_balance = post.ui_token_amount.amount.parse::<u64>().unwrap_or(0);

        if post_balance > pre_balance {
            // Skip SOL_MINT
            if post.mint == "So11111111111111111111111111111111111111112" {
                continue;
            }
            return Ok(Some((
                owner_str.to_string(),
                post.mint.clone(),
                lamports_spent,
            )));
        }
    }

    Ok(None)
}

/// Max subscriptions per WebSocket connection. Most Solana RPC providers
/// (including QuickNode) silently drop subscriptions beyond their per-connection
/// limit (typically 10-20). We split wallets across multiple connections to
/// ensure all wallets are actually monitored.
const MAX_SUBS_PER_WS: usize = 10;

/// Run a single WebSocket connection monitoring a batch of wallets.
/// Reconnects automatically on disconnect with exponential backoff.
async fn listen_wallet_batch(
    wss_url: String,
    batch: Vec<Pubkey>,
    batch_id: usize,
    all_wallets: Vec<Pubkey>,
    rpc: std::sync::Arc<RpcClient>,
    mint_tx: tokio::sync::mpsc::UnboundedSender<(String, Option<String>)>,
) {
    let mut attempt: u32 = 0;
    loop {
        match connect_async(&wss_url).await {
            Ok((ws_stream, _)) => {
                log::info!(
                    "WS batch {} connected — monitoring {} wallets",
                    batch_id,
                    batch.len()
                );
                attempt = 0;
                let (mut write, mut read) = ws_stream.split();

                for (idx, wallet) in batch.iter().enumerate() {
                    let sub_req = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": idx + 1,
                        "method": "logsSubscribe",
                        "params": [
                            { "mentions": [wallet.to_string()] },
                            { "commitment": "confirmed" }
                        ]
                    });

                    if let Err(e) = write.send(WsMessage::Text(sub_req.to_string())).await {
                        log::error!("WS batch {} sub failed: {:?}", batch_id, e);
                        break;
                    }
                }

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(WsMessage::Text(payload)) => {
                            if let Some(signature) = extract_signature_from_ws(&payload) {
                                let rpc_clone = rpc.clone();
                                let tw = all_wallets.clone();
                                let tx_clone = mint_tx.clone();

                                tokio::task::spawn_blocking(move || {
                                    match get_target_buy_info(&signature, rpc_clone.as_ref(), &tw) {
                                        Ok(Some((target_wallet, token_mint, lamports_spent))) => {
                                            log::info!(
                                                "Target {} buy: {} lamports ({:.4} SOL) -> {}",
                                                target_wallet,
                                                lamports_spent,
                                                lamports_spent as f64 / LAMPORTS_PER_SOL as f64,
                                                token_mint
                                            );
                                            let _ = tx_clone.send((
                                                token_mint,
                                                Some(target_wallet.to_string()),
                                            ));
                                        }
                                        Ok(None) => {}
                                        Err(e) => {
                                            log::debug!("Parse target tx failed: {}", e);
                                        }
                                    }
                                });
                            }
                        }
                        Ok(WsMessage::Ping(p)) => {
                            if let Err(e) = write.send(WsMessage::Pong(p)).await {
                                log::warn!("WS batch {} pong failed: {:?}", batch_id, e);
                                break;
                            }
                        }
                        Ok(WsMessage::Pong(_)) => {}
                        Ok(WsMessage::Close(frame)) => {
                            log::warn!("WS batch {} closed: {:?}", batch_id, frame);
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::error!("WS batch {} error: {:?}", batch_id, e);
                            break;
                        }
                    }
                }
                log::warn!("WS batch {} disconnected, reconnecting...", batch_id);
            }
            Err(e) => {
                log::error!("WS batch {} connect failed: {:?}", batch_id, e);
            }
        }

        let delay = 2u64.pow(attempt.min(4));
        log::info!(
            "WS batch {} reconnecting in {}s (attempt {})...",
            batch_id,
            delay,
            attempt + 1
        );
        tokio::time::sleep(Duration::from_secs(delay)).await;
        attempt += 1;
    }
}

/// Main WebSocket listener for target wallets. Splits wallets across multiple
/// connections (MAX_SUBS_PER_WS each) to avoid RPC subscription limits.
/// Each batch runs in its own tokio task with independent reconnect logic.
pub async fn listen_target_wallets(
    wss_url: String,
    target_wallets: Vec<Pubkey>,
    rpc: std::sync::Arc<RpcClient>,
    mint_tx: tokio::sync::mpsc::UnboundedSender<(String, Option<String>)>,
) -> Result<()> {
    let batches: Vec<Vec<Pubkey>> = target_wallets
        .chunks(MAX_SUBS_PER_WS)
        .map(|chunk| chunk.to_vec())
        .collect();

    log::info!(
        "Splitting {} wallets across {} WebSocket connections ({} per connection)",
        target_wallets.len(),
        batches.len(),
        MAX_SUBS_PER_WS
    );

    let mut handles = Vec::new();

    for (batch_id, batch) in batches.into_iter().enumerate() {
        let wss = wss_url.clone();
        let all = target_wallets.clone();
        let r = rpc.clone();
        let tx = mint_tx.clone();

        let handle = tokio::spawn(async move {
            listen_wallet_batch(wss, batch, batch_id, all, r, tx).await;
        });
        handles.push(handle);
    }

    // Wait for all batch listeners (they run forever unless all crash)
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}
