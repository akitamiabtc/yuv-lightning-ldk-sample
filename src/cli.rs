use crate::disk::{self, read_channel_peer_data, INBOUND_PAYMENTS_FNAME, OUTBOUND_PAYMENTS_FNAME};
use crate::hex_utils;
use crate::{
	ChannelManager, HTLCStatus, MillisatAmount, NetworkGraph, OnionMessenger, PaymentInfo,
	PaymentInfoStorage, PeerManager,
};
use bitcoin::hashes::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::PublicKey;
use bitcoin::PrivateKey;
use crossterm::event::{read, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, ClearType};
use crossterm::{cursor, terminal, ExecutableCommand};
use eyre::bail;
use lightning::ln::channelmanager::{PaymentId, RecipientOnionFields, Retry, UpdateBalance};
use lightning::ln::msgs::SocketAddress;
use lightning::ln::{ChannelId, PaymentHash, PaymentPreimage};
use lightning::onion_message::messenger::Destination;
use lightning::onion_message::packet::OnionMessageContents;
use lightning::routing::gossip::NodeId;
use lightning::routing::router::{PaymentParameters, RouteParameters};
use lightning::sign::{EntropySource, KeysManager};
use lightning::util::config::UserConfig;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Writeable, Writer};
use lightning_invoice::payment::{
	payment_parameters_from_invoice, payment_parameters_from_zero_amount_invoice,
};
use lightning_invoice::{utils, Bolt11Invoice, Currency};
use lightning_persister::fs_store::FilesystemStore;
use std::env;
use std::io::{stdout, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::str::{FromStr, SplitWhitespace};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use yuv_pixels::{Chroma, Luma, Pixel};

pub(crate) struct LdkUserInfo {
	pub(crate) bitcoind_rpc_username: String,
	pub(crate) bitcoind_rpc_password: String,
	pub(crate) bitcoind_rpc_port: u16,
	pub(crate) private_key: PrivateKey,
	pub(crate) bitcoind_rpc_host: String,
	pub(crate) ldk_storage_dir_path: String,
	pub(crate) ldk_peer_listening_port: u16,
	pub(crate) ldk_announced_listen_addr: Vec<SocketAddress>,
	pub(crate) ldk_announced_node_name: [u8; 32],
	pub(crate) network: Network,
	pub(crate) yuv_rpc_url: Option<String>,
}

#[derive(Debug)]
struct UserOnionMessageContents {
	tlv_type: u64,
	data: Vec<u8>,
}

impl OnionMessageContents for UserOnionMessageContents {
	fn tlv_type(&self) -> u64 {
		self.tlv_type
	}
}

impl Writeable for UserOnionMessageContents {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), std::io::Error> {
		w.write_all(&self.data)
	}
}

pub(crate) fn read_input(
	prefix: &str, commands_history: &mut Vec<String>,
) -> eyre::Result<Option<String>> {
	let prefix_size = prefix.len();
	let (term_width, _) = terminal::size()?;

	print!("\r{}", prefix);
	stdout().flush().unwrap();

	enable_raw_mode().unwrap();

	let mut stdout = stdout();
	let mut input_buffer = String::new();
	let mut cursor_position = 0;
	let mut history_position = 0;

	commands_history.insert(0, String::new());

	let result = loop {
		if let Event::Key(key_event) = read()? {
			match key_event.code {
				KeyCode::Char(c) => {
					if (c == 'd' || c == 'c') && key_event.modifiers == KeyModifiers::CONTROL {
						break None;
					}

					history_position = 0;
					if !c.is_ascii() {
						continue;
					}

					input_buffer.insert(cursor_position, c);
					commands_history[0] = input_buffer.clone();
					cursor_position += 1;
				}
				KeyCode::Left if cursor_position > 0 => cursor_position -= 1,
				KeyCode::Right if cursor_position < input_buffer.len() => cursor_position += 1,
				KeyCode::Backspace => {
					if cursor_position > 0 {
						input_buffer.remove(cursor_position - 1);
						cursor_position -= 1;
					}

					history_position = 0;
					commands_history[0] = input_buffer.clone();
				}
				KeyCode::Up => {
					if history_position > 50
						|| history_position >= commands_history.len().saturating_sub(1)
					{
						continue;
					}
					history_position += 1;

					input_buffer = commands_history[history_position].clone();
					cursor_position = input_buffer.len();
				}
				KeyCode::Down => {
					if history_position == 0 {
						continue;
					}
					history_position -= 1;

					input_buffer = commands_history[history_position].clone();
					cursor_position = input_buffer.len();
				}
				KeyCode::Enter => {
					if !input_buffer.is_empty() {
						commands_history[0] = input_buffer.clone();
						if commands_history.len() > 50 {
							commands_history.pop();
						}
					}

					if input_buffer.len() + prefix_size > term_width as usize {
						stdout.execute(cursor::MoveToColumn(0))?;
						stdout.execute(terminal::Clear(ClearType::CurrentLine))?;

						write!(stdout, "{}{}", prefix, &input_buffer)?;
					}

					stdout.flush().unwrap();

					break Some(input_buffer);
				}
				_ => {}
			}

			// Calculate visible portion of the buffer based on cursor position and terminal width
			let visible_width = (term_width as usize).saturating_sub(prefix_size);
			let start_pos = if cursor_position >= visible_width {
				cursor_position + 1 - visible_width
			} else {
				0
			};

			let end_pos = std::cmp::min(input_buffer.len(), start_pos + visible_width);

			// Clear the current line and reset cursor to the start of the line
			stdout.execute(cursor::MoveToColumn(0))?;
			stdout.execute(terminal::Clear(ClearType::CurrentLine))?;

			// Print the current state of the buffer, adjusting for terminal width
			write!(stdout, "{}{}", prefix, &input_buffer[start_pos..end_pos])?;

			// Move the cursor to its correct position within the visible window
			let visible_cursor_pos = cursor_position.saturating_sub(start_pos) + prefix_size;
			stdout.execute(cursor::MoveToColumn(visible_cursor_pos as u16))?;

			stdout.flush()?;
		}
	};

	disable_raw_mode().unwrap();
	println!();
	stdout.flush().unwrap();

	Ok(result)
}

