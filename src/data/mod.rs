use crate::data::loading::{
    BatchLimitType, BatchedIterator, DataGen, PipelineIterator, TextIterationStrategy,
};
use crate::data::preprocessing::{labeling, preprocessing, LabelingConfig, PreprocessingConfig};
use crate::text::clean;
use crate::tokenization::{tokenizer, Tokenization, TokenizationInfo, Tokenizer, TokenizerConfig};
use crate::unicode::{normalize, Normalization};
use crate::utils::{py_invalid_type_error, py_required_key_error};
use crate::windows::{windows, WindowConfig};
use anyhow::anyhow;
use numpy::ndarray::prelude::*;
use numpy::{IntoPyArray, PyArray2, PyArrayDyn};
use pyo3::basic::CompareOp;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::vec::IntoIter;

use self::loading::{
    inference_data_generator_from_file, inference_data_generator_from_python,
    text_data_generator_from_files, BufferedIterator, ItemSize, Tensorize, TensorizedIterator,
    TextIterator,
};

pub mod loading;
pub mod preprocessing;

#[derive(Clone, Debug, PartialOrd, PartialEq, Ord, Eq, Hash)]
#[pyclass]
pub struct TextData {
    #[pyo3(get)]
    original: String,
    #[pyo3(get)]
    processed: String,
    #[pyo3(get)]
    language: Option<String>,
}

impl TextData {
    pub fn new(original: String, processed: Option<String>, language: Option<String>) -> Self {
        let processed = processed.unwrap_or(original.clone());
        TextData {
            original,
            processed,
            language,
        }
    }
}

#[pymethods]
impl TextData {
    #[new]
    #[args(processed = "None", language = "None")]
    fn new_py(
        original: String,
        processed: Option<String>,
        language: Option<String>,
    ) -> PyResult<Self> {
        Ok(Self::new(original, processed, language))
    }

    fn __hash__(&self) -> u64 {
        let mut s = DefaultHasher::new();
        self.hash(&mut s);
        s.finish()
    }

    fn __richcmp__(&self, other: &Self, op: CompareOp) -> bool {
        op.matches(self.cmp(other))
    }
}

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum Label {
    Classification(i32),
    SeqClassification(Vec<i32>),
    Seq2Seq(Vec<i32>),
}

impl IntoPy<PyObject> for Label {
    fn into_py(self, py: Python<'_>) -> PyObject {
        let d = PyDict::new(py);
        let label_type = match self {
            Label::Classification(label) => {
                d.set_item("label", label).unwrap();
                "classification"
            }
            Label::SeqClassification(labels) => {
                d.set_item("labels", labels).unwrap();
                "sequence_classification"
            }
            Label::Seq2Seq(labels) => {
                d.set_item("labels", labels).unwrap();
                "seq2seq"
            }
        };
        d.set_item("type", label_type).unwrap();
        d.into()
    }
}

impl<'a> FromPyObject<'a> for Label {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        let d: &PyDict = ob.extract()?;
        let Some(label_type) = d.get_item("type") else {
            return Err(py_required_key_error("type", "label"));
        };
        let label_type: String = label_type.extract()?;
        let label = match label_type.as_str() {
            "classification" => {
                let Some(label) = d.get_item("label") else {
                    return Err(py_required_key_error(
                        "label",
                        "classification label"));
                };
                Label::Classification(label.extract()?)
            }
            "sequence_classification" => {
                let Some(labels) = d.get_item("labels") else {
                    return Err(py_required_key_error(
                        "labels",
                        "sequence classification label"));
                };
                Label::SeqClassification(labels.extract()?)
            }
            "seq2seq" => {
                let Some(labels) = d.get_item("labels") else {
                    return Err(py_required_key_error(
                        "labels",
                        "seq2seq label",
                    ));
                };
                Label::Seq2Seq(labels.extract()?)
            }
            k => {
                return Err(py_invalid_type_error(k, "label"));
            }
        };
        Ok(label)
    }
}

