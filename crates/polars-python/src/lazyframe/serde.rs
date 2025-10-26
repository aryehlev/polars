use std::io::{BufReader, BufWriter, Read};

use pyo3::prelude::*;

use super::PyLazyFrame;
use crate::exceptions::ComputeError;
use crate::file::get_file_like;
use crate::prelude::*;
use crate::utils::EnterPolarsExt;

#[pymethods]
#[allow(clippy::should_implement_trait)]
impl PyLazyFrame {
    /// Serialize into binary data.
    fn serialize_binary(&self, py: Python<'_>, py_f: Py<PyAny>) -> PyResult<()> {
        let file = get_file_like(py_f, true)?;
        let writer = BufWriter::new(file);
        py.enter_polars(|| {
            self.ldf
                .read()
                .logical_plan
                .serialize_versioned(writer, Default::default())
        })
    }

    /// Serialize into a JSON string.
    #[cfg(feature = "json")]
    fn serialize_json(&self, py: Python<'_>, py_f: Py<PyAny>) -> PyResult<()> {
        let file = get_file_like(py_f, true)?;
        let writer = BufWriter::new(file);
        py.enter_polars(|| {
            serde_json::to_writer(writer, &self.ldf.read().logical_plan)
                .map_err(|err| ComputeError::new_err(err.to_string()))
        })
    }

    /// Deserialize a file-like object containing binary data into a LazyFrame.
    #[staticmethod]
    fn deserialize_binary(py: Python<'_>, py_f: Py<PyAny>) -> PyResult<Self> {
        let file = get_file_like(py_f, false)?;
        let reader = BufReader::new(file);

        let lp: DslPlan = py.enter_polars(|| DslPlan::deserialize_versioned(reader))?;
        Ok(LazyFrame::from(lp).into())
    }

    /// Deserialize a file-like object containing JSON string data into a LazyFrame.
    #[staticmethod]
    #[cfg(feature = "json")]
    fn deserialize_json(py: Python<'_>, py_f: Py<PyAny>) -> PyResult<Self> {
        // it is faster to first read to memory and then parse: https://github.com/serde-rs/json/issues/160
        // so don't bother with files.
        let mut json = String::new();
        get_file_like(py_f, false)?
            .read_to_string(&mut json)
            .unwrap();

        // SAFETY:
        // We skipped the serializing/deserializing of the static in lifetime in `DataType`
        // so we actually don't have a lifetime at all when serializing.

        // &str still has a lifetime. But it's ok, because we drop it immediately
        // in this scope.
        let json = unsafe { std::mem::transmute::<&'_ str, &'static str>(json.as_str()) };

        let lp = py.enter_polars(|| {
            serde_json::from_str::<DslPlan>(json)
                .map_err(|err| ComputeError::new_err(err.to_string()))
        })?;
        Ok(LazyFrame::from(lp).into())
    }

    /// Convert LazyFrame to a template (serializable without data).
    ///
    /// Replaces all data sources with placeholders, allowing you to serialize
    /// just the transformation logic and apply it to different datasets later.
    ///
    /// Example:
    ///     >>> template = lf.select([pl.col("x").log1p()]).serialize_template()
    ///     >>> # Later: deserialize and bind to new data
    ///     >>> result = template.bind_data(new_df)
    #[cfg(feature = "ir_serde")]
    fn serialize_template(&self, py: Python<'_>) -> PyResult<Vec<u8>> {
        py.enter_polars(|| {
            let template = self.ldf.read().clone().to_template()?;
            serde_json::to_vec(&template)
                .map_err(|err| polars_err!(ComputeError: "serialization failed: {}", err))
        })
    }

    /// Deserialize a template and bind it to a DataFrame.
    ///
    /// Args:
    ///     data: Serialized template bytes
    ///     df: DataFrame to bind the template to
    ///
    /// Returns:
    ///     LazyFrame with template applied to the DataFrame
    #[staticmethod]
    #[cfg(feature = "ir_serde")]
    fn deserialize_template_and_bind(
        py: Python<'_>,
        data: Vec<u8>,
        df: &PyDataFrame,
    ) -> PyResult<Self> {
        use polars_plan::plans::IRPlan;

        py.enter_polars(|| {
            let template: IRPlan = serde_json::from_slice(&data)
                .map_err(|err| polars_err!(ComputeError: "deserialization failed: {}", err))?;

            let bound = template.bind_to_df(std::sync::Arc::new(df.df.clone()))?;
            Ok(LazyFrame::from(bound).into())
        })
    }
}
