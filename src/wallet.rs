use tonlib_core::wallet::mnemonic;
use tonlib_core::wallet::ton_wallet::TonWallet;
use tonlib_core::wallet::wallet_version::WalletVersion;

const DEFAULT_WALLET_ID_V5R1_TESTNET: i32 = 0x7FFFFFFD;

#[derive(Clone, Debug)]
pub struct Wallet {
    pub wallet: TonWallet,
}

impl Wallet {
    pub fn new(mnemonic_str: &str) -> anyhow::Result<Self> {
        let mnemonic = mnemonic::Mnemonic::from_str(mnemonic_str, &None)?;

        let wallet = TonWallet::new_with_params(
            WalletVersion::V5R1,
            mnemonic.to_key_pair()?,
            0,
            DEFAULT_WALLET_ID_V5R1_TESTNET,
        )?;

        Ok(Self { wallet })
    }
}
