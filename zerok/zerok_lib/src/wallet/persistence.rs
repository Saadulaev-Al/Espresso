use crate::key_set::OrderByOutputs;
use crate::set_merkle_tree::SetMerkleTree;
use crate::wallet::*;
use crate::{ProverKeySet, ValidatorState};
use arbitrary::Arbitrary;
use async_std::sync::Arc;
use atomic_store::{
    error::PersistenceError,
    load_store::{BincodeLoadStore, LoadStore},
    AppendLog, AtomicStore, AtomicStoreLoader, RollingLog,
};
use encryption::Cipher;
use hd::KeyTree;
use jf_txn::keys::{AuditorKeyPair, FreezerKeyPair, UserKeyPair};
use jf_txn::structs::AssetDefinition;
use rand_chacha::{rand_core::SeedableRng, ChaChaRng};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use snafu::ResultExt;
use std::path::PathBuf;

pub trait WalletLoader {
    type Meta; // Metadata stored in plaintext and used by the loader to access the wallet.
    fn location(&self) -> PathBuf;
    fn create(&mut self) -> Result<(Self::Meta, KeyTree), WalletError>;
    fn load(&mut self, meta: &Self::Meta) -> Result<KeyTree, WalletError>;

    /// This function can be overridden to create wallets with a particular public key.
    ///
    /// By default, this function returns None, and a random key pair will be generated.
    fn key_pair(&self) -> Option<UserKeyPair> {
        None
    }
}

// Serialization intermediate for the static part of a WalletState.
#[derive(Deserialize, Serialize)]
struct WalletStaticState<'a> {
    proving_keys: Arc<ProverKeySet<'a, OrderByOutputs>>,
    key_pair: UserKeyPair,
    auditor_key_pair: AuditorKeyPair,
    freezer_key_pair: FreezerKeyPair,
}

impl<'a> From<&WalletState<'a>> for WalletStaticState<'a> {
    fn from(w: &WalletState<'a>) -> Self {
        Self {
            proving_keys: w.proving_keys.clone(),
            key_pair: w.key_pair.clone(),
            auditor_key_pair: w.auditor_key_pair.clone(),
            freezer_key_pair: w.freezer_key_pair.clone(),
        }
    }
}

// Serialization intermediate for the dynamic part of a WalletState.
#[ser_test(arbitrary, ark(false))]
#[derive(Arbitrary, Debug, Deserialize, Serialize, PartialEq)]
struct WalletSnapshot {
    now: u64,
    validator: ValidatorState,
    records: RecordDatabase,
    nullifiers: SetMerkleTree,
    transactions: TransactionDatabase,
}

impl<'a> From<&WalletState<'a>> for WalletSnapshot {
    fn from(w: &WalletState<'a>) -> Self {
        Self {
            now: w.now,
            validator: w.validator.clone(),
            records: w.records.clone(),
            nullifiers: w.nullifiers.clone(),
            transactions: w.transactions.clone(),
        }
    }
}

struct EncryptingResourceAdapter<T> {
    cipher: Cipher<ChaChaRng>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> EncryptingResourceAdapter<T> {
    fn new(key: KeyTree) -> Self {
        Self {
            cipher: Cipher::new(key, ChaChaRng::from_entropy()),
            _phantom: Default::default(),
        }
    }

    fn cast<S>(&self) -> EncryptingResourceAdapter<S> {
        EncryptingResourceAdapter {
            cipher: self.cipher.clone(),
            _phantom: Default::default(),
        }
    }
}

impl<T: Serialize + DeserializeOwned> LoadStore for EncryptingResourceAdapter<T> {
    type ParamType = T;

    fn load(&self, stream: &[u8]) -> Result<Self::ParamType, PersistenceError> {
        let ciphertext = bincode::deserialize(stream)
            .map_err(|source| PersistenceError::BincodeDeError { source })?;
        let plaintext =
            self.cipher
                .decrypt(&ciphertext)
                .map_err(|err| PersistenceError::OtherLoadError {
                    inner: Box::new(err),
                })?;
        bincode::deserialize(&plaintext)
            .map_err(|source| PersistenceError::BincodeDeError { source })
    }

