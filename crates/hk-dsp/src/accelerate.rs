//! Apple Accelerate (vDSP) FFT provider (T-041), macOS with the `accelerate` feature.
//!
//! Links the system Accelerate framework (part of macOS; no crate, no redistribution) and
//! calls `vDSP_DFT_zop` directly. vDSP's DFT supports lengths `f·2^n` with `f ∈ {1, 3, 5, 15}`
//! and `n >= 3`; other lengths are refused (the registry falls back to rustfft). Two seams:
//!
//! - [`AccelerateFft`]: an [`FftBackend`] (split to real/imaginary arrays, transform, join);
//! - [`AccelerateSpectral`]: a [`SpectralBackend`] that windows straight into the split arrays
//!   and forms `|X|²` from the split output, skipping the interleave round trip.

use std::ffi::c_void;
use std::ptr::NonNull;

use num_complex::Complex32;

use crate::compute::SpectralBackend;
use crate::fft::FftBackend;
use crate::window::Window;

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vDSP_DFT_zop_CreateSetup(
        previous: *mut c_void,
        length: usize,
        direction: i32,
    ) -> *mut c_void;
    fn vDSP_DFT_Execute(
        setup: *const c_void,
        ir: *const f32,
        ii: *const f32,
        or: *mut f32,
        oi: *mut f32,
    );
    fn vDSP_DFT_DestroySetup(setup: *mut c_void);
}

/// `vDSP_DFT_FORWARD`.
const FORWARD: i32 = 1;

/// A vDSP DFT setup (owned).
struct Setup(NonNull<c_void>);

// SAFETY: a vDSP DFT setup is immutable after creation; Apple documents setups as shareable
// across threads for `vDSP_DFT_Execute`. We only move it between threads.
unsafe impl Send for Setup {}

impl Setup {
    fn new(n: usize) -> Result<Self, String> {
        // SAFETY: plain C call; a null `previous` creates a fresh setup; null return = refused.
        let raw = unsafe { vDSP_DFT_zop_CreateSetup(std::ptr::null_mut(), n, FORWARD) };
        NonNull::new(raw).map(Setup).ok_or_else(|| {
            format!("vDSP DFT does not support length {n} (needs f·2^n, f in 1/3/5/15, n >= 3)")
        })
    }

    /// Forward transform of split input into split output (all slices length `n`).
    fn execute(&self, ir: &[f32], ii: &[f32], or: &mut [f32], oi: &mut [f32]) {
        // SAFETY: the setup was created for `n` = every slice's length (checked by callers via
        // construction), input and output slices are distinct and valid for `n` elements.
        unsafe {
            vDSP_DFT_Execute(
                self.0.as_ptr(),
                ir.as_ptr(),
                ii.as_ptr(),
                or.as_mut_ptr(),
                oi.as_mut_ptr(),
            );
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        // SAFETY: created by vDSP_DFT_zop_CreateSetup and destroyed exactly once.
        unsafe { vDSP_DFT_DestroySetup(self.0.as_ptr()) }
    }
}

/// Accelerate forward FFT (unnormalised, FFT order).
pub struct AccelerateFft {
    setup: Setup,
    n: usize,
    ir: Vec<f32>,
    ii: Vec<f32>,
    or: Vec<f32>,
    oi: Vec<f32>,
}

impl AccelerateFft {
    /// Plans a transform of length `n`, or explains why vDSP cannot.
    pub fn new(n: usize) -> Result<Self, String> {
        Ok(Self {
            setup: Setup::new(n)?,
            n,
            ir: vec![0.0; n],
            ii: vec![0.0; n],
            or: vec![0.0; n],
            oi: vec![0.0; n],
        })
    }
}

impl FftBackend for AccelerateFft {
    fn len(&self) -> usize {
        self.n
    }

    fn name(&self) -> &'static str {
        "accelerate-vdsp"
    }

    fn forward(&mut self, buf: &mut [Complex32]) {
        assert_eq!(buf.len(), self.n, "buffer length != FFT length");
        for ((r, i), x) in self.ir.iter_mut().zip(self.ii.iter_mut()).zip(buf.iter()) {
            *r = x.re;
            *i = x.im;
        }
        self.setup
            .execute(&self.ir, &self.ii, &mut self.or, &mut self.oi);
        for ((x, &r), &i) in buf.iter_mut().zip(&self.or).zip(&self.oi) {
            *x = Complex32::new(r, i);
        }
    }
}

/// STFT rows through vDSP, one segment at a time (synchronous).
pub struct AccelerateSpectral {
    setup: Setup,
    window: Vec<f32>,
    ir: Vec<f32>,
    ii: Vec<f32>,
    or: Vec<f32>,
    oi: Vec<f32>,
    row: Vec<f32>,
}

impl AccelerateSpectral {
    /// Rows for `window`, or why vDSP cannot transform its length.
    pub fn new(window: &Window) -> Result<Self, String> {
        let n = window.len();
        Ok(Self {
            setup: Setup::new(n)?,
            window: window.coefficients().to_vec(),
            ir: vec![0.0; n],
            ii: vec![0.0; n],
            or: vec![0.0; n],
            oi: vec![0.0; n],
            row: vec![0.0; n],
        })
    }
}

impl SpectralBackend for AccelerateSpectral {
    fn name(&self) -> &'static str {
        "accelerate-vdsp"
    }

    fn fft_len(&self) -> usize {
        self.window.len()
    }

    fn submit(
        &mut self,
        span: &[Complex32],
        hop: usize,
        count: usize,
        sink: &mut dyn FnMut(&[f32]),
    ) {
        let n = self.window.len();
        assert!(span.len() >= (count.max(1) - 1) * hop + n, "span too short");
        let split = n - n / 2;
        for s in 0..count {
            let seg = &span[s * hop..s * hop + n];
            for (((r, i), x), &w) in self
                .ir
                .iter_mut()
                .zip(self.ii.iter_mut())
                .zip(seg)
                .zip(&self.window)
            {
                *r = x.re * w;
                *i = x.im * w;
            }
            self.setup
                .execute(&self.ir, &self.ii, &mut self.or, &mut self.oi);
            // fftshift: bin i <- FFT index (i + ceil(N/2)) mod N.
            let (neg, pos) = self.row.split_at_mut(n / 2);
            for ((p, &r), &i) in neg.iter_mut().zip(&self.or[split..]).zip(&self.oi[split..]) {
                *p = r * r + i * i;
            }
            for ((p, &r), &i) in pos.iter_mut().zip(&self.or[..split]).zip(&self.oi[..split]) {
                *p = r * r + i * i;
            }
            sink(&self.row);
        }
    }
}
