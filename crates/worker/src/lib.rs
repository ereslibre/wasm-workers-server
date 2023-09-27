// Copyright 2022 VMware, Inc.
// SPDX-License-Identifier: Apache-2.0

pub mod config;
pub mod errors;
pub mod features;
pub mod io;
mod stdio;

use actix_web::HttpRequest;
use config::Config;
use errors::Result;
use features::{folders::Folder, wasi_nn::WASI_NN_BACKEND_OPENVINO};
use io::{WasmInput, WasmOutput};
use sha256::digest as sha256_digest;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::{collections::HashMap, path::Path};
use stdio::Stdio;
use wasmtime::{
    component::{self, Component},
    Config as WasmtimeConfig, Engine, Linker, Module, Store,
};
use wasmtime_wasi::{ambient_authority, preview2, Dir, WasiCtxBuilder};
use wasmtime_wasi_nn::{InMemoryRegistry, WasiNnCtx};
use wws_config::Config as ProjectConfig;
use wws_runtimes::{init_runtime, Runtime};

pub enum ModuleOrComponent {
    Module(Module),
    Component(Component),
}

/// A worker contains the engine and the associated runtime.
/// This struct will process requests by preparing the environment
/// with the runtime and running it in Wasmtime
pub struct Worker {
    /// Worker identifier
    pub id: String,
    /// Wasmtime engine to run this worker
    engine: Arc<Engine>,
    /// Wasm Module or Component
    module_or_component: ModuleOrComponent,
    /// Worker runtime
    runtime: Box<dyn Runtime + Sync + Send>,
    /// Current config
    pub config: Config,
    /// The worker filepath
    path: PathBuf,
}

#[derive(Default, Clone)]
struct Host {
    wasi_preview1_ctx: Option<wasmtime_wasi::WasiCtx>,
    wasi_preview2_ctx: Option<Arc<preview2::WasiCtx>>,

    // Resource table for preview2 if the `preview2_ctx` is in use, otherwise
    // "just" an empty table.
    wasi_preview2_table: Arc<preview2::Table>,

    // State necessary for the preview1 implementation of WASI backed by the
    // preview2 host implementation. Only used with the `--preview2` flag right
    // now when running core modules.
    wasi_preview2_adapter: Arc<preview2::preview1::WasiPreview1Adapter>,

    wasi_nn: Option<Arc<WasiNnCtx>>,
}

impl preview2::WasiView for Host {
    fn table(&self) -> &preview2::Table {
        &self.wasi_preview2_table
    }

    fn table_mut(&mut self) -> &mut preview2::Table {
        Arc::get_mut(&mut self.wasi_preview2_table)
            .expect("preview2 is not compatible with threads")
    }

    fn ctx(&self) -> &preview2::WasiCtx {
        self.wasi_preview2_ctx.as_ref().unwrap()
    }

    fn ctx_mut(&mut self) -> &mut preview2::WasiCtx {
        let ctx = self.wasi_preview2_ctx.as_mut().unwrap();
        Arc::get_mut(ctx).expect("preview2 is not compatible with threads")
    }
}

impl preview2::preview1::WasiPreview1View for Host {
    fn adapter(&self) -> &preview2::preview1::WasiPreview1Adapter {
        &self.wasi_preview2_adapter
    }

    fn adapter_mut(&mut self) -> &mut preview2::preview1::WasiPreview1Adapter {
        Arc::get_mut(&mut self.wasi_preview2_adapter)
            .expect("preview2 is not compatible with threads")
    }
}

