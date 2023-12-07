use super::{Task, Transaction};
use crate::{rocksdb::RocksDB, verify};
use anyhow::Context;
use prost::Message;
use proto_flow::capture::{request, response, Request, Response};
use proto_flow::flow;
use proto_flow::runtime::{
    capture_request_ext,
    capture_response_ext::{self, PollResult},
    CaptureRequestExt,
};
use std::collections::BTreeMap;

pub fn recv_unary(request: Request, response: Response) -> anyhow::Result<Response> {
    if request.spec.is_some() && response.spec.is_some() {
        Ok(response)
    } else if request.spec.is_some() {
        verify("connector", "Spec").fail(response)
    } else if request.discover.is_some() && response.discovered.is_some() {
        Ok(response)
    } else if request.discover.is_some() {
        verify("connector", "Discovered").fail(response)
    } else if request.validate.is_some() && response.validated.is_some() {
        Ok(response)
    } else if request.validate.is_some() {
        verify("connector", "Validated").fail(response)
    } else if request.apply.is_some() && response.applied.is_some() {
        Ok(response)
    } else if request.apply.is_some() {
        verify("connector", "Applied").fail(response)
    } else {
        verify("client", "unary request").fail(request)
    }
}

pub async fn recv_client_first_open(open: &Request) -> anyhow::Result<RocksDB> {
    let db = RocksDB::open(open.get_internal()?.open.and_then(|o| o.rocksdb_descriptor)).await?;

    Ok(db)
}

pub async fn recv_client_open(open: &mut Request, db: &RocksDB) -> anyhow::Result<()> {
    let Some(open) = open.open.as_mut() else {
        return verify("client", "Open").fail(open);
    };
    let Some(capture) = open.capture.as_mut() else {
        return verify("client", "Open.Capture").fail(open);
    };

    open.state_json = db
        .load_connector_state(
            models::RawValue::from_str(&open.state_json)
                .context("failed to parse initial open connector state")?,
        )
        .await?
        .into();

    // TODO(johnny): Switch to erroring if `state_key` is not already populated.
    for binding in capture.bindings.iter_mut() {
        binding.state_key = assemble::encode_state_key(&binding.resource_path, binding.backfill);
    }

    Ok(())
}

pub async fn recv_connector_opened(
    db: &RocksDB,
    open: &Request,
    opened: Option<Response>,
    shapes_by_key: &mut BTreeMap<String, doc::Shape>,
) -> anyhow::Result<(
    Task,
    Task,
    Vec<doc::Shape>,
    doc::combine::Accumulator,
    doc::combine::Accumulator,
    Response,
)> {
    let mut opened = verify("connecter", "Opened").not_eof(opened)?;

    let task = Task::new(&open, &opened)?;
    // Inferred document shapes, indexed by binding offset.
    let shapes = task.binding_shapes_by_index(std::mem::take(shapes_by_key));

    // Create a pair of accumulators. While one is draining, the other is accumulating.
    let a1 = doc::combine::Accumulator::new(task.combine_spec()?, tempfile::tempfile()?)?;
    let a2 = doc::combine::Accumulator::new(task.combine_spec()?, tempfile::tempfile()?)?;

    let checkpoint = db.load_checkpoint().await?;

    opened.set_internal(|internal| {
        internal.opened = Some(capture_response_ext::Opened {
            runtime_checkpoint: Some(checkpoint),
        })
    });

    Ok((task.clone(), task, shapes, a1, a2, opened))
}

pub fn send_client_poll_result(
    buf: &mut bytes::BytesMut,
    task: &Task,
    txn: &Transaction,
) -> (bool, Response) {
    let poll_result = if txn.checkpoints != 0 {
        PollResult::Ready
    } else if txn.connector_eof && !task.restart.elapsed().is_zero() {
        PollResult::Restart
    } else if txn.connector_eof {
        PollResult::CoolOff
    } else {
        PollResult::NotReady
    };

    (
        poll_result == PollResult::Ready,
        Response {
            checkpoint: Some(response::Checkpoint { state: None }),
            ..Default::default()
        }
        .with_internal_buf(buf, |internal| {
            internal.checkpoint = Some(capture_response_ext::Checkpoint {
                stats: None,
                poll_result: poll_result as i32,
            });
        }),
    )
}

