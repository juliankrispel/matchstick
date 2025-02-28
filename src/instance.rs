use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use anyhow::{anyhow, Context};
use graph::{
    blockchain::{Blockchain, HostFnCtx},
    cheap_clone::CheapClone,
    components::store::{DeploymentId, DeploymentLocator},
    prelude::{DeploymentHash, Duration, HostMetrics, StopwatchMetrics},
    runtime::HostExportError,
    semver::Version,
};
use graph_chain_ethereum::Chain;
use graph_mock::MockMetricsRegistry;
use graph_runtime_test::common::{mock_context, mock_data_source};
use graph_runtime_wasm::{
    error::DeterminismLevel,
    module::{IntoTrap, IntoWasmRet, TimeoutStopwatch, WasmInstanceContext},
    ExperimentalFeatures, MappingContext, ValidModule,
};

use crate::subgraph_store::MockSubgraphStore;
use crate::{context::MatchstickInstanceContext, logging::Log};

/// The Matchstick Instance is simply a wrapper around WASM Instance and
pub struct MatchstickInstance<C: Blockchain> {
    /// Handle to WASM Instace.
    pub instance: wasmtime::Instance,

    pub instance_ctx: Rc<RefCell<Option<MatchstickInstanceContext<C>>>>,
}

// Initialization functions.
impl<C: Blockchain> MatchstickInstance<C> {
    pub fn new(path_to_wasm: &str) -> MatchstickInstance<Chain> {
        let subgraph_id = "ipfsMap";
        let deployment_id = &DeploymentHash::new(subgraph_id).unwrap_or_else(|err| {
            panic!(
                "{}",
                Log::Critical(format!("Could not create deployment id: {}", err)),
            );
        });
        let deployment = DeploymentLocator::new(DeploymentId::new(42), deployment_id.clone());
        let data_source = mock_data_source(path_to_wasm, Version::new(0, 0, 5));

        let metrics_registry = Arc::new(MockMetricsRegistry::new());

        let stopwatch_metrics = StopwatchMetrics::new(
            graph::slog::Logger::root(graph::slog::Discard, graph::prelude::o!()),
            deployment_id.clone(),
            metrics_registry.clone(),
        );

        let host_metrics = Arc::new(HostMetrics::new(
            metrics_registry,
            deployment_id.as_str(),
            stopwatch_metrics,
        ));

        let experimental_features = ExperimentalFeatures {
            allow_non_deterministic_ipfs: true,
        };

        let mock_subgraph_store = MockSubgraphStore {};

        let valid_module = Arc::new(
            ValidModule::new(
                Arc::new(std::fs::read(path_to_wasm).unwrap_or_else(|err| {
                    panic!(
                        "{}",
                        Log::Critical(format!(
                            "Something went wrong while trying to read `{}`: {}",
                            path_to_wasm, err,
                        )),
                    );
                }))
                .as_ref(),
            )
            .unwrap_or_else(|err| {
                panic!(
                    "{}",
                    Log::Critical(format!("Could not create ValidModule: {}", err)),
                );
            }),
        );

        MatchstickInstance::<Chain>::from_valid_module_with_ctx(
            valid_module,
            mock_context(
                deployment,
                data_source,
                Arc::from(mock_subgraph_store),
                Version::new(0, 0, 5),
            ),
            host_metrics,
            None,
            experimental_features,
        )
        .unwrap_or_else(|err| {
            panic!(
                "{}",
                Log::Critical(format!(
                    "Could not create WasmInstance from valid module with context: {}",
                    err,
                )),
            );
        })
    }

