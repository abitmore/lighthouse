use crate::observed_attesters::SlotSubcommitteeIndex;
use crate::types::consts::altair::SYNC_COMMITTEE_SUBNET_COUNT;
use crate::{BeaconChain, BeaconChainError, BeaconChainTypes};
use bls::FixedBytesExtended;
pub use metrics::*;
use slot_clock::SlotClock;
use std::sync::LazyLock;
use types::{BeaconState, Epoch, EthSpec, Hash256, Slot};

// Attestation simulator metrics
pub const VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_HEAD_ATTESTER_HIT_TOTAL: &str =
    "validator_monitor_attestation_simulator_head_attester_hit_total";
pub const VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_HEAD_ATTESTER_MISS_TOTAL: &str =
    "validator_monitor_attestation_simulator_head_attester_miss_total";
pub const VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_TARGET_ATTESTER_HIT_TOTAL: &str =
    "validator_monitor_attestation_simulator_target_attester_hit_total";
pub const VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_TARGET_ATTESTER_MISS_TOTAL: &str =
    "validator_monitor_attestation_simulator_target_attester_miss_total";
pub const VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_SOURCE_ATTESTER_HIT_TOTAL: &str =
    "validator_monitor_attestation_simulator_source_attester_hit_total";
pub const VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_SOURCE_ATTESTER_MISS_TOTAL: &str =
    "validator_monitor_attestation_simulator_source_attester_miss_total";

/*
 * Block Processing
 */
pub static BLOCK_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_block_processing_requests_total",
        "Count of blocks submitted for processing",
    )
});
pub static BLOCK_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_block_processing_successes_total",
        "Count of blocks processed without error",
    )
});
// Keeping the existing "snapshot_cache" metric name as it would break existing dashboards
pub static BLOCK_PROCESSING_SNAPSHOT_CACHE_SIZE: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_block_processing_snapshot_cache_size",
        "Count snapshots in the snapshot cache",
    )
});
pub static BLOCK_PROCESSING_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_seconds",
        "Full runtime of block processing",
    )
});
pub static BLOCK_PROCESSING_BLOCK_ROOT: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_block_root_seconds",
        "Time spent calculating the block root when processing a block.",
    )
});
pub static BLOCK_HEADER_PROCESSING_BLOCK_ROOT: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_header_processing_block_root_seconds",
        "Time spent calculating the block root for a beacon block header.",
    )
});
pub static BLOCK_PROCESSING_BLOB_ROOT: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_blob_root_seconds",
        "Time spent calculating the blob root when processing a block.",
    )
});
pub static BLOCK_PROCESSING_DB_READ: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_db_read_seconds",
        "Time spent loading block and state from DB for block processing",
    )
});
pub static BLOCK_PROCESSING_CATCHUP_STATE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_catch_up_state_seconds",
        "Time spent skipping slots on a state before processing a block.",
    )
});
pub static BLOCK_PROCESSING_COMMITTEE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_committee_building_seconds",
        "Time spent building/obtaining committees for block processing.",
    )
});
pub static BLOCK_PROCESSING_SIGNATURE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_signature_seconds",
        "Time spent doing signature verification for a block.",
    )
});
pub static BLOCK_PROCESSING_CORE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_core_seconds",
        "Time spent doing the core per_block_processing state processing.",
    )
});
pub static BLOCK_PROCESSING_STATE_ROOT: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_state_root_seconds",
        "Time spent calculating the state root when processing a block.",
    )
});
pub static BLOCK_PROCESSING_POST_EXEC_PROCESSING: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram_with_buckets(
            "beacon_block_processing_post_exec_pre_attestable_seconds",
            "Time between finishing execution processing and the block becoming attestable",
            linear_buckets(5e-3, 5e-3, 10),
        )
    });
pub static BLOCK_PROCESSING_DATA_COLUMNS_WAIT: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_block_processing_data_columns_wait_seconds",
        "Time spent waiting for data columns to be computed before starting database write",
        exponential_buckets(0.01, 2.0, 10),
    )
});
pub static BLOCK_PROCESSING_DB_WRITE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_db_write_seconds",
        "Time spent writing a newly processed block and state to DB",
    )
});
pub static BLOCK_PROCESSING_ATTESTATION_OBSERVATION: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_block_processing_attestation_observation_seconds",
            "Time spent hashing and remembering all the attestations in the block",
        )
    });
pub static BLOCK_PROCESSING_FORK_CHOICE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_block_processing_fork_choice_seconds",
        "Time spent running fork choice's `get_head` during block import",
        exponential_buckets(1e-3, 2.0, 8),
    )
});
pub static BLOCK_PROCESSING_PUBKEY_CACHE_LOCK: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_processing_pubkey_cache_lock_seconds",
        "Time spent waiting or holding the pubkey cache write lock",
    )
});
pub static BLOCK_SYNC_AGGREGATE_SET_BITS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "block_sync_aggregate_set_bits",
        "The number of true bits in the last sync aggregate in a block",
    )
});

/*
 * Block Production
 */
pub static BLOCK_PRODUCTION_REQUESTS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_block_production_requests_total",
        "Count of all block production requests",
    )
});
pub static BLOCK_PRODUCTION_SUCCESSES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_block_production_successes_total",
        "Count of blocks successfully produced.",
    )
});
pub static BLOCK_PRODUCTION_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_seconds",
        "Full runtime of block production",
    )
});
pub static BLOCK_PRODUCTION_FORK_CHOICE_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_fork_choice_seconds",
        "Time taken to run fork choice before block production",
    )
});
pub static BLOCK_PRODUCTION_GET_PROPOSER_HEAD_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram_with_buckets(
            "beacon_block_production_get_proposer_head_times",
            "Time taken for fork choice to compute the proposer head before block production",
            exponential_buckets(1e-3, 2.0, 8),
        )
    });
pub static BLOCK_PRODUCTION_STATE_LOAD_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_state_load_seconds",
        "Time taken to load the base state for block production",
    )
});
pub static BLOCK_PRODUCTION_SLOT_PROCESS_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_slot_process_seconds",
        "Time taken to advance the state to the block production slot",
    )
});
pub static BLOCK_PRODUCTION_UNAGGREGATED_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_unaggregated_seconds",
        "Time taken to import the naive aggregation pool for block production",
    )
});
pub static BLOCK_PRODUCTION_ATTESTATION_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_attestation_seconds",
        "Time taken to pack attestations into a block",
    )
});
pub static BLOCK_PRODUCTION_PROCESS_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_process_seconds",
        "Time taken to process the block produced",
    )
});
pub static BLOCK_PRODUCTION_STATE_ROOT_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_block_production_state_root_seconds",
        "Time taken to calculate the block's state root",
    )
});

/*
 * Block Statistics
 */
pub static OPERATIONS_PER_BLOCK_ATTESTATION: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_operations_per_block_attestation_total",
        "Number of attestations in a block",
        // Full block is 128.
        Ok(vec![
            0_f64, 1_f64, 3_f64, 15_f64, 31_f64, 63_f64, 127_f64, 255_f64,
        ]),
    )
});

pub static BLOCK_SIZE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_block_total_size",
        "Size of a signed beacon block",
        linear_buckets(5120_f64, 5120_f64, 10),
    )
});

/*
 * Unaggregated Attestation Verification
 */
pub static UNAGGREGATED_ATTESTATION_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_unaggregated_attestation_processing_requests_total",
            "Count of all unaggregated attestations submitted for processing",
        )
    });
pub static UNAGGREGATED_ATTESTATION_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_unaggregated_attestation_processing_successes_total",
            "Number of unaggregated attestations verified for gossip",
        )
    });
pub static UNAGGREGATED_ATTESTATION_GOSSIP_VERIFICATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_unaggregated_attestation_gossip_verification_seconds",
            "Full runtime of aggregated attestation gossip verification",
        )
    });

