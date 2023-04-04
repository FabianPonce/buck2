/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use buck2_core::env_helper::EnvHelper;
use buck2_critical_path::compute_critical_path_potentials;
use buck2_critical_path::GraphBuilder;
use buck2_data::BuildGraphExecutionInfo;
use buck2_data::CriticalPathEntry;
use buck2_data::ToProtoMessage;
use buck2_events::dispatch::instant_event;
use buck2_events::dispatch::with_dispatcher_async;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::metadata;
use derive_more::Display;
use derive_more::From;
use dice::UserComputationData;
use dupe::Dupe;
use dupe::OptionDupedExt;
use itertools::Itertools;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

use crate::actions::key::ActionKey;
use crate::actions::RegisteredAction;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::TransitiveSetProjectionKey;

pub struct ActionExecutionSignal {
    pub action: Arc<RegisteredAction>,
    pub duration: Duration,
}

pub struct TransitiveSetComputationSignal {
    pub key: TransitiveSetProjectionKey,
    pub artifacts: HashSet<ActionKey>,
    pub set_deps: HashSet<TransitiveSetProjectionKey>,
}

/// When dealing with dynamic outputs, we obtain an action key that'll result in us doing
/// (deferred) analysis and resolve to another action (the deferred action). We then go and execute
/// that action instead. Our predecessor map tracks action keys so we need to track this
/// redirection otherwise we'll have a broken link in our chain.
pub struct ActionRedirectionSignal {
    pub key: ActionKey,
    pub dest: ActionKey,
}

/* These signals are distinct from the main Buck event bus because some
 * analysis needs access to the entire build graph, and serializing the
 * entire build graph isn't feasible - therefore, we have these signals
 * with an unserializable but lightweight handle on a RegisteredAction.
 */
#[derive(From)]
pub enum BuildSignal {
    ActionExecution(ActionExecutionSignal),
    TransitiveSetComputation(TransitiveSetComputationSignal),
    ActionRedirection(ActionRedirectionSignal),
    BuildFinished,
}

#[derive(Clone, Dupe)]
pub struct BuildSignalSender {
    sender: Arc<UnboundedSender<BuildSignal>>,
}

impl BuildSignalSender {
    pub fn signal(&self, signal: impl Into<BuildSignal>) {
        let _ignore_error = self.sender.send(signal.into());
    }
}

#[derive(Clone, Dupe)]
struct CriticalPathNode<TKey: Eq, TValue> {
    /// The aggregated duration of this critical path.
    pub duration: Duration,
    /// The value of this node. If None, this node just won't be included when displaying.
    pub value: Option<TValue>,
    pub prev: Option<TKey>,
}

#[derive(Hash, Eq, PartialEq, Clone, Dupe, Debug, Display)]
pub enum NodeKey {
    ActionKey(ActionKey),
    TransitiveSetProjection(TransitiveSetProjectionKey),
}

pub struct BuildSignalReceiver<T> {
    receiver: UnboundedReceiverStream<BuildSignal>,
    backend: T,
}

fn extract_critical_path<TKey: Hash + Eq, TValue>(
    predecessors: &HashMap<TKey, CriticalPathNode<TKey, TValue>>,
) -> Vec<(&TKey, &Option<TValue>, Duration)> {
    let terminal = predecessors
        .iter()
        .max_by_key(|(_key, data)| data.duration)
        .map(|q| q.0);
    let mut path = itertools::unfold(terminal, |maybe_key| {
        if maybe_key.is_none() {
            return None;
        }
        let key = maybe_key.unwrap();
        let next = predecessors.get(key);
        *maybe_key = next.and_then(|q| (q.prev).as_ref());
        next.map(|x| (key, &x.value, x.duration))
    })
    .collect::<Vec<_>>();
    // Take differences of adjacent elements to recover action time from cumulative sum.
    path.reverse();
    for i in (1..path.len()).rev() {
        path[i].2 = path[i].2.saturating_sub(path[i - 1].2);
    }

    path
}

