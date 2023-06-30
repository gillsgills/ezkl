/// Representations of a computational graph's inputs.
pub mod input;
/// Crate for defining a computational graph and building a ZK-circuit from it.
pub mod model;
/// Representations of a computational graph's modules.
pub mod modules;
/// Inner elements of a computational graph that represent a single operation / constraints.
pub mod node;
/// Helper functions
pub mod utilities;
/// Representations of a computational graph's variables.
pub mod vars;

use halo2_proofs::circuit::Value;
pub use input::{DataSource, GraphWitness, WitnessSource};

#[cfg(not(target_arch = "wasm32"))]
use self::input::OnChainSourceInner;
use self::input::{FileSourceInner, GraphInput, WitnessFileSourceInner};
use crate::circuit::lookup::LookupOp;
use crate::circuit::modules::ModulePlanner;
use crate::circuit::CheckMode;
use crate::commands::RunArgs;
use crate::fieldutils::i128_to_felt;
use crate::graph::modules::ModuleInstanceOffset;
use crate::tensor::{Tensor, ValTensor};
use halo2_proofs::{
    circuit::Layouter,
    plonk::{Circuit, ConstraintSystem, Error as PlonkError},
};
use halo2curves::bn256::{self, Fr as Fp};
use halo2curves::ff::PrimeField;
use log::{error, info, trace};
pub use model::*;
pub use node::*;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use thiserror::Error;
pub use utilities::*;
pub use vars::*;

use self::modules::{
    GraphModules, ModuleConfigs, ModuleForwardResult, ModuleSettings, ModuleSizes,
};

/// circuit related errors.
#[derive(Debug, Error)]
pub enum GraphError {
    /// The wrong inputs were passed to a lookup node
    #[error("invalid inputs for a lookup node")]
    InvalidLookupInputs,
    /// Shape mismatch in circuit construction
    #[error("invalid dimensions used for node {0} ({1})")]
    InvalidDims(usize, String),
    /// Wrong method was called to configure an op
    #[error("wrong method was called to configure node {0} ({1})")]
    WrongMethod(usize, String),
    /// A requested node is missing in the graph
    #[error("a requested node is missing in the graph: {0}")]
    MissingNode(usize),
    /// The wrong method was called on an operation
    #[error("an unsupported method was called on node {0} ({1})")]
    OpMismatch(usize, String),
    /// This operation is unsupported
    #[error("unsupported operation in graph")]
    UnsupportedOp,
    /// A node has missing parameters
    #[error("a node is missing required params: {0}")]
    MissingParams(String),
    /// A node has missing parameters
    #[error("a node is has misformed params: {0}")]
    MisformedParams(String),
    /// Error in the configuration of the visibility of variables
    #[error("there should be at least one set of public variables")]
    Visibility,
    /// Ezkl only supports divisions by constants
    #[error("ezkl currently only supports division by constants")]
    NonConstantDiv,
    /// Ezkl only supports constant powers
    #[error("ezkl currently only supports constant exponents")]
    NonConstantPower,
    /// Error when attempting to rescale an operation
    #[error("failed to rescale inputs for {0}")]
    RescalingError(String),
    /// Error when attempting to load a model
    #[error("failed to load model")]
    ModelLoad,
    /// Packing exponent is too large
    #[error("largest packing exponent exceeds max. try reducing the scale")]
    PackingExponent,
}

const ASSUMED_BLINDING_FACTORS: usize = 6;

/// 26
const MAX_PUBLIC_SRS: u32 = bn256::Fr::S - 2;

/// Result from a forward pass
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ForwardResult {
    /// The inputs of the forward pass
    pub inputs: Vec<Tensor<Fp>>,
    /// The output of the forward pass
    pub outputs: Vec<Tensor<Fp>>,
    /// Any hashes of inputs generated during the forward pass
    pub processed_inputs: Option<ModuleForwardResult>,
    /// Any hashes of params generated during the forward pass
    pub processed_params: Option<ModuleForwardResult>,
    /// Any hashes of outputs generated during the forward pass
    pub processed_outputs: Option<ModuleForwardResult>,
    /// max lookup input
    pub max_lookup_input: i128,
}