pub(crate) fn poll_for_user_input(
	peer_manager: Arc<PeerManager>, channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>, network_graph: Arc<NetworkGraph>,
	onion_messenger: Arc<OnionMessenger>, inbound_payments: Arc<Mutex<PaymentInfoStorage>>,
	outbound_payments: Arc<Mutex<PaymentInfoStorage>>, ldk_data_dir: String, network: Network,
	logger: Arc<disk::FilesystemLogger>, fs_store: Arc<FilesystemStore>,
	default_config: Arc<Mutex<UserConfig>>,
) {
	println!(
		"\rLDK startup successful. Enter \"help\" to view available commands. Press Ctrl-D to quit."
	);
	println!("\rLDK logs are available at <your-supplied-ldk-data-dir-path>/.ldk/logs");
	println!("\rLocal Node ID is {}.", channel_manager.get_our_node_id());

	let mut commands_history = Vec::new();

	'outer: loop {
		stdout().flush().unwrap();
		let line = match read_input("> ", &mut commands_history).unwrap() {
			Some(line) => line,
			None => break,
		};

		if line.len() == 0 {
			continue;
		}

		let mut words = line.split_whitespace();
		if let Some(word) = words.next() {
			match word {
				"help" => help(),
				"configchannel" => {
					while let Some(word) = words.next() {
						match word {
							"--min-inb-htlc" => {
								let min_inbound_htlc = match parse_named_param(&mut words, word) {
									Some(min_htlc) => min_htlc,
									None => continue 'outer,
								};

								let mut default_config = default_config.lock().unwrap();
								default_config.channel_handshake_config.our_htlc_minimum_msat =
									min_inbound_htlc;
							}
							"--max-inb-htlc-pct" => {
								let max_inbound_htlc_percent =
									match parse_named_param(&mut words, word) {
										Some(min_htlc) => min_htlc,
										None => continue 'outer,
									};

								let mut default_config = default_config.lock().unwrap();
								default_config
									.channel_handshake_config
									.max_inbound_htlc_value_in_flight_percent_of_channel = max_inbound_htlc_percent;
							}
							"--support-yuv" => {
								let support_yuv = match parse_named_param(&mut words, word) {
									Some(min_htlc) => min_htlc,
									None => continue 'outer,
								};

								let mut default_config = default_config.lock().unwrap();
								default_config.support_yuv_payments = support_yuv;
							}
							_ => {
								println!("\rERROR: unknown parameter: {word}");
								continue 'outer;
							}
						}
					}
				}
				"openchannel" => {
					let peer_pubkey = words.next();
					let channel_value_sat = words.next();
					if peer_pubkey.is_none() || channel_value_sat.is_none() {
						println!("\rERROR: openchannel has 2 required arguments: `openchannel peer_pubkey channel_amt_satoshis [--pixel <luma>:<chroma>] [--public] [--with-anchors] [--min-inbound-htlc]`");
						continue;
					}

					let pubkey = match hex_utils::to_compressed_pubkey(peer_pubkey.unwrap()) {
						Some(pubkey) => pubkey,
						None => {
							println!("\rError: invalid peer pubkey");
							continue;
						}
					};

					let chan_amt_sat: Result<u64, _> = channel_value_sat.unwrap().parse();
					if chan_amt_sat.is_err() {
						println!("\rERROR: channel amount must be a number");
						continue;
					}

					let (mut announce_channel, mut with_anchors) = (false, false);
					let mut yuv_pixel = None;
					while let Some(word) = words.next() {
						match word {
							"--pixel" => {
								let pixel_word = words.next().unwrap_or_default();

								yuv_pixel = match parse_pixel_word(pixel_word) {
									Ok(pixel) => Some(pixel),
									Err(err) => {
										println!(
											"\rERROR: invalid `--pixel` param: {}",
											err.to_string()
										);
										continue 'outer;
									}
								}
							}
							"--public" | "--public=true" => announce_channel = true,
							"--public=false" => announce_channel = false,
							"--with-anchors" | "--with-anchors=true" => with_anchors = true,
							"--with-anchors=false" => with_anchors = false,
							_ => {
								println!("\rERROR: unknown parameter: {word}");
								continue 'outer;
							}
						}
					}

					let mut config = default_config.lock().unwrap().clone();
					config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx =
						with_anchors;
					config.channel_handshake_config.announced_channel = announce_channel;

					let peer_data_path_str = format!("{}/channel_peer_data", ldk_data_dir.clone());
					let peer_data_path = Path::new(peer_data_path_str.as_str());
					let peers_data = read_channel_peer_data(peer_data_path).unwrap();

					let peer_addr = match peers_data.get(&pubkey) {
						Some(peer_addr) => peer_addr,
						None => {
							println!("[ERROR]: Uknown peer: {}", pubkey.to_string());
							println!("List of known:");
							list_peers(ldk_data_dir.clone());
							continue;
						}
					};

					if let Err(_) = tokio::runtime::Handle::current().block_on(
						connect_peer_if_necessary(pubkey, *peer_addr, peer_manager.clone()),
					) {
						continue;
					}

					let _ = open_channel(
						pubkey,
						chan_amt_sat.unwrap(),
						config,
						channel_manager.clone(),
						yuv_pixel,
					);
				}
				"sendpayment" => {
					let invoice_str = words.next();
					if invoice_str.is_none() {
						println!(
							"\rERROR: sendpayment requires an invoice: `sendpayment <invoice>`"
						);
						continue;
					}

					let mut user_provided_amt: Option<u64> = None;
					if let Some(amt_msat_str) = words.next() {
						match amt_msat_str.parse() {
							Ok(amt) => user_provided_amt = Some(amt),
							Err(e) => {
								println!("ERROR: couldn't parse amount_msat: {}", e);
								continue;
							}
						};
					}

					match Bolt11Invoice::from_str(invoice_str.unwrap()) {
						Ok(invoice) => send_payment(
							&channel_manager,
							&invoice,
							user_provided_amt,
							&mut outbound_payments.lock().unwrap(),
							Arc::clone(&fs_store),
						),
						Err(e) => {
							println!("\rERROR: invalid invoice: {:?}", e);
						}
					}
				}
				"keysend" => {
					let dest_pubkey = match words.next() {
						Some(dest) => match hex_utils::to_compressed_pubkey(dest) {
							Some(pk) => pk,
							None => {
								println!("\rERROR: couldn't parse destination pubkey");
								continue;
							}
						},
						None => {
							println!("\rERROR: keysend requires a destination pubkey: `keysend <dest_pubkey> <amt_msat>`");
							continue;
						}
					};
					let amt_msat_str = match words.next() {
						Some(amt) => amt,
						None => {
							println!("\rERROR: keysend requires an amount in millisatoshis: `keysend <dest_pubkey> <amt_msat>`");
							continue;
						}
					};
					let amt_msat: u64 = match amt_msat_str.parse() {
						Ok(amt) => amt,
						Err(e) => {
							println!("\rERROR: couldn't parse amount_msat: {}", e);
							continue;
						}
					};
					keysend(
						&channel_manager,
						dest_pubkey,
						amt_msat,
						&*keys_manager,
						&mut outbound_payments.lock().unwrap(),
						Arc::clone(&fs_store),
					);
				}
				"getinvoice" => {
					let amt_str = words.next();
					if amt_str.is_none() {
						println!("\rERROR: getinvoice requires an amount in millisatoshis");
						continue;
					}

					let amt_msat: Result<u64, _> = amt_str.unwrap().parse();
					if amt_msat.is_err() {
						println!("\rERROR: getinvoice provided payment amount was not a number");
						continue;
					}

					let expiry_secs_str = words.next();
					if expiry_secs_str.is_none() {
						println!("\rERROR: getinvoice requires an expiry in seconds");
						continue;
					}

					let expiry_secs: Result<u32, _> = expiry_secs_str.unwrap().parse();
					if expiry_secs.is_err() {
						println!("\rERROR: getinvoice provided expiry was not a number");
						continue;
					}

					let mut yuv_pixel = None;
					while let Some(word) = words.next() {
						match word {
							"--pixel" => {
								let pixel_word = words.next().unwrap_or_default();

								yuv_pixel = match parse_pixel_word(pixel_word) {
									Ok(pixel) => Some(pixel),
									Err(err) => {
										println!(
											"\rERROR: invalid `--pixel` param: {}",
											err.to_string()
										);
										continue 'outer;
									}
								}
							}
							_ => {
								println!("\rERROR: unknown parameter: {word}");
								continue 'outer;
							}
						}
					}

					let mut inbound_payments = inbound_payments.lock().unwrap();
					get_invoice(
						amt_msat.unwrap(),
						&mut inbound_payments,
						&channel_manager,
						Arc::clone(&keys_manager),
						network,
						expiry_secs.unwrap(),
						yuv_pixel,
						Arc::clone(&logger),
					);
					fs_store
						.write("", "", INBOUND_PAYMENTS_FNAME, &inbound_payments.encode())
						.unwrap();
				}
				"connectpeer" => {
					let peer_pubkey_and_ip_addr = words.next();
					if peer_pubkey_and_ip_addr.is_none() {
						println!("\rERROR: connectpeer requires peer connection info: `connectpeer pubkey@host:port`");
						continue;
					}
					let (pubkey, peer_addr) =
						match parse_peer_info(peer_pubkey_and_ip_addr.unwrap().to_string()) {
							Ok(info) => info,
							Err(e) => {
								println!("\r{:?}", e.into_inner().unwrap());
								continue;
							}
						};
					if tokio::runtime::Handle::current()
						.block_on(connect_peer_if_necessary(
							pubkey,
							peer_addr,
							peer_manager.clone(),
						))
						.is_ok()
					{
						println!("\rSUCCESS: connected to peer {}", pubkey);
					}

					let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
					let _ = disk::persist_channel_peer(
						Path::new(&peer_data_path),
						peer_pubkey_and_ip_addr.unwrap(),
					);
				}
				"disconnectpeer" => {
					let peer_pubkey = words.next();
					if peer_pubkey.is_none() {
						println!("\rERROR: disconnectpeer requires peer public key: `disconnectpeer <peer_pubkey>`");
						continue;
					}

					let peer_pubkey =
						match bitcoin::secp256k1::PublicKey::from_str(peer_pubkey.unwrap()) {
							Ok(pubkey) => pubkey,
							Err(e) => {
								println!("\rERROR: {}", e.to_string());
								continue;
							}
						};

					if do_disconnect_peer(
						peer_pubkey,
						peer_manager.clone(),
						channel_manager.clone(),
					)
					.is_ok()
					{
						println!("\rSUCCESS: disconnected from peer {}", peer_pubkey);
					}
				}
				"listchannels" => list_channels(&channel_manager, &network_graph),
				"listpayments" => list_payments(
					&inbound_payments.lock().unwrap(),
					&outbound_payments.lock().unwrap(),
				),
				"closechannel" => {
					let channel_id_str = words.next();
					if channel_id_str.is_none() {
						println!("\rERROR: closechannel requires a channel ID: `closechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
					if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
						println!("\rERROR: couldn't parse channel_id");
						continue;
					}
					let mut channel_id = [0; 32];
					channel_id.copy_from_slice(&channel_id_vec.unwrap());

					let peer_pubkey_str = words.next();
					if peer_pubkey_str.is_none() {
						println!("\rERROR: closechannel requires a peer pubkey: `closechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
						Some(peer_pubkey_vec) => peer_pubkey_vec,
						None => {
							println!("\rERROR: couldn't parse peer_pubkey");
							continue;
						}
					};
					let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							println!("\rERROR: couldn't parse peer_pubkey");
							continue;
						}
					};

					close_channel(channel_id, peer_pubkey, channel_manager.clone());
				}
				"forceclosechannel" => {
					let channel_id_str = words.next();
					if channel_id_str.is_none() {
						println!("\rERROR: forceclosechannel requires a channel ID: `forceclosechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
					if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
						println!("\rERROR: couldn't parse channel_id");
						continue;
					}
					let mut channel_id = [0; 32];
					channel_id.copy_from_slice(&channel_id_vec.unwrap());

					let peer_pubkey_str = words.next();
					if peer_pubkey_str.is_none() {
						println!("\rERROR: forceclosechannel requires a peer pubkey: `forceclosechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
						Some(peer_pubkey_vec) => peer_pubkey_vec,
						None => {
							println!("\rERROR: couldn't parse peer_pubkey");
							continue;
						}
					};
					let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							println!("\rERROR: couldn't parse peer_pubkey");
							continue;
						}
					};

					force_close_channel(channel_id, peer_pubkey, channel_manager.clone());
				}
				"nodeinfo" => node_info(&channel_manager, &peer_manager),
				"listpeers" => list_peers(ldk_data_dir.clone()),
				"signmessage" => {
					const MSG_STARTPOS: usize = "signmessage".len() + 1;
					if line.trim().as_bytes().len() <= MSG_STARTPOS {
						println!("\rERROR: signmsg requires a message");
						continue;
					}
					println!(
						"\r{:?}",
						lightning::util::message_signing::sign(
							&line.trim().as_bytes()[MSG_STARTPOS..],
							&keys_manager.get_node_secret_key()
						)
					);
				}
				"sendonionmessage" => {
					let path_pks_str = words.next();
					if path_pks_str.is_none() {
						println!(
							"\rERROR: sendonionmessage requires at least one node id for the path"
						);
						continue;
					}
					let mut intermediate_nodes = Vec::new();
					let mut errored = false;
					for pk_str in path_pks_str.unwrap().split(",") {
						let node_pubkey_vec = match hex_utils::to_vec(pk_str) {
							Some(peer_pubkey_vec) => peer_pubkey_vec,
							None => {
								println!("\rERROR: couldn't parse peer_pubkey");
								errored = true;
								break;
							}
						};
						let node_pubkey = match PublicKey::from_slice(&node_pubkey_vec) {
							Ok(peer_pubkey) => peer_pubkey,
							Err(_) => {
								println!("\rERROR: couldn't parse peer_pubkey");
								errored = true;
								break;
							}
						};
						intermediate_nodes.push(node_pubkey);
					}
					if errored {
						continue;
					}
					let tlv_type = match words.next().map(|ty_str| ty_str.parse()) {
						Some(Ok(ty)) if ty >= 64 => ty,
						_ => {
							println!("\rNeed an integral message type above 64");
							continue;
						}
					};
					let data = match words.next().map(|s| hex_utils::to_vec(s)) {
						Some(Some(data)) => data,
						_ => {
							println!("\rNeed a hex data string");
							continue;
						}
					};
					let destination = Destination::Node(intermediate_nodes.pop().unwrap());
					match onion_messenger.send_onion_message(
						UserOnionMessageContents { tlv_type, data },
						destination,
						None,
					) {
						Ok(_) => println!("\rSUCCESS: forwarded onion message to first hop"),
						Err(e) => println!("\rERROR: failed to send onion message: {:?}", e),
					}
				}
				"updatebalance" => {
					let channel_id_str = words.next();
					if channel_id_str.is_none() {
						println!("\rERROR: updatebalance requires a channel ID: `updatebalance <channel_id> <peer_pubkey>`");
						continue;
					}
					let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
					if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
						println!("\rERROR: couldn't parse channel_id");
						continue;
					}
					let mut channel_id = [0; 32];
					channel_id.copy_from_slice(&channel_id_vec.unwrap());

					let peer_pubkey = match pubkey_from_input(&mut words) {
						Some(value) => value,
						None => continue,
					};

					let mut new_balance_msat = None;
					if let Some(msat_raw) = words.next() {
						let msat = match u64::from_str(msat_raw) {
							Ok(msat) => msat,
							Err(e) => {
								println!("\rERROR: invalid new_balance_msat (u64): {}", e);
								continue;
							}
						};

						new_balance_msat = Some(msat);
					}

					let mut new_yuv_luma = None;
					if let Some(luma_raw) = words.next() {
						let luma = match u128::from_str(luma_raw) {
							Ok(luma) => Luma::from(luma),
							Err(e) => {
								println!("\rERROR: invalid luma(u64): {}", e);
								continue;
							}
						};

						new_yuv_luma = Some(luma);
					}

					update_balance(
						channel_id,
						peer_pubkey,
						new_balance_msat,
						new_yuv_luma,
						channel_manager.clone(),
					);
				}
				"listnodes" => {
					println!("\r{}", &network_graph)
				}
				"quit" | "exit" => break,
				_ => println!("\rUnknown command. See \"help\" for available commands."),
			}
		}
	}
}