/*
 * Aggregated Attestation Verification
 */
pub static AGGREGATED_ATTESTATION_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_aggregated_attestation_processing_requests_total",
            "Count of all aggregated attestations submitted for processing",
        )
    });
pub static AGGREGATED_ATTESTATION_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_aggregated_attestation_processing_successes_total",
            "Number of aggregated attestations verified for gossip",
        )
    });
pub static AGGREGATED_ATTESTATION_GOSSIP_VERIFICATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_aggregated_attestation_gossip_verification_seconds",
            "Full runtime of aggregated attestation gossip verification",
        )
    });

/*
 * General Attestation Processing
 */
pub static ATTESTATION_PROCESSING_APPLY_TO_AGG_POOL: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_apply_to_agg_pool",
            "Time spent applying an attestation to the naive aggregation pool",
        )
    });
pub static ATTESTATION_PROCESSING_AGG_POOL_PRUNE: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_agg_pool_prune",
            "Time spent for the agg pool to prune",
        )
    });
pub static ATTESTATION_PROCESSING_AGG_POOL_INSERT: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_agg_pool_insert",
            "Time spent for the outer pool.insert() function of agg pool",
        )
    });
pub static ATTESTATION_PROCESSING_AGG_POOL_CORE_INSERT: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_agg_pool_core_insert",
            "Time spent for the core map.insert() function of agg pool",
        )
    });
pub static ATTESTATION_PROCESSING_AGG_POOL_AGGREGATION: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_agg_pool_aggregation",
            "Time spent doing signature aggregation when adding to the agg poll",
        )
    });
pub static ATTESTATION_PROCESSING_AGG_POOL_CREATE_MAP: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_agg_pool_create_map",
            "Time spent for creating a map for a new slot",
        )
    });
pub static ATTESTATION_PROCESSING_APPLY_TO_OP_POOL: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_apply_to_op_pool",
            "Time spent applying an attestation to the block inclusion pool",
        )
    });

/*
 * Attestation Processing
 */
pub static ATTESTATION_PROCESSING_SHUFFLING_CACHE_WAIT_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_shuffling_cache_wait_seconds",
            "Time spent on waiting for the shuffling cache lock during attestation processing",
        )
    });
pub static ATTESTATION_PROCESSING_COMMITTEE_BUILDING_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_committee_building_seconds",
            "Time spent on building committees during attestation processing",
        )
    });
pub static ATTESTATION_PROCESSING_STATE_READ_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_state_read_seconds",
            "Time spent on reading the state during attestation processing",
        )
    });
pub static ATTESTATION_PROCESSING_STATE_SKIP_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_state_skip_seconds",
            "Time spent on reading the state during attestation processing",
        )
    });
pub static ATTESTATION_PROCESSING_SIGNATURE_SETUP_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_signature_setup_seconds",
            "Time spent on setting up for the signature verification of attestation processing",
        )
    });
pub static ATTESTATION_PROCESSING_SIGNATURE_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_signature_seconds",
            "Time spent on the signature verification of attestation processing",
        )
    });

/*
 * Batch Attestation Processing
 */
pub static ATTESTATION_PROCESSING_BATCH_AGG_SIGNATURE_SETUP_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_batch_agg_signature_setup_times",
            "Time spent on setting up for the signature verification of batch aggregate processing",
        )
    });
pub static ATTESTATION_PROCESSING_BATCH_AGG_SIGNATURE_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_batch_agg_signature_times",
            "Time spent on the signature verification of batch aggregate attestation processing",
        )
    });
pub static ATTESTATION_PROCESSING_BATCH_UNAGG_SIGNATURE_SETUP_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
        "beacon_attestation_processing_batch_unagg_signature_setup_times",
        "Time spent on setting up for the signature verification of batch unaggregate processing"
    )
    });
pub static ATTESTATION_PROCESSING_BATCH_UNAGG_SIGNATURE_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_attestation_processing_batch_unagg_signature_times",
            "Time spent on the signature verification of batch unaggregate attestation processing",
        )
    });

/*
 * Shuffling cache
 */
pub static SHUFFLING_CACHE_HITS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_shuffling_cache_hits_total",
        "Count of times shuffling cache fulfils request",
    )
});
pub static SHUFFLING_CACHE_MISSES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_shuffling_cache_misses_total",
        "Count of times shuffling cache fulfils request",
    )
});
pub static SHUFFLING_CACHE_PROMISE_HITS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_shuffling_cache_promise_hits_total",
        "Count of times shuffling cache returns a promise to future shuffling",
    )
});
pub static SHUFFLING_CACHE_PROMISE_FAILS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_shuffling_cache_promise_fails_total",
        "Count of times shuffling cache detects a failed promise",
    )
});

/*
 * Early attester cache
 */
pub static BEACON_EARLY_ATTESTER_CACHE_HITS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_early_attester_cache_hits",
        "Count of times the early attester cache returns an attestation",
    )
});

pub static BEACON_REQRESP_PRE_IMPORT_CACHE_SIZE: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_reqresp_pre_import_cache_size",
        "Current count of items of the reqresp pre import cache",
    )
});
pub static BEACON_REQRESP_PRE_IMPORT_CACHE_HITS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_reqresp_pre_import_cache_hits",
            "Count of times the reqresp pre import cache returns an item",
        )
    });

/*
 * Attestation Production
 */
pub static ATTESTATION_PRODUCTION_SECONDS: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_attestation_production_seconds",
        "Full runtime of attestation production",
    )
});
pub static ATTESTATION_PRODUCTION_HEAD_SCRAPE_SECONDS: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "attestation_production_head_scrape_seconds",
            "Time taken to read the head state",
        )
    });
pub static ATTESTATION_PRODUCTION_CACHE_INTERACTION_SECONDS: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "attestation_production_cache_interaction_seconds",
            "Time spent interacting with the attester cache",
        )
    });
pub static ATTESTATION_PRODUCTION_CACHE_PRIME_SECONDS: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "attestation_production_cache_prime_seconds",
            "Time spent loading a new state from the disk due to a cache miss",
        )
    });

/*
 * Fork Choice
 */
pub static FORK_CHOICE_REQUESTS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_fork_choice_requests_total",
        "Count of occasions where fork choice has tried to find a head",
    )
});
pub static FORK_CHOICE_ERRORS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_fork_choice_errors_total",
        "Count of occasions where fork choice has returned an error when trying to find a head",
    )
});
pub static FORK_CHOICE_CHANGED_HEAD: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_fork_choice_changed_head_total",
        "Count of occasions fork choice has found a new head",
    )
});
pub static FORK_CHOICE_REORG_COUNT: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_fork_choice_reorg_total",
        "Count of occasions fork choice has switched to a different chain",
    )
});
pub static FORK_CHOICE_REORG_DISTANCE: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_fork_choice_reorg_distance",
        "The distance of each re-org of the fork choice algorithm",
    )
});
pub static FORK_CHOICE_REORG_COUNT_INTEROP: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_reorgs_total",
        "Count of occasions fork choice has switched to a different chain",
    )
});
pub static FORK_CHOICE_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_fork_choice_seconds",
        "Full runtime of fork choice",
        linear_buckets(10e-3, 20e-3, 10),
    )
});
pub static FORK_CHOICE_OVERRIDE_FCU_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_fork_choice_override_fcu_seconds",
        "Time taken to compute the optional forkchoiceUpdated override",
        exponential_buckets(1e-3, 2.0, 8),
    )
});
pub static FORK_CHOICE_AFTER_NEW_HEAD_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_fork_choice_after_new_head_seconds",
        "Time taken to run `after_new_head`",
        exponential_buckets(1e-3, 2.0, 10),
    )
});
pub static FORK_CHOICE_AFTER_FINALIZATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram_with_buckets(
            "beacon_fork_choice_after_finalization_seconds",
            "Time taken to run `after_finalization`",
            exponential_buckets(1e-3, 2.0, 10),
        )
    });
