//! This module tests the validation of commits as defined in
//! https://openmls.tech/book/message_validation.html#commit-message-validation

use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::{signatures::Signer, types::Ciphersuite};
use rstest::*;
use rstest_reuse::{self, *};
use tls_codec::{Deserialize, Serialize};

use super::utils::{
    generate_credential_with_key, generate_key_package, resign_message, CredentialWithKeyAndSigner,
};
use crate::{
    binary_tree::LeafNodeIndex,
    ciphersuite::signable::Signable,
    framing::*,
    group::{config::CryptoConfig, *},
    messages::proposals::*,
    schedule::{ExternalPsk, PreSharedKeyId, Psk},
    treesync::{
        errors::ApplyUpdatePathError, node::parent_node::PlainUpdatePathNode, treekem::UpdatePath,
    },
    versions::ProtocolVersion,
};

struct CommitValidationTestSetup {
    alice_group: MlsGroup,
    alice_credential: CredentialWithKeyAndSigner,
    bob_group: MlsGroup,
    charlie_group: MlsGroup,
}

// Validation test setup
fn validation_test_setup(
    wire_format_policy: WireFormatPolicy,
    ciphersuite: Ciphersuite,
    backend: &impl OpenMlsCryptoProvider,
) -> CommitValidationTestSetup {
    let group_id = GroupId::from_slice(b"Test Group");

    // Generate credentials with keys
    let alice_credential =
        generate_credential_with_key("Alice".into(), ciphersuite.signature_algorithm(), backend);

    let bob_credential =
        generate_credential_with_key("Bob".into(), ciphersuite.signature_algorithm(), backend);

    let charlie_credential =
        generate_credential_with_key("Charlie".into(), ciphersuite.signature_algorithm(), backend);

    // Generate KeyPackages
    let bob_key_package =
        generate_key_package(ciphersuite, Extensions::empty(), backend, bob_credential);

    let charlie_key_package = generate_key_package(
        ciphersuite,
        Extensions::empty(),
        backend,
        charlie_credential,
    );

    // Define the MlsGroup configuration

    let mls_group_config = MlsGroupConfig::builder()
        .wire_format_policy(wire_format_policy)
        .crypto_config(CryptoConfig::with_default_version(ciphersuite))
        .build();

    // === Alice creates a group ===
    let mut alice_group = MlsGroup::new_with_group_id(
        backend,
        &alice_credential.signer,
        &mls_group_config,
        group_id,
        alice_credential.credential_with_key.clone(),
    )
    .expect("An unexpected error occurred.");

    let (_message, welcome, _group_info) = alice_group
        .add_members(
            backend,
            &alice_credential.signer,
            &[bob_key_package, charlie_key_package],
        )
        .expect("error adding Bob to group");

    alice_group
        .merge_pending_commit(backend)
        .expect("error merging pending commit");

    let welcome = welcome.into_welcome().expect("Unexpected message type.");

    let bob_group = MlsGroup::new_from_welcome(
        backend,
        &mls_group_config,
        welcome.clone(),
        Some(alice_group.export_ratchet_tree().into()),
    )
    .expect("error creating group from welcome");

    let charlie_group = MlsGroup::new_from_welcome(
        backend,
        &mls_group_config,
        welcome,
        Some(alice_group.export_ratchet_tree().into()),
    )
    .expect("error creating group from welcome");

    CommitValidationTestSetup {
        alice_group,
        alice_credential,
        bob_group,
        charlie_group,
    }
}

