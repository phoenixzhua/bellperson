use std::fs::File;
use std::path::PathBuf;

use ec_gpu::GpuEngine;
use ec_gpu_gen::fft::FftKernel;
use ec_gpu_gen::rust_gpu_tools::Device;
use fs2::FileExt;
use log::{debug, info, warn};
use pairing::Engine;

use crate::gpu::error::{GpuError, GpuResult};
use crate::gpu::CpuGpuMultiexpKernel;

const GPU_LOCK_NAME: &str = "bellman.gpu.lock";
const PRIORITY_LOCK_NAME: &str = "bellman.priority.lock";
fn tmp_path(filename: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(filename);
    p
}

/// `GPULock` prevents two kernel objects to be instantiated simultaneously.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug)]
pub struct GPULock(File);
impl GPULock {
    pub fn lock() -> GPULock {
        let gpu_lock_file = tmp_path(GPU_LOCK_NAME);
        debug!("Acquiring GPU lock at {:?} ...", &gpu_lock_file);
        let f = File::create(&gpu_lock_file)
            .unwrap_or_else(|_| panic!("Cannot create GPU lock file at {:?}", &gpu_lock_file));
        f.lock_exclusive().unwrap();
        debug!("GPU lock acquired!");
        GPULock(f)
    }
}
impl Drop for GPULock {
    fn drop(&mut self) {
        self.0.unlock().unwrap();
        debug!("GPU lock released!");
    }
}

/// `PrioriyLock` is like a flag. When acquired, it means a high-priority process
/// needs to acquire the GPU really soon. Acquiring the `PriorityLock` is like
/// signaling all other processes to release their `GPULock`s.
/// Only one process can have the `PriorityLock` at a time.
#[derive(Debug)]
pub struct PriorityLock(File);
impl PriorityLock {
    pub fn lock() -> PriorityLock {
        let priority_lock_file = tmp_path(PRIORITY_LOCK_NAME);
        debug!("Acquiring priority lock at {:?} ...", &priority_lock_file);
        let f = File::create(&priority_lock_file).unwrap_or_else(|_| {
            panic!(
                "Cannot create priority lock file at {:?}",
                &priority_lock_file
            )
        });
        f.lock_exclusive().unwrap();
        debug!("Priority lock acquired!");
        PriorityLock(f)
    }

    pub fn wait(priority: bool) {
        if !priority {
            if let Err(err) = File::create(tmp_path(PRIORITY_LOCK_NAME))
                .unwrap()
                .lock_exclusive()
            {
                warn!("failed to create priority log: {:?}", err);
            }
        }
    }

    pub fn should_break(priority: bool) -> bool {
        if priority {
            return false;
        }
        if let Err(err) = File::create(tmp_path(PRIORITY_LOCK_NAME))
            .unwrap()
            .try_lock_shared()
        {
            // Check that the error is actually a locking one
            if err.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                return true;
            } else {
                warn!("failed to check lock: {:?}", err);
            }
        }
        false
    }
}

impl Drop for PriorityLock {
    fn drop(&mut self) {
        self.0.unlock().unwrap();
        debug!("Priority lock released!");
    }
}

fn create_fft_kernel<'a, E>(priority: bool) -> Option<FftKernel<'a, E>>
where
    E: Engine + GpuEngine,
{
    let devices = Device::all();
    let kernel = if priority {
        FftKernel::create_with_abort(&devices, &|| -> bool {
            // We only supply a function in case it is high priority, hence always passing in
            // `true`.
            PriorityLock::should_break(true)
        })
    } else {
        FftKernel::create(&devices)
    };
    match kernel {
        Ok(k) => {
            info!("GPU FFT kernel instantiated!");
            Some(k)
        }
        Err(e) => {
            warn!("Cannot instantiate GPU FFT kernel! Error: {}", e);
            None
        }
    }
}

fn create_multiexp_kernel<'a, E>(priority: bool) -> Option<CpuGpuMultiexpKernel<'a, E>>
where
    E: Engine + GpuEngine,
{
    let devices = Device::all();
    let kernel = if priority {
        CpuGpuMultiexpKernel::create_with_abort(&devices, &|| -> bool {
            // We only supply a function in case it is high priority, hence always passing in
            // `true`.
            PriorityLock::should_break(true)
        })
    } else {
        CpuGpuMultiexpKernel::create(&devices)
    };
    match kernel {
        Ok(k) => {
            info!("GPU Multiexp kernel instantiated!");
            Some(k)
        }
        Err(e) => {
            warn!("Cannot instantiate GPU Multiexp kernel! Error: {}", e);
            None
        }
    }
}

macro_rules! locked_kernel {
    ($class:ident, $kern:ident, $func:ident, $name:expr) => {
        #[allow(clippy::upper_case_acronyms)]
        pub struct $class<'a, E>
        where
            E: pairing::Engine + ec_gpu::GpuEngine,
        {
            priority: bool,
            kernel: Option<$kern<'a, E>>,
            // There should always be only one thing running on the GPU, hence create a
            // lock. It is set when a kernel is initiallized and released when the kernel is freed.
            gpu_lock: Option<GPULock>,
        }

        impl<'a, E> $class<'a, E>
        where
            E: pairing::Engine + ec_gpu::GpuEngine,
        {
            pub fn new(priority: bool) -> $class<'a, E> {
                $class::<E> {
                    priority,
                    kernel: None,
                    gpu_lock: None,
                }
            }

            fn init(&mut self) {
                if self.kernel.is_none() {
                    PriorityLock::wait(self.priority);
                    info!("GPU is available for {}!", $name);
                    self.gpu_lock = Some(GPULock::lock());
                    self.kernel = $func::<E>(self.priority);
                }
            }

            fn free(&mut self) {
                if let Some(_kernel) = self.kernel.take() {
                    warn!(
                        "GPU acquired by a high priority process! Freeing up {} kernels...",
                        $name
                    );
                    self.gpu_lock.take();
                }
            }

            pub fn with<F, R>(&mut self, mut f: F) -> GpuResult<R>
            where
                F: FnMut(&mut $kern<E>) -> GpuResult<R>,
            {
                if std::env::var("BELLMAN_NO_GPU").is_ok() {
                    return Err(GpuError::GpuDisabled);
                }

                self.init();

                loop {
                    if let Some(ref mut k) = self.kernel {
                        match f(k) {
                            Err(GpuError::GpuTaken) => {
                                self.free();
                                self.init();
                            }
                            Err(e) => {
                                warn!("GPU {} failed! Falling back to CPU... Error: {}", $name, e);
                                return Err(e);
                            }
                            Ok(v) => return Ok(v),
                        }
                    } else {
                        return Err(GpuError::KernelUninitialized);
                    }
                }
            }
        }
    };
}

locked_kernel!(LockedFFTKernel, FftKernel, create_fft_kernel, "FFT");
locked_kernel!(
    LockedMultiexpKernel,
    CpuGpuMultiexpKernel,
    create_multiexp_kernel,
    "Multiexp"
);
