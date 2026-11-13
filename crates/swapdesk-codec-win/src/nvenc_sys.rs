//! Hand-written bindings to the subset of `nvEncodeAPI.h` that the NVENC encoder uses, and the runtime loader of
//! `nvEncodeAPI64.dll` (D21: no new crate, no import library; the DLL belongs to the NVIDIA driver).
//!
//! Source of the layouts: NVIDIA's public header `nvEncodeAPI.h` as published in the FFmpeg `nv-codec-headers`
//! repository, tag `n13.1.15.0` (Video Codec SDK 13.1, `NVENCAPI_MAJOR_VERSION 13`, `NVENCAPI_MINOR_VERSION 1`):
//! <https://github.com/FFmpeg/nv-codec-headers/blob/n13.1.15.0/include/ffnvcodec/nvEncodeAPI.h>. The header carries
//! NVIDIA's MIT-style licence text (docs/research/windows.md 3d); no SDK sample code is used. Structs not listed here
//! (HEVC, AV1, motion-estimation, input-buffer paths) are not needed and are absent. Every struct keeps the C size and
//! field offsets: the unit tests at the bottom check them against the values `cl.exe` printed for the same header
//! (`sizeof` and `offsetof`), so a transcription slip fails a test instead of corrupting a driver call.
//!
//! The driver is asked first (`NvEncodeAPIGetMaxSupportedVersion`): a driver older than the header version is refused
//! with `Unsupported` and the caller falls back to Media Foundation.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{CStr, c_char, c_void};
use swapdesk_platform::{ErrorKind, PlatformError, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{GUID, PCSTR, PCWSTR};

use crate::sys::err;

/// `NVENCAPI_MAJOR_VERSION` and `NVENCAPI_MINOR_VERSION` of the header the layouts come from.
pub const API_MAJOR: u32 = 13;
pub const API_MINOR: u32 = 1;
/// `NVENCAPI_VERSION`.
pub const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);
/// What `NvEncodeAPIGetMaxSupportedVersion` returns for this header: `(major << 4) | minor`.
pub const API_MAX_VERSION_CODE: u32 = (API_MAJOR << 4) | API_MINOR;

/// `NVENCAPI_STRUCT_VERSION(ver)`.
pub const fn struct_version(ver: u32) -> u32 {
    API_VERSION | (ver << 16) | (0x7 << 28)
}
const HIGH: u32 = 1 << 31;

pub const NV_ENC_CONFIG_VER: u32 = struct_version(9) | HIGH;
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | HIGH;
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | HIGH;
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | HIGH;
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | HIGH;
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | HIGH;
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);
pub const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_version(1);
pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);

pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

