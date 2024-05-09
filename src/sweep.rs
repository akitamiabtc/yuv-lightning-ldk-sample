use crate::hex_utils;
use crate::wallet::Wallet;
use crate::yuv_client::YuvClient;
use crate::BitcoindClient;
use crate::ChannelManager;
use crate::FilesystemLogger;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{LockTime, PackedLockTime};
use bitcoin_client::RawTx;
use lightning::chain::chaininterface::{
	BroadcasterInterface, ConfirmationTarget, FeeEstimator, YuvBroadcaster,
};
use lightning::events::bump_transaction::WalletSource;
use lightning::log_info;
use lightning::sign::{EntropySource, KeysManager, SpendableOutputDescriptor};
use lightning::util::logger::Logger;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, WithoutLength, Writeable};
use lightning_persister::fs_store::FilesystemStore;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};

/// If we have any pending claimable outputs, we should slowly sweep them to our Bitcoin Core
/// wallet. We technically don't need to do this - they're ours to spend when we want and can just
/// use them to build new transactions instead, but we cannot feed them direclty into Bitcoin
/// Core's wallet so we have to sweep.
///
/// Note that this is unececssary for [`SpendableOutputDescriptor::StaticOutput`]s, which *do* have
/// an associated secret key we could simply import into Bitcoin Core's wallet, but for consistency
/// we don't do that here either.
pub(crate) async fn periodic_sweep(
	ldk_data_dir: String, keys_manager: Arc<KeysManager>, logger: Arc<FilesystemLogger>,
	persister: Arc<FilesystemStore>, wallet: Arc<tokio::sync::Mutex<Wallet>>,
	yuv_client: Option<Arc<YuvClient>>, bitcoind_client: Arc<BitcoindClient>,
	channel_manager: Arc<ChannelManager>,
) {
	// Regularly claim outputs which are exclusively spendable by us and send them to Bitcoin Core.
	// Note that if you more tightly integrate your wallet with LDK you may not need to do this -
	// these outputs can just be treated as normal outputs during coin selection.
	let pending_spendables_dir =
		format!("{}/{}", ldk_data_dir, crate::PENDING_SPENDABLE_OUTPUT_DIR);
	let processing_spendables_dir = format!("{}/processing_spendable_outputs", ldk_data_dir);
	let spendables_dir = format!("{}/spendable_outputs", ldk_data_dir);

	// We batch together claims of all spendable outputs generated each day, however only after
	// batching any claims of spendable outputs which were generated prior to restart. On a mobile
	// device we likely won't ever be online for more than a minute, so we have to ensure we sweep
	// any pending claims on startup, but for an always-online node you may wish to sweep even less
	// frequently than this (or move the interval await to the top of the loop)!
	//
	// There is no particular rush here, we just have to ensure funds are availably by the time we
	// need to send funds.
	let mut interval = tokio::time::interval(Duration::from_secs(5));

	loop {
		interval.tick().await; // Note that the first tick completes immediately
		if let Ok(dir_iter) = fs::read_dir(&pending_spendables_dir) {
			// Move any spendable descriptors from pending folder so that we don't have any
			// races with new files being added.
			for file_res in dir_iter {
				let file = file_res.unwrap();
				// Only move a file if its a 32-byte-hex'd filename, otherwise it might be a
				// temporary file.
				if file.file_name().len() == 64 {
					fs::create_dir_all(&processing_spendables_dir).unwrap();
					let mut holding_path = PathBuf::new();
					holding_path.push(&processing_spendables_dir);
					holding_path.push(&file.file_name());
					fs::rename(file.path(), holding_path).unwrap();
				}
			}
			// Now concatenate all the pending files we moved into one file in the
			// `spendable_outputs` directory and drop the processing directory.
			let mut outputs = Vec::new();
			if let Ok(processing_iter) = fs::read_dir(&processing_spendables_dir) {
				for file_res in processing_iter {
					outputs.append(&mut fs::read(file_res.unwrap().path()).unwrap());
				}
			}
			if !outputs.is_empty() {
				let key = hex_utils::hex_str(&keys_manager.get_secure_random_bytes());
				persister
					.write("spendable_outputs", "", &key, &WithoutLength(&outputs).encode())
					.unwrap();
				fs::remove_dir_all(&processing_spendables_dir).unwrap();
			}
		}
		// Iterate over all the sets of spendable outputs in `spendables_dir` and try to claim
		// them.
		// Note that here we try to claim each set of spendable outputs over and over again
		// forever, even long after its been claimed. While this isn't an issue per se, in practice
		// you may wish to track when the claiming transaction has confirmed and remove the
		// spendable outputs set. You may also wish to merge groups of unspent spendable outputs to
		// combine batches.
		if let Ok(dir_iter) = fs::read_dir(&spendables_dir) {
			for file_res in dir_iter {
				let mut outputs: Vec<SpendableOutputDescriptor> = Vec::new();
				let mut file = fs::File::open(file_res.unwrap().path()).unwrap();
				loop {
					// Check if there are any bytes left to read, and if so read a descriptor.
					match file.read_exact(&mut [0; 1]) {
						Ok(_) => {
							file.seek(SeekFrom::Current(-1)).unwrap();
						}
						Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
						Err(e) => Err(e).unwrap(),
					}
					outputs.push(Readable::read(&mut file).unwrap());
				}

				let wallet = wallet.lock().await;
				let Ok(destination_pubkey) = wallet.get_change_yuv_pubkey() else {
					lightning::log_error!(logger, "Failed to get change YUV pubkey");
					continue;
				};
				let output_descriptors = &outputs.iter().collect::<Vec<_>>();
				let tx_feerate =
					bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Background);

				// We set nLockTime to the current height to discourage fee sniping.
				let cur_height = channel_manager.current_best_block().height();
				let locktime: PackedLockTime =
					LockTime::from_height(cur_height).map_or(PackedLockTime::ZERO, |l| l.into());

				match keys_manager.spend_yuv_spendable_outputs(
					output_descriptors,
					tx_feerate,
					destination_pubkey,
					wallet.public_key(),
					Some(locktime),
					&Secp256k1::new(),
				) {
					Ok(yuv_spending_txs) => {
						// Note that, most likely, we've already sweeped this set of outputs
						// and they're already confirmed on-chain, so this broadcast will fail.
						for yuv_tx in yuv_spending_txs {
							let emulate_result = yuv_client
								.as_ref()
								.unwrap()
								.emulate_yuv_transaction(yuv_tx.clone())
								.await;

							if let Some(reason) = emulate_result {
								lightning::log_error!(
									logger,
									"Invalid spending YUV tx: {reason}; {:?}, proofs: {:?}",
									yuv_tx.bitcoin_tx.raw_hex(),
									yuv_tx.tx_type,
								);
								continue;
							}

							yuv_client
								.as_ref()
								.unwrap()
								.broadcast_transactions_proofs(yuv_tx.clone());

							bitcoind_client.broadcast_transactions(&[&yuv_tx.bitcoin_tx]);

							log_info!(
								logger,
								"Broadcasted YUV sweep tx: {}",
								yuv_tx.bitcoin_tx.txid()
							);
						}
					}
					Err(err) => {
						lightning::log_error!(
							logger,
							"Failed to sweep YUV spendable outputs: {}",
							err
						);
					}
				}
			}

			fs::remove_dir_all(&spendables_dir).unwrap();
		}
	}
}
