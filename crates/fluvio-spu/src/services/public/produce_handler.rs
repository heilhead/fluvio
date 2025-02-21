use std::io::{Error, ErrorKind};

use dataplane::batch::BatchRecords;
use fluvio::{Compression};
use fluvio_controlplane_metadata::topic::CompressionAlgorithm;
use fluvio_storage::StorageError;
use tracing::{debug, trace, error};
use tracing::instrument;

use dataplane::{ErrorCode, Isolation, Offset, RequestKind};
use dataplane::produce::{
    ProduceResponse, TopicProduceResponse, PartitionProduceResponse, PartitionProduceData,
    DefaultProduceRequest, DefaultTopicRequest,
};
use dataplane::api::RequestMessage;
use dataplane::api::ResponseMessage;
use dataplane::record::RecordSet;
use fluvio_controlplane_metadata::partition::ReplicaKey;
use tokio::select;
use std::time::Duration;
use fluvio_future::timer::sleep;

use crate::core::DefaultSharedGlobalContext;

struct TopicWriteResult {
    topic: String,
    partitions: Vec<PartitionWriteResult>,
}

#[derive(Default)]
struct PartitionWriteResult {
    replica_id: ReplicaKey,
    base_offset: Offset,
    leo: Offset,
    error_code: ErrorCode,
}

#[instrument(
    skip(request,ctx),
    fields(
        id = request.header.correlation_id(),
        client = %request.header.client_id()
    )
)]
pub async fn handle_produce_request(
    request: RequestMessage<DefaultProduceRequest>,
    ctx: DefaultSharedGlobalContext,
) -> Result<ResponseMessage<ProduceResponse>, Error> {
    let (header, produce_request) = request.get_header_request();
    trace!("Handling ProduceRequest: {:#?}", produce_request);

    let mut topic_results = Vec::with_capacity(produce_request.topics.len());
    for topic_request in produce_request.topics.into_iter() {
        let topic_result = handle_produce_topic(&ctx, topic_request).await;
        topic_results.push(topic_result);
    }
    wait_for_acks(
        produce_request.isolation,
        produce_request.timeout,
        &mut topic_results,
        &ctx,
    )
    .await;
    let response = into_response(topic_results);
    trace!("Returning ProduceResponse: {:#?}", &response);
    Ok(RequestMessage::<DefaultProduceRequest>::response_with_header(&header, response))
}

#[instrument(
    skip(ctx, topic_request),
    fields(topic = %topic_request.name),
)]
async fn handle_produce_topic(
    ctx: &DefaultSharedGlobalContext,
    topic_request: DefaultTopicRequest,
) -> TopicWriteResult {
    trace!("Handling produce request for topic:");

    let mut topic_result = TopicWriteResult {
        topic: topic_request.name,
        partitions: vec![],
    };
    for partition_request in topic_request.partitions.into_iter() {
        let replica_id = ReplicaKey::new(
            topic_result.topic.clone(),
            partition_request.partition_index,
        );
        let partition_response = handle_produce_partition(ctx, replica_id, partition_request).await;
        topic_result.partitions.push(partition_response);
    }
    topic_result
}

#[instrument(
    skip(ctx, replica_id, partition_request),
    fields(%replica_id),
)]
async fn handle_produce_partition<R: BatchRecords>(
    ctx: &DefaultSharedGlobalContext,
    replica_id: ReplicaKey,
    partition_request: PartitionProduceData<RecordSet<R>>,
) -> PartitionWriteResult {
    trace!("Handling produce request for partition:");

    let leader_state = match ctx.leaders_state().get(&replica_id).await {
        Some(leader_state) => leader_state,
        None => {
            debug!(%replica_id, "Replica not found");
            return PartitionWriteResult::error(replica_id, ErrorCode::NotLeaderForPartition);
        }
    };

    let replica_metadata = match ctx.replica_localstore().spec(&replica_id) {
        Some(replica_metadata) => replica_metadata,
        None => {
            error!(%replica_id, "Replica not found");
            return PartitionWriteResult::error(replica_id, ErrorCode::TopicNotFound);
        }
    };

    let mut records = partition_request.records;
    if validate_records(&records, replica_metadata.compression_type).is_err() {
        error!(%replica_id, "Compression in batch not supported by this topic");
        return PartitionWriteResult::error(replica_id, ErrorCode::CompressionError);
    }

    let write_result = leader_state
        .write_record_set(&mut records, ctx.follower_notifier())
        .await;

    match write_result {
        Ok((base_offset, leo)) => PartitionWriteResult::ok(replica_id, base_offset, leo),
        Err(err @ StorageError::BatchTooBig(_)) => {
            error!(%replica_id, "Batch is too big: {:#?}", err);
            PartitionWriteResult::error(replica_id, ErrorCode::MessageTooLarge)
        }
        Err(err) => {
            error!(%replica_id, "Error writing to replica: {:#?}", err);
            PartitionWriteResult::error(replica_id, ErrorCode::StorageError)
        }
    }
}

