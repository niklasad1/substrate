// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! # Intel SGX Enclave Hello World

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use core::sync::atomic::{AtomicBool, Ordering};
use frame_support::{
	debug, decl_module, decl_storage, decl_event, decl_error,
	dispatch::DispatchResult,
	weights::Pays
};
use frame_system::{self as system, offchain, ensure_signed};
use frame_system::offchain::{SendSignedTransaction, Signer};
use sp_core::crypto::KeyTypeId;
use sp_runtime::{
	RuntimeDebug,
	offchain::http,
	traits::Hash,
	transaction_validity::{TransactionValidity, TransactionSource}
};
use sp_std::vec::Vec;
use sp_std::*;

#[cfg(test)]
mod tests;

/// Defines application identifier for crypto keys of this module.
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"sgx!");

static REGISTRATION_BUSY: AtomicBool = AtomicBool::new(false);
static CALL_BUSY: AtomicBool = AtomicBool::new(false);

pub mod crypto {
	use crate::KEY_TYPE;
	use sp_core::sr25519::Signature as Sr25519Signature;
	use sp_runtime::{
		app_crypto::{app_crypto, sr25519},
		traits::Verify,
		MultiSignature, MultiSigner,
	};

	app_crypto!(sr25519, KEY_TYPE);

	pub struct TestAuthId;
	// implemented for ocw-runtime
	impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}

	// implemented for mock runtime in test
	impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature>
		for TestAuthId
	{
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}
}

#[derive(Encode, Decode, Default, Clone, PartialEq, Eq, RuntimeDebug)]
pub struct QuotingReport {
    pub cpusvn: [u8; 16],
    pub miscselect: u32,
    pub attributes: [u8; 16],
	/// SHA 256 of enclave measurement
    pub mrenclave: [u8; 32],
	/// Enclave public signing key
    pub mrsigner: [u8; 32],
    pub isvprodid: u16,
    pub isvsvn: u16,
    pub reportdata: Vec<u8>,
}

impl QuotingReport {
	/// Poor man's deserialization based on
	/// https://api.trustedservices.intel.com/documents/sgx-attestation-api-spec.pdf 4.3.1
	pub fn from_bytes(bytes: &[u8]) -> Self {
		debug::trace!(target: "sgx", "[QuotingReport::from_bytes] bytes: {:?}", bytes);
		let mut cpusvn = [0_u8; 16];
		let mut miscselect = [0_u8; 4];
		let mut attributes = [0_u8; 16];
		let mut mrenclave = [0_u8; 32];
		let mut mrsigner = [0_u8; 32];
		let mut isvprodid = [0_u8; 2];
		let mut isvsvn = [0_u8; 2];
		let mut reportdata = vec![0_u8; 64];

		cpusvn.copy_from_slice(&bytes[48..48+16]);
		miscselect.copy_from_slice(&bytes[64..64+4]);
		attributes.copy_from_slice(&bytes[96..96+16]);
		mrenclave.copy_from_slice(&bytes[112..112+32]);
		mrsigner.copy_from_slice(&bytes[176..176+32]);
		isvprodid.copy_from_slice(&bytes[304..304+2]);
		isvsvn.copy_from_slice(&bytes[306..306+2]);
		reportdata.copy_from_slice(&bytes[368..368+64]);

		Self {
			cpusvn,
			miscselect: u32::from_le_bytes(miscselect),
			attributes,
			mrenclave,
			mrsigner,
			isvprodid: u16::from_le_bytes(isvprodid),
			isvsvn: u16::from_le_bytes(isvsvn),
			reportdata,
		}
	}
}

// Note: keep in sync with subxt_sgx_runtime::Enclave
#[derive(Encode, Decode, Default, Clone, PartialEq, Eq, RuntimeDebug)]
pub struct Enclave {
	pub quote: QuotingReport,
	pub address: Vec<u8>,
	pub timestamp: u64,
	pub public_key: Vec<u8>,
}

type EnclaveAddress = Vec<u8>;

/// This pallet's configuration trait
pub trait Trait: frame_system::Trait + offchain::CreateSignedTransaction<Call<Self>>  {
	/// The identifier type for an authority.
	type AuthorityId: offchain::AppCrypto<Self::Public, Self::Signature>;
    /// The overarching dispatch call type.
    type Call: From<Call<Self>>;
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
}

decl_error! {
    pub enum Error for Module<T: Trait> {
		/// The enclave is already registrered
        EnclaveAlreadyRegistered,
		/// The enclave is not registrered
		EnclaveNotFound
    }
}