// fn build_hops_from_node_ids(
// 	network_graph: &Arc<lightning::routing::gossip::NetworkGraph<Arc<disk::FilesystemLogger>>>,
// 	node_ids: Vec<PublicKey>,
// ) -> eyre::Result<lightning::routing::router::Path> {
// 	let graph = network_graph.read_only();
// 	let mut path: Vec<RouteHop> = Vec::new();
//
// 	for node_ids in node_ids.windows(2) {
// 		let [current_node_id, next_node_id] = node_ids else {
// 			bail!("path must contain at least two nodes");
// 		};
//
// 		let current_node_id = NodeId::from_pubkey(&current_node_id);
// 		let Some(node) = graph.node(&current_node_id) else {
// 			bail!("node {} not found in network graph", current_node_id);
// 		};
//
// 		let Some(ref announcement) = node.announcement_info else {
// 			bail!("node {} not announced", current_node_id);
// 		};
//
// 		let next_node_id = NodeId::from_pubkey(next_node_id);
// 		let (channel_id, channel) = node
// 			.channels
// 			.iter()
// 			.find_map(|channel_id| {
// 				let Some(channel_info) = graph.channel(*channel_id) else {
// 					return None;
// 				};
//
// 				if channel_info.node_two != next_node_id {
// 					return None;
// 				}
//
// 				Some((channel_id, channel_info))
// 			})
// 			.ok_or_else(|| {
// 				eyre::eyre!("no channel between nodes: {} -> {}", current_node_id, next_node_id)
// 			})?;
//
// 		let channel_info = channel
// 			.one_to_two
// 			.as_ref()
// 			.ok_or_else(|| eyre::eyre!("channel {} is not announced", channel_id))?;
//
// 		path.push(RouteHop {
// 			pubkey: next_node_id.as_pubkey().unwrap(),
// 			node_features: announcement.features.clone(),
// 			short_channel_id: *channel_id,
// 			channel_features: channel.features.clone(),
// 			fee_msat: channel_info.fees.base_msat as u64,
// 			cltv_expiry_delta: channel_info.cltv_expiry_delta as u32,
// 			maybe_announced_channel: true,
// 		});
// 	}
//
// 	Ok(lightning::routing::router::Path { hops: path, blinded_tail: None })
// }

