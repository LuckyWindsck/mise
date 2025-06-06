use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::Backend;
use crate::cli::args::BackendArg;
use crate::config::Config;
use crate::config::env_directive::{EnvResolveOptions, EnvResults};
use crate::config::settings::{SETTINGS, SettingsStatusMissingTools};
use crate::env::{PATH_KEY, TERM_WIDTH};
use crate::env_diff::EnvMap;
use crate::errors::Error;
use crate::hooks::Hooks;
use crate::install_context::InstallContext;
use crate::path_env::PathEnv;
use crate::registry::tool_enabled;
use crate::ui::multi_progress_report::MultiProgressReport;
use crate::uv;
use crate::{backend, config, env, hooks};
pub use builder::ToolsetBuilder;
use console::truncate_str;
use eyre::{Result, WrapErr};
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use outdated_info::OutdatedInfo;
pub use outdated_info::is_outdated_version;
use tokio::sync::OnceCell;
use tokio::{sync::Semaphore, task::JoinSet};
pub use tool_request::ToolRequest;
pub use tool_request_set::{ToolRequestSet, ToolRequestSetBuilder};
pub use tool_source::ToolSource;
pub use tool_version::{ResolveOptions, ToolVersion};
pub use tool_version_list::ToolVersionList;

mod builder;
pub(crate) mod install_state;
pub(crate) mod outdated_info;
pub(crate) mod tool_request;
mod tool_request_set;
mod tool_source;
mod tool_version;
mod tool_version_list;

#[derive(Debug, Default, Clone, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ToolVersionOptions {
    pub os: Option<Vec<String>>,
    pub install_env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub opts: BTreeMap<String, String>,
}

impl ToolVersionOptions {
    pub fn is_empty(&self) -> bool {
        self.install_env.is_empty() && self.opts.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.opts.get(key)
    }

    pub fn merge(&mut self, other: &BTreeMap<String, String>) {
        for (key, value) in other {
            self.opts
                .entry(key.to_string())
                .or_insert(value.to_string());
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.opts.contains_key(key)
    }
}

pub fn parse_tool_options(s: &str) -> ToolVersionOptions {
    let mut tvo = ToolVersionOptions::default();
    for opt in s.split(',') {
        let (k, v) = opt.split_once('=').unwrap_or((opt, ""));
        if k.is_empty() {
            continue;
        }
        tvo.opts.insert(k.to_string(), v.to_string());
    }
    tvo
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub force: bool,
    pub jobs: Option<usize>,
    pub raw: bool,
    /// only install missing tools if passed as arguments
    pub missing_args_only: bool,
    pub auto_install_disable_tools: Option<Vec<String>>,
    pub resolve_options: ResolveOptions,
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions {
            jobs: Some(SETTINGS.jobs),
            raw: SETTINGS.raw,
            force: false,
            missing_args_only: true,
            auto_install_disable_tools: SETTINGS.auto_install_disable_tools.clone(),
            resolve_options: Default::default(),
        }
    }
}

/// a toolset is a collection of tools for various plugins
///
/// one example is a .tool-versions file
/// the idea is that we start with an empty toolset, then
/// merge in other toolsets from various sources
#[derive(Debug, Default, Clone)]
pub struct Toolset {
    pub versions: IndexMap<Arc<BackendArg>, ToolVersionList>,
    pub source: Option<ToolSource>,
    tera_ctx: OnceCell<tera::Context>,
}