decl_storage! {
	trait Store for Module<T: Trait> as SgxHelloWorld {
		/// Enclaves that are verified (i.e, verified via remote attestation)
		VerifiedEnclaves get(fn verified_enclaves): map hasher(blake2_128_concat) T::AccountId => Enclave;
		/// Enclaves that are waiting to be verified
		UnverifiedEnclaves get(fn unverified_enclaves): Vec<(T::AccountId, EnclaveAddress)>;
		/// Waiting enclave calls
		WaitingEnclaveCalls get(fn waiting_calls): Vec<(T::AccountId, Vec<u8>)>;
	}
}

decl_event!(
	pub enum Event<T> where AccountId = <T as frame_system::Trait>::AccountId {
		EnclaveAdded(AccountId),
		EnclaveRemoved(AccountId),
		EnclaveCallSuccess(Vec<u8>),
		EnclaveCallFailure(Vec<u8>),
	}
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		/// Try to register an enclave. Enqueues the candidate enclave in the `UnverifiedEnclaves` storage item. At a later
		/// time the worker will perform RA on the enclave and, if successful, add it to the `VerifiedEnclaves` storage item.
		///
		/// The transaction has to be signed with the enclave's signing key to work
		#[weight = (100, Pays::No)]
		pub fn register_enclave(origin, url: Vec<u8>) -> DispatchResult {
			debug::info!(target: "sgx", "[register_enclave] START, url: {:?}", url);
			let enclave = ensure_signed(origin)?;
			if <VerifiedEnclaves<T>>::contains_key(&enclave) {
				Err(Error::<T>::EnclaveAlreadyRegistered.into())
			} else {
				let mut unverified_enclaves = UnverifiedEnclaves::<T>::get();
				debug::trace!(target: "sgx", "[register_enclave] Unverified enclaves: {:?}; trying to register a new one: {:?}", unverified_enclaves.len(), enclave);
				match unverified_enclaves.binary_search_by(|(s, _)| s.cmp(&enclave)) {
					Ok(_) => Err(Error::<T>::EnclaveAlreadyRegistered.into()),
					Err(idx) => {
						debug::trace!(target: "sgx", "[register_enclave] register unverified_encalve; who={:?} at address={:?}", enclave, url);
						unverified_enclaves.insert(idx, (enclave, url));
						UnverifiedEnclaves::<T>::put(unverified_enclaves);
						Ok(())
					}
				}
			}
		}

		/// Try to deregister an enclave.
		///
		/// The transaction has to be signed with the enclave's signing key to work
		#[weight = (100, Pays::No)]
		pub fn deregister_enclave(origin) -> DispatchResult {
			let enclave = ensure_signed(origin)?;
			if <VerifiedEnclaves<T>>::contains_key(&enclave) {
				debug::info!(target: "sgx", "deregister who={:?}", enclave);
				<VerifiedEnclaves<T>>::remove(enclave.clone());
				Self::deposit_event(RawEvent::EnclaveRemoved(enclave));
				Ok(())
			} else {
				debug::info!(target: "sgx", "deregister who={:?} failed", enclave);
				Err(Error::<T>::EnclaveNotFound.into())
			}
		}

		/// Enqueue an encrypted extrinsic to be sent to the enclave/
		#[weight = 100]
		pub fn call_enclave(
			origin,
			enclave: T::AccountId,
			xt: Vec<u8>
		) -> DispatchResult {
			let _who = ensure_signed(origin)?;
			if <VerifiedEnclaves<T>>::contains_key(&enclave) {
				debug::info!(target: "sgx", "call_enclave; who={:?} with payload={:?}", enclave, xt);
				let mut waiting_calls = <WaitingEnclaveCalls<T>>::get();
				waiting_calls.push((enclave, xt));
				waiting_calls.sort();
				<WaitingEnclaveCalls<T>>::put(waiting_calls);
				Ok(())
			} else {
				debug::info!(target: "sgx", "call_enclave failed who={:?} not found", enclave);
				Err(Error::<T>::EnclaveNotFound.into())
			}
		}

		#[weight = (100, Pays::No)]
		fn enclave_remove_waiting_call(
			origin,
			dispatched_call: (T::AccountId, Vec<u8>),
			success: bool
		) -> DispatchResult {
			debug::trace!(target: "sgx", "remove waiting_call dispatched by={:?} output was success={} with payload={:?}", dispatched_call.0, success, dispatched_call.1);
			let _who = ensure_signed(origin)?;
			let mut waiting_calls = <WaitingEnclaveCalls<T>>::get();
			CALL_BUSY.compare_and_swap(true, false, Ordering::Relaxed);

			match waiting_calls.binary_search(&dispatched_call) {
				Ok(idx) => {
					waiting_calls.remove(idx);
					<WaitingEnclaveCalls<T>>::put(waiting_calls);
					let hash = T::Hashing::hash_of(&(&dispatched_call.0, &dispatched_call.1));
					let event = if success {
						RawEvent::EnclaveCallSuccess(hash.as_ref().to_vec())
					} else {
						RawEvent::EnclaveCallFailure(hash.as_ref().to_vec())
					};
					Self::deposit_event(event);
					Ok(())
				}
				Err(_) => {
					debug::error!(target: "sgx", "dispatched call to unknown enclave={:?} or unknown payload", dispatched_call.0);
					Err(Error::<T>::EnclaveNotFound.into())
				}
			}
		}

		#[weight = (100, Pays::No)]
		fn prune_unverified_enclaves(origin) -> DispatchResult {
			debug::info!(target: "sgx", "prune unverified enclaves");
			let _who = ensure_signed(origin)?;
			<UnverifiedEnclaves<T>>::kill();
			Ok(())
		}

		#[weight = (100, Pays::No)]
		fn register_verified_enclave(origin, enclave_id: T::AccountId, enclave: Enclave) -> DispatchResult {
			let _who = ensure_signed(origin)?;
			REGISTRATION_BUSY.compare_and_swap(true, false, Ordering::Relaxed);
			debug::info!(target: "sgx", "register_verified_enclave who={:?} with meta={:?}", enclave_id, enclave);
			<VerifiedEnclaves<T>>::insert(enclave_id.clone(), enclave);
			Self::deposit_event(RawEvent::EnclaveAdded(enclave_id));
			Ok(())
		}

		fn deposit_event() = default;

		/// Offchain Worker entry point.
		/// First checks for any pending enclave registration requests: if any, perform RA on each of them.
		/// Next checks for any pending enclave calls: if any, call `dispatch_waiting_calls`.
		//
		// TODO: use the offchain worker to re-verify the "trusted enclaves"
		// every x block or maybe could be done in `on_initialize` or `on_finalize`
		fn offchain_worker(block_number: T::BlockNumber) {
			debug::trace!(target: "sgx", "[offchain_worker] START at block_number: {:?}", block_number);


			let signer = Signer::<T, T::AuthorityId>::any_account();
			if !signer.can_sign() {
				debug::error!(target: "sgx", "No local accounts available. Consider adding one via `author_insertKey` RPC with keytype \"sgx!\"");
				return;
			}

			let waiting_enclaves = <UnverifiedEnclaves<T>>::get();
			if !waiting_enclaves.is_empty() {
				debug::trace!(target: "sgx", "[offchain_worker, #{:?}] There are {} enclaves awaiting registration", block_number, waiting_enclaves.len());
				if !REGISTRATION_BUSY.compare_and_swap(false, true, Ordering::Relaxed) {
					debug::trace!(target: "sgx", "[offchain_worker, #{:?}] Doing RA.", block_number);
					match Self::remote_attest_unverified_enclaves(block_number, &signer) {
						Ok(_) => debug::debug!(target: "sgx", "[offchain_worker, #{:?}] RA successful", block_number),
						Err(e) => debug::warn!(target: "sgx", "[offchain_worker, #{:?}] RA error: {:?}", block_number, e)
					};
				} else {
					debug::trace!(target: "sgx", "[offchain_worker, #{:?}] NOT doing RA – already in progress.", block_number);
				}
			}

			let waiting_calls = <WaitingEnclaveCalls<T>>::get();
			if !waiting_calls.is_empty() {
				debug::trace!(target: "sgx", "[offchain_worker, #{:?}] There are {} waiting enclave calls", block_number, waiting_calls.len());
				if !CALL_BUSY.compare_and_swap(false, true, Ordering::Relaxed) {
					match Self::dispatch_waiting_calls(block_number, &signer) {
						Ok(_) => debug::debug!(target: "sgx", "[offchain_worker, #{:?}] enclave call successful", block_number),
						Err(e) => debug::warn!(target: "sgx", "[offchain_worker, #{:?}] enclave call error: {:?}", block_number, e)
					};
				} else {
					debug::trace!(target: "sgx", "[offchain_worker, #{:?}] not dispatching waiting enclave calls - already in progress", block_number);
				}
			}

			// TODO: re-verify "trusted enclaves"
		}
	}
}