pub static FORK_CHOICE_READ_LOCK_AQUIRE_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_fork_choice_read_lock_aquire_seconds",
        "Time taken to aquire the fork-choice read lock",
        exponential_buckets(1e-4, 4.0, 7),
    )
});
pub static FORK_CHOICE_WRITE_LOCK_AQUIRE_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram_with_buckets(
        "beacon_fork_choice_write_lock_aquire_seconds",
        "Time taken to aquire the fork-choice write lock",
        exponential_buckets(1e-3, 4.0, 7),
    )
});
pub static FORK_CHOICE_SET_HEAD_LAG_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_fork_choice_set_head_lag_times",
        "Time taken between finding the head and setting the canonical head value",
    )
});
pub static BALANCES_CACHE_HITS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_balances_cache_hits_total",
        "Count of times balances cache fulfils request",
    )
});
pub static BALANCES_CACHE_MISSES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_balances_cache_misses_total",
        "Count of times balances cache misses request",
    )
});

/*
 * Persisting BeaconChain components to disk
 */
pub static PERSIST_HEAD: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_persist_head",
        "Time taken to persist the canonical head",
    )
});
pub static PERSIST_OP_POOL: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_persist_op_pool",
        "Time taken to persist the operations pool",
    )
});
pub static PERSIST_ETH1_CACHE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_persist_eth1_cache",
        "Time taken to persist the eth1 caches",
    )
});
pub static PERSIST_FORK_CHOICE: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_persist_fork_choice",
        "Time taken to persist the fork choice struct",
    )
});

/*
 * Eth1
 */
pub static DEFAULT_ETH1_VOTES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_eth1_default_votes",
        "Count of times we have voted default value for eth1 data",
    )
});

/*
 * Chain Head
 */
pub static UPDATE_HEAD_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_update_head_seconds",
        "Time taken to update the canonical head",
    )
});
pub static HEAD_STATE_SLOT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_slot",
        "Slot of the block at the head of the chain",
    )
});
pub static HEAD_STATE_SLOT_INTEROP: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_slot",
        "Slot of the block at the head of the chain",
    )
});
pub static HEAD_STATE_ROOT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_root",
        "Root of the block at the head of the chain",
    )
});
pub static HEAD_STATE_LATEST_BLOCK_SLOT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_latest_block_slot",
        "Latest block slot at the head of the chain",
    )
});
pub static HEAD_STATE_CURRENT_JUSTIFIED_ROOT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_current_justified_root",
        "Current justified root at the head of the chain",
    )
});
pub static HEAD_STATE_CURRENT_JUSTIFIED_EPOCH: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_current_justified_epoch",
        "Current justified epoch at the head of the chain",
    )
});
pub static HEAD_STATE_CURRENT_JUSTIFIED_EPOCH_INTEROP: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_current_justified_epoch",
            "Current justified epoch at the head of the chain",
        )
    });
pub static HEAD_STATE_PREVIOUS_JUSTIFIED_ROOT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_previous_justified_root",
        "Previous justified root at the head of the chain",
    )
});
pub static HEAD_STATE_PREVIOUS_JUSTIFIED_EPOCH: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_previous_justified_epoch",
        "Previous justified epoch at the head of the chain",
    )
});
pub static HEAD_STATE_PREVIOUS_JUSTIFIED_EPOCH_INTEROP: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_previous_justified_epoch",
            "Previous justified epoch at the head of the chain",
        )
    });
pub static HEAD_STATE_FINALIZED_ROOT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_finalized_root",
        "Finalized root at the head of the chain",
    )
});
pub static HEAD_STATE_FINALIZED_EPOCH: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_finalized_epoch",
        "Finalized epoch at the head of the chain",
    )
});
pub static HEAD_STATE_FINALIZED_EPOCH_INTEROP: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_finalized_epoch",
        "Finalized epoch at the head of the chain",
    )
});
pub static HEAD_STATE_TOTAL_VALIDATORS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_total_validators_total",
        "Count of validators at the head of the chain",
    )
});
pub static HEAD_STATE_ACTIVE_VALIDATORS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_active_validators_total",
        "Count of active validators at the head of the chain",
    )
});
pub static HEAD_STATE_ACTIVE_VALIDATORS_INTEROP: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_current_active_validators",
        "Count of active validators at the head of the chain",
    )
});
pub static HEAD_STATE_VALIDATOR_BALANCES: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_validator_balances_total",
        "Sum of all validator balances at the head of the chain",
    )
});
pub static HEAD_STATE_SLASHED_VALIDATORS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_slashed_validators_total",
        "Count of all slashed validators at the head of the chain",
    )
});
pub static HEAD_STATE_WITHDRAWN_VALIDATORS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_withdrawn_validators_total",
        "Sum of all validator balances at the head of the chain",
    )
});
pub static HEAD_STATE_ETH1_DEPOSIT_INDEX: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_head_state_eth1_deposit_index",
        "Eth1 deposit index at the head of the chain",
    )
});
pub static HEAD_STATE_ETH1_DEPOSITS_INTEROP: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_processed_deposits_total",
        "Total Eth1 deposits at the head of the chain",
    )
});

/*
 * Operation Pool
 */
pub static OP_POOL_NUM_ATTESTATIONS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_attestations_total",
        "Count of attestations in the op pool",
    )
});
pub static OP_POOL_NUM_ATTESTATION_DATA: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_attestation_data_total",
        "Count of attestation data in the op pool",
    )
});
pub static OP_POOL_MAX_AGGREGATES_PER_DATA: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_max_aggregates_per_data",
        "Max aggregates per AttestationData",
    )
});
pub static OP_POOL_NUM_ATTESTER_SLASHINGS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_attester_slashings_total",
        "Count of attester slashings in the op pool",
    )
});
pub static OP_POOL_NUM_PROPOSER_SLASHINGS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_proposer_slashings_total",
        "Count of proposer slashings in the op pool",
    )
});
pub static OP_POOL_NUM_VOLUNTARY_EXITS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_voluntary_exits_total",
        "Count of voluntary exits in the op pool",
    )
});
pub static OP_POOL_NUM_SYNC_CONTRIBUTIONS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_op_pool_sync_contributions_total",
        "Count of sync contributions in the op pool",
    )
});

/*
 * Attestation Observation Metrics
 */
pub static ATTN_OBSERVATION_PREV_EPOCH_ATTESTERS: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_attn_observation_epoch_attesters",
            "Count of attesters that have been seen by the beacon chain in the previous epoch",
        )
    });
pub static ATTN_OBSERVATION_PREV_EPOCH_AGGREGATORS: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_attn_observation_epoch_aggregators",
            "Count of aggregators that have been seen by the beacon chain in the previous epoch",
        )
    });

/*
 * Sync Committee Observation Metrics
 */
pub static SYNC_COMM_OBSERVATION_PREV_SLOT_SIGNERS: LazyLock<Result<IntGauge>> = LazyLock::new(
    || {
        try_create_int_gauge(
        "beacon_sync_comm_observation_slot_signers",
        "Count of sync committee contributors that have been seen by the beacon chain in the previous slot"
    )
    },
);
pub static SYNC_COMM_OBSERVATION_PREV_SLOT_AGGREGATORS: LazyLock<Result<IntGauge>> = LazyLock::new(
    || {
        try_create_int_gauge(
        "beacon_sync_comm_observation_slot_aggregators",
        "Count of sync committee aggregators that have been seen by the beacon chain in the previous slot"
    )
    },
);

/*
 * Validator Monitor Metrics (balances, etc)
 */
