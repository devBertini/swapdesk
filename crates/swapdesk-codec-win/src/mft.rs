//! Finding H.264 MFTs: `MFTEnum2` with the adapter LUID for hardware, `MFTEnumEx` for the software transform, and the
//! table of `ICodecAPI` properties the probe asks every encoder about (risk R2).

use swapdesk_platform::{AdapterId, Result};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFAttributes, IMFShutdown, IMFTransform, MF_TRANSFORM_ASYNC, MFCreateAttributes,
    MFMediaType_Video, MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFT_TRANSFORM_CLSID_Attribute, MFTEnum2, MFTEnumEx,
    MFVideoFormat_H264,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface};

use crate::sys::{Ctx, id_to_luid, init_thread};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    Encoder,
    Decoder,
}

/// A registered MFT that can be activated.
#[derive(Clone)]
pub(crate) struct Mft {
    pub activate: IMFActivate,
    pub name: String,
    pub clsid: GUID,
    pub hardware: bool,
    pub vendor: String,
}

fn read_string(attrs: &IMFAttributes, key: &GUID) -> String {
    // SAFETY: the buffer is sized from GetStringLength and passed with its length.
    unsafe {
        let Ok(len) = attrs.GetStringLength(key) else { return String::new() };
        let mut buf = vec![0u16; len as usize + 1];
        if attrs.GetString(key, &mut buf, None).is_err() {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

fn collect(ptr: *mut Option<IMFActivate>, count: u32, hardware: bool) -> Vec<Mft> {
    let mut out = Vec::new();
    if ptr.is_null() {
        return out;
    }
    // SAFETY: MFTEnum2/MFTEnumEx returned `count` activation objects in a CoTaskMem array; each is moved out once
    // and the array is freed after.
    unsafe {
        for i in 0..count as usize {
            let Some(activate) = (*ptr.add(i)).take() else { continue };
            let attrs: IMFAttributes = match activate.cast() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let name = read_string(&attrs, &MFT_FRIENDLY_NAME_Attribute);
            let vendor = read_string(&attrs, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
            let clsid = attrs.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).unwrap_or_default();
            out.push(Mft { activate, name, clsid, hardware, vendor });
        }
        CoTaskMemFree(Some(ptr as *const _));
    }
    // MFTEnum2 can list one transform twice (Quick Sync appears once per engine on this PC): one entry per CLSID.
    let mut seen: Vec<GUID> = Vec::new();
    out.retain(|m| {
        if seen.contains(&m.clsid) {
            false
        } else {
            seen.push(m.clsid);
            true
        }
    });
    out
}

/// Hardware MFTs of the category for the H.264 codec. With an adapter, `MFTEnum2` with `MFT_ENUM_ADAPTER_LUID` returns
/// only the MFTs of that GPU; without one, `MFTEnumEx` returns all of them.
pub(crate) fn hardware(cat: Category, adapter: Option<AdapterId>) -> Result<Vec<Mft>> {
    init_thread()?;
    let h264 = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let none = MFT_REGISTER_TYPE_INFO::default();
    let (cat_guid, input, output) = match cat {
        Category::Encoder => (MFT_CATEGORY_VIDEO_ENCODER, None, Some(&h264 as *const _)),
        Category::Decoder => (MFT_CATEGORY_VIDEO_DECODER, Some(&h264 as *const _), None),
    };
    let _ = none;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out pointers are valid; `attrs` outlives the call.
    unsafe {
        match adapter {
            Some(id) => {
                let mut attrs: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut attrs, 1).ctx("MFCreateAttributes")?;
                let attrs = attrs.expect("MFCreateAttributes returned an attribute store");
                let (low, high) = id_to_luid(id);
                let mut blob = [0u8; 8];
                blob[..4].copy_from_slice(&low.to_le_bytes());
                blob[4..].copy_from_slice(&high.to_le_bytes());
                attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob).ctx("SetBlob(MFT_ENUM_ADAPTER_LUID)")?;
                MFTEnum2(cat_guid, flags, input, output, &attrs, &mut ptr, &mut count).ctx("MFTEnum2")?;
            }
            None => {
                MFTEnumEx(cat_guid, flags, input, output, &mut ptr, &mut count).ctx("MFTEnumEx(hardware)")?;
            }
        }
    }
    Ok(collect(ptr, count, true))
}

/// The software MFTs of the category (synchronous transforms such as the inbox encoder and decoder).
pub(crate) fn software(cat: Category) -> Result<Vec<Mft>> {
    init_thread()?;
    let h264 = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let (cat_guid, input, output) = match cat {
        Category::Encoder => (MFT_CATEGORY_VIDEO_ENCODER, None, Some(&h264 as *const _)),
        Category::Decoder => (MFT_CATEGORY_VIDEO_DECODER, Some(&h264 as *const _), None),
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out pointers are valid for the call.
    unsafe { MFTEnumEx(cat_guid, flags, input, output, &mut ptr, &mut count) }.ctx("MFTEnumEx(software)")?;
    Ok(collect(ptr, count, false))
}

/// A GUID as lower-case hex without braces.
pub(crate) fn guid_text(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

impl Mft {
    pub(crate) fn activate(&self) -> Result<IMFTransform> {
        // SAFETY: activation creates a new transform instance.
        unsafe { self.activate.ActivateObject::<IMFTransform>() }.ctx("IMFActivate::ActivateObject")
    }

    /// Releases the transform this activation object created (`IMFActivate::ShutdownObject`, which also shuts it down).
    /// The activation object keeps its own reference to what it made, so dropping the transform alone frees nothing.
    pub(crate) fn shutdown_object(&self) {
        // SAFETY: ShutdownObject on a live activation object; the caller drops its own transform references.
        let _ = unsafe { self.activate.ShutdownObject() };
    }
}

/// True when the transform is an asynchronous MFT (every hardware encoder is).
pub(crate) fn is_async(transform: &IMFTransform) -> bool {
    // SAFETY: attribute reads on a live transform.
    unsafe { transform.GetAttributes().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC)).map(|v| v != 0).unwrap_or(false) }
}

/// Shuts a transform down (`IMFShutdown::Shutdown`), which hardware MFTs need to release their GPU session and to end the
/// event queue (an asynchronous MFT and its event callback hold each other until then). Synchronous MFTs usually do not
/// implement the interface: nothing to do for them. Errors are irrelevant at this point.
pub(crate) fn shutdown(transform: &IMFTransform) {
    if let Ok(s) = transform.cast::<IMFShutdown>() {
        // SAFETY: Shutdown on a live object; nothing else uses the transform after the owner's drop.
        let _ = unsafe { s.Shutdown() };
    }
}

/// Owns an activated transform until its owner takes over: dropped armed (an error between `ActivateObject` and the
/// owner being built), it shuts the transform and its activation object down, so a refused setting does not leak an MFT
/// and its GPU session at every retry. `disarm` hands the duty to the owner's own `Drop`.
pub(crate) struct ActivationGuard {
    transform: IMFTransform,
    activation: Mft,
    armed: bool,
}

impl ActivationGuard {
    pub(crate) fn new(transform: &IMFTransform, activation: &Mft) -> Self {
        Self { transform: transform.clone(), activation: activation.clone(), armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActivationGuard {
    fn drop(&mut self) {
        if self.armed {
            shutdown(&self.transform);
            self.activation.shutdown_object();
        }
    }
}

/// The `ICodecAPI` of a transform, when it has one.
pub(crate) fn codec_api(transform: &IMFTransform) -> Option<ICodecAPI> {
    transform.cast::<ICodecAPI>().ok()
}

/// The decoder properties the probe asks about.
pub(crate) fn decoder_properties() -> Vec<(&'static str, GUID)> {
    use windows::Win32::Media::MediaFoundation::*;
    vec![
        ("AVLowLatencyMode", CODECAPI_AVLowLatencyMode),
        ("AVDecVideoMaxCodedWidth", CODECAPI_AVDecVideoMaxCodedWidth),
        ("AVDecVideoMaxCodedHeight", CODECAPI_AVDecVideoMaxCodedHeight),
        ("AVDecVideoH264ErrorConcealment", CODECAPI_AVDecVideoH264ErrorConcealment),
        ("AVDecVideoDropPicWithMissingRef", CODECAPI_AVDecVideoDropPicWithMissingRef),
        ("AVDecVideoFastDecodeMode", CODECAPI_AVDecVideoFastDecodeMode),
        ("AVDecVideoSWPowerLevel", CODECAPI_AVDecVideoSWPowerLevel),
        ("AVDecVideoThumbnailGenerationMode", CODECAPI_AVDecVideoThumbnailGenerationMode),
        ("AVDecVideoAcceleration_H264", CODECAPI_AVDecVideoAcceleration_H264),
        ("AVDecVideoDXVAMode", CODECAPI_AVDecVideoDXVAMode),
        ("AVDecVideoInputScanType", CODECAPI_AVDecVideoInputScanType),
    ]
}

/// The properties the probe asks about, with the way the encoder uses each.
pub(crate) fn probe_properties() -> Vec<(&'static str, GUID)> {
    use windows::Win32::Media::MediaFoundation::*;
    vec![
        ("AVEncCommonRateControlMode", CODECAPI_AVEncCommonRateControlMode),
        ("AVEncCommonMeanBitRate", CODECAPI_AVEncCommonMeanBitRate),
        ("AVEncCommonMaxBitRate", CODECAPI_AVEncCommonMaxBitRate),
        ("AVEncCommonBufferSize", CODECAPI_AVEncCommonBufferSize),
        ("AVEncCommonQuality", CODECAPI_AVEncCommonQuality),
        ("AVEncCommonQualityVsSpeed", CODECAPI_AVEncCommonQualityVsSpeed),
        ("AVEncCommonLowLatency", CODECAPI_AVEncCommonLowLatency),
        ("AVLowLatencyMode", CODECAPI_AVLowLatencyMode),
        ("AVEncCommonRealTime", CODECAPI_AVEncCommonRealTime),
        ("AVEncCommonAllowFrameDrops", CODECAPI_AVEncCommonAllowFrameDrops),
        ("AVEncVideoForceKeyFrame", CODECAPI_AVEncVideoForceKeyFrame),
        ("AVEncMPVGOPSize", CODECAPI_AVEncMPVGOPSize),
        ("AVEncVideoMaxKeyframeDistance", CODECAPI_AVEncVideoMaxKeyframeDistance),
        ("AVEncMPVDefaultBPictureCount", CODECAPI_AVEncMPVDefaultBPictureCount),
        ("AVEncVideoMaxNumRefFrame", CODECAPI_AVEncVideoMaxNumRefFrame),
        ("AVEncVideoNumGOPsPerIDR", CODECAPI_AVEncVideoNumGOPsPerIDR),
        ("AVEncH264CABACEnable", CODECAPI_AVEncH264CABACEnable),
        ("AVEncVideoMinQP", CODECAPI_AVEncVideoMinQP),
        ("AVEncVideoMaxQP", CODECAPI_AVEncVideoMaxQP),
        ("AVEncVideoEncodeQP", CODECAPI_AVEncVideoEncodeQP),
        ("AVEncVideoContentType", CODECAPI_AVEncVideoContentType),
        ("AVEncVideoTemporalLayerCount", CODECAPI_AVEncVideoTemporalLayerCount),
        ("AVEncVideoGradualIntraRefresh", CODECAPI_AVEncVideoGradualIntraRefresh),
        ("AVEncVideoLTRBufferControl", CODECAPI_AVEncVideoLTRBufferControl),
        ("AVEncVideoDirtyRectEnabled", CODECAPI_AVEncVideoDirtyRectEnabled),
        ("AVEncVideoROIEnabled", CODECAPI_AVEncVideoROIEnabled),
        ("AVEncSliceControlMode", CODECAPI_AVEncSliceControlMode),
        ("AVEncAdaptiveMode", CODECAPI_AVEncAdaptiveMode),
        ("AVEncLowPowerEncoder", CODECAPI_AVEncLowPowerEncoder),
        ("AVEncVideoCBRMotionTradeoff", CODECAPI_AVEncVideoCBRMotionTradeoff),
        ("AVEncVideoUsage", CODECAPI_AVEncVideoUsage),
        ("AVEncNumWorkerThreads", CODECAPI_AVEncNumWorkerThreads),
    ]
}
