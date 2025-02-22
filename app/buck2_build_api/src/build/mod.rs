/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use buck2_artifact::artifact::artifact_type::BaseArtifactKind;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_cli_proto::build_request::Materializations;
use buck2_core::configuration::compatibility::MaybeCompatible;
use buck2_core::execution_types::executor_config::PathSeparatorKind;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_events::dispatch::console_message;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use dice::DiceComputations;
use dice::UserComputationData;
use dupe::Dupe;
use futures::future;
use futures::stream::BoxStream;
use futures::stream::FuturesUnordered;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::FutureExt;
use itertools::Itertools;
use tokio::sync::Mutex;

use crate::actions::artifact::get_artifact_fs::GetArtifactFs;
use crate::actions::artifact::materializer::ArtifactMaterializer;
use crate::analysis::calculation::RuleAnalysisCalculation;
use crate::artifact_groups::calculation::ArtifactGroupCalculation;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::build_signals::HasBuildSignals;
use crate::interpreter::rule_defs::cmd_args::AbsCommandLineContext;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use crate::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use crate::interpreter::rule_defs::provider::builtin::run_info::FrozenRunInfo;
use crate::interpreter::rule_defs::provider::test_provider::TestProvider;

mod graph_size;

/// The types of provider to build on the configured providers label
#[derive(Debug, Clone, Dupe, Allocative)]
pub enum BuildProviderType {
    Default,
    DefaultOther,
    Run,
    Test,
}

#[derive(Clone, Debug, Allocative)]
pub struct ConfiguredBuildTargetResultGen<T> {
    pub outputs: Vec<T>,
    pub run_args: Option<Vec<String>>,
    pub target_rule_type_name: Option<String>,
    pub configured_graph_size: Option<buck2_error::Result<MaybeCompatible<u64>>>,
    pub errors: Vec<buck2_error::Error>,
}

pub type ConfiguredBuildTargetResult =
    ConfiguredBuildTargetResultGen<buck2_error::Result<ProviderArtifacts>>;

pub struct BuildTargetResult {
    pub configured: BTreeMap<ConfiguredProvidersLabel, Option<ConfiguredBuildTargetResult>>,
    /// Errors that could not be associated with a specific configured target. These errors may be
    /// associated with a providers label, or might not be associated with any target at all.
    pub other_errors: BTreeMap<Option<ProvidersLabel>, Vec<buck2_error::Error>>,
}

impl BuildTargetResult {
    pub async fn collect_stream(
        mut stream: impl Stream<Item = BuildEvent> + Unpin,
        fail_fast: bool,
    ) -> anyhow::Result<Self> {
        // Create a map of labels to outputs, but retain the expected index of each output.
        let mut res = HashMap::<
            ConfiguredProvidersLabel,
            Option<ConfiguredBuildTargetResultGen<(usize, buck2_error::Result<ProviderArtifacts>)>>,
        >::new();
        let mut other_errors = BTreeMap::<_, Vec<_>>::new();

        while let Some(event) = stream.next().await {
            let ConfiguredBuildEvent { variant, label } = match event {
                BuildEvent::Configured(variant) => variant,
                BuildEvent::OtherError { label: target, err } => {
                    other_errors.entry(target).or_default().push(err);
                    continue;
                }
            };
            match variant {
                ConfiguredBuildEventVariant::SkippedIncompatible => {
                    res.entry((*label).clone()).or_insert(None);
                }
                ConfiguredBuildEventVariant::Prepared {
                    run_args,
                    target_rule_type_name,
                } => {
                    res.entry((*label).clone())
                        .or_insert(Some(ConfiguredBuildTargetResultGen {
                            outputs: Vec::new(),
                            run_args,
                            target_rule_type_name: Some(target_rule_type_name),
                            configured_graph_size: None,
                            errors: Vec::new(),
                        }));
                }
                ConfiguredBuildEventVariant::Output { index, output } => {
                    let is_err = output.is_err();

                    res.get_mut(label.as_ref())
                        .with_context(|| format!("BuildEventVariant::Output before BuildEventVariant::Prepared for {} (internal error)", label))?
                        .as_mut()
                        .with_context(|| format!("BuildEventVariant::Output for a skipped target: `{}` (internal error)", label))?
                        .outputs
                        .push((index, output));

                    if is_err && fail_fast {
                        break;
                    }
                }
                ConfiguredBuildEventVariant::GraphSize {
                    configured_graph_size,
                } => {
                    res.get_mut(label.as_ref())
                        .with_context(|| format!("BuildEventVariant::GraphSize before BuildEventVariant::Prepared for {} (internal error)", label))?
                        .as_mut()
                        .with_context(|| format!("BuildEventVariant::GraphSize for a skipped target: `{}` (internal error)", label))?
                        .configured_graph_size = Some(configured_graph_size);
                }
                ConfiguredBuildEventVariant::Error { err } => {
                    res.entry((*label).clone())
                        .or_insert(Some(ConfiguredBuildTargetResultGen {
                            outputs: Vec::new(),
                            run_args: None,
                            target_rule_type_name: None,
                            configured_graph_size: None,
                            errors: Vec::new(),
                        }))
                        .as_mut()
                        .unwrap()
                        .errors
                        .push(err);
                    if fail_fast {
                        break;
                    }
                }
            }
        }

        // Sort our outputs within each individual BuildTargetResult, then return those.
        // Also, turn our HashMap into a BTreeMap.
        let res = res
            .into_iter()
            .map(|(label, result)| {
                let result = result.map(|result| {
                    let ConfiguredBuildTargetResultGen {
                        mut outputs,
                        run_args,
                        target_rule_type_name,
                        configured_graph_size,
                        errors,
                    } = result;

                    // No need for a stable sort: the indices are unique (see below).
                    outputs.sort_unstable_by_key(|(index, _outputs)| *index);

                    // TODO: This whole building thing needs quite a bit of refactoring. We might
                    // request the same targets multiple times here, but since we know that
                    // ConfiguredTargetLabel -> Output is going to be deterministic, we just dedupe
                    // them using the index.
                    ConfiguredBuildTargetResult {
                        outputs: outputs
                            .into_iter()
                            .unique_by(|(index, _outputs)| *index)
                            .map(|(_index, outputs)| outputs)
                            .collect(),
                        run_args,
                        target_rule_type_name,
                        configured_graph_size,
                        errors,
                    }
                });

                (label, result)
            })
            .collect();

        Ok(Self {
            configured: res,
            other_errors,
        })
    }
}