impl Toolset {
    pub fn new(source: ToolSource) -> Self {
        Self {
            source: Some(source),
            ..Default::default()
        }
    }
    pub fn add_version(&mut self, tvr: ToolRequest) {
        let ba = tvr.ba();
        if self.is_disabled(ba) {
            return;
        }
        let tvl = self
            .versions
            .entry(tvr.ba().clone())
            .or_insert_with(|| ToolVersionList::new(ba.clone(), self.source.clone().unwrap()));
        tvl.requests.push(tvr);
    }
    pub fn merge(&mut self, other: Toolset) {
        let mut versions = other.versions;
        for (plugin, tvl) in self.versions.clone() {
            if !versions.contains_key(&plugin) {
                versions.insert(plugin, tvl);
            }
        }
        versions.retain(|_, tvl| !self.is_disabled(&tvl.backend));
        self.versions = versions;
        self.source = other.source;
    }
    pub async fn resolve(&mut self) -> eyre::Result<()> {
        let config = Config::get().await;
        self.list_missing_plugins();
        let mut jset: JoinSet<Result<_>> = JoinSet::new();
        for (i, (ba, mut tvl)) in self.versions.clone().into_iter().enumerate() {
            let config = config.clone();
            jset.spawn(async move {
                tvl.resolve(&config, &Default::default()).await?;
                Ok((i, ba, tvl))
            });
        }
        let tvls = jset
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .sorted_by_key(|(i, _, _)| *i)
            .map(|(_, ba, tvl)| (ba, tvl))
            .collect::<Vec<_>>();
        for (ba, tvl) in tvls {
            self.versions.insert(ba, tvl);
        }
        Ok(())
    }
    pub async fn install_missing_versions(
        &mut self,
        config: &Arc<Config>,
        opts: &InstallOptions,
    ) -> Result<Vec<ToolVersion>> {
        let versions = self
            .list_missing_versions()
            .await
            .into_iter()
            .filter(|tv| {
                !opts.missing_args_only
                    || matches!(self.versions[tv.ba()].source, ToolSource::Argument)
            })
            .filter(|tv| {
                if let Some(tools) = &opts.auto_install_disable_tools {
                    !tools.contains(&tv.ba().short)
                } else {
                    true
                }
            })
            .map(|tv| tv.request)
            .collect_vec();
        let versions = self.install_all_versions(config, versions, opts).await?;
        if !versions.is_empty() {
            config::rebuild_shims_and_runtime_symlinks(&versions).await?;
        }
        Ok(versions)
    }

    pub fn list_missing_plugins(&self) -> Vec<String> {
        self.versions
            .iter()
            .filter(|(_, tvl)| {
                tvl.versions
                    .first()
                    .map(|tv| tv.request.is_os_supported())
                    .unwrap_or_default()
            })
            .map(|(ba, _)| ba)
            .flat_map(|ba| ba.backend())
            .filter(|b| b.plugin().is_some_and(|p| !p.is_installed()))
            .map(|p| p.id().into())
            .collect()
    }

    /// sets the options on incoming requests to install to whatever is already in the toolset
    /// this handles the use-case where you run `mise use ubi:cilium/cilium-cli` (without CLi options)
    /// but this tool has options inside mise.toml
    fn init_request_options(&self, requests: &mut Vec<ToolRequest>) {
        for tr in requests {
            // TODO: tr.options() probably should be Option<ToolVersionOptions>
            // to differentiate between no options and empty options
            // without that it might not be possible to unset the options if they are set
            if !tr.options().is_empty() {
                continue;
            }
            if let Some(tvl) = self.versions.get(tr.ba()) {
                if tvl.requests.len() != 1 {
                    // TODO: handle this case with multiple versions
                    continue;
                }
                let options = tvl.requests[0].options();
                tr.set_options(options);
            }
        }
    }

    pub async fn install_all_versions(
        &mut self,
        config: &Arc<Config>,
        mut versions: Vec<ToolRequest>,
        opts: &InstallOptions,
    ) -> Result<Vec<ToolVersion>> {
        if versions.is_empty() {
            return Ok(vec![]);
        }
        hooks::run_one_hook(self, Hooks::Preinstall, None).await;
        self.init_request_options(&mut versions);
        show_python_install_hint(&versions);
        let mut installed = vec![];
        let mut leaf_deps = get_leaf_dependencies(&versions)?;
        while !leaf_deps.is_empty() {
            if leaf_deps.len() < versions.len() {
                debug!("installing {} leaf tools first", leaf_deps.len());
            }
            versions.retain(|tr| !leaf_deps.contains(tr));
            installed.extend(self.install_some_versions(config, leaf_deps, opts).await?);
            leaf_deps = get_leaf_dependencies(&versions)?;
        }

        trace!("install: resolving");
        install_state::reset();
        if let Err(err) = self.resolve().await {
            debug!("error resolving versions after install: {err:#}");
        }
        if log::log_enabled!(log::Level::Debug) {
            for tv in installed.iter() {
                let backend = tv.backend()?;
                let bin_paths = backend
                    .list_bin_paths(tv)
                    .await
                    .map_err(|e| {
                        warn!("Error listing bin paths for {tv}: {e:#}");
                    })
                    .unwrap_or_default();
                debug!("[{tv}] list_bin_paths: {bin_paths:?}");
                let config = Config::get().await;
                let env = backend
                    .exec_env(&config, self, tv)
                    .await
                    .map_err(|e| {
                        warn!("Error running exec-env: {e:#}");
                    })
                    .unwrap_or_default();
                if !env.is_empty() {
                    debug!("[{tv}] exec_env: {env:?}");
                }
            }
        }
        hooks::run_one_hook(self, Hooks::Postinstall, None).await;
        Ok(installed)
    }

