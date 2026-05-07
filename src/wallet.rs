use crate::error::{BotError, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Keypair, signer::Signer,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct WalletManager {
    keypair: Arc<Keypair>,
    rpc_client: Arc<RpcClient>,
    wallet_address: Pubkey,
}

impl WalletManager {
    pub fn new(keypair: Keypair, rpc_url: String) -> Self {
        let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        let wallet_address = keypair.pubkey();

        Self {
            keypair: Arc::new(keypair),
            rpc_client: Arc::new(rpc_client),
            wallet_address,
        }
    }

    pub fn get_address(&self) -> Pubkey {
        self.wallet_address
    }

    pub fn get_balance(&self) -> Result<u64> {
        self.rpc_client
            .get_balance(&self.wallet_address)
            .map_err(|e| BotError::SolanaClient(e))
    }

    pub fn get_token_balance(&self, token_mint: &Pubkey) -> Result<u64> {
        let token_accounts = self
            .rpc_client
            .get_token_accounts_by_owner(
                &self.wallet_address,
                solana_client::rpc_request::TokenAccountsFilter::Mint(*token_mint),
            )
            .map_err(|e| BotError::SolanaClient(e))?;

        if token_accounts.is_empty() {
            return Ok(0);
        }

        let token_account_pubkey: Pubkey = token_accounts[0]
            .pubkey
            .parse()
            .map_err(|e| BotError::Wallet(format!("Invalid token account pubkey: {}", e)))?;

        let account_info = self
            .rpc_client
            .get_token_account_balance(&token_account_pubkey)
            .map_err(|e| BotError::SolanaClient(e))?;

        Ok(account_info
            .amount
            .parse::<u64>()
            .map_err(|e| BotError::Wallet(format!("Failed to parse token balance: {}", e)))?)
    }

    pub fn get_keypair(&self) -> &Keypair {
        self.keypair.as_ref()
    }

    pub fn get_rpc_client(&self) -> &RpcClient {
        self.rpc_client.as_ref()
    }
}