#[derive(Clone, Debug)]
#[pyclass]
pub struct Item {
    #[pyo3(get)]
    data: TextData,
    #[pyo3(get)]
    tokenization: Tokenization,
    #[pyo3(get)]
    label: Label,
}

impl ItemSize for Item {
    fn size(&self) -> usize {
        self.tokenization.token_ids.len()
    }
}

impl Item {
    pub fn new(data: TextData, tokenization: Tokenization, label: Label) -> Self {
        Item {
            data,
            tokenization,
            label,
        }
    }
}

#[pymethods]
impl Item {
    fn __len__(&self) -> usize {
        self.size()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum InferenceDataFileFormat {
    Text,
    TextPlusDetections,
    TextPlusLanguage,
    TextPlusDetectionsPlusLanguage,
}

impl<'a> FromPyObject<'a> for InferenceDataFileFormat {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        let s: String = ob.extract()?;
        let format = match s.as_str() {
            "text" => InferenceDataFileFormat::Text,
            "text_detections" => InferenceDataFileFormat::TextPlusDetections,
            "text_language" => InferenceDataFileFormat::TextPlusLanguage,
            "text_detections_language" => InferenceDataFileFormat::TextPlusDetectionsPlusLanguage,
            k => return Err(py_invalid_type_error(k, "inference data file format")),
        };
        Ok(format)
    }
}

#[derive(Clone, Debug, PartialOrd, PartialEq, Ord, Eq, Hash)]
#[pyclass]
pub struct InferenceData {
    #[pyo3(get)]
    original: String,
    #[pyo3(get)]
    detections: Option<Vec<bool>>,
    #[pyo3(get)]
    language: Option<String>,
}

impl InferenceData {
    pub fn new(original: String, detections: Option<Vec<bool>>, language: Option<String>) -> Self {
        Self {
            original,
            detections,
            language,
        }
    }

    fn parse_detections(str: &str) -> Vec<bool> {
        str.split(char::is_whitespace)
            .map(|s| {
                str::parse::<u8>(s.trim())
                    .expect(format!("failed to parse {s} to integer").as_str())
                    != 0
            })
            .collect()
    }

