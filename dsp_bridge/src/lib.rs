use pyo3::prelude::*;
use numpy::{PyReadonlyArray1, PyArray1, IntoPyArray};
use rustfft::{FftPlanner, num_complex::Complex};
use std::sync::Arc;

#[pyclass]
pub struct FftHandler {
    fft: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    size: usize,
}

#[pymethods]
impl FftHandler {
    #[new]
    fn new(size: usize) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(size);
        
        // precompute Hann window
        let window: Vec<f32> = (0..size)
            .map(|i| 0.5 * (1.0 - f32::cos(2.0 * std::f32::consts::PI * i as f32 / (size - 1) as f32)))
            .collect();

        FftHandler { fft, window, size }
    }

    fn compute_db_spectrum<'py>(&self, py: Python<'py>, raw_data: PyReadonlyArray1<i16>) -> Bound<'py, PyArray1<f32>> {
        let input_data = raw_data.as_array();
        
        // transform to complex and apply window
        let mut buffer: Vec<Complex<f32>> = input_data
            .iter()
            .zip(self.window.iter())
            .map(|(&x, &w)| Complex::new(x as f32 * w, 0.0))
            .collect();

        self.fft.process(&mut buffer);

        // take magnitude and convert to dB
        let out_size = self.size / 2;
        let mut db_spectrum = Vec::with_capacity(out_size);
        
        for i in 0..out_size {
            let amplitude = buffer[i].norm();
            let db = 20.0 * (amplitude + 1e-6).log10();
            db_spectrum.push(db);
        }

        db_spectrum.into_pyarray(py)
    }
}

#[pymodule]
fn dsp_bridge(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<FftHandler>()?;
    Ok(())
}