pub static VALIDATOR_MONITOR_BALANCE_GWEI: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_balance_gwei",
        "The validator's balance in gwei.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_EFFECTIVE_BALANCE_GWEI: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_effective_balance_gwei",
            "The validator's effective balance in gwei.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_SLASHED: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_slashed",
        "Set to 1 if the validator is slashed.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_ACTIVE: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_active",
        "Set to 1 if the validator is active.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_EXITED: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_exited",
        "Set to 1 if the validator is exited.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_WITHDRAWABLE: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_withdrawable",
        "Set to 1 if the validator is withdrawable.",
        &["validator"],
    )
});
pub static VALIDATOR_ACTIVATION_ELIGIBILITY_EPOCH: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_activation_eligibility_epoch",
            "Set to the epoch where the validator will be eligible for activation.",
            &["validator"],
        )
    });
pub static VALIDATOR_ACTIVATION_EPOCH: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_activation_epoch",
        "Set to the epoch where the validator will activate.",
        &["validator"],
    )
});
pub static VALIDATOR_EXIT_EPOCH: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_exit_epoch",
        "Set to the epoch where the validator will exit.",
        &["validator"],
    )
});
pub static VALIDATOR_WITHDRAWABLE_EPOCH: LazyLock<Result<IntGaugeVec>> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_withdrawable_epoch",
        "Set to the epoch where the validator will be withdrawable.",
        &["validator"],
    )
});

/*
 * Validator Monitor Metrics (per-epoch summaries)
 */
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_ATTESTER_HIT: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_prev_epoch_on_chain_attester_hit",
            "Incremented if the validator is flagged as a previous epoch attester \
            during per epoch processing",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_ATTESTER_MISS: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_prev_epoch_on_chain_attester_miss",
            "Incremented if the validator is not flagged as a previous epoch attester \
            during per epoch processing",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_HEAD_ATTESTER_HIT: LazyLock<
    Result<IntCounterVec>,
> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_prev_epoch_on_chain_head_attester_hit",
        "Incremented if the validator is flagged as a previous epoch head attester \
            during per epoch processing",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_HEAD_ATTESTER_MISS: LazyLock<
    Result<IntCounterVec>,
> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_prev_epoch_on_chain_head_attester_miss",
        "Incremented if the validator is not flagged as a previous epoch head attester \
            during per epoch processing",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_TARGET_ATTESTER_HIT: LazyLock<
    Result<IntCounterVec>,
> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_prev_epoch_on_chain_target_attester_hit",
        "Incremented if the validator is flagged as a previous epoch target attester \
            during per epoch processing",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_TARGET_ATTESTER_MISS: LazyLock<
    Result<IntCounterVec>,
> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_prev_epoch_on_chain_target_attester_miss",
        "Incremented if the validator is not flagged as a previous epoch target attester \
            during per epoch processing",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ON_CHAIN_INCLUSION_DISTANCE: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_on_chain_inclusion_distance",
            "The attestation inclusion distance calculated during per epoch processing",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_ATTESTATIONS_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_attestations_total",
            "The number of unagg. attestations seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_ATTESTATIONS_MIN_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
            "validator_monitor_prev_epoch_attestations_min_delay_seconds",
            "The min delay between when the validator should send the attestation and when it was received.",
            &["validator"]
        )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ATTESTATION_AGGREGATE_INCLUSIONS: LazyLock<
    Result<IntGaugeVec>,
> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_prev_epoch_attestation_aggregate_inclusions",
        "The count of times an attestation was seen inside an aggregate.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ATTESTATION_BLOCK_INCLUSIONS: LazyLock<
    Result<IntGaugeVec>,
> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_prev_epoch_attestation_block_inclusions",
        "The count of times an attestation was seen inside a block.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_ATTESTATION_BLOCK_MIN_INCLUSION_DISTANCE: LazyLock<
    Result<IntGaugeVec>,
> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_prev_epoch_attestation_block_min_inclusion_distance",
        "The minimum inclusion distance observed for the inclusion of an attestation in a block.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_BEACON_BLOCKS_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_beacon_blocks_total",
            "The number of beacon_blocks seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_BEACON_BLOCKS_MIN_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
        "validator_monitor_prev_epoch_beacon_blocks_min_delay_seconds",
        "The min delay between when the validator should send the block and when it was received.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_AGGREGATES_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_aggregates_total",
            "The number of aggregates seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_AGGREGATES_MIN_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
            "validator_monitor_prev_epoch_aggregates_min_delay_seconds",
            "The min delay between when the validator should send the aggregate and when it was received.",
            &["validator"]
        )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_EXITS_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_exits_total",
            "The number of exits seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_PROPOSER_SLASHINGS_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_proposer_slashings_total",
            "The number of proposer slashings seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_ATTESTER_SLASHINGS_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_attester_slashings_total",
            "The number of attester slashings seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_SYNC_COMMITTEE_MESSAGES_TOTAL: LazyLock<
    Result<IntGaugeVec>,
> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_prev_epoch_sync_committee_messages_total",
        "The number of sync committee messages seen in the previous epoch.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_SYNC_COMMITTEE_MESSAGES_MIN_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
            "validator_monitor_prev_epoch_sync_committee_messages_min_delay_seconds",
            "The min delay between when the validator should send the sync committee message and when it was received.",
            &["validator"]
        )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_SYNC_CONTRIBUTION_INCLUSIONS: LazyLock<
    Result<IntGaugeVec>,
> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_prev_epoch_sync_contribution_inclusions",
        "The count of times a sync signature was seen inside a sync contribution.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_SYNC_SIGNATURE_BLOCK_INCLUSIONS: LazyLock<
    Result<IntGaugeVec>,
> = LazyLock::new(|| {
    try_create_int_gauge_vec(
        "validator_monitor_prev_epoch_sync_signature_block_inclusions",
        "The count of times a sync signature was seen inside a block.",
        &["validator"],
    )
});
pub static VALIDATOR_MONITOR_PREV_EPOCH_SYNC_CONTRIBUTIONS_TOTAL: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_prev_epoch_sync_contributions_total",
            "The number of sync contributions seen in the previous epoch.",
            &["validator"],
        )
    });
pub static VALIDATOR_MONITOR_PREV_EPOCH_SYNC_CONTRIBUTION_MIN_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
            "validator_monitor_prev_epoch_sync_contribution_min_delay_seconds",
            "The min delay between when the validator should send the sync contribution and when it was received.",
            &["validator"]
        )
});
pub static VALIDATOR_MONITOR_VALIDATOR_IN_CURRENT_SYNC_COMMITTEE: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
            "validator_monitor_validator_in_current_sync_committee",
            "Is the validator in the current sync committee (1 for true and 0 for false)",
            &["validator"],
        )
    });

/*
 * Validator Monitor Metrics (real-time)
 */
pub static VALIDATOR_MONITOR_VALIDATORS_TOTAL: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "validator_monitor_validators_total",
        "Count of validators that are specifically monitored by this beacon node",
    )
});
pub static VALIDATOR_MONITOR_UNAGGREGATED_ATTESTATION_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_unaggregated_attestation_total",
            "Number of unaggregated attestations seen",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_UNAGGREGATED_ATTESTATION_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
        "validator_monitor_unaggregated_attestation_delay_seconds",
        "The delay between when the validator should send the attestation and when it was received.",
        &["src", "validator"]
    )
});
pub static VALIDATOR_MONITOR_SYNC_COMMITTEE_MESSAGES_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_sync_committee_messages_total",
            "Number of sync committee messages seen",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_SYNC_COMMITTEE_MESSAGES_DELAY_SECONDS: LazyLock<Result<HistogramVec>> =
    LazyLock::new(|| {
        try_create_histogram_vec(
        "validator_monitor_sync_committee_messages_delay_seconds",
        "The delay between when the validator should send the sync committee message and when it was received.",
        &["src", "validator"]
    )
    });