pub fn parse_named_param<F: FromStr>(words: &mut SplitWhitespace, param_name: &str) -> Option<F> {
	let Some(param_raw) = words.next() else {
		println!("\rERROR: invalid {param_name} parameter");
		return None;
	};

	let Ok(param) = F::from_str(param_raw) else {
		println!("\rERROR: invalid {param_name} parameter");
		return None;
	};

	return Some(param);
}

pub fn parse_pixel_word(word: &str) -> eyre::Result<Pixel> {
	let mut splited_word = word.split(":");

	if let (Some(luma_raw), Some(chroma_raw)) = (splited_word.next(), splited_word.next()) {
		let luma = match u128::from_str(luma_raw) {
			Ok(luma) => Luma::from(luma),
			Err(e) => {
				bail!("invalid Luma(u64): {}", e);
			}
		};

		let chroma = match Chroma::from_address(chroma_raw) {
			Ok(chroma) => chroma,
			Err(e) => {
				bail!("invalid Chroma(P2TR): {}", e);
			}
		};

		return Ok(Pixel::new(luma, chroma));
	}

	bail!("Pixel must be in the form: <luma>:<chroma>")
}

fn pubkey_from_input(words: &mut SplitWhitespace<'_>) -> Option<PublicKey> {
	let peer_pubkey_str = words.next();
	if peer_pubkey_str.is_none() {
		println!("\rERROR: updatebalance requires a peer pubkey: `updatebalance <channel_id> <peer_pubkey>`");
		return None;
	}
	let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
		Some(peer_pubkey_vec) => peer_pubkey_vec,
		None => {
			println!("\rERROR: couldn't parse peer_pubkey");
			return None;
		}
	};
	let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
		Ok(peer_pubkey) => peer_pubkey,
		Err(_) => {
			println!("\rERROR: couldn't parse peer_pubkey");
			return None;
		}
	};

	Some(peer_pubkey)
}

