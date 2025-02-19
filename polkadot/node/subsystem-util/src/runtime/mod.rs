// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Convenient interface to runtime information.

use schnellru::{ByLength, LruMap};

use parity_scale_codec::Encode;
use sp_application_crypto::AppCrypto;
use sp_core::crypto::ByteArray;
use sp_keystore::{Keystore, KeystorePtr};

use polkadot_node_subsystem::{
	errors::RuntimeApiError,
	messages::{RuntimeApiMessage, RuntimeApiRequest},
	overseer, SubsystemSender,
};
use polkadot_primitives::{
	vstaging, CandidateEvent, CandidateHash, CoreState, EncodeAs, GroupIndex, GroupRotationInfo,
	Hash, IndexedVec, OccupiedCore, ScrapedOnChainVotes, SessionIndex, SessionInfo, Signed,
	SigningContext, UncheckedSigned, ValidationCode, ValidationCodeHash, ValidatorId,
	ValidatorIndex, LEGACY_MIN_BACKING_VOTES,
};

use crate::{
	request_availability_cores, request_candidate_events, request_from_runtime,
	request_key_ownership_proof, request_on_chain_votes, request_session_index_for_child,
	request_session_info, request_staging_async_backing_params, request_submit_report_dispute_lost,
	request_unapplied_slashes, request_validation_code_by_hash, request_validator_groups,
};

/// Errors that can happen on runtime fetches.
mod error;

use error::Result;
pub use error::{recv_runtime, Error, FatalError, JfyiError};

const LOG_TARGET: &'static str = "parachain::runtime-info";

/// Configuration for construction a `RuntimeInfo`.
pub struct Config {
	/// Needed for retrieval of `ValidatorInfo`
	///
	/// Pass `None` if you are not interested.
	pub keystore: Option<KeystorePtr>,

	/// How many sessions should we keep in the cache?
	pub session_cache_lru_size: u32,
}

/// Caching of session info.
///
/// It should be ensured that a cached session stays live in the cache as long as we might need it.
pub struct RuntimeInfo {
	/// Get the session index for a given relay parent.
	///
	/// We query this up to a 100 times per block, so caching it here without roundtrips over the
	/// overseer seems sensible.
	session_index_cache: LruMap<Hash, SessionIndex>,

	/// Look up cached sessions by `SessionIndex`.
	session_info_cache: LruMap<SessionIndex, ExtendedSessionInfo>,

	/// Key store for determining whether we are a validator and what `ValidatorIndex` we have.
	keystore: Option<KeystorePtr>,
}

/// `SessionInfo` with additional useful data for validator nodes.
pub struct ExtendedSessionInfo {
	/// Actual session info as fetched from the runtime.
	pub session_info: SessionInfo,
	/// Contains useful information about ourselves, in case this node is a validator.
	pub validator_info: ValidatorInfo,
}

/// Information about ourselves, in case we are an `Authority`.
///
/// This data is derived from the `SessionInfo` and our key as found in the keystore.
pub struct ValidatorInfo {
	/// The index this very validator has in `SessionInfo` vectors, if any.
	pub our_index: Option<ValidatorIndex>,
	/// The group we belong to, if any.
	pub our_group: Option<GroupIndex>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			keystore: None,
			// Usually we need to cache the current and the last session.
			session_cache_lru_size: 2,
		}
	}
}

impl RuntimeInfo {
	/// Create a new `RuntimeInfo` for convenient runtime fetches.
	pub fn new(keystore: Option<KeystorePtr>) -> Self {
		Self::new_with_config(Config { keystore, ..Default::default() })
	}

	/// Create with more elaborate configuration options.
	pub fn new_with_config(cfg: Config) -> Self {
		Self {
			session_index_cache: LruMap::new(ByLength::new(cfg.session_cache_lru_size.max(10))),
			session_info_cache: LruMap::new(ByLength::new(cfg.session_cache_lru_size)),
			keystore: cfg.keystore,
		}
	}