pub static VALIDATOR_MONITOR_SYNC_CONTRIBUTIONS_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_sync_contributions_total",
            "Number of sync contributions seen",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_SYNC_CONTRIBUTIONS_DELAY_SECONDS: LazyLock<Result<HistogramVec>> =
    LazyLock::new(|| {
        try_create_histogram_vec(
        "validator_monitor_sync_contributions_delay_seconds",
        "The delay between when the aggregator should send the sync contribution and when it was received.",
        &["src", "validator"]
    )
    });
pub static VALIDATOR_MONITOR_AGGREGATED_ATTESTATION_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_aggregated_attestation_total",
            "Number of aggregated attestations seen",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_AGGREGATED_ATTESTATION_DELAY_SECONDS: LazyLock<Result<HistogramVec>> =
    LazyLock::new(|| {
        try_create_histogram_vec(
        "validator_monitor_aggregated_attestation_delay_seconds",
        "The delay between then the validator should send the aggregate and when it was received.",
        &["src", "validator"]
    )
    });
pub static VALIDATOR_MONITOR_ATTESTATION_IN_AGGREGATE_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_attestation_in_aggregate_total",
            "Number of times an attestation has been seen in an aggregate",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_SYNC_COMMITTEE_MESSAGE_IN_CONTRIBUTION_TOTAL: LazyLock<
    Result<IntCounterVec>,
> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_sync_committee_message_in_contribution_total",
        "Number of times a sync committee message has been seen in a sync contribution",
        &["src", "validator"],
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_IN_AGGREGATE_DELAY_SECONDS: LazyLock<
    Result<HistogramVec>,
> = LazyLock::new(|| {
    try_create_histogram_vec(
        "validator_monitor_attestation_in_aggregate_delay_seconds",
        "The delay between when the validator should send the aggregate and when it was received.",
        &["src", "validator"],
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_IN_BLOCK_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_attestation_in_block_total",
            "Number of times an attestation has been seen in a block",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_SYNC_COMMITTEE_MESSAGE_IN_BLOCK_TOTAL: LazyLock<
    Result<IntCounterVec>,
> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_sync_committee_message_in_block_total",
        "Number of times a validator's sync committee message has been seen in a sync aggregate",
        &["src", "validator"],
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_IN_BLOCK_DELAY_SLOTS: LazyLock<Result<IntGaugeVec>> =
    LazyLock::new(|| {
        try_create_int_gauge_vec(
        "validator_monitor_attestation_in_block_delay_slots",
        "The excess slots (beyond the minimum delay) between the attestation slot and the block slot.",
        &["src", "validator"]
    )
    });
pub static VALIDATOR_MONITOR_BEACON_BLOCK_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_beacon_block_total",
            "Number of beacon blocks seen",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_BEACON_BLOCK_DELAY_SECONDS: LazyLock<Result<HistogramVec>> =
    LazyLock::new(|| {
        try_create_histogram_vec(
            "validator_monitor_beacon_block_delay_seconds",
            "The delay between when the validator should send the block and when it was received.",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_EXIT_TOTAL: LazyLock<Result<IntCounterVec>> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "validator_monitor_exit_total",
        "Number of beacon exits seen",
        &["src", "validator"],
    )
});
pub static VALIDATOR_MONITOR_PROPOSER_SLASHING_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_proposer_slashing_total",
            "Number of proposer slashings seen",
            &["src", "validator"],
        )
    });
pub static VALIDATOR_MONITOR_ATTESTER_SLASHING_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_attester_slashing_total",
            "Number of attester slashings seen",
            &["src", "validator"],
        )
    });

// Prevent recursion limit

/*
 * Block Delay Metrics
 */
pub static BEACON_BLOCK_DELAY_TOTAL: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_block_delay_total",
        "Duration between the start of the block's slot and the time when it was set as head.",
    )
});

pub static BEACON_BLOCK_DELAY_OBSERVED_SLOT_START: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_block_delay_observed_slot_start",
            "Duration between the start of the block's slot and the time the block was observed.",
        )
    });

pub static BEACON_BLOB_DELAY_ALL_OBSERVED_SLOT_START: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_blob_delay_all_observed_slot_start",
            "Duration between the start of the block's slot and the time the block was observed.",
        )
    });

pub static BEACON_BLOCK_DELAY_CONSENSUS_VERIFICATION_TIME: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_block_delay_consensus_verification_time",
            "The time taken to verify the block within Lighthouse",
        )
    });

pub static BEACON_BLOCK_DELAY_EXECUTION_TIME: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_block_delay_execution_time",
        "The duration in verifying the block with the execution layer.",
    )
});

pub static BEACON_BLOCK_DELAY_AVAILABLE_SLOT_START: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_block_delay_available_slot_start",
            "Duration between the time that block became available and the start of the slot.",
        )
    });
pub static BEACON_BLOCK_DELAY_ATTESTABLE_SLOT_START: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "beacon_block_delay_attestable_slot_start",
            "Duration between the time that block became attestable and the start of the slot.",
        )
    });

pub static BEACON_BLOCK_DELAY_IMPORTED_TIME: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_block_delay_imported_time",
        "Duration between the time the block became available and the time when it was imported.",
    )
});

pub static BEACON_BLOCK_DELAY_HEAD_IMPORTED_TIME: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
        "beacon_block_delay_head_imported_time",
        "Duration between the time that block was imported and the time when it was set as head.",
    )
    });
pub static BEACON_BLOCK_DELAY_HEAD_SLOT_START_EXCEEDED_TOTAL: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
        "beacon_block_delay_head_slot_start_exceeded_total",
        "A counter that is triggered when the duration between the start of the block's slot and the current time \
        will result in failed attestations.",
    )
    });

/*
 * General block metrics
 */
pub static GOSSIP_BEACON_BLOCK_SKIPPED_SLOTS: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "gossip_beacon_block_skipped_slots",
        "For each gossip blocks, the number of skip slots between it and its parent",
    )
});

/*
 * Sync Committee Message Verification
 */
pub static SYNC_MESSAGE_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_sync_committee_message_processing_requests_total",
        "Count of all sync messages submitted for processing",
    )
});
pub static SYNC_MESSAGE_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_sync_committee_message_processing_successes_total",
        "Number of sync messages verified for gossip",
    )
});
pub static SYNC_MESSAGE_GOSSIP_VERIFICATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_committee_message_gossip_verification_seconds",
            "Full runtime of sync contribution gossip verification",
        )
    });
pub static SYNC_MESSAGE_EQUIVOCATIONS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "sync_message_equivocations_total",
        "Number of sync messages with the same validator index for different blocks",
    )
});
pub static SYNC_MESSAGE_EQUIVOCATIONS_TO_HEAD: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "sync_message_equivocations_to_head_total",
        "Number of sync message which conflict with a previous message but elect the head",
    )
});

/*
 * Sync Committee Contribution Verification
 */
pub static SYNC_CONTRIBUTION_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_sync_contribution_processing_requests_total",
            "Count of all sync contributions submitted for processing",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_sync_contribution_processing_successes_total",
            "Number of sync contributions verified for gossip",
        )
    });
pub static SYNC_CONTRIBUTION_GOSSIP_VERIFICATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_gossip_verification_seconds",
            "Full runtime of sync contribution gossip verification",
        )
    });

/*
 * General Sync Committee Contribution Processing
 */
