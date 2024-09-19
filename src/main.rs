use anyhow::anyhow;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use deno_core::JsRuntime;

use deno_ast::{MediaType, ParseParams};
use deno_core::*;

use deno_permissions::PermissionsContainer;
use deno_permissions::{Permissions, PermissionsOptions};
use deno_runtime::worker::MainWorker;
use deno_runtime::worker::WorkerOptions;

use deno_core::anyhow::{bail, Error};
use deno_core::futures::FutureExt;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::{resolve_import, ModuleSourceCode, RequestedModuleType, ResolutionKind};

pub struct NetworkModuleLoader;

impl ModuleLoader for NetworkModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, Error> {
        Ok(resolve_import(specifier, referrer)?)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleSpecifier>,
        _is_dyn_import: bool,
        requested_module_type: RequestedModuleType,
    ) -> ModuleLoadResponse {
        let module_specifier = module_specifier.clone();

        ModuleLoadResponse::Async(
            async move {
                let mut redirect_module_url = None;
                let code = match module_specifier.scheme() {
                    "http" | "https" => {
                        log::debug!("loading url import: {}", module_specifier);
                        let res = reqwest::get(module_specifier.clone()).await?;
                        let res = res.error_for_status()?;
                        if res.url() != &module_specifier {
                            redirect_module_url = Some(res.url().clone());
                        }
                        res.bytes().await?.to_vec()
                    }
                    "file" => {
                        log::debug!("resolving file module");
                        let path = match module_specifier.to_file_path() {
                            Ok(path) => path,
                            Err(_) => bail!("Invalid file URL."),
                        };
                        tokio::fs::read(path).await?
                    }
                    schema => bail!("Invalid schema {}", schema),
                };

                let media_type = MediaType::from_specifier(&module_specifier);
                let (_, should_transpile) = match MediaType::from_specifier(&module_specifier) {
                    MediaType::JavaScript | MediaType::Mjs | MediaType::Cjs => {
                        (deno_core::ModuleType::JavaScript, false)
                    }
                    MediaType::Jsx => (deno_core::ModuleType::JavaScript, true),
                    MediaType::TypeScript
                    | MediaType::Mts
                    | MediaType::Dts
                    | MediaType::Dmts
                    | MediaType::Dcts
                    | MediaType::Tsx => (deno_core::ModuleType::JavaScript, true),
                    MediaType::Json => (deno_core::ModuleType::Json, false),
                    _ => (deno_core::ModuleType::JavaScript, false),
                };

                let code = if should_transpile {
                    log::debug!("compiling ts module");
                    let parsed = deno_ast::parse_module(ParseParams {
                        specifier: module_specifier.clone(),
                        text: Arc::from(String::from_utf8_lossy(code.as_ref())),
                        media_type,
                        capture_tokens: false,
                        scope_analysis: false,
                        maybe_syntax: None,
                    })?;

                    parsed
                        .transpile(
                            &deno_ast::TranspileOptions {
                                ..Default::default()
                            },
                            &deno_ast::EmitOptions {
                                ..Default::default()
                            },
                        )?
                        .into_source()
                        .source
                } else {
                    code
                };

                // TODO: The MIME types should probably be checked.
                let module_type = match requested_module_type {
                    RequestedModuleType::None => ModuleType::JavaScript,
                    RequestedModuleType::Json => ModuleType::Json,
                    RequestedModuleType::Other(_) => {
                        log::error!("unreachable module type requested");
                        bail!("Import types other than JSON are not supported");
                    }
                };

                if let Some(redirect_module_url) = redirect_module_url {
                    Ok(ModuleSource::new_with_redirect(
                        module_type,
                        ModuleSourceCode::Bytes(code.into_boxed_slice().into()),
                        &module_specifier,
                        &redirect_module_url,
                        None,
                    ))
                } else {
                    Ok(ModuleSource::new(
                        module_type,
                        ModuleSourceCode::Bytes(code.into_boxed_slice().into()),
                        &module_specifier,
                        None,
                    ))
                }
            }
            .boxed_local(),
        )
    }
}

pub fn init() {
    let platform = v8::new_default_platform(0, false).make_shared();
    JsRuntime::init_platform(Some(platform), false);
}

pub fn run_insecure(
    function: PathBuf,
    inputs: std::collections::HashMap<String, serde_json::Value>,
) -> Result<Value, anyhow::Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        //TODO: remove this runtime mechanism and use threadpool with channels
        let main_module = deno_core::resolve_path(function.clone(), &std::env::current_dir()?)
            .map_err(|e| anyhow!("could not load module function code: {}", e))?;

        log::debug!("setting up runtime worker");
        let worker_options = WorkerOptions {
            module_loader: std::rc::Rc::new(NetworkModuleLoader),
            ..Default::default()
        };

        //TODO: trickle down perms
        let permissions =
            PermissionsContainer::new(Permissions::from_options(&PermissionsOptions {
                allow_all: true,
                allow_env: None,
                deny_env: None,
                allow_hrtime: false,
                deny_hrtime: true,
                allow_net: Some(vec![]),
                deny_net: None,
                allow_ffi: None,
                deny_ffi: None,
                allow_read: None,
                deny_read: None,
                allow_run: None,
                deny_run: None,
                allow_sys: None,
                deny_sys: None,
                allow_write: None,
                deny_write: None,
                prompt: false,
            })?);
        let mut main_worker =
            MainWorker::bootstrap_from_options(main_module.clone(), permissions, worker_options);

        // main_worker.execute_main_module(&main_module).await?;
        let mod_id = main_worker.preload_main_module(&main_module).await?;

        log::debug!("evaluating function");
        //TODO: handle error
        let _ = main_worker.evaluate_module(mod_id);

        log::debug!("running event loop");
        main_worker.run_event_loop(false).await?;
        log::debug!("done event loop");
        let fres = {
            let global = main_worker.js_runtime.get_module_namespace(mod_id)?;
            let scope = &mut main_worker.js_runtime.handle_scope();
            let namespace = v8::Local::<v8::Object>::new(scope, global);

            let func_key = v8::String::new(scope, "main")
                .ok_or(anyhow!("could not setup main function key"))?;

            let func = namespace
                .get(scope, func_key.into())
                .ok_or(anyhow!("entrypoint not found"))?;
            let func = v8::Local::<v8::Function>::try_from(func)
                .map_err(|_| anyhow!("main function not found"))?;

            let i = serde_v8::to_v8(scope, inputs)
                .map_err(|_| anyhow!("inputs provided are invalid"))?;

            let recv = v8::Integer::new(scope, 1).into();
            let func_res = func
                .call(scope, recv, &[i])
                .ok_or(anyhow!("unknown error"))?;

            v8::Global::new(scope, func_res)
        };
        let f = main_worker.js_runtime.resolve_value(fres).await?;
        let scope = &mut main_worker.js_runtime.handle_scope();
        let local_f = v8::Local::<v8::Value>::new(scope, f);

        let deserialized_value = serde_v8::from_v8::<serde_json::Value>(scope, local_f)
            .map_err(|_| anyhow!("failed to deserialise returned value"))?;

        Ok(deserialized_value)
    })
}

pub fn deinit() {
    unsafe {
        v8::V8::dispose();
    }
    v8::V8::dispose_platform();
}
fn main() {
    init();

    let mut inputs: HashMap<String, Value> = HashMap::new();
    inputs.insert("secret_key".into(), Value::String("key123".into()));
    inputs.insert("payload".into(), Value::String("{}".into()));

    let result = run_insecure("./create_jwt.js".into(), inputs);

    print!("result = {:?}", result);
    deinit();
}