pub fn send_connector_acknowledge(last_checkpoints: &mut u32, task: &Task) -> Option<Request> {
    if *last_checkpoints != 0 && task.explicit_acknowledgements {
        let checkpoints = *last_checkpoints;
        *last_checkpoints = 0; // Reset.

        Some(Request {
            acknowledge: Some(request::Acknowledge { checkpoints }),
            ..Default::default()
        })
    } else {
        None
    }
}

pub fn send_client_captured_or_checkpoint(
    buf: &mut bytes::BytesMut,
    drained: doc::combine::DrainedDoc,
    shapes: &mut [doc::Shape],
    task: &Task,
    txn: &mut Transaction,
    wb: &mut rocksdb::WriteBatch,
) -> Response {
    let doc::combine::DrainedDoc { meta, root } = drained;

    let index = meta.binding();

    if index == task.bindings.len() {
        // This is a merged checkpoint state update.
        let updated_json =
            serde_json::to_string(&doc::SerPolicy::default().on_owned(&root)).unwrap();

        tracing::debug!(
            state=%updated_json,
            "persisting updated connector state",
        );
        () = wb.merge(RocksDB::CONNECTOR_STATE_KEY, &updated_json);

        let state = flow::ConnectorState {
            merge_patch: true,
            updated_json,
        };
        return Response {
            checkpoint: Some(response::Checkpoint { state: Some(state) }),
            ..Default::default()
        };
    }

    let binding = &task.bindings[index];
    let key_packed = doc::Extractor::extract_all_owned(&root, &binding.key_extractors, buf);
    let partitions_packed =
        doc::Extractor::extract_all_owned(&root, &binding.partition_extractors, buf);
    let doc_json = serde_json::to_string(&binding.ser_policy.on_owned(&root))
        .expect("document serialization cannot fail");

    let stats = &mut txn.stats.entry(index as u32).or_default().1;
    stats.docs_total += 1;
    stats.bytes_total += doc_json.len() as u64;

    if shapes[index].widen_owned(&root) {
        doc::shape::limits::enforce_shape_complexity_limit(
            &mut shapes[index],
            doc::shape::limits::DEFAULT_SCHEMA_COMPLEXITY_LIMIT,
        );
        txn.updated_inferences.insert(index);
    }

    Response {
        captured: Some(response::Captured {
            binding: index as u32,
            doc_json,
        }),
        ..Default::default()
    }
    .with_internal_buf(buf, |internal| {
        internal.captured = Some(capture_response_ext::Captured {
            key_packed,
            partitions_packed,
        });
    })
}

pub fn send_client_final_checkpoint(
    buf: &mut bytes::BytesMut,
    task: &Task,
    txn: &Transaction,
) -> Response {
    let mut capture = BTreeMap::<String, ops::stats::Binding>::new();

    for (index, binding_stats) in txn.stats.iter() {
        let index = *index as usize;
        let entry = capture
            .entry(task.bindings[index].collection_name.clone())
            .or_default();

        ops::merge_docs_and_bytes(&binding_stats.0, &mut entry.right);
        ops::merge_docs_and_bytes(&binding_stats.1, &mut entry.out);
    }

    let stats = ops::Stats {
        capture,
        derive: None,
        interval: None,
        materialize: Default::default(),
        meta: Some(ops::Meta {
            uuid: crate::UUID_PLACEHOLDER.to_string(),
        }),
        open_seconds_total: txn.started_at.elapsed().unwrap().as_secs_f64(),
        shard: Some(task.shard_ref.clone()),
        timestamp: Some(proto_flow::as_timestamp(txn.started_at)),
        txn_count: 1,
    };

    Response {
        checkpoint: Some(response::Checkpoint { state: None }),
        ..Default::default()
    }
    .with_internal_buf(buf, |internal| {
        internal.checkpoint = Some(capture_response_ext::Checkpoint {
            stats: Some(stats),
            poll_result: PollResult::Invalid as i32,
        })
    })
}

