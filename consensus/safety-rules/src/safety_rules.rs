// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    configurable_validator_signer::ConfigurableValidatorSigner,
    consensus_state::ConsensusState,
    counters,
    error::Error,
    logging::{LogEntry, LogEvent, SafetyLogSchema},
    persistent_safety_storage::PersistentSafetyStorage,
    t_safety_rules::TSafetyRules,
};
use aptos_crypto::{
    ed25519::{Ed25519PublicKey, Ed25519Signature},
    hash::CryptoHash,
    traits::Signature,
};
use aptos_logger::prelude::*;
use aptos_types::{
    epoch_change::EpochChangeProof,
    epoch_state::EpochState,
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    waypoint::Waypoint,
};
use consensus_types::{
    block_data::BlockData,
    common::{Author, Round},
    quorum_cert::QuorumCert,
    safety_data::SafetyData,
    timeout_2chain::{TwoChainTimeout, TwoChainTimeoutCertificate},
    vote::Vote,
    vote_data::VoteData,
    vote_proposal::MaybeSignedVoteProposal,
};
use serde::Serialize;
use std::cmp::Ordering;

pub(crate) fn next_round(round: Round) -> Result<Round, Error> {
    u64::checked_add(round, 1).ok_or(Error::IncorrectRound(round))
}

/// @TODO consider a cache of verified QCs to cut down on verification costs
pub struct SafetyRules {
    pub(crate) persistent_storage: PersistentSafetyStorage,
    pub(crate) execution_public_key: Option<Ed25519PublicKey>,
    pub(crate) export_consensus_key: bool,
    pub(crate) validator_signer: Option<ConfigurableValidatorSigner>,
    pub(crate) epoch_state: Option<EpochState>,
}

impl SafetyRules {
    /// Constructs a new instance of SafetyRules with the given persistent storage and the
    /// consensus private keys
    pub fn new(
        persistent_storage: PersistentSafetyStorage,
        verify_vote_proposal_signature: bool,
        export_consensus_key: bool,
    ) -> Self {
        let execution_public_key = if verify_vote_proposal_signature {
            Some(
                persistent_storage
                    .execution_public_key()
                    .expect("Unable to retrieve execution public key"),
            )
        } else {
            None
        };
        Self {
            persistent_storage,
            execution_public_key,
            export_consensus_key,
            validator_signer: None,
            epoch_state: None,
        }
    }

    /// Validity checks
    pub(crate) fn verify_proposal(
        &mut self,
        maybe_signed_vote_proposal: &MaybeSignedVoteProposal,
    ) -> Result<VoteData, Error> {
        let vote_proposal = &maybe_signed_vote_proposal.vote_proposal;
        let execution_signature = maybe_signed_vote_proposal.signature.as_ref();

        if let Some(public_key) = self.execution_public_key.as_ref() {
            execution_signature
                .ok_or(Error::VoteProposalSignatureNotFound)?
                .verify(vote_proposal, public_key)
                .map_err(|error| Error::InternalError(error.to_string()))?;
        }

        let proposed_block = vote_proposal.block();
        let safety_data = self.persistent_storage.safety_data()?;

        self.verify_epoch(proposed_block.epoch(), &safety_data)?;

        self.verify_qc(proposed_block.quorum_cert())?;
        proposed_block
            .validate_signature(&self.epoch_state()?.verifier)
            .map_err(|error| Error::InvalidProposal(error.to_string()))?;
        proposed_block
            .verify_well_formed()
            .map_err(|error| Error::InvalidProposal(error.to_string()))?;

        vote_proposal
            .gen_vote_data()
            .map_err(|error| Error::InvalidAccumulatorExtension(error.to_string()))
    }

    pub(crate) fn sign<T: Serialize + CryptoHash>(
        &self,
        message: &T,
    ) -> Result<Ed25519Signature, Error> {
        let signer = self.signer()?;
        signer.sign(message, &self.persistent_storage)
    }

    pub(crate) fn signer(&self) -> Result<&ConfigurableValidatorSigner, Error> {
        self.validator_signer
            .as_ref()
            .ok_or_else(|| Error::NotInitialized("validator_signer".into()))
    }