impl Worker {
    /// Creates a new Worker
    pub fn new(project_root: &Path, path: &Path, project_config: &ProjectConfig) -> Result<Self> {
        // Compute the identifier
        let id = sha256_digest(project_root.join(path).to_string_lossy().as_bytes());

        // Load configuration
        let mut config_path = path.to_path_buf();
        config_path.set_extension("toml");
        let mut config = Config::default();

        if fs::metadata(&config_path).is_ok() {
            match Config::try_from_file(config_path) {
                Ok(c) => config = c,
                Err(e) => {
                    eprintln!("Error loading the worker configuration: {}", e);
                }
            }
        }

        let engine =
            Engine::new(WasmtimeConfig::default().async_support(true).wasm_component_model(true)).map_err(|err| {
                errors::WorkerError::ConfigureRuntimeError {
                    reason: format!("error creating engine ({err})"),
                }
            })?;
        let runtime = init_runtime(project_root, path, project_config)?;
        let bytes = runtime.module_bytes()?;

        let module_or_component = if let Ok(component) = Component::from_binary(&engine, &bytes) {
            ModuleOrComponent::Component(component)
        } else if let Ok(module) = Module::from_binary(&engine, &bytes) {
            ModuleOrComponent::Module(module)
        } else {
            return Err(errors::WorkerError::BadWasmModuleOrComponent);
        };

        // Prepare the environment if required
        runtime.prepare()?;

        Ok(Self {
            id,
            engine: engine.into(),
            module_or_component,
            runtime,
            config,
            path: path.to_path_buf(),
        })
    }