    async fn install_some_versions(
        &mut self,
        config: &Arc<Config>,
        versions: Vec<ToolRequest>,
        opts: &InstallOptions,
    ) -> Result<Vec<ToolVersion>> {
        debug!("install_some_versions: {}", versions.iter().join(" "));
        let queue: Vec<_> = versions
            .into_iter()
            .rev()
            .chunk_by(|v| v.ba().clone())
            .into_iter()
            .map(|(ba, v)| Ok((ba.backend()?, v.collect_vec())))
            .collect::<Result<_>>()?;
        for (backend, _) in &queue {
            if let Some(plugin) = backend.plugin() {
                if !plugin.is_installed() {
                    let mpr = MultiProgressReport::get();
                    plugin.ensure_installed(&mpr, false).await.or_else(|err| {
                        if let Some(&Error::PluginNotInstalled(_)) = err.downcast_ref::<Error>() {
                            Ok(())
                        } else {
                            Err(err)
                        }
                    })?;
                }
            }
        }
        let raw = opts.raw || SETTINGS.raw;
        let jobs = match raw {
            true => 1,
            false => opts.jobs.unwrap_or(SETTINGS.jobs),
        };
        let semaphore = Arc::new(Semaphore::new(jobs));
        let ts = Arc::new(self.clone());
        let mut tset: JoinSet<Result<Vec<ToolVersion>, eyre::Report>> = JoinSet::new();
        let opts = Arc::new(opts.clone());
        for (ba, trs) in queue {
            let ts = ts.clone();
            let semaphore = semaphore.clone();
            let opts = opts.clone();
            let ba = ba.clone();
            let config = config.clone();
            tset.spawn(async move {
                let _permit = semaphore.acquire().await?;
                let mpr = MultiProgressReport::get();
                let mut installed = vec![];
                for tr in trs {
                    let tv = tr.resolve(&config, &opts.resolve_options).await?;
                    let ctx = InstallContext {
                        ts: ts.clone(),
                        pr: mpr.add(&tv.style()),
                        force: opts.force,
                    };
                    let old_tv = tv.clone();
                    let tv = ba
                        .install_version(ctx, tv)
                        .await
                        .wrap_err_with(|| format!("failed to install {old_tv}"))?;
                    installed.push(tv);
                }
                Ok(installed)
            });
        }
        let mut installed = vec![];
        while let Some(res) = tset.join_next().await {
            installed.extend(res??);
        }
        installed.reverse();
        Ok(installed)
    }

