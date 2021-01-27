// Copyright 2019-2020 ChainX Project Authors. Licensed under GPL-3.0.

//!

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[cfg(not(feature = "std"))]
use alloc::{format, string::String};

use frame_support::{
    debug, decl_error, decl_event, decl_module, decl_storage,
    dispatch::{DispatchResultWithPostInfo, Parameter},
    traits::Get,
    weights::Pays,
    StorageValue,
};

use frame_system::{
    ensure_signed,
    offchain::{
        AppCrypto, CreateSignedTransaction, SendSignedTransaction, SendTransactionTypes, Signer,
    },
};
use sp_application_crypto::RuntimeAppPublic;
use sp_core::crypto::KeyTypeId;
use sp_runtime::{
    offchain::{
        http,
        storage::StorageValueRef,
        storage_lock::{StorageLock, Time},
        Duration,
    },
    transaction_validity::{
        InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
        ValidTransaction,
    },
    SaturatedConversion,
};

use sp_std::{
    collections::btree_set::BTreeSet, convert::TryFrom, marker::PhantomData, str, vec, vec::Vec,
};

use light_bitcoin::{
    chain::{Block as BtcBlock, BlockHeader as BtcHeader, Transaction as BtcTransaction},
    keys::{Address as BtcAddress, Network as BtcNetwork},
    merkle::PartialMerkleTree,
    primitives::{hash_rev, H256 as BtcHash},
    serialization::{deserialize, serialize, Reader},
};
use xp_gateway_bitcoin::{BtcTxMetaType, BtcTxTypeDetector, OpReturnExtractor};
use xp_gateway_common::AccountExtractor;
use xpallet_assets::Chain;
use xpallet_gateway_bitcoin::{
    types::{BtcRelayedTxInfo, VoteResult},
    Module as XGatewayBitcoin, WeightInfo,
};
use xpallet_gateway_common::{trustees::bitcoin::BtcTrusteeAddrInfo, Module as XGatewayCommon};

/// Defines application identifier for crypto keys of this module.
///
/// Every module that deals with signatures needs to declare its unique identifier for
/// its crypto keys.
/// When offchain worker is signing transactions it's going to request keys of type
/// `KeyTypeId` from the keystore and use the ones it finds to sign the transaction.
/// The keys can be inserted manually via RPC (see `author_insertKey`).
pub const BTC_RELAY: KeyTypeId = KeyTypeId(*b"btcr");

/// Based on the above `KeyTypeId` we need to generate a pallet-specific crypto type wrappers.
/// We can use from supported crypto kinds (`sr25519`, `ed25519` and `ecdsa`) and augment
/// the types with this pallet-specific identifier.
pub mod app {
    pub use super::BTC_RELAY;
    use sp_core::sr25519::Signature as Sr25519Signature;
    use sp_runtime::app_crypto::{app_crypto, sr25519};
    use sp_runtime::{traits::Verify, MultiSignature, MultiSigner};

    app_crypto!(sr25519, BTC_RELAY);

    pub struct RelayAuthId;

    impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for RelayAuthId {
        type RuntimeAppPublic = Public;
        type GenericSignature = sp_core::sr25519::Signature;
        type GenericPublic = sp_core::sr25519::Public;
    }

    impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature>
        for RelayAuthId
    {
        type RuntimeAppPublic = Public;
        type GenericPublic = sp_core::sr25519::Public;
        type GenericSignature = sp_core::sr25519::Signature;
    }
}

sp_application_crypto::with_pair! {
    /// An bitcoin offchain keypair using sr25519 as its crypto.
    pub type AuthorityPair = app::Pair;
}

/// An bitcoin offchain identifier using sr25519 as its crypto.
pub type AuthorityId = app::Public;

/// An bitcoin offchain signature using sr25519 as its crypto.
pub type AuthoritySignature = app::Signature;

