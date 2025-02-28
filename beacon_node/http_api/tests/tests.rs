use beacon_chain::test_utils::RelativeSyncCommittee;
use beacon_chain::{
    test_utils::{AttestationStrategy, BeaconChainHarness, BlockStrategy, EphemeralHarnessType},
    BeaconChain, ChainConfig, StateSkipConfig, WhenSlotSkipped,
};
use eth2::{
    mixin::{RequestAccept, ResponseForkName, ResponseOptional},
    reqwest::RequestBuilder,
    types::{
        BlockId as CoreBlockId, ForkChoiceNode, ProduceBlockV3Response, StateId as CoreStateId, *,
    },
    BeaconNodeHttpClient, Error,
    Error::ServerMessage,
    StatusCode, Timeouts,
};
use execution_layer::test_utils::{
    MockBuilder, Operation, DEFAULT_BUILDER_PAYLOAD_VALUE_WEI, DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI,
};
use futures::stream::{Stream, StreamExt};
use futures::FutureExt;
use http_api::{
    test_utils::{create_api_server, ApiServer},
    BlockId, StateId,
};
use lighthouse_network::{types::SyncState, Enr, EnrExt, PeerId};
use logging::test_logger;
use network::NetworkReceivers;
use proto_array::ExecutionStatus;
use sensitive_url::SensitiveUrl;
use slot_clock::SlotClock;
use state_processing::per_block_processing::get_expected_withdrawals;
use state_processing::per_slot_processing;
use state_processing::state_advance::partial_state_advance;
use std::convert::TryInto;
use std::sync::Arc;
use tokio::time::Duration;
use tree_hash::TreeHash;
use types::application_domain::ApplicationDomain;
use types::{
    attestation::AttestationBase, AggregateSignature, BitList, Domain, EthSpec, ExecutionBlockHash,
    Hash256, Keypair, MainnetEthSpec, RelativeEpoch, SelectionProof, SignedRoot, Slot,
};

type E = MainnetEthSpec;

const SECONDS_PER_SLOT: u64 = 12;
const SLOTS_PER_EPOCH: u64 = 32;
const VALIDATOR_COUNT: usize = SLOTS_PER_EPOCH as usize;
const CHAIN_LENGTH: u64 = SLOTS_PER_EPOCH * 5 - 1; // Make `next_block` an epoch transition
const JUSTIFIED_EPOCH: u64 = 4;
const FINALIZED_EPOCH: u64 = 3;
const EXTERNAL_ADDR: &str = "/ip4/0.0.0.0/tcp/9000";

/// Skipping the slots around the epoch boundary allows us to check that we're obtaining states
/// from skipped slots for the finalized and justified checkpoints (instead of the state from the
/// block that those roots point to).
const SKIPPED_SLOTS: &[u64] = &[
    JUSTIFIED_EPOCH * SLOTS_PER_EPOCH - 1,
    JUSTIFIED_EPOCH * SLOTS_PER_EPOCH,
    FINALIZED_EPOCH * SLOTS_PER_EPOCH - 1,
    FINALIZED_EPOCH * SLOTS_PER_EPOCH,
];

struct ApiTester {
    ctx: Arc<http_api::Context<EphemeralHarnessType<E>>>,
    harness: Arc<BeaconChainHarness<EphemeralHarnessType<E>>>,
    chain: Arc<BeaconChain<EphemeralHarnessType<E>>>,
    client: BeaconNodeHttpClient,
    next_block: PublishBlockRequest<E>,
    reorg_block: PublishBlockRequest<E>,
    attestations: Vec<Attestation<E>>,
    contribution_and_proofs: Vec<SignedContributionAndProof<E>>,
    attester_slashing: AttesterSlashing<E>,
    proposer_slashing: ProposerSlashing,
    voluntary_exit: SignedVoluntaryExit,
    bls_to_execution_change: SignedBlsToExecutionChange,
    network_rx: NetworkReceivers<E>,
    local_enr: Enr,
    external_peer_id: PeerId,
    mock_builder: Option<Arc<MockBuilder<E>>>,
}

struct ApiTesterConfig {
    spec: ChainSpec,
    retain_historic_states: bool,
}

impl Default for ApiTesterConfig {
    fn default() -> Self {
        let mut spec = E::default_spec();
        spec.shard_committee_period = 2;
        Self {
            spec,
            retain_historic_states: false,
        }
    }
}

impl ApiTesterConfig {
    fn retain_historic_states(mut self) -> Self {
        self.retain_historic_states = true;
        self
    }
}

impl ApiTester {
    pub async fn new() -> Self {
        // This allows for testing voluntary exits without building out a massive chain.
        Self::new_from_config(ApiTesterConfig::default()).await
    }

    pub async fn new_with_hard_forks(altair: bool, bellatrix: bool) -> Self {
        let mut config = ApiTesterConfig::default();
        // Set whether the chain has undergone each hard fork.
        if altair {
            config.spec.altair_fork_epoch = Some(Epoch::new(0));
        }
        if bellatrix {
            config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
        }
        Self::new_from_config(config).await
    }

