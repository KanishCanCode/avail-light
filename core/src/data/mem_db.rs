use super::{keys::*, *};
use crate::data::Database;
use std::{
	borrow::Cow,
	collections::{hash_map, HashMap},
	iter,
	sync::{Arc, RwLock},
};

use kad_mem_providers::{ProviderIter, Providers, ProvidersConfig};
use libp2p::kad::{ProviderRecord, Record, RecordKey as DHTRecordKey};
use libp2p::{
	kad::{
		store::{Error, RecordStore, Result},
		KBucketKey,
	},
	PeerId,
};
use tracing::{instrument, trace, Level};
use RecordKey as DBRecordKey;

/// In-memory implementation of a `RecordStore`.
#[derive(Clone)]
pub struct MemoryStore {
	/// The identity of the peer owning the store.
	pub local_key: KBucketKey<PeerId>,
	/// The configuration of the store.
	pub config: MemoryStoreConfig,
	/// The stored (regular) records.
	pub records: HashMap<DHTRecordKey, Record>,
	/// The stored provider records.
	pub providers: Providers,
}

/// Configuration for a `MemoryStore`.
#[derive(Debug, Clone)]
pub struct MemoryStoreConfig {
	/// The maximum number of records.
	pub max_records: usize,
	/// The maximum size of record values, in bytes.
	pub max_value_bytes: usize,
	pub providers: ProvidersConfig,
}

impl Default for MemoryStoreConfig {
	// Default values kept in line with libp2p
	fn default() -> Self {
		Self {
			max_records: 1024,
			max_value_bytes: 65 * 1024,
			providers: Default::default(),
		}
	}
}

impl MemoryStore {
	/// Creates a new `MemoryRecordStore` with a default configuration.
	pub fn new(local_id: PeerId) -> Self {
		Self::with_config(local_id, Default::default())
	}

	/// Creates a new `MemoryRecordStore` with the given configuration.
	pub fn with_config(local_id: PeerId, config: MemoryStoreConfig) -> Self {
		MemoryStore {
			local_key: KBucketKey::from(local_id),
			records: HashMap::default(),
			providers: Providers::with_config(config.providers.clone()),
			config,
		}
	}

	/// Retains the records satisfying a predicate.
	#[instrument(level = Level::TRACE, skip(self, f))]
	pub fn retain<F>(&mut self, f: F)
	where
		F: FnMut(&DHTRecordKey, &mut Record) -> bool,
	{
		self.records.retain(f);
	}

	/// Shrinks the capacity of hashmap as much as possible
	pub fn shrink_hashmap(&mut self) {
		self.records.shrink_to_fit();

		trace!(
			"Memory store - Len: {:?}. Capacity: {:?}",
			self.records.len(),
			self.records.capacity()
		);
	}
}

#[derive(Clone)]
pub struct MemoryDB {
	db_map: Arc<RwLock<HashMap<HashMapKey, String>>>,
	libp2p_mem_store: MemoryStore,
}

#[derive(Eq, Hash, PartialEq)]
pub struct HashMapKey(pub String);

impl MemoryDB {
	pub fn new(local_id: PeerId) -> Self {
		MemoryDB {
			db_map: Default::default(),
			libp2p_mem_store: MemoryStore::new(local_id),
		}
	}
}

impl Database for MemoryDB {
	fn put<T: DBRecordKey>(&self, key: T, value: T::Type) {
		let mut db_map = self.db_map.write().expect("Lock acquired");

		db_map.insert(
			key.into(),
			serde_json::to_string(&value).expect("Encoding data for MemoryDB failed"),
		);
	}

	fn get<T: DBRecordKey>(&self, key: T) -> Option<T::Type> {
		let db_map = self.db_map.read().expect("Lock acquired");
		db_map
			.get(&key.into())
			.map(|value| serde_json::from_str(value).expect("Decoding data from MemoryDB failed"))
	}

	fn delete<T: DBRecordKey>(&self, key: T) {
		let mut db_map = self.db_map.write().expect("Lock acquired");
		db_map.remove(&key.into());
	}
}

