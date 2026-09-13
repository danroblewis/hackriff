//! Runtime provider selection (T-041, ADR-0007): parsing, config spec, fallback with reasons,
//! refusal of non-conformant providers, and that the selected processors run.

mod common;

use common::*;
use hk_core::Discontinuity;
use hk_dsp::compute::{Compute, ComputeOptions, Preference, ProviderKind, Workload};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, PfbConfig, StftConfig, WelchConfig};

#[test]
fn preferences_parse_and_round_trip() {
    for (s, p) in [
        ("auto", Preference::Auto),
        ("CPU", Preference::Cpu),
        ("cpu-mt", Preference::CpuMt),
        ("accelerate", Preference::Accelerate),
        ("vdsp", Preference::Accelerate),
        ("gpu", Preference::Gpu),
        ("metal", Preference::Gpu),
        ("vulkan", Preference::Gpu),
        ("cuda", Preference::Cuda),
    ] {
        assert_eq!(s.parse::<Preference>().unwrap(), p, "{s}");
        assert_eq!(p.to_string().parse::<Preference>().unwrap(), p);
    }
    assert!("fpga".parse::<Preference>().is_err());
}

#[test]
fn options_are_a_serde_data_spec() {
    let o: ComputeOptions = serde_json::from_str(
        r#"{"provider": "cpu-mt", "pfb": "gpu", "threads": 4, "gpu_in_flight": 0}"#,
    )
    .unwrap();
    assert_eq!(o.provider, Preference::CpuMt);
    assert_eq!(o.pfb, Some(Preference::Gpu));
    assert_eq!(o.stft, None);
    assert_eq!(o.threads, Some(4));
    assert_eq!(o.gpu_in_flight, 0);
    let d: ComputeOptions = serde_json::from_str("{}").unwrap();
    assert_eq!(d, ComputeOptions::default());
    assert!(serde_json::from_str::<ComputeOptions>(r#"{"bogus": 1}"#).is_err());
    let back: ComputeOptions = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
    assert_eq!(back, o);
}

#[test]
fn cuda_is_refused_as_not_conformant_and_falls_back_to_cpu() {
    assert!(!ProviderKind::Cuda.conformant());
    let compute = Compute::new(ComputeOptions {
        provider: Preference::Cuda,
        ..ComputeOptions::default()
    });
    let (pfb, sel) = compute.pfb(PfbConfig::new(64)).unwrap();
    assert_eq!(sel.workload, Workload::Pfb);
    assert_eq!(sel.requested, Preference::Cuda);
    assert_ne!(sel.provider, ProviderKind::Cuda);
    let why = sel.fallback.expect("fallback reason");
    assert!(why.contains("cuda"), "{why}");
    assert_eq!(pfb.config(), &PfbConfig::new(64));
    let status = compute.status();
    assert_eq!(status.len(), ProviderKind::ALL.len());
    assert!(
        status
            .iter()
            .find(|s| s.kind == ProviderKind::CpuReference)
            .unwrap()
            .usable
            .is_ok()
    );
}

#[test]
fn uncompiled_providers_fall_back_with_a_reason() {
    for (pref, kind) in [
        (Preference::Gpu, ProviderKind::GpuWgpu),
        (Preference::Accelerate, ProviderKind::Accelerate),
    ] {
        if kind.compiled() {
            continue;
        }
        let compute = Compute::new(ComputeOptions {
            provider: pref,
            ..ComputeOptions::default()
        });
        let config = StftConfig::new(WelchConfig::new(1024), 4);
        let (_, sel) = compute.stft(config).unwrap();
        assert_ne!(sel.provider, kind);
        assert!(
            sel.fallback
                .as_deref()
                .is_some_and(|w| w.contains("not compiled")),
            "{sel}"
        );
    }
}

#[test]
fn explicit_cpu_is_used_without_fallback() {
    let compute = Compute::new(ComputeOptions {
        provider: Preference::Cpu,
        ..ComputeOptions::default()
    });
    let (stft, sel) = compute
        .stft(StftConfig::new(WelchConfig::new(512), 2))
        .unwrap();
    assert_eq!(sel.provider, ProviderKind::CpuReference);
    assert_eq!(sel.fallback, None);
    assert_eq!(stft.backend_name(), "cpu-rustfft");
    let (fft, sel) = compute.fft(1000);
    assert_eq!(
        (fft.len(), sel.provider),
        (1000, ProviderKind::CpuReference)
    );
}

#[test]
fn invalid_configs_are_errors_not_fallbacks() {
    let compute = Compute::new(ComputeOptions::default());
    assert!(compute.pfb(PfbConfig::new(7)).is_err());
    assert!(
        compute
            .stft(StftConfig::new(WelchConfig::new(2), 4))
            .is_err()
    );
}

/// Whatever `auto` picks in this build, the processors it returns produce output.
#[test]
fn auto_selected_processors_run() {
    let compute = Compute::new(ComputeOptions::default());
    let fs = 2e6;
    let prov = provenance(100e6, fs);
    let x = synth::complex_noise(&mut Rng::new(1), 100_000, 1e-2);
    let h = header(0, &prov, Discontinuity::NONE);

    let (mut stft, sel) = compute
        .stft(StftConfig::new(WelchConfig::new(4096), 4))
        .unwrap();
    eprintln!("{sel}");
    let mut frames = stft.push(InputInfo::from(&h), &x, |_| {});
    frames += stft.flush(|_| {});
    assert_eq!(frames, ((100_000 - 4096) / 2048 + 1) / 4);

    let (mut pfb, sel) = compute.pfb(PfbConfig::new(800)).unwrap();
    eprintln!("{sel}");
    let mut samples = pfb.process_c32(InputInfo::from(&h), &x).samples().len();
    while let Some(out) = pfb.flush() {
        samples += out.samples().len();
    }
    let taps = pfb.prototype().len();
    assert_eq!(samples, 800 * (1 + (100_000 - taps) / 400));
}