enum ConfiguredBuildEventVariant {
    SkippedIncompatible,
    Prepared {
        run_args: Option<Vec<String>>,
        target_rule_type_name: String,
    },
    Output {
        output: buck2_error::Result<ProviderArtifacts>,
        /// Ensure a stable ordering of outputs.
        index: usize,
    },
    GraphSize {
        configured_graph_size: buck2_error::Result<MaybeCompatible<u64>>,
    },
    Error {
        /// An error that can't be associated with a single artifact.
        err: buck2_error::Error,
    },
}

/// Events to be accumulated using BuildTargetResult::collect_stream.
pub struct ConfiguredBuildEvent {
    label: Arc<ConfiguredProvidersLabel>,
    variant: ConfiguredBuildEventVariant,
}

pub enum BuildEvent {
    Configured(ConfiguredBuildEvent),
    // An error that cannot be associated with a specific configured target
    OtherError {
        label: Option<ProvidersLabel>,
        err: buck2_error::Error,
    },
}

#[derive(Copy, Clone, Dupe, Debug)]
pub struct BuildConfiguredLabelOptions {
    pub skippable: bool,
    pub want_configured_graph_size: bool,
}

pub async fn build_configured_label<'a>(
    ctx: &'a DiceComputations,
    materialization_context: &MaterializationContext,
    providers_label: ConfiguredProvidersLabel,
    providers_to_build: &ProvidersToBuild,
    opts: BuildConfiguredLabelOptions,
) -> BoxStream<'a, ConfiguredBuildEvent> {
    let providers_label = Arc::new(providers_label);
    build_configured_label_inner(
        ctx,
        materialization_context,
        providers_label.clone(),
        providers_to_build,
        opts,
    )
    .await
    .unwrap_or_else(|e| {
        futures::stream::once(futures::future::ready(ConfiguredBuildEvent {
            label: providers_label,
            variant: ConfiguredBuildEventVariant::Error { err: e.into() },
        }))
        .boxed()
    })
}