/// This pallet's configuration trait
pub trait Trait:
    SendTransactionTypes<Call<Self>>
    + CreateSignedTransaction<Call<Self>>
    + xpallet_gateway_bitcoin::Trait
    + xpallet_gateway_common::Trait
{
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;
    /// The overarching dispatch call type.
    type Call: From<Call<Self>>;
    /// A configuration for base priority of unsigned transactions.
    type UnsignedPriority: Get<TransactionPriority>;
    /// The identifier type for an offchain worker.
    type AuthorityId: Parameter + Default + RuntimeAppPublic + Ord;
    type RelayAuthId: AppCrypto<Self::Public, Self::Signature>;
    /// Weight information for extrinsics in this pallet.
    type WeightInfo: WeightInfo;
}

decl_event!(
    /// Events generated by the module.
    pub enum Event<T>
    where
        <T as frame_system::Trait>::AccountId
    {
        /// A Bitcoin block generated. [btc_block_height, btc_block_hash]
        NewBtcBlock(u32, BtcHash),
        /// A Bitcoin transaction. [btc_tx_hash]
        NewBtcTransaction(BtcHash),
        _PhantomData(PhantomData::<AccountId>),
    }
);

decl_error! {
    /// Error for the the module
    pub enum Error for Module<T: Trait> {
        /// Offchain HTTP I/O error.
        HttpIoError,
        /// Offchain HTTP deadline reached.
        HttpDeadlineReached,
        /// Offchain HTTP unknown error.
        HttpUnknown,
        /// Offchain HTTP body is not UTF-8.
        HttpBodyNotUTF8,
        /// Bitcoin serialization/deserialization error.
        BtcSserializationError,
        /// Btc send raw transaction rpc error.
        BtcSendRawTxError,
    }
}

impl<T: Trait> From<sp_core::offchain::HttpError> for Error<T> {
    fn from(err: sp_core::offchain::HttpError) -> Self {
        match err {
            sp_core::offchain::HttpError::DeadlineReached => Error::HttpDeadlineReached,
            sp_core::offchain::HttpError::IoError => Error::HttpIoError,
            sp_core::offchain::HttpError::Invalid => Error::HttpUnknown,
        }
    }
}

impl<T: Trait> From<sp_runtime::offchain::http::Error> for Error<T> {
    fn from(err: sp_runtime::offchain::http::Error) -> Self {
        match err {
            sp_runtime::offchain::http::Error::DeadlineReached => Error::HttpDeadlineReached,
            sp_runtime::offchain::http::Error::IoError => Error::HttpIoError,
            sp_runtime::offchain::http::Error::Unknown => Error::HttpUnknown,
        }
    }
}

decl_storage! {
    trait Store for Module<T: Trait> as XGatewayBitcoinOffchain {
        Keys get(fn keys): Vec<T::AuthorityId>;
    }
    add_extra_genesis {
        config(keys): Vec<T::AuthorityId>;
        build(|config| Module::<T>::initialize_keys(&config.keys))
    }
}

decl_module! {
    /// A public part of the pallet.
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        fn deposit_event() = default;

        fn offchain_worker(block_number: T::BlockNumber) {
            let network = XGatewayBitcoin::<T>::network_id();
            if block_number.saturated_into::<u64>() % 2 == 0 {
                // First, get withdrawal proposal from chain and broadcast to btc network.
                match Self::withdrawal_proposal_broadcast(network) {
                    Ok(Some(hash)) => {
                        debug::info!(
                                "[OCW|withdrawal_proposal_broadcast] Succeed! Transaction Hash: {:?}",
                                hash
                            );
                    }
                    Ok(None) => {
                        debug::info!("[OCW|withdrawal_proposal_broadcast] No Withdrawal Proposal");
                    }
                    _ => {
                        debug::error!("[OCW|withdrawal_proposal_broadcast] Failed!");
                        return;
                    }
                }
                // Second, filter transactions from confirmed block and push transactions to chain.
                if Self::filter_transactions_and_push(network) {
                    // Third, get new block from btc network and push block header to chain
                    Self::get_new_header_and_push(network);
                }
            }
        }

        #[weight = <T as Trait>::WeightInfo::push_header()]
        fn push_header(origin, height: u32, header: BtcHeader) -> DispatchResultWithPostInfo {
            let worker = ensure_signed(origin)?;
            debug::info!("[OCW] Worker:{:?} Push Header: {:?}", worker, header);
            XGatewayBitcoin::<T>::apply_push_header(header)?;
            Self::deposit_event(Event::<T>::NewBtcBlock(height, header.hash()));
            Ok(Pays::No.into())
        }

        #[weight = <T as Trait>::WeightInfo::push_transaction()]
        fn push_transaction(origin, tx: BtcTransaction, relayed_info: BtcRelayedTxInfo, prev_tx: Option<BtcTransaction>)  -> DispatchResultWithPostInfo {
            let worker = ensure_signed(origin)?;
            debug::info!("[OCW] Worker:{:?} Push Transaction: {:?}", worker, tx.hash());
            let relay_tx = relayed_info.into_relayed_tx(tx.clone());
            XGatewayBitcoin::<T>::apply_push_transaction(relay_tx, prev_tx)?;
            Self::deposit_event(Event::<T>::NewBtcTransaction(tx.hash()));
            Ok(Pays::No.into())
        }
    }
}