    pub(crate) fn epoch_state(&self) -> Result<&EpochState, Error> {
        self.epoch_state
            .as_ref()
            .ok_or_else(|| Error::NotInitialized("epoch_state".into()))
    }

    pub(crate) fn observe_qc(&self, qc: &QuorumCert, safety_data: &mut SafetyData) -> bool {
        let mut updated = false;
        let one_chain = qc.certified_block().round();
        let two_chain = qc.parent_block().round();
        if one_chain > safety_data.one_chain_round {
            safety_data.one_chain_round = one_chain;
            info!(
                SafetyLogSchema::new(LogEntry::OneChainRound, LogEvent::Update)
                    .preferred_round(safety_data.one_chain_round)
            );
            updated = true;
        }
        if two_chain > safety_data.preferred_round {
            safety_data.preferred_round = two_chain;
            info!(
                SafetyLogSchema::new(LogEntry::PreferredRound, LogEvent::Update)
                    .preferred_round(safety_data.preferred_round)
            );
            updated = true;
        }
        updated
    }

    /// Second voting rule
    fn verify_and_update_preferred_round(
        &mut self,
        quorum_cert: &QuorumCert,
        safety_data: &mut SafetyData,
    ) -> Result<bool, Error> {
        let preferred_round = safety_data.preferred_round;
        let one_chain_round = quorum_cert.certified_block().round();

        if one_chain_round < preferred_round {
            return Err(Error::IncorrectPreferredRound(
                one_chain_round,
                preferred_round,
            ));
        }
        Ok(self.observe_qc(quorum_cert, safety_data))
    }

    /// This verifies whether the author of one proposal is the validator signer
    fn verify_author(&self, author: Option<Author>) -> Result<(), Error> {
        let validator_signer_author = &self.signer()?.author();
        let author = author
            .ok_or_else(|| Error::InvalidProposal("No author found in the proposal".into()))?;
        if validator_signer_author != &author {
            return Err(Error::InvalidProposal(
                "Proposal author is not validator signer!".into(),
            ));
        }
        Ok(())
    }

    /// This verifies the epoch given against storage for consistent verification
    pub(crate) fn verify_epoch(&self, epoch: u64, safety_data: &SafetyData) -> Result<(), Error> {
        if epoch != safety_data.epoch {
            return Err(Error::IncorrectEpoch(epoch, safety_data.epoch));
        }

        Ok(())
    }

    /// First voting rule
    pub(crate) fn verify_and_update_last_vote_round(
        &self,
        round: Round,
        safety_data: &mut SafetyData,
    ) -> Result<(), Error> {
        if round <= safety_data.last_voted_round {
            return Err(Error::IncorrectLastVotedRound(
                round,
                safety_data.last_voted_round,
            ));
        }

        safety_data.last_voted_round = round;
        info!(
            SafetyLogSchema::new(LogEntry::LastVotedRound, LogEvent::Update)
                .last_voted_round(safety_data.last_voted_round)
        );

        Ok(())
    }

    /// This verifies a QC has valid signatures.
    pub(crate) fn verify_qc(&self, qc: &QuorumCert) -> Result<(), Error> {
        let epoch_state = self.epoch_state()?;

        qc.verify(&epoch_state.verifier)
            .map_err(|e| Error::InvalidQuorumCertificate(e.to_string()))?;
        Ok(())
    }

    // Internal functions mapped to the public interface to enable exhaustive logging and metrics

    fn guarded_consensus_state(&mut self) -> Result<ConsensusState, Error> {
        let waypoint = self.persistent_storage.waypoint()?;
        let safety_data = self.persistent_storage.safety_data()?;

        info!(SafetyLogSchema::new(LogEntry::State, LogEvent::Update)
            .author(self.persistent_storage.author()?)
            .epoch(safety_data.epoch)
            .last_voted_round(safety_data.last_voted_round)
            .preferred_round(safety_data.preferred_round)
            .waypoint(waypoint));

        Ok(ConsensusState::new(
            self.persistent_storage.safety_data()?,
            self.persistent_storage.waypoint()?,
            self.signer().is_ok(),
        ))
    }