// ValSem200: Commit must not cover inline self Remove proposal
#[apply(ciphersuites_and_backends)]
fn test_valsem200(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        ..
    } = validation_test_setup(PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ciphersuite, backend);

    // Since Alice won't commit to her own removal directly, we have to create
    // proposal and commit independently and then insert the proposal into the
    // commit manually.
    let serialized_proposal_message = alice_group
        .propose_remove_member(
            backend,
            &alice_credential.signer,
            alice_group.own_leaf_index(),
        )
        .expect("error creating commit")
        .tls_serialize_detached()
        .expect("serialization error");

    // Let's get the proposal out of the message.
    let proposal_message =
        MlsMessageIn::tls_deserialize(&mut serialized_proposal_message.as_slice())
            .expect("Could not deserialize message.")
            .into_plaintext()
            .expect("Message was not a plaintext.");

    let proposal = if let FramedContentBody::Proposal(proposal) = proposal_message.content() {
        proposal.clone()
    } else {
        panic!("Unexpected content type.");
    };

    // We have to clear the pending proposals so Alice doesn't try to commit to
    // her own remove.
    alice_group.clear_pending_proposals();

    // Now let's stick it in the commit.
    let serialized_message = alice_group
        .self_update(backend, &alice_credential.signer)
        .expect("Error creating self-update")
        .tls_serialize_detached()
        .expect("Could not serialize message.");

    let mut plaintext = MlsMessageIn::tls_deserialize(&mut serialized_message.as_slice())
        .expect("Could not deserialize message.")
        .into_plaintext()
        .expect("Message was not a plaintext.");

    // Keep the original plaintext for positive test later.
    let original_plaintext = plaintext.clone();

    let mut commit_content = if let FramedContentBody::Commit(commit) = plaintext.content() {
        commit.clone()
    } else {
        panic!("Unexpected content type.");
    };

    commit_content
        .proposals
        .push(ProposalOrRef::Proposal(proposal));

    plaintext.set_content(FramedContentBody::Commit(commit_content));

    let serialized_context = alice_group
        .export_group_context()
        .tls_serialize_detached()
        .expect("error serializing context");

    // We have to re-sign, since we changed the content.
    let tbs: FramedContentTbs = plaintext.into();
    let mut signed_plaintext: AuthenticatedContent = tbs
        .with_context(serialized_context)
        .sign(&alice_credential.signer)
        .expect("Error signing modified payload.");

    // Set old confirmation tag
    signed_plaintext.set_confirmation_tag(
        original_plaintext
            .confirmation_tag()
            .expect("no confirmation tag on original message")
            .clone(),
    );

    let mut signed_plaintext: PublicMessage = signed_plaintext.into();

    let membership_key = alice_group.group().message_secrets().membership_key();

    signed_plaintext
        .set_membership_tag(
            backend,
            membership_key,
            alice_group.group().message_secrets().serialized_context(),
        )
        .expect("error refreshing membership tag");

    // Have Bob try to process the commit.
    let message_in = ProtocolMessage::from(signed_plaintext);

    let err = bob_group
        .process_message(backend, message_in)
        .expect_err("Could process unverified message despite self remove.");

    assert_eq!(
        err,
        ProcessMessageError::InvalidCommit(StageCommitError::AttemptedSelfRemoval)
    );

    // Positive case
    bob_group
        .process_message(backend, ProtocolMessage::from(original_plaintext))
        .expect("Unexpected error.");
}