fn validate_records<R: BatchRecords>(
    records: &RecordSet<R>,
    compression: CompressionAlgorithm,
) -> Result<(), Error> {
    if records.batches.iter().all(|batch| {
        let batch_compression = if let Ok(compression) = batch.get_compression() {
            compression
        } else {
            return false;
        };
        match compression {
            CompressionAlgorithm::Any => true,
            CompressionAlgorithm::None => batch_compression == Compression::None,
            CompressionAlgorithm::Gzip => batch_compression == Compression::Gzip,
            CompressionAlgorithm::Snappy => batch_compression == Compression::Snappy,
            CompressionAlgorithm::Lz4 => batch_compression == Compression::Lz4,
        }
    }) {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Other,
            "Compression not supported by topic",
        ))
    }
}
/// For isolation = ReadCommitted wait until the replica's `hw` includes written records offsets or
/// until `timeout` passes. In case of timeout, the partition response returns `RequestTimedOut`
/// error code. The timeout is not shared between partitions.
///
/// For isolation = ReadUncommitted - it's no op.
async fn wait_for_acks(
    isolation: Isolation,
    timeout: Duration,
    results: &mut [TopicWriteResult],
    ctx: &DefaultSharedGlobalContext,
) {
    trace!(?isolation, "waiting for acks");
    match &isolation {
        Isolation::ReadCommitted => {
            for partition in results.iter_mut().flat_map(|r| r.partitions.iter_mut()) {
                if partition.error_code != ErrorCode::None {
                    trace!(?partition.replica_id, %partition.error_code, "partition result with error, skip waiting");
                    continue;
                }
                let leader_state = match ctx.leaders_state().get(&partition.replica_id).await {
                    Some(leader_state) => leader_state,
                    None => {
                        debug!(%partition.replica_id, "Replica not found");
                        partition.error_code = ErrorCode::NotLeaderForPartition;
                        continue;
                    }
                };
                let leo = partition.leo;
                if leader_state.hw().ge(&leo) {
                    trace!(?partition.replica_id, %leo, "batch already committed, skip waiting");
                    continue;
                }

                let mut listener = leader_state.offset_listener(&isolation);
                let wait_future = async {
                    loop {
                        let hw = listener.listen().await;
                        if hw.ge(&leo) {
                            break;
                        }
                    }
                };
                let timer = sleep(timeout);
                select! {
                    _ = wait_future => {
                        trace!(?partition.replica_id, "waiting for acks completed");
                    },
                    _ = timer => {
                        debug!(?partition.replica_id, "response timeout exceeded");
                        partition.error_code = ErrorCode::RequestTimedOut {
                            kind: RequestKind::Produce,
                            timeout_ms: timeout.as_millis() as u64
                        };
                    },
                }
            }
        }
        Isolation::ReadUncommitted => {}
    };
}

impl From<TopicWriteResult> for TopicProduceResponse {
    fn from(write_result: TopicWriteResult) -> Self {
        Self {
            name: write_result.topic,
            partitions: write_result
                .partitions
                .into_iter()
                .map(PartitionProduceResponse::from)
                .collect(),
        }
    }
}

impl PartitionWriteResult {
    fn error(replica_id: ReplicaKey, error_code: ErrorCode) -> Self {
        Self {
            replica_id,
            error_code,
            ..Default::default()
        }
    }

    fn ok(replica_id: ReplicaKey, base_offset: Offset, leo: Offset) -> Self {
        Self {
            replica_id,
            base_offset,
            leo,
            ..Default::default()
        }
    }
}

impl From<PartitionWriteResult> for PartitionProduceResponse {
    fn from(write_result: PartitionWriteResult) -> Self {
        Self {
            partition_index: write_result.replica_id.partition,
            error_code: write_result.error_code,
            base_offset: write_result.base_offset,
            ..Default::default()
        }
    }
}

fn into_response(topic_results: Vec<TopicWriteResult>) -> ProduceResponse {
    let responses = topic_results
        .into_iter()
        .map(TopicProduceResponse::from)
        .collect();
    ProduceResponse {
        responses,
        ..Default::default()
    }
}
