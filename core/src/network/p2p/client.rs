use color_eyre::{
	eyre::{eyre, WrapErr},
	Report, Result,
};
use futures::future::join_all;
use kate_recovery::{
	config,
	data::Cell,
	matrix::{Dimensions, Position, RowIndex},
};
use libp2p::{
	core::transport::ListenerId,
	kad::{store::RecordStore, Mode, PeerRecord, Quorum, Record, RecordKey},
	swarm::dial_opts::DialOpts,
	Multiaddr, PeerId,
};
use std::time::{Duration, Instant};
use sysinfo::System;
use tokio::sync::oneshot;
use tracing::{debug, info, trace};

use super::{
	event_loop::ConnectionEstablishedInfo, is_global, is_multiaddr_global, Command, CommandSender,
	EventLoop, MultiAddressInfo, PeerInfo, QueryChannel, SendableCommand,
};
use crate::types::MultiaddrConfig;

#[derive(Clone)]
pub struct Client {
	command_sender: CommandSender,
	/// Number of cells to fetch in parallel
	dht_parallelization_limit: usize,
	/// Cell time to live in DHT (in seconds)
	ttl: Duration,
}

struct DHTCell(Cell);

impl DHTCell {
	fn reference(&self, block: u32) -> String {
		self.0.reference(block)
	}

	fn dht_record(&self, block: u32, ttl: Duration) -> Record {
		Record {
			key: self.0.reference(block).as_bytes().to_vec().into(),
			value: self.0.content.to_vec(),
			publisher: None,
			expires: Instant::now().checked_add(ttl),
		}
	}
}
struct DHTRow((RowIndex, Vec<u8>));

impl DHTRow {
	fn reference(&self, block: u32) -> String {
		self.0 .0.reference(block)
	}

	fn dht_record(&self, block: u32, ttl: Duration) -> Record {
		Record {
			key: self.0 .0.reference(block).as_bytes().to_vec().into(),
			value: self.0 .1.clone(),
			publisher: None,
			expires: Instant::now().checked_add(ttl),
		}
	}
}

#[derive(Debug)]
pub struct BlockStat {
	pub total_count: usize,
	pub remaining_counter: usize,
	pub success_counter: usize,
	pub error_counter: usize,
	pub time_stat: u64,
}

impl BlockStat {
	pub fn increase_block_stat_counters(&mut self, cell_number: usize) {
		self.total_count += cell_number;
		self.remaining_counter += cell_number;
	}
}

type RunFn = dyn FnOnce(&mut EventLoop) -> Result<(), Report> + Send;
type AbortFn = dyn FnMut(Report) + Send + Sync;
struct ClosureCommand {
	run_fn: Box<RunFn>,
	abort_fn: Box<AbortFn>,
}

impl Command for ClosureCommand {
	fn run(self: Box<Self>, context: &mut EventLoop) -> Result<(), Report> {
		(self.run_fn)(context)
	}

	fn abort(&mut self, error: Report) {
		(self.abort_fn)(error)
	}
}

fn noop_abort(_error: Report) {
	// deliberately do nothing
}

// helper function which creates commands without specific abort behavior
fn create_command<F>(run: F) -> SendableCommand
where
	F: FnOnce(&mut EventLoop) -> Result<(), Report> + Send + 'static,
{
	Box::new(ClosureCommand {
		run_fn: Box::new(run),
		abort_fn: Box::new(noop_abort),
	})
}
// helper function which creates commands with specific abort behavior
// left currently as an example and a reminder of what can be done
#[allow(dead_code)]
fn create_command_with_abort<R, A>(run: R, abort: A) -> SendableCommand
where
	R: FnOnce(&mut EventLoop) -> Result<(), Report> + Send + 'static,
	A: FnMut(Report) + Send + Sync + 'static,
{
	Box::new(ClosureCommand {
		run_fn: Box::new(run),
		abort_fn: Box::new(abort),
	})
}

impl Client {
	pub fn new(sender: CommandSender, dht_parallelization_limit: usize, ttl: Duration) -> Self {
		Self {
			command_sender: sender,
			dht_parallelization_limit,
			ttl,
		}
	}

