use super::cache_manager::{FullCacheManager, NormalCacheManager};
use super::inputs_processor::DEFAULT_PROMPT_CHUNK_SIZE;
use super::isq::ImatrixDataSource;
use super::llg::build_tok_env;
use super::{
    get_model_paths, get_xlora_paths, text_models_inputs_processor::ModelInputs, AdapterKind,
    CacheManager, GeneralMetadata, Loader, ModelKind, ModelPaths, NormalModel, NormalModelLoader,
    TokenSource, XLoraPaths,
};
use super::{
    AdapterActivationMixin, AnyMoePipelineMixin, CacheManagerMixin, EitherCache,
    ForwardInputsResult, IsqOrganization, IsqPipelineMixin, MetadataMixin, ModelCategory,
    PreProcessingMixin,
};
use super::{
    AutoLoader, DeepSeekV2Loader, DeepSeekV3Loader, Gemma2Loader, GemmaLoader, LlamaLoader,
    MistralLoader, MixtralLoader, NormalLoaderType, Phi2Loader, Phi3Loader, Phi3_5MoELoader,
    Qwen2Loader, Starcoder2Loader,
};
use crate::amoe::AnyMoeExpertType;
use crate::device_map::{self, DeviceMapper};
use crate::lora::Ordering;
use crate::paged_attention::{calculate_cache_config, AttentionImplementation, CacheEngine};
use crate::pipeline::chat_template::{calculate_eos_tokens, GenerationConfig};
use crate::pipeline::get_chat_template;
use crate::pipeline::isq::UqffFullSer;
use crate::pipeline::sampling::sample_and_add_toks;
use crate::pipeline::text_models_inputs_processor::make_prompt_chunk;
use crate::pipeline::{ChatTemplate, LocalModelPaths};
use crate::prefix_cacher_v2::PrefixCacheManagerV2;
use crate::sequence::Sequence;
use crate::utils::tokenizer::get_tokenizer;
use crate::utils::varbuilder_utils::DeviceForLoadTensor;
use crate::utils::{tokens::get_token, varbuilder_utils::from_mmaped_safetensors};
use crate::xlora_models::NonGranularState;
use crate::{
    api_dir_list, api_get_file, get_mut_arcmutex, get_paths, get_uqff_paths, lora_model_loader,
    normal_model_loader, normal_model_loader_sharded, xlora_model_loader, DeviceMapSetting,
    PagedAttentionConfig, Pipeline, Topology, TryIntoDType,
};
use anyhow::{Context, Result};
use candle_core::{Device, Tensor, Var};
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use indicatif::MultiProgress;
use mistralrs_quant::{GgufMatMul, HqqLayer, IsqType, QuantizedSerdeType, ShardedSafeTensors};
use rand_isaac::Isaac64Rng;
use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
    IntoParallelRefMutIterator, ParallelIterator,
};
use regex_automata::meta::Regex;
use std::any::Any;
use std::borrow::Cow;
use std::num::{NonZero, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Barrier, RwLock};
use std::time::Instant;
use std::{env, fs};
use tokenizers::Tokenizer;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct NormalPipeline {
    parallel_models: Vec<Arc<dyn NormalModel + Send + Sync>>,
    tokenizer: Arc<Tokenizer>,
    no_kv_cache: bool,
    chat_template: Arc<ChatTemplate>,
    non_granular_state: Option<NonGranularState>,
    model_id: String,
    metadata: Arc<GeneralMetadata>,
    topology: Option<Topology>,
    silent: bool,
    organization: IsqOrganization,
    // For full UQFF serialization
    template_filename: Option<PathBuf>,
    generation_config: Option<PathBuf>,
    config: String,
    imatrix: Option<PathBuf>,
    mapper: Box<dyn DeviceMapper + Send + Sync>,
}

/// A loader for a "normal" (non-quantized) model.
pub struct NormalLoader {
    inner: Box<dyn NormalModelLoader>,
    model_id: String,
    config: NormalSpecificConfig,
    xlora_model_id: Option<String>,
    kind: ModelKind,
    xlora_order: Option<Ordering>,
    no_kv_cache: bool,
    chat_template: Option<String>,
    tokenizer_json: Option<String>,
    tgt_non_granular_index: Option<usize>,
    token_source: RwLock<Option<TokenSource>>,
    revision: RwLock<Option<String>>,
    from_uqff: RwLock<Option<PathBuf>>,
}

#[derive(Default)]
/// A builder for a loader for a "normal" (non-quantized) model.
pub struct NormalLoaderBuilder {
    model_id: Option<String>,
    config: NormalSpecificConfig,
    xlora_model_id: Option<String>,
    kind: ModelKind,
    xlora_order: Option<Ordering>,
    no_kv_cache: bool,
    chat_template: Option<String>,
    tokenizer_json: Option<String>,
    tgt_non_granular_index: Option<usize>,
}

#[derive(Clone, Default)]
/// Config specific to loading a normal model.
pub struct NormalSpecificConfig {
    pub use_flash_attn: bool,
    pub prompt_chunksize: Option<NonZeroUsize>,
    pub topology: Option<Topology>,
    pub organization: IsqOrganization,
    pub write_uqff: Option<PathBuf>,
    pub from_uqff: Option<PathBuf>,
    pub imatrix: Option<PathBuf>,
    pub calibration_file: Option<PathBuf>,
}

impl NormalLoaderBuilder {
    /// NOTE: Until v0.4.0, you should make sure to call `.with_no_kv_cache` if applicable.
    pub fn new(
        config: NormalSpecificConfig,
        chat_template: Option<String>,
        tokenizer_json: Option<String>,
        model_id: Option<String>,
    ) -> Self {
        Self {
            config,
            chat_template,
            tokenizer_json,
            model_id,
            kind: ModelKind::Normal,
            ..Default::default()
        }
    }

    // TODO(EricLBuehler): in 0.4.0 we can move this into the config
    pub fn with_no_kv_cache(mut self, no_kv_cache: bool) -> Self {
        self.no_kv_cache = no_kv_cache;
        self
    }

    fn with_adapter(
        mut self,
        xlora_model_id: String,
        xlora_order: Ordering,
        no_kv_cache: bool,
        tgt_non_granular_index: Option<usize>,
    ) -> Self {
        self.xlora_model_id = Some(xlora_model_id);
        self.xlora_order = Some(xlora_order);
        self.no_kv_cache = no_kv_cache;
        self.tgt_non_granular_index = tgt_non_granular_index;
        self.model_id = if let Some(id) = self.model_id {
            Some(id)
        } else {
            info!(
                "Using adapter base model ID: `{}`",
                self.xlora_order.as_ref().unwrap().base_model_id
            );
            Some(self.xlora_order.as_ref().unwrap().base_model_id.clone())
        };
        self
    }