fn help() {
	let package_version = env!("CARGO_PKG_VERSION");
	let package_name = env!("CARGO_PKG_NAME");
	println!("\r\n\tVERSION:");
	println!("\r\t  {} v{}", package_name, package_version);
	println!("\r\n\tUSAGE:");
	println!("\r\t  Command [arguments]");
	println!("\r\n\tCOMMANDS:");
	println!("\r\t  help\tShows a list of commands.");
	println!("\r\t  quit\tClose the application.");
	println!("\r\n\t  Channels:");
	println!("\r\t      openchannel peer_pubkey channel_amt_satoshis [--pixel <luma>:<chroma>][--public][--with-anchors]");
	println!("\r\t      closechannel <channel_id> <peer_pubkey>");
	println!("\r\t      forceclosechannel <channel_id> <peer_pubkey>");
	println!("\r\t      listchannels");
	println!("\r\t      configchannel");
	println!("\r\t          [--min-inb-htlc <min_inbound_htlc_msat>]");
	println!("\r\t          [--max-inb-htlc-pct <max_inbound_htlc_msat_percent>]");
	println!("\r\t          [--support-yuv <true|false>]");
	println!("\r\n\t  Peers:");
	println!("\r\t      connectpeer pubkey@host:port");
	println!("\r\t      disconnectpeer <peer_pubkey>");
	println!("\r\t      listpeers");
	println!("\r\n\t  Payments:");
	// println!("\r      sendhtlc <final_cltv_expiry> <amount_msats> [--yuv_amount=] <destination>");
	// println!(
	// 	"\r      sendalongpath <final_cltv_expiry> <amount_msats> [--yuv_amount=] <node_id...>"
	// );
	println!("\r\t      keysend <dest_pubkey> <amt_msats>");
	println!("\r\t      listpayments");
	println!("\r\n\t  Invoices:");
	println!("\r\t      getinvoice <amt_msats> <expiry_secs> [--pixel <luma>:<chroma>]");
	println!("\r\t      sendpayment <invoice>");
	println!("\r\n\t  UpdateBalance:");
	println!(
		"\r\t      updatebalance <channel_id> <peer_pubkey> [new_balance_msat] [new_yuv_luma]"
	);
	println!("\r\n\t  Other:");
	println!("\r\t      signmessage <message>");
	println!(
		"\r\t      sendonionmessage <node_id_1,node_id_2,..,destination_node_id> <type> <hex_bytes>"
	);
	println!("\r\t      nodeinfo");
}