impl<T> BuildSignalReceiver<T>
where
    T: BuildListenerBackend,
{
    fn new(receiver: UnboundedReceiver<BuildSignal>, backend: T) -> Self {
        Self {
            receiver: UnboundedReceiverStream::new(receiver),
            backend,
        }
    }

    pub async fn run_and_log(mut self) -> anyhow::Result<()> {
        while let Some(event) = self.receiver.next().await {
            match event {
                BuildSignal::ActionExecution(execution) => self.process_action(execution)?,
                BuildSignal::TransitiveSetComputation(tset) => {
                    self.process_transitive_set_computation(tset)?
                }
                BuildSignal::ActionRedirection(redirection) => {
                    self.process_action_redirection(redirection)?
                }
                BuildSignal::BuildFinished => break,
            }
        }

        let BuildInfo {
            critical_path,
            num_nodes,
            num_edges,
        } = self.backend.finish()?;

        instant_event(BuildGraphExecutionInfo {
            critical_path,
            critical_path2: Vec::new(),
            metadata: metadata::collect(),
            num_nodes,
            num_edges,
            uses_total_duration: false,
        });
        Ok(())
    }

    fn process_action(&mut self, execution: ActionExecutionSignal) -> Result<(), anyhow::Error> {
        // Identify most costly predecessor.
        let inputs = execution.action.inputs()?;

        let dep_keys = inputs.iter().filter_map(|dep| match dep {
            ArtifactGroup::Artifact(artifact) => {
                artifact.action_key().duped().map(NodeKey::ActionKey)
            }
            ArtifactGroup::TransitiveSetProjection(key) => {
                Some(NodeKey::TransitiveSetProjection(key.dupe()))
            }
        });

        self.backend.process_node(
            NodeKey::ActionKey(execution.action.key().dupe()),
            Some(execution.action.dupe()),
            execution.duration,
            dep_keys,
        );

        Ok(())
    }

    fn process_action_redirection(
        &mut self,
        redirection: ActionRedirectionSignal,
    ) -> anyhow::Result<()> {
        self.backend.process_node(
            NodeKey::ActionKey(redirection.key),
            None,
            Duration::from_secs(0), // Those nodes don't carry a duration.
            std::iter::once(NodeKey::ActionKey(redirection.dest)),
        );

        Ok(())
    }

    fn process_transitive_set_computation(
        &mut self,
        set: TransitiveSetComputationSignal,
    ) -> anyhow::Result<()> {
        let artifacts = set.artifacts.into_iter().map(NodeKey::ActionKey);
        let sets = set
            .set_deps
            .into_iter()
            .map(NodeKey::TransitiveSetProjection);

        self.backend.process_node(
            NodeKey::TransitiveSetProjection(set.key),
            None,
            Duration::from_secs(0), // Those nodes don't carry a duration.
            artifacts.chain(sets),
        );

        Ok(())
    }
}

pub trait BuildListenerBackend {
    fn process_node(
        &mut self,
        key: NodeKey,
        value: Option<Arc<RegisteredAction>>,
        duration: Duration,
        dep_keys: impl Iterator<Item = NodeKey>,
    );

    fn finish(self) -> anyhow::Result<BuildInfo>;
}

pub struct BuildInfo {
    critical_path: Vec<CriticalPathEntry>,
    num_nodes: u64,
    num_edges: u64,
}

struct DefaultBackend {
    predecessors: HashMap<NodeKey, CriticalPathNode<NodeKey, Arc<RegisteredAction>>>,
    num_nodes: u64,
    num_edges: u64,
}

impl DefaultBackend {
    fn new() -> Self {
        Self {
            predecessors: HashMap::new(),
            num_nodes: 0,
            num_edges: 0,
        }
    }
}

impl BuildListenerBackend for DefaultBackend {
    fn process_node(
        &mut self,
        key: NodeKey,
        value: Option<Arc<RegisteredAction>>,
        duration: Duration,
        dep_keys: impl Iterator<Item = NodeKey>,
    ) {
        let longest_ancestor = dep_keys
            .unique()
            .filter_map(|node_key| {
                self.num_edges += 1;
                let node_data = self.predecessors.get(&node_key)?;
                Some((node_key, node_data))
            })
            .max_by_key(|d| d.1.duration);

        let node = match longest_ancestor {
            Some((key, data)) => CriticalPathNode {
                prev: Some(key.dupe()),
                value,
                duration: data.duration + duration,
            },
            None => CriticalPathNode {
                prev: None,
                value,
                duration,
            },
        };

        self.num_nodes += 1;
        self.predecessors.insert(key, node.dupe());
    }

