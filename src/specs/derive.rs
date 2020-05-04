use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::path::PathBuf;

/// Config is the initialization configuration of a derive worker.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    // Path to the catalog.
    pub catalog: PathBuf,
    // Collection which we're deriving.
    pub collection: String,
    // Unix domain socket to listen on for message transform
    // streams and key/value state operations.
    pub socket_path: PathBuf,
    // Configuration for the worker's persistent state
    pub state: State,
}

/// Recorder is configuration to enable recording to a recovery log.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct State {
    // Directory which roots the persistent state of this worker.
    pub dir: PathBuf,
    // Author under which new operations should be fenced and recorded to the log.
    pub author: u32,
    // FSM which details the persistent state manifest, including its recovery log.
    pub fsm: Box<RawValue>,
}

/// SourceEnvelope is read from the flow-consumer within derive transaction streams.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceEnvelope<'d> {
    #[serde(borrow, deserialize_with = "super::deserialize_cow_str")]
    pub collection: Cow<'d, str>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RawDocument<'d> {
    #[serde(borrow, flatten)]
    pub raw: &'d RawValue,
}

/// DerivedEnvelope is published to the flow-consumer within derive transaction streams.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DerivedEnvelope {
    // Logical partition to which this message will be written.
    // Does *not* include a final physical partition component (eg "part=123").
    // That must be determined by mapping the key hash onto existing physical partitions.
    pub partition: String,
    // Hash of the composite primary key of this message.
    pub key_hash: u64,
}
