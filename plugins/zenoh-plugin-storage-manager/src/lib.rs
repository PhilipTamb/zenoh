//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
#![recursion_limit = "512"]

use async_std::channel::Sender;
use async_std::sync::Arc;
use async_std::task;
use libloading::Library;
use memory_backend::create_memory_backend;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use storages_mgt::StorageMessage;
use zenoh::net::runtime::Runtime;
use zenoh::plugins::{Plugin, RunningPluginTrait, ValidationFunction, ZenohPlugin};
use zenoh::prelude::*;
use zenoh::Session;
use zenoh_backend_traits::CreateBackend;
use zenoh_backend_traits::CREATE_BACKEND_FN_NAME;
use zenoh_backend_traits::{config::*, Backend};
use zenoh_core::Result as ZResult;
use zenoh_core::{bail, zlock};
use zenoh_util::LibLoader;

mod backends_mgt;
use backends_mgt::*;
mod memory_backend;
mod storages_mgt;

zenoh_plugin_trait::declare_plugin!(StoragesPlugin);
pub struct StoragesPlugin {}
impl ZenohPlugin for StoragesPlugin {}
impl Plugin for StoragesPlugin {
    const STATIC_NAME: &'static str = "storage-manager";

    type StartArgs = Runtime;
    type RunningPlugin = zenoh::plugins::RunningPlugin;