	/// Returns the session index expected at any child of the `parent` block.
	/// This does not return the session index for the `parent` block.
	pub async fn get_session_index_for_child<Sender>(
		&mut self,
		sender: &mut Sender,
		parent: Hash,
	) -> Result<SessionIndex>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		match self.session_index_cache.get(&parent) {
			Some(index) => Ok(*index),
			None => {
				let index =
					recv_runtime(request_session_index_for_child(parent, sender).await).await?;
				self.session_index_cache.insert(parent, index);
				Ok(index)
			},
		}
	}

	/// Get `ExtendedSessionInfo` by relay parent hash.
	pub async fn get_session_info<'a, Sender>(
		&'a mut self,
		sender: &mut Sender,
		relay_parent: Hash,
	) -> Result<&'a ExtendedSessionInfo>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		let session_index = self.get_session_index_for_child(sender, relay_parent).await?;

		self.get_session_info_by_index(sender, relay_parent, session_index).await
	}

	/// Get `ExtendedSessionInfo` by session index.
	///
	/// `request_session_info` still requires the parent to be passed in, so we take the parent
	/// in addition to the `SessionIndex`.
	pub async fn get_session_info_by_index<'a, Sender>(
		&'a mut self,
		sender: &mut Sender,
		parent: Hash,
		session_index: SessionIndex,
	) -> Result<&'a ExtendedSessionInfo>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		if self.session_info_cache.get(&session_index).is_none() {
			let session_info =
				recv_runtime(request_session_info(parent, session_index, sender).await)
					.await?
					.ok_or(JfyiError::NoSuchSession(session_index))?;
			let validator_info = self.get_validator_info(&session_info)?;

			let full_info = ExtendedSessionInfo { session_info, validator_info };

			self.session_info_cache.insert(session_index, full_info);
		}
		Ok(self
			.session_info_cache
			.get(&session_index)
			.expect("We just put the value there. qed."))
	}

	/// Convenience function for checking the signature of something signed.
	pub async fn check_signature<Sender, Payload, RealPayload>(
		&mut self,
		sender: &mut Sender,
		relay_parent: Hash,
		signed: UncheckedSigned<Payload, RealPayload>,
	) -> Result<
		std::result::Result<Signed<Payload, RealPayload>, UncheckedSigned<Payload, RealPayload>>,
	>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
		Payload: EncodeAs<RealPayload> + Clone,
		RealPayload: Encode + Clone,
	{
		let session_index = self.get_session_index_for_child(sender, relay_parent).await?;
		let info = self.get_session_info_by_index(sender, relay_parent, session_index).await?;
		Ok(check_signature(session_index, &info.session_info, relay_parent, signed))
	}

	/// Build `ValidatorInfo` for the current session.
	///
	///
	/// Returns: `None` if not a parachain validator.
	fn get_validator_info(&self, session_info: &SessionInfo) -> Result<ValidatorInfo> {
		if let Some(our_index) = self.get_our_index(&session_info.validators) {
			// Get our group index:
			let our_group =
				session_info.validator_groups.iter().enumerate().find_map(|(i, g)| {
					g.iter().find_map(|v| {
						if *v == our_index {
							Some(GroupIndex(i as u32))
						} else {
							None
						}
					})
				});
			let info = ValidatorInfo { our_index: Some(our_index), our_group };
			return Ok(info)
		}
		return Ok(ValidatorInfo { our_index: None, our_group: None })
	}

	/// Get our `ValidatorIndex`.
	///
	/// Returns: None if we are not a validator.
	fn get_our_index(
		&self,
		validators: &IndexedVec<ValidatorIndex, ValidatorId>,
	) -> Option<ValidatorIndex> {
		let keystore = self.keystore.as_ref()?;
		for (i, v) in validators.iter().enumerate() {
			if Keystore::has_keys(&**keystore, &[(v.to_raw_vec(), ValidatorId::ID)]) {
				return Some(ValidatorIndex(i as u32))
			}
		}
		None
	}
}

/// Convenience function for quickly checking the signature on signed data.
pub fn check_signature<Payload, RealPayload>(
	session_index: SessionIndex,
	session_info: &SessionInfo,
	relay_parent: Hash,
	signed: UncheckedSigned<Payload, RealPayload>,
) -> std::result::Result<Signed<Payload, RealPayload>, UncheckedSigned<Payload, RealPayload>>
where
	Payload: EncodeAs<RealPayload> + Clone,
	RealPayload: Encode + Clone,
{
	let signing_context = SigningContext { session_index, parent_hash: relay_parent };

	session_info
		.validators
		.get(signed.unchecked_validator_index())
		.ok_or_else(|| signed.clone())
		.and_then(|v| signed.try_into_checked(&signing_context, v))
}

/// Request availability cores from the runtime.
pub async fn get_availability_cores<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<CoreState>>
where
	Sender: overseer::SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_availability_cores(relay_parent, sender).await).await
}

/// Variant of `request_availability_cores` that only returns occupied ones.
pub async fn get_occupied_cores<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<OccupiedCore>>
where
	Sender: overseer::SubsystemSender<RuntimeApiMessage>,
{
	let cores = get_availability_cores(sender, relay_parent).await?;

	Ok(cores
		.into_iter()
		.filter_map(|core_state| {
			if let CoreState::Occupied(occupied) = core_state {
				Some(occupied)
			} else {
				None
			}
		})
		.collect())
}