// ValSem201: Path must be present, if at least one proposal requires a path
#[apply(ciphersuites_and_backends)]
fn test_valsem201(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    let wire_format_policy = PURE_PLAINTEXT_WIRE_FORMAT_POLICY;
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        charlie_group,
        ..
    } = validation_test_setup(wire_format_policy, ciphersuite, backend);

    let queued = |proposal: Proposal| {
        QueuedProposal::from_proposal_and_sender(
            ciphersuite,
            backend,
            proposal,
            &Sender::Member(alice_group.own_leaf_index()),
        )
        .unwrap()
    };

    let add_proposal = || {
        let dave_credential =
            generate_credential_with_key("Dave".into(), ciphersuite.signature_algorithm(), backend);
        let dave_key_package =
            generate_key_package(ciphersuite, Extensions::empty(), backend, dave_credential);

        queued(Proposal::Add(AddProposal {
            key_package: dave_key_package,
        }))
    };

    let psk_proposal = || {
        let secret = Secret::random(ciphersuite, backend, None).unwrap();
        let rand = backend
            .rand()
            .random_vec(ciphersuite.hash_length())
            .unwrap();
        let psk_id = PreSharedKeyId::new(
            ciphersuite,
            backend.rand(),
            Psk::External(ExternalPsk::new(rand)),
        )
        .unwrap();
        psk_id
            .write_to_key_store(backend, ciphersuite, secret.as_slice())
            .unwrap();
        queued(Proposal::PreSharedKey(PreSharedKeyProposal::new(psk_id)))
    };

    let update_proposal = queued(Proposal::Update(UpdateProposal {
        leaf_node: alice_group
            .own_leaf()
            .expect("Unable to get own leaf")
            .clone(),
    }));

    let remove_proposal = || {
        queued(Proposal::Remove(RemoveProposal {
            removed: charlie_group.own_leaf_index(),
        }))
    };

    let gce_proposal = || {
        queued(Proposal::GroupContextExtensions(
            GroupContextExtensionProposal::new(
                alice_group.group().group_context_extensions().clone(),
            ),
        ))
    };

    // ExternalInit Proposal cannot be used alone and has to be in an external commit which
    // always contains a path anyway
    // TODO: #916 when/if AppAck proposal are implemented (path not required)
    // TODO: #751 when ReInit proposal validation are implemented (path not required). Currently one
    // cannot distinguish when the commit has a single ReInit proposal from the commit without proposals
    // in [CoreGroup::apply_proposals()]
    let cases = vec![
        (vec![add_proposal()], false),
        (vec![psk_proposal()], false),
        (vec![update_proposal.clone()], true),
        (vec![remove_proposal()], true),
        (vec![gce_proposal()], true),
        // !path_required + !path_required = !path_required
        (vec![add_proposal(), psk_proposal()], false),
        // path_required + !path_required = path_required
        (vec![remove_proposal(), add_proposal()], true),
        // path_required + path_required = path_required
        (vec![update_proposal, remove_proposal()], true),
        // TODO: #566 this should work if GCE proposals validation were implemented
        // (vec![add_proposal(), gce_proposal()], true),
    ];

    for (proposal, is_path_required) in cases {
        // create a commit containing the proposals
        proposal
            .into_iter()
            .for_each(|p| alice_group.store_pending_proposal(p));

        let params = CreateCommitParams::builder()
            .framing_parameters(alice_group.framing_parameters())
            .proposal_store(&alice_group.proposal_store)
            // has to be turned off otherwise commit path is always present
            .force_self_update(false)
            .build();
        let commit = alice_group
            .group()
            .create_commit(params, backend, &alice_credential.signer)
            .unwrap()
            .commit;

        // verify that path can be omitted in some situations
        if let FramedContentBody::Commit(commit) = commit.content() {
            assert_eq!(commit.has_path(), is_path_required);
        } else {
            panic!()
        };

        let mut commit: PublicMessage = commit.into();
        let membership_key = alice_group.group().message_secrets().membership_key();
        commit
            .set_membership_tag(
                backend,
                membership_key,
                alice_group.group().message_secrets().serialized_context(),
            )
            .unwrap();
        // verify that a path is indeed required when the commit is received
        if is_path_required {
            let commit_wo_path = erase_path(
                backend,
                commit.clone(),
                &alice_group,
                &alice_credential.signer,
            );
            let processed_msg = bob_group.process_message(backend, commit_wo_path);
            assert_eq!(
                processed_msg.unwrap_err(),
                ProcessMessageError::InvalidCommit(StageCommitError::RequiredPathNotFound)
            );
        }

        // Positive case
        let process_message_result = bob_group.process_message(backend, commit);
        assert!(process_message_result.is_ok(), "{process_message_result:?}");

        // cleanup & restore for next iteration
        alice_group.clear_pending_proposals();
        alice_group.clear_pending_commit();
        bob_group.clear_pending_commit();
    }
}

