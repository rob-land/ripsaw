fn main() {
    use ripsaw::convert::hw::{encoder_smoke_test, probe_hw_support, HwBackend};
    use ripsaw::convert::plan::ConversionPlan;
    let codec = ConversionPlan::default_codec();
    let s = probe_hw_support();
    let resolved = s.resolve_auto(codec);
    println!("default = {:?}; Auto resolves -> {:?} ({})",
        ConversionPlan::default_hw_backend(), resolved, resolved.label());
    for b in [HwBackend::Qsv, HwBackend::Vaapi, HwBackend::Nvenc] {
        println!("  smoke_test {:?} = {}", b, encoder_smoke_test(b, codec, s.vaapi_device.as_deref()));
    }
}