async fn build_configured_label_inner<'a>(
    ctx: &'a DiceComputations,
    materialization_context: &MaterializationContext,
    providers_label: Arc<ConfiguredProvidersLabel>,
    providers_to_build: &ProvidersToBuild,
    opts: BuildConfiguredLabelOptions,
) -> anyhow::Result<BoxStream<'a, ConfiguredBuildEvent>> {
    let artifact_fs = ctx.get_artifact_fs().await?;

    let (outputs, run_args, target_rule_type_name) = {
        // A couple of these objects aren't Send and so scope them here so async transform doesn't get concerned.
        let providers = match ctx.get_providers(providers_label.as_ref()).await? {
            MaybeCompatible::Incompatible(reason) => {
                if opts.skippable {
                    console_message(reason.skipping_message(providers_label.target()));
                    return Ok(futures::stream::once(futures::future::ready(
                        ConfiguredBuildEvent {
                            label: providers_label.dupe(),
                            variant: ConfiguredBuildEventVariant::SkippedIncompatible,
                        },
                    ))
                    .boxed());
                } else {
                    return Err(reason.to_err());
                }
            }
            MaybeCompatible::Compatible(v) => v,
        };

        // Important we use an an ordered collections, so the order matches the order the rule
        // author wrote.
        let mut outputs = Vec::new();
        // Providers that produced each output, in the order of outputs above. We use a separate collection
        // otherwise we'd build the same output twice when it's both in DefaultInfo and RunInfo
        let collection = providers.provider_collection();

        let mut run_args: Option<Vec<String>> = None;

        if providers_to_build.default {
            collection
                .default_info()
                .for_each_default_output_artifact_only(&mut |o| {
                    outputs.push((ArtifactGroup::Artifact(o), BuildProviderType::Default));
                    Ok(())
                })?;
        }
        if providers_to_build.default_other {
            collection
                .default_info()
                .for_each_default_output_other_artifacts_only(&mut |o| {
                    outputs.push((o, BuildProviderType::DefaultOther));
                    Ok(())
                })?;
            // TODO(marwhal): We can remove this once we migrate all other outputs to be handled with Artifacts directly
            collection.default_info().for_each_other_output(&mut |o| {
                outputs.push((o, BuildProviderType::DefaultOther));
                Ok(())
            })?;
        }
        if providers_to_build.run {
            if let Some(runinfo) = providers
                .provider_collection()
                .builtin_provider::<FrozenRunInfo>()
            {
                let mut artifact_visitor = SimpleCommandLineArtifactVisitor::new();
                runinfo.visit_artifacts(&mut artifact_visitor)?;
                for input in artifact_visitor.inputs {
                    outputs.push((input, BuildProviderType::Run));
                }
                // Produce arguments to run on a local machine.
                let path_separator = if cfg!(windows) {
                    PathSeparatorKind::Windows
                } else {
                    PathSeparatorKind::Unix
                };
                let executor_fs = ExecutorFs::new(&artifact_fs, path_separator);
                let mut cli = Vec::<String>::new();
                let mut ctx = AbsCommandLineContext::new(&executor_fs);
                runinfo.add_to_command_line(&mut cli, &mut ctx)?;
                run_args = Some(cli);
            }
        }
        if providers_to_build.tests {
            if let Some(test_provider) = <dyn TestProvider>::from_collection(collection) {
                let mut artifact_visitor = SimpleCommandLineArtifactVisitor::new();
                test_provider.visit_artifacts(&mut artifact_visitor)?;
                for input in artifact_visitor.inputs {
                    outputs.push((input, BuildProviderType::Test));
                }
            }
        }

        let target_rule_type_name: String = ctx
            .get_configured_target_node(providers_label.target())
            .await?
            .require_compatible()?
            .rule_type()
            .name()
            .to_owned();

        (outputs, run_args, target_rule_type_name)
    };

    if let Some(signals) = ctx.per_transaction_data().get_build_signals() {
        signals.top_level_target(
            providers_label.target().dupe(),
            outputs
                .iter()
                .map(|(output, _type)| output.dupe())
                .collect(),
        );
    }

    if !opts.skippable && outputs.is_empty() {
        let docs = "https://buck2.build/docs/users/faq/common_issues/#why-does-my-target-not-have-any-outputs"; // @oss-enable
        // @oss-disable: let docs = "https://www.internalfb.com/intern/staticdocs/buck2/docs/users/faq/common_issues/#why-does-my-target-not-have-any-outputs";
        console_message(format!(
            "Target {} does not have any outputs. This means the rule did not define any outputs. See {} for more information",
            providers_label.target(),
            docs,
        ));
    }

    let outputs = outputs
        .into_iter()
        .enumerate()
        .map({
            |(index, (output, provider_type))| {
                let materialization_context = materialization_context.dupe();
                materialize_artifact_group_owned(ctx, output, materialization_context).map(
                    move |res| {
                        let res =
                            res.map_err(buck2_error::Error::from)
                                .map(|values| ProviderArtifacts {
                                    values,
                                    provider_type,
                                });

                        (index, res)
                    },
                )
            }
        })
        .collect::<FuturesUnordered<_>>()
        .map({
            let providers_label = providers_label.dupe();
            move |(index, output)| ConfiguredBuildEvent {
                label: providers_label.dupe(),
                variant: ConfiguredBuildEventVariant::Output { index, output },
            }
        });

    let stream = futures::stream::once(futures::future::ready(ConfiguredBuildEvent {
        label: providers_label.dupe(),
        variant: ConfiguredBuildEventVariant::Prepared {
            run_args,
            target_rule_type_name,
        },
    }))
    .chain(outputs);

    if opts.want_configured_graph_size {
        let stream = stream.chain(futures::stream::once(async move {
            let configured_graph_size =
                graph_size::get_configured_graph_size(ctx, providers_label.target())
                    .await
                    .map_err(|e| e.into());

            ConfiguredBuildEvent {
                label: providers_label,
                variant: ConfiguredBuildEventVariant::GraphSize {
                    configured_graph_size,
                },
            }
        }));

        Ok(stream.boxed())
    } else {
        Ok(stream.boxed())
    }
}
pub async fn materialize_artifact_group_owned(
    ctx: &DiceComputations,
    artifact_group: ArtifactGroup,
    materialization_context: MaterializationContext,
) -> anyhow::Result<ArtifactGroupValues> {
    materialize_artifact_group(ctx, &artifact_group, &materialization_context).await
}