impl<T: Trait> Module<T> {
	fn remote_attest_unverified_enclaves(block_number: T::BlockNumber, signer: &Signer<T, T::AuthorityId, frame_system::offchain::ForAny>) -> Result<(), &'static str> {
		debug::trace!(target: "sgx", "[remote_attest_unverified_enclaves] START at block_number: {:?}", block_number);
		let mut verified = Vec::new();

		for (enclave_sign, enclave_addr) in <UnverifiedEnclaves<T>>::get() {
			debug::trace!(target: "sgx", "[remote_attest_unverified_enclaves] Getting public key for {:?}/{:?}", enclave_sign, enclave_addr);
			let public_key = match Self::get_enclave_public_key(&enclave_addr) {
				Ok(pk) => pk,
				Err(e) => {
					debug::warn!(target: "sgx", "[remote_attest_unverified_enclaves] Could not get public key for enclave at {:?}/{:?}: {:?}. Is the enclave running? Ignoring.", enclave_sign, enclave_addr, e);
					continue
				}
			};
			debug::trace!(target: "sgx", "[remote_attest_unverified_enclaves] Sending RA for {:?}/{:?}", enclave_sign, enclave_addr);
			let qe = match Self::send_ra_request(&enclave_sign, &enclave_addr) {
				Ok(qe) => qe,
				Err(e) => {
					debug::warn!(target: "sgx", "[remote_attest_unverified_enclaves] request failed: {}. Enclave might be down; ignoring", e);
					continue
				}
			};

			let enclave = Enclave {
				address: enclave_addr.clone(),
				quote: QuotingReport::from_bytes(&qe),
				timestamp: sp_io::offchain::timestamp().unix_millis(),
				public_key,
			};
			debug::info!(target: "sgx", "[remote_attest_unverified_enclaves] received quoting_report: {:?}", enclave.quote);
			let vr = match Self::get_ias_verification_report(&qe) {
				Ok(vr) => vr,
				Err(e) => {
					debug::warn!(target: "sgx", "[remote_attest_unverified_enclaves] IAS request failed with error: {}", e);
					continue
				}
			};

			debug::info!(target: "sgx", "[remote_attest_unverified_enclaves] received ias_verification_report: {:?}", sp_std::str::from_utf8(&vr).unwrap());
			debug::warn!(target: "sgx", "[remote_attest_unverified_enclaves] ias_verification_report is not used yet");
			verified.push((enclave_sign, enclave))
		}