fn erase_path(
    backend: &impl OpenMlsCryptoProvider,
    mut plaintext: PublicMessage,
    alice_group: &MlsGroup,
    alice_signer: &impl Signer,
) -> ProtocolMessage {
    // Keep the original plaintext for positive test later.
    let original_plaintext = plaintext.clone();

    let mut commit_content = if let FramedContentBody::Commit(commit) = plaintext.content() {
        commit.clone()
    } else {
        panic!("Unexpected content type.");
    };
    commit_content.path = None;

    plaintext.set_content(FramedContentBody::Commit(commit_content));

    let plaintext = resign_message(
        alice_group,
        plaintext,
        &original_plaintext,
        backend,
        alice_signer,
    );

    plaintext.into()
}

// ValSem202: Path must be the right length
#[apply(ciphersuites_and_backends)]
fn test_valsem202(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        ..
    } = validation_test_setup(PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ciphersuite, backend);

    // Have Alice generate a self-updating commit, remove a node from the path,
    // re-sign and have Bob process it.

    // Create the self-update
    let serialized_update = alice_group
        .self_update(backend, &alice_credential.signer)
        .expect("Error creating self-update")
        .tls_serialize_detached()
        .expect("Could not serialize message.");

    let mut plaintext = MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
        .expect("Could not deserialize message.")
        .into_plaintext()
        .expect("Message was not a plaintext.");

    // Keep the original plaintext for positive test later.
    let original_plaintext = plaintext.clone();

    let mut commit_content = if let FramedContentBody::Commit(commit) = plaintext.content() {
        commit.clone()
    } else {
        panic!("Unexpected content type.");
    };
    if let Some(ref mut path) = commit_content.path {
        path.pop();
    };

    plaintext.set_content(FramedContentBody::Commit(commit_content));

    let plaintext = resign_message(
        &alice_group,
        plaintext,
        &original_plaintext,
        backend,
        &alice_credential.signer,
    );

    let update_message_in = ProtocolMessage::from(plaintext);

    let err = bob_group
        .process_message(backend, update_message_in)
        .expect_err("Could process unverified message despite path length mismatch.");

    assert_eq!(
        err,
        ProcessMessageError::InvalidCommit(StageCommitError::UpdatePathError(
            ApplyUpdatePathError::PathLengthMismatch
        ))
    );

    let original_update_plaintext =
        MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
            .expect("Could not deserialize message.");

    // Positive case
    bob_group
        .process_message(backend, original_update_plaintext)
        .expect("Unexpected error.");
}

// ValSem203: Path secrets must decrypt correctly
#[apply(ciphersuites_and_backends)]
fn test_valsem203(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        ..
    } = validation_test_setup(PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ciphersuite, backend);

    // Have Alice generate a self-updating commit, scramble some ciphertexts and
    // have Bob process the resulting commit.

    // Create the self-update
    let serialized_update = alice_group
        .self_update(backend, &alice_credential.signer)
        .expect("Error creating self-update")
        .tls_serialize_detached()
        .expect("Could not serialize message.");

    let mut plaintext = MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
        .expect("Could not deserialize message.")
        .into_plaintext()
        .expect("Message was not a plaintext.");

    // Keep the original plaintext for positive test later.
    let original_plaintext = plaintext.clone();

    let mut commit_content = if let FramedContentBody::Commit(commit) = plaintext.content() {
        commit.clone()
    } else {
        panic!("Unexpected content type.");
    };

    // This should cause decryption to fail.
    if let Some(ref mut path) = commit_content.path {
        path.flip_eps_bytes();
    };

    plaintext.set_content(FramedContentBody::Commit(commit_content));

    let plaintext = resign_message(
        &alice_group,
        plaintext,
        &original_plaintext,
        backend,
        &alice_credential.signer,
    );

    let update_message_in = ProtocolMessage::from(plaintext);

    let err = bob_group
        .process_message(backend, update_message_in)
        .expect_err("Could process unverified message despite scrambled ciphertexts.");

    assert_eq!(
        err,
        ProcessMessageError::InvalidCommit(StageCommitError::UpdatePathError(
            ApplyUpdatePathError::UnableToDecrypt
        ))
    );

    let original_update_plaintext =
        MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
            .expect("Could not deserialize message.");

    // Positive case
    bob_group
        .process_message(backend, original_update_plaintext)
        .expect("Unexpected error.");
}