pub static SYNC_CONTRIBUTION_PROCESSING_APPLY_TO_AGG_POOL: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_apply_to_agg_pool",
            "Time spent applying a sync contribution to the naive aggregation pool",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_AGG_POOL_PRUNE: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_agg_pool_prune",
            "Time spent for the agg pool to prune",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_AGG_POOL_INSERT: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_agg_pool_insert",
            "Time spent for the outer pool.insert() function of agg pool",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_AGG_POOL_CORE_INSERT: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_agg_pool_core_insert",
            "Time spent for the core map.insert() function of agg pool",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_AGG_POOL_AGGREGATION: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_agg_pool_aggregation",
            "Time spent doing signature aggregation when adding to the agg poll",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_AGG_POOL_CREATE_MAP: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_agg_pool_create_map",
            "Time spent for creating a map for a new slot",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_APPLY_TO_OP_POOL: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_apply_to_op_pool",
            "Time spent applying a sync contribution to the block inclusion pool",
        )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_SIGNATURE_SETUP_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
        "beacon_sync_contribution_processing_signature_setup_seconds",
        "Time spent on setting up for the signature verification of sync contribution processing"
    )
    });
pub static SYNC_CONTRIBUTION_PROCESSING_SIGNATURE_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_contribution_processing_signature_seconds",
            "Time spent on the signature verification of sync contribution processing",
        )
    });

/*
 * General Sync Committee Contribution Processing
 */
pub static SYNC_MESSAGE_PROCESSING_SIGNATURE_SETUP_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_committee_message_processing_signature_setup_seconds",
            "Time spent on setting up for the signature verification of sync message processing",
        )
    });
pub static SYNC_MESSAGE_PROCESSING_SIGNATURE_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_sync_committee_message_processing_signature_seconds",
            "Time spent on the signature verification of sync message processing",
        )
    });

/*
 * Checkpoint sync & backfill
 */
pub static BACKFILL_SIGNATURE_SETUP_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_backfill_signature_setup_seconds",
        "Time spent constructing the signature set during backfill sync",
    )
});
pub static BACKFILL_SIGNATURE_VERIFY_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_backfill_signature_verify_seconds",
        "Time spent verifying the signature set during backfill sync",
    )
});
pub static BACKFILL_SIGNATURE_TOTAL_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "beacon_backfill_signature_total_seconds",
        "Time spent verifying the signature set during backfill sync, including setup",
    )
});

/*
 * Pre-finalization block cache.
 */
pub static PRE_FINALIZATION_BLOCK_CACHE_SIZE: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_pre_finalization_block_cache_size",
        "Number of pre-finalization block roots cached for quick rejection",
    )
});
pub static PRE_FINALIZATION_BLOCK_LOOKUP_COUNT: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "beacon_pre_finalization_block_lookup_count",
        "Number of block roots subject to single block lookups",
    )
});

/*
 * Blob sidecar Verification
 */
pub static BLOBS_SIDECAR_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_blobs_sidecar_processing_requests_total",
        "Count of all blob sidecars submitted for processing",
    )
});
pub static BLOBS_SIDECAR_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_blobs_sidecar_processing_successes_total",
        "Number of blob sidecars verified for gossip",
    )
});
pub static BLOBS_SIDECAR_GOSSIP_VERIFICATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_blobs_sidecar_gossip_verification_seconds",
            "Full runtime of blob sidecars gossip verification",
        )
    });
pub static BLOB_SIDECAR_INCLUSION_PROOF_VERIFICATION: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "blob_sidecar_inclusion_proof_verification_seconds",
            "Time taken to verify blob sidecar inclusion proof",
        )
    });
pub static BLOB_SIDECAR_INCLUSION_PROOF_COMPUTATION: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "blob_sidecar_inclusion_proof_computation_seconds",
            "Time taken to compute blob sidecar inclusion proof",
        )
    });
pub static DATA_COLUMN_SIDECAR_COMPUTATION: LazyLock<Result<HistogramVec>> = LazyLock::new(|| {
    try_create_histogram_vec_with_buckets(
        "data_column_sidecar_computation_seconds",
        "Time taken to compute data column sidecar, including cells, proofs and inclusion proof",
        Ok(vec![0.1, 0.15, 0.25, 0.35, 0.5, 0.7, 1.0, 2.5, 5.0, 10.0]),
        &["blob_count"],
    )
});
pub static DATA_COLUMN_SIDECAR_INCLUSION_PROOF_VERIFICATION: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "data_column_sidecar_inclusion_proof_verification_seconds",
            "Time taken to verify data_column sidecar inclusion proof",
        )
    });
pub static DATA_COLUMN_SIDECAR_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_data_column_sidecar_processing_requests_total",
            "Count of all data column sidecars submitted for processing",
        )
    });
pub static DATA_COLUMN_SIDECAR_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_data_column_sidecar_processing_successes_total",
            "Number of data column sidecars verified for gossip",
        )
    });
pub static DATA_COLUMN_SIDECAR_GOSSIP_VERIFICATION_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_data_column_sidecar_gossip_verification_seconds",
            "Full runtime of data column sidecars gossip verification",
        )
    });
pub static DATA_COLUMNS_SIDECAR_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_blobs_column_sidecar_processing_successes_total",
            "Number of data column sidecars verified for gossip",
        )
    });

pub static BLOBS_FROM_EL_HIT_TOTAL: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_blobs_from_el_hit_total",
        "Number of blob batches fetched from the execution layer",
    )
});

pub static BLOBS_FROM_EL_MISS_TOTAL: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_blobs_from_el_miss_total",
        "Number of blob batches failed to fetch from the execution layer",
    )
});

pub static BLOBS_FROM_EL_EXPECTED_TOTAL: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_blobs_from_el_expected_total",
        "Number of blobs expected from the execution layer",
    )
});

pub static BLOBS_FROM_EL_RECEIVED_TOTAL: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_blobs_from_el_received_total",
        "Number of blobs fetched from the execution layer",
    )
});

/*
 * Light server message verification
 */
pub static FINALITY_UPDATE_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "light_client_finality_update_verification_success_total",
            "Number of light client finality updates verified for gossip",
        )
    });
/*
 * Light server message verification
 */
pub static OPTIMISTIC_UPDATE_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "light_client_optimistic_update_verification_success_total",
            "Number of light client optimistic updates verified for gossip",
        )
    });
/*
 * Aggregate subset metrics
 */
pub static SYNC_CONTRIBUTION_SUBSETS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_sync_contribution_subsets_total",
        "Count of new sync contributions that are subsets of already known aggregates",
    )
});
pub static AGGREGATED_ATTESTATION_SUBSETS: LazyLock<Result<IntCounter>> = LazyLock::new(|| {
    try_create_int_counter(
        "beacon_aggregated_attestation_subsets_total",
        "Count of new aggregated attestations that are subsets of already known aggregates",
    )
});
/*
 * Attestation simulator metrics
 */
pub static VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_HEAD_ATTESTER_HIT: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_HEAD_ATTESTER_HIT_TOTAL,
            "Incremented if a validator is flagged as a previous slot head attester \
        during per slot processing",
        )
    });
pub static VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_HEAD_ATTESTER_MISS: LazyLock<
    Result<IntCounter>,