/// model parameters
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphSettings {
    /// run args
    pub run_args: RunArgs,
    /// the potential number of constraints in the circuit
    pub num_constraints: usize,
    /// the shape of public inputs to the model (in order of appearance)
    pub model_instance_shapes: Vec<Vec<usize>>,
    /// model output scales
    pub model_output_scales: Vec<u32>,
    /// the of instance cells used by modules
    pub module_sizes: ModuleSizes,
    /// required_lookups
    pub required_lookups: Vec<LookupOp>,
    /// check mode
    pub check_mode: CheckMode,
}

impl GraphSettings {
    /// calculate the total number of instances
    pub fn total_instances(&self) -> Vec<usize> {
        let mut instances: Vec<usize> = self
            .model_instance_shapes
            .iter()
            .map(|x| x.iter().product())
            .collect();
        instances.extend(self.module_sizes.num_instances());

        instances
    }

    /// save params to file
    pub fn save(&self, path: &std::path::PathBuf) -> Result<(), std::io::Error> {
        let encoded = serde_json::to_string(&self)?;
        let mut file = std::fs::File::create(path)?;
        file.write_all(encoded.as_bytes())
    }
    /// load params from file
    pub fn load(path: &std::path::PathBuf) -> Result<Self, std::io::Error> {
        let mut file = std::fs::File::open(path)?;
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        let res = serde_json::from_str(&data)?;
        Ok(res)
    }
}

/// Configuration for a computational graph / model loaded from a `.onnx` file.
#[derive(Clone, Debug)]
pub struct GraphConfig {
    model_config: ModelConfig,
    module_configs: ModuleConfigs,
}

/// Defines the circuit for a computational graph / model loaded from a `.onnx` file.
#[derive(Clone, Debug, Default)]
pub struct GraphCircuit {
    /// The model / graph of computations.
    pub model: Model,
    /// Vector of input tensors to the model / graph of computations.
    pub inputs: Vec<Tensor<Fp>>,
    /// Vector of input tensors to the model / graph of computations.
    pub outputs: Vec<Tensor<Fp>>,
    /// The settings of the model / graph of computations.
    pub settings: GraphSettings,
    /// The settings of the model's modules.
    pub module_settings: ModuleSettings,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// The data source for a test
pub enum TestDataSource {
    /// The data is loaded from a file
    File,
    /// The data is loaded from the chain
    #[default]
    OnChain,
}

impl From<String> for TestDataSource {
    fn from(value: String) -> Self {
        match value.to_lowercase().as_str() {
            "file" => TestDataSource::File,
            "on-chain" => TestDataSource::OnChain,
            _ => panic!("not a valid test data source"),
        }
    }
}

#[derive(Clone, Debug, Default)]
///
pub struct TestSources {
    ///
    pub input: TestDataSource,
    ///
    pub output: TestDataSource,
}

///
#[derive(Clone, Debug, Default)]
pub struct TestOnChainData {
    /// The path to the test witness
    pub data: std::path::PathBuf,
    /// rpc endpoint
    pub rpc: Option<String>,
    ///
    pub data_sources: TestSources,
}

impl GraphCircuit {
    ///
    pub fn new(
        model: Model,
        run_args: RunArgs,
        check_mode: CheckMode,
    ) -> Result<GraphCircuit, Box<dyn std::error::Error>> {
        // placeholder dummy inputs - must call prepare_public_inputs to load data afterwards
        let mut inputs: Vec<Tensor<Fp>> = vec![];
        for shape in model.graph.input_shapes() {
            let t: Tensor<Fp> = Tensor::new(None, &shape).unwrap();
            inputs.push(t);
        }

        // dummy module settings, must load from GraphInput after
        let module_settings = ModuleSettings::default();

        let mut settings = model.gen_params(run_args, check_mode)?;

        let mut num_params = 0;
        if !model.const_shapes().is_empty() {
            for shape in model.const_shapes() {
                num_params += shape.iter().product::<usize>();
            }
        }

        let sizes = GraphModules::num_constraints_and_instances(
            model.graph.input_shapes(),
            vec![vec![num_params]],
            model.graph.output_shapes(),
            VarVisibility::from_args(run_args).unwrap(),
        );

        // number of instances used by modules
        settings.module_sizes = sizes.clone();

        // as they occupy independent rows
        settings.num_constraints = std::cmp::max(settings.num_constraints, sizes.max_constraints());

        Ok(GraphCircuit {
            model,
            inputs,
            outputs: vec![],
            settings,
            module_settings,
        })
    }

