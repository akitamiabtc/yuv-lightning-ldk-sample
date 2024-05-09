use crate::disk::FilesystemLogger;
use bdk::wallet::AddressIndex;
use bdk::SignOptions;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Script, Transaction};
use eyre::Context;
use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::log_error;
use lightning::util::logger::Logger;
use std::collections::HashMap;
use std::sync::Arc;
use ydk::wallet::WalletConfig as MemoryWalletConfig;
use ydk::wallet::{MemoryWallet, SyncOptions};
use yuv_pixels::{Chroma, Pixel};
use yuv_types::YuvTransaction;

#[derive(Clone)]
pub(crate) struct Wallet {
	ydk_wallet: MemoryWallet,
	logger: Arc<FilesystemLogger>,
}

pub const DUMMY_YUV_URL: &str = "http://localhost:8080";

impl Wallet {
	pub async fn from_config(
		mut config: MemoryWalletConfig, logger: Arc<FilesystemLogger>,
	) -> eyre::Result<Self> {
		let sync_yuv_wallet = !config.yuv_url.is_empty();

		// In case we won't create connection to the YUV server, we need to set
		// a dummy URL as it will be parsed, and we can't pass an empty string.
		config.yuv_url = if !sync_yuv_wallet { DUMMY_YUV_URL.to_string() } else { config.yuv_url };

		let ydk_wallet =
			ydk::Wallet::from_config(config).await.wrap_err("failed to initialize wallet")?;

		let options = SyncOptions { sync_yuv_wallet, ..Default::default() };

		ydk_wallet.sync(options).await.wrap_err("failed to sync wallet")?;

		Ok(Self { ydk_wallet, logger })
	}
}

#[allow(dead_code)]
impl Wallet {
	pub fn new_wallet_source(&self) -> Self {
		self.clone()
	}

	pub async fn new_yuv_funding_tx(
		&mut self, funding_pixel: Pixel, funding_holder_pubkey: PublicKey,
		funding_counterparty_pubkey: PublicKey, channel_value_satoshis: u64,
	) -> eyre::Result<YuvTransaction> {
		self.ydk_wallet.sync(SyncOptions::default()).await.wrap_err("failed to sync wallet")?;

		self.ydk_wallet
			.lightning_funding_tx(
				funding_pixel,
				funding_holder_pubkey,
				funding_counterparty_pubkey,
				channel_value_satoshis,
				None,
			)
			.await
	}

	pub fn new_funding_tx(
		&self, output_script: Script, channel_value_satoshis: u64,
	) -> eyre::Result<Transaction> {
		// SAFETY: it's okay as we are not accessing the wallet's DB directly as
		// suggested by the note in ydk.
		let bdk_wallet = unsafe { self.ydk_wallet.bitcoin_wallet() };
		let bdk_wallet_guard = bdk_wallet.read().unwrap();

		let mut tx_builder = bdk_wallet_guard.build_tx();
		tx_builder.add_recipient(output_script, channel_value_satoshis);

		let (mut psbt, _tx_details) = tx_builder.finish().wrap_err("failed to build funding tx")?;

		bdk_wallet_guard
			.sign(&mut psbt, SignOptions { trust_witness_utxo: true, ..Default::default() })
			.wrap_err("failed to sign funding tx")?;

		Ok(psbt.extract_tx())
	}

	pub async fn get_yuv_balances(&self) -> eyre::Result<HashMap<Chroma, u128>> {
		self.ydk_wallet.sync(SyncOptions::default()).await.wrap_err("failed to sync ydk wallet")?;

		Ok(self.ydk_wallet.balances())
	}

	pub async fn new_yuv_transfer(
		&self, recepient: PublicKey, chroma: Chroma, amount: u128,
	) -> eyre::Result<YuvTransaction> {
		self.ydk_wallet.sync(SyncOptions::default()).await.wrap_err("failed to sync ydk wallet")?;

		self.ydk_wallet.create_transfer(Pixel::new(amount, chroma), recepient, None).await
	}

	pub fn public_key(&self) -> PublicKey {
		self.ydk_wallet.public_key().inner
	}
}

impl WalletSource for Wallet {
	fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
		let bdk_wallet = unsafe { self.ydk_wallet.bitcoin_wallet() };
		let bdk_wallet_guard = bdk_wallet.read().unwrap();

		let utxos = bdk_wallet_guard.list_unspent().map_err(|err| {
			log_error!(&self.logger, "Failed to get list unspent utxos: {err}");
		})?;

		let ldk_utxos = utxos
			.into_iter()
			.map(|utxo| {
				Utxo {
					outpoint: utxo.outpoint,
					output: utxo.txout,
					satisfaction_weight: WITNESS_SCALE_FACTOR as u64 +
                        1 /* witness items */ + 1 /* schnorr sig len */ + 64,
				}
			})
			.collect();

		Ok(ldk_utxos)
	}

	fn get_change_script(&self) -> Result<Script, ()> {
		let bdk_wallet = unsafe { self.ydk_wallet.bitcoin_wallet() };
		let bdk_wallet_guard = bdk_wallet.read().unwrap();

		let address_info = bdk_wallet_guard.get_address(AddressIndex::Peek(0)).map_err(|err| {
			log_error!(&self.logger, "Failed to get address indo: {err}");
		})?;

		Ok(address_info.script_pubkey())
	}

	fn sign_tx(&self, tx: Transaction) -> Result<Transaction, ()> {
		let mut psbt = PartiallySignedTransaction::from_unsigned_tx(tx).map_err(|err| {
			log_error!(&self.logger, "Failed to get psbt from tx: {err}");
		})?;

		let bdk_wallet = unsafe { self.ydk_wallet.bitcoin_wallet() };
		let bdk_wallet_guard = bdk_wallet.read().unwrap();

		bdk_wallet_guard.sign(&mut psbt, SignOptions::default()).map_err(|err| {
			log_error!(&self.logger, "Failed to sign psbt: {err}");
		})?;

		Ok(psbt.extract_tx())
	}

	fn get_change_yuv_pubkey(&self) -> Result<PublicKey, ()> {
       	Ok(self.ydk_wallet.public_key().inner)
    }
}