pub async fn recv_client_start_commit(
    db: &RocksDB,
    request: Option<Request>,
    shapes: &[doc::Shape],
    task: &Task,
    txn: &Transaction,
    mut wb: rocksdb::WriteBatch,
) -> anyhow::Result<()> {
    let verify = verify("client", "StartCommit with runtime_checkpoint");
    let request = verify.not_eof(request)?;

    let CaptureRequestExt {
        start_commit:
            Some(capture_request_ext::StartCommit {
                runtime_checkpoint: Some(runtime_checkpoint),
                ..
            }),
        ..
    } = request.get_internal()?
    else {
        return verify.fail(request);
    };

    // Add the runtime checkpoint to our WriteBatch.
    tracing::debug!(
        checkpoint=?ops::DebugJson(&runtime_checkpoint),
        "persisting StartCommit.runtime_checkpoint",
    );
    wb.put(RocksDB::CHECKPOINT_KEY, &runtime_checkpoint.encode_to_vec());

    // We're about to write out our write batch which, when written to the
    // recovery log, irrevocably commits our transaction. Before doing so,
    // produce structured logs of all inferred schemas that have changed
    // in this transaction.
    for binding in txn.updated_inferences.iter() {
        let serialized = doc::shape::schema::to_schema(shapes[*binding].clone());

        tracing::info!(
            schema = ?ops::DebugJson(serialized),
            collection_name = %task.bindings[*binding].collection_name,
            binding = binding,
            "inferred schema updated"
        );
    }

    // Atomically write our commit batch.
    db.write_opt(wb, Default::default())
        .await
        .context("failed to write atomic RocksDB commit")?;

    Ok(())
}

pub fn send_client_started_commit() -> Response {
    Response {
        checkpoint: Some(response::Checkpoint { state: None }),
        ..Default::default()
    }
}

pub fn recv_connector_captured(
    accumulator: &mut doc::combine::Accumulator,
    captured: response::Captured,
    task: &Task,
    txn: &mut Transaction,
) -> anyhow::Result<()> {
    let response::Captured { binding, doc_json } = captured;

    let memtable = accumulator.memtable()?;
    let alloc = memtable.alloc();

    let mut doc = memtable
        .parse_json_str(&doc_json)
        .context("couldn't parse captured document as JSON")?;

    let uuid_ptr = &task
        .bindings
        .get(binding as usize)
        .with_context(|| "invalid captured binding {binding}")?
        .document_uuid_ptr;

    if !uuid_ptr.0.is_empty() {
        if let Some(node) = uuid_ptr.create_heap_node(&mut doc, alloc) {
            *node = doc::HeapNode::String(doc::BumpStr::from_str(crate::UUID_PLACEHOLDER, alloc));
        }
    }
    memtable.add(binding, doc, false)?;

    let stats = txn.stats.entry(binding).or_default();
    stats.0.docs_total += 1;
    stats.0.bytes_total += doc_json.len() as u64;

    txn.captured_bytes += doc_json.len();
    Ok(())
}

pub fn recv_connector_checkpoint(
    accumulator: &mut doc::combine::Accumulator,
    response: Response,
    task: &Task,
    txn: &mut Transaction,
) -> anyhow::Result<()> {
    let verify = verify("connector", "Captured or Checkpoint with state");
    let Some(response::Checkpoint { state: Some(state) }) = response.checkpoint else {
        return verify.fail(response);
    };
    let flow::ConnectorState {
        updated_json,
        merge_patch,
    } = state;

    let memtable = accumulator.memtable()?;
    let doc = memtable
        .parse_json_str(&updated_json)
        .context("couldn't parse connector state as JSON")?;

    // Combine over the checkpoint state.
    if !merge_patch {
        memtable.add(task.bindings.len() as u32, doc::HeapNode::Null, false)?;
    }
    memtable.add(task.bindings.len() as u32, doc, false)?;

    txn.checkpoints += 1;
    Ok(())
}