    fn store(&mut self, param: &Self::ParamType) -> Result<Vec<u8>, PersistenceError> {
        let plaintext = bincode::serialize(param)
            .map_err(|source| PersistenceError::BincodeSerError { source })?;
        let ciphertext =
            self.cipher
                .encrypt(&plaintext)
                .map_err(|err| PersistenceError::OtherStoreError {
                    inner: Box::new(err),
                })?;
        bincode::serialize(&ciphertext)
            .map_err(|source| PersistenceError::BincodeSerError { source })
    }
}

pub struct AtomicWalletStorage<'a, Meta: Serialize + DeserializeOwned> {
    store: AtomicStore,
    // Metadata given at initialization time that may not have been written to disk yet.
    meta: Meta,
    // Persisted metadata, if the wallet has already been committed to disk. This is a snapshot log
    // which only ever has at most 1 entry. It is reprsented as a log, rather than a plain file,
    // solely so that we can use the transaction mechanism of AtomicStore to ensure that the
    // metadata and static data are persisted to disk atomically when the wallet is created.
    persisted_meta: RollingLog<BincodeLoadStore<Meta>>,
    meta_dirty: bool,
    // Snapshot log with a single entry containing the static data.
    static_data: RollingLog<EncryptingResourceAdapter<WalletStaticState<'a>>>,
    static_dirty: bool,
    dynamic_state: RollingLog<EncryptingResourceAdapter<WalletSnapshot>>,
    dynamic_state_dirty: bool,
    auditable_assets: AppendLog<EncryptingResourceAdapter<AssetDefinition>>,
    auditable_assets_dirty: bool,
    defined_assets: AppendLog<EncryptingResourceAdapter<(AssetDefinition, AssetCodeSeed, Vec<u8>)>>,
    defined_assets_dirty: bool,
}

impl<'a, Meta: Send + Serialize + DeserializeOwned> AtomicWalletStorage<'a, Meta> {
    pub fn new(loader: &mut impl WalletLoader<Meta = Meta>) -> Result<Self, WalletError> {
        let directory = loader.location();
        let mut atomic_loader =
            AtomicStoreLoader::load(&directory, "wallet").context(PersistenceError)?;

        // Load the metadata first so the loader can use it to generate the encryption key needed to
        // read the rest of the data.
        let persisted_meta = RollingLog::load(
            &mut atomic_loader,
            BincodeLoadStore::default(),
            "wallet_meta",
            1024,
        )
        .context(PersistenceError)?;
        let (meta, key) = match persisted_meta.load_latest() {
            Ok(meta) => {
                let key = loader.load(&meta)?;
                (meta, key)
            }
            Err(_) => {
                // If there is no persisted metadata, ask the loader to generate a new wallet.
                loader.create()?
            }
        };

        let adaptor = EncryptingResourceAdapter::<()>::new(key);
        let static_data =
            RollingLog::load(&mut atomic_loader, adaptor.cast(), "wallet_static", 1024)
                .context(PersistenceError)?;
        let dynamic_state =
            RollingLog::load(&mut atomic_loader, adaptor.cast(), "wallet_dyn", 1024)
                .context(PersistenceError)?;
        let auditable_assets =
            AppendLog::load(&mut atomic_loader, adaptor.cast(), "wallet_aud", 1024)
                .context(PersistenceError)?;
        let defined_assets =
            AppendLog::load(&mut atomic_loader, adaptor.cast(), "wallet_def", 1024)
                .context(PersistenceError)?;
        let store = AtomicStore::open(atomic_loader).context(PersistenceError)?;

        Ok(Self {
            meta,
            persisted_meta,
            meta_dirty: false,
            static_data,
            static_dirty: false,
            store,
            dynamic_state,
            dynamic_state_dirty: false,
            auditable_assets,
            auditable_assets_dirty: false,
            defined_assets,
            defined_assets_dirty: false,
        })
    }

    pub async fn create(mut self: &mut Self, w: &WalletState<'a>) -> Result<(), WalletError> {
        // Store the initial static and dynamic state, and the metadata. We do this in a closure so
        // that if any operation fails, it will exit the closure but not this function, and we can
        // then commit or revert based on the results of the closure.
        let store = &mut self;
        match (|| async move {
            store
                .persisted_meta
                .store_resource(&store.meta)
                .context(PersistenceError)?;
            store.meta_dirty = true;
            store
                .static_data
                .store_resource(&WalletStaticState::from(w))
                .context(PersistenceError)?;
            store.static_dirty = true;
            store.store_snapshot(w).await
        })()
        .await
        {
            Ok(()) => {
                self.commit().await;
                Ok(())
            }
            Err(err) => {
                self.revert().await;
                Err(err)
            }
        }
    }
}