    pub fn with_xlora(
        mut self,
        xlora_model_id: String,
        xlora_order: Ordering,
        no_kv_cache: bool,
        tgt_non_granular_index: Option<usize>,
    ) -> Self {
        self.kind = ModelKind::Adapter {
            adapter: AdapterKind::XLora,
        };
        self.with_adapter(
            xlora_model_id,
            xlora_order,
            no_kv_cache,
            tgt_non_granular_index,
        )
    }

    pub fn with_lora(mut self, lora_model_id: String, lora_order: Ordering) -> Self {
        self.kind = ModelKind::Adapter {
            adapter: AdapterKind::Lora,
        };
        self.with_adapter(lora_model_id, lora_order, false, None)
    }

    /// If the loader type is not specified, loader type is automatically determined from the
    /// `architectures` array in the config.
    pub fn build(self, loader_tp: Option<NormalLoaderType>) -> anyhow::Result<Box<dyn Loader>> {
        let loader: Box<dyn NormalModelLoader> = match loader_tp {
            Some(NormalLoaderType::Mistral) => Box::new(MistralLoader),
            Some(NormalLoaderType::Gemma) => Box::new(GemmaLoader),
            Some(NormalLoaderType::Llama) => Box::new(LlamaLoader),
            Some(NormalLoaderType::Mixtral) => Box::new(MixtralLoader),
            Some(NormalLoaderType::Phi2) => Box::new(Phi2Loader),
            Some(NormalLoaderType::Phi3) => Box::new(Phi3Loader),
            Some(NormalLoaderType::Qwen2) => Box::new(Qwen2Loader),
            Some(NormalLoaderType::Gemma2) => Box::new(Gemma2Loader),
            Some(NormalLoaderType::Starcoder2) => Box::new(Starcoder2Loader),
            Some(NormalLoaderType::Phi3_5MoE) => Box::new(Phi3_5MoELoader),
            Some(NormalLoaderType::DeepSeekV2) => Box::new(DeepSeekV2Loader),
            Some(NormalLoaderType::DeepSeekV3) => Box::new(DeepSeekV3Loader),
            None => Box::new(AutoLoader),
        };
        Ok(Box::new(NormalLoader {
            inner: loader,
            model_id: self.model_id.unwrap(),
            config: self.config,
            xlora_model_id: self.xlora_model_id,
            kind: self.kind,
            xlora_order: self.xlora_order,
            no_kv_cache: self.no_kv_cache,
            chat_template: self.chat_template,
            tokenizer_json: self.tokenizer_json,
            tgt_non_granular_index: self.tgt_non_granular_index,
            token_source: RwLock::new(None),
            revision: RwLock::new(None),
            from_uqff: RwLock::new(None),
        }))
    }
}