> = LazyLock::new(|| {
    try_create_int_counter(
        VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_HEAD_ATTESTER_MISS_TOTAL,
        "Incremented if a validator is not flagged as a previous slot head attester \
        during per slot processing",
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_TARGET_ATTESTER_HIT: LazyLock<
    Result<IntCounter>,
> = LazyLock::new(|| {
    try_create_int_counter(
        VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_TARGET_ATTESTER_HIT_TOTAL,
        "Incremented if a validator is flagged as a previous slot target attester \
        during per slot processing",
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_TARGET_ATTESTER_MISS: LazyLock<
    Result<IntCounter>,
> = LazyLock::new(|| {
    try_create_int_counter(
        VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_TARGET_ATTESTER_MISS_TOTAL,
        "Incremented if a validator is not flagged as a previous slot target attester \
        during per slot processing",
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_SOURCE_ATTESTER_HIT: LazyLock<
    Result<IntCounter>,
> = LazyLock::new(|| {
    try_create_int_counter(
        VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_SOURCE_ATTESTER_HIT_TOTAL,
        "Incremented if a validator is flagged as a previous slot source attester \
        during per slot processing",
    )
});
pub static VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_SOURCE_ATTESTER_MISS: LazyLock<
    Result<IntCounter>,
> = LazyLock::new(|| {
    try_create_int_counter(
        VALIDATOR_MONITOR_ATTESTATION_SIMULATOR_SOURCE_ATTESTER_MISS_TOTAL,
        "Incremented if a validator is not flagged as a previous slot source attester \
        during per slot processing",
    )
});
/*
 * Missed block metrics
 */
pub static VALIDATOR_MONITOR_MISSED_BLOCKS_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "validator_monitor_missed_blocks_total",
            "Number of non-finalized blocks missed",
            &["validator"],
        )
    });

/*
 * Kzg related metrics
 */
pub static KZG_VERIFICATION_SINGLE_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "kzg_verification_single_seconds",
        "Runtime of single kzg verification",
    )
});
pub static KZG_VERIFICATION_BATCH_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "kzg_verification_batch_seconds",
        "Runtime of batched kzg verification",
    )
});
pub static KZG_VERIFICATION_DATA_COLUMN_SINGLE_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram_with_buckets(
            "kzg_verification_data_column_single_seconds",
            "Runtime of single data column kzg verification",
            Ok(vec![
                0.0005, 0.001, 0.0015, 0.002, 0.003, 0.004, 0.005, 0.007, 0.01, 0.02, 0.05,
            ]),
        )
    });
pub static KZG_VERIFICATION_DATA_COLUMN_BATCH_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram_with_buckets(
            "kzg_verification_data_column_batch_seconds",
            "Runtime of batched data column kzg verification",
            Ok(vec![
                0.002, 0.004, 0.006, 0.008, 0.01, 0.012, 0.015, 0.02, 0.03, 0.05, 0.07,
            ]),
        )
    });

pub static BLOCK_PRODUCTION_BLOBS_VERIFICATION_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(
    || {
        try_create_histogram(
            "beacon_block_production_blobs_verification_seconds",
            "Time taken to verify blobs against commitments and creating BlobSidecar objects in block production"
    )
    },
);
/*
 * Availability related metrics
 */
pub static BLOCK_AVAILABILITY_DELAY: LazyLock<Result<IntGauge>> = LazyLock::new(|| {
    try_create_int_gauge(
        "block_availability_delay",
        "Duration between start of the slot and the time at which all components of the block are available.",
    )
});

/*
 * Data Availability cache metrics
 */
pub static DATA_AVAILABILITY_OVERFLOW_MEMORY_BLOCK_CACHE_SIZE: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "data_availability_overflow_memory_block_cache_size",
            "Number of entries in the data availability overflow block memory cache.",
        )
    });
pub static DATA_AVAILABILITY_OVERFLOW_MEMORY_STATE_CACHE_SIZE: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "data_availability_overflow_memory_state_cache_size",
            "Number of entries in the data availability overflow state memory cache.",
        )
    });
pub static DATA_AVAILABILITY_OVERFLOW_STORE_CACHE_SIZE: LazyLock<Result<IntGauge>> =
    LazyLock::new(|| {
        try_create_int_gauge(
            "data_availability_overflow_store_cache_size",
            "Number of entries in the data availability overflow store cache.",
        )
    });
pub static DATA_AVAILABILITY_RECONSTRUCTION_TIME: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "data_availability_reconstruction_time_seconds",
            "Time taken to reconstruct columns",
        )
    });
pub static DATA_AVAILABILITY_RECONSTRUCTED_COLUMNS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "data_availability_reconstructed_columns_total",
            "Total count of reconstructed columns",
        )
    });

pub static KZG_DATA_COLUMN_RECONSTRUCTION_ATTEMPTS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "kzg_data_column_reconstruction_attempts",
            "Count of times data column reconstruction has been attempted",
        )
    });

pub static KZG_DATA_COLUMN_RECONSTRUCTION_FAILURES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "kzg_data_column_reconstruction_failures",
            "Count of times data column reconstruction has failed",
        )
    });

pub static KZG_DATA_COLUMN_RECONSTRUCTION_INCOMPLETE_TOTAL: LazyLock<Result<IntCounterVec>> =
    LazyLock::new(|| {
        try_create_int_counter_vec(
            "kzg_data_column_reconstruction_incomplete_total",
            "Count of times data column reconstruction attempts did not result in an import",
            &["reason"],
        )
    });

/*
 * light_client server metrics
 */
pub static LIGHT_CLIENT_SERVER_CACHE_STATE_DATA_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_light_client_server_cache_state_data_seconds",
            "Time taken to produce and cache state data",
        )
    });
pub static LIGHT_CLIENT_SERVER_CACHE_RECOMPUTE_UPDATES_TIMES: LazyLock<Result<Histogram>> =
    LazyLock::new(|| {
        try_create_histogram(
            "beacon_light_client_server_cache_recompute_updates_seconds",
            "Time taken to recompute and cache updates",
        )
    });
pub static LIGHT_CLIENT_SERVER_CACHE_PREV_BLOCK_CACHE_MISS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_light_client_server_cache_prev_block_cache_miss",
            "Count of prev block cache misses",
        )
    });

pub static LIGHT_CLIENT_SERVER_CACHE_PROCESSING_REQUESTS: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_light_client_server_cache_processing_requests",
            "Count of all requests to recompute and cache updates",
        )
    });

pub static LIGHT_CLIENT_SERVER_CACHE_PROCESSING_SUCCESSES: LazyLock<Result<IntCounter>> =
    LazyLock::new(|| {
        try_create_int_counter(
            "beacon_light_client_server_cache_processing_successes",
            "Count of all successful requests to recompute and cache updates",
        )
    });

/// Scrape the `beacon_chain` for metrics that are not constantly updated (e.g., the present slot,
/// head state info, etc) and update the Prometheus `DEFAULT_REGISTRY`.
pub fn scrape_for_metrics<T: BeaconChainTypes>(beacon_chain: &BeaconChain<T>) {
    let _ = beacon_chain.with_head(|head| {
        scrape_head_state(&head.beacon_state, head.beacon_state_root());
        Ok::<_, BeaconChainError>(())
    });

    if let Some(slot) = beacon_chain.slot_clock.now() {
        scrape_attestation_observation(slot, beacon_chain);
        scrape_sync_committee_observation(slot, beacon_chain);
    }

    let attestation_stats = beacon_chain.op_pool.attestation_stats();
    let chain_metrics = beacon_chain.metrics();

    // Kept duplicated for backwards compatibility
    set_gauge_by_usize(
        &BLOCK_PROCESSING_SNAPSHOT_CACHE_SIZE,
        beacon_chain.store.state_cache_len(),
    );

    set_gauge_by_usize(
        &BEACON_REQRESP_PRE_IMPORT_CACHE_SIZE,
        chain_metrics.reqresp_pre_import_cache_len,
    );

    let da_checker_metrics = beacon_chain.data_availability_checker.metrics();
    set_gauge_by_usize(
        &DATA_AVAILABILITY_OVERFLOW_MEMORY_BLOCK_CACHE_SIZE,
        da_checker_metrics.block_cache_size,
    );
    set_gauge_by_usize(
        &DATA_AVAILABILITY_OVERFLOW_MEMORY_STATE_CACHE_SIZE,
        da_checker_metrics.state_cache_size,
    );

    if let Some((size, num_lookups)) = beacon_chain.pre_finalization_block_cache.metrics() {
        set_gauge_by_usize(&PRE_FINALIZATION_BLOCK_CACHE_SIZE, size);
        set_gauge_by_usize(&PRE_FINALIZATION_BLOCK_LOOKUP_COUNT, num_lookups);
    }

    set_gauge_by_usize(
        &OP_POOL_NUM_ATTESTATIONS,
        attestation_stats.num_attestations,
    );
    set_gauge_by_usize(
        &OP_POOL_NUM_ATTESTATION_DATA,
        attestation_stats.num_attestation_data,
    );
    set_gauge_by_usize(
        &OP_POOL_MAX_AGGREGATES_PER_DATA,
        attestation_stats.max_aggregates_per_data,
    );
    set_gauge_by_usize(
        &OP_POOL_NUM_ATTESTER_SLASHINGS,
        beacon_chain.op_pool.num_attester_slashings(),
    );
    set_gauge_by_usize(
        &OP_POOL_NUM_PROPOSER_SLASHINGS,
        beacon_chain.op_pool.num_proposer_slashings(),
    );
    set_gauge_by_usize(
        &OP_POOL_NUM_VOLUNTARY_EXITS,
        beacon_chain.op_pool.num_voluntary_exits(),
    );
    set_gauge_by_usize(
        &OP_POOL_NUM_SYNC_CONTRIBUTIONS,
        beacon_chain.op_pool.num_sync_contributions(),
    );

    beacon_chain
        .validator_monitor
        .read()
        .scrape_metrics(&beacon_chain.slot_clock, &beacon_chain.spec);

    beacon_chain
        .canonical_head
        .fork_choice_read_lock()
        .scrape_for_metrics();

    beacon_chain.store.register_metrics();
}

