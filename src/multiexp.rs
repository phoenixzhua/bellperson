use std::sync::Arc;

use ec_gpu_gen::multiexp_cpu::{multiexp_cpu, QueryDensity, SourceBuilder};
use ec_gpu_gen::threadpool::{Waiter, Worker};
use ec_gpu_gen::EcError;
use ff::PrimeField;
use group::prime::PrimeCurveAffine;
use pairing::Engine;

use crate::gpu;

/// Perform multi-exponentiation. The caller is responsible for ensuring the
/// query size is the same as the number of exponents.
#[cfg(any(feature = "cuda", feature = "opencl"))]
pub fn multiexp<'b, Q, D, G, E, S>(
    pool: &Worker,
    bases: S,
    density_map: D,
    exponents: Arc<Vec<<G::Scalar as PrimeField>::Repr>>,
    kern: &mut gpu::LockedMultiexpKernel<E>,
) -> Waiter<Result<<G as PrimeCurveAffine>::Curve, EcError>>
where
    for<'a> &'a Q: QueryDensity,
    D: Send + Sync + 'static + Clone + AsRef<Q>,
    G: PrimeCurveAffine,
    E: gpu::GpuEngine,
    E: Engine<Fr = G::Scalar>,
    S: SourceBuilder<G>,
{
    // Try to run on the GPU.
    if let Ok(p) = kern.with(|k: &mut gpu::CpuGpuMultiexpKernel<E>| {
        let exps = density_map.as_ref().generate_exps::<E>(exponents.clone());
        let (bss, skip) = bases.clone().get();
        k.multiexp(pool, bss, exps, skip).map_err(Into::into)
    }) {
        return Waiter::done(Ok(p));
    }

    // Fallback to the CPU in case the GPU run failed.
    let result_cpu = multiexp_cpu::<_, _, _, E, _>(pool, bases, density_map, exponents);

    // Do not give the control back to the caller till the multiexp is done. Once done the GPU
    // might again be free, so we can run subsequent calls on the GPU instead of the CPU again.
    let result = result_cpu.wait();

    Waiter::done(result)
}

#[cfg(not(any(feature = "cuda", feature = "opencl")))]
pub fn multiexp<'b, Q, D, G, E, S>(
    pool: &Worker,
    bases: S,
    density_map: D,
    exponents: Arc<Vec<<G::Scalar as PrimeField>::Repr>>,
    _kern: &mut gpu::LockedMultiexpKernel<E>,
) -> Waiter<Result<<G as PrimeCurveAffine>::Curve, EcError>>
where
    for<'a> &'a Q: QueryDensity,
    D: Send + Sync + 'static + Clone + AsRef<Q>,
    G: PrimeCurveAffine,
    E: gpu::GpuEngine,
    E: Engine<Fr = G::Scalar>,
    S: SourceBuilder<G>,
{
    multiexp_cpu::<_, _, _, E, _>(pool, bases, density_map, exponents)
}
