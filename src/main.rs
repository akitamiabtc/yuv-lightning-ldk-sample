mod args;
pub mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod hex_utils;
mod sweep;
mod wallet;
mod yuv_client;

use crate::bitcoind_client::BitcoindClient;
use crate::disk::FilesystemLogger;
use crate::wallet::Wallet;
use crate::yuv_client::YuvClient;
use bdk::blockchain::rpc::Auth;
use bdk::descriptor;
use bdk::wallet::wallet_name_from_descriptor;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{BlockHash, Network};
use disk::{INBOUND_PAYMENTS_FNAME, OUTBOUND_PAYMENTS_FNAME};
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus, YuvConfirm};
use lightning::chain::{Filter, Watch};
use lightning::events::bump_transaction::{BumpTransactionEventHandler, Wallet as LdkWallet};
use lightning::events::{Event, PaymentFailureReason, PaymentPurpose};
use lightning::ln::chan_utils::NewUpdateBalanceRequest;
use lightning::ln::channelmanager::RecentPaymentDetails;
use lightning::ln::channelmanager::{
	ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::msgs::DecodeError;
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::{ChannelId, PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::onion_message::messenger::{DefaultMessageRouter, SimpleArcOnionMessenger};
use lightning::routing::gossip;
use lightning::routing::gossip::{NodeId, P2PGossipSync};
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::sign::{EntropySource, InMemorySigner, KeysManager, SpendableOutputDescriptor};
use lightning::util::config::UserConfig;
use lightning::util::logger::Logger;
use lightning::util::persist::{self, KVStore, MonitorUpdatingPersister};
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};
use lightning::{chain, impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use lightning_background_processor::{process_events_async, GossipSync};
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use rand::{thread_rng, RngCore};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex as TokioMutex;
use ydk::bitcoin_provider::{BitcoinProviderConfig, BitcoinRpcConfig};
use ydk::wallet::WalletConfig;

use yuv_pixels::Pixel;

pub(crate) const PENDING_SPENDABLE_OUTPUT_DIR: &'static str = "pending_spendable_outputs";

#[derive(Copy, Clone)]
pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

impl_writeable_tlv_based_enum!(HTLCStatus,
	(0, Pending) => {},
	(1, Succeeded) => {},
	(2, Failed) => {};
);

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

impl Readable for MillisatAmount {
	fn read<R: io::Read>(r: &mut R) -> Result<Self, DecodeError> {
		let amt: Option<u64> = Readable::read(r)?;
		Ok(MillisatAmount(amt))
	}
}

impl Writeable for MillisatAmount {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), io::Error> {
		self.0.write(w)
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
	yuv_pixel: Option<Pixel>,
}

impl_writeable_tlv_based!(PaymentInfo, {
	(0, preimage, required),
	(2, secret, required),
	(4, status, required),
	(6, amt_msat, required),
	(7, yuv_pixel, option),
});

pub(crate) struct PaymentInfoStorage {
	payments: HashMap<PaymentHash, PaymentInfo>,
}

impl_writeable_tlv_based!(PaymentInfoStorage, {
	(0, payments, required),
});

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<YuvClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<
		MonitorUpdatingPersister<
			Arc<FilesystemStore>,
			Arc<FilesystemLogger>,
			Arc<KeysManager>,
			Arc<KeysManager>,
		>,
	>,
>;

pub(crate) type GossipVerifier = lightning_block_sync::gossip::GossipVerifier<
	lightning_block_sync::gossip::TokioSpawner,
	Arc<lightning_block_sync::rpc::RpcClient>,
	Arc<FilesystemLogger>,
	Arc<YuvClient>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	YuvClient,
	BitcoindClient,
	GossipVerifier,
	FilesystemLogger,
>;

pub(crate) type ChannelManager = SimpleArcChannelManager<
	ChainMonitor,
	BitcoindClient,
	YuvClient,
	BitcoindClient,
	FilesystemLogger,
>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

type OnionMessenger = SimpleArcOnionMessenger<
	ChainMonitor,
	BitcoindClient,
	YuvClient,
	BitcoindClient,
	FilesystemLogger,
>;

pub(crate) type BumpTxEventHandler = BumpTransactionEventHandler<
	Arc<BitcoindClient>,
	Arc<LdkWallet<Arc<Wallet>, Arc<FilesystemLogger>>>,
	Arc<KeysManager>,
	Arc<FilesystemLogger>,
>;

async fn handle_ldk_events(
	channel_manager: &Arc<ChannelManager>, network_graph: &NetworkGraph,
	keys_manager: &KeysManager, bump_tx_event_handler: &Arc<BumpTxEventHandler>,
	inbound_payments: Arc<Mutex<PaymentInfoStorage>>,
	outbound_payments: Arc<Mutex<PaymentInfoStorage>>, fs_store: &Arc<FilesystemStore>,
	event: Event, wallet: Arc<TokioMutex<Wallet>>, default_config: Arc<Mutex<UserConfig>>,
) {
	match event {
		Event::FundingGenerationReady {
			temporary_channel_id,
			counterparty_node_id,
			channel_value_satoshis,
			output_script,
			funding_yuv_pixel,
			funding_holder_pubkey,
			funding_counterparty_pubkey,
			..
		} => {
			let mut wallet = wallet.lock().await;

			let (final_tx, yuv_proofs) = if let Some(funding_pixel) = funding_yuv_pixel {
				let yuv_tx_res = wallet
					.new_yuv_funding_tx(
						funding_pixel,
						funding_holder_pubkey,
						funding_counterparty_pubkey,
						channel_value_satoshis,
					)
					.await;

				let yuv_tx = match yuv_tx_res {
					Ok(yuv) => yuv,
					Err(err) => {
						eprintln!("ERROR: Closing channel. Failed to create funding transaction: {err:#?}");

						if let Err(err) = channel_manager.force_close_without_broadcasting_txn(
							&temporary_channel_id,
							&counterparty_node_id,
						) {
							eprintln!("ERROR: failed to force close channel: {err:?}");
						}
						return;
					}
				};

				(yuv_tx.bitcoin_tx, Some(yuv_tx.tx_type))
			} else {
				let tx = wallet.new_funding_tx(output_script, channel_value_satoshis).unwrap();

				(tx, None)
			};

			// Give the funding transaction back to LDK for opening the channel.
			match channel_manager.funding_transaction_generated(
				&temporary_channel_id,
				&counterparty_node_id,
				final_tx,
				yuv_proofs,
			) {
				Ok(()) => {}
				Err(err) => {
					println!("\r\nERROR: {:?}", err);
					print!("\r> ");
					io::stdout().flush().unwrap();
				}
			}
		}
		Event::PaymentClaimable {
			payment_hash,
			purpose,
			amount_msat,
			receiver_node_id: _,
			via_channel_id,
			via_user_channel_id: _,
			claim_deadline: _,
			onion_fields: _,
			counterparty_skimmed_fee_msat: _,
			yuv_amount,
		} => {
			print!(
				"\rEVENT: received payment from payment hash {} of {} millisatoshis",
				payment_hash, amount_msat,
			);

			if let Some(yuv_amount) = yuv_amount {
				let channels = channel_manager.list_channels();
				let channel_details =
					channels.iter().find(|c| Some(c.channel_id) == via_channel_id).unwrap();

				println!(
					" and YUV {} {}",
					yuv_amount,
					channel_details.yuv_holder_pixel.unwrap().chroma
				);
			} else {
				println!(" and no YUV");
			};

			print!("\r> ");
			io::stdout().flush().unwrap();
			let payment_preimage = match purpose {
				PaymentPurpose::Bolt11InvoicePayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::SpontaneousPayment(preimage) => Some(preimage),
				_ => None,
			};
			channel_manager.claim_funds(payment_preimage.unwrap());
		}
		Event::PaymentClaimed {
			payment_hash,
			purpose,
			amount_msat,
			receiver_node_id: _,
			htlcs,
			sender_intended_total_msat: _,
			sender_intended_total_yuv,
		} => {
			print!(
				"\rEVENT: claimed payment from payment hash {} of {} millisatoshis",
				payment_hash, amount_msat,
			);
			if let Some(yuv_amount) = sender_intended_total_yuv {
				let channels = channel_manager.list_channels();
				let channel_details =
					channels.iter().find(|c| c.channel_id == htlcs[0].channel_id).unwrap();

				println!(
					" and YUV {} {}",
					yuv_amount,
					channel_details.yuv_holder_pixel.unwrap().chroma
				);
			} else {
				println!(" and no YUV");
			};
			print!("\r> ");
			io::stdout().flush().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::Bolt11InvoicePayment {
					payment_preimage, payment_secret, ..
				} => (payment_preimage, Some(payment_secret)),
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
				_ => (None, None),
			};
			let mut inbound = inbound_payments.lock().unwrap();
			match inbound.payments.entry(payment_hash) {
				Entry::Occupied(mut e) => {
					let payment = e.get_mut();
					payment.status = HTLCStatus::Succeeded;
					payment.preimage = payment_preimage;
					payment.secret = payment_secret;
				}
				Entry::Vacant(e) => {
					e.insert(PaymentInfo {
						preimage: payment_preimage,
						secret: payment_secret,
						status: HTLCStatus::Succeeded,
						amt_msat: MillisatAmount(Some(amount_msat)),
						yuv_pixel: None,
					});
				}
			}
			fs_store.write("", "", INBOUND_PAYMENTS_FNAME, &inbound.encode()).unwrap();
		}
		Event::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
			let mut outbound = outbound_payments.lock().unwrap();
			match outbound.payments.get_mut(&payment_hash) {
				Some(payment) => {
					payment.preimage = Some(payment_preimage);
					payment.status = HTLCStatus::Succeeded;

					let yuv_log = if let Some(yuv_pixel) = payment.yuv_pixel {
						format!(" and YUV {} {}", yuv_pixel.luma.amount, yuv_pixel.chroma)
					} else {
						"".to_string()
					};

					let fee_log = if let Some(fee) = fee_paid_msat {
						format!(" (fee {} msat)", fee)
					} else {
						"".to_string()
					};

					println!(
						"\rEVENT: successfully sent payment of {} millisatoshis{}{} from \
								 payment hash {} with preimage {}",
						payment.amt_msat, fee_log, yuv_log, payment_hash, payment_preimage
					);
					print!("\r> ");
					io::stdout().flush().unwrap();
				}
				None => return,
			}
			fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound.encode()).unwrap();
		}
		Event::OpenChannelRequest {
			ref temporary_channel_id, ref counterparty_node_id, ..
		} => {
			let mut random_bytes = [0u8; 16];
			random_bytes.copy_from_slice(&keys_manager.get_secure_random_bytes()[..16]);
			let user_channel_id = u128::from_be_bytes(random_bytes);
			let res = channel_manager.accept_inbound_channel_override_config(
				temporary_channel_id,
				counterparty_node_id,
				user_channel_id,
				default_config.lock().unwrap().clone(),
			);

			if let Err(e) = res {
				println!(
					"\rEVENT: Failed to accept inbound channel ({}) from {}: {:?}",
					temporary_channel_id,
					hex_utils::hex_str(&counterparty_node_id.serialize()),
					e,
				);
			} else {
				println!(
					"\rEVENT: Accepted inbound channel ({}) from {}",
					temporary_channel_id,
					hex_utils::hex_str(&counterparty_node_id.serialize()),
				);
			}
			print!("\r> ");
			io::stdout().flush().unwrap();
		}
		Event::PaymentPathSuccessful { .. } => {}
		Event::PaymentPathFailed { .. } => {}
		Event::ProbeSuccessful { .. } => {}
		Event::ProbeFailed { .. } => {}
		Event::PaymentFailed { payment_hash, reason, .. } => {
			print!(
				"\rEVENT: Failed to send payment to payment hash {}: {:?}",
				payment_hash,
				if let Some(r) = reason { r } else { PaymentFailureReason::RetriesExhausted }
			);
			print!("\r> ");
			io::stdout().flush().unwrap();

			let mut outbound = outbound_payments.lock().unwrap();
			if outbound.payments.contains_key(&payment_hash) {
				let payment = outbound.payments.get_mut(&payment_hash).unwrap();
				payment.status = HTLCStatus::Failed;
			}
			fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound.encode()).unwrap();
		}
		Event::PaymentForwarded {
			prev_channel_id,
			next_channel_id,
			total_fee_earned_msat,
			claim_from_onchain_tx,
			outbound_amount_forwarded_msat,
			outbound_amount_forwarded_yuv,
			..
		} => {
			let read_only_network_graph = network_graph.read_only();
			let nodes = read_only_network_graph.nodes();
			let channels = channel_manager.list_channels();
			let mut yuv_log = Default::default();

			let mut node_str = |channel_id: &Option<ChannelId>| match channel_id {
				None => String::new(),
				Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id) {
					None => String::new(),
					Some(channel) => {
						yuv_log = if let Some(yuv_amount) = outbound_amount_forwarded_yuv {
							format!(
								" and YUV {} {}",
								yuv_amount,
								channel.yuv_holder_pixel.unwrap().chroma
							)
						} else {
							"".to_string()
						};

						match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
							None => "private node".to_string(),
							Some(node) => match &node.announcement_info {
								None => "unnamed node".to_string(),
								Some(announcement) => {
									format!("node {}", announcement.alias)
								}
							},
						}
					}
				},
			};
			let channel_str = |channel_id: &Option<ChannelId>| {
				channel_id
					.map(|channel_id| format!(" with channel {}", channel_id))
					.unwrap_or_default()
			};
			let from_prev_str =
				format!(" from {}{}", node_str(&prev_channel_id), channel_str(&prev_channel_id));
			let to_next_str =
				format!(" to {}{}", node_str(&next_channel_id), channel_str(&next_channel_id));

			let from_onchain_str = if claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};

			let amt_args = if let Some(v) = outbound_amount_forwarded_msat {
				format!("{}", v)
			} else {
				"?".to_string()
			};

			if let Some(fee_earned) = total_fee_earned_msat {
				println!(
					"\rEVENT: Forwarded payment for {} msat{}{}{}, earning {} msat {}",
					amt_args, yuv_log, from_prev_str, to_next_str, fee_earned, from_onchain_str
				);
			} else {
				println!(
					"\rEVENT: Forwarded payment for {} msat{}{}{}, claiming onchain {}",
					amt_args, yuv_log, from_prev_str, to_next_str, from_onchain_str
				);
			}
			print!("\r> ");
			io::stdout().flush().unwrap();
		}
		Event::HTLCHandlingFailed { .. } => {}
		Event::PendingHTLCsForwardable { time_forwardable } => {
			let forwarding_channel_manager = channel_manager.clone();
			tokio::spawn(async move {
				tokio::time::sleep(Duration::from_millis(time_forwardable.as_millis() as u64 * 2))
					.await;
				forwarding_channel_manager.process_pending_htlc_forwards();
			});
		}
		Event::SpendableOutputs { outputs, channel_id: _ } => {
			// SpendableOutputDescriptors, of which outputs is a vec of, are critical to keep track
			// of! While a `StaticOutput` descriptor is just an output to a static, well-known key,
			// other descriptors are not currently ever regenerated for you by LDK. Once we return
			// from this method, the descriptor will be gone, and you may lose track of some funds.
			//
			// Here we simply persist them to disk, with a background task running which will try
			// to spend them regularly (possibly duplicatively/RBF'ing them). These can just be
			// treated as normal funds where possible - they are only spendable by us and there is
			// no rush to claim them.
			for output in outputs {
				let key = hex_utils::hex_str(&keys_manager.get_secure_random_bytes());
				// Note that if the type here changes our read code needs to change as well.
				let output: SpendableOutputDescriptor = output;
				fs_store.write(PENDING_SPENDABLE_OUTPUT_DIR, "", &key, &output.encode()).unwrap();
			}
		}
		Event::ChannelPending { channel_id, counterparty_node_id, .. } => {
			println!(
				"\rEVENT: Channel {} with peer {} is pending awaiting funding lock-in!",
				channel_id,
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("\r> ");
			io::stdout().flush().unwrap();
		}
		Event::ChannelReady {
			ref channel_id,
			user_channel_id: _,
			ref counterparty_node_id,
			channel_type: _,
		} => {
			println!(
				"\rEVENT: Channel {} with peer {} is ready to be used!",
				channel_id,
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("\r> ");
			io::stdout().flush().unwrap();
		}
		Event::ChannelClosed {
			channel_id,
			reason,
			user_channel_id: _,
			counterparty_node_id,
			channel_capacity_sats: _,
			..
		} => {
			println!(
				"\rEVENT: Channel {} with counterparty {} closed due to: {:?}",
				channel_id,
				counterparty_node_id.map(|id| format!("{}", id)).unwrap_or("".to_owned()),
				reason
			);
			print!("\r> ");
			io::stdout().flush().unwrap();
		}
		Event::DiscardFunding { .. } => {
			// A "real" node should probably "lock" the UTXOs spent in funding transactions until
			// the funding transaction either confirms, or this event is generated.
		}
		Event::HTLCIntercepted { .. } => {}
		Event::BumpTransaction(event) => bump_tx_event_handler.handle_event(&event),
		Event::UpdateBalanceApplied(channel_id) => {
			println!("\rEVENT: Channel {} has applied the updated balances", channel_id);
			print!("\r> ");
			io::stdout().flush().unwrap();
		}
		Event::NewUpdateBalanceRequest { channel_id, request } => match request {
			NewUpdateBalanceRequest::NewBalances {
				updated_counterparty_msat,
				updated_counterparty_yuv_luma,
			} => {
				println!(
					"\rEVENT: Counterparty has requested an update to the balances in channel: {}. \
					 The new balances are: {} msat, {:?} yuv luma",
					channel_id, updated_counterparty_msat, updated_counterparty_yuv_luma.map(|luma| luma.amount)
				);
				print!("\r> ");
				io::stdout().flush().unwrap();
			}
			NewUpdateBalanceRequest::Revoke => {
				println!(
					"\rEVENT: Channel {} has requested to revoke the update balances",
					channel_id
				);
				print!("\r> ");
				io::stdout().flush().unwrap();
			}
		},
		_ => {}
	}
}