    pub fn from_str(s: &str, format: &InferenceDataFileFormat) -> Self {
        let (original, detections, language) = match format {
            InferenceDataFileFormat::Text => (s, None, None),
            InferenceDataFileFormat::TextPlusDetections => {
                let splits: Vec<&str> = s.split("\t").collect();
                assert_eq!(splits.len(), 2);
                (splits[0], Some(Self::parse_detections(splits[1])), None)
            }
            InferenceDataFileFormat::TextPlusLanguage => {
                let splits: Vec<&str> = s.split("\t").collect();
                assert_eq!(splits.len(), 2);
                (splits[0], None, Some(splits[1].trim().to_string()))
            }
            InferenceDataFileFormat::TextPlusDetectionsPlusLanguage => {
                let splits: Vec<&str> = s.split("\t").collect();
                assert_eq!(splits.len(), 3);
                (
                    splits[0],
                    Some(Self::parse_detections(splits[1])),
                    Some(splits[2].trim().to_string()),
                )
            }
        };
        Self::new(original.trim().to_string(), detections, language)
    }
}

#[derive(Clone, Debug)]
#[pyclass]
pub struct InferenceItem {
    #[pyo3(get)]
    data: InferenceData,
    #[pyo3(get)]
    tokenization: Tokenization,
    #[pyo3(get)]
    item_idx: usize,
    #[pyo3(get)]
    window_idx: usize,
    #[pyo3(get)]
    window: (usize, usize, usize, usize),
}

impl InferenceItem {
    pub fn new(
        data: InferenceData,
        tokenization: Tokenization,
        item_idx: usize,
        window_idx: usize,
        window: (usize, usize, usize, usize),
    ) -> Self {
        InferenceItem {
            data,
            tokenization,
            item_idx,
            window_idx,
            window,
        }
    }
}

impl ItemSize for InferenceItem {
    fn size(&self) -> usize {
        self.tokenization.token_ids.len()
    }
}

#[pymethods]
impl InferenceItem {
    fn __len__(&self) -> usize {
        self.size()
    }
}

#[derive(Debug)]
pub struct Batch<T> {
    items: Vec<T>,
}

impl<T> Batch<T> {
    pub fn new(items: Vec<T>) -> Self {
        Batch { items }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[pyclass]
pub struct DataBatch {
    batch: Batch<Item>,
    #[pyo3(get)]
    tensorized: <Batch<Item> as Tensorize>::Output,
    iter: Option<Box<dyn Iterator<Item = Item> + Send>>,
}

#[pymethods]
impl DataBatch {
    fn __len__(&self) -> usize {
        self.batch.len()
    }

    #[getter]
    fn items(&self) -> Vec<Item> {
        self.batch.items.clone()
    }

    fn __iter__(mut slf: PyRefMut<'_, Self>) -> PyRefMut<'_, Self> {
        slf.iter = Some(Box::new(slf.batch.items.clone().into_iter()));
        slf
    }

    fn __next__(&mut self) -> Option<Py<Item>> {
        if let Some(item) = self.iter.as_mut().unwrap().next() {
            Some(Python::with_gil(|py| {
                Py::new(py, item).expect("should not fail")
            }))
        } else {
            None
        }
    }
}

#[inline]
fn max_groups<'a>(tokenizations: impl Iterator<Item = &'a Tokenization>) -> usize {
    #[inline]
    fn size(tokenization: &Tokenization) -> usize {
        match &tokenization.info {
            TokenizationInfo::TokenGroups(token_groups)
                if token_groups.contains_key("code_point_groups") =>
            {
                token_groups["code_point_groups"].len()
            }
            TokenizationInfo::TokenGroups(token_groups)
                if token_groups.contains_key("byte_groups") =>
            {
                token_groups["byte_groups"].len()
            }
            _ => tokenization.token_ids.len(),
        }
    }
    tokenizations.map(|t| size(t)).max().unwrap_or(0)
}

#[inline]
fn join<T>(vectors: Vec<Vec<T>>) -> Vec<T> {
    let mut joined = vec![];
    for mut v in vectors {
        joined.append(&mut v);
    }
    joined
}

#[inline]
fn prepare(
    tokenizations: Vec<&Tokenization>,
    pad_token_id: u32,
) -> (Py<PyArray2<u32>>, Vec<usize>, Py<PyDict>) {
    let batch_size = tokenizations.len();
    let max_token_ids = tokenizations
        .iter()
        .map(|t| t.token_ids.len())
        .max()
        .unwrap_or(0);
    let mut token_ids = Vec::with_capacity(max_token_ids * tokenizations.len());
    let mut lengths = Vec::with_capacity(tokenizations.len());
    Python::with_gil(|py| {
        let d = PyDict::new(py);
        for tokenization in tokenizations {
            let num_token_ids = tokenization.token_ids.len();
            token_ids.append(&mut join(vec![
                tokenization.token_ids.clone(),
                vec![pad_token_id; max_token_ids - num_token_ids],
            ]));
            lengths.push(num_token_ids);
        }
        let token_id_arr = Array2::from_shape_vec((batch_size, max_token_ids), token_ids).unwrap();
        (
            token_id_arr.into_pyarray(py).into_py(py),
            lengths,
            d.into_py(py),
        )
    })
}

impl Tensorize for Batch<Item> {
    type Output = (
        Py<PyArray2<u32>>,
        Vec<usize>,
        Py<PyDict>,
        Py<PyArrayDyn<i32>>,
    );

    fn tensorize(&self, tokenizer: &Tokenizer) -> Self::Output {
        assert!(!self.items.is_empty());
        let (token_id_arr, lengths, info) = prepare(
            self.items.iter().map(|i| &i.tokenization).collect(),
            tokenizer.pad_token_id(),
        );

        let batch_size = self.len();
        let max_groups = max_groups(self.items.iter().map(|i| &i.tokenization));
        let mut labels = Vec::with_capacity(batch_size * max_groups);

        for item in &self.items {
            labels.append(&mut match &item.label {
                Label::Classification(label) => vec![*label],
                Label::SeqClassification(labels) => join(vec![
                    vec![-1; tokenizer.num_prefix_tokens()],
                    labels.clone(),
                    vec![
                        -1;
                        max_groups.saturating_sub(tokenizer.num_prefix_tokens() + labels.len())
                    ],
                ]),
                Label::Seq2Seq(_) => todo!(),
            });
        }
        let label_arr = match labels.len() {
            n if n == batch_size => Array1::from_vec(labels).into_dyn(),
            n => Array2::from_shape_vec((batch_size, n / batch_size), labels)
                .unwrap()
                .into_dyn(),
        };
        Python::with_gil(|py| {
            (
                token_id_arr,
                lengths,
                info,
                label_arr.into_pyarray(py).into_py(py),
            )
        })
    }
}

#[pyclass]
pub struct InferenceBatch {
    batch: Batch<InferenceItem>,
    #[pyo3(get)]
    tensorized: <Batch<InferenceItem> as Tensorize>::Output,
    iter: Option<Box<dyn Iterator<Item = InferenceItem> + Send>>,
}

#[pymethods]
impl InferenceBatch {
    fn __len__(&self) -> usize {
        self.batch.len()
    }

    #[getter]
    fn items(&self) -> Vec<InferenceItem> {
        self.batch.items.clone()
    }

    fn __iter__(mut slf: PyRefMut<'_, Self>) -> PyRefMut<'_, Self> {
        slf.iter = Some(Box::new(slf.batch.items.clone().into_iter()));
        slf
    }

    fn __next__(&mut self) -> Option<Py<InferenceItem>> {
        if let Some(item) = self.iter.as_mut().unwrap().next() {
            Some(Python::with_gil(|py| {
                Py::new(py, item).expect("should not fail")
            }))
        } else {
            None
        }
    }
}

impl Tensorize for Batch<InferenceItem> {
    type Output = (Py<PyArray2<u32>>, Vec<usize>, Py<PyDict>);

    fn tensorize(&self, tokenizer: &Tokenizer) -> Self::Output {
        prepare(
            self.items.iter().map(|i| &i.tokenization).collect(),
            tokenizer.pad_token_id(),
        )
    }
}

impl<T> IntoIterator for Batch<T> {
    type Item = T;
    type IntoIter = IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

#[derive(Debug, Clone)]
#[pyclass]
pub struct PreprocessingPipelineConfig {
    #[pyo3(get)]
    preprocessing: Vec<PreprocessingConfig>,
    #[pyo3(get)]
    labeling: LabelingConfig,
}

impl PreprocessingPipelineConfig {
    pub fn new(preprocessing: Vec<PreprocessingConfig>, labeling: LabelingConfig) -> Self {
        PreprocessingPipelineConfig {
            preprocessing,
            labeling,
        }
    }
}

#[pymethods]
impl PreprocessingPipelineConfig {
    #[new]
    fn py_new(preprocessing: Vec<PreprocessingConfig>, labeling: LabelingConfig) -> PyResult<Self> {
        Ok(Self::new(preprocessing, labeling))
    }
}

pub type ApplyFn<I, O> = dyn Send + Sync + 'static + Fn(I, usize, Option<u64>) -> O;
pub struct Pipeline<I, O> {
    apply_fn: Arc<ApplyFn<I, O>>,
}
impl<I, O> Clone for Pipeline<I, O> {
    fn clone(&self) -> Self {
        Self {
            apply_fn: self.apply_fn.clone(),
        }
    }
}

impl<I, O> Pipeline<I, O> {
    pub fn apply(&self, input: I, idx: usize, seed: Option<u64>) -> O {
        (self.apply_fn)(input, idx, seed)
    }

    pub fn new(apply_fn: Arc<ApplyFn<I, O>>) -> Self {
        Self { apply_fn }
    }
}

pub type TextDataPipeline = Pipeline<TextData, anyhow::Result<Item>>;
impl TextDataPipeline {
    pub fn with_tokenizer(
        pipeline_cfg: PreprocessingPipelineConfig,
        tokenizer_cfg: TokenizerConfig,
    ) -> Self {
        let tok = tokenizer(tokenizer_cfg);
        let preprocess_fn = preprocessing(pipeline_cfg.preprocessing);
        let label_fn = labeling(pipeline_cfg.labeling);
        Pipeline::new(Arc::new(move |data, _, seed| -> anyhow::Result<Item> {
            let data = preprocess_fn(data, seed)?;
            Ok(Item {
                tokenization: tok.tokenize(&data.processed, data.language.as_deref()),
                label: label_fn(&data)?,
                data,
            })
        }))
    }
}

pub type InferencePipeline = Pipeline<InferenceData, anyhow::Result<Vec<InferenceItem>>>;
impl InferencePipeline {
    pub fn with_windows(
        tokenizer_cfg: TokenizerConfig,
        window_cfg: WindowConfig,
        normalization: Option<Normalization>,
        use_graphemes: bool,
    ) -> Self {
        let tok = tokenizer(tokenizer_cfg);
        Pipeline::new(Arc::new(move |data, idx, _| {
            let mut data = InferenceData {
                original: clean(&data.original, use_graphemes),
                ..data
            };
            if normalization.is_some() {
                data.original = normalize(&data.original, normalization.unwrap(), use_graphemes);
            }
            Ok(windows(&data.original, &window_cfg)?
                .iter()
                .enumerate()
                .map(|(w_idx, w)| {
                    let tokenization = tok.tokenize(w.str, data.language.as_deref());
                    let boundaries = w.boundaries();
                    InferenceItem::new(data.clone(), tokenization, idx, w_idx, boundaries)
                })
                .collect())
        }))
    }
}

#[pyclass]
struct InferenceLoader {
    iter: Box<
        dyn Iterator<
                Item = (
                    Batch<InferenceItem>,
                    <Batch<InferenceItem> as Tensorize>::Output,
                ),
            > + Send,
    >,
    iter_err: Arc<Mutex<Option<anyhow::Error>>>,
    #[pyo3(get)]
    min_items: usize,
    #[pyo3(get)]
    splits: Vec<usize>,
}

impl InferenceLoader {
    pub fn new(
        generators: Vec<Box<dyn DataGen<Item = anyhow::Result<InferenceData>>>>,
        tokenizer_config: TokenizerConfig,
        window_config: WindowConfig,
        normalization: Option<Normalization>,
        use_graphemes: bool,
        num_threads: u8,
        buffer_size: usize,
        batch_limit: usize,
        batch_limit_type: BatchLimitType,
        prefetch_factor: usize,
        sort: bool,
    ) -> anyhow::Result<Self> {
        let pipeline = InferencePipeline::with_windows(
            tokenizer_config.clone(),
            window_config,
            normalization,
            use_graphemes,
        );
        let splits: Vec<usize> = generators.iter().map(|g| g.min_len()).collect();
        let min_items = splits.iter().sum();
        let prefetch_factor = prefetch_factor.max(1);
        let text_iter = TextIterator::new(generators, TextIterationStrategy::Sequential, None)?;
        let iter_err = Arc::new(Mutex::new(None));
        let text_iter_err = iter_err.clone();
        let pipe_iter_err = iter_err.clone();
        let iter = text_iter
            .scan((), move |_, d| {
                if d.is_err() {
                    *text_iter_err.lock().unwrap() = Some(d.unwrap_err());
                    None
                } else {
                    d.ok()
                }
            })
            .pipe(&pipeline, num_threads, None)
            .scan((), move |_, i| {
                if i.is_err() {
                    *pipe_iter_err.lock().unwrap() = Some(i.unwrap_err());
                    None
                } else {
                    i.ok()
                }
            })
            .flatten()
            .batched(
                sort,
                false,
                prefetch_factor,
                batch_limit,
                batch_limit_type,
                None,
            )
            .tensorized(tokenizer_config)
            .buffered(buffer_size);
        Ok(InferenceLoader {
            iter: Box::new(iter),
            iter_err,
            min_items,
            splits,
        })
    }
}

#[pymethods]
impl InferenceLoader {
    #[staticmethod]
    #[args(
        normalization = "Normalization::NFKC",
        use_graphemes = "true",
        num_threads = "(num_cpus::get() as u8).min(4)",
        buffer_size = "128",
        batch_limit = "16",
        batch_limit_type = "BatchLimitType::BatchSize",
        prefetch_factor = "4",
        sort = "false"
    )]
    pub fn from_iterator(
        iterator: PyObject,
        tokenizer_config: TokenizerConfig,
        window_config: WindowConfig,
        normalization: Option<Normalization>,
        use_graphemes: bool,
        num_threads: u8,
        buffer_size: usize,
        batch_limit: usize,
        batch_limit_type: BatchLimitType,
        prefetch_factor: usize,
        sort: bool,
    ) -> anyhow::Result<Self> {
        let text_gen = Box::new(inference_data_generator_from_python(iterator));
        Self::new(
            vec![text_gen],
            tokenizer_config,
            window_config,
            normalization,
            use_graphemes,
            num_threads,
            buffer_size,
            batch_limit,
            batch_limit_type,
            prefetch_factor,
            sort,
        )
    }

    #[staticmethod]
    #[args(
        file_format = "InferenceDataFileFormat::Text",
        normalization = "Normalization::NFKC",
        use_graphemes = "true",
        languages = "None",
        num_threads = "(num_cpus::get() as u8).min(4)",
        buffer_size = "128",
        batch_limit = "16",
        batch_limit_type = "BatchLimitType::BatchSize",
        prefetch_factor = "4",
        sort = "false"
    )]
    pub fn from_files(
        files: Vec<String>,
        tokenizer_config: TokenizerConfig,
        window_config: WindowConfig,
        file_format: InferenceDataFileFormat,
        normalization: Option<Normalization>,
        use_graphemes: bool,
        languages: Option<Vec<String>>,
        num_threads: u8,
        buffer_size: usize,
        batch_limit: usize,
        batch_limit_type: BatchLimitType,
        prefetch_factor: usize,
        sort: bool,
    ) -> anyhow::Result<Self> {
        if files.is_empty() {
            return Err(anyhow!("files is empty"));
        }
        if languages.is_some() && files.len() != languages.as_ref().unwrap().len() {
            return Err(anyhow!(
                "there must be one language for every file if specified, but \
                    got {} files and {} languages",
                files.len(),
                languages.as_ref().unwrap().len()
            ));
        }
        let mut generators = vec![];
        for (idx, file) in files.iter().enumerate() {
            let lang = if languages.is_some() {
                Some(languages.as_ref().unwrap()[idx].clone())
            } else {
                None
            };
            let generator = inference_data_generator_from_file(Path::new(file), file_format, lang)?;
            generators.push(generator);
        }
        Self::new(
            generators,
            tokenizer_config,
            window_config,
            normalization,
            use_graphemes,
            num_threads,
            buffer_size,
            batch_limit,
            batch_limit_type,
            prefetch_factor,
            sort,
        )
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> anyhow::Result<Option<Py<InferenceBatch>>> {
        if let Some((batch, tensorized)) = self.iter.next() {
            Ok(Some(Python::with_gil(|py| {
                let inf_batch = InferenceBatch {
                    batch,
                    tensorized,
                    iter: None,
                };
                Py::new(py, inf_batch).expect("should not fail")
            })))
        } else {
            // check if batch is None because iterator is stopped,
            // or because an error was encountered
            match self.iter_err.lock().unwrap().as_ref() {
                Some(e) => Err(anyhow!("error during inference iterator: {e}")),
                None => Ok(None),
            }
        }
    }
}

type DataIter = dyn Iterator<Item = (Batch<Item>, <Batch<Item> as Tensorize>::Output)> + Send;
#[pyclass]
struct DataLoader {
    pipeline: TextDataPipeline,
    files: Vec<String>,
    languages: Option<Vec<String>>,
    strategy: TextIterationStrategy,
    tokenizer_config: TokenizerConfig,
    num_threads: u8,
    buffer_size: usize,
    batch_limit: usize,
    batch_limit_type: BatchLimitType,
    epoch: usize,
    fast_forward: usize,
    limit: usize,
    skip: usize,
    rank: usize,
    world_size: usize,
    seed: Option<u64>,
    shuffle: bool,
    prefetch_factor: usize,
    sort: bool,
    // the next to values will be set after each __iter__ call
    #[pyo3(get)]
    min_items: Option<usize>,
    iter: Option<Box<DataIter>>,
}

impl DataLoader {
    fn new(
        files: Vec<String>,
        languages: Option<Vec<String>>,
        pipeline_config: PreprocessingPipelineConfig,
        tokenizer_config: TokenizerConfig,
        strategy: TextIterationStrategy,
        num_threads: u8,
        buffer_size: usize,
        batch_limit: usize,
        batch_limit_type: BatchLimitType,
        shuffle: bool,
        prefetch_factor: usize,
        sort: bool,
        seed: Option<u64>,
        skip: usize,
        limit: Option<usize>,
        distributed: Option<(usize, usize)>,
    ) -> PyResult<Self> {
        if shuffle && seed.is_none() {
            return Err(PyTypeError::new_err(
                "seed cannot be None if shuffle is true",
            ));
        }
        let prefetch_factor = prefetch_factor.max(1);
        let pipeline = Pipeline::with_tokenizer(pipeline_config, tokenizer_config.clone());
        // handle distributed arguments
        let (rank, world_size) = distributed.unwrap_or((0, 1));
        assert!(
            rank < world_size,
            "rank {rank} is invalid given world size {world_size}"
        );
        let limit = limit.unwrap_or(usize::MAX);
        Ok(DataLoader {
            pipeline,
            files,
            languages,
            strategy,
            tokenizer_config,
            num_threads,
            buffer_size,
            batch_limit,
            batch_limit_type,
            iter: None,
            min_items: None,
            epoch: 0,
            fast_forward: 0,
            limit,
            skip,
            rank,
            world_size,
            seed,
            shuffle,
            prefetch_factor,
            sort,
        })
    }