// ValSem204: Public keys from Path must be verified and match the private keys from the direct path
#[apply(ciphersuites_and_backends)]
fn test_valsem204(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        mut charlie_group,
    } = validation_test_setup(PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ciphersuite, backend);

    // Have Alice generate a self-updating commit, flip the last byte of one of
    // the public keys in the path and have Bob process the commit.

    // Create the self-update
    let serialized_update = alice_group
        .self_update(backend, &alice_credential.signer)
        .expect("Error creating self-update")
        .tls_serialize_detached()
        .expect("Could not serialize message.");

    let mut plaintext = MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
        .expect("Could not deserialize message.")
        .into_plaintext()
        .expect("Message was not a plaintext.");

    // Keep the original plaintext for positive test later.
    let original_plaintext = plaintext.clone();

    let mut commit_content = if let FramedContentBody::Commit(commit) = plaintext.content() {
        commit.clone()
    } else {
        panic!("Unexpected content type.");
    };

    // Let's piece together a context that we can use for decryption.
    // Let Charlie process the commit, so we can pull the post-merge tree hash
    // from them.
    let message = charlie_group
        .process_message(backend, original_plaintext.clone())
        .unwrap();
    match message.into_content() {
        ProcessedMessageContent::StagedCommitMessage(staged_commit) => charlie_group
            .merge_staged_commit(backend, *staged_commit)
            .unwrap(),
        _ => panic!("Unexpected message type."),
    }
    let mut encryption_context = alice_group.export_group_context().clone();
    let post_merge_tree_hash = charlie_group.export_group_context().tree_hash().to_vec();
    // We want a context, where everything is post-merge except the confirmed transcript hash.
    encryption_context.increment_epoch();
    encryption_context.update_tree_hash(post_merge_tree_hash);

    // We want to fail the check for public key equality, but we don't want to
    // invalidate the parent hash. So we'll have to encrypt new secrets. The
    // public keys derived from those secrets will then differ from the public
    // keys in the update path, thus causing the error.
    if let Some(ref mut path) = commit_content.path {
        let new_plain_path: Vec<PlainUpdatePathNode> = path
            .nodes()
            .iter()
            .map(|upn| {
                PlainUpdatePathNode::new(
                    upn.encryption_key().clone(),
                    Secret::random(ciphersuite, backend, ProtocolVersion::default())
                        .unwrap()
                        .into(),
                )
            })
            .collect();
        let new_nodes = alice_group
            .group()
            .public_group()
            .encrypt_path(
                backend,
                ciphersuite,
                &new_plain_path,
                &encryption_context.tls_serialize_detached().unwrap(),
                &[].into(),
                LeafNodeIndex::new(0),
            )
            .unwrap();
        let new_path = UpdatePath::new(path.leaf_node().clone(), new_nodes);
        commit_content.path = Some(new_path);
    };

    plaintext.set_content(FramedContentBody::Commit(commit_content));

    let plaintext = resign_message(
        &alice_group,
        plaintext,
        &original_plaintext,
        backend,
        &alice_credential.signer,
    );

    let update_message_in = ProtocolMessage::from(plaintext);

    let err = bob_group
        .process_message(backend, update_message_in)
        .expect_err("Could process unverified message despite modified public key in path.");

    assert_eq!(
        err,
        ProcessMessageError::InvalidCommit(StageCommitError::UpdatePathError(
            ApplyUpdatePathError::PathMismatch
        ))
    );

    let original_update_plaintext =
        MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
            .expect("Could not deserialize message.");

    // Positive case
    bob_group
        .process_message(backend, original_update_plaintext)
        .expect("Unexpected error.");
}