    ///
    pub fn new_from_settings(
        model: Model,
        mut settings: GraphSettings,
        check_mode: CheckMode,
    ) -> Result<GraphCircuit, Box<dyn std::error::Error>> {
        // placeholder dummy inputs - must call prepare_public_inputs to load data afterwards
        let mut inputs: Vec<Tensor<Fp>> = vec![];
        for shape in model.graph.input_shapes() {
            let t: Tensor<Fp> = Tensor::new(None, &shape).unwrap();
            inputs.push(t);
        }

        // dummy module settings, must load from GraphInput after
        let module_settings = ModuleSettings::default();

        settings.check_mode = check_mode;

        Ok(GraphCircuit {
            model,
            inputs,
            outputs: vec![],
            settings,
            module_settings,
        })
    }

    #[cfg(target_arch = "wasm32")]
    /// load inputs and outputs for the model
    pub fn load_graph_witness(
        &mut self,
        data: &GraphWitness,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.inputs =
            self.process_witness_source(&data.input_data, self.model.graph.input_shapes())?;
        self.outputs =
            self.process_witness_source(&data.output_data, self.model.graph.output_shapes())?;
        // load the module settings
        self.module_settings = ModuleSettings::from(data);

        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    /// load inputs and outputs for the model
    pub async fn load_graph_witness(
        &mut self,
        data: &GraphWitness,
        test_on_chain_data: Option<TestOnChainData>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut data = data.clone();

        // mutate it if need be
        if let Some(test_path) = test_on_chain_data {
            self.populate_on_chain_test_data(&mut data, test_path)
                .await?;
        } else {
            self.inputs = self
                .process_witness_source(
                    &data.input_data,
                    self.model.graph.input_shapes(),
                    self.model.graph.get_input_scales(),
                )
                .await?;
            self.outputs = self
                .process_witness_source(
                    &data.output_data,
                    self.model.graph.output_shapes(),
                    self.model.graph.get_output_scales(),
                )
                .await?;
        }

        // load the module settings
        self.module_settings = ModuleSettings::from(&data);

        Ok(())
    }

    /// Prepare the public inputs for the circuit.
    pub fn prepare_public_inputs(
        &mut self,
        data: &GraphWitness,
    ) -> Result<Vec<Vec<Fp>>, Box<dyn std::error::Error>> {
        // quantize the supplied data using the provided scale.
        // the ordering here is important, we want the inputs to come before the outputs
        // as they are configured in that order as Column<Instances>
        let mut public_inputs = vec![];
        if self.settings.run_args.input_visibility.is_public() {
            public_inputs = self.inputs.clone();
        }
        if self.settings.run_args.output_visibility.is_public() {
            public_inputs.extend(self.outputs.clone());
        }
        info!(
            "public inputs lengths: {:?}",
            public_inputs
                .iter()
                .map(|i| i.len())
                .collect::<Vec<usize>>()
        );
        trace!("{:?}", public_inputs);

        let mut pi_inner: Vec<Vec<Fp>> = public_inputs
            .iter()
            .map(|i| i.clone().into_iter().collect::<Vec<Fp>>())
            .collect::<Vec<Vec<Fp>>>();

        let module_instances =
            GraphModules::public_inputs(data, VarVisibility::from_args(self.settings.run_args)?);

        if !module_instances.is_empty() {
            pi_inner.extend(module_instances);
        }

        Ok(pi_inner)
    }

    ///
    #[cfg(target_arch = "wasm32")]
    pub fn load_graph_input(
        &mut self,
        data: &GraphInput,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let shapes = self.model.graph.input_shapes();
        let scales = vec![self.settings.run_args.scale; shapes.len()];
        self.inputs = self.process_data_source(&data.input_data, shapes, scales)?;
        Ok(())
    }

    ///
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn load_graph_input(
        &mut self,
        data: &GraphInput,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let shapes = self.model.graph.input_shapes();
        let scales = vec![self.settings.run_args.scale; shapes.len()];
        self.inputs = self
            .process_data_source(&data.input_data, shapes, scales)
            .await?;

        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    /// Process the data source for the model
    fn process_data_source(
        &mut self,
        data: &DataSource,
        shapes: Vec<Vec<usize>>,
        scales: Vec<u32>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        match &data {
            DataSource::OnChain(_) => {
                panic!("Cannot use on-chain data source as input for wasm rn.")
            }
            DataSource::File(file_data) => self.load_file_data(file_data, &shapes, scales),
        }
    }

    #[cfg(target_arch = "wasm32")]
    /// Process the data source for the model
    fn process_witness_source(
        &mut self,
        data: &WitnessSource,
        shapes: Vec<Vec<usize>>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        match &data {
            WitnessSource::OnChain(_) => {
                panic!("Cannot use on-chain data source as input for wasm rn.")
            }
            WitnessSource::File(file_data) => self.load_witness_file_data(file_data, &shapes),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    /// Process the data source for the model
    async fn process_data_source(
        &mut self,
        data: &DataSource,
        shapes: Vec<Vec<usize>>,
        scales: Vec<u32>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        match &data {
            DataSource::OnChain(source) => {
                let mut per_item_scale = vec![];
                for (i, shape) in shapes.iter().enumerate() {
                    per_item_scale.extend(vec![scales[i]; shape.iter().product::<usize>()]);
                }
                self.load_on_chain_data(source.clone(), &shapes, per_item_scale)
                    .await
            }
            DataSource::File(file_data) => self.load_file_data(file_data, &shapes, scales),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    /// Process the data source for the model
    async fn process_witness_source(
        &mut self,
        data: &WitnessSource,
        shapes: Vec<Vec<usize>>,
        scales: Vec<u32>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        match &data {
            WitnessSource::OnChain(source) => {
                let mut per_item_scale = vec![];
                for (i, shape) in shapes.iter().enumerate() {
                    per_item_scale.extend(vec![scales[i]; shape.iter().product::<usize>()]);
                }
                self.load_on_chain_data(source.clone(), &shapes, per_item_scale)
                    .await
            }
            WitnessSource::File(file_data) => self.load_witness_file_data(file_data, &shapes),
        }
    }

    /// Prepare on chain test data
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn load_on_chain_data(
        &mut self,
        source: OnChainSourceInner,
        shapes: &Vec<Vec<usize>>,
        scales: Vec<u32>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        use crate::eth::{evm_quantize, read_on_chain_inputs, setup_eth_backend};
        let (_, client) = setup_eth_backend(Some(&source.rpc)).await?;
        let inputs = read_on_chain_inputs(client.clone(), client.address(), &source.calls).await?;
        // quantize the supplied data using the provided scale + QuantizeData.sol
        let quantized_evm_inputs = evm_quantize(
            client,
            scales.into_iter().map(scale_to_multiplier).collect(),
            &inputs,
        )
        .await?;
        // on-chain data has already been quantized at this point. Just need to reshape it and push into tensor vector
        let mut inputs: Vec<Tensor<Fp>> = vec![];
        for (input, shape) in vec![quantized_evm_inputs].iter().zip(shapes) {
            let mut t: Tensor<Fp> = input.iter().cloned().collect();
            t.reshape(shape);
            inputs.push(t);
        }

        Ok(inputs)
    }

    ///
    pub fn load_file_data(
        &mut self,
        file_data: &FileSourceInner,
        shapes: &Vec<Vec<usize>>,
        scales: Vec<u32>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        // quantize the supplied data using the provided scale.
        let mut data: Vec<Tensor<Fp>> = vec![];
        for ((d, shape), scale) in file_data.iter().zip(shapes).zip(scales) {
            let t: Vec<Fp> = d
                .par_iter()
                .map(|x| i128_to_felt(quantize_float(x, 0.0, scale).unwrap()))
                .collect();

            let mut t: Tensor<Fp> = t.into_iter().into();
            t.reshape(shape);

            data.push(t);
        }
        Ok(data)
    }

    ///
    pub fn load_witness_file_data(
        &mut self,
        file_data: &WitnessFileSourceInner,
        shapes: &Vec<Vec<usize>>,
    ) -> Result<Vec<Tensor<Fp>>, Box<dyn std::error::Error>> {
        // quantize the supplied data using the provided scale.
        let mut data: Vec<Tensor<Fp>> = vec![];
        for (d, shape) in file_data.iter().zip(shapes) {
            let mut t: Tensor<Fp> = d.clone().into_iter().into();
            t.reshape(shape);
            data.push(t);
        }
        Ok(data)
    }

    /// Calibrate the circuit to the supplied data.
    pub fn calibrate(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let res = self.forward()?;
        let max_range = 2i128.pow(self.settings.run_args.bits as u32 - 1);
        if res.max_lookup_input > max_range {
            let recommended_bits = (res.max_lookup_input as f64).log2().ceil() as usize + 1;

            if recommended_bits <= (MAX_PUBLIC_SRS - 1) as usize {
                self.settings.run_args.bits = recommended_bits;
                self.settings.run_args.logrows = (recommended_bits + 1) as u32;
                return self.calibrate();
            } else {
                let err_string = format!("No possible value of bits (estimate {}) at scale {} can accomodate this value.", recommended_bits, self.settings.run_args.scale);
                return Err(err_string.into());
            }
        } else {
            let min_bits = (res.max_lookup_input as f64).log2().ceil() as usize + 1;

            let min_rows_from_constraints = (self.settings.num_constraints as f64
                + ASSUMED_BLINDING_FACTORS as f64)
                .log2()
                .ceil() as usize
                + 1;
            let mut logrows = std::cmp::max(min_bits + 1, min_rows_from_constraints);

            // ensure logrows is at least 4
            logrows = std::cmp::max(
                logrows,
                (ASSUMED_BLINDING_FACTORS as f64).ceil() as usize + 1,
            );

            logrows = std::cmp::min(logrows, MAX_PUBLIC_SRS as usize);

            info!(
                "setting bits to: {}, setting logrows to: {}",
                min_bits, logrows
            );
            self.settings.run_args.bits = min_bits;
            self.settings.run_args.logrows = logrows as u32;
        }

        self.settings = GraphCircuit::new(
            self.model.clone(),
            self.settings.run_args,
            self.settings.check_mode,
        )?
        .settings;

        Ok(())
    }

    /// Runs the forward pass of the model / graph of computations and any associated hashing.
    pub fn forward(&self) -> Result<ForwardResult, Box<dyn std::error::Error>> {
        let visibility = VarVisibility::from_args(self.settings.run_args)?;
        let mut processed_inputs = None;
        let mut processed_params = None;
        let mut processed_outputs = None;

        if visibility.input.requires_processing() {
            processed_inputs = Some(GraphModules::forward(&self.inputs, visibility.input)?);
        }

        if visibility.params.requires_processing() {
            let params = self.model.get_all_consts();
            let flattened_params = flatten_valtensors(params)?
                .get_felt_evals()?
                .into_iter()
                .into();
            processed_params = Some(GraphModules::forward(
                &[flattened_params],
                visibility.params,
            )?);
        }

        let outputs = self.model.forward(&self.inputs)?;

        if visibility.output.requires_processing() {
            processed_outputs = Some(GraphModules::forward(&outputs.outputs, visibility.output)?);
        }

        Ok(ForwardResult {
            inputs: self.inputs.clone(),
            outputs: outputs.outputs,
            processed_inputs,
            processed_params,
            processed_outputs,
            max_lookup_input: outputs.max_lookup_inputs,
        })
    }

    /// Create a new circuit from a set of input data and [RunArgs].
    pub fn from_run_args(
        run_args: &RunArgs,
        model_path: &std::path::PathBuf,
        check_mode: CheckMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let model = Model::from_run_args(run_args, model_path)?;
        Self::new(model, *run_args, check_mode)
    }

    /// Create a new circuit from a set of input data and [GraphSettings].
    pub fn from_settings(
        params: &GraphSettings,
        model_path: &std::path::PathBuf,
        check_mode: CheckMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let model = Model::from_run_args(&params.run_args, model_path)?;
        Self::new_from_settings(model, params.clone(), check_mode)
    }

    ///
    #[cfg(not(target_arch = "wasm32"))]
    async fn populate_on_chain_test_data(
        &mut self,
        data: &mut GraphWitness,
        test_on_chain_data: TestOnChainData,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Set up local anvil instance for reading on-chain data
        if matches!(
            test_on_chain_data.data_sources.input,
            TestDataSource::OnChain
        ) {
            // if not public then fail
            if !self.settings.run_args.input_visibility.is_public() {
                return Err("Cannot use on-chain data source as private data".into());
            }

            let input_data = match &data.input_data {
                WitnessSource::File(input_data) => input_data,
                WitnessSource::OnChain(_) => panic!(
                    "Cannot use on-chain data source as input for on-chain test. 
                    Will manually populate on-chain data from file source instead"
                ),
            };
            // Get the flatten length of input_data
            let length = input_data.iter().map(|x| x.len()).sum();
            let scales = vec![self.settings.run_args.scale; length];
            let datam: (Vec<Tensor<Fp>>, OnChainSourceInner) =
                OnChainSourceInner::test_from_file_data(
                    input_data,
                    scales,
                    self.model.graph.input_shapes(),
                    test_on_chain_data.rpc.as_deref(),
                )
                .await?;
            self.inputs = datam.0;
            data.input_data = datam.1.into();
        } else {
            self.inputs = self
                .process_witness_source(
                    &data.input_data,
                    self.model.graph.input_shapes(),
                    self.model.graph.get_input_scales(),
                )
                .await?;
        }
        if matches!(
            test_on_chain_data.data_sources.output,
            TestDataSource::OnChain
        ) {
            // if not public then fail
            if !self.settings.run_args.output_visibility.is_public() {
                return Err("Cannot use on-chain data source as private data".into());
            }

            let output_data = match &data.output_data {
                WitnessSource::File(output_data) => output_data,
                WitnessSource::OnChain(_) => panic!(
                    "Cannot use on-chain data source as output for on-chain test. 
                    Will manually populate on-chain data from file source instead"
                ),
            };
            let datum: (Vec<Tensor<Fp>>, OnChainSourceInner) =
                OnChainSourceInner::test_from_file_data(
                    output_data,
                    self.model.graph.get_output_scales(),
                    self.model.graph.output_shapes(),
                    test_on_chain_data.rpc.as_deref(),
                )
                .await?;
            self.outputs = datum.0;
            data.output_data = datum.1.into();
        } else {
            self.outputs = self
                .process_witness_source(
                    &data.input_data,
                    self.model.graph.input_shapes(),
                    self.model.graph.get_output_scales(),
                )
                .await?;
        }
        // Save the updated GraphInput struct to the data_path
        data.save(test_on_chain_data.data)?;
        Ok(())
    }
}

impl Circuit<Fp> for GraphCircuit {
    type Config = GraphConfig;
    type FloorPlanner = ModulePlanner;
    type Params = GraphSettings;

    fn without_witnesses(&self) -> Self {
        self.clone()
    }

    fn params(&self) -> Self::Params {
        // safe to clone because the model is Arc'd
        self.settings.clone()
    }

    fn configure_with_params(cs: &mut ConstraintSystem<Fp>, params: Self::Params) -> Self::Config {
        let visibility = VarVisibility::from_args(params.run_args).unwrap();

        let mut vars = ModelVars::new(
            cs,
            params.run_args.logrows as usize,
            params.num_constraints,
            params.model_instance_shapes.clone(),
            visibility.clone(),
            params.run_args.scale,
        );

        let base = Model::configure(
            cs,
            &mut vars,
            params.run_args.bits,
            params.required_lookups,
            params.check_mode,
        )
        .unwrap();

        let model_config = ModelConfig { base, vars };

        let module_configs = ModuleConfigs::from_visibility(cs, visibility, params.module_sizes);

        trace!(
            "log2_ceil of degrees {:?}",
            (cs.degree() as f32).log2().ceil()
        );

        GraphConfig {
            model_config,
            module_configs,
        }
    }

    fn configure(_: &mut ConstraintSystem<Fp>) -> Self::Config {
        unimplemented!("you should call configure_with_params instead")
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<Fp>,
    ) -> Result<(), PlonkError> {
        trace!("Setting input in synthesize");
        let mut inputs = self
            .inputs
            .iter()
            .map(|i| ValTensor::from(i.map(|x| Value::known(x))))
            .collect::<Vec<ValTensor<Fp>>>();

        let mut instance_offset = ModuleInstanceOffset::new();
        trace!("running input module layout");
        // we reserve module 0 for poseidon
        // we reserve module 1 for elgamal
        GraphModules::layout(
            &mut layouter,
            &config.module_configs,
            &mut inputs,
            self.settings.run_args.input_visibility,
            &mut instance_offset,
            &self.module_settings.input,
        )?;

        // now we need to flatten the params
        let mut flattened_params = vec![];
        if !self.model.get_all_consts().is_empty() {
            flattened_params =
                vec![
                    flatten_valtensors(self.model.get_all_consts()).map_err(|_| {
                        log::error!("failed to flatten params");
                        PlonkError::Synthesis
                    })?,
                ];
        }

        // now do stuff to the model params
        GraphModules::layout(
            &mut layouter,
            &config.module_configs,
            &mut flattened_params,
            self.settings.run_args.param_visibility,
            &mut instance_offset,
            &self.module_settings.params,
        )?;

        // now we need to assign the flattened params to the model
        let mut model = self.model.clone();
        if !self.model.get_all_consts().is_empty() {
            // now the flattened_params have been assigned to and we-assign them to the model consts such that they are constrained to be equal
            model.replace_consts(
                split_valtensor(flattened_params[0].clone(), self.model.const_shapes()).map_err(
                    |_| {
                        log::error!("failed to replace params");
                        PlonkError::Synthesis
                    },
                )?,
            );
        }

        // create a new module for the model (space 2)
        layouter.assign_region(|| "_new_module", |_| Ok(()))?;
        trace!("Laying out model");
        let mut outputs = model
            .layout(
                config.model_config.clone(),
                &mut layouter,
                &self.settings.run_args,
                &inputs,
                &config.model_config.vars,
            )
            .map_err(|e| {
                log::error!("{}", e);
                PlonkError::Synthesis
            })?;
        trace!("running output module layout");

        // this will re-enter module 0
        GraphModules::layout(
            &mut layouter,
            &config.module_configs,
            &mut outputs,
            self.settings.run_args.output_visibility,
            &mut instance_offset,
            &self.module_settings.output,
        )?;

        Ok(())
    }
}
