use async_trait::async_trait;
use fuel_crypto::{Message, Signature};
use fuels::{
    prelude::*,
    types::{coin_type_id::CoinTypeId, input::Input},
};
use fuels_accounts::{wallet::WalletUnlocked, Account};

#[derive(Clone, Debug)]
pub enum ForcClientAccount {
    Wallet(WalletUnlocked),
    KmsSigner,
}

#[async_trait]
impl Account for ForcClientAccount {
    async fn get_asset_inputs_for_amount(
        &self,
        asset_id: AssetId,
        amount: u64,
        excluded_coins: Option<Vec<CoinTypeId>>,
    ) -> Result<Vec<Input>> {
        match self {
            ForcClientAccount::Wallet(wallet) => {
                wallet
                    .get_asset_inputs_for_amount(asset_id, amount, excluded_coins)
                    .await
            }
            ForcClientAccount::KmsSigner => todo!(),
        }
    }
}

impl ViewOnlyAccount for ForcClientAccount {
    fn address(&self) -> &Bech32Address {
        match self {
            ForcClientAccount::Wallet(wallet) => wallet.address(),
            ForcClientAccount::KmsSigner => todo!(),
        }
    }

    fn try_provider(&self) -> Result<&Provider> {
        match self {
            ForcClientAccount::Wallet(wallet) => wallet.try_provider(),
            ForcClientAccount::KmsSigner => todo!(),
        }
    }
}

#[async_trait]
impl Signer for ForcClientAccount {
    async fn sign(&self, message: Message) -> Result<Signature> {
        match self {
            ForcClientAccount::Wallet(wallet) => wallet.sign(message).await,
            ForcClientAccount::KmsSigner => todo!(),
        }
    }

    fn address(&self) -> &Bech32Address {
        match self {
            ForcClientAccount::Wallet(wallet) => wallet.address(),
            ForcClientAccount::KmsSigner => todo!(),
        }
    }
}