    fn init_iter(&mut self) -> anyhow::Result<()> {
        let seed = if self.seed.is_some() {
            Some(self.seed.unwrap() + self.epoch as u64)
        } else {
            None
        };
        let mut generators = vec![];
        for (idx, file) in self.files.iter().enumerate() {
            let lang = if self.languages.is_some() {
                Some(self.languages.as_ref().unwrap()[idx].clone())
            } else {
                None
            };
            let generator = text_data_generator_from_files(Path::new(file), None, lang)?;
            generators.push(generator);
        }

        let text_iter = TextIterator::new(generators, self.strategy, seed)?;
        self.min_items = Some(
            text_iter
                .min_len()
                .min(self.limit)
                .saturating_sub(self.skip)
                / self.world_size,
        );
        let batch_iter = text_iter
            .take(self.limit)
            .skip(self.skip + self.fast_forward + self.rank)
            .step_by(self.world_size)
            .filter_map(|d| d.ok())
            .pipe(&self.pipeline, self.num_threads, seed)
            .filter_map(|i| i.ok())
            .batched(
                self.sort,
                self.shuffle,
                self.prefetch_factor,
                self.batch_limit,
                self.batch_limit_type,
                seed,
            )
            .tensorized(self.tokenizer_config.clone())
            .buffered(self.buffer_size);
        self.iter = Some(Box::new(batch_iter));
        Ok(())
    }
}

#[pymethods]
impl DataLoader {
    #[staticmethod]
    #[args(
        languages = "None",
        strategy = "TextIterationStrategy::Sequential",
        num_threads = "(num_cpus::get() as u8).min(4)",
        buffer_size = "128",
        batch_limit = "16",
        batch_limit_type = "BatchLimitType::BatchSize",
        shuffle = "false",
        prefetch_factor = "4",
        sort = "false",
        seed = "None",
        skip = "0",
        limit = "None",
        distributed = "None"
    )]
    pub fn from_files(
        files: Vec<String>,
        pipeline_config: PreprocessingPipelineConfig,
        tokenizer_config: TokenizerConfig,
        languages: Option<Vec<String>>,
        strategy: TextIterationStrategy,
        num_threads: u8,
        buffer_size: usize,
        batch_limit: usize,
        batch_limit_type: BatchLimitType,
        shuffle: bool,
        prefetch_factor: usize,
        sort: bool,
        seed: Option<u64>,
        skip: usize,
        limit: Option<usize>,
        distributed: Option<(usize, usize)>,
    ) -> PyResult<Self> {
        if files.is_empty() {
            return Err(PyTypeError::new_err("files is empty"));
        }
        if languages.is_some() && files.len() != languages.as_ref().unwrap().len() {
            return Err(PyTypeError::new_err(format!(
                "there must be one language for every file if specified, but \
                    got {} files and {} languages",
                files.len(),
                languages.as_ref().unwrap().len()
            )));
        }
        Self::new(
            files,
            languages,
            pipeline_config,
            tokenizer_config,
            strategy,
            num_threads,
            buffer_size,
            batch_limit,
            batch_limit_type,
            shuffle,
            prefetch_factor,
            sort,
            seed,
            skip,
            limit,
            distributed,
        )
    }