// ValSem205: Confirmation tag must be successfully verified
#[apply(ciphersuites_and_backends)]
fn test_valsem205(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        ..
    } = validation_test_setup(PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ciphersuite, backend);

    // Have Alice generate a self-updating commit, flip the last bit of the
    // confirmation tag and have Bob process the commit.

    // Create the self-update
    let serialized_update = alice_group
        .self_update(backend, &alice_credential.signer)
        .expect("Error creating self-update")
        .tls_serialize_detached()
        .expect("Could not serialize message.");

    let mut plaintext = MlsMessageIn::tls_deserialize(&mut serialized_update.as_slice())
        .expect("Could not deserialize message.")
        .into_plaintext()
        .expect("Message was not a plaintext.");

    // Keep the original plaintext for positive test later.
    let original_plaintext = plaintext.clone();

    let mut new_confirmation_tag = plaintext
        .confirmation_tag()
        .expect("no confirmation tag on commit")
        .clone();

    new_confirmation_tag.0.flip_last_byte();

    plaintext.set_confirmation_tag(Some(new_confirmation_tag));

    // Since the membership tag covers the confirmation tag, we have to refresh it.
    let membership_key = alice_group.group().message_secrets().membership_key();

    plaintext
        .set_membership_tag(
            backend,
            membership_key,
            alice_group.group().message_secrets().serialized_context(),
        )
        .expect("error refreshing membership tag");

    let update_message_in = ProtocolMessage::from(plaintext);

    let err = bob_group
        .process_message(backend, update_message_in)
        .expect_err("Could process unverified message despite confirmation tag mismatch.");

    assert_eq!(
        err,
        ProcessMessageError::InvalidCommit(StageCommitError::ConfirmationTagMismatch)
    );

    // Positive case
    bob_group
        .process_message(backend, ProtocolMessage::from(original_plaintext))
        .expect("Unexpected error.");
}

// this ensures that a member can process commits not containing all the stored proposals
#[apply(ciphersuites_and_backends)]
fn test_partial_proposal_commit(ciphersuite: Ciphersuite, backend: &impl OpenMlsCryptoProvider) {
    // Test with PublicMessage
    let CommitValidationTestSetup {
        mut alice_group,
        alice_credential,
        mut bob_group,
        ..
    } = validation_test_setup(PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ciphersuite, backend);

    let charlie_index = alice_group
        .members()
        .find(|m| m.credential.identity() == b"Charlie")
        .unwrap()
        .index;

    // Create first proposal in Alice's group
    let proposal_1 = alice_group
        .propose_remove_member(backend, &alice_credential.signer, charlie_index)
        .map(|(out, _)| MlsMessageIn::from(out))
        .unwrap();
    let proposal_1 = bob_group.process_message(backend, proposal_1).unwrap();
    match proposal_1.into_content() {
        ProcessedMessageContent::ProposalMessage(p) => bob_group.store_pending_proposal(*p),
        _ => unreachable!(),
    }

    // Create second proposal in Alice's group
    let proposal_2 = alice_group
        .propose_self_update(backend, &alice_credential.signer, None)
        .map(|(out, _)| MlsMessageIn::from(out))
        .unwrap();
    let proposal_2 = bob_group.process_message(backend, proposal_2).unwrap();
    match proposal_2.into_content() {
        ProcessedMessageContent::ProposalMessage(p) => bob_group.store_pending_proposal(*p),
        _ => unreachable!(),
    }

    // Alice creates a commit with only a subset of the epoch's proposals. Bob should still be able to process it.
    let remaining_proposal = alice_group
        .proposal_store
        .proposals()
        .next()
        .cloned()
        .unwrap();
    alice_group.proposal_store.empty();
    alice_group.proposal_store.add(remaining_proposal);
    let (commit, _, _) = alice_group
        .commit_to_pending_proposals(backend, &alice_credential.signer)
        .unwrap();
    // Alice herself should be able to merge the commit
    alice_group
        .merge_pending_commit(backend)
        .expect("Commits with partial proposals are not supported");

    // Bob should be able to process the commit
    bob_group
        .process_message(backend, commit.into_protocol_message().unwrap())
        .expect("Commits with partial proposals are not supported");
    bob_group
        .merge_pending_commit(backend)
        .expect("Commits with partial proposals are not supported");
}