#[async_trait]
impl<'a, Meta: Send + Serialize + DeserializeOwned> WalletStorage<'a>
    for AtomicWalletStorage<'a, Meta>
{
    fn exists(&self) -> bool {
        self.persisted_meta.load_latest().is_ok()
    }

    async fn load(&mut self) -> Result<WalletState<'a>, WalletError> {
        let static_state = self.static_data.load_latest().context(PersistenceError)?;
        let dynamic_state = self.dynamic_state.load_latest().context(PersistenceError)?;

        Ok(WalletState {
            // Static state
            proving_keys: static_state.proving_keys,
            key_pair: static_state.key_pair,
            auditor_key_pair: static_state.auditor_key_pair,
            freezer_key_pair: static_state.freezer_key_pair,

            // Dynamic state
            validator: dynamic_state.validator,
            now: dynamic_state.now,
            records: dynamic_state.records,
            nullifiers: dynamic_state.nullifiers,
            transactions: dynamic_state.transactions,

            // Monotonic state
            auditable_assets: self
                .auditable_assets
                .iter()
                .filter_map(|res| res.map(|def| (def.code, def)).ok())
                .collect(),
            defined_assets: self
                .defined_assets
                .iter()
                .filter_map(|res| {
                    res.map(|(def, seed, desc)| (def.code, (def, seed, desc)))
                        .ok()
                })
                .collect(),
        })
    }

    async fn store_snapshot(&mut self, w: &WalletState<'a>) -> Result<(), WalletError> {
        self.dynamic_state
            .store_resource(&WalletSnapshot::from(w))
            .context(PersistenceError)?;
        self.dynamic_state_dirty = true;
        Ok(())
    }

    async fn store_auditable_asset(&mut self, asset: &AssetDefinition) -> Result<(), WalletError> {
        self.auditable_assets
            .store_resource(asset)
            .context(PersistenceError)?;
        self.auditable_assets_dirty = true;
        Ok(())
    }

    async fn store_defined_asset(
        &mut self,
        asset: &AssetDefinition,
        seed: AssetCodeSeed,
        desc: &[u8],
    ) -> Result<(), WalletError> {
        self.defined_assets
            .store_resource(&(asset.clone(), seed, desc.to_vec()))
            .context(PersistenceError)?;
        self.defined_assets_dirty = true;
        Ok(())
    }

    async fn commit(&mut self) {
        {
            if self.meta_dirty {
                self.persisted_meta.commit_version().unwrap();
            } else {
                self.persisted_meta.skip_version().unwrap();
            }

            if self.static_dirty {
                self.static_data.commit_version().unwrap();
            } else {
                self.static_data.skip_version().unwrap();
            }

            if self.dynamic_state_dirty {
                self.dynamic_state.commit_version().unwrap();
            } else {
                self.dynamic_state.skip_version().unwrap();
            }

            if self.auditable_assets_dirty {
                self.auditable_assets.commit_version().unwrap();
            } else {
                self.auditable_assets.skip_version().unwrap();
            }

            if self.defined_assets_dirty {
                self.defined_assets.commit_version().unwrap();
            } else {
                self.defined_assets.skip_version().unwrap();
            }
        }

        self.store.commit_version().unwrap();

        self.meta_dirty = false;
        self.static_dirty = false;
        self.dynamic_state_dirty = false;
        self.auditable_assets_dirty = false;
        self.defined_assets_dirty = false;
    }

    async fn revert(&mut self) {
        self.persisted_meta.revert_version().unwrap();
        self.static_data.revert_version().unwrap();
        self.dynamic_state.revert_version().unwrap();
        self.auditable_assets.revert_version().unwrap();
        self.defined_assets.revert_version().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VerifierKeySet, MERKLE_HEIGHT, UNIVERSAL_PARAM};
    use jf_txn::{KeyPair, TransactionVerifyingKey};
    use phaselock::H_256;
    use rand_chacha::{
        rand_core::{RngCore, SeedableRng},
        ChaChaRng,
    };
    use std::iter::repeat_with;
    use tempdir::TempDir;
    use test_helpers::*;

    struct MockWalletLoader {
        dir: TempDir,
        key: KeyTree,
    }

    impl WalletLoader for MockWalletLoader {
        type Meta = ();

        fn location(&self) -> PathBuf {
            self.dir.path().into()
        }

        fn create(&mut self) -> Result<(Self::Meta, KeyTree), WalletError> {
            Ok(((), self.key.clone()))
        }

        fn load(&mut self, _meta: &Self::Meta) -> Result<KeyTree, WalletError> {
            Ok(self.key.clone())
        }
    }

    fn random_ro(rng: &mut ChaChaRng, key_pair: &UserKeyPair) -> RecordOpening {
        let amount = rng.next_u64();
        RecordOpening::new(
            rng,
            amount,
            AssetDefinition::native(),
            key_pair.pub_key(),
            FreezeFlag::Unfrozen,
        )
    }

    fn random_ros(rng: &mut ChaChaRng, key_pair: &UserKeyPair) -> Vec<RecordOpening> {
        repeat_with(|| random_ro(rng, key_pair)).take(3).collect()
    }

    fn random_memos(rng: &mut ChaChaRng, key_pair: &UserKeyPair) -> (Vec<ReceiverMemo>, Signature) {
        let memos = repeat_with(|| {
            let ro = random_ro(rng, key_pair);
            ReceiverMemo::from_ro(rng, &ro, &[]).unwrap()
        })
        .take(3)
        .collect::<Vec<_>>();
        let sig = sign_receiver_memos(&KeyPair::generate(rng), &memos).unwrap();
        (memos, sig)
    }

    fn random_txn_hash(rng: &mut ChaChaRng) -> ElaboratedTransactionHash {
        let mut hash = [0; H_256];
        rng.fill_bytes(&mut hash);
        let ret =
            crate::commit::RawCommitmentBuilder::<ElaboratedTransaction>::new("random_txn_hash")
                .fixed_size_bytes(&hash)
                .finalize();
        ElaboratedTransactionHash(ret)
    }

    async fn get_test_state(name: &str) -> (WalletState<'static>, MockWalletLoader, ChaChaRng) {
        let mut rng = ChaChaRng::from_seed([0x42u8; 32]);

        // Pick a few different sizes. It doesn't matter since all we're going to be doing is
        // serializing and deserializing, but try to choose representative data.
        let xfr_sizes = [(1, 2), (2, 3), (3, 3)];

        let mut xfr_prove_keys = vec![];
        let mut xfr_verif_keys = vec![];
        for (num_inputs, num_outputs) in xfr_sizes {
            let (xfr_prove_key, xfr_verif_key, _) = jf_txn::proof::transfer::preprocess(
                &*UNIVERSAL_PARAM,
                num_inputs,
                num_outputs,
                MERKLE_HEIGHT,
            )
            .unwrap();
            xfr_prove_keys.push(xfr_prove_key);
            xfr_verif_keys.push(TransactionVerifyingKey::Transfer(xfr_verif_key));
        }
        let (mint_prove_key, mint_verif_key, _) =
            jf_txn::proof::mint::preprocess(&*UNIVERSAL_PARAM, MERKLE_HEIGHT).unwrap();
        let (freeze_prove_key, freeze_verif_key, _) =
            jf_txn::proof::freeze::preprocess(&*UNIVERSAL_PARAM, 2, MERKLE_HEIGHT).unwrap();
        let record_merkle_tree = MerkleTree::new(MERKLE_HEIGHT).unwrap();
        let validator = ValidatorState::new(
            VerifierKeySet {
                xfr: KeySet::new(xfr_verif_keys.into_iter()).unwrap(),
                mint: TransactionVerifyingKey::Mint(mint_verif_key),
                freeze: KeySet::new(
                    vec![TransactionVerifyingKey::Freeze(freeze_verif_key)].into_iter(),
                )
                .unwrap(),
            },
            record_merkle_tree,
        );

        let state = WalletState {
            proving_keys: Arc::new(ProverKeySet {
                xfr: KeySet::new(xfr_prove_keys.into_iter()).unwrap(),
                freeze: KeySet::new(vec![freeze_prove_key].into_iter()).unwrap(),
                mint: mint_prove_key,
            }),
            key_pair: UserKeyPair::generate(&mut rng),
            auditor_key_pair: AuditorKeyPair::generate(&mut rng),
            freezer_key_pair: FreezerKeyPair::generate(&mut rng),
            validator,
            now: 0,

            records: Default::default(),
            auditable_assets: Default::default(),
            nullifiers: Default::default(),
            defined_assets: Default::default(),
            transactions: Default::default(),
        };

        let mut loader = MockWalletLoader {
            dir: TempDir::new(name).unwrap(),
            key: KeyTree::random(&mut rng),
        };
        {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            assert!(!storage.exists());
            storage.create(&state).await.unwrap();
            assert!(storage.exists());
        }

        (state, loader, rng)
    }

    #[async_std::test]
    async fn test_round_trip() -> std::io::Result<()> {
        let (mut stored, mut loader, mut rng) = get_test_state("test_round_trip").await;

        // Create a new storage instance to load the wallet back from disk, to ensure that what we
        // load comes only from persistent storage and not from any in-memory state of the first
        // instance.
        let loaded = {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage.load().await.unwrap()
        };
        assert_wallet_states_eq(&stored, &loaded);

        // Modify some dynamic state and load the wallet again.
        let ro = random_ro(&mut rng, &stored.key_pair);
        let comm = RecordCommitment::from(&ro);
        stored
            .validator
            .record_merkle_frontier
            .push(comm.to_field_element());
        stored
            .validator
            .past_record_merkle_roots
            .0
            .push_back(stored.validator.record_merkle_root);
        stored.validator.record_merkle_root =
            stored.validator.record_merkle_frontier.get_root_value();
        let mut nullifiers = SetMerkleTree::default();
        nullifiers.insert(Nullifier::random_for_test(&mut rng));
        stored.validator.nullifiers_root = nullifiers.hash();
        stored.nullifiers = nullifiers;
        stored.now += 1;
        stored.records.insert(
            ro,
            stored.validator.record_merkle_frontier.num_leaves(),
            &stored.key_pair,
        );
        let (receiver_memos, signature) = random_memos(&mut rng, &stored.key_pair);
        let txn_uid = TransactionUID(random_txn_hash(&mut rng));
        let txn = PendingTransaction {
            receiver_memos,
            signature,
            freeze_outputs: random_ros(&mut rng, &stored.key_pair),
            timeout: 5000,
            uid: txn_uid.clone(),
            hash: random_txn_hash(&mut rng),
        };
        stored.transactions.insert_pending(txn);
        stored.transactions.await_memos(txn_uid, vec![1, 2, 3]);

        // Snapshot the modified dynamic state and then reload.
        {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage.store_snapshot(&stored).await.unwrap();
            storage.commit().await;
        }
        let loaded = {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage.load().await.unwrap()
        };
        assert_wallet_states_eq(&stored, &loaded);

        // Append to monotonic state and then reload.
        let asset =
            AssetDefinition::new(AssetCode::random(&mut rng).0, Default::default()).unwrap();
        stored.auditable_assets.insert(asset.code, asset.clone());
        {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage.store_auditable_asset(&asset).await.unwrap();
            storage.commit().await;
        }
        let loaded = {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage.load().await.unwrap()
        };
        assert_wallet_states_eq(&stored, &loaded);

        let (code, seed) = AssetCode::random(&mut rng);
        let asset = AssetDefinition::new(code, Default::default()).unwrap();
        stored
            .defined_assets
            .insert(asset.code, (asset.clone(), seed, vec![]));
        {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage
                .store_defined_asset(&asset, seed, &[])
                .await
                .unwrap();
            storage.commit().await;
        }
        let loaded = {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage.load().await.unwrap()
        };
        assert_wallet_states_eq(&stored, &loaded);

        Ok(())
    }

    #[async_std::test]
    async fn test_revert() -> std::io::Result<()> {
        let (mut stored, mut loader, mut rng) = get_test_state("test_revert").await;

        // Make a change to one of the data structures, but revert it.
        let loaded = {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();
            storage
                .store_auditable_asset(&AssetDefinition::native())
                .await
                .unwrap();
            storage.revert().await;
            // Make sure committing after a revert does not commit the reverted changes.
            storage.commit().await;
            storage.load().await.unwrap()
        };
        assert_wallet_states_eq(&stored, &loaded);

        // Change multiple data structures and revert.
        let loaded = {
            let mut storage = AtomicWalletStorage::new(&mut loader).unwrap();

            let (code, seed) = AssetCode::random(&mut rng);
            let asset = AssetDefinition::new(code, Default::default()).unwrap();
            let ro = random_ro(&mut rng, &stored.key_pair);
            let nullifier = stored.key_pair.nullify(
                ro.asset_def.policy_ref().freezer_pub_key(),
                0,
                &RecordCommitment::from(&ro),
            );

            // Store some data.
            stored.records.insert(ro, 0, &stored.key_pair);
            storage.store_snapshot(&stored).await.unwrap();
            storage.store_auditable_asset(&asset).await.unwrap();
            storage
                .store_defined_asset(&asset, seed, &[])
                .await
                .unwrap();
            // Revert the changes.
            stored.records.remove_by_nullifier(nullifier).unwrap();
            storage.revert().await;

            // Commit after revert should be a no-op.
            storage.commit().await;
            storage.load().await.unwrap()
        };
        assert_wallet_states_eq(&stored, &loaded);

        Ok(())
    }
}