fn node_info(channel_manager: &Arc<ChannelManager>, peer_manager: &Arc<PeerManager>) {
	println!("\r{{");
	println!("\r\t node_pubkey: {}", channel_manager.get_our_node_id());
	let chans = channel_manager.list_channels();
	println!("\r\t num_channels: {}", chans.len());
	println!("\r\t num_usable_channels: {}", chans.iter().filter(|c| c.is_usable).count());
	let local_balance_msat = chans.iter().map(|c| c.balance_msat).sum::<u64>();
	println!("\r\t local_balance_msat: {}", local_balance_msat);
	println!("\r\t num_peers: {}", peer_manager.list_peers().len());
	println!("\r}}");
}

fn list_peers(ldk_data_dir: String) {
	let peer_data_path_str = format!("{}/channel_peer_data", ldk_data_dir);
	let peer_data_path = Path::new(peer_data_path_str.as_str());

	let peer_node_ids = read_channel_peer_data(peer_data_path).unwrap();

	if peer_node_ids.is_empty() {
		return;
	}

	println!("\r{{");
	for (pubkey, addr) in peer_node_ids {
		println!("\r\t pubkey: {}@{}", pubkey, addr);
	}
	println!("\r}}");
}

fn list_channels(channel_manager: &Arc<ChannelManager>, network_graph: &Arc<NetworkGraph>) {
	let list_channels = channel_manager.list_channels();

	if list_channels.is_empty() {
		return;
	}

	print!("\r[");
	for chan_info in channel_manager.list_channels() {
		println!("\r");
		println!("\r\t{{");
		println!("\r\t\tchannel_id: {},", chan_info.channel_id);
		if let Some(funding_txo) = chan_info.funding_txo {
			println!("\r\t\tfunding_txid: {},", funding_txo.txid);
		}

		println!(
			"\r\t\tpeer_pubkey: {},",
			hex_utils::hex_str(&chan_info.counterparty.node_id.serialize())
		);
		if let Some(node_info) = network_graph
			.read_only()
			.nodes()
			.get(&NodeId::from_pubkey(&chan_info.counterparty.node_id))
		{
			if let Some(announcement) = &node_info.announcement_info {
				println!("\r\t\tpeer_alias: {}", announcement.alias);
			}
		}

		if let Some(id) = chan_info.short_channel_id {
			println!("\r\t\tshort_channel_id: {},", id);
		}
		println!("\r\t\thtlc_limits: {{");
		println!("\r\t\t\tinbound: {{");
		println!("\r\t\t\t\tminimum_msat: {},", chan_info.inbound_htlc_minimum_msat.unwrap());
		println!("\r\t\t\t\tmaximum_msat: {},", chan_info.inbound_htlc_maximum_msat.unwrap());
		println!("\r\t\t\t}},");
		println!("\r\t\t\toutbound: {{");
		println!(
			"\r\t\t\t\tminimum_msat_configured: {},",
			chan_info.counterparty.outbound_htlc_minimum_msat.unwrap(),
		);
		println!(
			"\r\t\t\t\tminimum_msat_сonsidering_dust: {},",
			chan_info.next_outbound_htlc_minimum_msat,
		);
		println!(
			"\r\t\t\t\tmaximum_msat: {},",
			chan_info.counterparty.outbound_htlc_maximum_msat.unwrap()
		);
		println!("\r\t\t\t}},");
		println!("\r\t\t}},");
		println!("\r\t\tis_channel_ready: {},", chan_info.is_channel_ready);
		println!("\r\t\tchannel_value_satoshis: {},", chan_info.channel_value_satoshis);
		println!("\r\t\toutbound_capacity_msat: {},", chan_info.outbound_capacity_msat);
		if chan_info.is_usable {
			println!(
				"\r\t\tavailable_balance_for_send_msat: {},",
				chan_info.outbound_capacity_msat
			);
			println!("\r\t\tavailable_balance_for_recv_msat: {},", chan_info.inbound_capacity_msat);
			println!(
				"\r\t\tholder_reserved_satoshis: {},",
				chan_info.unspendable_punishment_reserve.unwrap_or(0)
			);
			println!(
				"\r\t\tcounterparty_reserved_satoshis: {},",
				chan_info.counterparty.unspendable_punishment_reserve
			);
		}
		println!("\r\t\tchannel_can_send_payments: {},", chan_info.is_usable);
		println!("\r\t\tpublic: {},", chan_info.is_public);
		if let (Some(holder_pixel), Some(counterparty_pixel)) =
			(chan_info.yuv_holder_pixel, chan_info.yuv_counterparty_pixel)
		{
			println!(
				"\r\t\tyuv_chroma: {},",
				holder_pixel.chroma.to_address(Network::Regtest).to_string()
			);
			println!("\r\t\tholder_yuv_amount: {},", holder_pixel.luma.amount);
			println!("\r\t\tcounterparty_yuv_amount: {},", counterparty_pixel.luma.amount);
		}
		if let Some(pending_update_balances) = chan_info.clone().pending_update_balance {
			println!("\r\t\tupdate_balance: {{");
			println!(
				"\r\t\t\tholder_ready_to_update_balance: {}",
				chan_info
					.clone()
					.update_balance_amounts
					.map_or(0, |update_balances| update_balances.holders_msat)
			);
			println!(
				"\r\t\t\tcounterparty_ready_to_update_balance: {}",
				chan_info
					.clone()
					.update_balance_amounts
					.map_or(0, |update_balances| update_balances.counterpartys_msat)
			);
			if let Some(inbound) = pending_update_balances.inbound_request {
				println!("\r\t\t\tinbound: {{");
				println!("\r\t\t\t\tnew_balance_msat: {},", inbound.inner().new_balance_msat);
				println!(
					"\r\t\t\t\tnew_yuv_pixel_luma: {},",
					inbound.inner().new_yuv_pixel_luma.map_or(0, |luma| luma.amount)
				);
				println!("\r\t\t\t}},");
			}
			if let Some(outbound) = pending_update_balances.outbound_request {
				println!("\r\t\t\toutbound: {{");
				println!("\r\t\t\t\tnew_balance_msat: {},", outbound.inner().new_balance_msat);
				println!(
					"\r\t\t\t\tnew_yuv_pixel_luma: {},",
					outbound.inner().new_yuv_pixel_luma.map_or(0, |luma| luma.amount)
				);
				println!("\r\t\t\t}}");
			}
			println!("\r\t\t}}");
		}
		println!("\r\t}},");
	}
	println!("\r\n]");
}