    fn start(name: &str, runtime: &Self::StartArgs) -> ZResult<Self::RunningPlugin> {
        std::mem::drop(env_logger::try_init());
        let config =
            { PluginConfig::try_from((name, runtime.config.lock().plugin(name).unwrap())) }?;
        Ok(Box::new(StorageRuntime::from(StorageRuntimeInner::new(
            runtime.clone(),
            config,
        )?)))
    }
}
struct StorageRuntime(Arc<Mutex<StorageRuntimeInner>>);
struct StorageRuntimeInner {
    name: String,
    runtime: Runtime,
    session: Arc<Session>,
    lib_loader: LibLoader,
    volumes: HashMap<String, VolumeHandle>,
    storages: HashMap<String, HashMap<String, Sender<StorageMessage>>>,
}
impl StorageRuntimeInner {
    fn status_key(&self) -> String {
        format!(
            "/@/router/{}/status/plugins/{}",
            &self.runtime.pid, &self.name
        )
    }
    fn new(runtime: Runtime, config: PluginConfig) -> ZResult<Self> {
        // Try to initiate login.
        // Required in case of dynamic lib, otherwise no logs.
        // But cannot be done twice in case of static link.
        let _ = env_logger::try_init();
        let PluginConfig {
            name,
            backend_search_dirs,
            volumes,
            storages,
            ..
        } = config;
        let lib_loader = backend_search_dirs
            .map(|search_dirs| LibLoader::new(&search_dirs, false))
            .unwrap_or_default();

        let session = Arc::new(zenoh::init(runtime.clone()).wait().unwrap());
        let mut new_self = StorageRuntimeInner {
            name,
            runtime,
            session,
            lib_loader,
            volumes: Default::default(),
            storages: Default::default(),
        };
        new_self.spawn_volume(VolumeConfig {
            name: "memory".into(),
            backend: None,
            paths: None,
            required: false,
            rest: Default::default(),
        })?;
        new_self.update(
            volumes
                .into_iter()
                .map(ConfigDiff::AddVolume)
                .chain(storages.into_iter().map(ConfigDiff::AddStorage)),
        )?;
        Ok(new_self)
    }
    fn update<I: IntoIterator<Item = ConfigDiff>>(&mut self, diffs: I) -> ZResult<()> {
        for diff in diffs {
            match diff {
                ConfigDiff::DeleteVolume(volume) => self.kill_volume(volume),
                ConfigDiff::AddVolume(volume) => {
                    self.spawn_volume(volume)?;
                }
                ConfigDiff::DeleteStorage(config) => self.kill_storage(config),
                ConfigDiff::AddStorage(config) => self.spawn_storage(config)?,
            }
        }
        Ok(())
    }
    fn kill_volume(&mut self, volume: VolumeConfig) {
        if let Some(storages) = self.storages.remove(&volume.name) {
            async_std::task::block_on(futures::future::join_all(
                storages
                    .into_iter()
                    .map(|(_, s)| async move { s.send(StorageMessage::Stop).await }),
            ));
        }
        std::mem::drop(self.volumes.remove(&volume.name));
    }
    fn spawn_volume(&mut self, config: VolumeConfig) -> ZResult<()> {
        let volume_id = config.name.clone();
        if volume_id == MEMORY_BACKEND_NAME {
            match create_memory_backend(config) {
                Ok(backend) => {
                    self.volumes.insert(
                        volume_id,
                        VolumeHandle::new(backend, None, "<static-memory>".into()),
                    );
                }
                Err(e) => bail!("{}", e),
            }
        } else {
            match config.backend_search_method() {
                BackendSearchMethod::ByPaths(paths) => {
                    for path in paths {
                        unsafe {
                            if let Ok((lib, path)) = LibLoader::load_file(path) {
                                self.loaded_backend_from_lib(
                                    &volume_id,
                                    config.clone(),
                                    lib,
                                    path,
                                )?;
                                break;
                            }
                        }
                    }
                    bail!(
                        "Failed to find a suitable library for volume {} from paths: {:?}",
                        volume_id,
                        paths
                    );
                }
                BackendSearchMethod::ByName(backend_name) => unsafe {
                    let backend_filename = format!("{}{}", BACKEND_LIB_PREFIX, &backend_name);
                    if let Ok((lib, path)) = self.lib_loader.search_and_load(&backend_filename) {
                        self.loaded_backend_from_lib(&volume_id, config.clone(), lib, path)?;
                    } else {
                        bail!(
                            "Failed to find a suitable library for volume {} (was looking for <lib>{}<.so/.dll/.dylib>)",
                            volume_id,
                            &backend_filename
                        );
                    }
                },
            };
        };
        Ok(())
    }
    unsafe fn loaded_backend_from_lib(
        &mut self,
        volume_id: &str,
        config: VolumeConfig,
        lib: Library,
        lib_path: PathBuf,
    ) -> ZResult<()> {
        if let Ok(create_backend) = lib.get::<CreateBackend>(CREATE_BACKEND_FN_NAME) {
            match create_backend(config) {
                Ok(backend) => {
                    self.volumes.insert(
                        volume_id.to_string(),
                        VolumeHandle::new(
                            backend,
                            Some(lib),
                            lib_path.to_string_lossy().into_owned(),
                        ),
                    );
                    Ok(())
                }
                Err(e) => bail!(
                    "Failed to load Backend {} from {} : {}",
                    volume_id,
                    lib_path.display(),
                    e
                ),
            }
        } else {
            bail!(
                "Failed to instantiate volume {} from {} : function {}(VolumeConfig) not found in lib",
                volume_id,
                lib_path.display(),
                String::from_utf8_lossy(CREATE_BACKEND_FN_NAME)
            );
        }
    }
    fn kill_storage(&mut self, config: StorageConfig) {
        let volume = &config.volume_id;
        if let Some(storages) = self.storages.get_mut(volume) {
            if let Some(storage) = storages.get_mut(&config.name) {
                log::debug!("Closing storage {} from volume {}", config.name, volume);
                let _ = async_std::task::block_on(storage.send(StorageMessage::Stop));
            }
        }
    }
    fn spawn_storage(&mut self, storage: StorageConfig) -> ZResult<()> {
        let admin_key = self.status_key() + "/storages/" + &storage.name;
        let volume_id = storage.volume_id.clone();
        if let Some(backend) = self.volumes.get_mut(&volume_id) {
            let storage_name = storage.name.clone();
            let in_interceptor = backend.backend.incoming_data_interceptor();
            let out_interceptor = backend.backend.outgoing_data_interceptor();
            let stopper = async_std::task::block_on(create_and_start_storage(
                admin_key,
                storage,
                &mut backend.backend,
                in_interceptor,
                out_interceptor,
                self.session.clone(),
            ))?;
            self.storages
                .entry(volume_id)
                .or_default()
                .insert(storage_name, stopper);
            Ok(())
        } else {
            bail!("`{}` volume not found", volume_id)
        }
    }
}
struct VolumeHandle {
    backend: Box<dyn Backend>,
    _lib: Option<Library>,
    lib_path: String,
    stopper: Arc<AtomicBool>,
}
impl VolumeHandle {
    fn new(backend: Box<dyn Backend>, lib: Option<Library>, lib_path: String) -> Self {
        VolumeHandle {
            backend,
            _lib: lib,
            lib_path,
            stopper: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}
impl Drop for VolumeHandle {
    fn drop(&mut self) {
        self.stopper
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}
impl From<StorageRuntimeInner> for StorageRuntime {
    fn from(inner: StorageRuntimeInner) -> Self {
        StorageRuntime(Arc::new(Mutex::new(inner)))
    }
}

const GIT_VERSION: &str = git_version::git_version!(prefix = "v", cargo_prefix = "v");
impl RunningPluginTrait for StorageRuntime {
    fn config_checker(&self) -> ValidationFunction {
        let name = { zlock!(self.0).name.clone() };
        let runtime = self.0.clone();
        Arc::new(move |_path, old, new| {
            let old = PluginConfig::try_from((&name, old))?;
            let new = PluginConfig::try_from((&name, new))?;
            log::info!("old: {:?}", &old);
            log::info!("new: {:?}", &new);
            let diffs = ConfigDiff::diffs(old, new);
            log::info!("diff: {:?}", &diffs);
            { zlock!(runtime).update(diffs) }?;
            Ok(None)
        })
    }

    fn adminspace_getter<'a>(
        &'a self,
        selector: &'a Selector<'a>,
        plugin_status_key: &str,
    ) -> ZResult<Vec<zenoh::plugins::Response>> {
        let mut responses = Vec::new();
        let mut key = String::from(plugin_status_key);
        let key_selector = selector.key_selector.as_str();
        with_extended_string(&mut key, &["/version"], |key| {
            if zenoh::utils::key_expr::intersect(key, key_selector) {
                responses.push(zenoh::plugins::Response {
                    key: key.clone(),
                    value: GIT_VERSION.into(),
                })
            }
        });
        let guard = self.0.lock().unwrap();
        with_extended_string(&mut key, &["/volumes/"], |key| {
            for (volume_id, volume) in &guard.volumes {
                with_extended_string(key, &[volume_id], |key| {
                    with_extended_string(key, &["/__path__"], |key| {
                        if zenoh::utils::key_expr::intersect(key, key_selector) {
                            responses.push(zenoh::plugins::Response {
                                key: key.clone(),
                                value: volume.lib_path.clone().into(),
                            })
                        }
                    });
                    if zenoh::utils::key_expr::intersect(key, key_selector) {
                        responses.push(zenoh::plugins::Response {
                            key: key.clone(),
                            value: volume.backend.get_admin_status(),
                        })
                    }
                });
            }
        });
        with_extended_string(&mut key, &["/storages/"], |key| {
            for storages in guard.storages.values() {
                for (storage, handle) in storages {
                    with_extended_string(key, &[storage], |key| {
                        if zenoh::utils::key_expr::intersect(key, key_selector) {
                            if let Ok(value) = task::block_on(async {
                                let (tx, rx) = async_std::channel::bounded(1);
                                let _ = handle.send(StorageMessage::GetStatus(tx)).await;
                                rx.recv().await
                            }) {
                                responses.push(zenoh::plugins::Response {
                                    key: key.clone(),
                                    value,
                                })
                            }
                        }
                    })
                }
            }
        });
        Ok(responses)
    }
}

const BACKEND_LIB_PREFIX: &str = "zbackend_";
const MEMORY_BACKEND_NAME: &str = "memory";

fn with_extended_string<R, F: FnMut(&mut String) -> R>(
    prefix: &mut String,
    suffixes: &[&str],
    mut closure: F,
) -> R {
    let prefix_len = prefix.len();
    for suffix in suffixes {
        prefix.push_str(suffix);
    }
    let result = closure(prefix);
    prefix.truncate(prefix_len);
    result
}