#[derive(Clone, Allocative)]
pub struct ProviderArtifacts {
    pub values: ArtifactGroupValues,
    pub provider_type: BuildProviderType,
}

// what type of artifacts to build based on the provider it came from
#[derive(Default, Clone)]
pub struct ProvidersToBuild {
    pub default: bool,
    pub default_other: bool,
    pub run: bool,
    pub tests: bool,
}

impl Debug for ProviderArtifacts {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderArtifacts")
            .field("values", &self.values.iter().collect::<Vec<_>>())
            .field("provider_type", &self.provider_type)
            .finish()
    }
}

pub async fn materialize_artifact_group(
    ctx: &DiceComputations,
    artifact_group: &ArtifactGroup,
    materialization_context: &MaterializationContext,
) -> anyhow::Result<ArtifactGroupValues> {
    let values = ctx.ensure_artifact_group(artifact_group).await?;

    if let MaterializationContext::Materialize { map, force } = materialization_context {
        future::try_join_all(values.iter().filter_map(|(artifact, _value)| {
            match artifact.as_parts().0 {
                BaseArtifactKind::Build(artifact) => {
                    match map.entry(artifact.dupe()) {
                        Entry::Vacant(v) => {
                            // Ensure we won't request this artifact elsewhere, and proceed to request
                            // it.
                            v.insert(());
                        }
                        Entry::Occupied(..) => {
                            // We've already requested this artifact, no use requesting it again.
                            return None;
                        }
                    }

                    Some(ctx.try_materialize_requested_artifact(artifact, *force))
                }
                BaseArtifactKind::Source(..) => None,
            }
        }))
        .await
        .context("Failed to materialize artifacts")?;
    }

    Ok(values)
}

#[derive(Clone, Dupe)]
pub enum MaterializationContext {
    Skip,
    Materialize {
        /// This map contains all the artifacts that we enqueued for materialization. This ensures
        /// we don't enqueue the same thing more than once.
        map: Arc<DashMap<BuildArtifact, ()>>,
        /// Whether we should force the materialization of requested artifacts, or defer to the
        /// config.
        force: bool,
    },
}

impl MaterializationContext {
    /// Create a new MaterializationContext that will force all materializations.
    pub fn force_materializations() -> Self {
        Self::Materialize {
            map: Arc::new(DashMap::new()),
            force: true,
        }
    }
}

pub trait ConvertMaterializationContext {
    fn from(self) -> MaterializationContext;

    fn with_existing_map(self, map: &Arc<DashMap<BuildArtifact, ()>>) -> MaterializationContext;
}

impl ConvertMaterializationContext for Materializations {
    fn from(self) -> MaterializationContext {
        match self {
            Materializations::Skip => MaterializationContext::Skip,
            Materializations::Default => MaterializationContext::Materialize {
                map: Arc::new(DashMap::new()),
                force: false,
            },
            Materializations::Materialize => MaterializationContext::Materialize {
                map: Arc::new(DashMap::new()),
                force: true,
            },
        }
    }

    fn with_existing_map(self, map: &Arc<DashMap<BuildArtifact, ()>>) -> MaterializationContext {
        match self {
            Materializations::Skip => MaterializationContext::Skip,
            Materializations::Default => MaterializationContext::Materialize {
                map: map.dupe(),
                force: false,
            },
            Materializations::Materialize => MaterializationContext::Materialize {
                map: map.dupe(),
                force: true,
            },
        }
    }
}

pub trait HasCreateUnhashedSymlinkLock {
    fn set_create_unhashed_symlink_lock(&mut self, lock: Arc<Mutex<()>>);

    fn get_create_unhashed_symlink_lock(&self) -> Arc<Mutex<()>>;
}

impl HasCreateUnhashedSymlinkLock for UserComputationData {
    fn set_create_unhashed_symlink_lock(&mut self, lock: Arc<Mutex<()>>) {
        self.data.set(lock);
    }

    fn get_create_unhashed_symlink_lock(&self) -> Arc<Mutex<()>> {
        self.data
            .get::<Arc<Mutex<()>>>()
            .expect("Lock for creating unhashed symlinks should be set")
            .dupe()
    }
}