// GUIDs.
pub const NV_ENC_CODEC_H264_GUID: GUID =
    GUID::from_values(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
/// `NV_ENC_CODEC_HEVC_GUID` and `NV_ENC_HEVC_PROFILE_MAIN_GUID`, copied from the header named above (lines 148 and 202).
pub const NV_ENC_CODEC_HEVC_GUID: GUID =
    GUID::from_values(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
pub const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID =
    GUID::from_values(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID =
    GUID::from_values(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
pub const NV_ENC_PRESET_P1_GUID: GUID =
    GUID::from_values(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]);
pub const NV_ENC_PRESET_P2_GUID: GUID =
    GUID::from_values(0xf581cfb8, 0x88d6, 0x4381, [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab]);
pub const NV_ENC_PRESET_P3_GUID: GUID =
    GUID::from_values(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]);
pub const NV_ENC_PRESET_P4_GUID: GUID =
    GUID::from_values(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]);
pub const NV_ENC_PRESET_P5_GUID: GUID =
    GUID::from_values(0x21c6e6b4, 0x297a, 0x4cba, [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3]);

// Enumerations (all 32-bit).
pub const NV_ENC_PARAMS_RC_CONSTQP: u32 = 0x0;
pub const NV_ENC_PARAMS_RC_VBR: u32 = 0x1;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 0x2;
pub const NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME: u32 = 0x1;
pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x1;
pub const NV_ENC_PIC_TYPE_IDR: u32 = 0x3;
pub const NV_ENC_PIC_TYPE_I: u32 = 0x2;
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0x0;
pub const NV_ENC_INPUT_IMAGE: u32 = 0x0;
pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0x0;
pub const NV_ENC_MEMORY_HEAP_AUTOSELECT: u32 = 0;
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const NV_ENC_H264_ENTROPY_CODING_MODE_CABAC: u32 = 0x1;

// `NV_ENC_PIC_FLAGS`.
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
pub const NV_ENC_PIC_FLAG_EOS: u32 = 0x8;

// Bit-field positions (little-endian MSVC layout, verified against the C compiler).
/// `NV_ENC_RC_PARAMS`: bit-field word after `vbvInitialDelay`.
pub const RC_ENABLE_AQ: u32 = 1 << 3;
pub const RC_ENABLE_TEMPORAL_AQ: u32 = 1 << 8;
pub const RC_ZERO_REORDER_DELAY: u32 = 1 << 9;
/// `NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ` (index 38 of the `NV_ENC_CAPS` enum in the header named above).
pub const NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ: u32 = 38;
/// `NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED`, and the value 1 of `NV_ENC_VUI_COLOR_PRIMARIES_BT709`,
/// `NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709` and `NV_ENC_VUI_MATRIX_COEFFS_BT709`.
pub const NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
pub const NV_ENC_VUI_BT709: u32 = 1;
/// `NV_ENC_CONFIG_H264`: first bit-field word.
pub const H264_ENABLE_INTRA_REFRESH: u32 = 1 << 10;
pub const H264_REPEAT_SPSPPS: u32 = 1 << 12;
pub const H264_SINGLE_SLICE_INTRA_REFRESH: u32 = 1 << 20;
/// `NV_ENC_RECONFIGURE_PARAMS`: `resetEncoder` and `forceIDR`.
pub const RECONF_RESET_ENCODER: u32 = 1 << 0;
pub const RECONF_FORCE_IDR: u32 = 1 << 1;

// NVENCSTATUS values.
pub const NV_ENC_SUCCESS: u32 = 0;
pub const NV_ENC_ERR_INVALID_VERSION: u32 = 15;
pub const NV_ENC_ERR_NEED_MORE_INPUT: u32 = 17;
pub const NV_ENC_ERR_NEED_MORE_OUTPUT: u32 = 26;

/// Name of an `NVENCSTATUS`.
pub fn status_name(code: u32) -> &'static str {
    const NAMES: [&str; 27] = [
        "SUCCESS",
        "NO_ENCODE_DEVICE",
        "UNSUPPORTED_DEVICE",
        "INVALID_ENCODERDEVICE",
        "INVALID_DEVICE",
        "DEVICE_NOT_EXIST",
        "INVALID_PTR",
        "INVALID_EVENT",
        "INVALID_PARAM",
        "INVALID_CALL",
        "OUT_OF_MEMORY",
        "ENCODER_NOT_INITIALIZED",
        "UNSUPPORTED_PARAM",
        "LOCK_BUSY",
        "NOT_ENOUGH_BUFFER",
        "INVALID_VERSION",
        "MAP_FAILED",
        "NEED_MORE_INPUT",
        "ENCODER_BUSY",
        "EVENT_NOT_REGISTERED",
        "GENERIC",
        "INCOMPATIBLE_CLIENT_KEY",
        "UNIMPLEMENTED",
        "RESOURCE_REGISTER_FAILED",
        "RESOURCE_NOT_REGISTERED",
        "RESOURCE_NOT_MAPPED",
        "NEED_MORE_OUTPUT",
    ];
    NAMES.get(code as usize).copied().unwrap_or("UNKNOWN")
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_QP {
    pub qpInterP: u32,
    pub qpInterB: u32,
    pub qpIntra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RC_PARAMS {
    pub version: u32,
    pub rateControlMode: u32,
    pub constQP: NV_ENC_QP,
    pub averageBitRate: u32,
    pub maxBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    /// enableMinQP, enableMaxQP, enableInitialRCQP, enableAQ, ..., zeroReorderDelay (bit 9), ...
    pub bits: u32,
    pub minQP: NV_ENC_QP,
    pub maxQP: NV_ENC_QP,
    pub initialRCQP: NV_ENC_QP,
    pub temporallayerIdxMask: u32,
    pub temporalLayerQP: [u8; 8],
    pub targetQuality: u8,
    pub targetQualityLSB: u8,
    pub lookaheadDepth: u16,
    pub lowDelayKeyFrameScale: u8,
    pub yDcQPIndexOffset: i8,
    pub uDcQPIndexOffset: i8,
    pub vDcQPIndexOffset: i8,
    pub qpMapMode: u32,
    pub multiPass: u32,
    pub alphaLayerBitrateRatio: u32,
    pub cbQPIndexOffset: i8,
    pub crQPIndexOffset: i8,
    pub reserved2: u16,
    pub lookaheadLevel: u32,
    pub viewBitrateRatios: [u8; 7],
    pub reserved3: u8,
    pub reserved1: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_H264_VUI_PARAMETERS {
    pub overscanInfoPresentFlag: u32,
    pub overscanInfo: u32,
    pub videoSignalTypePresentFlag: u32,
    pub videoFormat: u32,
    pub videoFullRangeFlag: u32,
    pub colourDescriptionPresentFlag: u32,
    pub colourPrimaries: u32,
    pub transferCharacteristics: u32,
    pub colourMatrix: u32,
    pub chromaSampleLocationFlag: u32,
    pub chromaSampleLocationTop: u32,
    pub chromaSampleLocationBot: u32,
    pub bitstreamRestrictionFlag: u32,
    pub timingInfoPresentFlag: u32,
    pub numUnitInTicks: u32,
    pub timeScale: u32,
    pub reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_H264 {
    /// enableTemporalSVC (0) ... enableIntraRefresh (10), enableConstrainedEncoding (11), repeatSPSPPS (12), ...
    pub bits: u32,
    pub level: u32,
    pub idrPeriod: u32,
    pub separateColourPlaneFlag: u32,
    pub disableDeblockingFilterIDC: u32,
    pub numTemporalLayers: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub adaptiveTransformMode: u32,
    pub fmoMode: u32,
    pub bdirectMode: u32,
    pub entropyCodingMode: u32,
    pub stereoMode: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFrames: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub h264VUIParameters: NV_ENC_CONFIG_H264_VUI_PARAMETERS,
    pub ltrNumFrames: u32,
    pub ltrTrustMode: u32,
    pub chromaFormatIDC: u32,
    pub maxTemporalLayers: u32,
    pub useBFramesAsRef: u32,
    pub numRefL0: u32,
    pub numRefL1: u32,
    pub outputBitDepth: u32,
    pub inputBitDepth: u32,
    pub tfLevel: u32,
    pub reserved1: [u32; 264],
    pub reserved2: [*mut c_void; 64],
}

/// The first fields of `NV_ENC_CONFIG_HEVC` (header n13.1.15.0, `struct _NV_ENC_CONFIG_HEVC`), up to and including
/// `hevcVUIParameters`, which is all this crate sets. The structure is a member of the `NV_ENC_CODEC_CONFIG` union that
/// `NV_ENC_CONFIG::h264Config` stands for, so it overlays the same bytes (see [`NV_ENC_CONFIG::hevc`]). Field order and
/// offsets follow the header; `hevc_head_follows_the_header` checks them. `NV_ENC_CONFIG_HEVC_VUI_PARAMETERS` is a typedef
/// of the H.264 VUI structure in the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_HEVC_HEAD {
    pub level: u32,
    pub tier: u32,
    pub minCUSize: u32,
    pub maxCUSize: u32,
    /// The bit-field word: useConstrainedIntraPred (bit 0) ... repeatSPSPPS (bit 7), enableIntraRefresh (bit 8),
    /// chromaFormatIDC (bits 9 and 10), and the rest.
    pub bits: u32,
    pub idrPeriod: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFramesInDPB: u32,
    pub ltrNumFrames: u32,
    pub vpsId: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub maxTemporalLayersMinus1: u32,
    pub hevcVUIParameters: NV_ENC_CONFIG_H264_VUI_PARAMETERS,
}

/// HEVC `repeatSPSPPS` (bit 7 of the word): VPS, SPS and PPS on every IDR picture.
pub const HEVC_REPEAT_SPSPPS: u32 = 1 << 7;
/// HEVC `enableIntraRefresh` (bit 8).
pub const HEVC_ENABLE_INTRA_REFRESH: u32 = 1 << 8;
/// HEVC `chromaFormatIDC` (bits 9 and 10) set to 1: 4:2:0.
pub const HEVC_CHROMA_420: u32 = 1 << 9;
/// Mask of the two `chromaFormatIDC` bits.
pub const HEVC_CHROMA_MASK: u32 = 0b11 << 9;

impl NV_ENC_CONFIG {
    /// The codec union read as the HEVC configuration.
    pub fn hevc(&mut self) -> &mut NV_ENC_CONFIG_HEVC_HEAD {
        // SAFETY: `h264Config` is the first (and largest) member of the codec union at offset 168; the HEVC structure
        // overlays the same bytes, `NV_ENC_CONFIG_HEVC_HEAD` is a prefix of it and is no larger than the union, and its
        // alignment (4) divides the union's (8).
        unsafe { &mut *std::ptr::from_mut(&mut self.h264Config).cast::<NV_ENC_CONFIG_HEVC_HEAD>() }
    }
}

/// `NV_ENC_CONFIG`; the codec union is as large as its H.264 member, so the H.264 config stands for it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG {
    pub version: u32,
    pub profileGUID: GUID,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: u32,
    pub mvPrecision: u32,
    pub rcParams: NV_ENC_RC_PARAMS,
    pub h264Config: NV_ENC_CONFIG_H264,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE {
    pub bits: u32,
    pub reserved1: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_INITIALIZE_PARAMS {
    pub version: u32,
    pub encodeGUID: GUID,
    pub presetGUID: GUID,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    /// reportSliceOffsets, enableSubFrameWrite, ... (all off here).
    pub bits: u32,
    pub privDataSize: u32,
    pub reserved: u32,
    pub privData: *mut c_void,
    pub encodeConfig: *mut NV_ENC_CONFIG,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    pub maxMEHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub tuningInfo: u32,
    pub bufferFormat: u32,
    pub numStateBuffers: u32,
    pub outputStatsLevel: u32,
    pub reserved1: [u32; 284],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RECONFIGURE_PARAMS {
    pub version: u32,
    pub reserved: u32,
    pub reInitEncodeParams: NV_ENC_INITIALIZE_PARAMS,
    /// resetEncoder (bit 0), forceIDR (bit 1).
    pub bits: u32,
    pub reserved2: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_PRESET_CONFIG {
    pub version: u32,
    pub reserved: u32,
    pub presetCfg: NV_ENC_CONFIG,
    pub reserved1: [u32; 256],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_PIC_PARAMS {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: u32,
    pub pictureStruct: u32,
    pub pictureType: u32,
    /// `NV_ENC_CODEC_PIC_PARAMS` (a union of 1544 bytes), left zero: no per-picture codec options are used.
    pub codecPicParams: [u64; 193],
    pub meHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub meExternalHints: *mut c_void,
    pub reserved2: [u32; 7],
    pub reserved5: [*mut c_void; 2],
    pub qpDeltaMap: *mut i8,
    pub qpDeltaMapSize: u32,
    pub reservedBitFields: u32,
    pub meHintRefPicDist: [u16; 2],
    pub diffPicNumHint: i32,
    pub alphaBuffer: *mut c_void,
    pub meExternalSbHints: *mut c_void,
    pub meSbHintsCount: u32,
    pub stateBufferIdx: u32,
    pub outputReconBuffer: *mut c_void,
    pub reserved3: [u32; 284],
    pub reserved6: [*mut c_void; 57],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_LOCK_BITSTREAM {
    pub version: u32,
    /// doNotWait (bit 0), ltrFrame, getRCStats.
    pub bits: u32,
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub frameIdx: u32,
    pub hwEncodeStatus: u32,
    pub numSlices: u32,
    pub bitstreamSizeInBytes: u32,
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub bitstreamBufferPtr: *mut c_void,
    pub pictureType: u32,
    pub pictureStruct: u32,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    pub ltrFrameIdx: u32,
    pub ltrFrameBitmap: u32,
    pub temporalId: u32,
    pub intraMBCount: u32,
    pub interMBCount: u32,
    pub averageMVX: i32,
    pub averageMVY: i32,
    pub alphaLayerSizeInBytes: u32,
    pub outputStatsPtrSize: u32,
    pub reserved: u32,
    pub outputStatsPtr: *mut c_void,
    pub frameIdxDisplay: u32,
    pub reserved1: [u32; 219],
    pub reserved2: [*mut c_void; 63],
    pub reservedInternal: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_MAP_INPUT_RESOURCE {
    pub version: u32,
    pub subResourceIndex: u32,
    pub inputResource: *mut c_void,
    pub registeredResource: *mut c_void,
    pub mappedResource: *mut c_void,
    pub mappedBufferFmt: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_REGISTER_RESOURCE {
    pub version: u32,
    pub resourceType: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub subResourceIndex: u32,
    pub resourceToRegister: *mut c_void,
    pub registeredResource: *mut c_void,
    pub bufferFormat: u32,
    pub bufferUsage: u32,
    pub pInputFencePoint: *mut c_void,
    pub chromaOffset: [u32; 2],
    pub chromaOffsetIn: [u32; 2],
    pub reserved1: [u32; 244],
    pub reserved2: [*mut c_void; 61],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CREATE_BITSTREAM_BUFFER {
    pub version: u32,
    pub size: u32,
    pub memoryHeap: u32,
    pub reserved: u32,
    pub bitstreamBuffer: *mut c_void,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

/// `NV_ENC_CAPS_PARAM`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CAPS_PARAM {
    pub version: u32,
    pub capsToQuery: u32,
    pub reserved: [u32; 62],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_SEQUENCE_PARAM_PAYLOAD {
    pub version: u32,
    pub inBufferSize: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub spsppsBuffer: *mut c_void,
    pub outSPSPPSPayloadSize: *mut u32,
    pub reserved: [u32; 250],
    pub reserved2: [*mut c_void; 64],
}

type Enc = *mut c_void;
pub type PNvEncOpenEncodeSessionEx =
    unsafe extern "system" fn(*mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut Enc) -> u32;
pub type PNvEncGetEncodePresetConfigEx =
    unsafe extern "system" fn(Enc, GUID, GUID, u32, *mut NV_ENC_PRESET_CONFIG) -> u32;
pub type PNvEncInitializeEncoder = unsafe extern "system" fn(Enc, *mut NV_ENC_INITIALIZE_PARAMS) -> u32;
pub type PNvEncCreateBitstreamBuffer = unsafe extern "system" fn(Enc, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> u32;
pub type PNvEncGetEncodeCaps = unsafe extern "system" fn(Enc, GUID, *mut NV_ENC_CAPS_PARAM, *mut i32) -> u32;
pub type PNvEncPtr = unsafe extern "system" fn(Enc, *mut c_void) -> u32;
pub type PNvEncEncodePicture = unsafe extern "system" fn(Enc, *mut NV_ENC_PIC_PARAMS) -> u32;
pub type PNvEncLockBitstream = unsafe extern "system" fn(Enc, *mut NV_ENC_LOCK_BITSTREAM) -> u32;
pub type PNvEncGetSequenceParams = unsafe extern "system" fn(Enc, *mut NV_ENC_SEQUENCE_PARAM_PAYLOAD) -> u32;
pub type PNvEncMapInputResource = unsafe extern "system" fn(Enc, *mut NV_ENC_MAP_INPUT_RESOURCE) -> u32;
pub type PNvEncDestroyEncoder = unsafe extern "system" fn(Enc) -> u32;
pub type PNvEncRegisterResource = unsafe extern "system" fn(Enc, *mut NV_ENC_REGISTER_RESOURCE) -> u32;
pub type PNvEncReconfigureEncoder = unsafe extern "system" fn(Enc, *mut NV_ENC_RECONFIGURE_PARAMS) -> u32;
pub type PNvEncGetLastErrorString = unsafe extern "system" fn(Enc) -> *const c_char;

type Slot = *const c_void;

/// `NV_ENCODE_API_FUNCTION_LIST`: the slots this crate calls are typed, the others are placeholders that keep the
/// offsets.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: Slot,
    pub nvEncGetEncodeGUIDCount: Slot,
    pub nvEncGetEncodeProfileGUIDCount: Slot,
    pub nvEncGetEncodeProfileGUIDs: Slot,
    pub nvEncGetEncodeGUIDs: Slot,
    pub nvEncGetInputFormatCount: Slot,
    pub nvEncGetInputFormats: Slot,
    pub nvEncGetEncodeCaps: Option<PNvEncGetEncodeCaps>,
    pub nvEncGetEncodePresetCount: Slot,
    pub nvEncGetEncodePresetGUIDs: Slot,
    pub nvEncGetEncodePresetConfig: Slot,
    pub nvEncInitializeEncoder: Option<PNvEncInitializeEncoder>,
    pub nvEncCreateInputBuffer: Slot,
    pub nvEncDestroyInputBuffer: Slot,
    pub nvEncCreateBitstreamBuffer: Option<PNvEncCreateBitstreamBuffer>,
    pub nvEncDestroyBitstreamBuffer: Option<PNvEncPtr>,
    pub nvEncEncodePicture: Option<PNvEncEncodePicture>,
    pub nvEncLockBitstream: Option<PNvEncLockBitstream>,
    pub nvEncUnlockBitstream: Option<PNvEncPtr>,
    pub nvEncLockInputBuffer: Slot,
    pub nvEncUnlockInputBuffer: Slot,
    pub nvEncGetEncodeStats: Slot,
    pub nvEncGetSequenceParams: Option<PNvEncGetSequenceParams>,
    pub nvEncRegisterAsyncEvent: Slot,
    pub nvEncUnregisterAsyncEvent: Slot,
    pub nvEncMapInputResource: Option<PNvEncMapInputResource>,
    pub nvEncUnmapInputResource: Option<PNvEncPtr>,
    pub nvEncDestroyEncoder: Option<PNvEncDestroyEncoder>,
    pub nvEncInvalidateRefFrames: Slot,
    pub nvEncOpenEncodeSessionEx: Option<PNvEncOpenEncodeSessionEx>,
    pub nvEncRegisterResource: Option<PNvEncRegisterResource>,
    pub nvEncUnregisterResource: Option<PNvEncPtr>,
    pub nvEncReconfigureEncoder: Option<PNvEncReconfigureEncoder>,
    pub reserved1: Slot,
    pub nvEncCreateMVBuffer: Slot,
    pub nvEncDestroyMVBuffer: Slot,
    pub nvEncRunMotionEstimationOnly: Slot,
    pub nvEncGetLastErrorString: Option<PNvEncGetLastErrorString>,
    pub nvEncSetIOCudaStreams: Slot,
    pub nvEncGetEncodePresetConfigEx: Option<PNvEncGetEncodePresetConfigEx>,
    pub nvEncGetSequenceParamEx: Slot,
    pub nvEncRestoreEncoderState: Slot,
    pub nvEncLookaheadPicture: Slot,
    pub reserved2: [Slot; 275],
}

/// A zero-filled value of a plain-data FFI struct (every field here is an integer, a float-free array or a nullable
/// pointer, for which all-zero bytes are valid).
pub(crate) fn zeroed<T: Copy>() -> Box<T> {
    // SAFETY: only used with the `#[repr(C)]` plain-data structs of this module, where zero bytes are a valid value.
    Box::new(unsafe { std::mem::zeroed() })
}

/// The loaded driver library with its function table. The DLL stays loaded for the life of the process.
pub struct NvencApi {
    pub fns: Box<NV_ENCODE_API_FUNCTION_LIST>,
    /// `(major << 4) | minor` the driver reported.
    pub driver_version_code: u32,
}

/// The name of the driver library.
pub const DEFAULT_LIBRARY: &str = "nvEncodeAPI64.dll";

impl NvencApi {
    /// Loads `library` (a DLL name or path), checks the driver's maximum API version against this crate's header version
    /// and fills the function table.
    pub fn load(library: &str) -> Result<Self> {
        let wide: Vec<u16> = library.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
        let module: HMODULE = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.map_err(|e| PlatformError {
            kind: ErrorKind::Unavailable,
            op: "NVENC::load",
            os_code: Some(i64::from(e.code().0)),
            detail: format!("{library}: {}", e.message()),
        })?;
        let sym = |name: &'static CStr| -> Result<*const c_void> {
            // SAFETY: `name` is NUL-terminated; the module handle came from LoadLibraryW above.
            match unsafe { GetProcAddress(module, PCSTR(name.as_ptr().cast())) } {
                Some(f) => Ok(f as *const c_void),
                None => Err(err(
                    ErrorKind::Unavailable,
                    "NVENC::load",
                    format!("{library} has no {}", name.to_string_lossy()),
                )),
            }
        };
        type GetMax = unsafe extern "system" fn(*mut u32) -> u32;
        type Create = unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> u32;
        // SAFETY: the exported symbols have exactly these signatures (nvEncodeAPI.h).
        let get_max: GetMax = unsafe { std::mem::transmute(sym(c"NvEncodeAPIGetMaxSupportedVersion")?) };
        let create: Create = unsafe { std::mem::transmute(sym(c"NvEncodeAPICreateInstance")?) };
        let mut code = 0u32;
        // SAFETY: valid out pointer.
        let status = unsafe { get_max(&mut code) };
        if status != NV_ENC_SUCCESS {
            return Err(err(ErrorKind::Unavailable, "NvEncodeAPIGetMaxSupportedVersion", status_name(status)));
        }
        if code < API_MAX_VERSION_CODE {
            return Err(err(
                ErrorKind::Unsupported,
                "NVENC::load",
                format!(
                    "the driver supports NVENC API {}.{}, this build needs {API_MAJOR}.{API_MINOR}",
                    code >> 4,
                    code & 0xf
                ),
            ));
        }
        let mut fns = zeroed::<NV_ENCODE_API_FUNCTION_LIST>();
        fns.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        // SAFETY: `fns` is a valid function list with its version set.
        let status = unsafe { create(&mut *fns) };
        if status != NV_ENC_SUCCESS {
            return Err(err(ErrorKind::Unavailable, "NvEncodeAPICreateInstance", status_name(status)));
        }
        Ok(Self { fns, driver_version_code: code })
    }
}

#[cfg(test)]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets printed by MSVC `cl.exe` (x64) for `nvEncodeAPI.h` n13.1.15.0.
    #[test]
    fn sizes_match_the_c_header() {
        assert_eq!(size_of::<GUID>(), 16);
        assert_eq!(size_of::<NV_ENC_RC_PARAMS>(), 128);
        assert_eq!(size_of::<NV_ENC_CONFIG_H264>(), 1792);
        assert_eq!(size_of::<NV_ENC_CONFIG>(), 3584);
        assert_eq!(size_of::<NV_ENC_INITIALIZE_PARAMS>(), 1800);
        assert_eq!(size_of::<NV_ENC_RECONFIGURE_PARAMS>(), 1816);
        assert_eq!(size_of::<NV_ENC_PRESET_CONFIG>(), 5128);
        assert_eq!(size_of::<NV_ENC_PIC_PARAMS>(), 3360);
        assert_eq!(size_of::<NV_ENC_LOCK_BITSTREAM>(), 1544);
        assert_eq!(size_of::<NV_ENC_MAP_INPUT_RESOURCE>(), 1544);
        assert_eq!(size_of::<NV_ENC_REGISTER_RESOURCE>(), 1536);
        assert_eq!(size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>(), 776);
        assert_eq!(size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>(), 1552);
        assert_eq!(size_of::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>(), 1544);
        assert_eq!(size_of::<NV_ENCODE_API_FUNCTION_LIST>(), 2552);
    }

    /// Offsets computed by hand from `struct _NV_ENC_CONFIG_HEVC` (every field a 4-byte word before the VUI block).
    #[test]
    fn hevc_head_follows_the_header() {
        assert_eq!(offset_of!(NV_ENC_CONFIG_HEVC_HEAD, level), 0);
        assert_eq!(offset_of!(NV_ENC_CONFIG_HEVC_HEAD, bits), 16);
        assert_eq!(offset_of!(NV_ENC_CONFIG_HEVC_HEAD, idrPeriod), 20);
        assert_eq!(offset_of!(NV_ENC_CONFIG_HEVC_HEAD, intraRefreshCnt), 28);
        assert_eq!(offset_of!(NV_ENC_CONFIG_HEVC_HEAD, sliceModeData), 56);
        assert_eq!(offset_of!(NV_ENC_CONFIG_HEVC_HEAD, hevcVUIParameters), 64);
        assert!(size_of::<NV_ENC_CONFIG_HEVC_HEAD>() <= size_of::<NV_ENC_CONFIG_H264>());
    }

    #[test]
    fn offsets_match_the_c_header() {
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, averageBitRate), 20);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, vbvInitialDelay), 32);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, minQP), 40);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, temporallayerIdxMask), 76);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, targetQuality), 88);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, lookaheadDepth), 90);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, lowDelayKeyFrameScale), 92);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, qpMapMode), 96);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, multiPass), 100);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, lookaheadLevel), 112);
        assert_eq!(offset_of!(NV_ENC_RC_PARAMS, reserved1), 124);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, level), 4);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, idrPeriod), 8);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, entropyCodingMode), 44);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, intraRefreshPeriod), 52);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, maxNumRefFrames), 60);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, h264VUIParameters), 72);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, chromaFormatIDC), 192);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, tfLevel), 220);
        assert_eq!(offset_of!(NV_ENC_CONFIG_H264, reserved1), 224);
        assert_eq!(offset_of!(NV_ENC_CONFIG, gopLength), 20);
        assert_eq!(offset_of!(NV_ENC_CONFIG, rcParams), 40);
        assert_eq!(offset_of!(NV_ENC_CONFIG, h264Config), 168);
        assert_eq!(offset_of!(NV_ENC_CONFIG, reserved), 1960);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, presetGUID), 20);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync), 60);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privDataSize), 72);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privData), 80);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeConfig), 88);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth), 96);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxMEHintCountsPerBlock), 104);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, tuningInfo), 136);
        assert_eq!(offset_of!(NV_ENC_INITIALIZE_PARAMS, outputStatsLevel), 148);
        assert_eq!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams), 8);
        assert_eq!(offset_of!(NV_ENC_PRESET_CONFIG, presetCfg), 8);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, inputTimeStamp), 24);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, inputBuffer), 40);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, bufferFmt), 64);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, codecPicParams), 80);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, meHintCountsPerBlock), 1624);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, reserved2), 1664);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, qpDeltaMap), 1712);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, alphaBuffer), 1736);
        assert_eq!(offset_of!(NV_ENC_PIC_PARAMS, outputReconBuffer), 1760);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputBitstream), 8);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes), 36);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputTimeStamp), 40);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr), 56);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, pictureType), 64);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputStatsPtr), 120);
        assert_eq!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameIdxDisplay), 128);
        assert_eq!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, inputResource), 8);
        assert_eq!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedResource), 24);
        assert_eq!(offset_of!(NV_ENC_REGISTER_RESOURCE, resourceToRegister), 24);
        assert_eq!(offset_of!(NV_ENC_REGISTER_RESOURCE, bufferFormat), 40);
        assert_eq!(offset_of!(NV_ENC_REGISTER_RESOURCE, pInputFencePoint), 48);
        assert_eq!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer), 16);
        assert_eq!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device), 8);
        assert_eq!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion), 24);
        assert_eq!(offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, spsppsBuffer), 16);
        assert_eq!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx), 240);
        assert_eq!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx), 320);
        assert_eq!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLookaheadPicture), 344);
    }

    #[test]
    fn versions_match_the_c_header() {
        assert_eq!(NV_ENC_CONFIG_VER, 0xf109_000d);
        assert_eq!(NV_ENC_INITIALIZE_PARAMS_VER, 0xf107_000d);
        assert_eq!(NV_ENC_RECONFIGURE_PARAMS_VER, 0xf102_000d);
        assert_eq!(NV_ENC_PRESET_CONFIG_VER, 0xf105_000d);
        assert_eq!(NV_ENC_PIC_PARAMS_VER, 0xf107_000d);
        assert_eq!(NV_ENC_LOCK_BITSTREAM_VER, 0xf102_000d);
        assert_eq!(NV_ENC_MAP_INPUT_RESOURCE_VER, 0x7104_000d);
        assert_eq!(NV_ENC_REGISTER_RESOURCE_VER, 0x7105_000d);
        assert_eq!(NV_ENC_CREATE_BITSTREAM_BUFFER_VER, 0x7101_000d);
        assert_eq!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, 0x7101_000d);
        assert_eq!(NV_ENCODE_API_FUNCTION_LIST_VER, 0x7102_000d);
        assert_eq!(NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER, 0x7101_000d);
        assert_eq!(NV_ENC_RC_PARAMS_VER, 0x7101_000d);
        assert_eq!(API_MAX_VERSION_CODE, 0xd1);
    }

    #[test]
    fn a_missing_library_is_unavailable_not_a_panic() {
        let e = NvencApi::load("swapdesk-no-such-nvenc.dll").err().expect("must fail");
        assert_eq!(e.kind, ErrorKind::Unavailable, "{e}");
    }
}