    pub async fn list_missing_versions(&self) -> Vec<ToolVersion> {
        let config = Config::get().await;
        measure!("toolset::list_missing_versions", {
            self.list_current_versions()
                .into_iter()
                .filter(|(p, tv)| {
                    tv.request.is_os_supported() && !p.is_version_installed(&config, tv, true)
                })
                .map(|(_, tv)| tv)
                .collect()
        })
    }
    pub async fn list_installed_versions(&self) -> Result<Vec<TVTuple>> {
        let config = Config::get().await;
        let current_versions: HashMap<(String, String), TVTuple> = self
            .list_current_versions()
            .into_iter()
            .map(|(p, tv)| ((p.id().into(), tv.version.clone()), (p.clone(), tv)))
            .collect();
        let current_versions = Arc::new(current_versions);
        let mut jset: JoinSet<Result<_>> = JoinSet::new();
        for (i, b) in backend::list().into_iter().enumerate() {
            let current_versions = current_versions.clone();
            let config = config.clone();
            jset.spawn(async move {
                let mut versions = vec![];
                for v in b.list_installed_versions()? {
                    if let Some((p, tv)) = current_versions.get(&(b.id().into(), v.clone())) {
                        versions.push((p.clone(), tv.clone()));
                    }
                    let tv = ToolRequest::new(b.ba().clone(), &v, ToolSource::Unknown)?
                        .resolve(&config, &Default::default())
                        .await?;
                    versions.push((b.clone(), tv));
                }
                Ok((i, versions))
            });
        }
        let versions = jset
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .sorted_by_key(|(i, _)| *i)
            .flat_map(|(_, versions)| versions)
            .collect();
        Ok(versions)
    }
    pub fn list_current_requests(&self) -> Vec<&ToolRequest> {
        self.versions
            .values()
            .flat_map(|tvl| &tvl.requests)
            .collect()
    }
    pub fn list_versions_by_plugin(&self) -> Vec<(Arc<dyn Backend>, &Vec<ToolVersion>)> {
        self.versions
            .iter()
            .flat_map(|(ba, v)| eyre::Ok((ba.backend()?, &v.versions)))
            .collect()
    }
    pub fn list_current_versions(&self) -> Vec<(Arc<dyn Backend>, ToolVersion)> {
        self.list_versions_by_plugin()
            .iter()
            .flat_map(|(p, v)| {
                v.iter().map(|v| {
                    // map cargo backend specific prefixes to ref
                    let tv = match v.version.split_once(':') {
                        Some((ref_type @ ("tag" | "branch" | "rev"), r)) => {
                            let request = ToolRequest::Ref {
                                backend: p.ba().clone(),
                                ref_: r.to_string(),
                                ref_type: ref_type.to_string(),
                                options: v.request.options().clone(),
                                source: v.request.source().clone(),
                            };
                            let version = format!("ref:{r}");
                            ToolVersion::new(request, version)
                        }
                        _ => v.clone(),
                    };
                    (p.clone(), tv)
                })
            })
            .collect()
    }
    pub async fn list_all_versions(&self) -> Result<Vec<(Arc<dyn Backend>, ToolVersion)>> {
        let versions = self
            .list_current_versions()
            .into_iter()
            .chain(self.list_installed_versions().await?)
            .unique_by(|(ba, tv)| (ba.clone(), tv.tv_pathname().to_string()))
            .collect();
        Ok(versions)
    }
    pub fn list_current_installed_versions(
        &self,
        config: &Config,
    ) -> Vec<(Arc<dyn Backend>, ToolVersion)> {
        self.list_current_versions()
            .into_iter()
            .filter(|(p, v)| p.is_version_installed(config, v, true))
            .collect()
    }
    pub async fn list_outdated_versions(&self, bump: bool) -> Vec<OutdatedInfo> {
        let config = Config::get().await;
        let mut jset = JoinSet::new();
        for (i, (t, tv)) in self.list_current_versions().into_iter().enumerate() {
            let config = config.clone();
            jset.spawn(async move {
                match t.outdated_info(&tv, bump).await {
                    Ok(Some(oi)) => return Some((i, oi)),
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Error getting outdated info for {tv}: {e:#}");
                        return None;
                    }
                }
                if t.symlink_path(&tv).is_some() {
                    trace!("skipping symlinked version {tv}");
                    // do not consider symlinked versions to be outdated
                    return None;
                }
                OutdatedInfo::resolve(&config, tv.clone(), bump)
                    .await
                    .unwrap_or_else(|e| {
                        warn!("Error creating OutdatedInfo for {tv}: {e:#}");
                        None
                    })
                    .map(|oi| (i, oi))
            });
        }
        jset.join_all()
            .await
            .into_iter()
            .flatten()
            .sorted_by_key(|(i, _)| *i)
            .map(|(_, oi)| oi)
            .collect()
    }
    /// returns env_with_path but also with the existing env vars from the system
    pub async fn full_env(&self, config: &Config) -> Result<EnvMap> {
        let mut env = env::PRISTINE_ENV.clone().into_iter().collect::<EnvMap>();
        env.extend(self.env_with_path(config).await?.clone());
        Ok(env)
    }
    /// the full mise environment including all tool paths
    pub async fn env_with_path(&self, config: &Config) -> Result<EnvMap> {
        let (mut env, env_results) = self.final_env(config).await?;
        let mut path_env = PathEnv::from_iter(env::PATH.clone());
        for p in self.list_final_paths(config, env_results).await? {
            path_env.add(p.clone());
        }
        env.insert(PATH_KEY.to_string(), path_env.to_string());
        Ok(env)
    }
    pub async fn env_from_tools(&self) -> Vec<(String, String, String)> {
        let mut jset = JoinSet::new();
        let config = Config::get().await;
        for (i, (b, tv)) in self
            .list_current_installed_versions(&config)
            .into_iter()
            .enumerate()
        {
            if matches!(tv.request, ToolRequest::System { .. }) {
                continue;
            }
            let this = Arc::new(self.clone());
            jset.spawn(async move {
                let config = Config::get().await;
                match b.exec_env(&config, &this, &tv).await {
                    Ok(env) => env
                        .into_iter()
                        .map(|(k, v)| (i, k, v, b.id().to_string()))
                        .collect(),
                    Err(e) => {
                        warn!("Error running exec-env: {:#}", e);
                        Vec::new()
                    }
                }
            });
        }
        jset.join_all()
            .await
            .into_iter()
            .flatten()
            .sorted_by_key(|(i, _, _, _)| *i)
            .map(|(_, k, v, id)| (k, v, id))
            .filter(|(k, _, _)| k.to_uppercase() != "PATH")
            .collect()
    }
    async fn env(&self, config: &Config) -> Result<EnvMap> {
        time!("env start");
        let entries = self
            .env_from_tools()
            .await
            .into_iter()
            .map(|(k, v, _)| (k, v))
            .collect::<Vec<(String, String)>>();
        let add_paths = entries
            .iter()
            .filter(|(k, _)| k == "MISE_ADD_PATH" || k == "RTX_ADD_PATH")
            .map(|(_, v)| v)
            .join(":");
        let mut env: EnvMap = entries
            .into_iter()
            .filter(|(k, _)| k != "RTX_ADD_PATH")
            .filter(|(k, _)| k != "MISE_ADD_PATH")
            .filter(|(k, _)| !k.starts_with("RTX_TOOL_OPTS__"))
            .filter(|(k, _)| !k.starts_with("MISE_TOOL_OPTS__"))
            .rev()
            .collect();
        if !add_paths.is_empty() {
            env.insert(PATH_KEY.to_string(), add_paths);
        }
        env.extend(config.env().await?.clone());
        if let Some(venv) = uv::uv_venv().await {
            for (k, v) in venv.env {
                env.insert(k, v);
            }
        }
        time!("env end");
        Ok(env)
    }
    pub async fn final_env(&self, config: &Config) -> Result<(EnvMap, EnvResults)> {
        let mut env = self.env(config).await?;
        let mut tera_env = env::PRISTINE_ENV.clone().into_iter().collect::<EnvMap>();
        tera_env.extend(env.clone());
        let mut path_env = PathEnv::from_iter(env::PATH.clone());
        for p in self.list_paths().await {
            path_env.add(p);
        }
        for p in config.path_dirs().await?.clone() {
            path_env.add(p);
        }
        tera_env.insert(PATH_KEY.to_string(), path_env.to_string());
        let mut ctx = config.tera_ctx.clone();
        ctx.insert("env", &tera_env);
        let env_results = self.load_post_env(config, ctx, &tera_env).await?;
        env.extend(
            env_results
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.0.clone())),
        );
        Ok((env, env_results))
    }
    pub async fn list_paths(&self) -> Vec<PathBuf> {
        let config = Config::get().await;
        let mut jset = JoinSet::new();
        for (i, (p, tv)) in self
            .list_current_installed_versions(&config)
            .into_iter()
            .enumerate()
        {
            jset.spawn(async move {
                p.list_bin_paths(&tv)
                    .await
                    .unwrap_or_else(|e| {
                        warn!("Error listing bin paths for {tv}: {e:#}");
                        Vec::new()
                    })
                    .into_iter()
                    .map(|p| (i, p))
                    .collect::<Vec<_>>()
            });
        }

        jset.join_all()
            .await
            .into_iter()
            .flatten()
            .sorted_by_key(|(i, _)| *i)
            .map(|(_, path)| path)
            .filter(|p| p.parent().is_some()) // TODO: why?
            .collect()
    }
    /// same as list_paths but includes config.list_paths, venv paths, and MISE_ADD_PATHs from self.env()
    pub async fn list_final_paths(
        &self,
        config: &Config,
        env_results: EnvResults,
    ) -> Result<Vec<PathBuf>> {
        let mut paths = IndexSet::new();
        for p in config.path_dirs().await?.clone() {
            paths.insert(p);
        }
        if let Some(venv) = uv::uv_venv().await {
            paths.insert(venv.venv_path);
        }
        if let Some(path) = self.env(config).await?.get(&*PATH_KEY) {
            paths.insert(PathBuf::from(path));
        }
        for p in self.list_paths().await {
            paths.insert(p);
        }
        let mut path_env = PathEnv::from_iter(env::PATH.clone());
        for p in paths.clone().into_iter() {
            path_env.add(p);
        }
        // these are returned in order, but we need to run the post_env stuff last and then put the results in the front
        let paths = env_results.env_paths.into_iter().chain(paths).collect();
        Ok(paths)
    }
    pub async fn tera_ctx(&self) -> Result<&tera::Context> {
        self.tera_ctx
            .get_or_try_init(async || {
                let config = Config::try_get().await?;
                let env = self.full_env(&config).await?;
                let mut ctx = config.tera_ctx.clone();
                ctx.insert("env", &env);
                Ok(ctx)
            })
            .await
    }
    pub async fn which(&self, bin_name: &str) -> Option<(Arc<dyn Backend>, ToolVersion)> {
        let config = Config::get().await;
        for (p, tv) in self.list_current_installed_versions(&config) {
            match Box::pin(p.which(&tv, bin_name)).await {
                Ok(Some(_bin)) => return Some((p, tv)),
                Ok(None) => {}
                Err(e) => {
                    debug!("Error running which: {:#}", e);
                }
            }
        }
        None
    }
    pub async fn which_bin(&self, bin_name: &str) -> Option<PathBuf> {
        let (p, tv) = Box::pin(self.which(bin_name)).await?;
        Box::pin(p.which(&tv, bin_name)).await.ok().flatten()
    }
    pub async fn install_missing_bin(
        &mut self,
        config: &Arc<Config>,
        bin_name: &str,
    ) -> Result<Option<Vec<ToolVersion>>> {
        let mut plugins = IndexSet::new();
        for (p, tv) in self.list_current_installed_versions(config) {
            if let Ok(Some(_bin)) = p.which(&tv, bin_name).await {
                plugins.insert(p);
            }
        }
        for plugin in plugins {
            let versions = self
                .list_missing_versions()
                .await
                .into_iter()
                .filter(|tv| tv.ba() == &**plugin.ba())
                .filter(|tv| match &SETTINGS.auto_install_disable_tools {
                    Some(disable_tools) => !disable_tools.contains(&tv.ba().short),
                    None => true,
                })
                .map(|tv| tv.request)
                .collect_vec();
            if !versions.is_empty() {
                let versions = self
                    .install_all_versions(config, versions.clone(), &InstallOptions::default())
                    .await?;
                if !versions.is_empty() {
                    config::rebuild_shims_and_runtime_symlinks(&versions).await?;
                }
                return Ok(Some(versions));
            }
        }
        Ok(None)
    }

    pub async fn list_rtvs_with_bin(&self, bin_name: &str) -> Result<Vec<ToolVersion>> {
        let mut rtvs = vec![];
        for (p, tv) in self.list_installed_versions().await? {
            match p.which(&tv, bin_name).await {
                Ok(Some(_bin)) => rtvs.push(tv),
                Ok(None) => {}
                Err(e) => {
                    warn!("Error running which: {:#}", e);
                }
            }
        }
        Ok(rtvs)
    }

    // shows a warning if any versions are missing
    // only displays for tools which have at least one version already installed
    pub async fn notify_if_versions_missing(&self) {
        let missing = self
            .list_missing_versions()
            .await
            .into_iter()
            .filter(|tv| match SETTINGS.status.missing_tools() {
                SettingsStatusMissingTools::Never => false,
                SettingsStatusMissingTools::Always => true,
                SettingsStatusMissingTools::IfOtherVersionsInstalled => tv
                    .backend()
                    .is_ok_and(|b| b.list_installed_versions().is_ok_and(|f| !f.is_empty())),
            })
            .collect_vec();
        if missing.is_empty() || *env::__MISE_SHIM {
            return;
        }
        let versions = missing
            .iter()
            .map(|tv| tv.style())
            .collect::<Vec<_>>()
            .join(" ");
        warn!(
            "missing: {}",
            truncate_str(&versions, *TERM_WIDTH - 14, "…"),
        );
    }

    fn is_disabled(&self, ba: &BackendArg) -> bool {
        !ba.is_os_supported()
            || !tool_enabled(
                &SETTINGS.enable_tools(),
                &SETTINGS.disable_tools(),
                &ba.short.to_string(),
            )
    }

    async fn load_post_env(
        &self,
        config: &Config,
        ctx: tera::Context,
        env: &EnvMap,
    ) -> Result<EnvResults> {
        let entries = config
            .config_files
            .iter()
            .rev()
            .map(|(source, cf)| {
                cf.env_entries()
                    .map(|ee| ee.into_iter().map(|e| (e, source.clone())))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        // trace!("load_env: entries: {:#?}", entries);
        let env_results = EnvResults::resolve(
            config,
            ctx,
            env,
            entries,
            EnvResolveOptions {
                tools: true,
                ..Default::default()
            },
        )
        .await?;
        if log::log_enabled!(log::Level::Trace) {
            trace!("{env_results:#?}");
        } else if !env_results.is_empty() {
            debug!("{env_results:?}");
        }
        Ok(env_results)
    }
}

fn show_python_install_hint(versions: &[ToolRequest]) {
    let num_python = versions
        .iter()
        .filter(|tr| tr.ba().tool_name == "python")
        .count();
    if num_python != 1 {
        return;
    }
    hint!(
        "python_multi",
        "use multiple versions simultaneously with",
        "mise use python@3.12 python@3.11"
    );
}

impl Display for Toolset {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let plugins = &self
            .versions
            .iter()
            .map(|(_, v)| v.requests.iter().map(|tvr| tvr.to_string()).join(" "))
            .collect_vec();
        write!(f, "{}", plugins.join(", "))
    }
}