		signer.send_signed_transaction(|_account| {
			debug::trace!(target: "sgx", "Sending signed transaction to prune unverified enclaves");
			Call::prune_unverified_enclaves()
		});

		for (enclave_sign, enclave) in verified {
			signer.send_signed_transaction(|_account| {
				debug::trace!(target: "sgx", "Sending signed transaction to register enclave with AccountId={:?} on chain", enclave_sign);
				Call::register_verified_enclave(enclave_sign.clone(), enclave.clone())
			});
		}

		Ok(())
	}

	fn dispatch_waiting_calls(
		block_number: T::BlockNumber,
		signer: &Signer<T, T::AuthorityId, frame_system::offchain::ForAny>
	) -> Result<(), &'static str> {
		debug::trace!(target: "sgx", "[dispatch_waiting_calls] START at block_number: {:?}", block_number);
		let pending_calls = <WaitingEnclaveCalls<T>>::get();
		debug::trace!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Pending enclave calls: {}", block_number, pending_calls.len());
		let mut dispatched = Vec::new();
		let mut fail_count = 0;

		for (enclave_id, xt) in pending_calls {
			if !<VerifiedEnclaves<T>>::contains_key(&enclave_id) {
				continue;
			}
			let enclave = <VerifiedEnclaves<T>>::get(&enclave_id);
			debug::trace!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Enclave: {:?}, enclave id: {:?}", block_number, enclave, enclave_id);
			let mut full_address = Vec::new();
			full_address.extend(&enclave.address);
			full_address.extend("/enclave_call".as_bytes());
			let enclave_addr = sp_std::str::from_utf8(&full_address).unwrap();
			debug::info!(target: "sgx", "[dispatch_waiting_calls, #{:?}]: sending enclave_call to={:?} at address={:?}", block_number, enclave_id, enclave_addr);

			let mut success = false;
			let enclave_request = http::Request::post(&enclave_addr, vec![&xt])
				.add_header("substrate_sgx", "1.0")
				.send()
				.and_then(|r| Ok(r.wait()));
			match enclave_request {
				Ok(Ok(r)) if r.code >= 200 && r.code < 300 => {
					debug::info!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Enclave call was successful.", block_number);
					success = true;
				},
				Ok(Ok(response)) => {
					fail_count += 1;
					let body = response.body().collect::<Vec<u8>>();
					let body = sp_std::str::from_utf8(&body).unwrap();
					debug::warn!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Enclave call failed with HTTP status: {}, body: {:?}",
						block_number, response.code, body);
				},
				Ok(Err(e)) => {
					fail_count += 1;
					debug::warn!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Unexpected error: {:?}", block_number, e);
				},
				Err(e) => {
					fail_count += 1;
					debug::warn!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Transport error: {:?}", block_number, e);
				}
			}
			dispatched.push((enclave_id, xt, success));
		}

		for (enclave, xt, success) in dispatched {
			signer.send_signed_transaction(|_account| {
				debug::trace!(target: "sgx", "[dispatch_waiting_calls, #{:?}] Sending signed transaction to remove dispatched enclave call", block_number);
				Call::enclave_remove_waiting_call((enclave.clone(), xt.clone()), success)
			});
		}

		if fail_count == 0 {
			Ok(())
		} else {
			Err("There were failed enclave calls")
		}
	}

	/// Request a QUOTE from the enclave (proxied by the client)
	fn send_ra_request(signer: &T::AccountId, enclave_addr: &[u8]) -> Result<Vec<u8>, &'static str> {
		let mut full_address: Vec<u8> = Vec::new();
		full_address.extend(enclave_addr);
		full_address.extend("/quoting_report".as_bytes());
		let enclave_addr = sp_std::str::from_utf8(&full_address).map_err(|_e| "enclave address must be valid utf8")?;
		let body = vec![b"remote_attest\r\n"];
		debug::debug!(target: "sgx","[send_ra_request]: sending remote attestion request to enclave={:?} at address={:?}", signer, enclave_addr);
		let pending = http::Request::post(&enclave_addr, body)
			.add_header("substrate_sgx", "1.0")
			.send()
			.unwrap();
		let response = pending.wait().expect("http IO error");
		Ok(response.body().collect())
	}

	fn get_enclave_public_key(enclave_addr: &[u8]) -> Result<Vec<u8>, &'static str> {
		let mut endpoint = vec![];
		endpoint.extend(enclave_addr);
		endpoint.extend("/public_key".as_bytes());
		let endpoint = sp_std::str::from_utf8(&endpoint)
			.map_err(|_e| "enclave public key endpoint address must be valid utf8")?;
		debug::debug!(target: "sgx","[get_enclave_public_key]: fetching public key from enclave at address={:?}", endpoint);
		let req = http::Request::get(&endpoint)
			.add_header("substrate_sgx", "1.0")
			.send()
			.expect("enclave has a public_key endpoint");
		let response = req.wait().expect("http works");
		Ok(response.body().collect())
	}

	// https://api.trustedservices.intel.com/documents/sgx-attestation-api-spec.pdf
	/// Send the QUOTE obtained from the enclave to Intel
	fn get_ias_verification_report(quote: &[u8]) -> Result<Vec<u8>, &'static str> {
		debug::trace!(target: "sgx", "[get_ias_verification_report] START");
		const IAS_REPORT_URL: &str = "https://api.trustedservices.intel.com/sgx/dev/attestation/v4/report";
		const API_KEY: &str = "e9589de0dfe5482588600a73d08b70f6";

		// { "isvEnclaveQuote": "<base64 encoded quote>" }
		let encoded_quote = base64::encode(&quote);
		let mut body = Vec::new();
		body.push("{\"isvEnclaveQuote\":");
		body.push("\"");
		body.push(&encoded_quote);
		body.push("\"}");

		let pending = http::Request::post(IAS_REPORT_URL, body)
			.add_header("Content-Type", "application/json")
			.add_header("Ocp-Apim-Subscription-Key", API_KEY)
			.send()
			.unwrap();
		debug::trace!(target: "sgx", "[get_ias_verification_report] waiting for request to complete");
		let response = pending.wait().expect("http IO error");
		if response.code == 200 {
			Ok(response.body().collect())
		} else {
			Err("Intel IAS error")
		}
	}
}

#[allow(deprecated)] // ValidateUnsigned
impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
	type Call = Call<T>;

	fn validate_unsigned(
		_source: TransactionSource,
		_call: &Self::Call,
	) -> TransactionValidity {
		todo!("implement when sgx_hello_world is using unsigned transactions");
	}
}