    fn from_valid_module_with_ctx(
        valid_module: Arc<ValidModule>,
        ctx: MappingContext<C>,
        host_metrics: Arc<HostMetrics>,
        timeout: Option<Duration>,
        experimental_features: ExperimentalFeatures,
    ) -> Result<MatchstickInstance<C>, anyhow::Error> {
        let mut linker = wasmtime::Linker::new(&wasmtime::Store::new(valid_module.module.engine()));
        let host_fns = ctx.host_fns.cheap_clone();
        let api_version = ctx.host_exports.api_version.clone();

        let shared_ctx: Rc<RefCell<Option<MatchstickInstanceContext<C>>>> =
            Rc::new(RefCell::new(None));
        let ctx: Rc<RefCell<Option<MappingContext<C>>>> = Rc::new(RefCell::new(Some(ctx)));

        // Start the timeout watchdog task.
        let timeout_stopwatch = Arc::new(std::sync::Mutex::new(TimeoutStopwatch::start_new()));
        if let Some(timeout) = timeout {
            // This task is likely to outlive the instance, which is fine.
            let interrupt_handle = linker.store().interrupt_handle().unwrap();
            let timeout_stopwatch = timeout_stopwatch.clone();
            graph::spawn_allow_panic(async move {
                let minimum_wait = Duration::from_secs(1);
                loop {
                    let time_left =
                        timeout.checked_sub(timeout_stopwatch.lock().unwrap().elapsed());
                    match time_left {
                        None => break interrupt_handle.interrupt(), // Timed out.

                        Some(time) if time < minimum_wait => break interrupt_handle.interrupt(),
                        Some(time) => tokio::time::delay_for(time).await,
                    }
                }
            });
        }

        macro_rules! link {
            ($wasm_name:expr, $($rust_name:ident).*, $($param:ident),*) => {
                link!($wasm_name, $($rust_name).*, "host_export_other", $($param),*)
            };

            ($wasm_name:expr, $($rust_name:ident).*, $section:expr, $($param:ident),*) => {
                let modules = valid_module
                    .import_name_to_modules
                    .get($wasm_name)
                    .into_iter()
                    .flatten();

                for module in modules {
                    let func_shared_ctx = Rc::downgrade(&shared_ctx);
                    let valid_module = valid_module.cheap_clone();
                    let host_metrics = host_metrics.cheap_clone();
                    let timeout_stopwatch = timeout_stopwatch.cheap_clone();
                    let ctx = ctx.cheap_clone();
                    linker.func(
                        module,
                        $wasm_name,
                        move |caller: wasmtime::Caller, $($param: u32),*| {
                            let instance = func_shared_ctx.upgrade().unwrap();
                            let mut instance = instance.borrow_mut();

                            if instance.is_none() {
                                *instance = Some(MatchstickInstanceContext::new(
                                    WasmInstanceContext::from_caller(
                                        caller,
                                        ctx.borrow_mut().take().unwrap(),
                                        valid_module.cheap_clone(),
                                        host_metrics.cheap_clone(),
                                        timeout,
                                        timeout_stopwatch.cheap_clone(),
                                        experimental_features.clone()
                                    ).unwrap())
                                )
                            }

                            let instance = instance.as_mut().unwrap();
                            let _section = instance.wasm_ctx.host_metrics.stopwatch.start_section($section);

                            let result = instance.$($rust_name).*(
                                $($param.into()),*
                            );
                            match result {
                                Ok(result) => Ok(result.into_wasm_ret()),
                                Err(e) => {
                                    match IntoTrap::determinism_level(&e) {
                                        DeterminismLevel::Deterministic => {
                                            instance.wasm_ctx.deterministic_host_trap = true;
                                        },
                                        DeterminismLevel::PossibleReorg => {
                                            instance.wasm_ctx.possible_reorg = true;
                                        },
                                        DeterminismLevel::Unimplemented | DeterminismLevel::NonDeterministic => {},
                                    }

                                    Err(IntoTrap::into_trap(e))
                                }
                            }
                        }
                    )?;
                }
            };
        }

        for host_fn in host_fns.iter() {
            let modules = valid_module
                .import_name_to_modules
                .get(host_fn.name)
                .into_iter()
                .flatten();

            for module in modules {
                let func_shared_ctx = Rc::downgrade(&shared_ctx);
                let host_fn = host_fn.cheap_clone();
                linker.func(module, host_fn.name, move |call_ptr: u32| {
                    let start = Instant::now();
                    let instance = func_shared_ctx.upgrade().unwrap();
                    let mut instance = instance.borrow_mut();

                    let instance = match &mut *instance {
                        Some(instance) => instance,

                        None => {
                            return Err(anyhow!(
                                "{} is not allowed in global variables",
                                host_fn.name
                            )
                            .into())
                        }
                    };

                    let name_for_metrics = host_fn.name.replace('.', "_");
                    let stopwatch = &instance.wasm_ctx.host_metrics.stopwatch;
                    let _section =
                        stopwatch.start_section(&format!("host_export_{}", name_for_metrics));

                    let ctx = HostFnCtx {
                        logger: instance.wasm_ctx.ctx.logger.cheap_clone(),
                        block_ptr: instance.wasm_ctx.ctx.block_ptr.cheap_clone(),
                        heap: &mut instance.wasm_ctx,
                    };
                    let ret = (host_fn.func)(ctx, call_ptr).map_err(|e| match e {
                        HostExportError::Deterministic(e) => {
                            instance.wasm_ctx.deterministic_host_trap = true;
                            e
                        }
                        HostExportError::PossibleReorg(e) => {
                            instance.wasm_ctx.possible_reorg = true;
                            e
                        }
                        HostExportError::Unknown(e) => e,
                    })?;
                    instance
                        .wasm_ctx
                        .host_metrics
                        .observe_host_fn_execution_time(
                            start.elapsed().as_secs_f64(),
                            &name_for_metrics,
                        );
                    Ok(ret)
                })?;
            }
        }

        link!("ethereum.call", ethereum_call, contract_call_ptr);
        link!("ethereum.encode", wasm_ctx.ethereum_encode, params_ptr);
        link!(
            "ethereum.decode",
            wasm_ctx.ethereum_decode,
            params_ptr,
            data_ptr
        );

        link!(
            "abort",
            wasm_ctx.abort,
            message_ptr,
            file_name_ptr,
            line,
            column
        );

        link!(
            "mockFunction",
            mock_function,
            contract_address_ptr,
            fn_name_ptr,
            fn_signature_ptr,
            fn_args_ptr,
            return_value_ptr,
            reverts
        );

        link!("clearStore", clear_store,);
        link!("logStore", log_store,);
        link!(
            "store.get",
            mock_store_get,
            "host_export_store_get",
            entity,
            id
        );
        link!(
            "store.set",
            mock_store_set,
            "host_export_store_set",
            entity,
            id,
            data
        );

        link!(
            "ipfs.cat",
            wasm_ctx.ipfs_cat,
            "host_export_ipfs_cat",
            hash_ptr
        );
        link!(
            "ipfs.map",
            wasm_ctx.ipfs_map,
            "host_export_ipfs_map",
            link_ptr,
            callback,
            user_data,
            flags
        );

        link!("store.remove", mock_store_remove, entity_ptr, id_ptr);

        link!(
            "typeConversion.bytesToString",
            wasm_ctx.bytes_to_string,
            ptr
        );
        link!("typeConversion.bytesToHex", wasm_ctx.bytes_to_hex, ptr);
        link!(
            "typeConversion.bigIntToString",
            wasm_ctx.big_int_to_string,
            ptr
        );
        link!("typeConversion.bigIntToHex", wasm_ctx.big_int_to_hex, ptr);
        link!("typeConversion.stringToH160", wasm_ctx.string_to_h160, ptr);
        link!(
            "typeConversion.bytesToBase58",
            wasm_ctx.bytes_to_base58,
            ptr
        );

        link!("json.fromBytes", wasm_ctx.json_from_bytes, ptr);
        link!("json.try_fromBytes", wasm_ctx.json_try_from_bytes, ptr);
        link!("json.toI64", wasm_ctx.json_to_i64, ptr);
        link!("json.toU64", wasm_ctx.json_to_u64, ptr);
        link!("json.toF64", wasm_ctx.json_to_f64, ptr);
        link!("json.toBigInt", wasm_ctx.json_to_big_int, ptr);

        link!("crypto.keccak256", wasm_ctx.crypto_keccak_256, ptr);

        link!("bigInt.plus", wasm_ctx.big_int_plus, x_ptr, y_ptr);
        link!("bigInt.minus", wasm_ctx.big_int_minus, x_ptr, y_ptr);
        link!("bigInt.times", wasm_ctx.big_int_times, x_ptr, y_ptr);
        link!(
            "bigInt.dividedBy",
            wasm_ctx.big_int_divided_by,
            x_ptr,
            y_ptr
        );
        link!(
            "bigInt.dividedByDecimal",
            wasm_ctx.big_int_divided_by_decimal,
            x,
            y
        );
        link!("bigInt.mod", wasm_ctx.big_int_mod, x_ptr, y_ptr);
        link!("bigInt.pow", wasm_ctx.big_int_pow, x_ptr, exp);
        link!("bigInt.fromString", wasm_ctx.big_int_from_string, ptr);
        link!("bigInt.bitOr", wasm_ctx.big_int_bit_or, x_ptr, y_ptr);
        link!("bigInt.bitAnd", wasm_ctx.big_int_bit_and, x_ptr, y_ptr);
        link!("bigInt.leftShift", wasm_ctx.big_int_left_shift, x_ptr, bits);
        link!(
            "bigInt.rightShift",
            wasm_ctx.big_int_right_shift,
            x_ptr,
            bits
        );

        link!("bigDecimal.toString", wasm_ctx.big_decimal_to_string, ptr);
        link!(
            "bigDecimal.fromString",
            wasm_ctx.big_decimal_from_string,
            ptr
        );
        link!("bigDecimal.plus", wasm_ctx.big_decimal_plus, x_ptr, y_ptr);
        link!("bigDecimal.minus", wasm_ctx.big_decimal_minus, x_ptr, y_ptr);
        link!("bigDecimal.times", wasm_ctx.big_decimal_times, x_ptr, y_ptr);
        link!(
            "bigDecimal.dividedBy",
            wasm_ctx.big_decimal_divided_by,
            x,
            y
        );
        link!(
            "bigDecimal.equals",
            wasm_ctx.big_decimal_equals,
            x_ptr,
            y_ptr
        );

        link!("dataSource.create", mock_data_source_create, name, params);
        link!(
            "dataSource.createWithContext",
            mock_data_source_create_with_context,
            name,
            params,
            context
        );
        link!("dataSource.address", wasm_ctx.data_source_address,);
        link!("dataSource.network", wasm_ctx.data_source_network,);
        link!("dataSource.context", wasm_ctx.data_source_context,);

        link!("ens.nameByHash", wasm_ctx.ens_name_by_hash, ptr);

        link!("log.log", log, level, msg_ptr);

        // `arweave and `box` functionality was removed, but apiVersion <= 0.0.4 must link it.
        if api_version <= Version::new(0, 0, 4) {
            link!(
                "arweave.transactionData",
                wasm_ctx.arweave_transaction_data,
                ptr
            );
            link!("box.profile", wasm_ctx.box_profile, ptr);
        }

        link!(
            "_registerTest",
            register_test,
            name_ptr,
            should_fail_ptr,
            func_idx
        );

        link!(
            "_assert.fieldEquals",
            assert_field_equals,
            entity_type_ptr,
            id_ptr,
            field_name_ptr,
            expected_val_ptr
        );

        link!("_assert.equals", assert_equals, expected_ptr, actual_ptr);
        link!(
            "_assert.notInStore",
            assert_not_in_store,
            entity_type_ptr,
            id_ptr
        );

        let instance = linker.instantiate(&valid_module.module)?;

        if shared_ctx.borrow().is_none() {
            *shared_ctx.borrow_mut() = Some(MatchstickInstanceContext::new(
                WasmInstanceContext::from_instance(
                    &instance,
                    ctx.borrow_mut().take().unwrap(),
                    valid_module,
                    host_metrics,
                    timeout,
                    timeout_stopwatch,
                    experimental_features,
                )?,
            ));
        }

        match api_version {
            version if version <= Version::new(0, 0, 4) => {}
            _ => {
                instance
                    .get_func("_start")
                    .context("`_start` function not found")?
                    .typed::<(), ()>()?
                    .call(())
                    .unwrap();
            }
        }

        Ok(MatchstickInstance {
            instance,
            instance_ctx: shared_ctx,
        })
    }
}
