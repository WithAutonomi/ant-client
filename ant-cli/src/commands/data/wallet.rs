use ant_core::data::Wallet;
use clap::Subcommand;

/// Wallet subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum WalletAction {
    /// Show wallet address.
    Address,
    /// Show wallet balance.
    Balance,
}

impl WalletAction {
    pub async fn execute(self, wallet: Wallet) -> anyhow::Result<()> {
        match self {
            WalletAction::Address => {
                let address = wallet.address();
                println!("{address:?}");
            }
            WalletAction::Balance => {
                let balance = wallet
                    .balance_of_tokens()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to get balance: {e}"))?;
                println!("{balance}");
            }
        }
        Ok(())
    }
}