impl Loader for NormalLoader {
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn load_model_from_hf(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
        dtype: &dyn TryIntoDType,
        device: &Device,
        silent: bool,
        mapper: DeviceMapSetting,
        in_situ_quant: Option<IsqType>,
        paged_attn_config: Option<PagedAttentionConfig>,
    ) -> Result<Arc<Mutex<dyn Pipeline + Send + Sync>>> {
        let paths: anyhow::Result<Box<dyn ModelPaths>> = get_paths!(
            LocalModelPaths,
            &token_source,
            revision.clone(),
            self,
            None,
            None,
            silent,
            self.config.from_uqff.is_some()
        );
        if let Some(from_uqff) = self.config.from_uqff.clone() {
            *self.from_uqff.write().unwrap() = Some(get_uqff_paths!(&from_uqff, self, silent));
        }
        *self
            .token_source
            .write()
            .expect("Failed to write to token source") = Some(token_source);
        *self.revision.write().expect("Failed to write to revision") = revision;
        self.load_model_from_path(
            &paths?,
            dtype,
            device,
            silent,
            mapper,
            in_situ_quant,
            paged_attn_config,
        )
    }

    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn load_model_from_path(
        &self,
        paths: &Box<dyn ModelPaths>,
        dtype: &dyn TryIntoDType,
        device: &Device,
        silent: bool,
        mut mapper: DeviceMapSetting,
        in_situ_quant: Option<IsqType>,
        mut paged_attn_config: Option<PagedAttentionConfig>,
    ) -> Result<Arc<Mutex<dyn Pipeline + Send + Sync>>> {
        let config = std::fs::read_to_string(paths.get_config_filename())?;

        // Apply default prompt size here
        let prompt_chunksize = self
            .config
            .prompt_chunksize
            .unwrap_or(DEFAULT_PROMPT_CHUNK_SIZE.try_into().unwrap())
            .get();

        info!("Prompt chunk size is {prompt_chunksize}.",);

        let available_devices = device_map::get_all_similar_devices(device)?;

        let use_nccl = available_devices.iter().all(|dev| dev.is_cuda())
            && available_devices.len() > 1
            && (std::env::var("MISTRALRS_NO_NCCL").is_err()
                || std::env::var("MISTRALRS_NO_NCCL").is_ok_and(|x| x != "1"))
            && cfg!(feature = "nccl");

        // If auto, convert to Map if not using nccl
        if use_nccl {
            mapper = DeviceMapSetting::Nccl {
                devices: available_devices.clone(),
            };
        } else if let DeviceMapSetting::Auto(params) = mapper.clone() {
            // Initial dtype
            let dtype = dtype.try_into_dtype(&available_devices.iter().collect::<Vec<_>>())?;

            // ISQ or UQFF: quantized path
            // Match logic below where UQFF has priority
            let (layer_sizes_in_bytes, non_mapped_size_in_bytes, total_model_size_in_bytes) =
                if let Some(serialized) = &*self.from_uqff.read().unwrap() {
                    let weight_pack_factor = {
                        let ser_artifacts = unsafe {
                            candle_core::safetensors::MmapedSafetensors::new(serialized)?
                        };
                        let mut total_pack_factors = 0;
                        let total_tensors = ser_artifacts.tensors().len();
                        for (_, artifact) in ser_artifacts.tensors() {
                            let artifact = artifact.data();
                            // NOTE(EricLBuehler): isq type is ALWAYS byte 4 (5th) of the tensor.
                            let isq_type = artifact[mistralrs_quant::UQFF_QUANT_TYPE_OFFSET];
                            let pack_factor = match QuantizedSerdeType::try_from(isq_type as usize)?
                            {
                                QuantizedSerdeType::Hqq => {
                                    HqqLayer::get_isq_type_from_uqff(Cow::Borrowed(artifact))?
                                        .pack_factor(dtype)
                                }
                                QuantizedSerdeType::Gguf => {
                                    GgufMatMul::get_isq_type_from_uqff(Cow::Borrowed(artifact))?
                                        .pack_factor(dtype)
                                }
                                QuantizedSerdeType::Fp8 => IsqType::F8E4M3.pack_factor(dtype),
                                QuantizedSerdeType::Unquant => 1,
                            };
                            total_pack_factors += pack_factor;
                        }

                        total_pack_factors / total_tensors
                    };

                    let layer_sizes_in_bytes =
                        self.inner
                            .layer_sizes_in_bytes(&config, dtype, weight_pack_factor)?;
                    let non_mapped_size_in_bytes =
                        self.inner
                            .non_mapped_size_in_bytes(&config, dtype, weight_pack_factor)?;
                    let layer_sizes_sum = layer_sizes_in_bytes.iter().sum::<usize>();
                    (
                        layer_sizes_in_bytes,
                        non_mapped_size_in_bytes,
                        layer_sizes_sum + non_mapped_size_in_bytes,
                    )
                } else if let Some(isq) = in_situ_quant {
                    let weight_pack_factor = isq.pack_factor(dtype);
                    let layer_sizes_in_bytes =
                        self.inner
                            .layer_sizes_in_bytes(&config, dtype, weight_pack_factor)?;
                    let non_mapped_size_in_bytes =
                        self.inner
                            .non_mapped_size_in_bytes(&config, dtype, weight_pack_factor)?;
                    let layer_sizes_sum = layer_sizes_in_bytes.iter().sum::<usize>();
                    (
                        layer_sizes_in_bytes,
                        non_mapped_size_in_bytes,
                        layer_sizes_sum + non_mapped_size_in_bytes,
                    )
                } else {
                    let layer_sizes_in_bytes =
                        self.inner.layer_sizes_in_bytes(&config, dtype, 1)?;
                    let non_mapped_size_in_bytes =
                        self.inner.non_mapped_size_in_bytes(&config, dtype, 1)?;
                    let layer_sizes_sum = layer_sizes_in_bytes.iter().sum::<usize>();
                    (
                        layer_sizes_in_bytes,
                        non_mapped_size_in_bytes,
                        layer_sizes_sum + non_mapped_size_in_bytes,
                    )
                };

            let new = self.inner.get_device_layers(
                &config,
                self.inner.num_layers(&config)?,
                layer_sizes_in_bytes,
                non_mapped_size_in_bytes,
                total_model_size_in_bytes,
                &available_devices,
                dtype,
                &params,
                prompt_chunksize,
                paged_attn_config.as_ref(),
            )?;
            mapper = DeviceMapSetting::Map(new);
        }

        let pipeline_mapper = mapper.into_mapper(
            self.inner.get_total_device_mapping_num_layers(&config)?,
            device,
            self.config.topology.as_ref(),
        )?;
        let mapper = mapper.into_mapper(
            self.inner.get_total_device_mapping_num_layers(&config)?,
            device,
            self.config.topology.as_ref(),
        )?;
        let mut layer_devices = Vec::new();
        for layer in 0..self.inner.get_total_device_mapping_num_layers(&config)? {
            let device = mapper.device_for(layer, false).cloned();
            layer_devices.push(device);
        }
        let dtype = mapper.get_min_dtype(dtype)?;

        // TODO: PagedAttention is not supported with CPU for now.
        // This check is not really necessary because `get_device_layers` should prevent it.
        let mapping_uses_cpu = mapper.get_unique_devices().iter().any(Device::is_cpu);
        if mapping_uses_cpu {
            warn!("Device mapping contains a mix of GPU and CPU. There is no CPU support for PagedAttention, disabling PagedAttention.");
            paged_attn_config = None;
        }

        info!(
            "Model config: {:?}",
            self.inner
                .get_config_repr(&config, self.config.use_flash_attn)?
        );

        let mut loading_isq = in_situ_quant.is_some() || self.config.from_uqff.is_some();
        if let Some(ref topology) = self.config.topology {
            loading_isq |= topology
                .0
                .iter()
                .any(|layer| layer.as_ref().is_some_and(|layer| layer.isq.is_some()));
        }

        if self.config.imatrix.is_some() && self.config.calibration_file.is_some() {
            anyhow::bail!(
                "`imatrix` and `calibration_file` were both specified, this is not allowed."
            );
        }

        // Load onto the regular device if not using isq or if the calibration file is specified
        let load_device = if !loading_isq || self.config.calibration_file.is_some() {
            loading_isq = false;
            device.clone()
        } else {
            Device::Cpu
        };

        let is_xlora = self.kind.is_adapted_and(|a| a.is_x_lora());

        let attention_mechanism = if paged_attn_config.is_some() {
            AttentionImplementation::PagedAttention
        } else {
            AttentionImplementation::Eager
        };

        let multi_progress = Arc::new(MultiProgress::new());

        let mut parallel_models = if use_nccl {
            #[cfg(not(feature = "nccl"))]
            warn!(
                "NCCL support was included in the build, be sure to build with `--features nccl`."
            );

            // NCCL case!

            let pipeline_parallel_size = std::env::var("MISTRALRS_PIPELINE_PARALLEL")
                .map(|ref x| {
                    usize::from_str(x).expect(
                        "Invalid MISTRALRS_PIPELINE_PARALLEL setting (could not parse as integer)",
                    )
                })
                .unwrap_or(1);

            if pipeline_parallel_size == 0 {
                anyhow::bail!("MISTRALRS_PIPELINE_PARALLEL must be nonzero")
            }

            let local_world_size = available_devices.len() / pipeline_parallel_size;
            let global_world_size = if let Ok(x) = std::env::var("MISTRALRS_MN_GLOBAL_WORLD_SIZE") {
                usize::from_str(&x).context("MISTRALRS_MN_GLOBAL_WORLD_SIZE")?
            } else {
                local_world_size
            };

            let use_multi_node = global_world_size != local_world_size;
            if use_multi_node {
                info!("Global world size != local world size, entering multi-node.");
            }

            if global_world_size < local_world_size || global_world_size % local_world_size != 0 {
                anyhow::bail!("Global world size {global_world_size} must both be at least and divide the local world size {local_world_size}");
            }

            info!("Local tensor parallel world size is {local_world_size}");
            info!("Global tensor parallel world size is {global_world_size}");
            info!("Pipeline parallelism size is {pipeline_parallel_size}");

            let mut ids = (0..pipeline_parallel_size)
                .map(|_| mistralrs_quant::Id::new())
                .collect::<Vec<_>>();

            if ids.len() != 1 && use_multi_node {
                anyhow::bail!(
                    "MISTRALRS_PIPELINE_PARALLEL cannot be set at the same time as MISTRALRS_MN_GLOBAL_WORLD_SIZE; multi-node is incompatible with pipeline parallel."
                );
            }

            if use_multi_node {
                let id = &mut ids[0];
                if let Ok(n_nodes) = env::var("MISTRALRS_MN_HEAD_NUM_WORKERS") {
                    let n_nodes =
                        usize::from_str(&n_nodes).context("MISTRALRS_MN_HEAD_NUM_WORKERS")?;
                    info!("Head node managing {n_nodes} workers.");
                    let Ok(port) = env::var("MISTRALRS_MN_HEAD_PORT") else {
                        anyhow::bail!(
                            "Got MISTRALRS_MN_HEAD_NUM_WORKERS, expected MISTRALRS_MN_HEAD_PORT"
                        );
                    };
                    info!("Head node initializing connection on {port}.");
                    let server = mistralrs_quant::Server::new(
                        &format!("0.0.0.0:{port}"),
                        n_nodes,
                        local_world_size,
                    )?;

                    server.broadcast_id(id)?;
                } else if let Ok(addr) = env::var("MISTRALRS_MN_WORKER_SERVER_ADDR") {
                    info!("Worker node connecting to {addr}.");
                    let client = mistralrs_quant::Client::new(addr.parse()?, local_world_size)?;

                    *id = client.receive_id()?;
                }
            }

            if available_devices.len() % ids.len() != 0 {
                anyhow::bail!(
                    "Pipeline parallel size {} must divide the number of available devices {}",
                    pipeline_parallel_size,
                    available_devices.len()
                );
            }

            let split_available_devices = available_devices
                .chunks(available_devices.len() / pipeline_parallel_size)
                .collect::<Vec<_>>();

            let rank_offset = if env::var("MISTRALRS_MN_WORKER_SERVER_ADDR").is_ok() {
                let Ok(node_id) = env::var("MISTRALRS_MN_WORKER_ID") else {
                    anyhow::bail!(
                        "Got MISTRALRS_MN_WORKER_SERVER_ADDR, expected MISTRALRS_MN_WORKER_ID"
                    );
                };
                let node_id = usize::from_str(&node_id).context("MISTRALRS_MN_WORKER_ID")?;
                info!("Worker ID is {node_id}.");
                (node_id + 1) * local_world_size
            } else {
                0
            };

            // Transpose
            let mut comms_all = Vec::new();
            for (pipeline_parallel_i, devices_per_pipeline_parallel) in
                split_available_devices.iter().enumerate()
            {
                // Each pipeline parallel gets its own barrier
                let barrier = if let Ok(n_nodes) = env::var("MISTRALRS_MN_HEAD_NUM_WORKERS") {
                    let n_nodes =
                        usize::from_str(&n_nodes).context("MISTRALRS_MN_HEAD_NUM_WORKERS")?;
                    let Ok(port) = env::var("MISTRALRS_MN_HEAD_PORT") else {
                        anyhow::bail!(
                            "Got MISTRALRS_MN_HEAD_NUM_WORKERS, expected MISTRALRS_MN_HEAD_PORT"
                        );
                    };
                    let server = mistralrs_quant::Server::new(
                        &format!("0.0.0.0:{port}"),
                        n_nodes,
                        local_world_size,
                    )?;

                    Arc::new(server) as Arc<dyn mistralrs_quant::BarrierLike>
                } else if let Ok(addr) = env::var("MISTRALRS_MN_WORKER_SERVER_ADDR") {
                    let client = mistralrs_quant::Client::new(addr.parse()?, local_world_size)?;
                    Arc::new(client) as Arc<dyn mistralrs_quant::BarrierLike>
                } else {
                    Arc::new(Barrier::new(local_world_size))
                        as Arc<dyn mistralrs_quant::BarrierLike>
                };

                // They each block on each other
                // https://docs.nvidia.com/deeplearning/nccl/user-guide/docs/api/comms.html?ncclcomminitrank#ncclcomminitrank
                let comms = devices_per_pipeline_parallel
                    .par_iter()
                    .enumerate()
                    .map(|(rank, device)| {
                        #[cfg(feature = "cuda")]
                        {
                            use candle_core::cuda::cudarc::driver::result;
                            unsafe {
                                result::ctx::set_current(*device.as_cuda_device()?.cu_primary_ctx())
                            }
                            .unwrap();
                        }
                        mistralrs_quant::Comm::from_device(
                            ids[pipeline_parallel_i],
                            device,
                            rank + rank_offset,
                            global_world_size,
                            barrier.clone(),
                        )
                    })
                    .collect::<candle_core::Result<Vec<_>>>()?;
                comms_all.push(
                    comms
                        .into_iter()
                        .map(Arc::new)
                        .zip(devices_per_pipeline_parallel.to_vec())
                        .collect::<Vec<_>>(),
                );
            }

            // row major: number of ranks x pipeline parallel
            // Also corresponds to the device for that comm for the
            let comms = (0..local_world_size)
                .map(|pipeline_parallel_i| {
                    comms_all
                        .iter()
                        .map(|comms_for_rank| comms_for_rank[pipeline_parallel_i].clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            let make_dummy_regexes = if loading_isq && self.config.from_uqff.is_some() {
                // Dummy weights for the layers which will be overwritten...
                Some(std::sync::Arc::new(
                    if matches!(self.config.organization, IsqOrganization::MoeExpertsOnly) {
                        self.inner.isq_layer_regexes_moqe(&config)?
                    } else {
                        self.inner.isq_layer_regexes(&config)?
                    },
                ))
            } else {
                None
            };

            let sharded_vb = unsafe {
                ShardedSafeTensors::sharded(
                    paths.get_weight_filenames(),
                    dtype,
                    &load_device,
                    make_dummy_regexes,
                )?
            };

            info!("Loading all ranks.");
            comms
                .into_par_iter()
                .map(|comm_per_pipeline_parallel| {
                    let device = comm_per_pipeline_parallel[0].1.clone();

                    // The mapper is specific to this pipeline
                    let mapper = DeviceMapSetting::NcclPipelineParallel {
                        devices_and_comms: comm_per_pipeline_parallel.clone(),
                        nm_device: device.clone(),
                    }
                    .into_mapper(
                        self.inner.get_total_device_mapping_num_layers(&config)?,
                        &device,
                        None,
                    )?;

                    let sharded_vb = if !loading_isq {
                        sharded_vb.clone().set_device(device.clone())
                    } else {
                        sharded_vb.clone()
                    };

                    // Special case for normal models which support nccl so should be more optimially loaded.
                    let model = match self.kind {
                        ModelKind::Normal => normal_model_loader_sharded!(
                            sharded_vb,
                            config,
                            self.inner,
                            self.config.use_flash_attn,
                            mapper,
                            loading_isq,
                            device,
                            attention_mechanism,
                            multi_progress.clone(),
                        ),
                        ModelKind::Adapter {
                            adapter: AdapterKind::XLora,
                        } => xlora_model_loader!(
                            paths,
                            Some(dtype),
                            &load_device,
                            layer_devices.clone(),
                            config,
                            self.inner,
                            self.config.use_flash_attn,
                            silent,
                            mapper,
                            loading_isq,
                            device,
                            multi_progress.clone(),
                        ),
                        ModelKind::Adapter {
                            adapter: AdapterKind::Lora,
                        } => lora_model_loader!(
                            paths,
                            dtype,
                            &load_device,
                            layer_devices.clone(),
                            config,
                            self.inner,
                            self.config.use_flash_attn,
                            silent,
                            mapper,
                            loading_isq,
                            device,
                            multi_progress.clone(),
                        ),
                        _ => unreachable!(),
                    };

                    Ok(model)
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            let model = match self.kind {
                ModelKind::Normal => normal_model_loader!(
                    paths,
                    Some(dtype),
                    &load_device,
                    layer_devices.clone(),
                    config,
                    self.inner,
                    self.config.use_flash_attn,
                    silent,
                    mapper,
                    loading_isq,
                    self.config.from_uqff.is_some(),
                    device.clone(),
                    attention_mechanism,
                    matches!(self.config.organization, IsqOrganization::MoeExpertsOnly),
                    multi_progress.clone(),
                ),
                ModelKind::Adapter {
                    adapter: AdapterKind::XLora,
                } => xlora_model_loader!(
                    paths,
                    Some(dtype),
                    &load_device,
                    layer_devices.clone(),
                    config,
                    self.inner,
                    self.config.use_flash_attn,
                    silent,
                    mapper,
                    loading_isq,
                    device.clone(),
                    multi_progress.clone(),
                ),
                ModelKind::Adapter {
                    adapter: AdapterKind::Lora,
                } => lora_model_loader!(
                    paths,
                    dtype,
                    &load_device,
                    layer_devices.clone(),
                    config,
                    self.inner,
                    self.config.use_flash_attn,
                    silent,
                    mapper,
                    loading_isq,
                    device.clone(),
                    multi_progress.clone(),
                ),
                _ => unreachable!(),
            };
            vec![model]
        };

        let tokenizer = get_tokenizer(paths.get_tokenizer_filename(), None)?;
        let gen_conf: Option<GenerationConfig> = paths.get_gen_conf_filename().map(|f| {
            serde_json::from_str(&fs::read_to_string(f).unwrap())
                .expect("bos_token_id/eos_token_id missing in generation_config.json")
        });

        let chat_template = get_chat_template(
            paths,
            &paths
                .get_chat_template_json()
                .as_ref()
                .map(|x| x.to_string_lossy().to_string())
                .clone(),
            &self.chat_template,
            None,
        );

        if let Some(calibration_file) = &self.config.calibration_file {
            let calibration_data = std::fs::read_to_string(calibration_file)?;
            // Tokenize, don't add bos yet
            let tokens = tokenizer
                .encode(calibration_data, false)
                .map_err(anyhow::Error::msg)?
                .get_ids()
                .to_vec();
            info!(
                "Collecting imatrix from calibration file `{}` of {} tokens.",
                calibration_file.display(),
                tokens.len()
            );
            let bos_toks = chat_template.bos_tok().map(|b| vec![b]).unwrap_or_default();
            let bos_tok_id = tokenizer
                .token_to_id(&bos_toks[0])
                .expect("Somehow the bos token is not present.");

            for model in &mut parallel_models {
                match self.config.organization {
                    IsqOrganization::Default => model.begin_track_stats()?,
                    IsqOrganization::MoeExpertsOnly => {
                        model.begin_track_stats_moe_experts_only()?
                    }
                }
            }

            const CHUNK_SIZE: usize = 1024;
            let n_chunks = tokens.len().div_ceil(CHUNK_SIZE);
            let start = Instant::now();
            for (i, chunk) in tokens.chunks(CHUNK_SIZE).enumerate() {
                let chunk = [vec![bos_tok_id], chunk.to_vec()].concat();
                let chunk_len = chunk.len();

                let start = Instant::now();
                let inputs = make_prompt_chunk(
                    0,
                    vec![chunk],
                    &[0],
                    &load_device,
                    None,
                    false,
                    None,
                    Some(pipeline_mapper.as_ref()),
                )?;
                let _ = parallel_models
                    .par_iter()
                    .map(|model| {
                        model.forward(
                            &inputs.input.to_device(model.device())?,
                            &inputs.positions,
                            inputs.context_lens.clone(),
                            inputs.position_ids.clone(),
                            None,
                            &inputs.flash_meta.clone(),
                        )
                    })
                    .collect::<candle_core::Result<Vec<_>>>()?;
                for model in &mut parallel_models {
                    match model.cache_mut() {
                        EitherCache::Full(full) => {
                            for layer in &mut *full.lock() {
                                *layer = None
                            }
                        }
                        EitherCache::Normal(normal) => {
                            for layer in &mut *normal.lock().unwrap().0 {
                                layer.set_len(0);
                            }
                        }
                    }
                }
                let end = Instant::now();
                info!(
                    "Processed chunk {}/{n_chunks} ({chunk_len} tokens), {:.2}s",
                    i + 1,
                    end.duration_since(start).as_secs_f32()
                );
            }
            load_device.synchronize()?;
            let end = Instant::now();
            info!(
                "Finished collecting imatrix in {:.2}s",
                end.duration_since(start).as_secs_f32()
            );
        }

        if (in_situ_quant.is_some() || self.config.topology.is_some())
            && self.config.from_uqff.is_none()
        {
            let imatrix_source = match (
                self.config.imatrix.as_ref(),
                self.config.calibration_file.is_some(),
            ) {
                (None, false) => None,
                (Some(file), false) => Some(ImatrixDataSource::File(file)),
                (None, true) => Some(ImatrixDataSource::Collected),
                (Some(_), true) => unreachable!(),
            };

            info!("Applying ISQ to all ranks.");

            let multi_progress = Arc::new(MultiProgress::new());
            parallel_models
                .par_iter_mut()
                .map(|model| {
                    model.quantize(
                        in_situ_quant,
                        model.device().clone(),
                        self.config.topology.as_ref(),
                        silent,
                        imatrix_source,
                        self.config.organization,
                        self.config.write_uqff.as_ref(),
                        UqffFullSer {
                            tokenizer: &tokenizer,
                            template_filename: paths.get_template_filename(),
                            generation_config: paths.get_gen_conf_filename(),
                            config: config.clone(),
                            processor_filename: &None,
                            preprocessor_filename: &None,
                        },
                        multi_progress.clone(),
                    )
                })
                .collect::<candle_core::Result<Vec<_>>>()?;
        } else if let Some(from_uqff) = &*self.from_uqff.read().unwrap() {
            let world_size = parallel_models.len();
            for (rank, model) in parallel_models.iter_mut().enumerate() {
                info!("Loading UFF for rank {}/{world_size}", rank + 1);

                model.load_from_artifacts(
                    device.clone(),
                    self.config.topology.as_ref(),
                    silent,
                    from_uqff,
                )?;
            }
        }

        let paged_attn_config = if matches!(self.kind, ModelKind::Adapter { .. }) {
            warn!(
                "Adapter parallel_models do not currently support PagedAttention, running without"
            );
            None
        } else {
            paged_attn_config
        };

        let (cache_config, cache_engines) = if let Some(paged_attn_config) = paged_attn_config {
            let cache_config = calculate_cache_config(
                paged_attn_config.mem_gpu,
                paged_attn_config.mem_cpu,
                paged_attn_config.block_size,
                dtype,
                parallel_models[0].config(),
                device,
                &pipeline_mapper
                    .get_unique_devices()
                    .into_iter()
                    .map(Some)
                    .collect::<Vec<_>>(),
                silent,
            )?;
            let mut cache_engines = Vec::new();
            for model in &mut parallel_models {
                let mut layer_devices = Vec::new();
                for layer in 0..self.inner.get_total_device_mapping_num_layers(&config)? {
                    let device = model.get_layers().1.device_for(layer, false).cloned();
                    layer_devices.push(device);
                }
                let cache_engine = CacheEngine::new(
                    model.config(),
                    &cache_config,
                    dtype,
                    model.device(),
                    layer_devices.clone(),
                )?;
                cache_engines.push(cache_engine)
            }
            (Some(cache_config), Some(cache_engines))
        } else {
            (None, None)
        };

        let max_seq_len = parallel_models[0].max_seq_len();
        let tok_env = build_tok_env(tokenizer.clone());
        let num_hidden_layers = match parallel_models[0].cache() {
            EitherCache::Full(full) => full.lock().len(),
            EitherCache::Normal(normal) => normal.lock().unwrap().0.len(),
        };
        let eos = calculate_eos_tokens(&chat_template, gen_conf, &tokenizer);
        let sliding_window = parallel_models[0].config().sliding_window;
        let model_metadata = Arc::new(parallel_models[0].config().clone());

        let parallel_models = parallel_models
            .into_iter()
            .map(Arc::from)
            .collect::<Vec<_>>();
        Ok(Arc::new(Mutex::new(NormalPipeline {
            parallel_models,
            tokenizer: tokenizer.into(),
            no_kv_cache: self.no_kv_cache,
            chat_template: Arc::new(chat_template),
            non_granular_state: self.tgt_non_granular_index.map(|tgt_non_granular_index| {
                NonGranularState {
                    non_granular_index: Arc::new(Mutex::new(0)),
                    tgt_non_granular_index,
                }
            }),
            model_id: self.model_id.clone(),
            metadata: Arc::new(GeneralMetadata {
                max_seq_len,
                tok_env: Some(tok_env),
                no_kv_cache: self.no_kv_cache,
                no_prefix_cache: is_xlora,
                num_hidden_layers,
                eos_tok: eos,
                kind: self.kind.clone(),
                is_xlora,
                activation_dtype: dtype,
                sliding_window,
                cache_config,
                cache_engines,
                prompt_chunksize: Some(NonZero::new(prompt_chunksize).unwrap()),
                model_metadata: Some(model_metadata),
            }),
            topology: self.config.topology.clone(),
            silent,
            organization: self.config.organization,
            template_filename: paths.get_template_filename().clone(),
            generation_config: paths.get_gen_conf_filename().cloned(),
            config,
            imatrix: self.config.imatrix.clone(),
            mapper: pipeline_mapper,
        })))
    }

    fn get_id(&self) -> String {
        self.xlora_model_id
            .as_deref()
            .unwrap_or(&self.model_id)
            .to_string()
    }

    fn get_kind(&self) -> ModelKind {
        self.kind.clone()
    }
}

impl PreProcessingMixin for NormalPipeline {
    fn get_chat_template(&self) -> Option<Arc<ChatTemplate>> {
        Some(self.chat_template.clone())
    }
    fn get_input_processor_config(&self) -> Option<Arc<dyn Any>> {
        None
    }
}

impl IsqPipelineMixin for NormalPipeline {
    fn re_isq_model(&mut self, dtype: IsqType) -> Result<()> {
        let device = self.device().clone();
        let multi_progress = Arc::new(MultiProgress::new());
        self.parallel_models
            .par_iter_mut()
            .map(|model| {
                Arc::get_mut(model).unwrap().quantize(
                    Some(dtype),
                    device.clone(),
                    self.topology.as_ref(),
                    self.silent,
                    self.imatrix.as_ref().map(ImatrixDataSource::File),
                    self.organization,
                    None,
                    UqffFullSer {
                        tokenizer: &self.tokenizer,
                        template_filename: &self.template_filename,
                        generation_config: self.generation_config.as_ref(),
                        config: self.config.clone(),
                        processor_filename: &None,
                        preprocessor_filename: &None,
                    },
                    multi_progress.clone(),
                )
            })
            .collect::<candle_core::Result<Vec<_>>>()?;
        Ok(())
    }
}

impl CacheManagerMixin for NormalPipeline {
    fn clone_in_cache(&self, seqs: &mut [&mut Sequence]) {
        if self.parallel_models.len() != 1 {
            panic!("Number of parallel models is not 1.");
        }
        if matches!(self.parallel_models[0].cache(), EitherCache::Full(_)) {
            FullCacheManager.clone_in_cache(self, seqs, false)
        } else {
            NormalCacheManager.clone_in_cache(self, seqs, false)
        }
    }
    fn clone_out_cache(&self, seqs: &mut [&mut Sequence]) {
        if self.parallel_models.len() != 1 {
            panic!("Number of parallel models is not 1.");
        }
        if matches!(self.parallel_models[0].cache(), EitherCache::Full(_)) {
            FullCacheManager.clone_out_cache(self, seqs, false)
        } else {
            NormalCacheManager.clone_out_cache(self, seqs, false)
        }
    }
    fn set_none_cache(
        &self,
        seqs: &mut [&mut Sequence],
        reset_non_granular: bool,
        modify_draft_cache: bool,

        load_preallocated_cache: bool,
    ) {
        if self.parallel_models.len() != 1 {
            panic!("Number of parallel models is not 1.");
        }
        if matches!(self.parallel_models[0].cache(), EitherCache::Full(_)) {
            FullCacheManager.set_none_cache(self, seqs, modify_draft_cache, false);
        } else {
            NormalCacheManager.set_none_cache(
                self,
                seqs,
                modify_draft_cache,
                load_preallocated_cache,
            );
        }
        if reset_non_granular {
            self.reset_non_granular_state()
        }
    }
    fn cache(&self) -> &EitherCache {
        self.parallel_models[0].cache()
    }
}

impl AdapterActivationMixin for NormalPipeline {
    fn activate_adapters(&mut self, adapter_names: Vec<String>) -> anyhow::Result<usize> {
        let sum = self
            .parallel_models
            .par_iter_mut()
            .map(|model| {
                Arc::get_mut(model)
                    .unwrap()
                    .activate_adapters(adapter_names.clone())
                    .map_err(anyhow::Error::msg)
            })
            .collect::<Result<Vec<_>>>()?
            .iter()
            .sum();

        Ok(sum)
    }
}

impl MetadataMixin for NormalPipeline {
    fn device(&self) -> Device {
        self.parallel_models[0].device().clone()
    }
    fn tokenizer(&self) -> Option<Arc<Tokenizer>> {
        Some(self.tokenizer.clone())
    }
    fn name(&self) -> String {
        self.model_id.clone()
    }
    fn reset_non_granular_state(&self) {
        if let Some(s) = self.non_granular_state.as_ref() {
            *self.cache().full().get_scalings_cache() = None;
            *get_mut_arcmutex!(s.non_granular_index) = 0;
        }
    }
    fn get_metadata(&self) -> Arc<GeneralMetadata> {
        self.metadata.clone()
    }
    fn device_mapper(&self) -> Option<&dyn DeviceMapper> {
        Some(&*self.mapper)
    }
}

#[async_trait::async_trait]
impl Pipeline for NormalPipeline {
    fn forward_inputs(
        &mut self,
        inputs: Box<dyn Any>,
        return_raw_logits: bool,
    ) -> Result<ForwardInputsResult, candle_core::Error> {
        let ModelInputs {
            input_ids,
            input_ids_full,
            seqlen_offsets,
            seqlen_offsets_full,
            context_lens,
            position_ids,
            paged_attn_meta,
            flash_meta,
            flash_meta_full,
        } = *inputs.downcast().expect("Downcast failed.");
        let metadata = self.get_metadata();
        let paged_attn_meta = match (&metadata.cache_engines, &paged_attn_meta) {
            (Some(cache_engines), Some(meta)) => Some((cache_engines, meta)),
            (Some(_), None) => {
                // This can happen if Rust-side user code is wrong
                candle_core::bail!("Forward step expected a PagedAttention input metadata. This was not provided, please ensure that the scheduler config is correctly configured for PagedAttention.")
            }
            (None, Some(_)) => {
                // This should never happen but we handle it anyway
                candle_core::bail!("Forward step got a PagedAttention input metadata but there is no cache engine. Please raise an issue.")
            }
            (None, None) => None,
        };
        #[cfg(feature = "metal")]
        let logits = objc::rc::autoreleasepool(|| -> candle_core::Result<Tensor> {
            match self.parallel_models[0].is_xlora() {
                false => {
                    // No NCCL
                    if self.parallel_models.len() == 1 {
                        let paged_attn_meta = paged_attn_meta
                            .as_ref()
                            .map(|meta| (meta.0[0].get_kv_cache().clone(), meta.1.clone()));

                        self.parallel_models[0].forward(
                            &input_ids,
                            &seqlen_offsets,
                            context_lens,
                            position_ids,
                            paged_attn_meta.as_ref().map(|(a, b)| (a.clone(), b)),
                            &flash_meta,
                        )
                    } else {
                        let mut handles = Vec::new();
                        for (rank, model) in self.parallel_models.iter().cloned().enumerate() {
                            let paged_attn_meta = paged_attn_meta
                                .as_ref()
                                .map(|meta| (meta.0[rank].get_kv_cache().clone(), meta.1.clone()));
                            let input_ids = input_ids.to_device(model.device())?;
                            let seqlen_offsets = seqlen_offsets.clone();
                            let context_lens = context_lens.clone();
                            let position_ids = position_ids.clone();
                            let flash_meta = flash_meta.clone();

                            handles.push(std::thread::spawn(move || {
                                model.forward(
                                    &input_ids,
                                    &seqlen_offsets,
                                    context_lens,
                                    position_ids,
                                    paged_attn_meta.as_ref().map(|(a, b)| (a.clone(), b)),
                                    &flash_meta,
                                )
                            }));
                        }

                        // Wait until all spawned threads are done
                        while !handles.iter().all(|h| h.is_finished()) {}

                        let logits_vec = handles
                            .into_iter()
                            .map(|handle| handle.join().unwrap())
                            .collect::<candle_core::Result<Vec<_>>>()?;

                        Ok(logits_vec[0].clone())
                    }
                }
                true => self.parallel_models[0].xlora_forward(
                    &input_ids,
                    input_ids_full.as_ref().unwrap_or(&input_ids),
                    &seqlen_offsets,
                    seqlen_offsets_full.as_ref().unwrap_or(&seqlen_offsets),
                    self.no_kv_cache,
                    &self.non_granular_state,
                    context_lens,
                    position_ids,
                    &flash_meta,
                    flash_meta_full.as_ref().unwrap_or(&flash_meta),
                ),
            }
        })?;
        #[cfg(not(feature = "metal"))]
        let logits = match self.parallel_models[0].is_xlora() {
            false => {
                // No NCCL
                if self.parallel_models.len() == 1 {
                    let paged_attn_meta = paged_attn_meta
                        .as_ref()
                        .map(|meta| (meta.0[0].get_kv_cache().clone(), meta.1.clone()));
                    self.parallel_models[0].forward(
                        &input_ids,
                        &seqlen_offsets,
                        context_lens,
                        position_ids,
                        paged_attn_meta.as_ref().map(|(a, b)| (a.clone(), b)),
                        &flash_meta,
                    )?
                } else {
                    let mut handles = Vec::new();
                    for (rank, model) in self.parallel_models.iter().cloned().enumerate() {
                        let paged_attn_meta = paged_attn_meta
                            .as_ref()
                            .map(|meta| (meta.0[rank].get_kv_cache().clone(), meta.1.clone()));
                        let input_ids = input_ids.to_device(model.device())?;
                        let seqlen_offsets = seqlen_offsets.clone();
                        let context_lens = context_lens.clone();
                        let position_ids = position_ids.clone();
                        let flash_meta = flash_meta.clone();

                        handles.push(std::thread::spawn(move || {
                            #[cfg(feature = "cuda")]
                            {
                                use candle_core::cuda::cudarc::driver::result;
                                unsafe {
                                    result::ctx::set_current(
                                        *model.device().as_cuda_device()?.cu_primary_ctx(),
                                    )
                                }
                                .unwrap();
                            }

                            model.forward(
                                &input_ids,
                                &seqlen_offsets,
                                context_lens,
                                position_ids,
                                paged_attn_meta.as_ref().map(|(a, b)| (a.clone(), b)),
                                &flash_meta,
                            )
                        }));
                    }

                    // Wait until all spawned threads are done
                    while !handles.iter().all(|h| h.is_finished()) {}

                    let logits_vec = handles
                        .into_iter()
                        .map(|handle| handle.join().unwrap())
                        .collect::<candle_core::Result<Vec<_>>>()?;

                    logits_vec[0].clone()
                }
            }
            true => self.parallel_models[0].xlora_forward(
                &input_ids,
                input_ids_full.as_ref().unwrap_or(&input_ids),
                &seqlen_offsets,
                seqlen_offsets_full.as_ref().unwrap_or(&seqlen_offsets),
                self.no_kv_cache,
                &self.non_granular_state,
                context_lens,
                position_ids,
                &flash_meta,
                flash_meta_full.as_ref().unwrap_or(&flash_meta),
            )?,
        };
        if return_raw_logits {
            Ok(ForwardInputsResult::RawLogits { logits })
        } else {
            Ok(ForwardInputsResult::CausalGeneration { logits })
        }
    }
    async fn sample_causal_gen(
        &self,
        seqs: &mut [&mut Sequence],
        logits: Vec<Tensor>,
        prefix_cacher: &mut PrefixCacheManagerV2,
        disable_eos_stop: bool,
        rng: Arc<std::sync::Mutex<Isaac64Rng>>,
    ) -> Result<(), candle_core::Error> {
        sample_and_add_toks(self, seqs, logits, prefix_cacher, disable_eos_stop, rng).await
    }
    fn category(&self) -> ModelCategory {
        ModelCategory::Text
    }
}

impl AnyMoePipelineMixin for NormalPipeline {
    fn amoe_finish_training(&mut self, gate_model_id: Option<String>) -> candle_core::Result<()> {
        self.parallel_models
            .par_iter_mut()
            .map(|model| {
                Arc::get_mut(model)
                    .unwrap()
                    .finish_training(gate_model_id.clone())
            })
            .collect::<candle_core::Result<Vec<_>>>()?;
        Ok(())
    }
    fn amoe_layer_vars(&self) -> Vec<Vec<Var>> {
        self.parallel_models
            .par_iter()
            .flat_map(|model| model.get_vars())
            .collect::<Vec<_>>()
    }
    fn amoe_base_model_trainable_params(&self) -> usize {
        self.parallel_models
            .par_iter()
            .map(|model| model.trainable_params())
            .sum()
    }
    fn amoe_take_cached_gating_outputs(&mut self) -> Vec<Tensor> {
        self.parallel_models
            .par_iter_mut()
            .flat_map(|model| Arc::get_mut(model).unwrap().take_cached_gating_outputs())
            .collect::<Vec<_>>()
    }
    fn amoe_create_layers(
        &mut self,
        model_ids: Vec<String>,
        token: &TokenSource,
        revision: Option<String>,
        match_regex: &str,
        config: crate::amoe::AnyMoeConfig,
        dtype: candle_core::DType,
        dev: &Device,
        (prefix, mlp): (String, String),
        layers: Vec<usize>,
        expert_type: AnyMoeExpertType,
        silent: bool,
        gate_model_id: Option<String>,
    ) -> candle_core::Result<()> {
        let mut vbs = Vec::new();
        // Precompile regex here
        let regex = Regex::new(match_regex).map_err(candle_core::Error::msg)?;
        for model_id in model_ids {
            let model_id_str = &model_id;
            let model_id = Path::new(&model_id);

            let api = {
                let mut api = ApiBuilder::new()
                    .with_progress(!silent)
                    .with_token(get_token(token).map_err(candle_core::Error::msg)?);
                if let Ok(x) = std::env::var("HF_HUB_CACHE") {
                    api = api.with_cache_dir(x.into());
                }
                api.build().map_err(candle_core::Error::msg)?
            };
            let revision = revision.clone().unwrap_or("main".to_string());
            let api = api.repo(Repo::with_revision(
                model_id_str.clone(),
                RepoType::Model,
                revision.clone(),
            ));

            let mut filenames = vec![];
            for rfilename in api_dir_list!(api, model_id).filter(|x| x.ends_with(".safetensors")) {
                filenames.push(api_get_file!(api, &rfilename, model_id));
            }

            let regex = regex.clone();
            let match_regex_clone = match_regex.to_string();
            let layers_clone = layers.clone();
            let vb = from_mmaped_safetensors(
                filenames,
                vec![],
                Some(dtype),
                dev,
                vec![None],
                silent,
                None,
                move |key| {
                    if regex.is_match(&key) {
                        // Idx of the last char of the layer id, +1
                        // Assumes N.MLP
                        let last_layer_idx = key.find(&match_regex_clone).unwrap() - 1;
                        let first_layer_idx = key[..last_layer_idx].rfind('.').unwrap();
                        let layer_n = key[first_layer_idx + 1..last_layer_idx]
                            .parse::<usize>()
                            .unwrap();
                        layers_clone.contains(&layer_n) || layers_clone.is_empty()
                    } else {
                        false
                    }
                },
                Arc::new(|_| DeviceForLoadTensor::Base),
            )?;
            vbs.push(vb);
        }

        let gate_vb = if let Some(gate_model_id) = gate_model_id {
            let model_id_str = &gate_model_id;
            let model_id = Path::new(&gate_model_id);

            let api = {
                let mut api = ApiBuilder::new()
                    .with_progress(!silent)
                    .with_token(get_token(token).map_err(candle_core::Error::msg)?);
                if let Ok(x) = std::env::var("HF_HUB_CACHE") {
                    api = api.with_cache_dir(x.into());
                }
                api.build().map_err(candle_core::Error::msg)?
            };
            let revision = revision.clone().unwrap_or("main".to_string());
            let api = api.repo(Repo::with_revision(
                model_id_str.clone(),
                RepoType::Model,
                revision.clone(),
            ));

            let mut gate_filenames = vec![];
            for rfilename in api_dir_list!(api, model_id).filter(|x| x.ends_with(".safetensors")) {
                gate_filenames.push(api_get_file!(api, &rfilename, model_id));
            }
            assert_eq!(
                gate_filenames.len(),
                1,
                "Gate model ID must contain only one .safetensors file"
            );

            let vb = from_mmaped_safetensors(
                gate_filenames.clone(),
                vec![],
                Some(dtype),
                dev,
                vec![None],
                silent,
                None,
                |_| true,
                Arc::new(|_| DeviceForLoadTensor::Base),
            )?;
            info!(
                "Loaded gating layers from `{}`",
                gate_filenames[0].display()
            );
            Some(vb)
        } else {
            None
        };

        self.parallel_models
            .iter_mut()
            .map(|model| {
                Arc::get_mut(model).unwrap().create_anymoe_layers(
                    vbs.clone(),
                    config.clone(),
                    (prefix.clone(), mlp.clone()),
                    layers.clone(),
                    expert_type.clone(),
                    gate_vb.clone(),
                )
            })
            .collect::<candle_core::Result<Vec<_>>>()?;

        Ok(())
    }
    fn amoe_supported(&self) -> bool {
        self.parallel_models
            .iter()
            .all(|model| model.amoe_supported())
    }
}