/// Scrape the given `state` assuming it's the head state, updating the `DEFAULT_REGISTRY`.
fn scrape_head_state<E: EthSpec>(state: &BeaconState<E>, state_root: Hash256) {
    set_gauge_by_slot(&HEAD_STATE_SLOT, state.slot());
    set_gauge_by_slot(&HEAD_STATE_SLOT_INTEROP, state.slot());
    set_gauge_by_hash(&HEAD_STATE_ROOT, state_root);
    set_gauge_by_slot(
        &HEAD_STATE_LATEST_BLOCK_SLOT,
        state.latest_block_header().slot,
    );
    set_gauge_by_hash(
        &HEAD_STATE_CURRENT_JUSTIFIED_ROOT,
        state.current_justified_checkpoint().root,
    );
    set_gauge_by_epoch(
        &HEAD_STATE_CURRENT_JUSTIFIED_EPOCH,
        state.current_justified_checkpoint().epoch,
    );
    set_gauge_by_epoch(
        &HEAD_STATE_CURRENT_JUSTIFIED_EPOCH_INTEROP,
        state.current_justified_checkpoint().epoch,
    );
    set_gauge_by_hash(
        &HEAD_STATE_PREVIOUS_JUSTIFIED_ROOT,
        state.previous_justified_checkpoint().root,
    );
    set_gauge_by_epoch(
        &HEAD_STATE_PREVIOUS_JUSTIFIED_EPOCH,
        state.previous_justified_checkpoint().epoch,
    );
    set_gauge_by_epoch(
        &HEAD_STATE_PREVIOUS_JUSTIFIED_EPOCH_INTEROP,
        state.previous_justified_checkpoint().epoch,
    );
    set_gauge_by_hash(
        &HEAD_STATE_FINALIZED_ROOT,
        state.finalized_checkpoint().root,
    );
    set_gauge_by_epoch(
        &HEAD_STATE_FINALIZED_EPOCH,
        state.finalized_checkpoint().epoch,
    );
    set_gauge_by_epoch(
        &HEAD_STATE_FINALIZED_EPOCH_INTEROP,
        state.finalized_checkpoint().epoch,
    );
    set_gauge_by_usize(&HEAD_STATE_TOTAL_VALIDATORS, state.validators().len());
    set_gauge_by_u64(
        &HEAD_STATE_VALIDATOR_BALANCES,
        state.balances().iter().sum(),
    );
    set_gauge_by_u64(&HEAD_STATE_ETH1_DEPOSIT_INDEX, state.eth1_deposit_index());
    set_gauge_by_u64(
        &HEAD_STATE_ETH1_DEPOSITS_INTEROP,
        state.eth1_data().deposit_count,
    );
    set_gauge_by_usize(&HEAD_STATE_TOTAL_VALIDATORS, state.validators().len());
    set_gauge_by_u64(
        &HEAD_STATE_VALIDATOR_BALANCES,
        state.balances().iter().sum(),
    );

    let mut num_active: usize = 0;
    let mut num_slashed: usize = 0;
    let mut num_withdrawn: usize = 0;

    for v in state.validators() {
        if v.is_active_at(state.current_epoch()) {
            num_active += 1;
        }

        if v.slashed {
            num_slashed += 1;
        }

        if v.is_withdrawable_at(state.current_epoch()) {
            num_withdrawn += 1;
        }
    }

    set_gauge_by_usize(&HEAD_STATE_ACTIVE_VALIDATORS, num_active);
    set_gauge_by_usize(&HEAD_STATE_ACTIVE_VALIDATORS_INTEROP, num_active);
    set_gauge_by_usize(&HEAD_STATE_SLASHED_VALIDATORS, num_slashed);
    set_gauge_by_usize(&HEAD_STATE_WITHDRAWN_VALIDATORS, num_withdrawn);
}

fn scrape_attestation_observation<T: BeaconChainTypes>(slot_now: Slot, chain: &BeaconChain<T>) {
    let prev_epoch = slot_now.epoch(T::EthSpec::slots_per_epoch()) - 1;

    if let Some(count) = chain
        .observed_gossip_attesters
        .read()
        .observed_validator_count(prev_epoch)
    {
        set_gauge_by_usize(&ATTN_OBSERVATION_PREV_EPOCH_ATTESTERS, count);
    }

    if let Some(count) = chain
        .observed_aggregators
        .read()
        .observed_validator_count(prev_epoch)
    {
        set_gauge_by_usize(&ATTN_OBSERVATION_PREV_EPOCH_AGGREGATORS, count);
    }
}

fn scrape_sync_committee_observation<T: BeaconChainTypes>(slot_now: Slot, chain: &BeaconChain<T>) {
    let prev_slot = slot_now - 1;

    let contributors = chain.observed_sync_contributors.read();
    let mut contributor_sum = 0;
    for i in 0..SYNC_COMMITTEE_SUBNET_COUNT {
        if let Some(count) =
            contributors.observed_validator_count(SlotSubcommitteeIndex::new(prev_slot, i))
        {
            contributor_sum += count;
        }
    }
    drop(contributors);
    set_gauge_by_usize(&SYNC_COMM_OBSERVATION_PREV_SLOT_SIGNERS, contributor_sum);

    let sync_aggregators = chain.observed_sync_aggregators.read();
    let mut aggregator_sum = 0;
    for i in 0..SYNC_COMMITTEE_SUBNET_COUNT {
        if let Some(count) =
            sync_aggregators.observed_validator_count(SlotSubcommitteeIndex::new(prev_slot, i))
        {
            aggregator_sum += count;
        }
    }
    drop(sync_aggregators);
    set_gauge_by_usize(&SYNC_COMM_OBSERVATION_PREV_SLOT_AGGREGATORS, aggregator_sum);
}

fn set_gauge_by_slot(gauge: &Result<IntGauge>, value: Slot) {
    set_gauge(gauge, value.as_u64() as i64);
}

fn set_gauge_by_epoch(gauge: &Result<IntGauge>, value: Epoch) {
    set_gauge(gauge, value.as_u64() as i64);
}

fn set_gauge_by_hash(gauge: &Result<IntGauge>, value: Hash256) {
    set_gauge(gauge, value.to_low_u64_le() as i64);
}

fn set_gauge_by_usize(gauge: &Result<IntGauge>, value: usize) {
    set_gauge(gauge, value as i64);
}

fn set_gauge_by_u64(gauge: &Result<IntGauge>, value: u64) {
    set_gauge(gauge, value as i64);
}