/// Get group rotation info based on the given `relay_parent`.
pub async fn get_group_rotation_info<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<GroupRotationInfo>
where
	Sender: overseer::SubsystemSender<RuntimeApiMessage>,
{
	// We drop `groups` here as we don't need them, because of `RuntimeInfo`. Ideally we would not
	// fetch them in the first place.
	let (_, info) = recv_runtime(request_validator_groups(relay_parent, sender).await).await?;
	Ok(info)
}

/// Get `CandidateEvent`s for the given `relay_parent`.
pub async fn get_candidate_events<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<CandidateEvent>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_candidate_events(relay_parent, sender).await).await
}

/// Get on chain votes.
pub async fn get_on_chain_votes<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Option<ScrapedOnChainVotes>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_on_chain_votes(relay_parent, sender).await).await
}

/// Fetch `ValidationCode` by hash from the runtime.
pub async fn get_validation_code_by_hash<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	validation_code_hash: ValidationCodeHash,
) -> Result<Option<ValidationCode>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_validation_code_by_hash(relay_parent, validation_code_hash, sender).await)
		.await
}

/// Fetch a list of `PendingSlashes` from the runtime.
pub async fn get_unapplied_slashes<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<(SessionIndex, CandidateHash, vstaging::slashing::PendingSlashes)>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_unapplied_slashes(relay_parent, sender).await).await
}

/// Generate validator key ownership proof.
///
/// Note: The choice of `relay_parent` is important here, it needs to match
/// the desired session index of the validator set in question.
pub async fn key_ownership_proof<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	validator_id: ValidatorId,
) -> Result<Option<vstaging::slashing::OpaqueKeyOwnershipProof>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_key_ownership_proof(relay_parent, validator_id, sender).await).await
}

/// Submit a past-session dispute slashing report.
pub async fn submit_report_dispute_lost<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	dispute_proof: vstaging::slashing::DisputeProof,
	key_ownership_proof: vstaging::slashing::OpaqueKeyOwnershipProof,
) -> Result<Option<()>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(
		request_submit_report_dispute_lost(
			relay_parent,
			dispute_proof,
			key_ownership_proof,
			sender,
		)
		.await,
	)
	.await
}

/// Prospective parachains mode of a relay parent. Defined by
/// the Runtime API version.
///
/// Needed for the period of transition to asynchronous backing.
#[derive(Debug, Copy, Clone)]
pub enum ProspectiveParachainsMode {
	/// Runtime API without support of `async_backing_params`: no prospective parachains.
	Disabled,
	/// vstaging runtime API: prospective parachains.
	Enabled {
		/// The maximum number of para blocks between the para head in a relay parent
		/// and a new candidate. Restricts nodes from building arbitrary long chains
		/// and spamming other validators.
		max_candidate_depth: usize,
		/// How many ancestors of a relay parent are allowed to build candidates on top
		/// of.
		allowed_ancestry_len: usize,
	},
}

impl ProspectiveParachainsMode {
	/// Returns `true` if mode is enabled, `false` otherwise.
	pub fn is_enabled(&self) -> bool {
		matches!(self, ProspectiveParachainsMode::Enabled { .. })
	}
}

/// Requests prospective parachains mode for a given relay parent based on
/// the Runtime API version.
pub async fn prospective_parachains_mode<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<ProspectiveParachainsMode>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let result =
		recv_runtime(request_staging_async_backing_params(relay_parent, sender).await).await;

	if let Err(error::Error::RuntimeRequest(RuntimeApiError::NotSupported { runtime_api_name })) =
		&result
	{
		gum::trace!(
			target: LOG_TARGET,
			?relay_parent,
			"Prospective parachains are disabled, {} is not supported by the current Runtime API",
			runtime_api_name,
		);

		Ok(ProspectiveParachainsMode::Disabled)
	} else {
		let vstaging::AsyncBackingParams { max_candidate_depth, allowed_ancestry_len } = result?;
		Ok(ProspectiveParachainsMode::Enabled {
			max_candidate_depth: max_candidate_depth as _,
			allowed_ancestry_len: allowed_ancestry_len as _,
		})
	}
}

/// Request the min backing votes value.
/// Prior to runtime API version 6, just return a hardcoded constant.
pub async fn request_min_backing_votes(
	parent: Hash,
	session_index: SessionIndex,
	sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
) -> Result<u32> {
	let min_backing_votes_res = recv_runtime(
		request_from_runtime(parent, sender, |tx| {
			RuntimeApiRequest::MinimumBackingVotes(session_index, tx)
		})
		.await,
	)
	.await;

	if let Err(Error::RuntimeRequest(RuntimeApiError::NotSupported { .. })) = min_backing_votes_res
	{
		gum::trace!(
			target: LOG_TARGET,
			?parent,
			"Querying the backing threshold from the runtime is not supported by the current Runtime API",
		);

		Ok(LEGACY_MIN_BACKING_VOTES)
	} else {
		min_backing_votes_res
	}
}