    fn guarded_initialize(&mut self, proof: &EpochChangeProof) -> Result<(), Error> {
        let waypoint = self.persistent_storage.waypoint()?;
        let last_li = proof
            .verify(&waypoint)
            .map_err(|e| Error::InvalidEpochChangeProof(format!("{}", e)))?;
        let ledger_info = last_li.ledger_info();
        let epoch_state = ledger_info
            .next_epoch_state()
            .cloned()
            .ok_or(Error::InvalidLedgerInfo)?;

        // Update the waypoint to a newer value, this might still be older than the current epoch.
        let new_waypoint = &Waypoint::new_epoch_boundary(ledger_info)
            .map_err(|error| Error::InternalError(error.to_string()))?;
        if new_waypoint.version() > waypoint.version() {
            self.persistent_storage.set_waypoint(new_waypoint)?;
        }

        let current_epoch = self.persistent_storage.safety_data()?.epoch;
        match current_epoch.cmp(&epoch_state.epoch) {
            Ordering::Greater => {
                // waypoint is not up to the current epoch.
                return Err(Error::WaypointOutOfDate(
                    waypoint.version(),
                    new_waypoint.version(),
                    current_epoch,
                    epoch_state.epoch,
                ));
            }
            Ordering::Less => {
                // start new epoch
                self.persistent_storage.set_safety_data(SafetyData::new(
                    epoch_state.epoch,
                    0,
                    0,
                    0,
                    None,
                ))?;

                info!(SafetyLogSchema::new(LogEntry::Epoch, LogEvent::Update)
                    .epoch(epoch_state.epoch));
            }
            Ordering::Equal => (),
        };
        self.epoch_state = Some(epoch_state.clone());

        let author = self.persistent_storage.author()?;
        let expected_key = epoch_state.verifier.get_public_key(&author);
        let initialize_result = match expected_key {
            None => Err(Error::ValidatorNotInSet(author.to_string())),
            Some(expected_key) => {
                let current_key = self.signer().ok().map(|s| s.public_key());
                if current_key == Some(expected_key.clone()) {
                    debug!(
                        SafetyLogSchema::new(LogEntry::KeyReconciliation, LogEvent::Success),
                        "in set",
                    );
                    Ok(())
                } else if self.export_consensus_key {
                    // Try to export the consensus key directly from storage.
                    match self
                        .persistent_storage
                        .consensus_key_for_version(expected_key)
                    {
                        Ok(consensus_key) => {
                            self.validator_signer = Some(ConfigurableValidatorSigner::new_signer(
                                author,
                                consensus_key,
                            ));
                            Ok(())
                        }
                        Err(Error::SecureStorageMissingDataError(error)) => {
                            Err(Error::ValidatorKeyNotFound(error))
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    // Try to generate a signature over a test message to ensure the expected key
                    // is actually held in storage.
                    self.validator_signer = Some(ConfigurableValidatorSigner::new_handle(
                        author,
                        expected_key,
                    ));
                    self.sign(ledger_info)
                        .map(|_signature| ())
                        .map_err(|error| Error::ValidatorKeyNotFound(error.to_string()))
                }
            }
        };
        initialize_result.map_err(|error| {
            info!(
                SafetyLogSchema::new(LogEntry::KeyReconciliation, LogEvent::Error).error(&error),
            );
            self.validator_signer = None;
            error
        })
    }

    fn guarded_sign_proposal(&mut self, block_data: &BlockData) -> Result<Ed25519Signature, Error> {
        self.signer()?;
        self.verify_author(block_data.author())?;

        let mut safety_data = self.persistent_storage.safety_data()?;
        self.verify_epoch(block_data.epoch(), &safety_data)?;

        if block_data.round() <= safety_data.last_voted_round {
            return Err(Error::InvalidProposal(format!(
                "Proposed round {} is not higher than last voted round {}",
                block_data.round(),
                safety_data.last_voted_round
            )));
        }

        self.verify_qc(block_data.quorum_cert())?;
        self.verify_and_update_preferred_round(block_data.quorum_cert(), &mut safety_data)?;
        // we don't persist the updated preferred round to save latency (it'd be updated upon voting)

        let signature = self.sign(block_data)?;
        Ok(signature)
    }

    fn guarded_sign_commit_vote(
        &mut self,
        ledger_info: LedgerInfoWithSignatures,
        new_ledger_info: LedgerInfo,
    ) -> Result<Ed25519Signature, Error> {
        self.signer()?;

        let old_ledger_info = ledger_info.ledger_info();

        if !old_ledger_info.commit_info().is_ordered_only() {
            return Err(Error::InvalidOrderedLedgerInfo(old_ledger_info.to_string()));
        }

        if !old_ledger_info
            .commit_info()
            .match_ordered_only(new_ledger_info.commit_info())
        {
            return Err(Error::InconsistentExecutionResult(
                old_ledger_info.commit_info().to_string(),
                new_ledger_info.commit_info().to_string(),
            ));
        }

        // Verify that ledger_info contains at least 2f + 1 dostinct signatures
        ledger_info
            .verify_signatures(&self.epoch_state()?.verifier)
            .map_err(|error| Error::InvalidQuorumCertificate(error.to_string()))?;

        // TODO: add guarding rules in unhappy path
        // TODO: add extension check

        let signature = self.sign(&new_ledger_info)?;

        Ok(signature)
    }
}

impl TSafetyRules for SafetyRules {
    fn consensus_state(&mut self) -> Result<ConsensusState, Error> {
        let cb = || self.guarded_consensus_state();
        run_and_log(cb, |log| log, LogEntry::ConsensusState)
    }

    fn initialize(&mut self, proof: &EpochChangeProof) -> Result<(), Error> {
        let cb = || self.guarded_initialize(proof);
        run_and_log(cb, |log| log, LogEntry::Initialize)
    }

    fn sign_proposal(&mut self, block_data: &BlockData) -> Result<Ed25519Signature, Error> {
        let round = block_data.round();
        let cb = || self.guarded_sign_proposal(block_data);
        run_and_log(cb, |log| log.round(round), LogEntry::SignProposal)
    }

    fn sign_timeout_with_qc(
        &mut self,
        timeout: &TwoChainTimeout,
        timeout_cert: Option<&TwoChainTimeoutCertificate>,
    ) -> Result<Ed25519Signature, Error> {
        let cb = || self.guarded_sign_timeout_with_qc(timeout, timeout_cert);
        run_and_log(
            cb,
            |log| log.round(timeout.round()),
            LogEntry::SignTimeoutWithQC,
        )
    }

    fn construct_and_sign_vote_two_chain(
        &mut self,
        maybe_signed_vote_proposal: &MaybeSignedVoteProposal,
        timeout_cert: Option<&TwoChainTimeoutCertificate>,
    ) -> Result<Vote, Error> {
        let round = maybe_signed_vote_proposal.vote_proposal.block().round();
        let cb = || {
            self.guarded_construct_and_sign_vote_two_chain(maybe_signed_vote_proposal, timeout_cert)
        };
        run_and_log(
            cb,
            |log| log.round(round),
            LogEntry::ConstructAndSignVoteTwoChain,
        )
    }

    fn sign_commit_vote(
        &mut self,
        ledger_info: LedgerInfoWithSignatures,
        new_ledger_info: LedgerInfo,
    ) -> Result<Ed25519Signature, Error> {
        let cb = || self.guarded_sign_commit_vote(ledger_info, new_ledger_info);
        run_and_log(cb, |log| log, LogEntry::SignCommitVote)
    }
}

fn run_and_log<F, L, R>(callback: F, log_cb: L, log_entry: LogEntry) -> Result<R, Error>
where
    F: FnOnce() -> Result<R, Error>,
    L: for<'a> Fn(SafetyLogSchema<'a>) -> SafetyLogSchema<'a>,
{
    let _timer = counters::start_timer("internal", log_entry.as_str());
    debug!(log_cb(SafetyLogSchema::new(log_entry, LogEvent::Request)));
    counters::increment_query(log_entry.as_str(), "request");
    callback()
        .map(|v| {
            info!(log_cb(SafetyLogSchema::new(log_entry, LogEvent::Success)));
            counters::increment_query(log_entry.as_str(), "success");
            v
        })
        .map_err(|err| {
            error!(log_cb(SafetyLogSchema::new(log_entry, LogEvent::Error)).error(&err));
            counters::increment_query(log_entry.as_str(), "error");
            err
        })
}