fn list_payments(inbound_payments: &PaymentInfoStorage, outbound_payments: &PaymentInfoStorage) {
	print!("\r[");

	let inbound_payments = &inbound_payments.payments;
	if !inbound_payments.is_empty() {
		println!();
		for (payment_hash, payment_info) in inbound_payments {
			println!("\r\t{{");
			println!("\r\t\tamount_millisatoshis: {},", payment_info.amt_msat);
			println!("\r\t\tpayment_hash: {},", payment_hash);
			println!("\r\t\thtlc_direction: inbound,");
			println!(
				"\r\t\thtlc_status: {},",
				match payment_info.status {
					HTLCStatus::Pending => "pending",
					HTLCStatus::Succeeded => "succeeded",
					HTLCStatus::Failed => "failed",
				}
			);

			print!("\r\t}},\n\r");
		}
	}

	let outbound_payments = &outbound_payments.payments;
	for (payment_hash, payment_info) in outbound_payments {
		println!();
		println!("\r\t{{");
		println!("\r\t\tamount_millisatoshis: {},", payment_info.amt_msat);
		println!("\r\t\tpayment_hash: {},", payment_hash);
		println!("\r\t\thtlc_direction: outbound,");
		println!(
			"\r\thtlc_status: {},",
			match payment_info.status {
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			}
		);

		println!("\r\t}},\n\r");
	}
	println!("]");
}

pub(crate) async fn connect_peer_if_necessary(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	for peer in peer_manager.list_peers() {
		if peer.counterparty_node_id == pubkey {
			return Ok(());
		}
	}
	let res = do_connect_peer(pubkey, peer_addr, peer_manager).await;
	if res.is_err() {
		println!("\rERROR: failed to connect to peer");
	}
	res
}

pub(crate) async fn do_connect_peer(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), pubkey, peer_addr).await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				tokio::select! {
					_ = &mut connection_closed_future => return Err(()),
					_ = tokio::time::sleep(Duration::from_millis(10)) => {},
				}
				if peer_manager
					.list_peers()
					.iter()
					.find(|details| details.counterparty_node_id == pubkey)
					.is_some()
				{
					return Ok(());
				}
			}
		}
		None => Err(()),
	}
}

fn do_disconnect_peer(
	pubkey: PublicKey, peer_manager: Arc<PeerManager>, channel_manager: Arc<ChannelManager>,
) -> Result<(), ()> {
	//check for open channels with peer
	for channel in channel_manager.list_channels() {
		if channel.counterparty.node_id == pubkey {
			println!(
				"\rError: Node has an active channel with this peer, close any channels first"
			);
			return Err(());
		}
	}

	//check the pubkey matches a valid connected peer
	let peers = peer_manager.list_peers();
	if !peers.iter().any(|peer| pubkey == peer.counterparty_node_id) {
		println!("\rError: Could not find peer {}", pubkey);
		return Err(());
	}

	peer_manager.disconnect_by_node_id(pubkey);
	Ok(())
}

fn open_channel(
	peer_pubkey: PublicKey, channel_amt_sat: u64, config: UserConfig,
	channel_manager: Arc<ChannelManager>, yuv_pixel: Option<Pixel>,
) -> Result<(), ()> {
	match channel_manager.create_channel(
		peer_pubkey,
		channel_amt_sat,
		0,
		0,
		yuv_pixel,
		None,
		Some(config),
	) {
		Ok(_) => {
			println!("\rEVENT: initiated channel with peer {}. ", peer_pubkey);
			Ok(())
		}
		Err(e) => {
			println!("\rERROR: failed to open channel: {:?}", e);
			Err(())
		}
	}
}

fn send_payment(
	channel_manager: &ChannelManager, invoice: &Bolt11Invoice, required_amount_msat: Option<u64>,
	outbound_payments: &mut PaymentInfoStorage, fs_store: Arc<FilesystemStore>,
) {
	let payment_id = PaymentId((*invoice.payment_hash()).to_byte_array());
	let payment_secret = Some(*invoice.payment_secret());
	let zero_amt_invoice =
		invoice.amount_milli_satoshis().is_none() || invoice.amount_milli_satoshis() == Some(0);

	let pay_params_opt = if zero_amt_invoice {
		if let Some(amt_msat) = required_amount_msat {
			payment_parameters_from_zero_amount_invoice(invoice, amt_msat)
		} else {
			println!("Need an amount for the given 0-value invoice");
			print!("> ");
			return;
		}
	} else {
		if required_amount_msat.is_some() && invoice.amount_milli_satoshis() != required_amount_msat
		{
			println!(
				"Amount didn't match invoice value of {}msat",
				invoice.amount_milli_satoshis().unwrap_or(0)
			);
			print!("> ");
			return;
		}
		payment_parameters_from_invoice(invoice)
	};

	let (payment_hash, recipient_onion, route_params) = match pay_params_opt {
		Ok(res) => res,
		Err(e) => {
			println!("Failed to parse invoice: {:?}", e);
			print!("> ");
			return;
		}
	};

	outbound_payments.payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: payment_secret,
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(invoice.amount_milli_satoshis()),
			yuv_pixel: invoice.yuv_pixel(),
		},
	);
	fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound_payments.encode()).unwrap();
	match channel_manager.send_payment(
		payment_hash,
		recipient_onion,
		payment_id,
		route_params,
		Retry::Timeout(Duration::from_secs(10)),
	) {
		Ok(_payment_id) => {
			let payee_pubkey = invoice.recover_payee_pub_key();
			let amt_msat = invoice.amount_milli_satoshis().unwrap();
			println!("\rEVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
		}
		Err(e) => {
			println!("\rERROR: failed to send payment: {:?}", e);
			outbound_payments.payments.get_mut(&payment_hash).unwrap().status = HTLCStatus::Failed;
			fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound_payments.encode()).unwrap();
		}
	};
}