impl RecordStore for MemoryDB {
	type RecordsIter<'a> =
		iter::Map<hash_map::Values<'a, DHTRecordKey, Record>, fn(&'a Record) -> Cow<'a, Record>>;

	type ProvidedIter<'a> = ProviderIter<'a>;

	#[instrument(level = Level::TRACE, skip(self))]
	fn get(&self, k: &DHTRecordKey) -> Option<Cow<'_, Record>> {
		self.libp2p_mem_store.records.get(k).map(Cow::Borrowed)
	}

	#[instrument(level = Level::TRACE, skip(self))]
	fn put(&mut self, r: Record) -> Result<()> {
		if r.value.len() >= self.libp2p_mem_store.config.max_value_bytes {
			return Err(Error::ValueTooLarge);
		}

		let num_records = self.libp2p_mem_store.records.len();

		match self.libp2p_mem_store.records.entry(r.key.clone()) {
			hash_map::Entry::Occupied(mut e) => {
				e.insert(r);
			},
			hash_map::Entry::Vacant(e) => {
				if num_records >= self.libp2p_mem_store.config.max_records {
					return Err(Error::MaxRecords);
				}
				e.insert(r);
			},
		}

		Ok(())
	}

	#[instrument(level = Level::TRACE, skip(self))]
	fn remove(&mut self, k: &DHTRecordKey) {
		self.libp2p_mem_store.records.remove(k);
	}

	#[instrument(level = Level::TRACE, skip(self))]
	fn records(&self) -> Self::RecordsIter<'_> {
		self.libp2p_mem_store.records.values().map(Cow::Borrowed)
	}

	fn add_provider(&mut self, record: ProviderRecord) -> Result<()> {
		self.libp2p_mem_store
			.providers
			.add_provider(self.libp2p_mem_store.local_key.clone(), record)
	}

	fn providers(&self, key: &DHTRecordKey) -> Vec<ProviderRecord> {
		self.libp2p_mem_store.providers.providers(key)
	}

	fn provided(&self) -> Self::ProvidedIter<'_> {
		self.libp2p_mem_store.providers.provided()
	}

	fn remove_provider(&mut self, key: &DHTRecordKey, provider: &PeerId) {
		self.libp2p_mem_store
			.providers
			.remove_provider(key, provider)
	}
}

impl From<AppDataKey> for HashMapKey {
	fn from(value: AppDataKey) -> Self {
		let AppDataKey(app_id, block_num) = value;
		HashMapKey(format!(
			"{APP_STATE_CF}:{APP_ID_PREFIX}:{app_id}:{block_num}"
		))
	}
}

impl From<BlockHeaderKey> for HashMapKey {
	fn from(value: BlockHeaderKey) -> Self {
		let BlockHeaderKey(block_num) = value;
		HashMapKey(format!(
			"{APP_STATE_CF}:{BLOCK_HEADER_KEY_PREFIX}:{block_num}"
		))
	}
}

impl From<VerifiedCellCountKey> for HashMapKey {
	fn from(value: VerifiedCellCountKey) -> Self {
		let VerifiedCellCountKey(block_num) = value;
		HashMapKey(format!(
			"{APP_STATE_CF}:{VERIFIED_CELL_COUNT_PREFIX}:{block_num}"
		))
	}
}

impl From<FinalitySyncCheckpointKey> for HashMapKey {
	fn from(_: FinalitySyncCheckpointKey) -> Self {
		HashMapKey(FINALITY_SYNC_CHECKPOINT_KEY.to_string())
	}
}

impl From<RpcNodeKey> for HashMapKey {
	fn from(_: RpcNodeKey) -> Self {
		HashMapKey(CONNECTED_RPC_NODE_KEY.to_string())
	}
}

impl From<IsFinalitySyncedKey> for HashMapKey {
	fn from(_: IsFinalitySyncedKey) -> Self {
		HashMapKey(IS_FINALITY_SYNCED_KEY.to_string())
	}
}

impl From<VerifiedSyncDataKey> for HashMapKey {
	fn from(_: VerifiedSyncDataKey) -> Self {
		HashMapKey(VERIFIED_SYNC_DATA.to_string())
	}
}

impl From<AchievedSyncConfidenceKey> for HashMapKey {
	fn from(_: AchievedSyncConfidenceKey) -> Self {
		HashMapKey(ACHIEVED_SYNC_CONFIDENCE_KEY.to_string())
	}
}

impl From<VerifiedSyncHeaderKey> for HashMapKey {
	fn from(_: VerifiedSyncHeaderKey) -> Self {
		HashMapKey(VERIFIED_SYNC_HEADER_KEY.to_string())
	}
}

impl From<LatestSyncKey> for HashMapKey {
	fn from(_: LatestSyncKey) -> Self {
		HashMapKey(LATEST_SYNC_KEY.to_string())
	}
}

impl From<VerifiedDataKey> for HashMapKey {
	fn from(_: VerifiedDataKey) -> Self {
		HashMapKey(VERIFIED_DATA_KEY.to_string())
	}
}

impl From<AchievedConfidenceKey> for HashMapKey {
	fn from(_: AchievedConfidenceKey) -> Self {
		HashMapKey(ACHIEVED_CONFIDENCE_KEY.to_string())
	}
}

impl From<VerifiedHeaderKey> for HashMapKey {
	fn from(_: VerifiedHeaderKey) -> Self {
		HashMapKey(VERIFIED_HEADER_KEY.to_string())
	}
}

impl From<LatestHeaderKey> for HashMapKey {
	fn from(_: LatestHeaderKey) -> Self {
		HashMapKey(LATEST_HEADER_KEY.to_string())
	}
}

impl From<IsSyncedKey> for HashMapKey {
	fn from(_: IsSyncedKey) -> Self {
		HashMapKey(IS_SYNCED_KEY.to_string())
	}
}

impl From<ClientIdKey> for HashMapKey {
	fn from(_: ClientIdKey) -> Self {
		HashMapKey(CLIENT_ID_KEY.to_string())
	}
}

impl From<P2PKeypairKey> for HashMapKey {
	fn from(_: P2PKeypairKey) -> Self {
		HashMapKey(P2P_KEYPAIR_KEY.to_string())
	}
}