    fn prepare_preview1_ctx(
        &self,
        linker: &mut Linker<Host>,
        input: &[u8],
        vars: &[(String, String)],
        folders: &Vec<Folder>,
    ) -> Result<(Stdio, WasiCtxBuilder)> {
        // Add WASI
        wasmtime_wasi::add_to_linker(linker, |host| host.wasi_preview1_ctx.as_mut().unwrap())
            .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                reason: format!("error adding WASI to linker ({err})"),
            })?;

        // Create the initial WASI context
        let mut wasi_builder = WasiCtxBuilder::new();

        // Configure the stdio
        let stdio = Stdio::new(input);
        stdio.configure_wasi_ctx(Some(&mut wasi_builder), None);

        wasi_builder
            .envs(vars)
            .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                reason: format!("error adding environment variables ({err})"),
            })?;

        for folder in folders {
            if let Some(base) = &self.path.parent() {
                let dir = Dir::open_ambient_dir(base.join(&folder.from), ambient_authority())
                    .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                        reason: format!("error setting up ambient directories ({err})"),
                    })?;
                wasi_builder.preopened_dir(dir, &folder.to).map_err(|err| {
                    errors::WorkerError::ConfigureRuntimeError {
                        reason: format!("error preopening directories ({err})"),
                    }
                })?;
            } else {
                return Err(errors::WorkerError::FailedToInitialize);
            }
        }

        Ok((stdio, wasi_builder))
    }

    fn prepare_preview2_ctx<T: preview2::preview1::WasiPreview1View>(
        &self,
        linker: &mut Linker<T>,
        component_linker: &mut component::Linker<T>,
        input: &[u8],
        vars: &[(String, String)],
        folders: &Vec<Folder>,
    ) -> Result<(Stdio, preview2::WasiCtxBuilder)> {
        // Add WASI
        preview2::command::add_to_linker(component_linker).map_err(|err| {
            errors::WorkerError::ConfigureRuntimeError {
                reason: format!("error setting up WASI preview 2: {err}"),
            }
        })?;

        // Create the initial WASI context
        let mut wasi_builder = preview2::WasiCtxBuilder::new();

        // Configure the stdio
        let stdio = Stdio::new(input);
        stdio.configure_wasi_ctx(None, Some(&mut wasi_builder));

        for folder in folders {
            if let Some(base) = &self.path.parent() {
                let dir = Dir::open_ambient_dir(base.join(&folder.from), ambient_authority())
                    .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                        reason: format!("error setting up ambient directories ({err})"),
                    })?;
                wasi_builder.preopened_dir(
                    dir,
                    preview2::DirPerms::READ | preview2::DirPerms::MUTATE,
                    preview2::FilePerms::READ | preview2::FilePerms::WRITE,
                    &folder.to,
                );
            } else {
                return Err(errors::WorkerError::FailedToInitialize);
            }
        }

        Ok((stdio, wasi_builder))
    }

    pub async fn run(
        &self,
        request: &HttpRequest,
        body: &str,
        kv: Option<HashMap<String, String>>,
        vars: &HashMap<String, String>,
    ) -> Result<WasmOutput> {
        let input = serde_json::to_string(&WasmInput::new(request, body, kv)).unwrap();

        // Prepare environment variables
        let tuple_vars: Vec<(String, String)> =
            vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        // Mount folders from the configuration
        let preopened_folders = if let Some(folders) = self.config.folders.clone() {
            folders.clone()
        } else {
            Vec::new()
        };

        let mut module_linker: wasmtime::Linker<Host> = Linker::new(&self.engine);
        // let (stdio_preview1, mut wasi_preview1_ctx_builder) =
        //     self.prepare_preview1_ctx(&mut module_linker, &input, &tuple_vars, &preopened_folders)?;
        let mut component_linker: component::Linker<Host> = component::Linker::new(&self.engine);
        let (stdio_preview2, mut wasi_preview2_ctx_builder) = self.prepare_preview2_ctx(
            &mut module_linker,
            &mut component_linker,
            &input.as_bytes(),
            &tuple_vars,
            &preopened_folders,
        )?;

        // WASI-NN
        let allowed_backends = &self.config.features.wasi_nn.allowed_backends;
        if !allowed_backends.is_empty() {
            // For now, we only support OpenVINO
            if allowed_backends.len() != 1
                || !allowed_backends.contains(&WASI_NN_BACKEND_OPENVINO.to_string())
            {
                eprintln!("❌ The only WASI-NN supported backend name is \"{WASI_NN_BACKEND_OPENVINO}\". Please, update your config.");
                None
            } else {
                wasmtime_wasi_nn::wit::ML::add_to_linker(&mut component_linker, |s: &mut Host| {
                    Arc::get_mut(s.wasi_nn.as_mut().unwrap())
                        .expect("wasi-nn is not implemented with multi-threading support")
                })
                .map_err(|_| {
                    errors::WorkerError::RuntimeError(
                        wws_runtimes::errors::RuntimeError::WasiContextError,
                    )
                })?;

                Some(Arc::new(WasiNnCtx::new(
                    Vec::new(),
                    InMemoryRegistry::new().into(),
                )))
            }
        } else {
            None
        };

        let contents = match self.module_or_component {
            ModuleOrComponent::Module(ref module) => Vec::new(),
            ModuleOrComponent::Component(ref component) => {
                // Pass to the runtime to add any WASI specific requirement
                self.runtime
                    .prepare_wasi_ctx(&mut wasi_preview2_ctx_builder)?;

                let mut table = Arc::new(preview2::Table::default());
                let host = Host {
                    wasi_preview1_ctx: None,
                    wasi_preview2_ctx: Some(Arc::new(
                        wasi_preview2_ctx_builder
                            .build(Arc::get_mut(&mut table).unwrap())
                            .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                                reason: format!("error setting up WASI preview 2: {err}"),
                            })?,
                    )),
                    wasi_preview2_table: table,
                    ..Host::default()
                };

                let mut store = Store::new(&self.engine, host);

                let (command, _instance) =
                    preview2::command::Command::instantiate_async(&mut store, component, &component_linker)
                    .await
                    .unwrap();
                let result = command
                    .wasi_cli_run()
                    .call_run(&mut store)
                    .await
                    .unwrap();

                // let component_instance = component_linker.instantiate(&mut store, component)
                //     .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                //         reason: format!("error instantiating component")
                //     })?;
                // component_instance
                //     .call_run(&mut store);

                // component_linker
                //     .module(&mut store, "", module)
                //     .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                //         reason: format!("error instantiating module ({err})"),
                //     })?
                //     .get_default(&mut store, "")
                //     .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                //         reason: format!("error getting default export of module ({err})"),
                //     })?
                //     .typed::<(), ()>(&store)
                //     .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                //         reason: format!("error getting typed object ({err})"),
                //     })?
                //     .call(&mut store, ())
                //     .map_err(|err| errors::WorkerError::ConfigureRuntimeError {
                //         reason: format!("error calling function ({err})"),
                //     })?;

                // stdio_preview2
                //     .stdout2
                //     .clone()
                //     .try_into_inner()
                //     .unwrap_or_default()
                //     .to_vec()

                Vec::new()
            }
        };

        // Build the output
        let output: WasmOutput = serde_json::from_slice(&contents).map_err(|err| {
            errors::WorkerError::ConfigureRuntimeError {
                reason: format!("error building output ({err})"),
            }
        })?;

        Ok(output)
    }
}