async fn start_ldk() {
	let args = match args::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	// ## Setup
	// Step 1: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

	let secp_ctx = Secp256k1::new();
	let wallet_name = wallet_name_from_descriptor(
		descriptor!(wpkh(args.private_key)).unwrap(),
		None,
		args.network,
		&secp_ctx,
	)
	.unwrap();

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
		args.network,
		wallet_name,
		tokio::runtime::Handle::current(),
		Arc::clone(&logger),
	)
	.await
	{
		Ok(client) => Arc::new(client),
		Err(e) => {
			println!("\rFailed to connect to bitcoind client: {}", e);
			return;
		}
	};

	// Check that the bitcoind we've connected to is running the network we expect
	let bitcoind_chain = bitcoind_client.get_blockchain_info().await.chain;
	if bitcoind_chain
		!= match args.network {
			Network::Bitcoin => "main",
			Network::Testnet => "test",
			Network::Regtest => "regtest",
			Network::Signet => "signet",
			_ => "unknown",
		} {
		println!(
			"\rChain argument ({}) didn't match bitcoind chain ({})",
			args.network, bitcoind_chain
		);
		return;
	}

	// Step 2: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 3: Initialize the BroadcasterInterface

	// BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
	// broadcaster.
	let broadcaster = bitcoind_client.clone();

	// Step 4: Initialize the KeysManager

	// The key seed that we use to derive the node privkey (that corresponds to the node pubkey) and
	// other secret key material.
	let keys_seed_path = format!("{}/keys_seed", ldk_data_dir.clone());
	let keys_seed = if let Ok(seed) = fs::read(keys_seed_path.clone()) {
		assert_eq!(seed.len(), 32);
		let mut key = [0; 32];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		match File::create(keys_seed_path.clone()) {
			Ok(mut f) => {
				Write::write_all(&mut f, &key).expect("Failed to write node keys seed to disk");
				f.sync_all().expect("Failed to sync node keys seed to disk");
			}
			Err(e) => {
				println!("\rERROR: Unable to create keys seed file {}: {}", keys_seed_path, e);
				return;
			}
		}
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
	let keys_manager = Arc::new(KeysManager::new(&keys_seed, cur.as_secs(), cur.subsec_nanos()));

	let wallet_config = WalletConfig {
		privkey: args.private_key,
		network: args.network,
		bitcoin_provider: BitcoinProviderConfig::BitcoinRpc(BitcoinRpcConfig {
			url: format!("{}:{}", args.bitcoind_rpc_host, args.bitcoind_rpc_port),
			network: args.network,
			auth: Auth::UserPass {
				username: args.bitcoind_rpc_username,
				password: args.bitcoind_rpc_password,
			},
			start_time: 0,
		}),
		yuv_url: args.yuv_rpc_url.clone().unwrap_or_default(),
	};

	let (wallet, wallet_source) = {
		let wallet = Wallet::from_config(wallet_config.clone(), logger.clone()).await.unwrap();
		let wallet_source = wallet.new_wallet_source();

		(Arc::new(TokioMutex::new(wallet)), Arc::new(wallet_source))
	};

	let bump_tx_event_handler = Arc::new(BumpTransactionEventHandler::new(
		Arc::clone(&broadcaster),
		Arc::new(LdkWallet::new(wallet_source, Arc::clone(&logger))),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
	));

	// Step 5: Initialize Persistence
	let fs_store = Arc::new(FilesystemStore::new(ldk_data_dir.clone().into()));
	let persister = Arc::new(MonitorUpdatingPersister::new(
		Arc::clone(&fs_store),
		Arc::clone(&logger),
		1000,
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
	));
	// Alternatively, you can use the `FilesystemStore` as a `Persist` directly, at the cost of
	// larger `ChannelMonitor` update writes (but no deletion or cleanup):
	//let persister = Arc::clone(&fs_store);

	let yuv_client_opt = match args.yuv_rpc_url.clone() {
		Some(yuv_rpc_url) => {
			let yuv_client =
				YuvClient::new(yuv_rpc_url, tokio::runtime::Handle::current(), Arc::clone(&logger));

			Some(Arc::new(yuv_client))
		}
		None => None,
	};

	// Step 6: Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(ChainMonitor::new(
		None,
		Arc::clone(&broadcaster),
		yuv_client_opt.clone(),
		Arc::clone(&logger),
		Arc::clone(&fee_estimator),
		Arc::clone(&persister),
	));

	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors = persister
		.read_all_channel_monitors_with_updates(
			&bitcoind_client,
			yuv_client_opt.clone(),
			&bitcoind_client,
		)
		.unwrap();
	// If you are using the `FilesystemStore` as a `Persist` directly, use
	// `lightning::util::persist::read_channel_monitors` like this:
	//read_channel_monitors(Arc::clone(&persister), Arc::clone(&keys_manager), Arc::clone(&keys_manager)).unwrap();

	// Step 8: Poll for the best chain tip, which may be used by the channel manager & spv client
	let polled_chain_tip = init::validate_best_block_header(bitcoind_client.as_ref())
		.await
		.expect("Failed to fetch best block header and best block");

	// Step 9: Initialize routing ProbabilisticScorer
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let network_graph =
		Arc::new(disk::read_network(Path::new(&network_graph_path), args.network, logger.clone()));

	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
	let scorer = Arc::new(RwLock::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	)));

	// Step 10: Create Router
	let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
	let router = Arc::new(DefaultRouter::new(
		network_graph.clone(),
		logger.clone(),
		keys_manager.clone(),
		scorer.clone(),
		scoring_fee_params,
	));

	// Step 11: Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.channel_handshake_limits.force_announced_channel_preference = false;
	user_config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = true;
	user_config.manually_accept_inbound_channels = true;
	user_config.channel_handshake_config.our_htlc_minimum_msat = 4_000_000;
	user_config.support_yuv_payments = args.yuv_rpc_url.is_some();
	let default_config = Arc::new(Mutex::new(user_config));
	let mut restarting_node = true;
	let (channel_manager_blockhash, channel_manager) = {
		if let Ok(mut f) = File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}

			let yuv_client = yuv_client_opt.clone();
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				yuv_client,
				router,
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
			);
			<(BlockHash, ChannelManager)>::read(&mut f, read_args)
				.expect("Failed to read ChannelManager from disk")
		} else {
			// We're starting a fresh node.
			restarting_node = false;

			let polled_best_block = polled_chain_tip.to_best_block();
			let polled_best_block_hash = polled_best_block.block_hash;
			let chain_params =
				ChainParameters { network: args.network, best_block: polled_best_block };
			let fresh_channel_manager = ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				yuv_client_opt.clone(),
				router,
				logger.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
				cur.as_secs() as u32,
			);
			(polled_best_block_hash, fresh_channel_manager)
		}
	};

	// Step 12: Sync ChannelMonitors and ChannelManager to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let chain_tip = if restarting_node {
		let mut chain_listeners = vec![(
			channel_manager_blockhash,
			&channel_manager as &(dyn chain::Listen + Send + Sync),
		)];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(
					channel_monitor,
					broadcaster.clone(),
					yuv_client_opt.clone(),
					fee_estimator.clone(),
					logger.clone(),
				),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners.push((
				monitor_listener_info.0,
				&monitor_listener_info.1 as &(dyn chain::Listen + Send + Sync),
			));
		}

		init::synchronize_listeners(
			bitcoind_client.as_ref(),
			args.network,
			&mut cache,
			chain_listeners,
		)
		.await
		.unwrap()
	} else {
		polled_chain_tip
	};

	// Step 13: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		assert_eq!(
			chain_monitor.watch_channel(funding_outpoint, channel_monitor),
			Ok(ChannelMonitorUpdateStatus::Completed)
		);
	}

	// Step 14: Optional: Initialize the P2PGossipSync
	let gossip_sync =
		Arc::new(P2PGossipSync::new(Arc::clone(&network_graph), None, logger.clone()));

	// Step 15: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::clone(&channel_manager),
		Arc::new(DefaultMessageRouter::new(Arc::clone(&network_graph), Arc::clone(&keys_manager))),
		Arc::clone(&channel_manager),
		IgnoringMessageHandler {},
	));
	let mut ephemeral_bytes = [0; 32];
	let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
	thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler = MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: gossip_sync.clone(),
		onion_message_handler: onion_messenger.clone(),
		custom_message_handler: IgnoringMessageHandler {},
	};
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		current_time.try_into().unwrap(),
		&ephemeral_bytes,
		logger.clone(),
		Arc::clone(&keys_manager),
	));

	// Install a GossipVerifier in in the P2PGossipSync
	let utxo_lookup = GossipVerifier::with_yuv(
		Arc::clone(&bitcoind_client.bitcoind_rpc_client),
		lightning_block_sync::gossip::TokioSpawner,
		Arc::clone(&gossip_sync),
		Arc::clone(&peer_manager),
		yuv_client_opt.as_ref().map(|yuv_client| Arc::clone(&yuv_client)),
	);

	gossip_sync.add_utxo_lookup(Some(utxo_lookup));

	// ## Running LDK
	// Step 16: Initialize networking

	let peer_manager_connection_handler = peer_manager.clone();
	let listening_port = args.ldk_peer_listening_port;
	let stop_listen_connect = Arc::new(AtomicBool::new(false));
	let stop_listen = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let listener = tokio::net::TcpListener::bind(format!("[::]:{}", listening_port))
			.await
			.expect("Failed to bind to listen port - is something else already listening on it?");
		loop {
			let peer_mgr = peer_manager_connection_handler.clone();
			let tcp_stream = listener.accept().await.unwrap().0;
			if stop_listen.load(Ordering::Acquire) {
				return;
			}
			tokio::spawn(async move {
				lightning_net_tokio::setup_inbound(
					peer_mgr.clone(),
					tcp_stream.into_std().unwrap(),
				)
				.await;
			});
		}
	});

	// Step 17: Connect and Disconnect Blocks
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	tokio::spawn(async move {
		let chain_poller = poll::ChainPoller::new(bitcoind_block_source.as_ref(), args.network);
		let chain_listener = (chain_monitor_listener, channel_manager_listener);
		let mut spv_client = SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	let inbound_payments = Arc::new(Mutex::new(disk::read_payment_info(Path::new(&format!(
		"{}/{}",
		ldk_data_dir, INBOUND_PAYMENTS_FNAME
	)))));
	let outbound_payments = Arc::new(Mutex::new(disk::read_payment_info(Path::new(&format!(
		"{}/{}",
		ldk_data_dir, OUTBOUND_PAYMENTS_FNAME
	)))));
	let recent_payments_payment_hashes = channel_manager
		.list_recent_payments()
		.into_iter()
		.filter_map(|p| match p {
			RecentPaymentDetails::Pending { payment_hash, .. } => Some(payment_hash),
			RecentPaymentDetails::Fulfilled { payment_hash, .. } => payment_hash,
			RecentPaymentDetails::Abandoned { payment_hash, .. } => Some(payment_hash),
			RecentPaymentDetails::AwaitingInvoice { payment_id: _ } => todo!(),
		})
		.collect::<Vec<PaymentHash>>();
	for (payment_hash, payment_info) in outbound_payments
		.lock()
		.unwrap()
		.payments
		.iter_mut()
		.filter(|(_, i)| matches!(i.status, HTLCStatus::Pending))
	{
		if !recent_payments_payment_hashes.contains(payment_hash) {
			payment_info.status = HTLCStatus::Failed;
		}
	}
	fs_store
		.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound_payments.lock().unwrap().encode())
		.unwrap();

	// Step 18: Handle LDK Events
	let channel_manager_event_listener = Arc::clone(&channel_manager);
	let network_graph_event_listener = Arc::clone(&network_graph);
	let keys_manager_event_listener = Arc::clone(&keys_manager);
	let inbound_payments_event_listener = Arc::clone(&inbound_payments);
	let outbound_payments_event_listener = Arc::clone(&outbound_payments);
	let fs_store_event_listener = Arc::clone(&fs_store);

	if let Some(yuv_client) = yuv_client_opt.clone() {
		let channel_manager = Arc::clone(&channel_manager);
		let chain_monitor = Arc::clone(&chain_monitor);
		let yuv_listener = yuv_client.clone();
		tokio::spawn(async move {
			loop {
				let tx_ids_to_request = channel_manager.get_pending_yuv_txs();

				if !tx_ids_to_request.is_empty() {
					let pending_txs =
						yuv_listener.get_list_raw_yuv_transactions(tx_ids_to_request).await;

					if !pending_txs.is_empty() {
						channel_manager.yuv_transactions_confirmed(pending_txs);
					}
				}

				let tx_ids_to_request = chain_monitor.get_pending_yuv_txs();

				if !tx_ids_to_request.is_empty() {
					let pending_txs =
						yuv_listener.get_list_raw_yuv_transactions(tx_ids_to_request).await;

					if !pending_txs.is_empty() {
						chain_monitor.yuv_transactions_confirmed(pending_txs);
					}
				}

				tokio::time::sleep(Duration::from_secs(1)).await;
			}
		});
	}

	let event_handlers_wallet = wallet.clone();
	let event_jandlers_default_config = default_config.clone();
	let event_handler = move |event: Event| {
		let channel_manager_event_listener = Arc::clone(&channel_manager_event_listener);
		let network_graph_event_listener = Arc::clone(&network_graph_event_listener);
		let keys_manager_event_listener = Arc::clone(&keys_manager_event_listener);
		let bump_tx_event_handler = Arc::clone(&bump_tx_event_handler);
		let inbound_payments_event_listener = Arc::clone(&inbound_payments_event_listener);
		let outbound_payments_event_listener = Arc::clone(&outbound_payments_event_listener);
		let fs_store_event_listener = Arc::clone(&fs_store_event_listener);
		let wallet = Arc::clone(&event_handlers_wallet.clone());
		let default_config = Arc::clone(&event_jandlers_default_config);

		async move {
			handle_ldk_events(
				&channel_manager_event_listener,
				&network_graph_event_listener,
				&keys_manager_event_listener,
				&bump_tx_event_handler,
				inbound_payments_event_listener,
				outbound_payments_event_listener,
				&fs_store_event_listener,
				event,
				wallet,
				default_config,
			)
			.await;
		}
	};

	// Step 19: Persist ChannelManager and NetworkGraph
	let persister = Arc::new(FilesystemStore::new(ldk_data_dir.clone().into()));

	// Step 20: Background Processing
	let (bp_exit, bp_exit_check) = tokio::sync::watch::channel(());
	let mut background_processor = tokio::spawn(process_events_async(
		Arc::clone(&persister),
		event_handler,
		chain_monitor.clone(),
		channel_manager.clone(),
		GossipSync::p2p(gossip_sync.clone()),
		peer_manager.clone(),
		logger.clone(),
		Some(scorer.clone()),
		move |t| {
			let mut bp_exit_fut_check = bp_exit_check.clone();
			Box::pin(async move {
				tokio::select! {
					_ = tokio::time::sleep(t) => false,
					_ = bp_exit_fut_check.changed() => true,
				}
			})
		},
		false,
		|| Some(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap()),
	));

	// Regularly reconnect to channel peers.
	let connect_cm = Arc::clone(&channel_manager);
	let connect_pm = Arc::clone(&peer_manager);
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir);
	let stop_connect = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		loop {
			interval.tick().await;
			match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
				Ok(info) => {
					let peers = connect_pm.list_peers();
					for node_id in connect_cm
						.list_channels()
						.iter()
						.map(|chan| chan.counterparty.node_id)
						.filter(|id| {
							!peers.iter().any(|details| details.counterparty_node_id.eq(id))
						}) {
						if stop_connect.load(Ordering::Acquire) {
							return;
						}
						for (pubkey, peer_addr) in info.iter() {
							if *pubkey == node_id {
								let _ = cli::do_connect_peer(
									*pubkey,
									*peer_addr,
									Arc::clone(&connect_pm),
								)
								.await;
							}
						}
					}
				}
				Err(e) => println!("\rERROR: errored reading channel peer info from disk: {:?}", e),
			}
		}
	});

	// Regularly broadcast our node_announcement. This is only required (or possible) if we have
	// some public channels.
	let peer_man = Arc::clone(&peer_manager);
	let chan_man = Arc::clone(&channel_manager);
	let network = args.network;
	let an_logger = Arc::clone(&logger);
	tokio::spawn(async move {
		// First wait a minute until we have some peers and maybe have opened a channel.
		tokio::time::sleep(Duration::from_secs(60)).await;
		// Then, update our announcement once an hour to keep it fresh but avoid unnecessary churn
		// in the global gossip network.
		let mut interval = tokio::time::interval(Duration::from_secs(30)); // TODO: turn it back to 3600
		loop {
			interval.tick().await;
			// Don't bother trying to announce if we don't have any public channls, though our
			// peers should drop such an announcement anyway. Note that announcement may not
			// propagate until we have a channel with 6+ confirmations.
			if chan_man.list_channels().iter().any(|chan| chan.is_public) {
				peer_man.broadcast_node_announcement(
					[0; 3],
					args.ldk_announced_node_name,
					args.ldk_announced_listen_addr.clone(),
				);

				lightning::log_trace!(&an_logger, "Node announcement broadcasted");
			}
		}
	});

	tokio::spawn(sweep::periodic_sweep(
		ldk_data_dir.clone(),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::clone(&persister),
		Arc::clone(&wallet),
		yuv_client_opt,
		Arc::clone(&bitcoind_client),
		Arc::clone(&channel_manager),
	));

	// Start the CLI.
	let cli_channel_manager = Arc::clone(&channel_manager);
	let cli_persister = Arc::clone(&persister);
	let cli_logger = Arc::clone(&logger);
	let cli_peer_manager = Arc::clone(&peer_manager);
	let cli_poll = tokio::task::spawn_blocking(move || {
		cli::poll_for_user_input(
			cli_peer_manager,
			cli_channel_manager,
			keys_manager,
			network_graph,
			onion_messenger,
			inbound_payments,
			outbound_payments,
			ldk_data_dir,
			network,
			cli_logger,
			cli_persister,
			default_config,
		);
	});

	// Exit if either CLI polling exits or the background processor exits (which shouldn't happen
	// unless we fail to write to the filesystem).
	let mut bg_res = Ok(Ok(()));
	tokio::select! {
		_ = cli_poll => {},
		bg_exit = &mut background_processor => {
			bg_res = bg_exit;
		},
	}

	// Disconnect our peers and stop accepting new connections. This ensures we don't continue
	// updating our channel data after we've stopped the background processor.
	stop_listen_connect.store(true, Ordering::Release);
	peer_manager.disconnect_all_peers();

	if let Err(err) = bg_res {
		persister
			.write(
				persist::CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				persist::CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				persist::CHANNEL_MANAGER_PERSISTENCE_KEY,
				&channel_manager.encode(),
			)
			.unwrap();

		panic!("ERR: background processing stopped with result {err}, exiting.",);
	}

	// Stop the background processor.
	if !bp_exit.is_closed() {
		bp_exit.send(()).unwrap();
		background_processor.await.unwrap().unwrap();
	}
}

#[tokio::main]
pub async fn main() {
	#[cfg(not(target_os = "windows"))]
	{
		// Catch Ctrl-C with a dummy signal handler.
		unsafe {
			let mut new_action: libc::sigaction = core::mem::zeroed();
			let mut old_action: libc::sigaction = core::mem::zeroed();

			extern "C" fn dummy_handler(
				_: libc::c_int, _: *const libc::siginfo_t, _: *const libc::c_void,
			) {
			}

			new_action.sa_sigaction = dummy_handler as libc::sighandler_t;
			new_action.sa_flags = libc::SA_SIGINFO;

			libc::sigaction(
				libc::SIGINT,
				&new_action as *const libc::sigaction,
				&mut old_action as *mut libc::sigaction,
			);
		}
	}

	start_ldk().await;
}
