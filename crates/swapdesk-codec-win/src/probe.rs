//! The R2 probe: which H.264 encoder MFTs this PC has, and what each one answers for the `ICodecAPI` properties.

use swapdesk_platform::{EncoderConfig, EncoderInfo, Result, VideoEncoder};
use swapdesk_proto::Preset;

use crate::encoder::{EncoderOptions, MfH264Encoder, MftChoice, MftSummary, PropertySupport, SettingOutcome};
use crate::mft::{self, Category};
use crate::sys::{AdapterDesc, list_adapters};

/// One encoder MFT as configured for 1920x1080 at 60 fps (Automática).
#[derive(Debug, Clone)]
pub struct EncoderProbe {
    /// The adapter the encoder ran on (frames and conversion).
    pub adapter: AdapterDesc,
    /// The friendly name of the MFT asked for.
    pub name: String,
    /// The summary when the MFT configured; the error text when it did not.
    pub mft: std::result::Result<MftSummary, String>,
    pub info: Option<EncoderInfo>,
    pub support: Vec<PropertySupport>,
    pub settings: Vec<SettingOutcome>,
    /// `AVEncCommonRateControlMode` values 0 to 6 and whether the MFT took each.
    pub rate_modes: Vec<(u32, bool)>,
}

/// Every encoder MFT the machine offers: the hardware MFTs of each hardware adapter (`MFTEnum2` with its LUID) and the
/// software MFT (on the first hardware adapter, for the colour conversion).
pub fn probe_encoders() -> Result<Vec<EncoderProbe>> {
    let adapters = list_adapters()?;
    let mut out = Vec::new();
    let mut first_hardware = None;
    for adapter in adapters.iter().filter(|a| !a.software) {
        first_hardware.get_or_insert_with(|| adapter.clone());
        for m in mft::hardware(Category::Encoder, Some(adapter.id))? {
            out.push(run(adapter, &m.name, &mft::guid_text(&m.clsid)));
        }
    }
    if let Some(adapter) = first_hardware {
        for m in mft::software(Category::Encoder)? {
            out.push(run(&adapter, &m.name, &mft::guid_text(&m.clsid)));
        }
    }
    Ok(out)
}

/// One decoder MFT and what it answers.
#[derive(Debug, Clone)]
pub struct DecoderProbe {
    pub name: String,
    pub clsid: String,
    pub hardware: bool,
    /// The adapter it was enumerated for (hardware MFTs).
    pub adapter: Option<String>,
    pub asynchronous: bool,
    /// The activation error, when there is one.
    pub activation: std::result::Result<(), String>,
    pub support: Vec<PropertySupport>,
}

/// Every H.264 decoder MFT: hardware ones per adapter, then the software (inbox) one.
pub fn probe_decoders() -> Result<Vec<DecoderProbe>> {
    let mut found: Vec<(mft::Mft, Option<String>)> = Vec::new();
    for adapter in list_adapters()?.into_iter().filter(|a| !a.software) {
        for m in mft::hardware(Category::Decoder, Some(adapter.id))? {
            found.push((m, Some(adapter.name.clone())));
        }
    }
    for m in mft::software(Category::Decoder)? {
        found.push((m, None));
    }
    let mut out = Vec::new();
    for (m, adapter) in found {
        let clsid = mft::guid_text(&m.clsid);
        match m.activate() {
            Err(e) => out.push(DecoderProbe {
                name: m.name.clone(),
                clsid,
                hardware: m.hardware,
                adapter,
                asynchronous: false,
                activation: Err(e.to_string()),
                support: Vec::new(),
            }),
            Ok(t) => {
                let support = match mft::codec_api(&t) {
                    Some(c) => mft::decoder_properties()
                        .into_iter()
                        .map(|(name, key)| PropertySupport {
                            name,
                            supported: crate::encoder::is_supported(&c, &key),
                            // SAFETY: IsModifiable only reads the property state.
                            modifiable: unsafe { c.IsModifiable(&key) }.0 == 0,
                        })
                        .collect(),
                    None => Vec::new(),
                };
                out.push(DecoderProbe {
                    name: m.name.clone(),
                    clsid,
                    hardware: m.hardware,
                    adapter,
                    asynchronous: mft::is_async(&t),
                    activation: Ok(()),
                    support,
                });
            }
        }
    }
    Ok(out)
}

fn run(adapter: &AdapterDesc, name: &str, clsid: &str) -> EncoderProbe {
    let mut enc =
        MfH264Encoder::new(EncoderOptions { choice: MftChoice::Clsid(clsid.to_string()), ..Default::default() });
    let cfg = EncoderConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        kbps: 20_000,
        preset: Preset::Auto,
        adapter: adapter.id,
        codec: swapdesk_platform::Codec::H264,
    };
    match enc.configure(&cfg) {
        Ok(info) => EncoderProbe {
            adapter: adapter.clone(),
            name: name.to_string(),
            mft: Ok(enc.mft().cloned().expect("configured encoder has an MFT")),
            info: Some(info),
            support: enc.property_support().to_vec(),
            settings: enc.settings().to_vec(),
            rate_modes: enc.rate_modes().to_vec(),
        },
        Err(e) => EncoderProbe {
            adapter: adapter.clone(),
            name: name.to_string(),
            mft: Err(e.to_string()),
            info: None,
            support: Vec::new(),
            settings: Vec::new(),
            rate_modes: Vec::new(),
        },
    }
}