fn keysend<E: EntropySource>(
	channel_manager: &ChannelManager, payee_pubkey: PublicKey, amt_msat: u64, entropy_source: &E,
	outbound_payments: &mut PaymentInfoStorage, fs_store: Arc<FilesystemStore>,
) {
	let payment_preimage = PaymentPreimage(entropy_source.get_secure_random_bytes());
	let payment_hash = PaymentHash::from(payment_preimage);

	let route_params = RouteParameters::from_payment_params_and_value(
		PaymentParameters::for_keysend(payee_pubkey, 40, false),
		amt_msat,
	);
	outbound_payments.payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: None,
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(Some(amt_msat)),
			yuv_pixel: None,
		},
	);
	fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound_payments.encode()).unwrap();
	match channel_manager.send_spontaneous_payment_with_retry(
		Some(payment_preimage),
		RecipientOnionFields::spontaneous_empty(),
		PaymentId(payment_hash.0),
		route_params,
		Retry::Timeout(Duration::from_secs(10)),
	) {
		Ok(_payment_hash) => {
			println!("\rEVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
		}
		Err(e) => {
			println!("\rERROR: failed to send payment: {:?}", e);
			outbound_payments.payments.get_mut(&payment_hash).unwrap().status = HTLCStatus::Failed;
			fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound_payments.encode()).unwrap();
		}
	};
}

fn get_invoice(
	amt_msat: u64, inbound_payments: &mut PaymentInfoStorage, channel_manager: &ChannelManager,
	keys_manager: Arc<KeysManager>, network: Network, expiry_secs: u32, yuv_pixel: Option<Pixel>,
	logger: Arc<disk::FilesystemLogger>,
) {
	let currency = match network {
		Network::Bitcoin => Currency::Bitcoin,
		Network::Testnet => Currency::BitcoinTestnet,
		Network::Regtest => Currency::Regtest,
		Network::Signet => Currency::Signet,
		_ => {
			println!("\rERROR: unsupported network");
			return;
		}
	};

	let duration = SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.expect("for the foreseeable future this shouldn't happen");

	let invoice = if let Some(yuv_pixel) = yuv_pixel {
		utils::create_yuv_invoice_from_channelmanager_and_duration_since_epoch(
			channel_manager,
			keys_manager,
			logger,
			currency,
			Some(amt_msat),
			"ldk-tutorial-node".to_string(),
			duration,
			expiry_secs,
			None,
			yuv_pixel,
		)
	} else {
		utils::create_invoice_from_channelmanager_and_duration_since_epoch(
			channel_manager,
			keys_manager,
			logger,
			currency,
			Some(amt_msat),
			"ldk-tutorial-node".to_string(),
			duration,
			expiry_secs,
			None,
		)
	};

	let invoice = match invoice {
		Ok(inv) => {
			println!("\rSUCCESS: generated invoice: {}", inv);
			inv
		}
		Err(e) => {
			println!("\rERROR: failed to create invoice: {:?}", e);
			return;
		}
	};

	let payment_hash = PaymentHash(*invoice.payment_hash().as_byte_array());
	inbound_payments.payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(Some(amt_msat)),
			yuv_pixel,
		},
	);
}

fn close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager.close_channel(&ChannelId(channel_id), &counterparty_node_id) {
		Ok(()) => println!("\rEVENT: initiating channel close"),
		Err(e) => println!("\rERROR: failed to close channel: {:?}", e),
	}
}

fn force_close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager
		.force_close_broadcasting_latest_txn(&ChannelId(channel_id), &counterparty_node_id)
	{
		Ok(()) => println!("\rEVENT: initiating channel force-close"),
		Err(e) => println!("\rERROR: failed to force-close channel: {:?}", e),
	}
}

fn update_balance(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, new_balance_msat: Option<u64>,
	new_yuv_luma: Option<Luma>, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager.update_balance(UpdateBalance {
		channel_id: ChannelId(channel_id),
		node_id: counterparty_node_id,
		new_balance_msat,
		new_yuv_luma,
	}) {
		Ok(is_applying) => {
			println!("\rEVENT: initiating channel update-balance");
			if is_applying {
				println!("\rEVENT: exchanging commitments with updated balances");
			}
		}
		Err(e) => println!("\rERROR: failed to send update-balance request: {:?}", e),
	}
}

pub(crate) fn parse_peer_info(
	peer_pubkey_and_ip_addr: String,
) -> Result<(PublicKey, SocketAddr), std::io::Error> {
	let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
	let pubkey = pubkey_and_addr.next();
	let peer_addr_str = pubkey_and_addr.next();
	if peer_addr_str.is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: incorrectly formatted peer info. Should be formatted as: `pubkey@host:port`",
		));
	}

	let peer_addr = peer_addr_str.unwrap().to_socket_addrs().map(|mut r| r.next());
	if peer_addr.is_err() || peer_addr.as_ref().unwrap().is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: couldn't parse pubkey@host:port into a socket address",
		));
	}

	let pubkey = hex_utils::to_compressed_pubkey(pubkey.unwrap());
	if pubkey.is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: unable to parse given pubkey for node",
		));
	}

	Ok((pubkey.unwrap(), peer_addr.unwrap().unwrap()))
}