    fn finish(self) -> anyhow::Result<BuildInfo> {
        let critical_path = extract_critical_path(&self.predecessors)
            .into_iter()
            .filter_map(|(_key, maybe_action, duration)| {
                let action = maybe_action.as_ref()?;
                if duration == Duration::ZERO {
                    return None;
                }
                let name = format!(
                    "{} {}{}",
                    action.owner(),
                    action.category(),
                    action
                        .identifier()
                        .map_or_else(|| "".to_owned(), |v| format!("[{}]", v))
                );
                Some((name, duration, action))
            })
            .map(|(name, duration, action)| {
                anyhow::Ok(CriticalPathEntry {
                    action_name: name,
                    action_key: Some(action.key().as_proto()),
                    duration: Some(duration.try_into()?),
                    action_name_fields: Some(buck2_data::ActionName {
                        category: action.category().to_string(),
                        identifier: action
                            .identifier()
                            .map_or_else(|| "".to_owned(), |i| i.to_owned()),
                    }),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(BuildInfo {
            critical_path,
            num_nodes: self.num_nodes,
            num_edges: self.num_edges,
        })
    }
}

/// An implementation of critical path that uses a longest-paths graph in order to produce
/// potential savings in addition to the critical path.
struct LongestPathGraphBackend {
    builder: anyhow::Result<GraphBuilder<NodeKey, NodeData>>,
}

struct NodeData {
    action: Option<Arc<RegisteredAction>>,
    duration: Duration,
}

impl LongestPathGraphBackend {
    fn new() -> Self {
        Self {
            builder: Ok(GraphBuilder::new()),
        }
    }
}

impl BuildListenerBackend for LongestPathGraphBackend {
    fn process_node(
        &mut self,
        key: NodeKey,
        action: Option<Arc<RegisteredAction>>,
        duration: Duration,
        dep_keys: impl Iterator<Item = NodeKey>,
    ) {
        let builder = match self.builder.as_mut() {
            Ok(b) => b,
            Err(..) => return,
        };

        let res = builder.push(key, dep_keys, NodeData { action, duration });

        match res {
            Ok(()) => {}
            Err(e) => self.builder = Err(e.into()),
        }
    }

    fn finish(self) -> anyhow::Result<BuildInfo> {
        let (graph, keys, data) = self.builder?.finish();
        drop(keys);

        let durations = data.try_map_ref(|d| {
            d.duration
                .as_micros()
                .try_into()
                .context("Duration `as_micros()` exceeds u64")
        })?;

        let (critical_path, _critical_path_cost, _potentials) =
            compute_critical_path_potentials(&graph, &durations)?;

        drop(durations);

        let critical_path = critical_path
            .values()
            .filter_map(|idx| {
                let data = &data[*idx];
                let action = data.action.as_ref()?;

                let name = format!(
                    "{} {}{}",
                    action.owner(),
                    action.category(),
                    action
                        .identifier()
                        .map_or_else(|| "".to_owned(), |v| format!("[{}]", v))
                );

                Some((name, data.duration, action))
            })
            .map(|(name, duration, action)| {
                anyhow::Ok(CriticalPathEntry {
                    action_name: name,
                    action_key: Some(action.key().as_proto()),
                    duration: Some(duration.try_into()?),
                    action_name_fields: Some(buck2_data::ActionName {
                        category: action.category().to_string(),
                        identifier: action
                            .identifier()
                            .map_or_else(|| "".to_owned(), |i| i.to_owned()),
                    }),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(BuildInfo {
            critical_path,
            num_nodes: graph.vertices_count() as _,
            num_edges: graph.edges_count() as _,
        })
    }
}

pub trait SetBuildSignals {
    fn set_build_signals(&mut self, sender: BuildSignalSender);
}

impl SetBuildSignals for UserComputationData {
    fn set_build_signals(&mut self, sender: BuildSignalSender) {
        self.data.set(sender);
    }
}

pub trait HasBuildSignals {
    fn get_build_signals(&self) -> Option<&BuildSignalSender>;
}

impl HasBuildSignals for UserComputationData {
    fn get_build_signals(&self) -> Option<&BuildSignalSender> {
        self.data.get::<BuildSignalSender>().ok()
    }
}

fn start_listener(
    events: EventDispatcher,
    backend: impl BuildListenerBackend + Send + 'static,
) -> (BuildSignalSender, JoinHandle<anyhow::Result<()>>) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let sender = BuildSignalSender {
        sender: Arc::new(sender),
    };

    let listener = BuildSignalReceiver::new(receiver, backend);
    let receiver_task_handle = tokio::spawn(with_dispatcher_async(events.dupe(), async move {
        listener.run_and_log().await
    }));

    (sender, receiver_task_handle)
}

/// Creates a Build Listener signal pair and invokes the given asynchronous function with the send-end of the signal
/// sender.
///
/// Build listeners in this module operate by creating a matched pair of signal senders and signal receivers. Senders
/// are Dupe and allow for arbitrarily many writeres. Receivers are not Dupe and are expected to be driven by a single
/// thread. This implies that, in order for the receiver to function correctly and dispatch to build listeners, it must
/// be run in a backround task that is periodically polled.
///
/// This function arranges for a background task to be spawned that drives the receiver, while invoking the called
/// function with a live BuildSignalSender that can be used to send events to the listening receiver. Upon return of
/// `scope`, the sender terminates the receiver by sending a `BuildFinished` signal and joins the receiver task.
pub async fn scope<F, R, Fut>(events: EventDispatcher, func: F) -> anyhow::Result<R>
where
    F: FnOnce(BuildSignalSender) -> Fut,
    Fut: Future<Output = anyhow::Result<R>>,
{
    static USE_LONGEST_PATH_GRAPH: EnvHelper<bool> = EnvHelper::new("BUCK2_USE_LONGEST_PATH_GRAPH");
    let use_longest_path_graph = USE_LONGEST_PATH_GRAPH.get_copied()?.unwrap_or_default();

    let (sender, handle) = if use_longest_path_graph {
        start_listener(events, LongestPathGraphBackend::new())
    } else {
        start_listener(events, DefaultBackend::new())
    };
    let result = func(sender.dupe()).await;
    sender.signal(BuildSignal::BuildFinished);
    handle.await??;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    type CriticalPathMap = HashMap<i32, CriticalPathNode<i32, i32>>;

    fn cp_insert(
        predecessors: &mut CriticalPathMap,
        key: i32,
        prev: Option<i32>,
        duration: Duration,
    ) {
        predecessors.insert(
            key,
            CriticalPathNode {
                duration,
                value: Some(key),
                prev,
            },
        );
    }
    #[test]
    fn empty_path() {
        let predecessors = CriticalPathMap::new();
        assert_eq!(extract_critical_path(&predecessors), vec![]);
    }

    #[test]
    fn unit_path() {
        let mut predecessors = CriticalPathMap::new();
        cp_insert(&mut predecessors, 1, None, Duration::from_secs(3));
        assert_eq!(
            extract_critical_path(&predecessors),
            vec![(&1, &Some(1), Duration::from_secs(3))],
        );
    }

    #[test]
    fn long_path() {
        let mut predecessors = HashMap::new();
        /*   -> 1 -> 2 -> 3
         *   5s   6s   7s
         *
         *      1 -> 4
         *        9s
         */
        cp_insert(&mut predecessors, 1, None, Duration::from_secs(5));
        cp_insert(&mut predecessors, 2, Some(1), Duration::from_secs(11));
        cp_insert(&mut predecessors, 3, Some(2), Duration::from_secs(18));
        cp_insert(&mut predecessors, 4, Some(1), Duration::from_secs(14));
        assert_eq!(
            extract_critical_path(&predecessors),
            vec![
                (&1, &Some(1), Duration::from_secs(5)),
                (&2, &Some(2), Duration::from_secs(6)),
                (&3, &Some(3), Duration::from_secs(7)),
            ],
        );
    }
}