    pub async fn new_from_config(config: ApiTesterConfig) -> Self {
        let spec = Arc::new(config.spec);

        let mut harness = BeaconChainHarness::builder(MainnetEthSpec)
            .spec(spec.clone())
            .chain_config(ChainConfig {
                reconstruct_historic_states: config.retain_historic_states,
                ..ChainConfig::default()
            })
            .logger(logging::test_logger())
            .deterministic_keypairs(VALIDATOR_COUNT)
            .deterministic_withdrawal_keypairs(VALIDATOR_COUNT)
            .fresh_ephemeral_store()
            .mock_execution_layer_with_config()
            .build();

        harness
            .mock_execution_layer
            .as_ref()
            .unwrap()
            .server
            .execution_block_generator()
            .move_to_terminal_block()
            .unwrap();

        harness.advance_slot();

        for _ in 0..CHAIN_LENGTH {
            let slot = harness.chain.slot().unwrap().as_u64();

            if !SKIPPED_SLOTS.contains(&slot) {
                harness
                    .extend_chain_with_light_client_data(
                        1,
                        BlockStrategy::OnCanonicalHead,
                        AttestationStrategy::AllValidators,
                    )
                    .await;
            }

            harness.advance_slot();
        }

        let head = harness.chain.head_snapshot();

        assert_eq!(
            harness.chain.slot().unwrap(),
            head.beacon_block.slot() + 1,
            "precondition: current slot is one after head"
        );

        let (next_block, _next_state) = harness
            .make_block(head.beacon_state.clone(), harness.chain.slot().unwrap())
            .await;
        let next_block = PublishBlockRequest::from(next_block);

        // `make_block` adds random graffiti, so this will produce an alternate block
        let (reorg_block, _reorg_state) = harness
            .make_block(head.beacon_state.clone(), harness.chain.slot().unwrap() + 1)
            .await;
        let reorg_block = PublishBlockRequest::from(reorg_block);

        let head_state_root = head.beacon_state_root();
        let attestations = harness
            .get_unaggregated_attestations(
                &AttestationStrategy::AllValidators,
                &head.beacon_state,
                head_state_root,
                head.beacon_block_root,
                harness.chain.slot().unwrap(),
            )
            .into_iter()
            .flat_map(|vec| vec.into_iter().map(|(attestation, _subnet_id)| attestation))
            .collect::<Vec<_>>();

        assert!(
            !attestations.is_empty(),
            "precondition: attestations for testing"
        );

        let current_epoch = harness
            .chain
            .slot()
            .expect("should get current slot")
            .epoch(E::slots_per_epoch());
        let is_altair = spec
            .altair_fork_epoch
            .map(|epoch| epoch <= current_epoch)
            .unwrap_or(false);
        let contribution_and_proofs = if is_altair {
            harness
                .make_sync_contributions(
                    &head.beacon_state,
                    head_state_root,
                    harness.chain.slot().unwrap(),
                    RelativeSyncCommittee::Current,
                )
                .into_iter()
                .filter_map(|(_, contribution)| contribution)
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        let attester_slashing = harness.make_attester_slashing(vec![0, 1]);
        let proposer_slashing = harness.make_proposer_slashing(2);
        let voluntary_exit = harness.make_voluntary_exit(3, harness.chain.epoch().unwrap());
        let bls_to_execution_change = harness.make_bls_to_execution_change(4, Address::zero());

        let chain = harness.chain.clone();

        assert_eq!(
            chain
                .canonical_head
                .cached_head()
                .finalized_checkpoint()
                .epoch,
            2,
            "precondition: finality"
        );
        assert_eq!(
            chain
                .canonical_head
                .cached_head()
                .justified_checkpoint()
                .epoch,
            3,
            "precondition: justification"
        );

        let log = test_logger();

        let ApiServer {
            ctx,
            server,
            listening_socket,
            network_rx,
            local_enr,
            external_peer_id,
        } = create_api_server(chain.clone(), &harness.runtime, log).await;

        harness.runtime.task_executor.spawn(server, "api_server");

        // Late-initalize the mock builder now that the mock execution node and beacon API ports
        // have been allocated.
        let beacon_api_port = listening_socket.port();
        let beacon_url =
            SensitiveUrl::parse(format!("http://127.0.0.1:{beacon_api_port}").as_str()).unwrap();
        let mock_builder_server = harness.set_mock_builder(beacon_url.clone());

        // Start the mock builder service prior to building the chain out.
        harness.runtime.task_executor.spawn(
            async move { mock_builder_server.await },
            "mock_builder_server",
        );

        let mock_builder = harness.mock_builder.clone();

        let client = BeaconNodeHttpClient::new(
            beacon_url,
            Timeouts::set_all(Duration::from_secs(SECONDS_PER_SLOT)),
        );

        Self {
            ctx,
            harness: Arc::new(harness),
            chain,
            client,
            next_block,
            reorg_block,
            attestations,
            contribution_and_proofs,
            attester_slashing,
            proposer_slashing,
            voluntary_exit,
            bls_to_execution_change,
            network_rx,
            local_enr,
            external_peer_id,
            mock_builder,
        }
    }

    pub async fn new_from_genesis() -> Self {
        let harness = Arc::new(
            BeaconChainHarness::builder(MainnetEthSpec)
                .default_spec()
                .deterministic_keypairs(VALIDATOR_COUNT)
                .deterministic_withdrawal_keypairs(VALIDATOR_COUNT)
                .fresh_ephemeral_store()
                .build(),
        );

        harness.advance_slot();

        let head = harness.chain.head_snapshot();

        let (next_block, _next_state) = harness
            .make_block(head.beacon_state.clone(), harness.chain.slot().unwrap())
            .await;
        let next_block = PublishBlockRequest::from(next_block);

        // `make_block` adds random graffiti, so this will produce an alternate block
        let (reorg_block, _reorg_state) = harness
            .make_block(head.beacon_state.clone(), harness.chain.slot().unwrap())
            .await;
        let reorg_block = PublishBlockRequest::from(reorg_block);

        let head_state_root = head.beacon_state_root();
        let attestations = harness
            .get_unaggregated_attestations(
                &AttestationStrategy::AllValidators,
                &head.beacon_state,
                head_state_root,
                head.beacon_block_root,
                harness.chain.slot().unwrap(),
            )
            .into_iter()
            .flat_map(|vec| vec.into_iter().map(|(attestation, _subnet_id)| attestation))
            .collect::<Vec<_>>();

        let attester_slashing = harness.make_attester_slashing(vec![0, 1]);
        let proposer_slashing = harness.make_proposer_slashing(2);
        let voluntary_exit = harness.make_voluntary_exit(3, harness.chain.epoch().unwrap());
        let bls_to_execution_change = harness.make_bls_to_execution_change(4, Address::zero());

        let chain = harness.chain.clone();

        let log = test_logger();

        let ApiServer {
            ctx,
            server,
            listening_socket,
            network_rx,
            local_enr,
            external_peer_id,
        } = create_api_server(chain.clone(), &harness.runtime, log).await;

        harness.runtime.task_executor.spawn(server, "api_server");

        let client = BeaconNodeHttpClient::new(
            SensitiveUrl::parse(&format!(
                "http://{}:{}",
                listening_socket.ip(),
                listening_socket.port()
            ))
            .unwrap(),
            Timeouts::set_all(Duration::from_secs(SECONDS_PER_SLOT)),
        );

        Self {
            ctx,
            harness,
            chain,
            client,
            next_block,
            reorg_block,
            attestations,
            contribution_and_proofs: vec![],
            attester_slashing,
            proposer_slashing,
            voluntary_exit,
            bls_to_execution_change,
            network_rx,
            local_enr,
            external_peer_id,
            mock_builder: None,
        }
    }

    fn validator_keypairs(&self) -> &[Keypair] {
        &self.harness.validator_keypairs
    }

    pub async fn new_mev_tester() -> Self {
        let tester = Self::new_with_hard_forks(true, true)
            .await
            .test_post_validator_register_validator()
            .await;
        // Make sure bids always meet the minimum threshold.
        tester.mock_builder.as_ref().unwrap();
        tester
    }

    pub async fn new_mev_tester_default_payload_value() -> Self {
        let mut config = ApiTesterConfig {
            retain_historic_states: false,
            spec: E::default_spec(),
        };
        config.spec.altair_fork_epoch = Some(Epoch::new(0));
        config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
        let tester = Self::new_from_config(config)
            .await
            .test_post_validator_register_validator()
            .await;
        tester
            .mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_BUILDER_PAYLOAD_VALUE_WEI,
            )));
        tester
    }

    fn skip_slots(self, count: u64) -> Self {
        for _ in 0..count {
            self.chain
                .slot_clock
                .set_slot(self.chain.slot().unwrap().as_u64() + 1);
        }

        self
    }

    fn interesting_state_ids(&self) -> Vec<StateId> {
        let mut ids = vec![
            StateId(CoreStateId::Head),
            StateId(CoreStateId::Genesis),
            StateId(CoreStateId::Finalized),
            StateId(CoreStateId::Justified),
            StateId(CoreStateId::Slot(Slot::new(0))),
            StateId(CoreStateId::Slot(Slot::new(32))),
            StateId(CoreStateId::Slot(Slot::from(SKIPPED_SLOTS[0]))),
            StateId(CoreStateId::Slot(Slot::from(SKIPPED_SLOTS[1]))),
            StateId(CoreStateId::Slot(Slot::from(SKIPPED_SLOTS[2]))),
            StateId(CoreStateId::Slot(Slot::from(SKIPPED_SLOTS[3]))),
            StateId(CoreStateId::Root(Hash256::zero())),
        ];
        ids.push(StateId(CoreStateId::Root(
            self.chain.canonical_head.cached_head().head_state_root(),
        )));
        ids
    }

    fn interesting_block_ids(&self) -> Vec<BlockId> {
        let mut ids = vec![
            BlockId(CoreBlockId::Head),
            BlockId(CoreBlockId::Genesis),
            BlockId(CoreBlockId::Finalized),
            BlockId(CoreBlockId::Justified),
            BlockId(CoreBlockId::Slot(Slot::new(0))),
            BlockId(CoreBlockId::Slot(Slot::new(32))),
            BlockId(CoreBlockId::Slot(Slot::from(SKIPPED_SLOTS[0]))),
            BlockId(CoreBlockId::Slot(Slot::from(SKIPPED_SLOTS[1]))),
            BlockId(CoreBlockId::Slot(Slot::from(SKIPPED_SLOTS[2]))),
            BlockId(CoreBlockId::Slot(Slot::from(SKIPPED_SLOTS[3]))),
            BlockId(CoreBlockId::Root(Hash256::zero())),
        ];
        ids.push(BlockId(CoreBlockId::Root(
            self.chain.canonical_head.cached_head().head_block_root(),
        )));
        ids
    }
    pub async fn test_beacon_genesis(self) -> Self {
        let result = self.client.get_beacon_genesis().await.unwrap().data;

        let state = &self.chain.head_snapshot().beacon_state;
        let expected = GenesisData {
            genesis_time: state.genesis_time(),
            genesis_validators_root: state.genesis_validators_root(),
            genesis_fork_version: self.chain.spec.genesis_fork_version,
        };

        assert_eq!(result, expected);

        self
    }

    // finalization tests
    pub async fn test_beacon_states_root_finalized(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let state_root = state_id.root(&self.chain);
            let state = state_id.state(&self.chain);

            // if .root or .state fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_state
            // occurs after the state_root and state calls, and that the state_root and state calls
            // were correct.
            if state_root.is_err() || state.is_err() {
                continue;
            }

            // now that we know the state is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_beacon_states_root(state_id.0)
                .await
                .unwrap()
                .unwrap()
                .finalized
                .unwrap();

            let (state_root, _, _) = state_root.unwrap();
            let (state, _, _) = state.unwrap();
            let state_slot = state.slot();
            let expected = self
                .chain
                .is_finalized_state(&state_root, state_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn test_beacon_states_fork_finalized(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let state_root = state_id.root(&self.chain);
            let state = state_id.state(&self.chain);

            // if .root or .state fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_state
            // occurs after the state_root and state calls, and that the state_root and state calls
            // were correct.
            if state_root.is_err() || state.is_err() {
                continue;
            }

            // now that we know the state is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_beacon_states_fork(state_id.0)
                .await
                .unwrap()
                .unwrap()
                .finalized
                .unwrap();

            let (state_root, _, _) = state_root.unwrap();
            let (state, _, _) = state.unwrap();
            let state_slot = state.slot();
            let expected = self
                .chain
                .is_finalized_state(&state_root, state_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn test_beacon_states_finality_checkpoints_finalized(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let state_root = state_id.root(&self.chain);
            let state = state_id.state(&self.chain);

            // if .root or .state fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_state
            // occurs after the state_root and state calls, and that the state_root and state calls
            // were correct.
            if state_root.is_err() || state.is_err() {
                continue;
            }

            // now that we know the state is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_beacon_states_finality_checkpoints(state_id.0)
                .await
                .unwrap()
                .unwrap()
                .finalized
                .unwrap();

            let (state_root, _, _) = state_root.unwrap();
            let (state, _, _) = state.unwrap();
            let state_slot = state.slot();
            let expected = self
                .chain
                .is_finalized_state(&state_root, state_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn test_beacon_headers_block_id_finalized(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let block_root = block_id.root(&self.chain);
            let block = block_id.full_block(&self.chain).await;

            // if .root or .state fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_state
            // occurs after the state_root and state calls, and that the state_root and state calls
            // were correct.
            if block_root.is_err() || block.is_err() {
                continue;
            }

            // now that we know the block is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_beacon_headers_block_id(block_id.0)
                .await
                .unwrap()
                .unwrap()
                .finalized
                .unwrap();

            let (block_root, _, _) = block_root.unwrap();
            let (block, _, _) = block.unwrap();
            let block_slot = block.slot();
            let expected = self
                .chain
                .is_finalized_block(&block_root, block_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", block_id);
        }

        self
    }

    pub async fn test_beacon_blocks_finalized<E: EthSpec>(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let block_root = block_id.root(&self.chain);
            let block = block_id.full_block(&self.chain).await;

            // if .root or .full_block fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_block
            // occurs after those calls, and that they were correct.
            if block_root.is_err() || block.is_err() {
                continue;
            }

            // now that we know the block is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_beacon_blocks::<MainnetEthSpec>(block_id.0)
                .await
                .unwrap()
                .unwrap()
                .metadata
                .finalized
                .unwrap();

            let (block_root, _, _) = block_root.unwrap();
            let (block, _, _) = block.unwrap();
            let block_slot = block.slot();
            let expected = self
                .chain
                .is_finalized_block(&block_root, block_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", block_id);
        }

        self
    }

    pub async fn test_beacon_blinded_blocks_finalized<E: EthSpec>(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let block_root = block_id.root(&self.chain);
            let block = block_id.full_block(&self.chain).await;

            // if .root or .full_block fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_block
            // occurs after those calls, and that they were correct.
            if block_root.is_err() || block.is_err() {
                continue;
            }

            // now that we know the block is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_beacon_blinded_blocks::<MainnetEthSpec>(block_id.0)
                .await
                .unwrap()
                .unwrap()
                .metadata
                .finalized
                .unwrap();

            let (block_root, _, _) = block_root.unwrap();
            let (block, _, _) = block.unwrap();
            let block_slot = block.slot();
            let expected = self
                .chain
                .is_finalized_block(&block_root, block_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", block_id);
        }

        self
    }

    pub async fn test_debug_beacon_states_finalized(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let state_root = state_id.root(&self.chain);
            let state = state_id.state(&self.chain);

            // if .root or .state fail, skip the test. those would be errors outside the scope
            // of this test, here we're testing the finalized field assuming the call to .is_finalized_state
            // occurs after the state_root and state calls, and that the state_root and state calls
            // were correct.
            if state_root.is_err() || state.is_err() {
                continue;
            }

            // now that we know the state is valid, we can unwrap() everything we need
            let result = self
                .client
                .get_debug_beacon_states::<MainnetEthSpec>(state_id.0)
                .await
                .unwrap()
                .unwrap()
                .metadata
                .finalized
                .unwrap();

            let (state_root, _, _) = state_root.unwrap();
            let (state, _, _) = state.unwrap();
            let state_slot = state.slot();
            let expected = self
                .chain
                .is_finalized_state(&state_root, state_slot)
                .unwrap();

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn test_beacon_states_root(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let result = self
                .client
                .get_beacon_states_root(state_id.0)
                .await
                .unwrap()
                .map(|res| res.data.root);

            let expected = state_id
                .root(&self.chain)
                .ok()
                .map(|(root, _execution_optimistic, _finalized)| root);

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn test_beacon_states_fork(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let result = self
                .client
                .get_beacon_states_fork(state_id.0)
                .await
                .unwrap()
                .map(|res| res.data);

            let expected = state_id.fork(&self.chain).ok();

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn test_beacon_states_finality_checkpoints(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let result = self
                .client
                .get_beacon_states_finality_checkpoints(state_id.0)
                .await
                .unwrap()
                .map(|res| res.data);

            let expected = state_id.state(&self.chain).ok().map(
                |(state, _execution_optimistic, _finalized)| FinalityCheckpointsData {
                    previous_justified: state.previous_justified_checkpoint(),
                    current_justified: state.current_justified_checkpoint(),
                    finalized: state.finalized_checkpoint(),
                },
            );

            assert_eq!(result, expected, "{:?}", state_id);
        }

        self
    }

    pub async fn post_beacon_states_validator_balances_unsupported_media_failure(self) -> Self {
        for state_id in self.interesting_state_ids() {
            for validator_indices in self.interesting_validator_indices() {
                let validator_index_ids = validator_indices
                    .iter()
                    .cloned()
                    .map(|i| ValidatorId::Index(i))
                    .collect::<Vec<ValidatorId>>();

                let unsupported_media_response = self
                    .client
                    .post_beacon_states_validator_balances_with_ssz_header(
                        state_id.0,
                        validator_index_ids,
                    )
                    .await;

                if let Err(unsupported_media_response) = unsupported_media_response {
                    match unsupported_media_response {
                        ServerMessage(error) => {
                            assert_eq!(error.code, 415)
                        }
                        _ => panic!("Should error with unsupported media response"),
                    }
                } else {
                    panic!("Should error with unsupported media response");
                }
            }
        }

        self
    }

    pub async fn test_beacon_states_validator_balances(self) -> Self {
        for state_id in self.interesting_state_ids() {
            for validator_indices in self.interesting_validator_indices() {
                let state_opt = state_id.state(&self.chain).ok();
                let validators: Vec<Validator> = match state_opt.as_ref() {
                    Some((state, _execution_optimistic, _finalized)) => {
                        state.validators().clone().to_vec()
                    }
                    None => vec![],
                };
                let validator_index_ids = validator_indices
                    .iter()
                    .cloned()
                    .map(|i| ValidatorId::Index(i))
                    .collect::<Vec<ValidatorId>>();
                let validator_pubkey_ids = validator_indices
                    .iter()
                    .cloned()
                    .map(|i| {
                        ValidatorId::PublicKey(
                            validators
                                .get(i as usize)
                                .map_or(PublicKeyBytes::empty(), |val| val.pubkey),
                        )
                    })
                    .collect::<Vec<ValidatorId>>();

                let result_index_ids = self
                    .client
                    .get_beacon_states_validator_balances(
                        state_id.0,
                        Some(validator_index_ids.as_slice()),
                    )
                    .await
                    .unwrap()
                    .map(|res| res.data);
                let result_pubkey_ids = self
                    .client
                    .get_beacon_states_validator_balances(
                        state_id.0,
                        Some(validator_pubkey_ids.as_slice()),
                    )
                    .await
                    .unwrap()
                    .map(|res| res.data);
                let result_post_index_ids = self
                    .client
                    .post_beacon_states_validator_balances(state_id.0, validator_index_ids)
                    .await
                    .unwrap()
                    .map(|res| res.data);
                let result_post_pubkey_ids = self
                    .client
                    .post_beacon_states_validator_balances(state_id.0, validator_pubkey_ids)
                    .await
                    .unwrap()
                    .map(|res| res.data);

                let expected = state_opt.map(|(state, _execution_optimistic, _finalized)| {
                    let mut validators = Vec::with_capacity(validator_indices.len());

                    for i in validator_indices {
                        if i < state.balances().len() as u64 {
                            validators.push(ValidatorBalanceData {
                                index: i as u64,
                                balance: *state.balances().get(i as usize).unwrap(),
                            });
                        }
                    }

                    validators
                });

                assert_eq!(result_index_ids, expected, "{:?}", state_id);
                assert_eq!(result_pubkey_ids, expected, "{:?}", state_id);
                assert_eq!(result_post_index_ids, expected, "{:?}", state_id);
                assert_eq!(result_post_pubkey_ids, expected, "{:?}", state_id);
            }
        }

        self
    }

    pub async fn test_beacon_states_validators(self) -> Self {
        for state_id in self.interesting_state_ids() {
            for statuses in self.interesting_validator_statuses() {
                for validator_indices in self.interesting_validator_indices() {
                    let state_opt = state_id
                        .state(&self.chain)
                        .ok()
                        .map(|(state, _execution_optimistic, _finalized)| state);
                    let validators: Vec<Validator> = match state_opt.as_ref() {
                        Some(state) => state.validators().to_vec(),
                        None => vec![],
                    };
                    let validator_index_ids = validator_indices
                        .iter()
                        .cloned()
                        .map(|i| ValidatorId::Index(i))
                        .collect::<Vec<ValidatorId>>();
                    let validator_pubkey_ids = validator_indices
                        .iter()
                        .cloned()
                        .map(|i| {
                            ValidatorId::PublicKey(
                                validators
                                    .get(i as usize)
                                    .map_or(PublicKeyBytes::empty(), |val| val.pubkey),
                            )
                        })
                        .collect::<Vec<ValidatorId>>();

                    let result_index_ids = self
                        .client
                        .get_beacon_states_validators(
                            state_id.0,
                            Some(validator_index_ids.as_slice()),
                            None,
                        )
                        .await
                        .unwrap()
                        .map(|res| res.data);
                    let result_pubkey_ids = self
                        .client
                        .get_beacon_states_validators(
                            state_id.0,
                            Some(validator_pubkey_ids.as_slice()),
                            None,
                        )
                        .await
                        .unwrap()
                        .map(|res| res.data);
                    let post_result_index_ids = self
                        .client
                        .post_beacon_states_validators(state_id.0, Some(validator_index_ids), None)
                        .await
                        .unwrap()
                        .map(|res| res.data);
                    let post_result_pubkey_ids = self
                        .client
                        .post_beacon_states_validators(state_id.0, Some(validator_pubkey_ids), None)
                        .await
                        .unwrap()
                        .map(|res| res.data);

                    let expected = state_opt.map(|state| {
                        let epoch = state.current_epoch();
                        let far_future_epoch = self.chain.spec.far_future_epoch;

                        let mut validators = Vec::with_capacity(validator_indices.len());

                        for i in validator_indices {
                            if i >= state.validators().len() as u64 {
                                continue;
                            }
                            let validator = state.validators().get(i as usize).unwrap().clone();
                            let status = ValidatorStatus::from_validator(
                                &validator,
                                epoch,
                                far_future_epoch,
                            );
                            if statuses.contains(&status)
                                || statuses.is_empty()
                                || statuses.contains(&status.superstatus())
                            {
                                validators.push(ValidatorData {
                                    index: i as u64,
                                    balance: *state.balances().get(i as usize).unwrap(),
                                    status,
                                    validator,
                                });
                            }
                        }

                        validators
                    });

                    assert_eq!(result_index_ids, expected, "{:?}", state_id);
                    assert_eq!(result_pubkey_ids, expected, "{:?}", state_id);
                    assert_eq!(post_result_index_ids, expected, "{:?}", state_id);
                    assert_eq!(post_result_pubkey_ids, expected, "{:?}", state_id);
                }
            }
        }

        self
    }

    pub async fn test_beacon_states_validator_id(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let state_opt = state_id
                .state(&self.chain)
                .ok()
                .map(|(state, _execution_optimistic, _finalized)| state);
            let validators = match state_opt.as_ref() {
                Some(state) => state.validators().to_vec(),
                None => vec![],
            };

            for (i, validator) in validators.into_iter().enumerate() {
                let validator_ids = &[
                    ValidatorId::PublicKey(validator.pubkey),
                    ValidatorId::Index(i as u64),
                ];

                for validator_id in validator_ids {
                    let result = self
                        .client
                        .get_beacon_states_validator_id(state_id.0, validator_id)
                        .await
                        .unwrap()
                        .map(|res| res.data);

                    if result.is_none() && state_opt.is_none() {
                        continue;
                    }

                    let state = state_opt.as_ref().expect("result should be none");

                    let expected = {
                        let epoch = state.current_epoch();
                        let far_future_epoch = self.chain.spec.far_future_epoch;

                        ValidatorData {
                            index: i as u64,
                            balance: *state.balances().get(i).unwrap(),
                            status: ValidatorStatus::from_validator(
                                &validator,
                                epoch,
                                far_future_epoch,
                            ),
                            validator: validator.clone(),
                        }
                    };

                    assert_eq!(result, Some(expected), "{:?}, {:?}", state_id, validator_id);
                }
            }
        }

        self
    }

    pub async fn test_beacon_states_committees(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let mut state_opt = state_id
                .state(&self.chain)
                .ok()
                .map(|(state, _execution_optimistic, _finalized)| state);

            let epoch_opt = state_opt.as_ref().map(|state| state.current_epoch());
            let results = self
                .client
                .get_beacon_states_committees(state_id.0, None, None, epoch_opt)
                .await
                .unwrap()
                .map(|res| res.data);

            if results.is_none() && state_opt.is_none() {
                continue;
            }

            let state = state_opt.as_mut().expect("result should be none");

            state.build_all_committee_caches(&self.chain.spec).unwrap();
            let committees = state
                .get_beacon_committees_at_epoch(RelativeEpoch::Current)
                .unwrap();

            for (i, result) in results.unwrap().into_iter().enumerate() {
                let expected = &committees[i];

                assert_eq!(result.index, expected.index, "{}", state_id);
                assert_eq!(result.slot, expected.slot, "{}", state_id);
                assert_eq!(
                    result
                        .validators
                        .into_iter()
                        .map(|i| i as usize)
                        .collect::<Vec<_>>(),
                    expected.committee.to_vec(),
                    "{}",
                    state_id
                );
            }
        }

        self
    }

    pub async fn test_beacon_states_randao(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let mut state_opt = state_id
                .state(&self.chain)
                .ok()
                .map(|(state, _execution_optimistic, _finalized)| state);

            let epoch_opt = state_opt.as_ref().map(|state| state.current_epoch());
            let result = self
                .client
                .get_beacon_states_randao(state_id.0, epoch_opt)
                .await
                .unwrap()
                .map(|res| res.data);

            if result.is_none() && state_opt.is_none() {
                continue;
            }

            let state = state_opt.as_mut().expect("result should be none");
            let randao_mix = state
                .get_randao_mix(state.slot().epoch(E::slots_per_epoch()))
                .unwrap();

            assert_eq!(result.unwrap().randao, *randao_mix);
        }

        self
    }

    pub async fn test_beacon_headers_all_slots(self) -> Self {
        for slot in 0..CHAIN_LENGTH {
            let slot = Slot::from(slot);

            let result = self
                .client
                .get_beacon_headers(Some(slot), None)
                .await
                .unwrap()
                .map(|res| res.data);

            let root = self
                .chain
                .block_root_at_slot(slot, WhenSlotSkipped::None)
                .unwrap();

            if root.is_none() && result.is_none() {
                continue;
            }

            let root = root.unwrap();
            let block = self
                .chain
                .block_at_slot(slot, WhenSlotSkipped::Prev)
                .unwrap()
                .unwrap();
            let header = BlockHeaderData {
                root,
                canonical: true,
                header: BlockHeaderAndSignature {
                    message: block.message().block_header(),
                    signature: block.signature().clone().into(),
                },
            };
            let expected = vec![header];

            assert_eq!(result.unwrap(), expected, "slot {:?}", slot);
        }

        self
    }

    pub async fn test_beacon_headers_all_parents(self) -> Self {
        let mut roots = self
            .chain
            .forwards_iter_block_roots(Slot::new(0))
            .unwrap()
            .map(Result::unwrap)
            .map(|(root, _slot)| root)
            .collect::<Vec<_>>();

        // The iterator natively returns duplicate roots for skipped slots.
        roots.dedup();

        for i in 1..roots.len() {
            let parent_root = roots[i - 1];
            let child_root = roots[i];

            let result = self
                .client
                .get_beacon_headers(None, Some(parent_root))
                .await
                .unwrap()
                .unwrap()
                .data;

            assert_eq!(result.len(), 1, "i {}", i);
            assert_eq!(result[0].root, child_root, "i {}", i);
        }

        self
    }

    pub async fn test_beacon_headers_block_id(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let result = self
                .client
                .get_beacon_headers_block_id(block_id.0)
                .await
                .unwrap()
                .map(|res| res.data);

            let block_root_opt = block_id
                .root(&self.chain)
                .ok()
                .map(|(root, _execution_optimistic, _finalized)| root);

            if let CoreBlockId::Slot(slot) = block_id.0 {
                if block_root_opt.is_none() {
                    assert!(SKIPPED_SLOTS.contains(&slot.as_u64()));
                } else {
                    assert!(!SKIPPED_SLOTS.contains(&slot.as_u64()));
                }
            }

            let block_opt = block_id
                .full_block(&self.chain)
                .await
                .ok()
                .map(|(block, _execution_optimistic, _finalized)| block);

            if block_opt.is_none() && result.is_none() {
                continue;
            }

            let result = result.unwrap();
            let block = block_opt.unwrap();
            let block_root = block_root_opt.unwrap();
            let canonical = self
                .chain
                .block_root_at_slot(block.slot(), WhenSlotSkipped::None)
                .unwrap()
                .map_or(false, |canonical| block_root == canonical);

            assert_eq!(result.canonical, canonical, "{:?}", block_id);
            assert_eq!(result.root, block_root, "{:?}", block_id);
            assert_eq!(
                result.header.message,
                block.message().block_header(),
                "{:?}",
                block_id
            );
            assert_eq!(
                result.header.signature,
                block.signature().clone().into(),
                "{:?}",
                block_id
            );
        }

        self
    }

    pub async fn test_beacon_blocks_root(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let result = self
                .client
                .get_beacon_blocks_root(block_id.0)
                .await
                .unwrap()
                .map(|res| res.data.root);

            let expected = block_id
                .root(&self.chain)
                .ok()
                .map(|(root, _execution_optimistic, _finalized)| root);
            if let CoreBlockId::Slot(slot) = block_id.0 {
                if expected.is_none() {
                    assert!(SKIPPED_SLOTS.contains(&slot.as_u64()));
                } else {
                    assert!(!SKIPPED_SLOTS.contains(&slot.as_u64()));
                }
            }
            assert_eq!(result, expected, "{:?}", block_id);
        }

        self
    }

    pub async fn test_post_beacon_blocks_valid(mut self) -> Self {
        let next_block = self.next_block.clone();

        self.client.post_beacon_blocks(&next_block).await.unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid blocks should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_blocks_ssz_valid(mut self) -> Self {
        let next_block = &self.next_block;

        self.client
            .post_beacon_blocks_ssz(next_block)
            .await
            .unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid blocks should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_blocks_invalid(mut self) -> Self {
        let block = self
            .harness
            .make_block_with_modifier(
                self.harness.get_current_state(),
                self.harness.get_current_slot(),
                |b| {
                    *b.state_root_mut() = Hash256::zero();
                },
            )
            .await
            .0;

        assert!(self
            .client
            .post_beacon_blocks(&PublishBlockRequest::from(block))
            .await
            .is_err());

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "gossip valid blocks should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_blocks_ssz_invalid(mut self) -> Self {
        let block = self
            .harness
            .make_block_with_modifier(
                self.harness.get_current_state(),
                self.harness.get_current_slot(),
                |b| {
                    *b.state_root_mut() = Hash256::zero();
                },
            )
            .await
            .0;

        assert!(self
            .client
            .post_beacon_blocks_ssz(&PublishBlockRequest::from(block))
            .await
            .is_err());

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "gossip valid blocks should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_blocks_duplicate(self) -> Self {
        let block_contents = self
            .harness
            .make_block(
                self.harness.get_current_state(),
                self.harness.get_current_slot(),
            )
            .await
            .0
            .into();

        assert!(self
            .client
            .post_beacon_blocks(&block_contents)
            .await
            .is_ok());

        // Blinded deneb block contents is just the blinded block
        let blinded_block_contents = block_contents.signed_block().clone_as_blinded();

        // Test all the POST methods in sequence, they should all behave the same.
        let responses = vec![
            self.client
                .post_beacon_blocks(&block_contents)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blocks_v2(&block_contents, None)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blocks_ssz(&block_contents)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blocks_v2_ssz(&block_contents, None)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blinded_blocks(&blinded_block_contents)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blinded_blocks_v2(&blinded_block_contents, None)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blinded_blocks_ssz(&blinded_block_contents)
                .await
                .unwrap_err(),
            self.client
                .post_beacon_blinded_blocks_v2_ssz(&blinded_block_contents, None)
                .await
                .unwrap_err(),
        ];
        for (i, response) in responses.into_iter().enumerate() {
            assert_eq!(
                response.status().unwrap(),
                StatusCode::ACCEPTED,
                "response {i}"
            );
        }

        self
    }

    pub async fn test_beacon_blocks(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let expected = block_id
                .full_block(&self.chain)
                .await
                .ok()
                .map(|(block, _execution_optimistic, _finalized)| block);

            if let CoreBlockId::Slot(slot) = block_id.0 {
                if expected.is_none() {
                    assert!(SKIPPED_SLOTS.contains(&slot.as_u64()));
                } else {
                    assert!(!SKIPPED_SLOTS.contains(&slot.as_u64()));
                }
            }

            // Check the JSON endpoint.
            let json_result = self.client.get_beacon_blocks(block_id.0).await.unwrap();

            if let (Some(json), Some(expected)) = (&json_result, &expected) {
                assert_eq!(&json.data, expected.as_ref(), "{:?}", block_id);
                assert_eq!(
                    json.version,
                    Some(expected.fork_name(&self.chain.spec).unwrap())
                );
            } else {
                assert_eq!(json_result, None);
                assert_eq!(expected, None);
            }

            // Check the SSZ endpoint.
            let ssz_result = self
                .client
                .get_beacon_blocks_ssz(block_id.0, &self.chain.spec)
                .await
                .unwrap();
            assert_eq!(
                ssz_result.as_ref(),
                expected.as_ref().map(|b| b.as_ref()),
                "{:?}",
                block_id
            );

            // Check that the legacy v1 API still works but doesn't return a version field.
            let v1_result = self.client.get_beacon_blocks_v1(block_id.0).await.unwrap();
            if let (Some(v1_result), Some(expected)) = (&v1_result, &expected) {
                assert_eq!(v1_result.version, None);
                assert_eq!(&v1_result.data, expected.as_ref());
            } else {
                assert_eq!(v1_result, None);
                assert_eq!(expected, None);
            }

            // Check that version headers are provided.
            let url = self.client.get_beacon_blocks_path(block_id.0).unwrap();

            let builders: Vec<fn(RequestBuilder) -> RequestBuilder> = vec![
                |b| b,
                |b| b.accept(Accept::Ssz),
                |b| b.accept(Accept::Json),
                |b| b.accept(Accept::Any),
            ];

            for req_builder in builders {
                let raw_res = self
                    .client
                    .get_response(url.clone(), req_builder)
                    .await
                    .optional()
                    .unwrap();
                if let (Some(raw_res), Some(expected)) = (&raw_res, &expected) {
                    assert_eq!(
                        raw_res.fork_name_from_header().unwrap(),
                        Some(expected.fork_name(&self.chain.spec).unwrap())
                    );
                } else {
                    assert!(raw_res.is_none());
                    assert_eq!(expected, None);
                }
            }
        }

        self
    }

    pub async fn test_beacon_blinded_blocks(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let expected = block_id
                .blinded_block(&self.chain)
                .ok()
                .map(|(block, _execution_optimistic, _finalized)| block);

            if let CoreBlockId::Slot(slot) = block_id.0 {
                if expected.is_none() {
                    assert!(SKIPPED_SLOTS.contains(&slot.as_u64()));
                } else {
                    assert!(!SKIPPED_SLOTS.contains(&slot.as_u64()));
                }
            }

            // Check the JSON endpoint.
            let json_result = self
                .client
                .get_beacon_blinded_blocks(block_id.0)
                .await
                .unwrap();

            if let (Some(json), Some(expected)) = (&json_result, &expected) {
                assert_eq!(&json.data, expected, "{:?}", block_id);
                assert_eq!(
                    json.version,
                    Some(expected.fork_name(&self.chain.spec).unwrap())
                );
            } else {
                assert_eq!(json_result, None);
                assert_eq!(expected, None);
            }

            // Check the SSZ endpoint.
            let ssz_result = self
                .client
                .get_beacon_blinded_blocks_ssz(block_id.0, &self.chain.spec)
                .await
                .unwrap();
            assert_eq!(ssz_result.as_ref(), expected.as_ref(), "{:?}", block_id);

            // Check that version headers are provided.
            let url = self
                .client
                .get_beacon_blinded_blocks_path(block_id.0)
                .unwrap();

            let builders: Vec<fn(RequestBuilder) -> RequestBuilder> = vec![
                |b| b,
                |b| b.accept(Accept::Ssz),
                |b| b.accept(Accept::Json),
                |b| b.accept(Accept::Any),
            ];

            for req_builder in builders {
                let raw_res = self
                    .client
                    .get_response(url.clone(), req_builder)
                    .await
                    .optional()
                    .unwrap();
                if let (Some(raw_res), Some(expected)) = (&raw_res, &expected) {
                    assert_eq!(
                        raw_res.fork_name_from_header().unwrap(),
                        Some(expected.fork_name(&self.chain.spec).unwrap())
                    );
                } else {
                    assert!(raw_res.is_none());
                    assert_eq!(expected, None);
                }
            }
        }

        self
    }

    pub async fn test_get_blob_sidecars(self, use_indices: bool) -> Self {
        let block_id = BlockId(CoreBlockId::Finalized);
        let (block_root, _, _) = block_id.root(&self.chain).unwrap();
        let (block, _, _) = block_id.full_block(&self.chain).await.unwrap();
        let num_blobs = block.num_expected_blobs();
        let blob_indices = if use_indices {
            Some(
                (0..num_blobs.saturating_sub(1) as u64)
                    .into_iter()
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let result = match self
            .client
            .get_blobs::<E>(CoreBlockId::Root(block_root), blob_indices.as_deref())
            .await
        {
            Ok(result) => result.unwrap().data,
            Err(e) => panic!("query failed incorrectly: {e:?}"),
        };

        assert_eq!(
            result.len(),
            blob_indices.map_or(num_blobs, |indices| indices.len())
        );
        let expected = block.slot();
        assert_eq!(result.get(0).unwrap().slot(), expected);

        self
    }

    /// Test fetching of blob sidecars that are not available in the database due to pruning.
    ///
    /// If `zero_blobs` is false, test a block with >0 blobs, which should be unavailable.
    /// If `zero_blobs` is true, then test a block with 0 blobs, which should still be available.
    pub async fn test_get_blob_sidecars_pruned(self, zero_blobs: bool) -> Self {
        // Prune all blobs prior to the database's split epoch.
        let store = &self.chain.store;
        let split_epoch = store.get_split_slot().epoch(E::slots_per_epoch());
        let force_prune = true;
        self.chain
            .store
            .try_prune_blobs(force_prune, split_epoch)
            .unwrap();

        let oldest_blob_slot = store.get_blob_info().oldest_blob_slot.unwrap();

        assert_ne!(
            oldest_blob_slot, 0,
            "blob pruning should have pruned some blobs"
        );

        // Find a block with either 0 blobs or 1+ depending on the value of `zero_blobs`.
        let mut test_slot = None;
        for slot in 0..oldest_blob_slot.as_u64() {
            let block_id = BlockId(CoreBlockId::Slot(Slot::new(slot)));
            let (block, _, _) = block_id.blinded_block(&self.chain).unwrap();
            let num_blobs = block.num_expected_blobs();

            if (zero_blobs && num_blobs == 0) || (!zero_blobs && num_blobs > 0) {
                test_slot = Some(Slot::new(slot));
                break;
            }
        }
        let test_slot = test_slot.expect(&format!(
            "should be able to find a block matching zero_blobs={zero_blobs}"
        ));

        match self
            .client
            .get_blobs::<E>(CoreBlockId::Slot(test_slot), None)
            .await
        {
            Ok(result) => {
                if zero_blobs {
                    assert_eq!(
                        &result.unwrap().data[..],
                        &[],
                        "empty blobs are always available"
                    );
                } else {
                    assert_eq!(result, None, "blobs should have been pruned");
                }
            }
            Err(e) => panic!("failed with non-404 status: {e:?}"),
        }

        self
    }

    pub async fn test_get_blob_sidecars_pre_deneb(self) -> Self {
        let oldest_blob_slot = self.chain.store.get_blob_info().oldest_blob_slot.unwrap();
        assert_ne!(
            oldest_blob_slot, 0,
            "oldest_blob_slot should be non-zero and post-Deneb"
        );
        let test_slot = oldest_blob_slot - 1;
        assert!(
            !self
                .chain
                .spec
                .fork_name_at_slot::<E>(test_slot)
                .deneb_enabled(),
            "Deneb should not be enabled at {test_slot}"
        );

        match self
            .client
            .get_blobs::<E>(CoreBlockId::Slot(test_slot), None)
            .await
        {
            Ok(result) => panic!("queries for pre-Deneb slots should fail. got: {result:?}"),
            Err(e) => assert_eq!(e.status().unwrap(), 400),
        }

        self
    }

    pub async fn test_beacon_blocks_attestations(self) -> Self {
        for block_id in self.interesting_block_ids() {
            let result = self
                .client
                .get_beacon_blocks_attestations_v2(block_id.0)
                .await
                .unwrap()
                .map(|res| res.data);

            let expected = block_id.full_block(&self.chain).await.ok().map(
                |(block, _execution_optimistic, _finalized)| {
                    block
                        .message()
                        .body()
                        .attestations()
                        .map(|att| att.clone_as_attestation())
                        .collect::<Vec<_>>()
                        .into()
                },
            );

            if let CoreBlockId::Slot(slot) = block_id.0 {
                if expected.is_none() {
                    assert!(SKIPPED_SLOTS.contains(&slot.as_u64()));
                } else {
                    assert!(!SKIPPED_SLOTS.contains(&slot.as_u64()));
                }
            }

            assert_eq!(result, expected, "{:?}", block_id);
        }

        self
    }

    pub async fn test_post_beacon_pool_attestations_valid_v1(mut self) -> Self {
        self.client
            .post_beacon_pool_attestations_v1(self.attestations.as_slice())
            .await
            .unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid attestation should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_attestations_valid_v2(mut self) -> Self {
        let fork_name = self
            .attestations
            .first()
            .map(|att| self.chain.spec.fork_name_at_slot::<E>(att.data().slot))
            .unwrap();
        self.client
            .post_beacon_pool_attestations_v2(self.attestations.as_slice(), fork_name)
            .await
            .unwrap();
        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid attestation should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_attestations_invalid_v1(mut self) -> Self {
        let mut attestations = Vec::new();
        for attestation in &self.attestations {
            let mut invalid_attestation = attestation.clone();
            invalid_attestation.data_mut().slot += 1;

            // add both to ensure we only fail on invalid attestations
            attestations.push(attestation.clone());
            attestations.push(invalid_attestation);
        }

        let err = self
            .client
            .post_beacon_pool_attestations_v1(attestations.as_slice())
            .await
            .unwrap_err();

        match err {
            Error::ServerIndexedMessage(IndexedErrorMessage {
                code,
                message: _,
                failures,
            }) => {
                assert_eq!(code, 400);
                assert_eq!(failures.len(), self.attestations.len());
            }
            _ => panic!("query did not fail correctly"),
        }

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "if some attestations are valid, we should send them to the network"
        );

        self
    }
    pub async fn test_post_beacon_pool_attestations_invalid_v2(mut self) -> Self {
        let mut attestations = Vec::new();
        for attestation in &self.attestations {
            let mut invalid_attestation = attestation.clone();
            invalid_attestation.data_mut().slot += 1;

            // add both to ensure we only fail on invalid attestations
            attestations.push(attestation.clone());
            attestations.push(invalid_attestation);
        }

        let fork_name = self
            .attestations
            .first()
            .map(|att| self.chain.spec.fork_name_at_slot::<E>(att.data().slot))
            .unwrap();

        let err_v2 = self
            .client
            .post_beacon_pool_attestations_v2(attestations.as_slice(), fork_name)
            .await
            .unwrap_err();

        match err_v2 {
            Error::ServerIndexedMessage(IndexedErrorMessage {
                code,
                message: _,
                failures,
            }) => {
                assert_eq!(code, 400);
                assert_eq!(failures.len(), self.attestations.len());
            }
            _ => panic!("query did not fail correctly"),
        }

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "if some attestations are valid, we should send them to the network"
        );

        self
    }

    pub async fn test_get_beacon_light_client_updates(self) -> Self {
        let current_epoch = self.chain.epoch().unwrap();
        let current_sync_committee_period = current_epoch
            .sync_committee_period(&self.chain.spec)
            .unwrap();

        let result = match self
            .client
            .get_beacon_light_client_updates::<E>(current_sync_committee_period as u64, 1)
            .await
        {
            Ok(result) => result,
            Err(e) => panic!("query failed incorrectly: {e:?}"),
        };

        let expected = self
            .chain
            .light_client_server_cache
            .get_light_client_updates(
                &self.chain.store,
                current_sync_committee_period as u64,
                1,
                &self.chain.spec,
            )
            .unwrap();

        assert_eq!(1, expected.len());
        assert_eq!(result.clone().unwrap().len(), expected.len());
        self
    }

    pub async fn test_get_beacon_light_client_bootstrap(self) -> Self {
        let block_id = BlockId(CoreBlockId::Finalized);
        let (block_root, _, _) = block_id.root(&self.chain).unwrap();

        let result = match self
            .client
            .get_light_client_bootstrap::<E>(block_root)
            .await
        {
            Ok(result) => result,
            Err(e) => panic!("query failed incorrectly: {e:?}"),
        };

        assert!(result.is_some());

        let expected = self
            .chain
            .light_client_server_cache
            .get_light_client_bootstrap(&self.chain.store, &block_root, 1u64, &self.chain.spec);

        assert!(expected.is_ok());

        assert_eq!(result.unwrap().data, expected.unwrap().unwrap().0);

        self
    }

    pub async fn test_get_beacon_light_client_optimistic_update(self) -> Self {
        // get_beacon_light_client_optimistic_update returns Ok(None) on 404 NOT FOUND
        let result = match self
            .client
            .get_beacon_light_client_optimistic_update::<E>()
            .await
        {
            Ok(result) => result.map(|res| res.data),
            Err(e) => panic!("query failed incorrectly: {e:?}"),
        };

        let expected = self
            .chain
            .light_client_server_cache
            .get_latest_optimistic_update();
        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_beacon_light_client_finality_update(self) -> Self {
        let result = match self
            .client
            .get_beacon_light_client_finality_update::<E>()
            .await
        {
            Ok(result) => result.map(|res| res.data),
            Err(e) => panic!("query failed incorrectly: {e:?}"),
        };

        let expected = self
            .chain
            .light_client_server_cache
            .get_latest_finality_update();
        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_beacon_pool_attestations(self) -> Self {
        let result = self
            .client
            .get_beacon_pool_attestations_v1(None, None)
            .await
            .unwrap()
            .data;

        let mut expected = self.chain.op_pool.get_all_attestations();
        expected.extend(self.chain.naive_aggregation_pool.read().iter().cloned());

        assert_eq!(result, expected);

        let result = self
            .client
            .get_beacon_pool_attestations_v2(None, None)
            .await
            .unwrap()
            .data;
        assert_eq!(result, expected);

        self
    }

    pub async fn test_post_beacon_pool_attester_slashings_valid_v1(mut self) -> Self {
        self.client
            .post_beacon_pool_attester_slashings_v1(&self.attester_slashing)
            .await
            .unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid attester slashing should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_attester_slashings_valid_v2(mut self) -> Self {
        let fork_name = self
            .chain
            .spec
            .fork_name_at_slot::<E>(self.attester_slashing.attestation_1().data().slot);
        self.client
            .post_beacon_pool_attester_slashings_v2(&self.attester_slashing, fork_name)
            .await
            .unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid attester slashing should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_attester_slashings_invalid_v1(mut self) -> Self {
        let mut slashing = self.attester_slashing.clone();
        match &mut slashing {
            AttesterSlashing::Base(ref mut slashing) => {
                slashing.attestation_1.data.slot += 1;
            }
            AttesterSlashing::Electra(ref mut slashing) => {
                slashing.attestation_1.data.slot += 1;
            }
        }

        self.client
            .post_beacon_pool_attester_slashings_v1(&slashing)
            .await
            .unwrap_err();

        assert!(
            self.network_rx.network_recv.recv().now_or_never().is_none(),
            "invalid attester slashing should not be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_attester_slashings_invalid_v2(mut self) -> Self {
        let mut slashing = self.attester_slashing.clone();
        match &mut slashing {
            AttesterSlashing::Base(ref mut slashing) => {
                slashing.attestation_1.data.slot += 1;
            }
            AttesterSlashing::Electra(ref mut slashing) => {
                slashing.attestation_1.data.slot += 1;
            }
        }

        let fork_name = self
            .chain
            .spec
            .fork_name_at_slot::<E>(self.attester_slashing.attestation_1().data().slot);
        self.client
            .post_beacon_pool_attester_slashings_v2(&slashing, fork_name)
            .await
            .unwrap_err();

        assert!(
            self.network_rx.network_recv.recv().now_or_never().is_none(),
            "invalid attester slashing should not be sent to network"
        );

        self
    }

    pub async fn test_get_beacon_pool_attester_slashings(self) -> Self {
        let result = self
            .client
            .get_beacon_pool_attester_slashings_v1()
            .await
            .unwrap()
            .data;

        let expected = self.chain.op_pool.get_all_attester_slashings();

        assert_eq!(result, expected);

        let result = self
            .client
            .get_beacon_pool_attester_slashings_v2()
            .await
            .unwrap()
            .data;
        assert_eq!(result, expected);

        self
    }

    pub async fn test_post_beacon_pool_proposer_slashings_valid(mut self) -> Self {
        self.client
            .post_beacon_pool_proposer_slashings(&self.proposer_slashing)
            .await
            .unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid proposer slashing should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_proposer_slashings_invalid(mut self) -> Self {
        let mut slashing = self.proposer_slashing.clone();
        slashing.signed_header_1.message.slot += 1;

        self.client
            .post_beacon_pool_proposer_slashings(&slashing)
            .await
            .unwrap_err();

        assert!(
            self.network_rx.network_recv.recv().now_or_never().is_none(),
            "invalid proposer slashing should not be sent to network"
        );

        self
    }

    pub async fn test_get_beacon_pool_proposer_slashings(self) -> Self {
        let result = self
            .client
            .get_beacon_pool_proposer_slashings()
            .await
            .unwrap()
            .data;

        let expected = self.chain.op_pool.get_all_proposer_slashings();

        assert_eq!(result, expected);

        self
    }

    pub async fn test_post_beacon_pool_voluntary_exits_valid(mut self) -> Self {
        self.client
            .post_beacon_pool_voluntary_exits(&self.voluntary_exit)
            .await
            .unwrap();

        assert!(
            self.network_rx.network_recv.recv().await.is_some(),
            "valid exit should be sent to network"
        );

        self
    }

    pub async fn test_post_beacon_pool_voluntary_exits_invalid(mut self) -> Self {
        let mut exit = self.voluntary_exit.clone();
        exit.message.epoch += 1;

        self.client
            .post_beacon_pool_voluntary_exits(&exit)
            .await
            .unwrap_err();

        assert!(
            self.network_rx.network_recv.recv().now_or_never().is_none(),
            "invalid exit should not be sent to network"
        );

        self
    }

    pub async fn test_get_beacon_pool_voluntary_exits(self) -> Self {
        let result = self
            .client
            .get_beacon_pool_voluntary_exits()
            .await
            .unwrap()
            .data;

        let expected = self.chain.op_pool.get_all_voluntary_exits();

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_config_fork_schedule(self) -> Self {
        let result = self.client.get_config_fork_schedule().await.unwrap().data;

        let expected: Vec<Fork> = ForkName::list_all()
            .into_iter()
            .filter_map(|fork| self.chain.spec.fork_for_name(fork))
            .collect();

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_config_spec(self) -> Self {
        let result = self
            .client
            .get_config_spec::<ConfigAndPresetElectra>()
            .await
            .map(|res| ConfigAndPreset::Electra(res.data))
            .unwrap();
        let expected = ConfigAndPreset::from_chain_spec::<E>(&self.chain.spec, None);

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_config_deposit_contract(self) -> Self {
        let result = self
            .client
            .get_config_deposit_contract()
            .await
            .unwrap()
            .data;

        let expected = DepositContractData {
            address: self.chain.spec.deposit_contract_address,
            chain_id: self.chain.spec.deposit_chain_id,
        };

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_node_version(self) -> Self {
        let result = self.client.get_node_version().await.unwrap().data;

        let expected = VersionData {
            version: lighthouse_version::version_with_platform(),
        };

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_node_syncing(self) -> Self {
        let result = self.client.get_node_syncing().await.unwrap().data;
        let head_slot = self.chain.canonical_head.cached_head().head_slot();
        let sync_distance = self.chain.slot().unwrap() - head_slot;

        let expected = SyncingData {
            is_syncing: false,
            is_optimistic: false,
            // these tests run without the Bellatrix fork enabled
            el_offline: true,
            head_slot,
            sync_distance,
        };

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_node_syncing_stalled(self) -> Self {
        // Set sync status to stalled.
        *self
            .ctx
            .network_globals
            .as_ref()
            .unwrap()
            .sync_state
            .write() = SyncState::Stalled;

        let is_syncing = self
            .client
            .get_node_syncing()
            .await
            .unwrap()
            .data
            .is_syncing;
        assert_eq!(is_syncing, true);

        // Reset sync state.
        *self
            .ctx
            .network_globals
            .as_ref()
            .unwrap()
            .sync_state
            .write() = SyncState::Synced;

        self
    }

    pub async fn test_get_node_identity(self) -> Self {
        let result = self.client.get_node_identity().await.unwrap().data;

        let expected = IdentityData {
            peer_id: self.local_enr.peer_id().to_string(),
            enr: self.local_enr.clone(),
            p2p_addresses: self.local_enr.multiaddr_p2p_tcp(),
            discovery_addresses: self.local_enr.multiaddr_p2p_udp(),
            metadata: eth2::types::MetaData {
                seq_number: 0,
                attnets: "0x0000000000000000".to_string(),
                syncnets: "0x00".to_string(),
            },
        };

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_node_health(self) -> Self {
        let status = self.client.get_node_health().await;
        match status {
            Ok(_) => {
                panic!("should return 503 error status code");
            }
            Err(e) => {
                assert_eq!(e.status().unwrap(), 503);
            }
        }
        self
    }

    pub async fn test_get_node_peers_by_id(self) -> Self {
        let result = self
            .client
            .get_node_peers_by_id(self.external_peer_id.clone())
            .await
            .unwrap()
            .data;

        let expected = PeerData {
            peer_id: self.external_peer_id.to_string(),
            enr: None,
            last_seen_p2p_address: EXTERNAL_ADDR.to_string(),
            state: PeerState::Connected,
            direction: PeerDirection::Inbound,
        };

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_node_peers(self) -> Self {
        let peer_states: Vec<Option<&[PeerState]>> = vec![
            Some(&[PeerState::Connected]),
            Some(&[PeerState::Connecting]),
            Some(&[PeerState::Disconnected]),
            Some(&[PeerState::Disconnecting]),
            None,
            Some(&[PeerState::Connected, PeerState::Connecting]),
        ];
        let peer_dirs: Vec<Option<&[PeerDirection]>> = vec![
            Some(&[PeerDirection::Outbound]),
            Some(&[PeerDirection::Inbound]),
            Some(&[PeerDirection::Inbound, PeerDirection::Outbound]),
            None,
        ];

        for states in peer_states {
            for dirs in peer_dirs.clone() {
                let result = self.client.get_node_peers(states, dirs).await.unwrap();
                let expected_peer = PeerData {
                    peer_id: self.external_peer_id.to_string(),
                    enr: None,
                    last_seen_p2p_address: EXTERNAL_ADDR.to_string(),
                    state: PeerState::Connected,
                    direction: PeerDirection::Inbound,
                };

                let state_match =
                    states.map_or(true, |states| states.contains(&PeerState::Connected));
                let dir_match = dirs.map_or(true, |dirs| dirs.contains(&PeerDirection::Inbound));

                let mut expected_peers = Vec::new();
                if state_match && dir_match {
                    expected_peers.push(expected_peer);
                }

                assert_eq!(
                    result,
                    PeersData {
                        meta: PeersMetaData {
                            count: expected_peers.len() as u64
                        },
                        data: expected_peers,
                    }
                );
            }
        }
        self
    }

    pub async fn test_get_node_peer_count(self) -> Self {
        let result = self.client.get_node_peer_count().await.unwrap().data;
        assert_eq!(
            result,
            PeerCount {
                connected: 1,
                connecting: 0,
                disconnected: 0,
                disconnecting: 0,
            }
        );
        self
    }

    pub async fn test_get_debug_beacon_states(self) -> Self {
        for state_id in self.interesting_state_ids() {
            let result_json = self
                .client
                .get_debug_beacon_states(state_id.0)
                .await
                .unwrap();

            let mut expected = state_id
                .state(&self.chain)
                .ok()
                .map(|(state, _execution_optimistic, _finalized)| state);
            expected.as_mut().map(|state| state.drop_all_caches());

            if let (Some(json), Some(expected)) = (&result_json, &expected) {
                assert_eq!(json.data, *expected, "{:?}", state_id);
                assert_eq!(
                    json.version,
                    Some(expected.fork_name(&self.chain.spec).unwrap())
                );
            } else {
                assert_eq!(result_json, None);
                assert_eq!(expected, None);
            }

            // Check SSZ API.
            let result_ssz = self
                .client
                .get_debug_beacon_states_ssz(state_id.0, &self.chain.spec)
                .await
                .unwrap();
            assert_eq!(result_ssz, expected, "{:?}", state_id);

            // Check that version headers are provided.
            let url = self
                .client
                .get_debug_beacon_states_path(state_id.0)
                .unwrap();

            let builders: Vec<fn(RequestBuilder) -> RequestBuilder> =
                vec![|b| b, |b| b.accept(Accept::Ssz)];

            for req_builder in builders {
                let raw_res = self
                    .client
                    .get_response(url.clone(), req_builder)
                    .await
                    .optional()
                    .unwrap();
                if let (Some(raw_res), Some(expected)) = (&raw_res, &expected) {
                    assert_eq!(
                        raw_res.fork_name_from_header().unwrap(),
                        Some(expected.fork_name(&self.chain.spec).unwrap())
                    );
                } else {
                    assert!(raw_res.is_none());
                    assert_eq!(expected, None);
                }
            }
        }

        self
    }

    pub async fn test_get_debug_beacon_heads(self) -> Self {
        let result = self
            .client
            .get_debug_beacon_heads()
            .await
            .unwrap()
            .data
            .into_iter()
            .map(|head| (head.root, head.slot))
            .collect::<Vec<_>>();

        let expected = self.chain.heads();

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_debug_fork_choice(self) -> Self {
        let result = self.client.get_debug_fork_choice().await.unwrap();

        let beacon_fork_choice = self.chain.canonical_head.fork_choice_read_lock();

        let expected_proto_array = beacon_fork_choice.proto_array().core_proto_array();

        assert_eq!(
            result.justified_checkpoint,
            expected_proto_array.justified_checkpoint
        );
        assert_eq!(
            result.finalized_checkpoint,
            expected_proto_array.finalized_checkpoint
        );

        let expected_fork_choice_nodes: Vec<ForkChoiceNode> = expected_proto_array
            .nodes
            .iter()
            .map(|node| {
                let execution_status = if node.execution_status.is_execution_enabled() {
                    Some(node.execution_status.to_string())
                } else {
                    None
                };
                ForkChoiceNode {
                    slot: node.slot,
                    block_root: node.root,
                    parent_root: node
                        .parent
                        .and_then(|index| expected_proto_array.nodes.get(index))
                        .map(|parent| parent.root),
                    justified_epoch: node.justified_checkpoint.epoch,
                    finalized_epoch: node.finalized_checkpoint.epoch,
                    weight: node.weight,
                    validity: execution_status,
                    execution_block_hash: node
                        .execution_status
                        .block_hash()
                        .map(|block_hash| block_hash.into_root()),
                }
            })
            .collect();

        assert_eq!(result.fork_choice_nodes, expected_fork_choice_nodes);

        // need to drop beacon_fork_choice here, else borrow checker will complain
        // that self cannot be moved out since beacon_fork_choice borrowed self.chain
        // and might still live after self is moved out
        drop(beacon_fork_choice);
        self
    }

    fn validator_count(&self) -> usize {
        self.chain.head_snapshot().beacon_state.validators().len()
    }

    fn interesting_validator_indices(&self) -> Vec<Vec<u64>> {
        let validator_count = self.validator_count() as u64;

        let mut interesting = vec![
            vec![],
            vec![0],
            vec![0, 1],
            vec![0, 1, 3],
            vec![validator_count],
            vec![validator_count, 1],
            vec![validator_count, 1, 3],
            vec![u64::MAX],
            vec![u64::MAX, 1],
            vec![u64::MAX, 1, 3],
        ];

        interesting.push((0..validator_count).collect());

        interesting
    }

    fn interesting_validator_statuses(&self) -> Vec<Vec<ValidatorStatus>> {
        let interesting = vec![
            vec![],
            vec![ValidatorStatus::Active],
            vec![
                ValidatorStatus::PendingInitialized,
                ValidatorStatus::PendingQueued,
                ValidatorStatus::ActiveOngoing,
                ValidatorStatus::ActiveExiting,
                ValidatorStatus::ActiveSlashed,
                ValidatorStatus::ExitedUnslashed,
                ValidatorStatus::ExitedSlashed,
                ValidatorStatus::WithdrawalPossible,
                ValidatorStatus::WithdrawalDone,
                ValidatorStatus::Active,
                ValidatorStatus::Pending,
                ValidatorStatus::Exited,
                ValidatorStatus::Withdrawal,
            ],
        ];
        interesting
    }

    pub async fn test_get_validator_duties_attester(self) -> Self {
        let current_epoch = self.chain.epoch().unwrap().as_u64();

        let half = current_epoch / 2;
        let first = current_epoch - half;
        let last = current_epoch + half;

        for epoch in first..=last {
            for indices in self.interesting_validator_indices() {
                let epoch = Epoch::from(epoch);

                // The endpoint does not allow getting duties past the next epoch.
                if epoch > current_epoch + 1 {
                    assert_eq!(
                        self.client
                            .post_validator_duties_attester(epoch, indices.as_slice())
                            .await
                            .unwrap_err()
                            .status()
                            .map(Into::into),
                        Some(400)
                    );
                    continue;
                }

                let results = self
                    .client
                    .post_validator_duties_attester(epoch, indices.as_slice())
                    .await
                    .unwrap();

                let dependent_root = self
                    .chain
                    .block_root_at_slot(
                        (epoch - 1).start_slot(E::slots_per_epoch()) - 1,
                        WhenSlotSkipped::Prev,
                    )
                    .unwrap()
                    .unwrap_or(self.chain.head_beacon_block_root());

                assert_eq!(results.dependent_root, dependent_root);

                let result_duties = results.data;

                let mut state = self
                    .chain
                    .state_at_slot(
                        epoch.start_slot(E::slots_per_epoch()),
                        StateSkipConfig::WithStateRoots,
                    )
                    .unwrap();
                state
                    .build_committee_cache(RelativeEpoch::Current, &self.chain.spec)
                    .unwrap();

                let expected_len = indices
                    .iter()
                    .filter(|i| **i < state.validators().len() as u64)
                    .count();

                assert_eq!(result_duties.len(), expected_len);

                for (indices_set, &i) in indices.iter().enumerate() {
                    if let Some(duty) = state
                        .get_attestation_duties(i as usize, RelativeEpoch::Current)
                        .unwrap()
                    {
                        let expected = AttesterData {
                            pubkey: state.validators().get(i as usize).unwrap().pubkey,
                            validator_index: i,
                            committees_at_slot: duty.committees_at_slot,
                            committee_index: duty.index,
                            committee_length: duty.committee_len as u64,
                            validator_committee_index: duty.committee_position as u64,
                            slot: duty.slot,
                        };

                        let result = result_duties
                            .iter()
                            .find(|duty| duty.validator_index == i)
                            .unwrap();

                        assert_eq!(
                            *result, expected,
                            "epoch: {}, indices_set: {}",
                            epoch, indices_set
                        );
                    } else {
                        assert!(
                            !result_duties.iter().any(|duty| duty.validator_index == i),
                            "validator index should not exist in response"
                        );
                    }
                }
            }
        }

        self
    }

    pub async fn test_get_validator_duties_proposer(self) -> Self {
        let current_epoch = self.chain.epoch().unwrap();

        for epoch in 0..=self.chain.epoch().unwrap().as_u64() + 1 {
            let epoch = Epoch::from(epoch);

            let dependent_root = self
                .chain
                .block_root_at_slot(
                    epoch.start_slot(E::slots_per_epoch()) - 1,
                    WhenSlotSkipped::Prev,
                )
                .unwrap()
                .unwrap_or(self.chain.head_beacon_block_root());

            // Presently, the beacon chain harness never runs the code that primes the proposer
            // cache. If this changes in the future then we'll need some smarter logic here, but
            // this is succinct and effective for the time being.
            assert!(
                self.chain
                    .beacon_proposer_cache
                    .lock()
                    .get_epoch::<E>(dependent_root, epoch)
                    .is_none(),
                "the proposer cache should miss initially"
            );

            let result = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap();

            // Check that current-epoch requests prime the proposer cache, whilst non-current
            // requests don't.
            if epoch == current_epoch {
                assert!(
                    self.chain
                        .beacon_proposer_cache
                        .lock()
                        .get_epoch::<E>(dependent_root, epoch)
                        .is_some(),
                    "a current-epoch request should prime the proposer cache"
                );
            } else {
                assert!(
                    self.chain
                        .beacon_proposer_cache
                        .lock()
                        .get_epoch::<E>(dependent_root, epoch)
                        .is_none(),
                    "a non-current-epoch request should not prime the proposer cache"
                );
            }

            let mut state = self
                .chain
                .state_at_slot(
                    epoch.start_slot(E::slots_per_epoch()),
                    StateSkipConfig::WithStateRoots,
                )
                .unwrap();

            state
                .build_committee_cache(RelativeEpoch::Current, &self.chain.spec)
                .unwrap();

            let expected_duties = epoch
                .slot_iter(E::slots_per_epoch())
                .map(|slot| {
                    let index = state
                        .get_beacon_proposer_index(slot, &self.chain.spec)
                        .unwrap();
                    let pubkey = state.validators().get(index).unwrap().pubkey;

                    ProposerData {
                        pubkey,
                        validator_index: index as u64,
                        slot,
                    }
                })
                .collect::<Vec<_>>();

            let expected = DutiesResponse {
                data: expected_duties,
                execution_optimistic: Some(false),
                dependent_root,
            };

            assert_eq!(result, expected);

            // If it's the current epoch, check the function with a primed proposer cache.
            if epoch == current_epoch {
                // This is technically a double-check, but it's defensive.
                assert!(
                    self.chain
                        .beacon_proposer_cache
                        .lock()
                        .get_epoch::<E>(dependent_root, epoch)
                        .is_some(),
                    "the request should prime the proposer cache"
                );

                let result = self
                    .client
                    .get_validator_duties_proposer(epoch)
                    .await
                    .unwrap();

                assert_eq!(result, expected);
            }
        }

        // Requests to the epochs after the next epoch should fail.
        self.client
            .get_validator_duties_proposer(current_epoch + 2)
            .await
            .unwrap_err();

        self
    }

    pub async fn test_get_validator_duties_early(self) -> Self {
        let current_epoch = self.chain.epoch().unwrap();
        let next_epoch = current_epoch + 1;
        let current_epoch_start = self
            .chain
            .slot_clock
            .start_of(current_epoch.start_slot(E::slots_per_epoch()))
            .unwrap();

        self.chain.slot_clock.set_current_time(
            current_epoch_start
                - self.chain.spec.maximum_gossip_clock_disparity()
                - Duration::from_millis(1),
        );

        let dependent_root = self
            .chain
            .block_root_at_slot(
                current_epoch.start_slot(E::slots_per_epoch()) - 1,
                WhenSlotSkipped::Prev,
            )
            .unwrap()
            .unwrap_or(self.chain.head_beacon_block_root());

        self.client
            .get_validator_duties_proposer(current_epoch)
            .await
            .expect("should get proposer duties for the next epoch outside of tolerance");

        assert!(
            self.chain
                .beacon_proposer_cache
                .lock()
                .get_epoch::<E>(dependent_root, current_epoch)
                .is_none(),
            "should not prime the proposer cache outside of tolerance"
        );

        assert_eq!(
            self.client
                .post_validator_duties_attester(next_epoch, &[0])
                .await
                .unwrap_err()
                .status()
                .map(Into::into),
            Some(400),
            "should not get attester duties outside of tolerance"
        );

        self.chain.slot_clock.set_current_time(
            current_epoch_start - self.chain.spec.maximum_gossip_clock_disparity(),
        );

        self.client
            .get_validator_duties_proposer(current_epoch)
            .await
            .expect("should get proposer duties within tolerance");

        assert!(
            self.chain
                .beacon_proposer_cache
                .lock()
                .get_epoch::<E>(dependent_root, current_epoch)
                .is_some(),
            "should prime the proposer cache inside the tolerance"
        );

        self.client
            .post_validator_duties_attester(next_epoch, &[0])
            .await
            .expect("should get attester duties within tolerance");

        self
    }

    pub async fn test_block_production(self) -> Self {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() * 3 {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = epoch.signing_root(domain);
                sk.sign(message).into()
            };

            let block = self
                .client
                .get_validator_blocks::<E>(slot, &randao_reveal, None)
                .await
                .unwrap()
                .data
                .deconstruct()
                .0;

            let signed_block = block.sign(&sk, &fork, genesis_validators_root, &self.chain.spec);
            let signed_block_contents =
                PublishBlockRequest::try_from(Arc::new(signed_block.clone())).unwrap();

            self.client
                .post_beacon_blocks(&signed_block_contents)
                .await
                .unwrap();

            assert_eq!(self.chain.head_beacon_block().as_ref(), &signed_block);

            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }

        self
    }

    pub async fn test_block_production_ssz(self) -> Self {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() * 3 {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = epoch.signing_root(domain);
                sk.sign(message).into()
            };

            let block_bytes = self
                .client
                .get_validator_blocks_ssz::<E>(slot, &randao_reveal, None)
                .await
                .unwrap()
                .expect("block bytes");

            let block_contents =
                FullBlockContents::<E>::from_ssz_bytes(&block_bytes, &self.chain.spec)
                    .expect("block contents bytes can be decoded");

            let signed_block_contents =
                block_contents.sign(&sk, &fork, genesis_validators_root, &self.chain.spec);

            self.client
                .post_beacon_blocks_ssz(&signed_block_contents)
                .await
                .unwrap();

            assert_eq!(
                self.chain.head_beacon_block(),
                *signed_block_contents.signed_block()
            );

            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }

        self
    }

    /// Check that the metadata from the headers & JSON response body are consistent, and that the
    /// consensus block value is non-zero.
    fn check_block_v3_metadata(
        metadata: &ProduceBlockV3Metadata,
        response: &JsonProduceBlockV3Response<E>,
    ) {
        // Compare fork name to ForkVersionedResponse rather than metadata consensus_version, which
        // is deserialized to a dummy value.
        assert_eq!(Some(metadata.consensus_version), response.version);
        assert_eq!(ForkName::Base, response.metadata.consensus_version);
        assert_eq!(
            metadata.execution_payload_blinded,
            response.metadata.execution_payload_blinded
        );
        assert_eq!(
            metadata.execution_payload_value,
            response.metadata.execution_payload_value
        );
        assert_eq!(
            metadata.consensus_block_value,
            response.metadata.consensus_block_value
        );
        assert!(!metadata.consensus_block_value.is_zero());
    }

    pub async fn test_block_production_v3_ssz(self) -> Self {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() * 3 {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = epoch.signing_root(domain);
                sk.sign(message).into()
            };

            let (response, metadata) = self
                .client
                .get_validator_blocks_v3_ssz::<E>(slot, &randao_reveal, None, None)
                .await
                .unwrap();

            match response {
                ProduceBlockV3Response::Blinded(blinded_block) => {
                    assert!(metadata.execution_payload_blinded);
                    assert_eq!(
                        metadata.consensus_version,
                        blinded_block.to_ref().fork_name(&self.chain.spec).unwrap()
                    );
                    let signed_blinded_block =
                        blinded_block.sign(&sk, &fork, genesis_validators_root, &self.chain.spec);

                    self.client
                        .post_beacon_blinded_blocks_ssz(&signed_blinded_block)
                        .await
                        .unwrap();

                    let head_block = self.chain.head_beacon_block().clone_as_blinded();
                    assert_eq!(head_block, signed_blinded_block);

                    self.chain.slot_clock.set_slot(slot.as_u64() + 1);
                }
                ProduceBlockV3Response::Full(block_contents) => {
                    assert!(!metadata.execution_payload_blinded);
                    assert_eq!(
                        metadata.consensus_version,
                        block_contents
                            .block()
                            .to_ref()
                            .fork_name(&self.chain.spec)
                            .unwrap()
                    );
                    let signed_block_contents =
                        block_contents.sign(&sk, &fork, genesis_validators_root, &self.chain.spec);

                    self.client
                        .post_beacon_blocks_ssz(&signed_block_contents)
                        .await
                        .unwrap();

                    assert_eq!(
                        self.chain.head_beacon_block(),
                        *signed_block_contents.signed_block()
                    );

                    self.chain.slot_clock.set_slot(slot.as_u64() + 1);
                }
            }
        }

        self
    }

    pub async fn test_block_production_no_verify_randao(self) -> Self {
        for _ in 0..E::slots_per_epoch() {
            let slot = self.chain.slot().unwrap();

            let block = self
                .client
                .get_validator_blocks_modular::<E>(
                    slot,
                    &Signature::infinity().unwrap().into(),
                    None,
                    SkipRandaoVerification::Yes,
                )
                .await
                .unwrap()
                .data
                .deconstruct()
                .0;
            assert_eq!(block.slot(), slot);
            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }

        self
    }

    pub async fn test_block_production_verify_randao_invalid(self) -> Self {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let bad_randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = (epoch + 1).signing_root(domain);
                sk.sign(message).into()
            };

            // Check failure with no `skip_randao_verification` passed.
            self.client
                .get_validator_blocks::<E>(slot, &bad_randao_reveal, None)
                .await
                .unwrap_err();

            // Check failure with `skip_randao_verification` (requires infinity sig).
            self.client
                .get_validator_blocks_modular::<E>(
                    slot,
                    &bad_randao_reveal,
                    None,
                    SkipRandaoVerification::Yes,
                )
                .await
                .unwrap_err();

            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }

        self
    }

    pub async fn test_blinded_block_production(&self) {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() * 3 {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = epoch.signing_root(domain);
                sk.sign(message).into()
            };

            let block = self
                .client
                .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
                .await
                .unwrap()
                .data;

            let signed_block = block.sign(&sk, &fork, genesis_validators_root, &self.chain.spec);

            self.client
                .post_beacon_blinded_blocks(&signed_block)
                .await
                .unwrap();

            let head_block = self
                .client
                .get_beacon_blocks(CoreBlockId::Head)
                .await
                .unwrap()
                .unwrap()
                .data;

            assert_eq!(head_block.clone_as_blinded(), signed_block);

            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }
    }

    pub async fn test_blinded_block_production_ssz(&self) {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() * 3 {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = epoch.signing_root(domain);
                sk.sign(message).into()
            };

            let block_contents_bytes = self
                .client
                .get_validator_blinded_blocks_ssz::<E>(slot, &randao_reveal, None)
                .await
                .unwrap()
                .expect("block bytes");

            let block_contents =
                FullBlockContents::<E>::from_ssz_bytes(&block_contents_bytes, &self.chain.spec)
                    .expect("block contents bytes can be decoded");

            let signed_block_contents =
                block_contents.sign(&sk, &fork, genesis_validators_root, &self.chain.spec);

            self.client
                .post_beacon_blinded_blocks_ssz(
                    &signed_block_contents.signed_block().clone_as_blinded(),
                )
                .await
                .unwrap();

            let head_block = self
                .client
                .get_beacon_blocks(CoreBlockId::Head)
                .await
                .unwrap()
                .unwrap()
                .data;

            let signed_block = signed_block_contents.signed_block();
            assert_eq!(head_block, **signed_block);

            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }
    }

    pub async fn test_blinded_block_production_no_verify_randao(self) -> Self {
        for _ in 0..E::slots_per_epoch() {
            let slot = self.chain.slot().unwrap();

            let blinded_block = self
                .client
                .get_validator_blinded_blocks_modular::<E>(
                    slot,
                    &Signature::infinity().unwrap().into(),
                    None,
                    SkipRandaoVerification::Yes,
                )
                .await
                .unwrap()
                .data;
            assert_eq!(blinded_block.slot(), slot);
            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }

        self
    }

    pub async fn test_blinded_block_production_verify_randao_invalid(self) -> Self {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        for _ in 0..E::slots_per_epoch() {
            let slot = self.chain.slot().unwrap();
            let epoch = self.chain.epoch().unwrap();

            let proposer_pubkey_bytes = self
                .client
                .get_validator_duties_proposer(epoch)
                .await
                .unwrap()
                .data
                .into_iter()
                .find(|duty| duty.slot == slot)
                .map(|duty| duty.pubkey)
                .unwrap();
            let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

            let sk = self
                .validator_keypairs()
                .iter()
                .find(|kp| kp.pk == proposer_pubkey)
                .map(|kp| kp.sk.clone())
                .unwrap();

            let bad_randao_reveal = {
                let domain = self.chain.spec.get_domain(
                    epoch,
                    Domain::Randao,
                    &fork,
                    genesis_validators_root,
                );
                let message = (epoch + 1).signing_root(domain);
                sk.sign(message).into()
            };

            // Check failure with full randao verification enabled.
            self.client
                .get_validator_blinded_blocks::<E>(slot, &bad_randao_reveal, None)
                .await
                .unwrap_err();

            // Check failure with `skip_randao_verification` (requires infinity sig).
            self.client
                .get_validator_blinded_blocks_modular::<E>(
                    slot,
                    &bad_randao_reveal,
                    None,
                    SkipRandaoVerification::Yes,
                )
                .await
                .unwrap_err();

            self.chain.slot_clock.set_slot(slot.as_u64() + 1);
        }

        self
    }

    pub async fn test_get_validator_attestation_data(self) -> Self {
        let mut state = self.chain.head_beacon_state_cloned();
        let slot = state.slot();
        state
            .build_committee_cache(RelativeEpoch::Current, &self.chain.spec)
            .unwrap();

        for index in 0..state.get_committee_count_at_slot(slot).unwrap() {
            let result = self
                .client
                .get_validator_attestation_data(slot, index)
                .await
                .unwrap()
                .data;

            let expected = self
                .chain
                .produce_unaggregated_attestation(slot, index)
                .unwrap()
                .data()
                .clone();

            assert_eq!(result, expected);
        }

        self
    }

    pub async fn test_get_validator_aggregate_attestation(self) -> Self {
        if self
            .chain
            .spec
            .fork_name_at_slot::<E>(self.chain.slot().unwrap())
            .electra_enabled()
        {
            for attestation in self.chain.naive_aggregation_pool.read().iter() {
                let result = self
                    .client
                    .get_validator_aggregate_attestation_v2(
                        attestation.data().slot,
                        attestation.data().tree_hash_root(),
                        attestation.committee_index().expect("committee index"),
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .data;
                let expected = attestation;

                assert_eq!(&result, expected);
            }
        } else {
            let attestation = self
                .chain
                .head_beacon_block()
                .message()
                .body()
                .attestations()
                .next()
                .unwrap()
                .clone_as_attestation();
            let result = self
                .client
                .get_validator_aggregate_attestation_v1(
                    attestation.data().slot,
                    attestation.data().tree_hash_root(),
                )
                .await
                .unwrap()
                .unwrap()
                .data;
            let expected = attestation;

            assert_eq!(result, expected);
        }

        self
    }

    pub async fn get_aggregate(&mut self) -> SignedAggregateAndProof<E> {
        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let mut head = self.chain.head_snapshot().as_ref().clone();
        while head.beacon_state.current_epoch() < epoch {
            per_slot_processing(&mut head.beacon_state, None, &self.chain.spec).unwrap();
        }
        head.beacon_state
            .build_committee_cache(RelativeEpoch::Current, &self.chain.spec)
            .unwrap();

        let committee_len = head.beacon_state.get_committee_count_at_slot(slot).unwrap();
        let fork = head.beacon_state.fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        let duties = self
            .client
            .post_validator_duties_attester(
                epoch,
                (0..self.validator_keypairs().len() as u64)
                    .collect::<Vec<u64>>()
                    .as_slice(),
            )
            .await
            .unwrap()
            .data;

        let (i, kp, duty, proof) = self
            .validator_keypairs()
            .iter()
            .enumerate()
            .find_map(|(i, kp)| {
                let duty = duties[i].clone();

                let proof = SelectionProof::new::<E>(
                    duty.slot,
                    &kp.sk,
                    &fork,
                    genesis_validators_root,
                    &self.chain.spec,
                );

                if proof
                    .is_aggregator(committee_len as usize, &self.chain.spec)
                    .unwrap()
                {
                    Some((i, kp, duty, proof))
                } else {
                    None
                }
            })
            .expect("there is at least one aggregator for this epoch")
            .clone();

        if duty.slot > slot {
            self.chain.slot_clock.set_slot(duty.slot.into());
        }

        let attestation_data = self
            .client
            .get_validator_attestation_data(duty.slot, duty.committee_index)
            .await
            .unwrap()
            .data;

        // TODO(electra) make fork-agnostic
        let mut attestation = Attestation::Base(AttestationBase {
            aggregation_bits: BitList::with_capacity(duty.committee_length as usize).unwrap(),
            data: attestation_data,
            signature: AggregateSignature::infinity(),
        });

        attestation
            .sign(
                &kp.sk,
                duty.validator_committee_index as usize,
                &fork,
                genesis_validators_root,
                &self.chain.spec,
            )
            .unwrap();

        SignedAggregateAndProof::from_aggregate(
            i as u64,
            attestation.to_ref(),
            Some(proof),
            &kp.sk,
            &fork,
            genesis_validators_root,
            &self.chain.spec,
        )
    }

    pub async fn test_get_validator_aggregate_and_proofs_valid_v1(mut self) -> Self {
        let aggregate = self.get_aggregate().await;

        self.client
            .post_validator_aggregate_and_proof_v1::<E>(&[aggregate])
            .await
            .unwrap();

        assert!(self.network_rx.network_recv.recv().await.is_some());

        self
    }

    pub async fn test_get_validator_aggregate_and_proofs_invalid_v1(mut self) -> Self {
        let mut aggregate = self.get_aggregate().await;
        match &mut aggregate {
            SignedAggregateAndProof::Base(ref mut aggregate) => {
                aggregate.message.aggregate.data.slot += 1;
            }
            SignedAggregateAndProof::Electra(ref mut aggregate) => {
                aggregate.message.aggregate.data.slot += 1;
            }
        }

        self.client
            .post_validator_aggregate_and_proof_v1::<E>(&[aggregate.clone()])
            .await
            .unwrap_err();

        assert!(self.network_rx.network_recv.recv().now_or_never().is_none());

        self
    }

    pub async fn test_get_validator_aggregate_and_proofs_valid_v2(mut self) -> Self {
        let aggregate = self.get_aggregate().await;
        let fork_name = self
            .chain
            .spec
            .fork_name_at_slot::<E>(aggregate.message().aggregate().data().slot);
        self.client
            .post_validator_aggregate_and_proof_v2::<E>(&[aggregate], fork_name)
            .await
            .unwrap();

        assert!(self.network_rx.network_recv.recv().await.is_some());

        self
    }

    pub async fn test_get_validator_aggregate_and_proofs_invalid_v2(mut self) -> Self {
        let mut aggregate = self.get_aggregate().await;
        match &mut aggregate {
            SignedAggregateAndProof::Base(ref mut aggregate) => {
                aggregate.message.aggregate.data.slot += 1;
            }
            SignedAggregateAndProof::Electra(ref mut aggregate) => {
                aggregate.message.aggregate.data.slot += 1;
            }
        }

        let fork_name = self
            .chain
            .spec
            .fork_name_at_slot::<E>(aggregate.message().aggregate().data().slot);
        self.client
            .post_validator_aggregate_and_proof_v2::<E>(&[aggregate], fork_name)
            .await
            .unwrap_err();
        assert!(self.network_rx.network_recv.recv().now_or_never().is_none());

        self
    }

    pub async fn test_get_validator_beacon_committee_subscriptions(mut self) -> Self {
        let subscription = BeaconCommitteeSubscription {
            validator_index: 0,
            committee_index: 0,
            committees_at_slot: 1,
            slot: Slot::new(1),
            is_aggregator: true,
        };

        self.client
            .post_validator_beacon_committee_subscriptions(&[subscription])
            .await
            .unwrap();

        self.network_rx
            .validator_subscription_recv
            .recv()
            .now_or_never()
            .unwrap();

        self
    }

    pub async fn test_post_validator_register_validator(self) -> Self {
        let mut registrations = vec![];
        let mut fee_recipients = vec![];

        let genesis_epoch = self.chain.spec.genesis_slot.epoch(E::slots_per_epoch());
        let fork = Fork {
            current_version: self.chain.spec.genesis_fork_version,
            previous_version: self.chain.spec.genesis_fork_version,
            epoch: genesis_epoch,
        };

        let expected_gas_limit = 11_111_111;

        for (val_index, keypair) in self.validator_keypairs().iter().enumerate() {
            let pubkey = keypair.pk.compress();
            let fee_recipient = Address::from_low_u64_be(val_index as u64);

            let data = ValidatorRegistrationData {
                fee_recipient,
                gas_limit: expected_gas_limit,
                timestamp: 0,
                pubkey,
            };

            let domain = self.chain.spec.get_domain(
                genesis_epoch,
                Domain::ApplicationMask(ApplicationDomain::Builder),
                &fork,
                Hash256::zero(),
            );
            let message = data.signing_root(domain);
            let signature = keypair.sk.sign(message);

            let signed = SignedValidatorRegistrationData {
                message: data,
                signature,
            };

            fee_recipients.push(fee_recipient);
            registrations.push(signed);
        }

        self.client
            .post_validator_register_validator(&registrations)
            .await
            .unwrap();

        for (val_index, (_, fee_recipient)) in self
            .chain
            .head_snapshot()
            .beacon_state
            .validators()
            .into_iter()
            .zip(fee_recipients.into_iter())
            .enumerate()
        {
            let actual = self
                .chain
                .execution_layer
                .as_ref()
                .unwrap()
                .get_suggested_fee_recipient(val_index as u64)
                .await;
            assert_eq!(actual, fee_recipient);
        }

        self
    }

    pub async fn test_post_validator_register_validator_slashed(self) -> Self {
        // slash a validator
        self.client
            .post_beacon_pool_attester_slashings_v1(&self.attester_slashing)
            .await
            .unwrap();

        self.harness
            .extend_chain(
                1,
                BlockStrategy::OnCanonicalHead,
                AttestationStrategy::AllValidators,
            )
            .await;

        let mut registrations = vec![];
        let mut fee_recipients = vec![];

        let genesis_epoch = self.chain.spec.genesis_slot.epoch(E::slots_per_epoch());
        let fork = Fork {
            current_version: self.chain.spec.genesis_fork_version,
            previous_version: self.chain.spec.genesis_fork_version,
            epoch: genesis_epoch,
        };

        let expected_gas_limit = 11_111_111;

        for (val_index, keypair) in self.validator_keypairs().iter().enumerate() {
            let pubkey = keypair.pk.compress();
            let fee_recipient = Address::from_low_u64_be(val_index as u64);

            let data = ValidatorRegistrationData {
                fee_recipient,
                gas_limit: expected_gas_limit,
                timestamp: 0,
                pubkey,
            };

            let domain = self.chain.spec.get_domain(
                genesis_epoch,
                Domain::ApplicationMask(ApplicationDomain::Builder),
                &fork,
                Hash256::zero(),
            );
            let message = data.signing_root(domain);
            let signature = keypair.sk.sign(message);

            let signed = SignedValidatorRegistrationData {
                message: data,
                signature,
            };

            fee_recipients.push(fee_recipient);
            registrations.push(signed);
        }

        self.client
            .post_validator_register_validator(&registrations)
            .await
            .unwrap();

        for (val_index, (_, fee_recipient)) in self
            .chain
            .head_snapshot()
            .beacon_state
            .validators()
            .into_iter()
            .zip(fee_recipients.into_iter())
            .enumerate()
        {
            let actual = self
                .chain
                .execution_layer
                .as_ref()
                .unwrap()
                .get_suggested_fee_recipient(val_index as u64)
                .await;
            if val_index == 0 || val_index == 1 {
                assert_eq!(actual, Address::from_low_u64_be(val_index as u64));
            } else {
                assert_eq!(actual, fee_recipient);
            }
        }

        self
    }

    pub async fn test_post_validator_liveness_epoch(self) -> Self {
        let epoch = self.chain.epoch().unwrap();
        let head_state = self.chain.head_beacon_state_cloned();
        let indices = (0..head_state.validators().len())
            .map(|i| i as u64)
            .collect::<Vec<_>>();

        // Construct the expected response
        let expected: Vec<StandardLivenessResponseData> = head_state
            .validators()
            .iter()
            .enumerate()
            .map(|(index, _)| StandardLivenessResponseData {
                index: index as u64,
                is_live: false,
            })
            .collect();

        let result = self
            .client
            .post_validator_liveness_epoch(epoch, &indices)
            .await
            .unwrap()
            .data;

        assert_eq!(result, expected);

        // Attest to the current slot
        self.client
            .post_beacon_pool_attestations_v1(self.attestations.as_slice())
            .await
            .unwrap();

        let result = self
            .client
            .post_validator_liveness_epoch(epoch, &indices)
            .await
            .unwrap()
            .data;

        let committees = head_state
            .get_beacon_committees_at_slot(self.chain.slot().unwrap())
            .unwrap();
        let attesting_validators: Vec<usize> = committees
            .into_iter()
            .flat_map(|committee| committee.committee.iter().cloned())
            .collect();
        // All attesters should now be considered live
        let expected = expected
            .into_iter()
            .map(|mut a| {
                if attesting_validators.contains(&(a.index as usize)) {
                    a.is_live = true;
                }
                a
            })
            .collect::<Vec<_>>();

        assert_eq!(result, expected);

        self
    }

    // Helper function for tests that require a valid RANDAO signature.
    async fn get_test_randao(&self, slot: Slot, epoch: Epoch) -> (u64, SignatureBytes) {
        let fork = self.chain.canonical_head.cached_head().head_fork();
        let genesis_validators_root = self.chain.genesis_validators_root;

        let (proposer_pubkey_bytes, proposer_index) = self
            .client
            .get_validator_duties_proposer(epoch)
            .await
            .unwrap()
            .data
            .into_iter()
            .find(|duty| duty.slot == slot)
            .map(|duty| (duty.pubkey, duty.validator_index))
            .unwrap();
        let proposer_pubkey = (&proposer_pubkey_bytes).try_into().unwrap();

        let sk = self
            .validator_keypairs()
            .iter()
            .find(|kp| kp.pk == proposer_pubkey)
            .map(|kp| kp.sk.clone())
            .unwrap();

        let randao_reveal = {
            let domain =
                self.chain
                    .spec
                    .get_domain(epoch, Domain::Randao, &fork, genesis_validators_root);
            let message = epoch.signing_root(domain);
            sk.sign(message).into()
        };
        (proposer_index, randao_reveal)
    }

    pub async fn test_payload_v3_respects_registration(self) -> Self {
        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: BlindedPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Blinded(payload) => {
                payload.body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);
        assert_eq!(payload.gas_limit(), 11_111_111);

        self
    }

    pub async fn test_payload_v3_zero_builder_boost_factor(self) -> Self {
        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, Some(0))
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: FullPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Full(payload) => {
                payload.block().body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);
        assert_eq!(payload.gas_limit(), 16_384);

        self
    }

    pub async fn test_payload_v3_max_builder_boost_factor(self) -> Self {
        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, Some(u64::MAX))
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: BlindedPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Blinded(payload) => {
                payload.body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);
        assert_eq!(payload.gas_limit(), 11_111_111);

        self
    }

    pub async fn test_payload_respects_registration(self) -> Self {
        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);
        assert_eq!(payload.gas_limit(), 11_111_111);

        // If this cache is empty, it indicates fallback was not used, so the payload came from the
        // mock builder.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());

        self
    }

    pub async fn test_payload_accepts_mutated_gas_limit(self) -> Self {
        // Mutate gas limit.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::GasLimit(30_000_000));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);
        assert_eq!(payload.gas_limit(), 30_000_000);

        // This cache should not be populated because fallback should not have been used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());
        self
    }

    pub async fn test_payload_v3_accepts_mutated_gas_limit(self) -> Self {
        // Mutate gas limit.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::GasLimit(30_000_000));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: BlindedPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Blinded(payload) => {
                payload.body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);
        assert_eq!(payload.gas_limit(), 30_000_000);

        self
    }

    pub async fn test_payload_accepts_changed_fee_recipient(self) -> Self {
        let test_fee_recipient = "0x4242424242424242424242424242424242424242"
            .parse::<Address>()
            .unwrap();

        // Mutate fee recipient.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::FeeRecipient(test_fee_recipient));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        assert_eq!(payload.fee_recipient(), test_fee_recipient);

        // This cache should not be populated because fallback should not have been used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());
        self
    }

    pub async fn test_payload_v3_accepts_changed_fee_recipient(self) -> Self {
        let test_fee_recipient = "0x4242424242424242424242424242424242424242"
            .parse::<Address>()
            .unwrap();

        // Mutate fee recipient.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::FeeRecipient(test_fee_recipient));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: BlindedPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Blinded(payload) => {
                payload.body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        assert_eq!(payload.fee_recipient(), test_fee_recipient);

        self
    }

    pub async fn test_payload_rejects_invalid_parent_hash(self) -> Self {
        let invalid_parent_hash =
            "0x4242424242424242424242424242424242424242424242424242424242424242"
                .parse::<Hash256>()
                .unwrap();

        // Mutate parent hash.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::ParentHash(invalid_parent_hash));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let expected_parent_hash = self
            .chain
            .head_snapshot()
            .beacon_state
            .latest_execution_payload_header()
            .unwrap()
            .block_hash();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        assert_eq!(payload.parent_hash(), expected_parent_hash);

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_payload_v3_rejects_invalid_parent_hash(self) -> Self {
        let invalid_parent_hash =
            "0x4242424242424242424242424242424242424242424242424242424242424242"
                .parse::<Hash256>()
                .unwrap();

        // Mutate parent hash.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::ParentHash(invalid_parent_hash));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let expected_parent_hash = self
            .chain
            .head_snapshot()
            .beacon_state
            .latest_execution_payload_header()
            .unwrap()
            .block_hash();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: FullPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Full(payload) => {
                payload.block().body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a blinded payload"),
        };

        assert_eq!(payload.parent_hash(), expected_parent_hash);

        self
    }

    pub async fn test_payload_rejects_invalid_prev_randao(self) -> Self {
        let invalid_prev_randao =
            "0x4242424242424242424242424242424242424242424242424242424242424242"
                .parse::<Hash256>()
                .unwrap();

        // Mutate prev randao.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::PrevRandao(invalid_prev_randao));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let expected_prev_randao = self
            .chain
            .canonical_head
            .cached_head()
            .head_random()
            .unwrap();
        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        assert_eq!(payload.prev_randao(), expected_prev_randao);

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_payload_v3_rejects_invalid_prev_randao(self) -> Self {
        let invalid_prev_randao =
            "0x4242424242424242424242424242424242424242424242424242424242424242"
                .parse::<Hash256>()
                .unwrap();

        // Mutate prev randao.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::PrevRandao(invalid_prev_randao));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let expected_prev_randao = self
            .chain
            .canonical_head
            .cached_head()
            .head_random()
            .unwrap();
        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: FullPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Full(payload) => {
                payload.block().body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        assert_eq!(payload.prev_randao(), expected_prev_randao);

        self
    }

    pub async fn test_payload_rejects_invalid_block_number(self) -> Self {
        let invalid_block_number = 2;

        // Mutate block number.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::BlockNumber(invalid_block_number));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let expected_block_number = self
            .chain
            .head_snapshot()
            .beacon_state
            .latest_execution_payload_header()
            .unwrap()
            .block_number()
            + 1;

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        assert_eq!(payload.block_number(), expected_block_number);

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_payload_v3_rejects_invalid_block_number(self) -> Self {
        let invalid_block_number = 2;

        // Mutate block number.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::BlockNumber(invalid_block_number));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let expected_block_number = self
            .chain
            .head_snapshot()
            .beacon_state
            .latest_execution_payload_header()
            .unwrap()
            .block_number()
            + 1;

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: FullPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Full(payload) => {
                payload.block().body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        assert_eq!(payload.block_number(), expected_block_number);

        self
    }

    pub async fn test_payload_rejects_invalid_timestamp(self) -> Self {
        let invalid_timestamp = 2;

        // Mutate timestamp.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Timestamp(invalid_timestamp));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let min_expected_timestamp = self
            .chain
            .head_snapshot()
            .beacon_state
            .latest_execution_payload_header()
            .unwrap()
            .timestamp();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        assert!(payload.timestamp() > min_expected_timestamp);

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_payload_v3_rejects_invalid_timestamp(self) -> Self {
        let invalid_timestamp = 2;

        // Mutate timestamp.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Timestamp(invalid_timestamp));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let min_expected_timestamp = self
            .chain
            .head_snapshot()
            .beacon_state
            .latest_execution_payload_header()
            .unwrap()
            .timestamp();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: FullPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Full(payload) => {
                payload.block().body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a blinded payload"),
        };

        assert!(payload.timestamp() > min_expected_timestamp);

        self
    }

    pub async fn test_payload_rejects_invalid_signature(self) -> Self {
        self.mock_builder.as_ref().unwrap().invalid_signatures();

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_payload_v3_rejects_invalid_signature(self) -> Self {
        self.mock_builder.as_ref().unwrap().invalid_signatures();

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        self
    }

    pub async fn test_builder_chain_health_skips(self) -> Self {
        let slot = self.chain.slot().unwrap();

        // Since we are proposing this slot, start the count from the previous slot.
        let prev_slot = slot - Slot::new(1);
        let head_slot = self.chain.canonical_head.cached_head().head_slot();
        let epoch = self.chain.epoch().unwrap();

        // Inclusive here to make sure we advance one slot past the threshold.
        for _ in (prev_slot - head_slot).as_usize()..=self.chain.config.builder_fallback_skips {
            self.harness.advance_slot();
        }

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_builder_v3_chain_health_skips(self) -> Self {
        let slot = self.chain.slot().unwrap();

        // Since we are proposing this slot, start the count from the previous slot.
        let prev_slot = slot - Slot::new(1);
        let head_slot = self.chain.canonical_head.cached_head().head_slot();
        let epoch = self.chain.epoch().unwrap();

        // Inclusive here to make sure we advance one slot past the threshold.
        for _ in (prev_slot - head_slot).as_usize()..=self.chain.config.builder_fallback_skips {
            self.harness.advance_slot();
        }

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        self
    }

    pub async fn test_builder_chain_health_skips_per_epoch(self) -> Self {
        // Fill an epoch with `builder_fallback_skips_per_epoch` skip slots.
        for i in 0..E::slots_per_epoch() {
            if i == 0 || i as usize > self.chain.config.builder_fallback_skips_per_epoch {
                self.harness
                    .extend_chain(
                        1,
                        BlockStrategy::OnCanonicalHead,
                        AttestationStrategy::AllValidators,
                    )
                    .await;
            }
            self.harness.advance_slot();
        }

        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(next_slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // This cache should not be populated because fallback should not have been used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());

        // Without proposing, advance into the next slot, this should make us cross the threshold
        // number of skips, causing us to use the fallback.
        self.harness.advance_slot();
        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(next_slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());

        self
    }

    pub async fn test_builder_v3_chain_health_skips_per_epoch(self) -> Self {
        // Fill an epoch with `builder_fallback_skips_per_epoch` skip slots.
        for i in 0..E::slots_per_epoch() {
            if i == 0 || i as usize > self.chain.config.builder_fallback_skips_per_epoch {
                self.harness
                    .extend_chain(
                        1,
                        BlockStrategy::OnCanonicalHead,
                        AttestationStrategy::AllValidators,
                    )
                    .await;
            }
            self.harness.advance_slot();
        }

        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(next_slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Blinded(_) => (),
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        // Without proposing, advance into the next slot, this should make us cross the threshold
        // number of skips, causing us to use the fallback.
        self.harness.advance_slot();
        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(next_slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        self
    }

    pub async fn test_builder_chain_health_epochs_since_finalization(self) -> Self {
        let skips = E::slots_per_epoch()
            * self.chain.config.builder_fallback_epochs_since_finalization as u64;

        for _ in 0..skips {
            self.harness.advance_slot();
        }

        // Fill the next epoch with blocks, should be enough to justify, not finalize.
        for _ in 0..E::slots_per_epoch() {
            self.harness
                .extend_chain(
                    1,
                    BlockStrategy::OnCanonicalHead,
                    AttestationStrategy::AllValidators,
                )
                .await;
            self.harness.advance_slot();
        }

        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(next_slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());

        // Fill another epoch with blocks, should be enough to finalize. (Sneaky plus 1 because this
        // scenario starts at an epoch boundary).
        for _ in 0..E::slots_per_epoch() + 1 {
            self.harness
                .extend_chain(
                    1,
                    BlockStrategy::OnCanonicalHead,
                    AttestationStrategy::AllValidators,
                )
                .await;
            self.harness.advance_slot();
        }

        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(next_slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // This cache should not be populated because fallback should not have been used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());

        self
    }

    pub async fn test_builder_v3_chain_health_epochs_since_finalization(self) -> Self {
        let skips = E::slots_per_epoch()
            * self.chain.config.builder_fallback_epochs_since_finalization as u64;

        for _ in 0..skips {
            self.harness.advance_slot();
        }

        // Fill the next epoch with blocks, should be enough to justify, not finalize.
        for _ in 0..E::slots_per_epoch() {
            self.harness
                .extend_chain(
                    1,
                    BlockStrategy::OnCanonicalHead,
                    AttestationStrategy::AllValidators,
                )
                .await;
            self.harness.advance_slot();
        }

        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(next_slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        // Fill another epoch with blocks, should be enough to finalize. (Sneaky plus 1 because this
        // scenario starts at an epoch boundary).
        for _ in 0..E::slots_per_epoch() + 1 {
            self.harness
                .extend_chain(
                    1,
                    BlockStrategy::OnCanonicalHead,
                    AttestationStrategy::AllValidators,
                )
                .await;
            self.harness.advance_slot();
        }

        let next_slot = self.chain.slot().unwrap();

        let (_, randao_reveal) = self
            .get_test_randao(next_slot, next_slot.epoch(E::slots_per_epoch()))
            .await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(next_slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Blinded(_) => (),
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        self
    }

    pub async fn test_builder_chain_health_optimistic_head(self) -> Self {
        // Make sure the next payload verification will return optimistic before advancing the chain.
        self.harness.mock_execution_layer.as_ref().map(|el| {
            el.server.all_payloads_syncing(true);
            el
        });
        self.harness
            .extend_chain(
                1,
                BlockStrategy::OnCanonicalHead,
                AttestationStrategy::AllValidators,
            )
            .await;
        self.harness.advance_slot();

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);

        // If this cache is populated, it indicates fallback to the local EE was correctly used.
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());

        self
    }

    pub async fn test_builder_v3_chain_health_optimistic_head(self) -> Self {
        // Make sure the next payload verification will return optimistic before advancing the chain.
        self.harness.mock_execution_layer.as_ref().map(|el| {
            el.server.all_payloads_syncing(true);
            el
        });
        self.harness
            .extend_chain(
                1,
                BlockStrategy::OnCanonicalHead,
                AttestationStrategy::AllValidators,
            )
            .await;
        self.harness.advance_slot();

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (proposer_index, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let payload: FullPayload<E> = match payload_type.data {
            ProduceBlockV3Response::Full(payload) => {
                payload.block().body().execution_payload().unwrap().into()
            }
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        let expected_fee_recipient = Address::from_low_u64_be(proposer_index as u64);
        assert_eq!(payload.fee_recipient(), expected_fee_recipient);

        self
    }

    pub async fn test_builder_payload_chosen_when_more_profitable(self) -> Self {
        // Mutate value.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI + 1,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // The builder's payload should've been chosen, so this cache should not be populated
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());
        self
    }

    pub async fn test_builder_payload_v3_chosen_when_more_profitable(self) -> Self {
        // Mutate value.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI + 1,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Blinded(_) => (),
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        self
    }

    pub async fn test_local_payload_chosen_when_equally_profitable(self) -> Self {
        // Mutate value.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // The local payload should've been chosen, so this cache should be populated
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_local_payload_v3_chosen_when_equally_profitable(self) -> Self {
        // Mutate value.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        self
    }

    pub async fn test_local_payload_chosen_when_more_profitable(self) -> Self {
        // Mutate value.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI - 1,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // The local payload should've been chosen, so this cache should be populated
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_local_payload_v3_chosen_when_more_profitable(self) -> Self {
        // Mutate value.
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI - 1,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();

        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        self
    }

    pub async fn test_builder_works_post_capella(self) -> Self {
        // Ensure builder payload is chosen
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI + 1,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // The builder's payload should've been chosen, so this cache should not be populated
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_none());
        self
    }

    pub async fn test_builder_works_post_deneb(self) -> Self {
        // Ensure builder payload is chosen
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI + 1,
            )));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        let _block_contents = match payload_type.data {
            ProduceBlockV3Response::Blinded(payload) => payload,
            ProduceBlockV3Response::Full(_) => panic!("Expecting a blinded payload"),
        };

        self
    }

    pub async fn test_lighthouse_rejects_invalid_withdrawals_root(self) -> Self {
        // Ensure builder payload *would be* chosen
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI + 1,
            )));
        // Set withdrawals root to something invalid
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::WithdrawalsRoot(Hash256::repeat_byte(0x42)));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let payload: BlindedPayload<E> = self
            .client
            .get_validator_blinded_blocks::<E>(slot, &randao_reveal, None)
            .await
            .unwrap()
            .data
            .body()
            .execution_payload()
            .unwrap()
            .into();

        // The local payload should've been chosen because the builder's was invalid
        assert!(self
            .chain
            .execution_layer
            .as_ref()
            .unwrap()
            .get_payload_by_root(&payload.tree_hash_root())
            .is_some());
        self
    }

    pub async fn test_lighthouse_rejects_invalid_withdrawals_root_v3(self) -> Self {
        // Ensure builder payload *would be* chosen
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::Value(Uint256::from(
                DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI + 1,
            )));
        // Set withdrawals root to something invalid
        self.mock_builder
            .as_ref()
            .unwrap()
            .add_operation(Operation::WithdrawalsRoot(Hash256::repeat_byte(0x42)));

        let slot = self.chain.slot().unwrap();
        let epoch = self.chain.epoch().unwrap();
        let (_, randao_reveal) = self.get_test_randao(slot, epoch).await;

        let (payload_type, metadata) = self
            .client
            .get_validator_blocks_v3::<E>(slot, &randao_reveal, None, None)
            .await
            .unwrap();
        Self::check_block_v3_metadata(&metadata, &payload_type);

        match payload_type.data {
            ProduceBlockV3Response::Full(_) => (),
            ProduceBlockV3Response::Blinded(_) => panic!("Expecting a full payload"),
        };

        self
    }

    #[cfg(target_os = "linux")]
    pub async fn test_get_lighthouse_health(self) -> Self {
        self.client.get_lighthouse_health().await.unwrap();

        self
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn test_get_lighthouse_health(self) -> Self {
        self.client.get_lighthouse_health().await.unwrap_err();

        self
    }

    pub async fn test_get_lighthouse_syncing(self) -> Self {
        self.client.get_lighthouse_syncing().await.unwrap();

        self
    }

    pub async fn test_get_lighthouse_proto_array(self) -> Self {
        self.client.get_lighthouse_proto_array().await.unwrap();

        self
    }

    pub async fn test_get_lighthouse_validator_inclusion_global(self) -> Self {
        let epoch = self.chain.epoch().unwrap() - 1;
        self.client
            .get_lighthouse_validator_inclusion_global(epoch)
            .await
            .unwrap();

        self
    }

    pub async fn test_get_lighthouse_validator_inclusion(self) -> Self {
        let epoch = self.chain.epoch().unwrap() - 1;
        self.client
            .get_lighthouse_validator_inclusion(epoch, ValidatorId::Index(0))
            .await
            .unwrap();

        self
    }

    pub async fn test_get_lighthouse_eth1_syncing(self) -> Self {
        self.client.get_lighthouse_eth1_syncing().await.unwrap();

        self
    }

    pub async fn test_get_lighthouse_eth1_block_cache(self) -> Self {
        let blocks = self.client.get_lighthouse_eth1_block_cache().await.unwrap();

        assert!(blocks.data.is_empty());

        self
    }

    pub async fn test_get_lighthouse_eth1_deposit_cache(self) -> Self {
        let deposits = self
            .client
            .get_lighthouse_eth1_deposit_cache()
            .await
            .unwrap();

        assert!(deposits.data.is_empty());

        self
    }

    pub async fn test_get_lighthouse_staking(self) -> Self {
        let result = self.client.get_lighthouse_staking().await.unwrap();

        assert_eq!(result, self.chain.eth1_chain.is_some());

        self
    }

    pub async fn test_get_lighthouse_database_info(self) -> Self {
        let info = self.client.get_lighthouse_database_info().await.unwrap();

        assert_eq!(info.anchor, self.chain.store.get_anchor_info());
        assert_eq!(info.split, self.chain.store.get_split_info());
        assert_eq!(
            info.schema_version,
            store::metadata::CURRENT_SCHEMA_VERSION.as_u64()
        );

        self
    }

    pub async fn test_post_lighthouse_database_reconstruct(self) -> Self {
        let response = self
            .client
            .post_lighthouse_database_reconstruct()
            .await
            .unwrap();
        assert_eq!(response, "success");
        self
    }

    pub async fn test_post_lighthouse_liveness(self) -> Self {
        let epoch = self.chain.epoch().unwrap();
        let head_state = self.chain.head_beacon_state_cloned();
        let indices = (0..head_state.validators().len())
            .map(|i| i as u64)
            .collect::<Vec<_>>();

        // Construct the expected response
        let expected: Vec<LivenessResponseData> = head_state
            .validators()
            .iter()
            .enumerate()
            .map(|(index, _)| LivenessResponseData {
                index: index as u64,
                is_live: false,
                epoch,
            })
            .collect();

        let result = self
            .client
            .post_lighthouse_liveness(indices.as_slice(), epoch)
            .await
            .unwrap()
            .data;

        assert_eq!(result, expected);

        // Attest to the current slot
        self.client
            .post_beacon_pool_attestations_v1(self.attestations.as_slice())
            .await
            .unwrap();

        let result = self
            .client
            .post_lighthouse_liveness(indices.as_slice(), epoch)
            .await
            .unwrap()
            .data;

        let committees = head_state
            .get_beacon_committees_at_slot(self.chain.slot().unwrap())
            .unwrap();
        let attesting_validators: Vec<usize> = committees
            .into_iter()
            .flat_map(|committee| committee.committee.iter().cloned())
            .collect();
        // All attesters should now be considered live
        let expected = expected
            .into_iter()
            .map(|mut a| {
                if attesting_validators.contains(&(a.index as usize)) {
                    a.is_live = true;
                }
                a
            })
            .collect::<Vec<_>>();

        assert_eq!(result, expected);

        self
    }

    pub async fn test_get_events(self) -> Self {
        // Subscribe to all events
        let topics = vec![
            EventTopic::Attestation,
            EventTopic::VoluntaryExit,
            EventTopic::Block,
            EventTopic::BlockGossip,
            EventTopic::Head,
            EventTopic::FinalizedCheckpoint,
            EventTopic::AttesterSlashing,
            EventTopic::ProposerSlashing,
            EventTopic::BlsToExecutionChange,
        ];
        let mut events_future = self
            .client
            .get_events::<E>(topics.as_slice())
            .await
            .unwrap();

        let expected_attestation_len = self.attestations.len();

        self.client
            .post_beacon_pool_attestations_v1(self.attestations.as_slice())
            .await
            .unwrap();

        let attestation_events = poll_events(
            &mut events_future,
            expected_attestation_len,
            Duration::from_millis(10000),
        )
        .await;
        assert_eq!(
            attestation_events.as_slice(),
            self.attestations
                .clone()
                .into_iter()
                .map(|attestation| EventKind::Attestation(Box::new(attestation)))
                .collect::<Vec<_>>()
                .as_slice()
        );

        // Produce a voluntary exit event
        self.client
            .post_beacon_pool_voluntary_exits(&self.voluntary_exit)
            .await
            .unwrap();

        let exit_events = poll_events(&mut events_future, 1, Duration::from_millis(10000)).await;
        assert_eq!(
            exit_events.as_slice(),
            &[EventKind::VoluntaryExit(self.voluntary_exit.clone())]
        );

        // Produce a BLS to execution change event
        self.client
            .post_beacon_pool_bls_to_execution_changes(&[self.bls_to_execution_change.clone()])
            .await
            .unwrap();

        let bls_events = poll_events(&mut events_future, 1, Duration::from_millis(10000)).await;
        assert_eq!(
            bls_events.as_slice(),
            &[EventKind::BlsToExecutionChange(Box::new(
                self.bls_to_execution_change.clone()
            ))]
        );

        // Submit the next block, which is on an epoch boundary, so this will produce a finalized
        // checkpoint event, head event, and block event
        let block_root = self.next_block.signed_block().canonical_root();

        // current_duty_dependent_root = block root because this is the first slot of the epoch
        let current_duty_dependent_root = self.chain.head_beacon_block_root();
        let current_slot = self.chain.slot().unwrap();
        let next_slot = self.next_block.signed_block().slot();
        let finalization_distance = E::slots_per_epoch() * 2;

        let expected_block = EventKind::Block(SseBlock {
            block: block_root,
            slot: next_slot,
            execution_optimistic: false,
        });

        let expected_head = EventKind::Head(SseHead {
            block: block_root,
            slot: next_slot,
            state: self.next_block.signed_block().state_root(),
            current_duty_dependent_root,
            previous_duty_dependent_root: self
                .chain
                .block_root_at_slot(current_slot - E::slots_per_epoch(), WhenSlotSkipped::Prev)
                .unwrap()
                .unwrap(),
            epoch_transition: true,
            execution_optimistic: false,
        });

        let finalized_block_root = self
            .chain
            .block_root_at_slot(next_slot - finalization_distance, WhenSlotSkipped::Prev)
            .unwrap()
            .unwrap();
        let finalized_block = self
            .chain
            .get_blinded_block(&finalized_block_root)
            .unwrap()
            .unwrap();
        let finalized_state_root = finalized_block.state_root();

        let expected_finalized = EventKind::FinalizedCheckpoint(SseFinalizedCheckpoint {
            block: finalized_block_root,
            state: finalized_state_root,
            epoch: Epoch::new(3),
            execution_optimistic: false,
        });

        self.client
            .post_beacon_blocks(&self.next_block)
            .await
            .unwrap();

        let expected_gossip = EventKind::BlockGossip(Box::new(BlockGossip {
            slot: next_slot,
            block: block_root,
        }));

        let block_events = poll_events(&mut events_future, 4, Duration::from_millis(10000)).await;
        assert_eq!(
            block_events.as_slice(),
            &[
                expected_gossip,
                expected_block,
                expected_head,
                expected_finalized
            ]
        );

        // Test a reorg event
        let mut chain_reorg_event_future = self
            .client
            .get_events::<E>(&[EventTopic::ChainReorg])
            .await
            .unwrap();

        let expected_reorg = EventKind::ChainReorg(SseChainReorg {
            slot: self.reorg_block.signed_block().slot(),
            depth: 1,
            old_head_block: self.next_block.signed_block().canonical_root(),
            old_head_state: self.next_block.signed_block().state_root(),
            new_head_block: self.reorg_block.signed_block().canonical_root(),
            new_head_state: self.reorg_block.signed_block().state_root(),
            epoch: self
                .next_block
                .signed_block()
                .slot()
                .epoch(E::slots_per_epoch()),
            execution_optimistic: false,
        });

        self.harness.advance_slot();

        self.client
            .post_beacon_blocks(&self.reorg_block)
            .await
            .unwrap();

        let reorg_event = poll_events(
            &mut chain_reorg_event_future,
            1,
            Duration::from_millis(10000),
        )
        .await;
        assert_eq!(reorg_event.as_slice(), &[expected_reorg]);

        // Test attester slashing event
        let mut attester_slashing_event_future = self
            .client
            .get_events::<E>(&[EventTopic::AttesterSlashing])
            .await
            .unwrap();

        self.harness.add_attester_slashing(vec![1, 2, 3]).unwrap();

        let attester_slashing_event = poll_events(
            &mut attester_slashing_event_future,
            1,
            Duration::from_millis(10000),
        )
        .await;

        assert!(attester_slashing_event.len() == 1);

        // Test proposer slashing event
        let mut proposer_slashing_event_future = self
            .client
            .get_events::<E>(&[EventTopic::ProposerSlashing])
            .await
            .unwrap();

        self.harness.add_proposer_slashing(1).unwrap();

        let proposer_slashing_event = poll_events(
            &mut proposer_slashing_event_future,
            1,
            Duration::from_millis(10000),
        )
        .await;

        assert!(proposer_slashing_event.len() == 1);

        self
    }

    pub async fn test_get_expected_withdrawals_invalid_state(self) -> Self {
        let state_id = CoreStateId::Root(Hash256::zero());

        let result = self.client.get_expected_withdrawals(&state_id).await;

        match result {
            Err(e) => {
                assert_eq!(e.status().unwrap(), 404);
            }
            _ => panic!("query did not fail correctly"),
        }

        self
    }

    pub async fn test_get_expected_withdrawals_capella(self) -> Self {
        let slot = self.chain.slot().unwrap();
        let state_id = CoreStateId::Slot(slot);

        // calculate the expected withdrawals
        let (mut state, _, _) = StateId(state_id).state(&self.chain).unwrap();
        let proposal_slot = state.slot() + 1;
        let proposal_epoch = proposal_slot.epoch(E::slots_per_epoch());
        let (state_root, _, _) = StateId(state_id).root(&self.chain).unwrap();
        if proposal_epoch != state.current_epoch() {
            let _ = partial_state_advance(
                &mut state,
                Some(state_root),
                proposal_slot,
                &self.chain.spec,
            );
        }
        let expected_withdrawals = get_expected_withdrawals(&state, &self.chain.spec)
            .unwrap()
            .0;

        // fetch expected withdrawals from the client
        let result = self.client.get_expected_withdrawals(&state_id).await;
        match result {
            Ok(withdrawal_response) => {
                assert_eq!(withdrawal_response.execution_optimistic, Some(false));
                assert_eq!(withdrawal_response.finalized, Some(false));
                assert_eq!(withdrawal_response.data, expected_withdrawals.to_vec());
            }
            Err(_) => {
                panic!("query failed incorrectly");
            }
        }

        self
    }

    pub async fn test_get_expected_withdrawals_pre_capella(self) -> Self {
        let state_id = CoreStateId::Head;

        let result = self.client.get_expected_withdrawals(&state_id).await;

        match result {
            Err(e) => {
                assert_eq!(e.status().unwrap(), 400);
            }
            _ => panic!("query did not fail correctly"),
        }

        self
    }

    pub async fn test_get_events_altair(self) -> Self {
        let topics = vec![EventTopic::ContributionAndProof];
        let mut events_future = self
            .client
            .get_events::<E>(topics.as_slice())
            .await
            .unwrap();

        let expected_contribution_len = self.contribution_and_proofs.len();

        self.client
            .post_validator_contribution_and_proofs(self.contribution_and_proofs.as_slice())
            .await
            .unwrap();

        let contribution_events = poll_events(
            &mut events_future,
            expected_contribution_len,
            Duration::from_millis(10000),
        )
        .await;
        assert_eq!(
            contribution_events.as_slice(),
            self.contribution_and_proofs
                .clone()
                .into_iter()
                .map(|contribution| EventKind::ContributionAndProof(Box::new(contribution)))
                .collect::<Vec<_>>()
                .as_slice()
        );

        self
    }

    pub async fn test_get_events_from_genesis(self) -> Self {
        let topics = vec![EventTopic::Block, EventTopic::Head];
        let mut events_future = self
            .client
            .get_events::<E>(topics.as_slice())
            .await
            .unwrap();

        let block_root = self.next_block.signed_block().canonical_root();
        let next_slot = self.next_block.signed_block().slot();

        let expected_block = EventKind::Block(SseBlock {
            block: block_root,
            slot: next_slot,
            execution_optimistic: false,
        });

        let expected_head = EventKind::Head(SseHead {
            block: block_root,
            slot: next_slot,
            state: self.next_block.signed_block().state_root(),
            current_duty_dependent_root: self.chain.genesis_block_root,
            previous_duty_dependent_root: self.chain.genesis_block_root,
            epoch_transition: false,
            execution_optimistic: false,
        });

        self.client
            .post_beacon_blocks(&self.next_block)
            .await
            .unwrap();

        let block_events = poll_events(&mut events_future, 2, Duration::from_millis(10000)).await;
        assert_eq!(block_events.as_slice(), &[expected_block, expected_head]);

        self
    }

    pub async fn test_check_optimistic_responses(&mut self) {
        // Check responses are not optimistic.
        let result = self
            .client
            .get_beacon_headers_block_id(CoreBlockId::Head)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.execution_optimistic, Some(false));

        // Change head to be optimistic.
        self.chain
            .canonical_head
            .fork_choice_write_lock()
            .proto_array_mut()
            .core_proto_array_mut()
            .nodes
            .last_mut()
            .map(|head_node| {
                head_node.execution_status = ExecutionStatus::Optimistic(ExecutionBlockHash::zero())
            });

        // Check responses are now optimistic.
        let result = self
            .client
            .get_beacon_headers_block_id(CoreBlockId::Head)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.execution_optimistic, Some(true));
    }
}

async fn poll_events<S: Stream<Item = Result<EventKind<E>, eth2::Error>> + Unpin, E: EthSpec>(
    stream: &mut S,
    num_events: usize,
    timeout: Duration,
) -> Vec<EventKind<E>> {
    let mut events = Vec::new();

    let collect_stream_fut = async {
        loop {
            if let Some(result) = stream.next().await {
                events.push(result.unwrap());
                if events.len() == num_events {
                    return;
                }
            }
        }
    };

    tokio::select! {
            _ = collect_stream_fut => {events}
            _ = tokio::time::sleep(timeout) => { return events; }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_events() {
    ApiTester::new().await.test_get_events().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_events_altair() {
    let mut config = ApiTesterConfig::default();
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    ApiTester::new_from_config(config)
        .await
        .test_get_events_altair()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_events_from_genesis() {
    ApiTester::new_from_genesis()
        .await
        .test_get_events_from_genesis()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_unsupported_media_response() {
    ApiTester::new()
        .await
        .post_beacon_states_validator_balances_unsupported_media_failure()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_get() {
    ApiTester::new()
        .await
        .test_beacon_genesis()
        .await
        .test_beacon_states_root_finalized()
        .await
        .test_beacon_states_fork_finalized()
        .await
        .test_beacon_states_finality_checkpoints_finalized()
        .await
        .test_beacon_headers_block_id_finalized()
        .await
        .test_beacon_blocks_finalized::<MainnetEthSpec>()
        .await
        .test_beacon_blinded_blocks_finalized::<MainnetEthSpec>()
        .await
        .test_debug_beacon_states_finalized()
        .await
        .test_beacon_states_root()
        .await
        .test_beacon_states_fork()
        .await
        .test_beacon_states_finality_checkpoints()
        .await
        .test_beacon_states_validators()
        .await
        .test_beacon_states_validator_balances()
        .await
        .test_beacon_states_committees()
        .await
        .test_beacon_states_validator_id()
        .await
        .test_beacon_states_randao()
        .await
        .test_beacon_headers_all_slots()
        .await
        .test_beacon_headers_all_parents()
        .await
        .test_beacon_headers_block_id()
        .await
        .test_beacon_blocks()
        .await
        .test_beacon_blinded_blocks()
        .await
        .test_beacon_blocks_attestations()
        .await
        .test_beacon_blocks_root()
        .await
        .test_get_beacon_pool_attestations()
        .await
        .test_get_beacon_pool_attester_slashings()
        .await
        .test_get_beacon_pool_proposer_slashings()
        .await
        .test_get_beacon_pool_voluntary_exits()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_beacon_blocks_valid() {
    ApiTester::new().await.test_post_beacon_blocks_valid().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_beacon_blocks_ssz_valid() {
    ApiTester::new()
        .await
        .test_post_beacon_blocks_ssz_valid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_post_beacon_blocks_ssz_invalid() {
    ApiTester::new()
        .await
        .test_post_beacon_blocks_ssz_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_beacon_blocks_invalid() {
    ApiTester::new()
        .await
        .test_post_beacon_blocks_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_beacon_blocks_duplicate() {
    ApiTester::new()
        .await
        .test_post_beacon_blocks_duplicate()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attestations_valid_v1() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attestations_valid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attestations_invalid_v1() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attestations_invalid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attestations_valid_v2() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attestations_valid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attestations_invalid_v2() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attestations_invalid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attester_slashings_valid_v1() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attester_slashings_valid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attester_slashings_invalid_v1() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attester_slashings_invalid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attester_slashings_valid_v2() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attester_slashings_valid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_attester_slashings_invalid_v2() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_attester_slashings_invalid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_proposer_slashings_valid() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_proposer_slashings_valid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_proposer_slashings_invalid() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_proposer_slashings_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_voluntary_exits_valid() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_voluntary_exits_valid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beacon_pools_post_voluntary_exits_invalid() {
    ApiTester::new()
        .await
        .test_post_beacon_pool_voluntary_exits_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_get() {
    ApiTester::new()
        .await
        .test_get_config_fork_schedule()
        .await
        .test_get_config_spec()
        .await
        .test_get_config_deposit_contract()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_get() {
    ApiTester::new()
        .await
        .test_get_debug_beacon_states()
        .await
        .test_get_debug_beacon_heads()
        .await
        .test_get_debug_fork_choice()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_get() {
    ApiTester::new()
        .await
        .test_get_node_version()
        .await
        .test_get_node_syncing()
        .await
        .test_get_node_syncing_stalled()
        .await
        .test_get_node_identity()
        .await
        .test_get_node_health()
        .await
        .test_get_node_peers_by_id()
        .await
        .test_get_node_peers()
        .await
        .test_get_node_peer_count()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_light_client_updates() {
    let config = ApiTesterConfig {
        spec: ForkName::Altair.make_genesis_spec(E::default_spec()),
        ..<_>::default()
    };
    ApiTester::new_from_config(config)
        .await
        .test_get_beacon_light_client_updates()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_light_client_bootstrap() {
    let config = ApiTesterConfig {
        spec: ForkName::Altair.make_genesis_spec(E::default_spec()),
        ..<_>::default()
    };
    ApiTester::new_from_config(config)
        .await
        .test_get_beacon_light_client_bootstrap()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_light_client_optimistic_update() {
    let config = ApiTesterConfig {
        spec: ForkName::Altair.make_genesis_spec(E::default_spec()),
        ..<_>::default()
    };
    ApiTester::new_from_config(config)
        .await
        .test_get_beacon_light_client_optimistic_update()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_light_client_finality_update() {
    let config = ApiTesterConfig {
        spec: ForkName::Altair.make_genesis_spec(E::default_spec()),
        ..<_>::default()
    };
    ApiTester::new_from_config(config)
        .await
        .test_get_beacon_light_client_finality_update()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_duties_early() {
    ApiTester::new()
        .await
        .test_get_validator_duties_early()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_duties_attester() {
    ApiTester::new()
        .await
        .test_get_validator_duties_attester()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_duties_attester_with_skip_slots() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_duties_attester()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_duties_proposer() {
    ApiTester::new_from_config(ApiTesterConfig::default().retain_historic_states())
        .await
        .test_get_validator_duties_proposer()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_duties_proposer_with_skip_slots() {
    ApiTester::new_from_config(ApiTesterConfig::default().retain_historic_states())
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_duties_proposer()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production() {
    ApiTester::new().await.test_block_production().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_with_skip_slots() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_block_production()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_no_verify_randao() {
    ApiTester::new()
        .await
        .test_block_production_no_verify_randao()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_verify_randao_invalid() {
    ApiTester::new()
        .await
        .test_block_production_verify_randao_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_ssz_full_payload() {
    ApiTester::new().await.test_block_production_ssz().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_ssz_with_skip_slots() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_block_production_ssz()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_ssz_v3() {
    ApiTester::new().await.test_block_production_v3_ssz().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_production_v3_ssz_with_skip_slots() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_block_production_v3_ssz()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_full_payload_premerge() {
    ApiTester::new().await.test_blinded_block_production().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_ssz_full_payload_premerge() {
    ApiTester::new()
        .await
        .test_blinded_block_production_ssz()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_with_skip_slots_full_payload_premerge() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_blinded_block_production()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_ssz_with_skip_slots_full_payload_premerge() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_blinded_block_production_ssz()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_no_verify_randao_full_payload_premerge() {
    ApiTester::new()
        .await
        .test_blinded_block_production_no_verify_randao()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_verify_randao_invalid_full_payload_premerge() {
    ApiTester::new()
        .await
        .test_blinded_block_production_verify_randao_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_blinded_payload_premerge() {
    ApiTester::new().await.test_blinded_block_production().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_with_skip_slots_blinded_payload_premerge() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_blinded_block_production()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_no_verify_randao_blinded_payload_premerge() {
    ApiTester::new()
        .await
        .test_blinded_block_production_no_verify_randao()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blinded_block_production_verify_randao_invalid_blinded_payload_premerge() {
    ApiTester::new()
        .await
        .test_blinded_block_production_verify_randao_invalid()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_attestation_data() {
    ApiTester::new()
        .await
        .test_get_validator_attestation_data()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_attestation_data_with_skip_slots() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_attestation_data()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_attestation() {
    ApiTester::new()
        .await
        .test_get_validator_aggregate_attestation()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_attestation_with_skip_slots() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_aggregate_attestation()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_valid_v1() {
    ApiTester::new()
        .await
        .test_get_validator_aggregate_and_proofs_valid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_valid_with_skip_slots_v1() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_aggregate_and_proofs_valid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_valid_v2() {
    ApiTester::new()
        .await
        .test_get_validator_aggregate_and_proofs_valid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_valid_with_skip_slots_v2() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_aggregate_and_proofs_valid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_invalid_v1() {
    ApiTester::new()
        .await
        .test_get_validator_aggregate_and_proofs_invalid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_invalid_with_skip_slots_v1() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_aggregate_and_proofs_invalid_v1()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_invalid_v2() {
    ApiTester::new()
        .await
        .test_get_validator_aggregate_and_proofs_invalid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_aggregate_and_proofs_invalid_with_skip_slots_v2() {
    ApiTester::new()
        .await
        .skip_slots(E::slots_per_epoch() * 2)
        .test_get_validator_aggregate_and_proofs_invalid_v2()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_validator_beacon_committee_subscriptions() {
    ApiTester::new()
        .await
        .test_get_validator_beacon_committee_subscriptions()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_validator() {
    ApiTester::new()
        .await
        .test_post_validator_register_validator()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_validator_slashed() {
    ApiTester::new()
        .await
        .test_post_validator_register_validator_slashed()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_valid() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_respects_registration()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_zero_builder_boost_factor() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_zero_builder_boost_factor()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_max_builder_boost_factor() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_max_builder_boost_factor()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_valid_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_respects_registration()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_gas_limit_mutation() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_accepts_mutated_gas_limit()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_gas_limit_mutation_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_accepts_mutated_gas_limit()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_fee_recipient_mutation() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_accepts_changed_fee_recipient()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_register_fee_recipient_mutation_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_accepts_changed_fee_recipient()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blinded_block_invalid_parent_hash() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_rejects_invalid_parent_hash()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_full_block_invalid_parent_hash_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_rejects_invalid_parent_hash()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blinded_block_invalid_prev_randao() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_rejects_invalid_prev_randao()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_full_block_invalid_prev_randao_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_rejects_invalid_prev_randao()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blinded_block_invalid_block_number() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_rejects_invalid_block_number()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_full_block_invalid_block_number_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_rejects_invalid_block_number()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blinded_block_invalid_timestamp() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_rejects_invalid_timestamp()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_full_block_invalid_timestamp_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_rejects_invalid_timestamp()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blinded_block_invalid_signature() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_rejects_invalid_signature()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_full_block_invalid_signature_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_payload_v3_rejects_invalid_signature()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_skips() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_chain_health_skips()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_skips_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_v3_chain_health_skips()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_skips_per_epoch() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_chain_health_skips_per_epoch()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_skips_per_epoch_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_v3_chain_health_skips_per_epoch()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_epochs_since_finalization() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_chain_health_epochs_since_finalization()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_epochs_since_finalization_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_v3_chain_health_epochs_since_finalization()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_optimistic_head() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_chain_health_optimistic_head()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_chain_health_optimistic_head_v3() {
    ApiTester::new_mev_tester()
        .await
        .test_builder_v3_chain_health_optimistic_head()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_payload_chosen_by_profit() {
    ApiTester::new_mev_tester_default_payload_value()
        .await
        .test_builder_payload_chosen_when_more_profitable()
        .await
        .test_local_payload_chosen_when_equally_profitable()
        .await
        .test_local_payload_chosen_when_more_profitable()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_payload_chosen_by_profit_v3() {
    ApiTester::new_mev_tester_default_payload_value()
        .await
        .test_builder_payload_v3_chosen_when_more_profitable()
        .await
        .test_local_payload_v3_chosen_when_equally_profitable()
        .await
        .test_local_payload_v3_chosen_when_more_profitable()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_works_post_capella() {
    let mut config = ApiTesterConfig {
        retain_historic_states: false,
        spec: E::default_spec(),
    };
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));

    ApiTester::new_from_config(config)
        .await
        .test_post_validator_register_validator()
        .await
        .test_builder_works_post_capella()
        .await
        .test_lighthouse_rejects_invalid_withdrawals_root()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_works_post_deneb() {
    let mut config = ApiTesterConfig {
        retain_historic_states: false,
        spec: E::default_spec(),
    };
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));
    config.spec.deneb_fork_epoch = Some(Epoch::new(0));

    ApiTester::new_from_config(config)
        .await
        .test_post_validator_register_validator()
        .await
        .test_builder_works_post_deneb()
        .await
        .test_lighthouse_rejects_invalid_withdrawals_root_v3()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blob_sidecars() {
    let mut config = ApiTesterConfig {
        retain_historic_states: false,
        spec: E::default_spec(),
    };
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));
    config.spec.deneb_fork_epoch = Some(Epoch::new(0));

    ApiTester::new_from_config(config)
        .await
        .test_post_beacon_blocks_valid()
        .await
        .test_get_blob_sidecars(false)
        .await
        .test_get_blob_sidecars(true)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blob_sidecars_pruned() {
    let mut config = ApiTesterConfig::default();
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));
    config.spec.deneb_fork_epoch = Some(Epoch::new(0));

    ApiTester::new_from_config(config)
        .await
        .test_get_blob_sidecars_pruned(false)
        .await
        .test_get_blob_sidecars_pruned(true)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_blob_sidecars_pre_deneb() {
    let mut config = ApiTesterConfig::default();
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));
    config.spec.deneb_fork_epoch = Some(Epoch::new(1));

    ApiTester::new_from_config(config)
        .await
        .test_get_blob_sidecars_pre_deneb()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_validator_liveness_epoch() {
    ApiTester::new()
        .await
        .test_post_validator_liveness_epoch()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lighthouse_endpoints() {
    ApiTester::new()
        .await
        .test_get_lighthouse_health()
        .await
        .test_get_lighthouse_syncing()
        .await
        .test_get_lighthouse_proto_array()
        .await
        .test_get_lighthouse_validator_inclusion()
        .await
        .test_get_lighthouse_validator_inclusion_global()
        .await
        .test_get_lighthouse_eth1_syncing()
        .await
        .test_get_lighthouse_eth1_block_cache()
        .await
        .test_get_lighthouse_eth1_deposit_cache()
        .await
        .test_get_lighthouse_staking()
        .await
        .test_get_lighthouse_database_info()
        .await
        .test_post_lighthouse_database_reconstruct()
        .await
        .test_post_lighthouse_liveness()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn optimistic_responses() {
    ApiTester::new_with_hard_forks(true, true)
        .await
        .test_check_optimistic_responses()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_withdrawals_invalid_pre_capella() {
    let mut config = ApiTesterConfig::default();
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    ApiTester::new_from_config(config)
        .await
        .test_get_expected_withdrawals_pre_capella()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_withdrawals_invalid_state() {
    let mut config = ApiTesterConfig::default();
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));
    ApiTester::new_from_config(config)
        .await
        .test_get_expected_withdrawals_invalid_state()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_withdrawals_valid_capella() {
    let mut config = ApiTesterConfig::default();
    config.spec.altair_fork_epoch = Some(Epoch::new(0));
    config.spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    config.spec.capella_fork_epoch = Some(Epoch::new(0));
    ApiTester::new_from_config(config)
        .await
        .test_get_expected_withdrawals_capella()
        .await;
}