impl<T: Trait> Module<T> {
    fn initialize_keys(keys: &[T::AuthorityId]) {
        if !keys.is_empty() {
            assert!(Keys::<T>::get().is_empty(), "Keys are already initialized!");
            Keys::<T>::put(keys);
        }
    }
}

/// Most of the functions are moved outside of the `decl_module!` macro.
///
/// This greatly helps with error messages, as the ones inside the macro
/// can sometimes be hard to debug.
impl<T: Trait> Module<T> {
    // Get trustee pair
    fn get_trustee_pair(session_number: u32) -> Result<Option<(BtcAddress, BtcAddress)>, Error<T>> {
        if let Some(trustee_session_info) =
            XGatewayCommon::<T>::trustee_session_info_of(Chain::Bitcoin, session_number)
        {
            let hot_addr: Vec<u8> = trustee_session_info.0.hot_address;
            let hot_addr = BtcTrusteeAddrInfo::try_from(hot_addr).unwrap();
            let hot_addr = String::from_utf8(hot_addr.addr)
                .unwrap()
                .parse::<BtcAddress>()
                .unwrap();
            let cold_addr: Vec<u8> = trustee_session_info.0.cold_address;
            let cold_addr = BtcTrusteeAddrInfo::try_from(cold_addr).unwrap();
            let cold_addr = String::from_utf8(cold_addr.addr)
                .unwrap()
                .parse::<BtcAddress>()
                .unwrap();
            debug::info!("[OCW|get_trustee_pair] ChainX X-BTC Trustee Session Info (session number = {:?}):[Hot Address: {}, Cold Address: {}]",
                            session_number,
                            hot_addr,
                            cold_addr,);
            Ok(Some((hot_addr, cold_addr)))
        } else {
            Ok(None)
        }
    }
    // Get new header from btc network and push header to chain
    fn get_new_header_and_push(network: BtcNetwork) {
        let best_index = XGatewayBitcoin::<T>::best_index().height;
        let mut next_height = best_index + 1;
        for _ in 0..=5 {
            let btc_block_hash = match Self::fetch_block_hash(next_height, network) {
                Ok(Some(hash)) => {
                    debug::info!("[OCW] ₿ Block #{} hash: {}", next_height, hash);
                    hash
                }
                Ok(None) => {
                    debug::warn!("[OCW] ₿ Block #{} has not been generated yet", next_height);
                    return;
                }
                Err(err) => {
                    debug::warn!("[OCW] ₿ {:?}", err);
                    continue;
                }
            };

            let btc_block = match Self::fetch_block(&btc_block_hash[..], network) {
                Ok(block) => {
                    debug::info!("[OCW] ₿ Block {}", hash_rev(block.hash()));
                    block
                }
                Err(err) => {
                    debug::warn!("[OCW] ₿ {:?}", err);
                    continue;
                }
            };

            let btc_header = btc_block.header;
            // Determine whether it is a branch block
            if XGatewayBitcoin::<T>::block_hash_for(best_index)
                .contains(&btc_header.previous_header_hash)
            {
                let signer = Signer::<T, T::RelayAuthId>::any_account();
                let result = signer.send_signed_transaction(|_acct| {
                    Call::push_header(next_height, btc_block.header)
                });
                if let Some((_acct, res)) = result {
                    if res.is_err() {
                        debug::error!(
                            "[OCW] Failed to submit unsigned transaction for pushing header: {:?}",
                            res
                        );
                    } else {
                        debug::info!(
                            "[OCW] ₿ Submitting signed transaction for pushing header: #{}",
                            next_height
                        );
                    }
                }
            } else {
                next_height -= 1;
                continue;
            }
            break;
        }
    }
    // Filter transactions in confirmed block and push transactions to chain
    fn filter_transactions_and_push(network: BtcNetwork) -> bool {
        if let Some(confirmed_index) = XGatewayBitcoin::<T>::confirmed_index() {
            let confirm_height = confirmed_index.height;
            // Get confirmed height from local storage
            let confirmed_info = StorageValueRef::persistent(b"ocw::confirmed");
            let mut lock = StorageLock::<'_, Time>::new(b"ocw::lock");
            // Prevent repeated filtering of transactions
            if let Ok(_guard) = lock.try_lock() {
                if let Some(Some(confirmed)) = confirmed_info.get::<u32>() {
                    if confirmed == confirm_height {
                        return true;
                    }
                }
            } else {
                debug::info!("[OCW] Read Lock Can't Be Acquired.");
                debug::info!("[OCW] There is a worker working here. Please don't take his job. ");
                return false;
            }
            // Prevent unstable network connections
            for i in 0..=5 {
                if i == 5 {
                    return false;
                }
                let confirm_hash = match Self::fetch_block_hash(confirm_height, network) {
                    Ok(Some(hash)) => {
                        debug::info!("[OCW] ₿ Confirmed Block #{} hash: {}", confirm_height, hash);
                        hash
                    }
                    Ok(None) => {
                        debug::warn!("[OCW] ₿ Confirmed Block #{} Failed", confirm_height);
                        continue;
                    }
                    Err(err) => {
                        debug::warn!("[OCW] ₿ Confirmed {:?}", err);
                        continue;
                    }
                };

                let btc_confirmed_block = match Self::fetch_block(&confirm_hash[..], network) {
                    Ok(block) => {
                        debug::info!("[OCW] ₿ Confirmed Block {}", hash_rev(block.hash()));
                        block
                    }
                    Err(err) => {
                        debug::warn!("[OCW] ₿ Confirmed {:?}", err);
                        continue;
                    }
                };

                let mut lock = StorageLock::<'_, Time>::new(b"ocw::lock");
                if let Ok(_guard) = lock.try_lock() {
                    debug::info!("[OCW] A Worker start to working...");
                    if Self::push_xbtc_transaction(&btc_confirmed_block, network) {
                        confirmed_info.set(&confirm_height);
                        return true;
                    }
                } else {
                    debug::info!("[OCW] A worker exists.");
                    return false;
                };
            }
        }
        true
    }
    // Submit XBTC deposit/withdraw transaction to the ChainX
    fn push_xbtc_transaction(confirmed_block: &BtcBlock, network: BtcNetwork) -> bool {
        let mut needed = Vec::new();
        let mut tx_hashes = Vec::with_capacity(confirmed_block.transactions.len());
        let mut tx_matches = Vec::with_capacity(confirmed_block.transactions.len());

        // Get trustee info
        let trustee_session_info_len =
            XGatewayCommon::<T>::trustee_session_info_len(Chain::Bitcoin);
        let current_trustee_session_number = trustee_session_info_len
            .checked_sub(1)
            .unwrap_or(u32::max_value());
        let current_trustee_pair = match Self::get_trustee_pair(current_trustee_session_number) {
            Ok(Some((hot, cold))) => (hot, cold),
            _ => {
                debug::warn!("[OCW] Can't get current trustee pair!");
                return false;
            }
        };
        let btc_min_deposit = XGatewayBitcoin::<T>::btc_min_deposit();
        // Construct BtcTxTypeDetector
        let btc_tx_detector =
            BtcTxTypeDetector::new(network, btc_min_deposit, current_trustee_pair, None);
        // Filter transaction type (only deposit and withdrawal)
        for tx in &confirmed_block.transactions {
            // Prepare for constructing partial merkle tree
            tx_hashes.push(tx.hash());
            if tx.is_coinbase() {
                tx_matches.push(false);
                continue;
            }
            let outpoint = tx.inputs[0].previous_output;
            let prev_tx_hash = hex::encode(hash_rev(outpoint.txid));
            for i in 0..=5 {
                if i == 5 {
                    return false;
                }
                let prev_tx = match Self::fetch_transaction(&prev_tx_hash[..], network) {
                    Ok(prev) => prev,
                    _ => continue,
                };
                match btc_tx_detector.detect_transaction_type(
                    &tx,
                    Some(&prev_tx),
                    OpReturnExtractor::extract_account,
                ) {
                    BtcTxMetaType::Withdrawal | BtcTxMetaType::Deposit(..) => {
                        tx_matches.push(true);
                        needed.push((tx.clone(), Some(prev_tx.clone())));
                    }
                    BtcTxMetaType::HotAndCold
                    | BtcTxMetaType::TrusteeTransition
                    | BtcTxMetaType::Irrelevance => tx_matches.push(false),
                }
                break;
            }
        }
        // Push x-btc withdraw/deposit transactions if they exist
        if !needed.is_empty() {
            // Construct partial merkle tree
            let merkle_proof = PartialMerkleTree::from_txids(&tx_hashes, &tx_matches);
            // Push xbtc relay (withdraw/deposit) transaction
            let signer = Signer::<T, T::RelayAuthId>::any_account();
            for (tx, prev_tx) in needed {
                let relayed_info = BtcRelayedTxInfo {
                    block_hash: confirmed_block.hash(),
                    merkle_proof: merkle_proof.clone(),
                };
                let result = signer.send_signed_transaction(|_acct| {
                    Call::push_transaction(tx.clone(), relayed_info.clone(), prev_tx.clone())
                });
                if let Some((_acct, res)) = result {
                    if res.is_err() {
                        debug::error!("[OCW] Failed to submit signed transaction for pushing transaction: {:?}",res);
                    } else {
                        debug::info!(
                            "[OCW] Submitting signed transaction for pushing transaction: #{:?}",
                            tx.hash()
                        );
                    }
                }
            }
        } else {
            debug::info!(
                "[OCW|push_x-btc_transaction] No X-BTC Deposit/Withdraw Transactions in th Confirmed Block {:?}",
                hash_rev(confirmed_block.hash())
            );
        }
        true
    }
    // Get withdrawal proposal from chain and broadcast raw transaction
    fn withdrawal_proposal_broadcast(network: BtcNetwork) -> Result<Option<String>, ()> {
        if let Some(withdrawal_proposal) = XGatewayBitcoin::<T>::withdrawal_proposal() {
            if withdrawal_proposal.sig_state == VoteResult::Finish {
                let tx = serialize(&withdrawal_proposal.tx).take();
                let hex_tx = hex::encode(&tx);
                debug::info!("[OCW|send_raw_transaction] Btc Tx Hex: {}", hex_tx);
                match Self::send_raw_transaction(hex_tx, network) {
                    Ok(hash) => {
                        debug::info!(
                            "[OCW|withdrawal_proposal_broadcast] Transaction Hash: {:?}",
                            hash
                        );
                        return Ok(Some(hash));
                    }
                    Err(err) => {
                        debug::warn!("[OCW|withdrawal_proposal_broadcast] Error {:?}", err);
                    }
                }
            }
        }
        Ok(None)
    }
    // Http get request
    fn get<U: AsRef<str>>(url: U) -> Result<Vec<u8>, Error<T>> {
        // Set timeout
        let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
        // Http get request
        let pending = http::Request::get(url.as_ref())
            .deadline(deadline)
            .send()
            .map_err(Error::<T>::from)?;
        // Http response
        let response = pending
            .try_wait(deadline)
            .map_err(|_| Error::<T>::HttpDeadlineReached)??;
        // Let's check the status code before we proceed to reading the response.
        if response.code != 200 {
            debug::warn!("Unexpected status code: {}", response.code);
            return Err(Error::<T>::HttpUnknown);
        }
        // Response body
        let resp_body = response.body().collect::<Vec<u8>>();
        Ok(resp_body)
    }
    // Http post request
    fn post<B, I>(url: &str, req_body: B) -> Result<Vec<u8>, Error<T>>
    where
        B: Default + IntoIterator<Item = I>,
        I: AsRef<[u8]>,
    {
        // Set timeout
        let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
        // Http post request
        let pending = http::Request::post(url, req_body)
            .deadline(deadline)
            .send()
            .map_err(Error::<T>::from)?;
        // Http response
        let response = pending
            .try_wait(deadline)
            .map_err(|_| Error::<T>::HttpDeadlineReached)??;
        // Let's check the status code before we proceed to reading the response.
        if response.code != 200 {
            debug::warn!("Unexpected status code: {}", response.code);
            return Err(Error::<T>::HttpUnknown);
        }
        // Response body
        let resp_body = response.body().collect::<Vec<u8>>();
        Ok(resp_body)
    }
    // Get btc block hash from btc network
    fn fetch_block_hash(height: u32, network: BtcNetwork) -> Result<Option<String>, Error<T>> {
        let url = match network {
            BtcNetwork::Mainnet => format!("https://blockstream.info/api/block-height/{}", height),
            BtcNetwork::Testnet => format!(
                "https://blockstream.info/testnet/api/block-height/{}",
                height
            ),
        };
        let resp_body = Self::get(url)?;
        let resp_body = str::from_utf8(&resp_body).map_err(|_| {
            debug::warn!("No UTF8 body");
            Error::<T>::HttpBodyNotUTF8
        })?;
        const RESP_BLOCK_NOT_FOUND: &str = "Block not found";
        if resp_body == RESP_BLOCK_NOT_FOUND {
            debug::info!("₿ Block #{} not found", height);
            Ok(None)
        } else {
            let hash: String = resp_body.into();
            Ok(Some(hash))
        }
    }
    // Get btc block from btc network
    fn fetch_block(hash: &str, network: BtcNetwork) -> Result<BtcBlock, Error<T>> {
        let url = match network {
            BtcNetwork::Mainnet => format!("https://blockstream.info/api/block/{}/raw", hash),
            BtcNetwork::Testnet => {
                format!("https://blockstream.info/testnet/api/block/{}/raw", hash)
            }
        };
        let body = Self::get(url)?;
        let block = deserialize::<_, BtcBlock>(Reader::new(&body))
            .map_err(|_| Error::<T>::BtcSserializationError)?;
        Ok(block)
    }
    // Get transaction from btc network
    fn fetch_transaction(hash: &str, network: BtcNetwork) -> Result<BtcTransaction, Error<T>> {
        let url = match network {
            BtcNetwork::Mainnet => format!("https://blockstream.info/api/tx/{}/raw", hash),
            BtcNetwork::Testnet => format!("https://blockstream.info/testnet/api/tx/{}/raw", hash),
        };
        let body = Self::get(url)?;
        let transaction = deserialize::<_, BtcTransaction>(Reader::new(&body))
            .map_err(|_| Error::<T>::BtcSserializationError)?;
        debug::info!("₿ Transaction {}", hash_rev(transaction.hash()));
        Ok(transaction)
    }
    // Broadcast raw transaction to btc network
    fn send_raw_transaction<TX: AsRef<[u8]>>(
        hex_tx: TX,
        network: BtcNetwork,
    ) -> Result<String, Error<T>> {
        let url = match network {
            BtcNetwork::Mainnet => "https://blockstream.info/api/tx",
            BtcNetwork::Testnet => "https://blockstream.info/testnet/api/tx",
        };
        let resp_body = Self::post(url, vec![hex_tx.as_ref()])?;
        let resp_body = str::from_utf8(&resp_body).map_err(|_| {
            debug::warn!("No UTF8 body");
            Error::<T>::HttpBodyNotUTF8
        })?;

        if resp_body.len() == 2 * BtcHash::len_bytes() {
            let hash: String = resp_body.into();
            debug::info!(
                "₿ Send Transaction successfully, Hash: {}, HexTx: {}",
                hash,
                hex::encode(hex_tx.as_ref())
            );
            Ok(hash)
        } else if resp_body.starts_with(SEND_RAW_TX_ERR_PREFIX) {
            if let Some(err) = Self::parse_send_raw_tx_error(resp_body) {
                debug::info!(
                    "₿ Send Transaction error: (code: {}, msg: {}), HexTx: {}",
                    err.code,
                    err.message,
                    hex::encode(hex_tx.as_ref())
                );
            } else {
                debug::info!(
                    "₿ Send Transaction unknown error, HexTx: {}",
                    hex::encode(hex_tx.as_ref())
                );
            }
            Err(Error::<T>::BtcSendRawTxError)
        } else {
            debug::info!(
                "₿ Send Transaction unknown error, HexTx: {}",
                hex::encode(hex_tx.as_ref())
            );
            Err(Error::<T>::BtcSendRawTxError)
        }
    }
    // Parse broadcast's error
    fn parse_send_raw_tx_error(resp_body: &str) -> Option<SendRawTxError> {
        use lite_json::JsonValue;
        let rest_resp = resp_body.trim_start_matches(SEND_RAW_TX_ERR_PREFIX);
        let value = lite_json::parse_json(rest_resp).ok();
        value.and_then(|v| match v {
            JsonValue::Object(obj) => {
                let code = obj
                    .iter()
                    .find(|(k, _)| k == &['c', 'o', 'd', 'e'])
                    .map(|(_, code)| code);
                let message = obj
                    .iter()
                    .find(|(k, _)| k == &['m', 'e', 's', 's', 'a', 'g', 'e'])
                    .map(|(_, msg)| msg);
                match (code, message) {
                    (Some(JsonValue::Number(code)), Some(JsonValue::String(msg))) => {
                        Some(SendRawTxError {
                            code: code.integer,
                            message: msg.iter().collect(),
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        })
    }
}

const SEND_RAW_TX_ERR_PREFIX: &str = "send raw transaction RPC error: ";
struct SendRawTxError {
    code: i64,
    message: String,
}

impl<T: Trait> sp_runtime::BoundToRuntimeAppPublic for Module<T> {
    type Public = T::AuthorityId;
}

impl<T: Trait> pallet_session::OneSessionHandler<T::AccountId> for Module<T> {
    type Key = T::AuthorityId;

    fn on_genesis_session<'a, I: 'a>(validators: I)
    where
        I: Iterator<Item = (&'a T::AccountId, Self::Key)>,
    {
        let keys = validators.map(|x| x.1).collect::<Vec<_>>();
        Self::initialize_keys(&keys);
    }

    fn on_new_session<'a, I: 'a>(changed: bool, validators: I, queued_validators: I)
    where
        I: Iterator<Item = (&'a T::AccountId, Self::Key)>,
    {
        if changed {
            let keys = validators
                .chain(queued_validators)
                .map(|x| x.1)
                .collect::<BTreeSet<_>>();
            Keys::<T>::put(keys.into_iter().collect::<Vec<_>>());
        }
    }

    fn on_disabled(_validator_index: usize) {}
}

impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
    type Call = Call<T>;

    fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
        match call {
            Call::push_header(_height, _header) => {
                ValidTransaction::with_tag_prefix("XGatewayBitcoinOffchain")
                .priority(T::UnsignedPriority::get())
                .and_provides("push_header") // TODO: a tag is required, otherwise the transactions will not be pruned.
                // .and_provides((current_session, authority_id)) provide a tag?
                .longevity(1u64) // FIXME a proper longevity
                .propagate(true)
                .build()
            }
            Call::push_transaction(_tx, _relayed_info, _prev_tx) => {
                ValidTransaction::with_tag_prefix("XGatewayBitcoinOffchain")
                .priority(T::UnsignedPriority::get())
                .and_provides("push_transaction") // TODO: a tag is required, otherwise the transactions will not be pruned.
                // .and_provides((current_session, authority_id)) provide a tag?
                .longevity(1u64) // FIXME a proper longevity
                .propagate(true)
                .build()
            }
            _ => InvalidTransaction::Call.into(),
        }
    }
}
