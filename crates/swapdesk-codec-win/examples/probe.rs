//! Prints the adapters, the H.264 encoder MFTs of this PC and what each answers for the `ICodecAPI` properties (risk R2).
//! Run: `cargo run -p swapdesk-codec-win --example probe`.

#[cfg(windows)]
fn main() {
    use swapdesk_codec_win::{list_adapters, probe_encoders};

    println!("# Adapters");
    for a in list_adapters().expect("adapters") {
        println!("- {} | LUID {:#018x} | vendor {:#06x} | software {}", a.name, a.id.0, a.vendor_id, a.software);
    }
    println!();
    println!("# Encoder MFTs (configured at 1920x1080, 60 fps, 20000 kbps, Automatica)");
    for p in probe_encoders().expect("probe") {
        println!();
        println!("## {} on {}", p.name, p.adapter.name);
        match &p.mft {
            Err(e) => println!("configure failed: {e}"),
            Ok(m) => {
                println!(
                    "CLSID {} | vendor `{}` | hardware {} | async {} | D3D11 input {} | profile {:?} | level {} | low latency {}",
                    m.clsid,
                    m.vendor,
                    m.hardware,
                    m.asynchronous,
                    m.d3d_input,
                    p.info.as_ref().map(|i| i.profile),
                    p.info.as_ref().map_or(0, |i| i.level),
                    p.info.as_ref().is_some_and(|i| i.low_latency)
                );
                println!();
                println!("| property | IsSupported | IsModifiable |");
                println!("|---|---|---|");
                for s in &p.support {
                    println!("| {} | {} | {} |", s.name, s.supported, s.modifiable);
                }
                println!();
                let modes: Vec<String> =
                    p.rate_modes.iter().map(|(m, ok)| format!("{m}={}", if *ok { "yes" } else { "no" })).collect();
                println!(
                    "Rate control modes accepted (0 CBR, 1 peak VBR, 2 VBR, 3 quality, 4 low-delay VBR, 5, 6): {}",
                    modes.join(", ")
                );
                println!();
                println!("| setting asked | value | supported | applied | HRESULT |");
                println!("|---|---|---|---|---|");
                for s in &p.settings {
                    let hr = s.hresult.map_or(String::from("-"), |h| format!("{:#010x}", h as u32));
                    println!("| {} | {} | {} | {} | {} |", s.name, s.value, s.supported, s.applied, hr);
                }
            }
        }
    }
    decoders();
}

#[cfg(windows)]
fn decoders() {
    use swapdesk_codec_win::probe_decoders;
    println!();
    println!("# Decoder MFTs");
    for d in probe_decoders().expect("decoder probe") {
        println!();
        println!(
            "## {} (CLSID {}, hardware {}, adapter {}, async {})",
            d.name,
            d.clsid,
            d.hardware,
            d.adapter.as_deref().unwrap_or("-"),
            d.asynchronous
        );
        match &d.activation {
            Err(e) => println!("activation failed: {e}"),
            Ok(()) => {
                println!();
                println!("| property | IsSupported | IsModifiable |");
                println!("|---|---|---|");
                for s in &d.support {
                    println!("| {} | {} | {} |", s.name, s.supported, s.modifiable);
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
