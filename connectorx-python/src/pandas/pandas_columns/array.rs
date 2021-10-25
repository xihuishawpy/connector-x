use super::{check_dtype, HasPandasColumn, PandasColumn, PandasColumnObject, GIL_MUTEX};
use crate::errors::ConnectorXPythonError;
use anyhow::anyhow;
use fehler::throws;
use ndarray::{ArrayViewMut2, Axis, Ix2};
use numpy::{npyffi::NPY_TYPES, Element, PyArray, PyArrayDescr};
use pyo3::{FromPyObject, Py, PyAny, PyResult, Python, ToPyObject};
use std::any::TypeId;
use std::marker::PhantomData;

#[derive(Clone)]
#[repr(transparent)]
pub struct PyList(Py<pyo3::types::PyList>);

// In order to put it into a numpy array
impl Element for PyList {
    const DATA_TYPE: numpy::DataType = numpy::DataType::Object;
    fn is_same_type(dtype: &PyArrayDescr) -> bool {
        unsafe { *dtype.as_dtype_ptr() }.type_num == NPY_TYPES::NPY_OBJECT as i32
    }
}

pub struct ArrayBlock<'a, V> {
    data: ArrayViewMut2<'a, PyList>,
    buf_size_mb: usize,
    _value_type: PhantomData<V>,
}

impl<'a, V> FromPyObject<'a> for ArrayBlock<'a, V> {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        check_dtype(ob, "object")?;
        let array = ob.downcast::<PyArray<PyList, Ix2>>()?;
        let data = unsafe { array.as_array_mut() };
        Ok(ArrayBlock::<V> {
            data,
            buf_size_mb: 16, // in MB
            _value_type: PhantomData,
        })
    }
}

impl<'a, V> ArrayBlock<'a, V> {
    #[throws(ConnectorXPythonError)]
    pub fn split(self) -> Vec<ArrayColumn<V>> {
        let mut ret = vec![];
        let mut view = self.data;

        let nrows = view.ncols();
        while view.nrows() > 0 {
            let (col, rest) = view.split_at(Axis(0), 1);
            view = rest;
            ret.push(ArrayColumn::<V> {
                data: col
                    .into_shape(nrows)?
                    .into_slice()
                    .ok_or_else(|| anyhow!("get None for splitted FloatArray data"))?
                    .as_mut_ptr(),
                lengths: vec![],
                row_idx: vec![],
                buffer: Vec::with_capacity(self.buf_size_mb * (1 << 17) * 11 / 10), // allocate a little bit more memory to avoid Vec growth
                buf_size: self.buf_size_mb * (1 << 17),
            })
        }
        ret
    }
}

pub struct ArrayColumn<V> {
    data: *mut PyList,
    buffer: Vec<V>,
    lengths: Vec<usize>, // usize::MAX if the string is None
    row_idx: Vec<usize>,
    buf_size: usize,
}

unsafe impl<V> Send for ArrayColumn<V> {}
unsafe impl<V> Sync for ArrayColumn<V> {}

impl<V> PandasColumnObject for ArrayColumn<V>
where
    V: Send + ToPyObject,
{
    fn typecheck(&self, id: TypeId) -> bool {
        id == TypeId::of::<PyList>() || id == TypeId::of::<Option<PyList>>()
    }

    fn typename(&self) -> &'static str {
        std::any::type_name::<PyList>()
    }

    #[throws(ConnectorXPythonError)]
    fn finalize(&mut self) {
        self.flush()?;
    }
}

impl PandasColumn<Vec<f64>> for ArrayColumn<f64> {
    #[throws(ConnectorXPythonError)]
    fn write(&mut self, val: Vec<f64>, row: usize) {
        self.lengths.push(val.len());
        self.buffer.extend_from_slice(&val[..]);
        self.row_idx.push(row);
        self.try_flush()?;
    }
}

impl PandasColumn<Option<Vec<f64>>> for ArrayColumn<f64> {
    #[throws(ConnectorXPythonError)]
    fn write(&mut self, val: Option<Vec<f64>>, row: usize) {
        match val {
            Some(v) => {
                self.lengths.push(v.len());
                self.buffer.extend_from_slice(&v[..]);
                self.row_idx.push(row);
                self.try_flush()?;
            }
            None => {
                self.lengths.push(usize::MAX);
                self.row_idx.push(row);
            }
        }
    }
}

impl PandasColumn<Vec<i64>> for ArrayColumn<i64> {
    #[throws(ConnectorXPythonError)]
    fn write(&mut self, val: Vec<i64>, row: usize) {
        self.lengths.push(val.len());
        self.buffer.extend_from_slice(&val[..]);
        self.row_idx.push(row);
        self.try_flush()?;
    }
}

impl PandasColumn<Option<Vec<i64>>> for ArrayColumn<i64> {
    #[throws(ConnectorXPythonError)]
    fn write(&mut self, val: Option<Vec<i64>>, row: usize) {
        match val {
            Some(v) => {
                self.lengths.push(v.len());
                self.buffer.extend_from_slice(&v[..]);
                self.row_idx.push(row);
                self.try_flush()?;
            }
            None => {
                self.lengths.push(usize::MAX);
                self.row_idx.push(row);
            }
        }
    }
}

impl HasPandasColumn for Vec<f64> {
    type PandasColumn<'a> = ArrayColumn<f64>;
}

impl HasPandasColumn for Option<Vec<f64>> {
    type PandasColumn<'a> = ArrayColumn<f64>;
}

impl HasPandasColumn for Vec<i64> {
    type PandasColumn<'a> = ArrayColumn<i64>;
}

impl HasPandasColumn for Option<Vec<i64>> {
    type PandasColumn<'a> = ArrayColumn<i64>;
}
impl<V> ArrayColumn<V>
where
    V: Send + ToPyObject,
{
    pub fn partition(self, counts: usize) -> Vec<ArrayColumn<V>> {
        let mut partitions = vec![];

        for _ in 0..counts {
            partitions.push(ArrayColumn {
                data: self.data,
                lengths: vec![],
                row_idx: vec![],
                buffer: Vec::with_capacity(self.buf_size),
                buf_size: self.buf_size,
            });
        }
        partitions
    }

    #[throws(ConnectorXPythonError)]
    pub fn flush(&mut self) {
        let nvecs = self.lengths.len();

        if nvecs > 0 {
            let py = unsafe { Python::assume_gil_acquired() };

            {
                // allocation in python is not thread safe
                let _guard = GIL_MUTEX
                    .lock()
                    .map_err(|e| anyhow!("mutex poisoned {}", e))?;
                let mut start = 0;
                for (i, &len) in self.lengths.iter().enumerate() {
                    if len != usize::MAX {
                        let end = start + len;
                        unsafe {
                            // allocate and write in the same time
                            *self.data.add(self.row_idx[i]) = PyList(
                                pyo3::types::PyList::new(py, &self.buffer[start..end]).into(),
                            );
                        };
                        start = end;
                    } else {
                        unsafe {
                            let n = unsafe { Py::from_borrowed_ptr(py, pyo3::ffi::Py_None()) };
                            *self.data.add(self.row_idx[i]) = PyList(n);
                        }
                    }
                }
            }

            self.buffer.truncate(0);
            self.lengths.truncate(0);
            self.row_idx.truncate(0);
        }
    }

    #[throws(ConnectorXPythonError)]
    pub fn try_flush(&mut self) {
        if self.buffer.len() >= self.buf_size {
            self.flush()?;
        }
    }
}