    fn __iter__(mut slf: PyRefMut<'_, Self>) -> anyhow::Result<PyRefMut<'_, Self>> {
        slf.init_iter()?;
        Ok(slf)
    }

    fn __next__(&mut self) -> anyhow::Result<Option<Py<DataBatch>>> {
        if self.iter.is_none() {
            self.init_iter()?;
        }
        if let Some((batch, tensorized)) = self.iter.as_mut().unwrap().next() {
            Ok(Some(Python::with_gil(|py| {
                let data_batch = DataBatch {
                    batch,
                    tensorized,
                    iter: None,
                };
                Py::new(py, data_batch).expect("should not fail")
            })))
        } else {
            Ok(None)
        }
    }

    fn set_epoch(&mut self, epoch: usize) {
        self.epoch = epoch;
    }

    fn set_fast_forward(&mut self, num_items: usize) {
        self.fast_forward = num_items
    }
}

/// A submodule containing functionality for text data loading.
/// Currently supported:
/// - loading text files
/// - loading in memory lists of strings
/// - several loading strategies (sequential, interleaved, weighted)
/// - single or multi-threaded preprocessing
/// - batched loading (limited by a max batch size or a max number of tokens)
/// - distributed loading (distribute work across multiple processes or machines)
pub(super) fn add_submodule(py: Python<'_>, parent_module: &PyModule) -> PyResult<()> {
    let m = PyModule::new(py, "data")?;
    m.add_class::<DataLoader>()?;
    m.add_class::<InferenceLoader>()?;
    m.add_class::<PreprocessingPipelineConfig>()?;
    m.add_class::<TextData>()?;
    m.add_class::<InferenceData>()?;
    m.add_class::<Item>()?;
    m.add_class::<InferenceItem>()?;
    m.add_class::<DataBatch>()?;
    m.add_class::<InferenceBatch>()?;
    parent_module.add_submodule(m)?;

    Ok(())
}