impl From<ToolRequestSet> for Toolset {
    fn from(trs: ToolRequestSet) -> Self {
        let mut ts = Toolset::default();
        for (ba, versions, source) in trs.into_iter() {
            ts.source = Some(source.clone());
            let mut tvl = ToolVersionList::new(ba.clone(), source);
            for tr in versions {
                tvl.requests.push(tr);
            }
            ts.versions.insert(ba, tvl);
        }
        ts
    }
}

fn get_leaf_dependencies(requests: &[ToolRequest]) -> eyre::Result<Vec<ToolRequest>> {
    // reverse maps potential shorts like "cargo-binstall" for "cargo:cargo-binstall"
    let versions_hash = requests
        .iter()
        .flat_map(|tr| tr.ba().all_fulls())
        .collect::<HashSet<_>>();
    let leaves = requests
        .iter()
        .map(|tr| {
            match tr.backend()?.get_all_dependencies(true)?.iter().all(|dep| {
                // dep is a dependency of tr so if it is in versions_hash (meaning it's also being installed) then it is not a leaf node
                !dep.all_fulls()
                    .iter()
                    .any(|full| versions_hash.contains(full))
            }) {
                true => Ok(Some(tr)),
                false => Ok(None),
            }
        })
        .flatten_ok()
        .map_ok(|tr| tr.clone())
        .collect::<Result<Vec<_>>>()?;
    Ok(leaves)
}

type TVTuple = (Arc<dyn Backend>, ToolVersion);

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use test_log::test;

    use super::ToolVersionOptions;
    #[test]
    fn test_tool_version_options() {
        let t = |input, f| {
            let opts = super::parse_tool_options(input);
            assert_eq!(opts, f);
        };
        t("", ToolVersionOptions::default());
        t(
            "exe=rg",
            ToolVersionOptions {
                opts: [("exe".to_string(), "rg".to_string())]
                    .iter()
                    .cloned()
                    .collect(),
                ..Default::default()
            },
        );
        t(
            "exe=rg,match=musl",
            ToolVersionOptions {
                opts: [
                    ("exe".to_string(), "rg".to_string()),
                    ("match".to_string(), "musl".to_string()),
                ]
                .iter()
                .cloned()
                .collect(),
                ..Default::default()
            },
        );
    }
}
