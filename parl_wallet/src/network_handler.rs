use std::{
    collections::{
        HashMap,
        HashSet
    },
    sync::Arc,
    time::Duration
};
use thiserror::Error;
use anyhow::Error;
use log::{debug, error, trace, warn};
use tokio::{sync::Mutex, task::JoinHandle, time::sleep};
use parl_common::{
    account::CiphertextCache,
    api::{
        daemon::{
            BlockResponse,
            NewBlockEvent
        },
        wallet::BalanceChanged,
        RPCTransactionType
    },
    asset::AssetWithData,
    config::PARL_ASSET,
    crypto::{
        elgamal::Ciphertext,
        Address,
        Hash
    },
    serializer::Serializer,
    transaction::Role,
    utils::{sanitize_daemon_address, spawn_task}
};
use crate::{
    config::AUTO_RECONNECT_INTERVAL,
    daemon_api::DaemonAPI,
    entry::{
        EntryData,
        TransactionEntry,
        TransferIn,
        TransferOut
    },
    storage::Balance,
    wallet::{
        Event, Wallet
    }
};

// NetworkHandler must be behind a Arc to be accessed from Wallet (to stop it) or from tokio task
pub type SharedNetworkHandler = Arc<NetworkHandler>;

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("network handler is already running")]
    AlreadyRunning,
    #[error("network handler is not running")]
    NotRunning,
    #[error(transparent)]
    TaskError(#[from] tokio::task::JoinError),
    #[error(transparent)]
    DaemonAPIError(#[from] Error),
    #[error("Network mismatch")]
    NetworkMismatch
}

pub struct NetworkHandler {
    // tokio task
    task: Mutex<Option<JoinHandle<Result<(), Error>>>>,
    // wallet where we can save every data from chain
    wallet: Arc<Wallet>,
    // api to communicate with daemon
    // It is behind a Arc to be shared across several wallets
    // in case someone make a custom service and don't want to create a new connection
    api: Arc<DaemonAPI>
}

impl NetworkHandler {
    // Create a new network handler with a wallet and a daemon address
    // This will create itself a DaemonAPI and verify if connection is possible
    pub async fn new<S: ToString>(wallet: Arc<Wallet>, daemon_address: S) -> Result<SharedNetworkHandler, Error> {
        let s = daemon_address.to_string();
        let api = DaemonAPI::new(format!("{}/json_rpc", sanitize_daemon_address(s.as_str()))).await?;
        Self::with_api(wallet, Arc::new(api)).await
    }

    // Create a new network handler with an already created daemon API
    pub async fn with_api(wallet: Arc<Wallet>, api: Arc<DaemonAPI>) -> Result<SharedNetworkHandler, Error> {
        // check that we can correctly get version from daemon
        let version = api.get_version().await?;
        debug!("Connected to daemon running version {}", version);

        Ok(Arc::new(Self {
            task: Mutex::new(None),
            wallet,
            api
        }))
    }

    // Start the internal loop to sync all missed blocks and all newly added blocks
    pub async fn start(self: &Arc<Self>, auto_reconnect: bool) -> Result<(), NetworkError> {
        trace!("Starting network handler");

        if self.is_running().await {
            return Err(NetworkError::AlreadyRunning)
        }

        if !self.api.is_online() {
            debug!("API is offline, trying to reconnect");
            if !self.api.reconnect().await? {
                error!("Couldn't reconnect to server");
                return Err(NetworkError::NotRunning)
            }
        }

        let zelf = Arc::clone(&self);
        *self.task.lock().await = Some(spawn_task("network-handler", async move {
            loop {
                let res =  zelf.start_syncing().await;
                if let Err(e) = res.as_ref() {
                    error!("Error while syncing: {}", e);
                }

                // Notify that we are offline
                zelf.wallet.propagate_event(Event::Offline).await;

                if !auto_reconnect {
                    // Turn off the websocket connection
                    if let Err(e) = zelf.api.disconnect().await {
                        debug!("Error while closing websocket connection: {}", e);
                    }

                    break res;
                } else {
                    if !zelf.api.is_online() {
                        debug!("API is offline, trying to reconnect");
                        if !zelf.api.reconnect().await? {
                            error!("Couldn't reconnect to server, trying again in {} seconds", AUTO_RECONNECT_INTERVAL);
                            sleep(Duration::from_secs(AUTO_RECONNECT_INTERVAL)).await;
                        } else {
                            // Notify that we are back online
                            zelf.wallet.propagate_event(Event::Online).await;
                        }
                    } else {
                        warn!("Daemon is online but we couldn't sync, trying again in {} seconds", AUTO_RECONNECT_INTERVAL);
                        sleep(Duration::from_secs(AUTO_RECONNECT_INTERVAL)).await;
                    }
                }
            }
        }));


        // Notify that we are online
        self.wallet.propagate_event(Event::Online).await;

        Ok(())
    }

    // Stop the internal loop to stop syncing
    pub async fn stop(&self) -> Result<(), NetworkError> {
        trace!("Stopping network handler");
        if let Some(handle) = self.task.lock().await.take() {
            if handle.is_finished() {
                // We are already finished, which mean the event got triggered
                handle.await??;
            } else {
                handle.abort();

                // Notify that we are offline
                self.wallet.propagate_event(Event::Offline).await;
            }

            // Turn off the websocket connection
            if let Err(e) = self.api.disconnect().await {
                debug!("Error while closing websocket connection: {}", e);
            }

            Ok(())
        } else {
            Err(NetworkError::NotRunning)
        }
    }

    // Retrieve the daemon API used
    pub fn get_api(&self) -> &DaemonAPI {
        &self.api
    }

    // check if the network handler is running (that we have a task and its not finished)
    pub async fn is_running(&self) -> bool {
        let task = self.task.lock().await;
        if let Some(handle) = task.as_ref() {
            !handle.is_finished() && self.api.is_online()
        } else {
            false
        }
    }

    // Process a block by checking if it contains any transaction for us
    // Or that we mined it
    // Returns assets that changed and returns the highest nonce if we send a transaction
    async fn process_block(&self, address: &Address, block: BlockResponse, topoheight: u64) -> Result<Option<(HashSet<Hash>, Option<u64>)>, Error> {
        let block_hash = block.hash.into_owned();
        debug!("Processing block {} at topoheight {}", block_hash, topoheight);

        if block.miner.is_mainnet() != self.wallet.get_network().is_mainnet() {
            debug!("Block {} at topoheight {} is not on the same network as the wallet", block_hash, topoheight);
            return Err(NetworkError::NetworkMismatch.into())
        }

        let mut assets_changed = HashSet::new();
        // Miner address to verify if we mined the block
        let miner = block.miner.into_owned().to_public_key();

        // Prevent storing changes multiple times
        let mut changes_stored = false;

        // create Coinbase entry if its our address and we're looking for PARL asset
        if miner == *address.get_public_key() {
            debug!("Block {} at topoheight {} is mined by us", block_hash, topoheight);
            if let Some(reward) = block.miner_reward {
                let coinbase = EntryData::Coinbase { reward };
                let entry = TransactionEntry::new(block_hash.clone(), topoheight, coinbase);
                assets_changed.insert(PARL_ASSET);

                let broadcast = {
                    let mut storage = self.wallet.get_storage().write().await;
                    if storage.has_transaction(entry.get_hash())? {
                        false
                    } else {
                        storage.save_transaction(entry.get_hash(), &entry)?;
    
                        // Store the changes for history
                        if !changes_stored {
                            storage.add_topoheight_to_changes(topoheight, &block_hash)?;
                            changes_stored = true;
                        }
                        true
                    }
                };

                // Propagate the event to the wallet
                if broadcast {
                    self.wallet.propagate_event(Event::NewTransaction(entry.serializable(self.wallet.get_network().is_mainnet()))).await;
                }
            } else {
                warn!("No reward for block {} at topoheight {}", block_hash, topoheight);
            }
        }

        // Highest nonce we found in this block
        let mut our_highest_nonce = None;

        // Verify all TXs one by one to find one for us
        for tx in block.transactions.into_iter() {
            trace!("Checking transaction {}", tx.hash);
            let is_owner = *tx.source.get_public_key() == *address.get_public_key();
            let entry: Option<EntryData> = match tx.data {
                RPCTransactionType::Burn(payload) => {
                    let payload = payload.into_owned();
                    if is_owner {
                        Some(EntryData::Burn { asset: payload.asset, amount: payload.amount })
                    } else {
                        None
                    }
                },
                RPCTransactionType::Transfers(txs) => {
                    let mut transfers_in: Vec<TransferIn> = Vec::new();
                    let mut transfers_out: Vec<TransferOut> = Vec::new();
                    for transfer in txs {
                        let destination = transfer.destination.to_public_key();
                        if is_owner || destination == *address.get_public_key() {
                            // Get the right handle
                            let (role, handle) = if is_owner {
                                (Role::Sender, transfer.sender_handle)
                            } else {
                                (Role::Receiver, transfer.receiver_handle)
                            };

                            // Decompress commitment it if possible
                            let commitment = match transfer.commitment.decompress() {
                                Ok(c) => c,
                                Err(e) => {
                                    error!("Error while decompressing commitment of TX {}: {}", tx.hash, e);
                                    continue;
                                }
                            };

                            // Same for handle
                            let handle = match handle.decompress() {
                                Ok(h) => h,
                                Err(e) => {
                                    error!("Error while decompressing handle of TX {}: {}", tx.hash, e);
                                    continue;
                                }
                            };

                            let extra_data = if let Some(cipher) = transfer.extra_data.into_owned() {
                                self.wallet.decrypt_extra_data(cipher, &handle, role).ok()
                            } else {
                                None
                            };

                            debug!("Decrypting amount from TX {}", tx.hash);
                            let ciphertext = Ciphertext::new(commitment, handle);
                            let amount = Arc::clone(&self.wallet).decrypt_ciphertext(ciphertext).await?;

                            let asset = transfer.asset.into_owned();
                            assets_changed.insert(asset.clone());

                            if is_owner {
                                let transfer = TransferOut::new(destination, asset, amount, extra_data);
                                transfers_out.push(transfer);
                            } else {
                                let transfer = TransferIn::new(asset, amount, extra_data);
                                transfers_in.push(transfer);
                            }
                        }
                    }

                    if is_owner { // check that we are owner of this TX
                        Some(EntryData::Outgoing { transfers: transfers_out, fee: tx.fee, nonce: tx.nonce })
                    } else if !transfers_in.is_empty() { // otherwise, check that we received one or few transfers from it
                        Some(EntryData::Incoming { from: tx.source.to_public_key(), transfers: transfers_in })
                    } else { // this TX has nothing to do with us, nothing to save
                        None
                    }
                }
            };

            if let Some(entry) = entry {
                let is_tx_stored = {
                    let storage = self.wallet.get_storage().read().await;
                    storage.has_transaction(&tx.hash)?
                };

                // Even if we probably scanned it before and a DAG reorg happened,
                // It shouldn't be found because it got deleted from storage
                if is_tx_stored {
                    debug!("Transaction {} was already stored, skipping", tx.hash);
                    continue;
                }

                // Transaction found at which topoheight it was executed
                let mut tx_topoheight = topoheight;

                // New transaction entry that may be linked to us, check if TX was executed
                if !self.api.is_tx_executed_in_block(&tx.hash, &block_hash).await? {
                    warn!("Transaction {} was a good candidate but was not executed in block {}, searching its block executor", tx.hash, block_hash);
                    // Don't skip the TX, we may have missed it
                    match self.api.get_transaction_executor(&tx.hash).await {
                        Ok(executor) => {
                            tx_topoheight = executor.block_topoheight;
                            debug!("Transaction {} was executed in block {} at topoheight {}", tx.hash, executor.block_hash, executor.block_topoheight);
                        },
                        Err(e) => {
                            // Tx is maybe not executed, this is really rare event
                            warn!("Error while fetching topoheight execution of transaction {}: {}", tx.hash, e);
                            continue;
                        }
                    }
                }

                // Find the highest nonce
                if is_owner && our_highest_nonce.map(|n| tx.nonce > n).unwrap_or(true) {
                    our_highest_nonce = Some(tx.nonce);
                }

                // Save the transaction
                let entry = TransactionEntry::new(tx.hash.into_owned(), tx_topoheight, entry);
                {
                    let mut storage = self.wallet.get_storage().write().await;
                    storage.save_transaction(entry.get_hash(), &entry)?;
                    // Store the changes for history
                    if !changes_stored {
                        storage.add_topoheight_to_changes(topoheight, &block_hash)?;
                        changes_stored = true;
                    }
                }

                // Propagate the event to the wallet
                self.wallet.propagate_event(Event::NewTransaction(entry.serializable(self.wallet.get_network().is_mainnet()))).await;
            }
        }

        if !changes_stored || assets_changed.is_empty() {
            Ok(None)
        } else {
            // Increase by one to get the new nonce
            Ok(Some((assets_changed, our_highest_nonce.map(|n| n + 1))))
        }
    }

    // Scan the chain using a specific balance asset, this helps us to get a list of version to only requests blocks where changes happened
    // When the block is requested, we don't limit the syncing to asset in parameter
    async fn get_balance_and_transactions(&self, topoheight_processed: &mut HashSet<u64>, address: &Address, asset: &Hash, min_topoheight: u64, balances: bool, highest_nonce: &mut Option<u64>) -> Result<(), Error> {
        // Retrieve the highest version
        let (mut topoheight, mut version) = self.api.get_balance(address, asset).await.map(|res| (res.topoheight, res.version))?;
        // don't sync already synced blocks
        if min_topoheight >= topoheight {
            return Ok(())
        }

        // Determine if its the highest version of balance or not
        // This is used to save the latest balance
        let mut highest_version = true;
        loop {
            let (mut balance, _, _, previous_topoheight) = version.consume();
            // add this topoheight in cache to not re-process it (blocks are independant of asset to have faster sync)
            // if its not already processed, do it
            if topoheight_processed.insert(topoheight) {
                let response = self.api.get_block_with_txs_at_topoheight(topoheight).await?;
                let changes = self.process_block(address, response, topoheight).await?;

                // Check if a change occured, we are the highest version and update balances is requested
                if let Some((_, nonce)) = changes.filter(|_| balances && highest_version) {
                    let mut storage = self.wallet.get_storage().write().await;

                    if highest_nonce.is_none() {
                        // Get the highest nonce from storage
                        *highest_nonce = Some(storage.get_nonce()?);
                    }

                    // Store only the highest nonce
                    // Because if we are building queued transactions, it may break our queue
                    // Our we couldn't submit new txs before they get removed from mempool
                    if let Some(nonce) = nonce.filter(|n| highest_nonce.as_ref().map(|h| *h < *n).unwrap_or(true)) {
                        debug!("Storing new highest nonce {}", nonce);
                        storage.set_nonce(nonce)?;
                        *highest_nonce = Some(nonce);
                    }

                    // If we have no balance in storage OR the stored ciphertext isn't the same, we should store it
                    let store = storage.get_balance_for(asset).await.map(|b| b.ciphertext != balance).unwrap_or(true);
                    if store {
                        debug!("Storing balance for asset {}", asset);
                        let plaintext_balance = if let Some(plaintext_balance) = storage.get_unconfirmed_balance_decoded_for(&asset, &balance.compressed()).await? {
                            plaintext_balance
                        } else {
                            trace!("Decrypting balance for asset {}", asset);
                            let ciphertext = balance.decompressed()?;
                            Arc::clone(&self.wallet).decrypt_ciphertext(ciphertext.clone()).await?
                        };

                        // Store the new balance
                        storage.set_balance_for(asset, Balance::new(plaintext_balance, balance)).await?;

                        // Propagate the event
                        self.wallet.propagate_event(Event::BalanceChanged(BalanceChanged {
                            asset: asset.clone(),
                            balance: plaintext_balance
                        })).await;
                    }
                }
            }

            // Prepare a new iteration
            if let Some(previous) = previous_topoheight {
                // don't sync already synced blocks
                if min_topoheight >= previous {
                    return Ok(())
                }

                topoheight = previous;
                version = self.api.get_balance_at_topoheight(address, asset, previous).await?;
            } else {
                return Ok(())
            }

            // Only first iteration is the highest one
            highest_version = false;
        }
    }

    // Locate the last topoheight valid for syncing, this support soft forks, DAG reorgs, etc...
    // Balances and nonce may be outdated, but we will sync them later
    // All transactions / changes above the last valid topoheight will be deleted
    // Returns daemon topoheight along wallet stable topoheight and if back sync is needed
    async fn locate_sync_topoheight_and_clean(&self) -> Result<(u64, Hash, u64, bool), NetworkError> {
        let info = self.api.get_info().await?;
        let daemon_topoheight = info.topoheight;
        let daemon_block_hash = info.top_block_hash;
        let pruned_topoheight = info.pruned_topoheight.unwrap_or(0);

        // Verify that we are on the same network
        {
            let network = self.wallet.get_network();
            if info.network != *network {
                error!("Network mismatch! Our network is {} while daemon is {}", network, info.network);
                return Err(NetworkError::NetworkMismatch)
            }
        }

        // Retrieve the highest point possible
        let synced_topoheight = {
            let storage = self.wallet.get_storage().read().await;
            if storage.has_top_block_hash()? {
                // Check that the daemon topoheight is the same as our
                // Verify also that the top block hash is same as our
                let top_block_hash = storage.get_top_block_hash()?;
                let synced_topoheight = storage.get_synced_topoheight()?;

                // Check if its the top
                if daemon_topoheight == synced_topoheight && daemon_block_hash == top_block_hash {
                    // No need to sync back, we are already synced
                    return Ok((daemon_topoheight, daemon_block_hash, synced_topoheight, false))
                }

                // Verify we are not above the daemon chain
                if synced_topoheight > daemon_topoheight {
                    warn!("We are above the daemon chain, we should sync from scratch");
                    return Ok((daemon_topoheight, daemon_block_hash, 0, true))
                }

                if synced_topoheight > pruned_topoheight {
                    // Check if it's still a correct block
                    let header = self.api.get_block_at_topoheight(synced_topoheight).await?;
                    let block_hash = header.hash.into_owned();
                    if block_hash == top_block_hash {
                        // topoheight and block hash are equal, we are still on right chain
                        return Ok((daemon_topoheight, daemon_block_hash, synced_topoheight, false))
                    }
                }

                synced_topoheight
            } else {
                storage.get_synced_topoheight().unwrap_or(0)
            }
        };

        // Search the highest block that is still valid for wallet
        let mut maximum = synced_topoheight;
        let block_hash = loop {
            maximum = {
                let storage = self.wallet.get_storage().read().await;
                storage.get_highest_topoheight_in_changes_below(maximum)?
            };

            // We are completely wrong, we should sync from scratch
            if maximum == 0 {
                break None;
            }

            // We are under the pruned topoheight,
            // lets assume we are on the right chain under it
            if maximum < pruned_topoheight {
                maximum = pruned_topoheight;
                break None;
            }

            // Retrieve local hash
            let local_hash = {
                let storage = self.wallet.get_storage().read().await;
                storage.get_block_hash_for_topoheight(maximum)?
            };

            // Check if we are on the same chain
            debug!("Checking if we are on the same chain at topoheight {}", maximum);
            let header = self.api.get_block_at_topoheight(maximum).await?;
            let block_hash = header.hash.into_owned();
            if block_hash == local_hash {
                break Some(local_hash);
            }

            // Looks like we are on a different chain
            maximum -= 1;
        };

        // Get the hash of the block at this topoheight
        let block_hash = if let Some(block_hash) = block_hash {
            block_hash
        } else {
            let response = self.api.get_block_at_topoheight(maximum).await?;
            response.hash.into_owned()
        };

        let mut storage = self.wallet.get_storage().write().await;        
        // Now let's clean everything
        if storage.delete_changes_above_topoheight(maximum)? {
            warn!("Cleaning transactions above topoheight {}", maximum);
            // Changes were deleted, we should also delete transactions
            storage.delete_transactions_above_topoheight(maximum)?;
        }

        // Save the new values
        storage.set_synced_topoheight(maximum)?;
        storage.set_top_block_hash(&block_hash)?;
        // Add it only if its not already in changes
        if !storage.has_topoheight_in_changes(maximum)? {
            storage.add_topoheight_to_changes(maximum, &block_hash)?;
        }

        // Verify its not the first time we do a sync
        if synced_topoheight != 0 {
            self.wallet.propagate_event(Event::Rescan { start_topoheight: maximum }).await;   
        }

        Ok((daemon_topoheight, daemon_block_hash, maximum, true))
    }

    // Sync the latest version of our balances and nonces and determine if we should parse all blocks
    // If assets are provided, we'll only sync these assets
    // TODO: this may bug with Smart Contract integration as we could receive a new asset and not detect it
    // If nonce is not provided, we will fetch it from the daemon
    async fn sync_head_state(&self, address: &Address, assets: Option<HashSet<Hash>>, nonce: Option<u64>, sync_nonce: bool) -> Result<bool, Error> {
        trace!("syncing head state");
        let new_nonce = if nonce.is_some() {
            nonce
        } else if sync_nonce {
            trace!("no nonce provided, fetching it from daemon");
            match self.api.get_nonce(&address).await.map(|v| v.version) {
                Ok(v) => Some(v.get_nonce()),
                Err(e) => {
                    debug!("Error while fetching last nonce: {}", e);
                    {
                        let mut storage = self.wallet.get_storage().write().await;
                        if storage.has_any_balance().await? {
                            warn!("We have balances but we couldn't fetch the nonce, deleting all balances");
                            storage.delete_balances().await?;
                            storage.delete_assets().await?;
                        }
                    }
                    // Account is not registered, we can return safely here
                    return Ok(false)
                }
            }
        } else {
            None
        };

        let assets = if let Some(assets) = assets {
            assets
        } else {
            trace!("no assets provided, fetching all assets");
            self.api.get_account_assets(address).await?
        };

        trace!("assets: {}", assets.len());

        let mut balances: HashMap<&Hash, CiphertextCache> = HashMap::new();
        // Store newly detected assets
        // Get the final balance of each asset
        for asset in &assets {
            trace!("asset: {}", asset);
            // check if we have this asset locally
            if !{
                let storage = self.wallet.get_storage().read().await;
                storage.contains_asset(&asset).await?
            } {
                let data = self.api.get_asset(&asset).await?;
                
                // Add the asset to the storage
                {
                    let mut storage = self.wallet.get_storage().write().await;
                    storage.add_asset(&asset, data.get_decimals()).await?;
                }

                // New asset added to the wallet, inform listeners
                self.wallet.propagate_event(Event::NewAsset(AssetWithData::new(asset.clone(), data))).await;
            }

            // get the balance for this asset
            let result = self.api.get_balance(&address, &asset).await?;
            trace!("found balance at topoheight: {}", result.topoheight);
            balances.insert(asset, result.version.take_balance());
        }

        let mut should_sync_blocks = false;
        // Apply changes
        {
            if let Some(new_nonce) = new_nonce {
                let mut storage = self.wallet.get_storage().write().await;
                if storage.get_nonce().map(|n| n != new_nonce).unwrap_or(true) {
                    // Store the new nonce
                    debug!("Storing new nonce {}", new_nonce);
                    storage.set_nonce(new_nonce)?;
                    should_sync_blocks = true;
                }
            }

            for (asset, mut ciphertext) in balances {
                let (must_update, balance_cache) = {
                    let storage = self.wallet.get_storage().read().await;
                    let must_update = match storage.get_balance_for(&asset).await {
                        Ok(mut previous) => previous.ciphertext.compressed() != ciphertext.compressed(),
                        // If we don't have a balance for this asset, we should update it
                        Err(_) => true
                    };

                    // If we must update, check if we have a cache for this balance
                    let balance_cache = if must_update {
                        storage.get_unconfirmed_balance_decoded_for(&asset, &ciphertext.compressed()).await?
                    } else {
                        None
                    };

                    (must_update, balance_cache)
                };

                if must_update {
                    trace!("must update balance for asset: {}, ct: {:?}", asset, ciphertext.to_bytes());
                    let value = if let Some(cache) = balance_cache {
                        cache
                    } else {
                        trace!("Decrypting balance for asset {}", asset);
                        Arc::clone(&self.wallet).decrypt_ciphertext(ciphertext.decompressed()?.clone()).await?
                    };

                    // Inform the change of the balance
                    self.wallet.propagate_event(Event::BalanceChanged(BalanceChanged {
                        asset: asset.clone(),
                        balance: value
                    })).await;

                    // Update the balance
                    let mut storage = self.wallet.get_storage().write().await;
                    storage.set_balance_for(asset, Balance::new(value, ciphertext)).await?;

                    // We should sync new blocks to get the TXs
                    should_sync_blocks = true;
                }
            }
        }

        Ok(should_sync_blocks)
    }

    // Locate the highest valid topoheight we synced to, clean wallet storage
    // then sync again the head state
    async fn sync(&self, address: &Address, event: Option<NewBlockEvent>) -> Result<(), Error> {
        trace!("sync");
        // First, locate the last topoheight valid for syncing
        let (daemon_topoheight, daemon_block_hash, wallet_topoheight, sync_back) = self.locate_sync_topoheight_and_clean().await?;
        debug!("Daemon topoheight: {}, wallet topoheight: {}, sync back: {}", daemon_topoheight, wallet_topoheight, sync_back);

        let mut sync_new_blocks = false;
        // Sync back is requested, sync the head state again
        if sync_back {
            trace!("sync back");
            // Now sync head state, this will helps us to determinate if we should sync blocks or not
            sync_new_blocks = self.sync_head_state(&address, None, None, true).await?;
        }

        if let Some(block) = event {
            trace!("new block event received");
            // We can safely handle it by hand because `locate_sync_topoheight_and_clean` secure us from being on a wrong chain
            if let Some(topoheight) = block.topoheight {
                if let Some((assets, mut nonce)) = self.process_block(address, block, topoheight).await? {
                    trace!("We must sync head state");
                    {
                        let storage = self.wallet.get_storage().read().await;
                        // Verify that its a higher nonce than our locally stored
                        // Because if we are building queued transactions, it may break our queue
                        // Our we couldn't submit new txs before they get removed from mempool
                        let stored_nonce = storage.get_nonce().unwrap_or(0);
                        if nonce.is_some_and(|n| n <= stored_nonce) {
                            nonce = None;
                        }
                    }
                    // A change happened in this block, lets update balance and nonce
                    sync_new_blocks |= self.sync_head_state(&address, Some(assets), nonce, false).await?;
                }
            } else {
                // It is a block that got directly orphaned by DAG, ignore it
                debug!("Block {} is not ordered, skipping it", block.hash);
            }
        } else {
            sync_new_blocks = true;
        }

        // we have something that changed, sync transactions
        if sync_new_blocks {
            debug!("Syncing new blocks");
            self.sync_new_blocks(address, wallet_topoheight, true).await?;
        }

        // Update the topoheight and block hash for wallet
        {
            let mut storage = self.wallet.get_storage().write().await;
            storage.set_synced_topoheight(daemon_topoheight)?;
            storage.set_top_block_hash(&daemon_block_hash)?;
        }

        // Propagate the event
        self.wallet.propagate_event(Event::NewTopoHeight { topoheight: daemon_topoheight }).await;
        debug!("Synced to topoheight {}", daemon_topoheight);
        Ok(())
    }

    // Runs an infinite loop to sync on each new block added in chain
    // Because of potential forks and DAG reorg during attacks,
    // we verify the last valid topoheight where changes happened
    async fn start_syncing(self: &Arc<Self>) -> Result<(), Error> {
        // Generate only one time the address
        let address = self.wallet.get_address();
        // Do a first sync to be up-to-date with the daemon
        self.sync(&address, None).await?;

        // Thanks to websocket, we can be notified when a new block is added in chain
        // this allows us to have a instant sync of each new block instead of polling periodically
        let mut on_new_block = self.api.on_new_block_event().await?;

        // Because DAG can reorder any blocks in stable height, its possible we missed some txs because they were not executed
        // when the block was added. We must check on DAG reorg for each block just to be sure
        let mut on_block_ordered = self.api.on_block_ordered_event().await?;

        // For better security, verify that an orphaned TX isn't in our ledger
        // This is rare event but may happen if someone try to do something shady
        let mut on_transaction_orphaned = self.api.on_transaction_orphaned_event().await?;

        // Network events to detect if we are online or offline
        let mut on_connection = self.api.on_connection().await;
        let mut on_connection_lost = self.api.on_connection_lost().await;

        loop {
            tokio::select! {
                // Wait on a new block, we don't parse the block directly as it may
                // have reorg the chain
                res = on_new_block.next() => {
                    trace!("on_new_block_event");
                    let event = res?;
                    self.sync(&address, Some(event)).await?;
                },
                res = on_block_ordered.next() => {
                    trace!("on_block_ordered_event");
                    let event = res?;
                    let topoheight = event.topoheight;
                    let mut process_block = false;
                    {
                        let mut storage = self.wallet.get_storage().write().await;
                        if let Some(hash) = storage.get_block_hash_for_topoheight(topoheight).ok() {
                            if topoheight != 0 && hash != *event.block_hash {
                                warn!("DAG reorg detected at topoheight {}, deleting all changes above", topoheight);
                                storage.delete_changes_above_topoheight(topoheight - 1)?;
                                if storage.get_synced_topoheight().unwrap_or(0) > topoheight {
                                    warn!("We are above the reorg, restart syncing from {}", topoheight);
                                    storage.set_synced_topoheight(topoheight)?;
                                    storage.set_top_block_hash(&event.block_hash)?;
                                }
                                process_block = true;
                            }
                        }
                    }

                    if process_block {
                        // Sync this block again as it may have some TXs executed
                        let block = self.api.get_block_at_topoheight(topoheight).await?;
                        if let Some((assets, _)) = self.process_block(&address, block, topoheight).await? {
                            debug!("Found changes for assets: {}", assets.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "));
                        }
                    }
                },
                res = on_transaction_orphaned.next() => {
                    trace!("on_transaction_orphaned_event");
                    let event = res?;
                    let tx = event.data;

                    let mut storage = self.wallet.get_storage().write().await;
                    if storage.has_transaction(&tx.hash)? {
                        warn!("Transaction {} was orphaned, deleting it", tx.hash);
                        storage.delete_transaction(&tx.hash)?;
                    }

                    if storage.get_tx_cache().is_some_and(|cache| cache.last_tx_hash_created == *tx.hash) {
                        warn!("Transaction {} was orphaned, deleting it from cache", tx.hash);
                        storage.clear_tx_cache();
                    }
                },
                // Detect network events
                res = on_connection.recv() => {
                    trace!("on_connection");
                    res?;
                    // We are connected again, make sure we are still up-to-date with node 
                    self.sync(&address, None).await?;

                    self.wallet.propagate_event(Event::Online).await;
                },
                res = on_connection_lost.recv() => {
                    trace!("on_connection_lost");
                    res?;
                    self.wallet.propagate_event(Event::Offline).await;
                }
            }
        }
    }

    // Sync all new blocks until the current topoheight
    async fn sync_new_blocks(&self, address: &Address, current_topoheight: u64, balances: bool) -> Result<(), Error> {
        let assets = {
            let storage = self.wallet.get_storage().read().await;
            storage.get_assets().await?
        };

        // cache for all topoheight we already processed
        // this will prevent us to request more than one time the same topoheight
        let mut topoheight_processed = HashSet::new();

        // get balance and transactions for each asset
        let mut highest_nonce = None;
        for asset in assets {
            debug!("calling get balances and transactions {}", current_topoheight);
            if let Err(e) = self.get_balance_and_transactions(&mut topoheight_processed, &address, &asset, current_topoheight, balances, &mut highest_nonce).await {
                error!("Error while syncing balance for asset {}: {}", asset, e);
            }
        }
        Ok(())
    }
}