	async fn execute_sync<F, T>(&self, command_creator: F) -> Result<T>
	where
		F: FnOnce(oneshot::Sender<Result<T>>) -> SendableCommand,
	{
		let (response_sender, response_receiver) = oneshot::channel();
		let command = command_creator(response_sender);
		self.command_sender
			.send(command)
			.map_err(|_| eyre!("receiver should not be dropped"))?;
		response_receiver
			.await
			.wrap_err("sender should not be dropped")?
	}

	pub async fn start_listening(&self, addrs: Vec<Multiaddr>) -> Result<Vec<ListenerId>> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let results: Result<Vec<ListenerId>, _> = addrs
					.into_iter()
					.map(|addr| context.swarm.listen_on(addr))
					.collect();
				response_sender
					.send(results.map_err(Into::into))
					.map_err(|e| {
						eyre!("Encountered error while sending Start Listening response: {e:?}")
					})?;
				Ok(())
			})
		})
		.await
	}

	pub async fn add_address(&self, peer_id: PeerId, peer_addr: Multiaddr) -> Result<()> {
		self.command_sender
			.send(create_command(move |context: &mut EventLoop| {
				context
					.swarm
					.behaviour_mut()
					.kademlia
					.add_address(&peer_id, peer_addr);
				Ok(())
			}))
			.map_err(|_| eyre!("Failed to send the Add Address Command to the EventLoop"))
	}

	pub async fn dial_peer(
		&self,
		peer_id: PeerId,
		peer_address: Vec<Multiaddr>,
	) -> Result<ConnectionEstablishedInfo> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let opts = DialOpts::peer_id(peer_id).addresses(peer_address).build();
				context.swarm.dial(opts)?;

				context
					.pending_swarm_events
					.insert(peer_id, response_sender);
				Ok(())
			})
		})
		.await
	}

	pub async fn bootstrap(&self) -> Result<()> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let query_id = context.swarm.behaviour_mut().kademlia.bootstrap()?;
				context
					.pending_kad_queries
					.insert(query_id, QueryChannel::Bootstrap(response_sender));
				Ok(())
			})
		})
		.await
	}

	pub async fn add_autonat_server(&self, peer_id: PeerId, address: Multiaddr) -> Result<()> {
		self.command_sender
			.send(create_command(move |context: &mut EventLoop| {
				context
					.swarm
					.behaviour_mut()
					.auto_nat
					.add_server(peer_id, Some(address));
				Ok(())
			}))
			.map_err(|_| eyre!("Failed to send the Add AutoNat Server Command to the EventLoop"))
	}

	pub async fn bootstrap_on_startup(&self, bootstraps: &[MultiaddrConfig]) -> Result<()> {
		for (peer, addr) in bootstraps.iter().map(Into::into) {
			self.dial_peer(peer, vec![addr.clone()])
				.await
				.wrap_err("Dialing Bootstrap peer failed.")?;
			self.add_address(peer, addr.clone()).await?;

			self.add_autonat_server(peer, addr).await?;
		}
		self.bootstrap().await
	}

	async fn get_kad_record(&self, key: RecordKey) -> Result<PeerRecord> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let query_id = context.swarm.behaviour_mut().kademlia.get_record(key);
				context
					.pending_kad_queries
					.insert(query_id, QueryChannel::GetRecord(response_sender));
				Ok(())
			})
		})
		.await
	}

	async fn put_kad_record(
		&self,
		records: Vec<Record>,
		quorum: Quorum,
		block_num: u32,
	) -> Result<()> {
		self.command_sender
			.send(create_command(move |context: &mut EventLoop| {
				context
					.active_blocks
					.entry(block_num)
					// increase the total cell count we monitor if the block entry already exists
					.and_modify(|block| block.increase_block_stat_counters(records.len()))
					// initiate counting for the new block if the block doesn't exist
					.or_insert(BlockStat {
						total_count: records.len(),
						remaining_counter: records.len(),
						success_counter: 0,
						error_counter: 0,
						time_stat: 0,
					});

				for record in records {
					let _ = context
						.swarm
						.behaviour_mut()
						.kademlia
						.put_record(record, quorum);
				}
				Ok(())
			}))
			.map_err(|_| eyre!("Failed to send the Put Kad Record Command to the EventLoop"))
	}

	pub async fn count_dht_entries(&self) -> Result<(usize, usize)> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let mut total_peers: usize = 0;
				let mut peers_with_non_pvt_addr: usize = 0;
				for bucket in context.swarm.behaviour_mut().kademlia.kbuckets() {
					for item in bucket.iter() {
						for address in item.node.value.iter() {
							if is_multiaddr_global(address) {
								peers_with_non_pvt_addr += 1;
								// We just need to hit the first external address
								break;
							}
						}
						total_peers += 1;
					}
				}

				response_sender
					.send(Ok((total_peers, peers_with_non_pvt_addr)))
					.map_err(|e| {
						eyre!("Encountered error while sending Count DHT Entries response: {e:?}")
					})?;
				Ok(())
			})
		})
		.await
	}

	pub async fn list_connected_peers(&self) -> Result<Vec<String>> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let connected_peer_list = context
					.swarm
					.connected_peers()
					.map(|peer_id| peer_id.to_string())
					.collect::<Vec<_>>();

				response_sender.send(Ok(connected_peer_list)).map_err(|e| {
					eyre!("Encountered error while sending List Connected Peers response: {e:?}")
				})?;
				Ok(())
			})
		})
		.await
	}

	pub async fn reconfigure_kademlia_mode(
		&self,
		memory_gb_threshold: f64,
		cpus_threshold: usize,
	) -> Result<Mode> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let external_addresses: Vec<String> = context
					.swarm
					.external_addresses()
					.map(ToString::to_string)
					.collect();
				if matches!(context.kad_mode, Mode::Client) && !external_addresses.is_empty() {
					const BYTES_IN_GB: usize = 1024 * 1024 * 1024;

					let system = System::new_all();
					let memory_gb = system.total_memory() as f64 / BYTES_IN_GB as f64;
					let cpus = system.cpus().len();
					trace!("Total memory: {memory_gb} GB, CPU core count: {cpus}");

					if memory_gb > memory_gb_threshold && cpus > cpus_threshold {
						info!("Switching Kademlia mode to server!");
						context
							.swarm
							.behaviour_mut()
							.kademlia
							.set_mode(Some(Mode::Server));
						context.kad_mode = Mode::Server;
					}
				} else if matches!(context.kad_mode, Mode::Server) && external_addresses.is_empty()
				{
					info!("Peer is not externally reachable, switching to client mode.");
					context
						.swarm
						.behaviour_mut()
						.kademlia
						.set_mode(Some(Mode::Client));
					context.kad_mode = Mode::Client;
				}

				response_sender.send(Ok(context.kad_mode)).map_err(|e| {
					eyre!(
						"Encountered error while sending Reconfigure Kademlia Mode response: {e:?}"
					)
				})?;
				Ok(())
			})
		})
		.await
	}

	pub async fn get_local_peer_info(&self) -> Result<PeerInfo> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let public_listeners: Vec<String> = context
					.swarm
					.external_addresses()
					.filter(|multiaddr| {
						multiaddr.iter().any(
							|protocol| matches!(protocol, libp2p::multiaddr::Protocol::Ip4(ip) if is_global(ip)),
						)
					})
					.map(ToString::to_string)
					.collect();
				let local_listeners: Vec<String> =
					context.swarm.listeners().map(ToString::to_string).collect();
				let external_addresses: Vec<String> = context
					.swarm
					.external_addresses()
					.map(ToString::to_string)
					.collect();

				response_sender
					.send(Ok(PeerInfo {
						peer_id: context.swarm.local_peer_id().to_string(),
						operation_mode: context.kad_mode.to_string(),
						peer_multiaddr: None,
						local_listeners,
						external_listeners: external_addresses,
						public_listeners,
					}))
					.map_err(|e| {
						eyre!("Encountered error while sending Local Peer Info response: {e:?}")
					})?;
				Ok(())
			})
		})
		.await
	}

	pub async fn get_external_peer_info(&self, peer_id: PeerId) -> Result<MultiAddressInfo> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let mut multiaddresses: Vec<String> = Vec::new();

				for bucket in context.swarm.behaviour_mut().kademlia.kbuckets() {
					for item in bucket.iter() {
						if *item.node.key.preimage() == peer_id {
							for addr in item.node.value.iter() {
								multiaddresses.push(addr.to_string());
							}
						}
					}
				}

				response_sender
					.send(Ok(MultiAddressInfo {
						multiaddresses,
						peer_id: peer_id.to_string(),
					}))
					.map_err(|e| {
						eyre!("Encountered error while sending External Peer Info response: {e:?}")
					})?;
				Ok(())
			})
		})
		.await
	}

	// Reduces the size of Kademlias underlying hashmap
	pub async fn shrink_kademlia_map(&self) -> Result<()> {
		self.command_sender
			.send(create_command(|context: &mut EventLoop| {
				context
					.swarm
					.behaviour_mut()
					.kademlia
					.store_mut()
					.shrink_hashmap();

				Ok(())
			}))
			.map_err(|_| eyre!("Failed to send the Shrink Kademlia Map Command to the EventLoop"))
	}

	pub async fn get_kademlia_map_size(&self) -> Result<usize> {
		self.execute_sync(|response_sender| {
			create_command(move |context: &mut EventLoop| {
				let size = context
					.swarm
					.behaviour_mut()
					.kademlia
					.store_mut()
					.records()
					.count();

				response_sender.send(Ok(size)).map_err(|e| {
					eyre!("Encountered error while sending Get Kademlia Map Size response: {e:?}")
				})?;
				Ok(())
			})
		})
		.await
	}

	pub async fn prune_expired_records(&self, now: Instant) -> Result<usize> {
		self.execute_sync(|response_sender| {
			if cfg!(feature = "kademlia-rocksdb") {
				create_command(move |_| {
					response_sender.send(Ok(0)).map_err(|e| {
						eyre!(
							"Encountered error while sending Prune Expired Records response: {e:?}"
						)
					})?;
					Ok(())
				})
			} else {
				create_command(move |context: &mut EventLoop| {
					let store = context.swarm.behaviour_mut().kademlia.store_mut();

					let before = store.records().count();
					store.retain(|_, record| !record.is_expired(now));
					let after = store.records().count();

					response_sender.send(Ok(before - after)).map_err(|e| {
						eyre!(
							"Encountered error while sending Prune Expired Records response: {e:?}"
						)
					})?;
					Ok(())
				})
			}
		})
		.await
	}

	// Since callers ignores DHT errors, debug logs are used to observe DHT behavior.
	// Return type assumes that cell is not found in case when error is present.
	async fn fetch_cell_from_dht(&self, block_number: u32, position: Position) -> Option<Cell> {
		let reference = position.reference(block_number);
		let record_key = RecordKey::from(reference.as_bytes().to_vec());

		trace!("Getting DHT record for reference {}", reference);

		match self.get_kad_record(record_key).await {
			Ok(peer_record) => {
				trace!("Fetched cell {reference} from the DHT");

				let try_content: Result<[u8; config::COMMITMENT_SIZE + config::CHUNK_SIZE], _> =
					peer_record.record.value.try_into();

				let Ok(content) = try_content else {
					debug!("Cannot convert cell {reference} into 80 bytes");
					return None;
				};

				Some(Cell { position, content })
			},
			Err(error) => {
				trace!("Cell {reference} not found in the DHT: {error}");
				None
			},
		}
	}

	async fn fetch_row_from_dht(
		&self,
		block_number: u32,
		row_index: u32,
	) -> Option<(u32, Vec<u8>)> {
		let row_index = RowIndex(row_index);
		let reference = row_index.reference(block_number);
		let record_key = RecordKey::from(reference.as_bytes().to_vec());

		trace!("Getting DHT record for reference {}", reference);

		match self.get_kad_record(record_key).await {
			Ok(peer_record) => Some((row_index.0, peer_record.record.value)),
			Err(error) => {
				debug!("Row {reference} not found in the DHT: {error}");
				None
			},
		}
	}

	/// Fetches cells from DHT.
	/// Returns fetched cells and unfetched positions (so we can try RPC fetch).
	///
	/// # Arguments
	///
	/// * `block_number` - Block number
	/// * `positions` - Cell positions to fetch
	pub async fn fetch_cells_from_dht(
		&self,
		block_number: u32,
		positions: &[Position],
	) -> (Vec<Cell>, Vec<Position>) {
		let mut cells = Vec::<Option<Cell>>::with_capacity(positions.len());

		for positions in positions.chunks(self.dht_parallelization_limit) {
			let fetch = |&position| self.fetch_cell_from_dht(block_number, position);
			let results = join_all(positions.iter().map(fetch)).await;
			cells.extend(results.into_iter().collect::<Vec<_>>());
		}

		let unfetched = cells
			.iter()
			.zip(positions)
			.filter(|(cell, _)| cell.is_none())
			.map(|(_, &position)| position)
			.collect::<Vec<_>>();

		let fetched = cells.into_iter().flatten().collect();

		(fetched, unfetched)
	}

	/// Fetches rows from DHT.
	/// Returns fetched rows and unfetched row indexes (so we can try RPC fetch).
	///
	/// # Arguments
	///
	/// * `block_number` - Block number
	/// * `rows` - Row indexes to fetch
	pub async fn fetch_rows_from_dht(
		&self,
		block_number: u32,
		dimensions: Dimensions,
		row_indexes: &[u32],
	) -> Vec<Option<Vec<u8>>> {
		let mut rows = vec![None; dimensions.extended_rows() as usize];
		for row_indexes in row_indexes.chunks(self.dht_parallelization_limit) {
			let fetch = |row| self.fetch_row_from_dht(block_number, row);
			let fetched_rows = join_all(row_indexes.iter().cloned().map(fetch)).await;
			for (row_index, row) in fetched_rows.into_iter().flatten() {
				rows[row_index as usize] = Some(row);
			}
		}
		rows
	}

	async fn insert_into_dht(&self, records: Vec<(String, Record)>, block_num: u32) -> Result<()> {
		if records.is_empty() {
			return Err(eyre!("Cant send empty record list."));
		}
		self.put_kad_record(
			records.into_iter().map(|e| e.1).collect(),
			Quorum::One,
			block_num,
		)
		.await
	}

	/// Inserts cells into the DHT.
	/// There is no rollback, and errors will be logged and skipped,
	/// which means that we cannot rely on error logs as alert mechanism.
	/// Returns the success rate of the PUT operations measured by dividing
	/// the number of returned errors with the total number of input cells
	///
	/// # Arguments
	///
	/// * `block` - Block number
	/// * `cells` - Matrix cells to store into DHT
	pub async fn insert_cells_into_dht(&self, block: u32, cells: Vec<Cell>) -> Result<()> {
		let records: Vec<_> = cells
			.into_iter()
			.map(DHTCell)
			.map(|cell| (cell.reference(block), cell.dht_record(block, self.ttl)))
			.collect::<Vec<_>>();
		self.insert_into_dht(records, block).await
	}

	/// Inserts rows into the DHT.
	/// There is no rollback, and errors will be logged and skipped,
	/// which means that we cannot rely on error logs as alert mechanism.
	/// Returns the success rate of the PUT operations measured by dividing
	/// the number of returned errors with the total number of input rows
	///
	/// # Arguments
	///
	/// * `block` - Block number
	/// * `rows` - Matrix rows to store into DHT
	pub async fn insert_rows_into_dht(
		&self,
		block: u32,
		rows: Vec<(RowIndex, Vec<u8>)>,
	) -> Result<()> {
		let records: Vec<_> = rows
			.into_iter()
			.map(DHTRow)
			.map(|row| (row.reference(block), row.dht_record(block, self.ttl)))
			.collect::<Vec<_>>();

		self.insert_into_dht(records, block).await
	}
}
