# Swapdesk research: Windows (plan stage 2)

Status: research note for M0 stage 2, written 2026-11-07. It feeds the Windows part of `docs/RESEARCH.md`.
Every source below was read on **2026-11-07**. Quotes are short and verbatim. A claim with no primary source is marked **UNPROVEN**. A claim that rests only on what was seen on this PC is marked **LOCAL** (observed with the command named, read-only). A design inference is marked **GUESS**.

Verdicts: **confirmed** (a primary page says so), **refuted** (a primary page says the opposite of what the plan assumed), **unproven** (no primary page found; a probe or spike must settle it).

---

## 1. DXGI Desktop Duplication on the secure desktop; Windows.Graphics.Capture

### 1a. Only LocalSystem can duplicate the secure desktop: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput
  > "E_ACCESSDENIED if the application does not have access privilege to the current desktop image. For example, only an application that runs at LOCAL_SYSTEM can access the secure desktop."

### 1b. Capture must run inside the console session, not in session 0: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiadapter-enumoutputs
  > "If you call this API in a Session 0 process, it returns DXGI_ERROR_NOT_CURRENTLY_AVAILABLE."
- Consequence: the service cannot capture. The helper (SYSTEM token, console session id) is required. This matches C2.

### 1c. Desktop switch: ACCESS_LOST, release, create again: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe
  > "DXGI_ERROR_ACCESS_LOST if the desktop duplication interface is invalid. ... Examples of this situation are: Desktop switch, Mode change, Switch from DWM on, DWM off, or other full-screen application. In this situation, the application must release the IDXGIOutputDuplication interface and create a new IDXGIOutputDuplication for the new content."
- DuplicateOutput page (above):
  > "If DuplicateOutput fails with DXGI_ERROR_UNSUPPORTED, the application can wait for system notification of desktop switches and mode changes and then call DuplicateOutput again after such a notification occurs. For more information, refer to EVENT_SYSTEM_DESKTOPSWITCH ... and ... WM_DISPLAYCHANGE."
  > "By default, only four processes can use a IDXGIOutputDuplication interface at the same time within a single session." (`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` when the limit is hit.)
  > "DXGI_ERROR_SESSION_DISCONNECTED if DuplicateOutput failed because the session is currently disconnected."

### 1d. OpenInputDesktop / SetThreadDesktop: confirmed, with a constraint
- https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-openinputdesktop
  > "Opens the desktop that receives user input." ... "The calling process must have an associated window station ... The window station associated with the calling process must be capable of receiving input."
- https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setthreaddesktop
  > "The SetThreadDesktop function will fail if the calling thread has any windows or hooks on its current desktop (unless the hDesktop parameter is a handle to the current desktop)."
- Consequence: the capture thread and the input thread in the helper must own **no windows and no hooks** (no hidden message window, no COM STA that creates one). Desktop-switch notification (`EVENT_SYSTEM_DESKTOPSWITCH` via a WinEvent hook) must live on a **separate thread** that never switches desktops. The helper must start on `winsta0` (CreateProcessAsUser `lpDesktop = "winsta0\\default"`, see 5d) so it has an interactive window station.
- **GUESS** (common practice, no primary page found): on ACCESS_LOST the thread closes its desktop handle, calls `OpenInputDesktop` + `SetThreadDesktop` again, then rebuilds the D3D device and `DuplicateOutput`. The M2 test "reacquire after a desktop switch" proves it on this PC with Win+L and a UAC prompt.

### 1e. Windows.Graphics.Capture (WGC) on the secure desktop or from a service: unproven, and not needed
- https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture
  > "With screen capture, developers invoke secure system UI for end users to pick the display or application window to be captured, and a yellow notification border is drawn by the system around the actively captured item."
- No Microsoft page says WGC works on the Winlogon/UAC desktop or from session 0. Third-party reports (OBS forum, Looking Glass wiki) say the secure desktop is not capturable by normal tools; not primary. **UNPROVEN.**
- Consequence: keep DDA as the only Windows capture path in v1. WGC stays out (consent UI, yellow border, no documented secure-desktop support).

---

## 2. Hybrid graphics (Intel iGPU + NVIDIA RTX laptop)

### 2a. DDA must run on the adapter that owns the output: confirmed
- DuplicateOutput page:
  > "A pointer to the Direct3D device interface ... This device must be created from the adapter to which the output is connected."
  > "E_INVALIDARG ... The specified device (pDevice) is invalid, was not created on the correct adapter ..."
- https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/error-when-dda-capable-app-is-against-gpu (KB 3019314, "Applies to: Windows 8.1", page dated 2026-02-12)
  > "This issue occurs because the DDA does not support being run against the discrete GPU on a Microsoft Hybrid system. By design, the call fails together with error code DXGI_ERROR_UNSUPPORTED in such a scenario."
  > "To work around this issue, run the application on the integrated GPU instead of on the discrete GPU on a Microsoft Hybrid system."
- **Plan correction.** The plan says "never DDA against the discrete GPU". The precise rule is: **create the D3D11 device on the adapter whose `EnumOutputs` returns that output**, per output. On a MUX laptop or with an external monitor wired to the dGPU, that adapter *is* the NVIDIA one.
- **LOCAL** (`Get-CimInstance Win32_VideoController`, 2026-11-07): this PC reports both "Intel(R) Graphics" (2560 wide, 240 Hz) and "NVIDIA GeForce RTX 5070 Laptop GPU" (2560 wide, 144 Hz) with an active mode. **GUESS:** one display hangs off each adapter, so capture here may need one device per adapter. The M2 probe prints adapter, LUID and output for each display.

### 2b. Enumerating which adapter owns each output: confirmed
- EnumOutputs page (above): "EnumOutputs first returns the output on which the desktop primary is displayed. This output corresponds with an index of zero."
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/ns-dxgi1_6-dxgi_output_desc1 — `DXGI_OUTPUT_DESC1` carries `DeviceName`, `DesktopCoordinates`, `AttachedToDesktop`, `Rotation`, `HMONITOR Monitor`, colour space.
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/nf-dxgi1_6-idxgifactory6-enumadapterbygpupreference
  > "Enumerates graphics adapters based on a given GPU preference."
- Method: `IDXGIFactory1::EnumAdapters1` → `EnumOutputs` per adapter → map `HMONITOR`/`DeviceName` → adapter LUID (`DXGI_ADAPTER_DESC1.AdapterLuid`). Do not rely on GPU preference for capture; use it only to pick a separate encode adapter.

### 2c. Getting frames to the encoder
- **Same-adapter encode via Media Foundation, choosing the adapter by LUID: confirmed.**
  https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenum2
  > "This function extends the MFTEnumEx function to allow external applications and internal components to discover the hardware MFTs that correspond to a specific video adapter."
  > "To specify a specific hardware adapter for which MFTs are queried, set the MFT_ENUM_ADAPTER_LUID attribute to the LUID of the adapter. If you do this, you must also specify the MFT_ENUM_FLAG_HARDWARE flag or E_INVALIDARG is returned."
- **Both vendors ship an H.264 MF encoder here. LOCAL** (registry `HKLM\SOFTWARE\Classes\MediaFoundation\Transforms`, 2026-11-07): "Intel® Quick Sync Video H.264 Encoder MFT", "NVIDIA H.264 Encoder MFT" and the inbox "H264 Encoder MFT" are registered.
  Consequence: NVENC is reachable **through Media Foundation** without the NVENC SDK. One `VideoEncoder` implementation (MF) covers Quick Sync, NVENC and the software MFT; the choice is the adapter LUID passed to `MFTEnum2`.
- **Cross-adapter copy (Intel output → NVIDIA encoder): confirmed only for D3D12; unproven for D3D11.**
  https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_heap_flags — `D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER = 0x20`, "The heap is allowed to share resources across adapters."
  https://learn.microsoft.com/en-us/windows/win32/direct3d12/shared-heaps
  > "Some texture layouts are allowed on cross adapter heaps ... Certain restrictions may apply to such textures, such as only supporting copying."
  > "There is no mechanism to share cross-adapter resources across D3D API versions. Cross-adapter shared resources are only supported in system memory."
  Consequence: DDA hands out D3D11 textures and MF encoders take D3D11 (DXGI device manager). A cross-adapter path needs a D3D11→D3D12 bridge, which the page says has no mechanism, or a CPU staging copy (map, memcpy, upload). **UNPROVEN** that a D3D11 shared NT handle opens on another adapter. Recommended order for M2 measurement: (1) encode on the output's own adapter (Quick Sync for the Intel panel, NVENC for a dGPU output); (2) only if (1) misses the budget, measure a CPU staging copy to the other adapter; (3) D3D12 cross-adapter is not worth it in v1.
- Hardware MFTs are asynchronous: https://learn.microsoft.com/en-us/windows/win32/medfound/hardware-mfts
  > "Hardware MFTs must use the new asynchronous processing model, as described in Asynchronous MFTs. Hardware MFTs must support dynamic format changes ..."
  Consequence: the codec crate drives `METransformNeedInput`/`METransformHaveOutput` events (unlock with `MF_TRANSFORM_ASYNC_UNLOCK`), not the synchronous loop of the software MFT. Two code paths in one crate.

---

## 3. Media Foundation H.264 encoder and decoder; 4:4:4; NVENC licensing

### 3a. Encoder controls: confirmed
- https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-encoder (inbox MFT, `Mfh264enc.dll`)
  - Profiles: "Baseline Profile, Main Profile, High profile (requires Windows 8)". Inputs: I420, IYUV, **NV12**, YUY2, YV12. "The output type must be set before the input type."
  - Low latency: "The encoder defaults to slice encoding, to minimize latency. To enable multi-frame encoding, set the CODECAPI_AVLowLatencyMode property to VARIANT_FALSE."
  - Bitrate change: "In Windows 8, you can set the average bit rate during encoding. If the bit rate changes, the encoder uses adaptive encoding." (`CODECAPI_AVEncCommonMeanBitRate`; CBR via `eAVEncCommonRateControlMode_CBR`.)
  - Keyframe: `CODECAPI_AVEncVideoForceKeyFrame` "Forces the encoder to code the next frame as a key frame."
  - Hardware: "If a certified hardware encoder is present, it will generally be used instead of the inbox system encoder ... The following Windows 8 and Windows 8.1 ICodecAPI properties are required: ... CODECAPI_AVEncCommonMeanBitRate ... CODECAPI_AVEncVideoForceKeyFrame". `CODECAPI_AVLowLatencyMode` is listed as **optional** for certified hardware encoders.
- https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avencvideoforcekeyframe
  > "This property applies to the next frame that the encoder receives as input. ... The setting resets to zero after the next frame."
- https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avlowlatencymode
  > "This property applies to both encoders and decoders. Low-latency mode is useful for real-time communications or live capture, when latency should be minimized."
- Consequence: DDA gives BGRA (desktop-dup-api page: "The format of the desktop image is always DXGI_FORMAT_B8G8R8A8_UNORM"), so a GPU colour conversion BGRA→NV12 (the D3D11 video processor) sits before the encoder. Low-latency mode, B-frames 0 and GOP control must be **probed per vendor MFT** (`ICodecAPI::IsSupported`), since low latency is optional for hardware MFTs. Intra-refresh has no property in the inbox list: **UNPROVEN** on the vendor MFTs; fall back to forced keyframes on request.

### 3b. Decoder: confirmed, 4:2:0 only
- https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-decoder
  > "supports decoding of Baseline, Main, and High profiles, up to level 5.1." ... "Chroma Formats 4:2:0 chroma or monochrome" ... "Maximum Resolution 4096 × 2304 pixels" ... "In Windows 8, the H.264 decoder also supports ... CODECAPI_AVLowLatencyMode Enables or disables low-latency decoding mode."
  > "Input data must conform to Annex B of ISO/IEC 14496-10. The data must include the start codes."
- Consequence: the wire format is Annex B with start codes. Level 5.1 caps 2560×1440 at high frame rates; **GUESS** 1440p at 120+ fps exceeds 5.1, so the SPEC caps fps per resolution or uses level 5.2 only on hardware decoders that accept it (probe).

### 3c. 4:4:4: refuted for the plan's "text" preset as written
- NVENC encodes H.264 4:4:4: https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html — H.264 table row "4:4:4 encoding (only CAVLC) Y" across generations.
- But the MF H.264 decoder decodes **only 4:2:0** (3b). The Intel and inbox MF encoders list no 4:4:4 profile (3a). macOS decode of H.264 4:4:4: see the macOS research. **UNPROVEN** that any MF path *encodes* 4:4:4 through `ICodecAPI`.
- Consequence: the v1 "text" preset becomes **4:2:0 at high bitrate / low QP with a lower frame rate**, which every encoder and decoder supports. 4:4:4 goes to "Later" (it would need NVENC SDK direct plus a software 4:4:4 decoder on the viewer).

### 3d. NVENC SDK licensing for an open-source app: confirmed, and avoidable
- https://developer.nvidia.com/nvidia-video-codec-sdk-license-agreement
  > "... distribute the Software and derivative works thereof (in object code only) as integrated in Licensee software products solely for use with supported NVIDIA GPU hardware products ..."
- The API header itself is MIT-style: https://github.com/FFmpeg/nv-codec-headers (file `include/ffnvcodec/nvEncodeAPI.h`, "Corresponds to Video Codec SDK version 13.1.15")
  > "This copyright notice applies to this header file only: Copyright (c) 2010-2026 NVIDIA Corporation. Permission is hereby granted, free of charge, to any person obtaining a copy of this software ..."
- Consequence: an MIT/Apache app may ship bindings generated from that header (the SDK samples stay out). Because the NVIDIA MF encoder is present (2c), **v1 needs no NVENC SDK at all**. Recommend: MF only in v1; direct NVENC only if MF's NVIDIA MFT misses the latency budget. Which DLL the driver installs for direct NVENC: **UNPROVEN** (not read today).

---

## 4. Cisco openh264 prebuilt binary

### 4a. Patent coverage terms: confirmed (exact text)
- https://www.openh264.org/BINARY_LICENSE.txt (v1.0)
  > "Additionally, this binary is licensed under Cisco's AVC/H.264 Patent Portfolio License from MPEG LA, at no cost to you, provided that the requirements and conditions shown below in the AVC/H.264 Patent Portfolio sections are met."
  > "1. The Cisco-provided binary is separately downloaded to an end user's device, and not integrated into or combined with third party software prior to being downloaded to the end user's device;
  > 2. The end user must have the ability to control (e.g., to enable, disable, or re-enable) the use of the Cisco-provided binary;
  > 3. Third party software, in the location where end users can control the use of the Cisco-provided binary, must display the following text: "OpenH264 Video Codec provided by Cisco Systems, Inc."
  > 4. Any third-party software that makes use of the Cisco-provided binary must reproduce all of the above text, as well as this last condition, in the EULA and/or in another location where licensing information is to be presented to the end user."
  > "THIS PRODUCT IS LICENSED UNDER THE AVC PATENT PORTFOLIO LICENSE FOR THE PERSONAL USE OF A CONSUMER OR OTHER USES IN WHICH IT DOES NOT RECEIVE REMUNERATION ..."
- https://www.openh264.org/faq.html
  > "Cisco is only covering the licensing fees for its own binary module, and products or projects that utilize it must download it at the time the product or project is installed on the user's computer or device."
- Consequence (**changes the plan**): the binary **cannot ship inside the installer**. The app (or installer) must download it from Cisco on the user's machine, with an enable/disable toggle and the attribution text. That is an Internet download, which breaks "no cloud" unless it is a one-time, user-triggered, opt-in step. Since Windows always has the inbox MF H.264 software encoder and decoder (3a, 3b), **openh264 adds nothing on Windows**. Recommend: drop openh264 from D4 for Windows; reopen it only if the MF software MFT is unusable (for example Windows "N" editions without the Media Feature Pack, **UNPROVEN** today). This reopens D4: an owner decision.

### 4b. How the `openh264` crate loads it: confirmed
- https://docs.rs/openh264/latest/openh264/ (0.9.9, BSD-2-Clause)
  > "source - Uses the bundled OpenH264 source; works out of the box (default). libloading - You'll need to provide Cisco's prebuilt library."
- https://docs.rs/openh264/latest/openh264/enum.OpenH264API.html
  > `pub fn from_blob_path(path: impl AsRef<OsStr>) -> Result<DynamicAPI, Error>` "Creates an OpenH264 API via the provided shared library if the library is well-known. In order for this to have any (legal) use, you should download the library from Cisco during installation ... This function also checks the file's SHA against a list of well-known libraries we can load."
  > `from_blob_path_unchecked` ... "Will cause UB if the provided path does not match the current platform and version."
- Consequence: if openh264 is ever used, depend on it with `default-features = false, features = ["libloading"]` (the default `source` feature compiles the codec from source, which the Cisco patent grant does **not** cover) and call only `from_blob_path` (SHA-checked).

---

## 5. Input injection, UIPI, SendSAS, launching into the console session

### 5a. SendInput is bound by UIPI: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput
  > "This function is subject to UIPI. Applications are permitted to inject input only into applications that are at an equal or lesser integrity level."
  > "This function fails when it is blocked by UIPI. Note that neither GetLastError nor the return value will indicate the failure was caused by UIPI blocking."
- https://learn.microsoft.com/en-us/windows/win32/secauthz/mandatory-integrity-control
  > "Windows defines four integrity levels: low, medium, high, and system. Standard users receive medium, elevated users receive high. ... system services receive system integrity."
- Consequence: a helper with a SYSTEM token runs at system integrity, above high, so it reaches elevated windows. A user-token helper does not. Silent UIPI failure means the input test must check the **effect** (text in the target window), never the return value. Injection happens on the input desktop after `SetThreadDesktop` (1d), so the same "no windows, no hooks" rule applies to the input thread.

### 5b. SendSAS: refuted for the helper, confirmed for the service
- https://learn.microsoft.com/en-us/windows/win32/api/sas/nf-sas-sendsas
  > "To successfully call the SendSAS function, an application must either be running as a service or have the uiAccess attribute of the requestedExecutionLevel element set to "true" in its application manifest."
  > "Applications with the uiAccess attribute set to "true" must be signed by using Authenticode. In addition, the application must reside in a protected location ... \Program Files\ ... \windows\system32\"
  > "The local security policy of a computer must be configured to allow services and applications to simulate a SAS. ... Computer Configuration | Administrative Templates | Windows Components | Windows Logon Options | Disable or enable software Secure Attention Sequence"
  > "A service can impersonate the token of another process that calls that service. In this case, a call to the SendSAS function by that service simulates a SAS on the session associated with the impersonated token."
- **Plan correction:** the helper is a process launched by the service, not a service, and v1 has no Authenticode certificate (uiAccess needs one). So **`swapdesk-service` calls `SendSAS(FALSE)`**, on an IPC request from the helper. To target the console session it can impersonate the helper (the page's impersonation sentence). **UNPROVEN** which session a non-impersonating service call targets; the M6 test settles it.

### 5c. SoftwareSASGeneration policy: confirmed (name, key, options); value numbers unproven
- https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-winlogon
  > "If you set this policy setting to "Services," services can simulate the SAS. ... If you disable or don't configure this setting, only Ease of Access applications running on the secure desktop can simulate the SAS."
  > "Name SoftwareSASGeneration ... Registry Key Name Software\Microsoft\Windows\CurrentVersion\Policies\System ... ADMX File Name WinLogon.admx"
- The page lists the CSP editions as Pro, Enterprise, Education, IoT Enterprise; **Home is not listed** for the MDM CSP. **UNPROVEN** that the registry value takes effect on Home (this PC is Home): M6 tests it here.
- The DWORD numbers for "None / Services / Ease of Access / both" are not on this page: **UNPROVEN** (read WinLogon.admx on this PC in M8 before writing the installer).
- Consequence: the installer sets `HKLM\Software\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration` to "Services" and the uninstaller restores the previous value (save it first), not just deletes it.

### 5d. Launching the helper into the console session: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-wtsgetactiveconsolesessionid
  > "If there is no session attached to the physical console (for example, if the physical console session is in the process of being attached or detached), this function returns 0xFFFFFFFF." ... "To monitor the state of the current physical console session, use the WTSRegisterSessionNotification function."
- https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken
  > "To call this function successfully, the calling application must be running within the context of the LocalSystem account and have the SE_TCB_NAME privilege." ... "Service providers must close token handles after they have finished using them."
- https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-token_information_class
  > "If TokenSessionId is set with SetTokenInformation, the application must have the Act As Part Of the Operating System privilege, and the application must be enabled to set the session ID in a token."
- https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw
  > "Typically, the process that calls the CreateProcessAsUser function must have the SE_INCREASE_QUOTA_NAME privilege and may require the SE_ASSIGNPRIMARYTOKEN_NAME privilege if the token is not assignable."
  > "If you want the process to be interactive, specify winsta0\default."
- Recipe (C2): service (LocalSystem) → `WTSGetActiveConsoleSessionId` (retry on 0xFFFFFFFF) →
  - SYSTEM helper (capture + input, all desktops): `OpenProcessToken(self)` → `DuplicateTokenEx(TokenPrimary)` → `SetTokenInformation(TokenSessionId, console)` → `CreateProcessAsUserW(..., lpDesktop = "winsta0\\default")`;
  - user helper (terminal, clipboard as the user): `WTSQueryUserToken(console)`; it fails at the login screen when no one is logged on, which is the expected "no terminal at the login screen" case.
  - Relaunch on `WM_WTSSESSION_CHANGE` / `SERVICE_CONTROL_SESSIONCHANGE` (console connect, logon, logoff).
- Note: "Any program running in the context of a service will have a session identifier of zero (0)" (WTSQueryUserToken page) — confirms the service itself is in session 0.

---

## 6. WASAPI loopback and process-exclusion loopback

### 6a. System loopback: confirmed
- https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
  > "In the call to the IAudioClient::Initialize method, change the value of the second parameter (StreamFlags) from 0 to AUDCLNT_STREAMFLAGS_LOOPBACK."
  > "In Windows 10 versions 1703 and higher, event-driven loopback clients are supported, and no longer need the workaround involving the render stream."
  > "A client can enable loopback mode only for a shared-mode stream (AUDCLNT_SHAREMODE_SHARED). Exclusive-mode streams cannot operate in loopback mode."
- Consequence: event-driven shared-mode loopback on the default render endpoint; follow default-device changes (`IMMNotificationClient`). Audio from apps in exclusive mode is not captured (document it).

### 6b. Process loopback (exclude the viewer): confirmed, Windows 10 build 20348+
- https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params
  > "TargetProcessId: The ID of the process for which the render streams, and the render streams of its child processes, will be included or excluded when activating the process loopback stream." ... "Minimum supported client: Windows 10 Build 20348"
- https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode
  > "PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE: Render streams from the specified process and its child processes are excluded from the activated process loopback stream."
- Consequence: when a viewer and a host share one PC (loopback tests, swap on the same machine), the host activates process loopback **excluding the viewer's PID tree** via `ActivateAudioInterfaceAsync`. Fall back to plain loopback below build 20348. Measurements still use the sine source (C7).
- **UNPROVEN:** that loopback opened from the **SYSTEM** helper captures the logged-on user's audio session. If not, audio capture moves to the user-token helper (it only matters while a user is logged on). M3 probe.

---

## 7. Windows Firewall and network category

### 7a. Category detection: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/netlistmgr/ne-netlistmgr-nlm_network_category
  > "NLM_NETWORK_CATEGORY_PUBLIC Value: 0 The network is a public (untrusted) network. NLM_NETWORK_CATEGORY_PRIVATE Value: 0x1 ..."
  > "The private or public network categories must never be used to assume which Windows Firewall ports are open, as the user can change the default settings of these categories. Instead, Firewall APIs should be called to ensure the ports that the required ports are open."
- https://learn.microsoft.com/en-us/windows/win32/api/netlistmgr/nn-netlistmgr-inetworklistmanager — `GetNetworks` / `GetNetworkConnections` enumerate networks; `INetwork::GetCategory` reads the category.
- https://learn.microsoft.com/en-us/windows/win32/api/netlistmgr/nf-netlistmgr-inetwork-setcategory
  > "Callers of this API must be members of the Administrators group."

### 7b. Rule scope and profile: confirmed
- https://learn.microsoft.com/en-us/windows/win32/api/netfw/nf-netfw-inetfwrule-get_remoteaddresses
  > ""LocalSubnet" indicates any local address on the local subnet. This token is not case-sensitive."
- https://learn.microsoft.com/en-us/powershell/module/netsecurity/new-netfirewallrule
  > "-Profile Specifies one or more profiles to which the rule is assigned. The rule is active on the local computer only when the specified profile is currently active. ... The acceptable values for this parameter are: Any, Domain, Private, Public, or NotApplicable. The default value is Any."
- https://learn.microsoft.com/en-us/windows/win32/api/netfw/nf-netfw-inetfwpolicy2-get_blockallinboundtraffic
  > "... all the exceptions (such as GloballyOpenPorts, Applications, or Services) which are specified in the profile are ignored and only locally-initiated traffic is allowed."

### 7c. Behaviour on a Public network
- Design: rules for `swapdesk-service.exe`, inbound UDP (QUIC port) and UDP 5353, `RemoteAddresses = LocalSubnet`, **Profile = Private (and Domain)**. On Public the rule is inactive by design, so inbound pairing and sessions fail. Outbound (this PC as viewer) still works, since the firewall allows outbound by default (**UNPROVEN** today; not re-read).
- Detection, following the page's own warning: the UI reports "Public" from `INetworkListManager` **and** checks the firewall (`INetFwPolicy2::CurrentProfileTypes`, `BlockAllInboundTraffic`, the rule's state) before telling the user what to do. Offer a button that opens Settings for that network; do not call `SetCategory` from the service on the user's behalf (an owner decision if ever wanted).

---

## 8. mDNS on Windows and coexistence with `mdns-sd`

### 8a. Windows has a built-in mDNS responder on 5353, and the port is shared: confirmed locally, partly by docs
- **LOCAL** (`netstat -ano -p UDP` and `Get-NetUDPEndpoint -LocalPort 5353`, 2026-11-07): `0.0.0.0:5353` is bound at the same time by `svchost` hosting **Dnscache**, by `chrome` and by `adb`.
- Native DNS-SD API exists: https://learn.microsoft.com/en-us/windows/win32/api/windns/nf-windns-dnsserviceregister — "Used to register a discoverable service on this device", minimum client Windows 10, `dnsapi.dll` (summarised from the search result of that page; the full page was not re-read today: **UNPROVEN** detail).
- Winsock semantics: https://learn.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse
  > "The exception to this non-deterministic behavior is multicast sockets. If two sockets are bound to the same interface and port and are members of the same multicast group, data will be delivered to both sockets, rather than an arbitrarily chosen one."
  > "The SO_REUSEADDR option has very few uses in normal applications aside from multicast sockets where data is delivered to all of the sockets bound on the same port."

### 8b. `mdns-sd` socket options: confirmed from its source
- https://docs.rs/mdns-sd/latest/mdns_sd/ (0.21.7, Apache-2.0 OR MIT); source https://github.com/keepsimple1/mdns-sd `src/service_daemon.rs` (main):
  - `fd.set_reuse_address(true)` always; `set_reuse_port(true)` only under `#[cfg(unix)]`.
  - `sock.set_multicast_if_v4(ip)` per interface; `set_multicast_ttl_v4(255)` / `set_multicast_hops_v6(255)`.
  - Public API: `ServiceDaemon::enable_interface`, `disable_interface`, `set_multicast_loop_v4/v6`.
- Consequence: `mdns-sd` coexists with Dnscache through `SO_REUSEADDR` multicast delivery (both get every packet). Bind it only to the guard-approved physical interfaces with `disable_interface(All)` then `enable_interface(...)`.
- **Plan clarification:** mDNS packets carry IP TTL 255 (RFC 6762 practice, which mdns-sd follows). The "hop limit 1" rule applies to the QUIC sockets, not to mDNS; mDNS stays link-local through 224.0.0.251 / ff02::fb. Write it this way in the SPEC so the guard test does not fail on mDNS.
- Risk: another process can also read Swapdesk's announcements (by design of mDNS). The TXT record carries only name, OS and key fingerprint, never secrets.

---

## 9. Tauri 2 Windows installers (WiX MSI and NSIS)

- https://v2.tauri.app/distribute/windows-installer/
  > "Tauri applications for Windows are either distributed as Microsoft Installers (.msi files) using the WiX Toolset v3 or as setup executables (-setup.exe files) using NSIS."
  > NSIS hooks: "NSIS_HOOK_PREINSTALL: Runs before copying files ... NSIS_HOOK_POSTINSTALL: Runs after the installer has finished copying all files ... NSIS_HOOK_PREUNINSTALL: Runs before removing any files ... NSIS_HOOK_POSTUNINSTALL: Runs after files, registry keys and shortcuts have been removed." configured by `"bundle" > "windows" > "nsis" > "installerHooks": "./windows/hooks.nsh"`.
  > "If you prefer your app installation to be available system-wide (which requires Administrator privileges) you can set installMode to perMachine"
  > WiX: "fragmentPaths" + "componentRefs"; "ComponentGroup, Component, FeatureGroup, Feature and Merge element ids must be referenced on the wix object of tauri.conf.json ..."
  > "Building MSI packages ... requires the VBSCRIPT optional feature to be enabled on Windows."
  > WebView2 `webviewInstallMode`: `downloadBootstrapper` "Requires an internet connection"; `offlineInstaller` "increases the installer size by around 127MB, but allows your application to be installed even if an internet connection is not available."
- https://docs.rs/tauri-utils/latest/tauri_utils/config/struct.WixConfig.html (2.10.2): fields are `version, upgrade_code, language, template, fragment_paths, component_group_refs, component_refs, feature_group_refs, feature_refs, merge_refs, enable_elevated_update_task, banner_path, dialog_image_path, fips_compliant`. **No WiX extension list**, so `WixFirewallExtension` / `WixUtilExtension` elements are not available without a custom `template` (**GUESS** that a template alone suffices; not tested).
- WiX v3 service element: https://wixtoolset.org/docs/v3/xsd/wix/serviceinstall/ — "Adds services for parent Component. Use the ServiceControl element to remove services."
- https://v2.tauri.app/develop/sidecar/ — `bundle.externalBin`; "a binary with the same name and a -$TARGET_TRIPLE suffix must exist on the specified path." (ship `swapdesk-service` and `swapdesk-helper` this way.)
- Recommendation (one mechanism for both installers):
  1. `swapdesk-service.exe install` / `uninstall` subcommands in Rust do all three jobs: register the service (SCM), add or remove the firewall rules (`INetFwPolicy2`), save and set or restore `SoftwareSASGeneration`. Also delete the DPAPI key files and the trust store on uninstall (with a "keep my devices" option: an owner decision on the default).
  2. **NSIS (primary):** `installMode = perMachine`; POSTINSTALL runs `"$INSTDIR\swapdesk-service.exe" install`; PREUNINSTALL runs `... uninstall`. Simple and testable.
  3. **MSI (optional):** a fragment with `ServiceInstall`/`ServiceControl` for the service and a deferred custom action for the rule and the policy; more work, needed only for managed deployment. Recommend NSIS only in v1.
  4. `webviewInstallMode = offlineInstaller` or `embedBootstrapper` so install does not depend on the Internet (LAN-only spirit); size versus robustness is an owner decision (D-new).

---

## 10. quinn and rustls

### 10a. Datagrams: confirmed
- https://docs.rs/quinn/latest/quinn/struct.Connection.html (quinn 0.11.12, rustls ^0.23.5)
  > `max_datagram_size`: "Compute the maximum size of datagrams that may be passed to send_datagram(). Returns None if datagrams are unsupported by the peer or disabled locally. This may change over the lifetime of a connection according to variation in the path MTU estimate. ... if the peer's limit is large this is guaranteed to be a little over a kilobyte at minimum."
  > `send_datagram`: "... data must both fit inside a single QUIC packet and be smaller than the maximum dictated by the peer. Previously queued datagrams which are still unsent may be discarded to make space for this datagram, in order of oldest to newest."
  > `send_datagram_wait`: "this method will wait for buffer space during congestion conditions, which effectively prioritizes old datagrams over new datagrams."
- Consequence: confirms the plan's split. Opus packets (20 ms at 64 to 128 kbit/s are about 160 to 320 bytes) fit well under ~1 KB; use `send_datagram` (drop oldest) for audio. Video never goes as datagrams.

### 10b. Stale frames: confirmed
- https://docs.rs/quinn/latest/quinn/struct.SendStream.html
  > `reset`: "Close the send stream immediately. No new data can be written after calling this method. Locally buffered data is dropped, and previously transmitted data will no longer be retransmitted if lost."
  > `set_priority`: "Every send stream has an initial priority of 0. Locally buffered data from streams with higher priority will be transmitted before data from streams with lower priority."
- https://docs.rs/quinn/latest/quinn/struct.RecvStream.html
  > `stop`: "Discards unread data and notifies the peer to stop transmitting."
- Consequence: sender resets a frame's stream once a newer keyframe supersedes it (or past a deadline); the receiver `stop`s a stream it will not render. Give input and control streams a higher priority than video. Also raise `max_concurrent_uni_streams` (one stream per frame at 60 fps).

### 10c. rustls RFC 7250 raw public keys and RFC 5705 exporters: confirmed in rustls; quinn + RPK unproven
- https://docs.rs/rustls/latest/rustls/manual/_04_features/index.html (rustls 0.23.46)
  > "Exporters (RFC5705)" ... "RFC7250 raw public keys for TLS1.3"
- API hooks: `ResolvesServerCert::only_raw_public_keys` (https://docs.rs/rustls/latest/rustls/server/trait.ResolvesServerCert.html) and `ServerCertVerifier::requires_raw_public_keys` (https://docs.rs/rustls/latest/rustls/client/danger/trait.ServerCertVerifier.html).
- quinn exposes the exporter: `Connection::export_keying_material(output, label, context)` "Derive keying material from this connection's TLS session secrets. ... See RFC5705 for more information."
- quinn's `peer_identity` "can be downcast to a Vec<rustls::pki_types::CertificateDer>"; **UNPROVEN** that quinn 0.11 negotiates RPK end to end (it passes rustls configs through; no quinn page says RPK is tested). Fallback with the same security: a self-signed certificate that wraps the pinned Ed25519 key, verified by comparing the SPKI to the pinned key in a custom verifier. Consequence: M1 stage 10 starts with a 1-hour spike "quinn + rustls RPK handshake", falling back to SPKI pinning.

### 10d. ring vs aws-lc-rs on Windows MSVC: confirmed
- https://docs.rs/rustls/latest/rustls/ — "by default it uses aws-lc-rs ... ring is also available via the ring crate feature" ... "Specifying default-features = false when depending on rustls will remove the implicit dependency on aws-lc-rs."
- https://aws.github.io/aws-lc-rs/requirements/windows.html — x86_64-pc-windows-msvc default build: "C/C++ Compiler & *NASM" ... "* NASM is recommended on x86-64 but can be avoided using prebuilt NASM objects." ... "Prebuilt NASM objects are only used as a fallback when NASM is not available."
- https://github.com/briansmith/ring/blob/main/BUILDING.md — "For Windows targets, "Build Tools for Visual Studio 2022" ... The "Desktop development with C++" workflow must be installed." ... "For Windows x86 and x86-64, the packaged crate contains precompiled object files for the assembly language modules so no macro assembler."
- **LOCAL:** `which nasm cmake clang` finds none on this PC; VS 2022 Build Tools are present.
- Consequence: both build here (aws-lc-rs through prebuilt NASM objects; ring with MSVC only). Recommend **ring** in v1 for the smaller C build and no assembler fallback path on the MacBook. quinn must be configured with the same provider (`quinn` features `rustls-ring`, **UNPROVEN** feature name today; check `Cargo.toml` at M0 step 6). A dependency choice: goes into the D4 dialog.

---

## 11. PAKE for the 6-digit pairing

### 11a. Audit status: no audited Rust PAKE found
- https://github.com/RustCrypto/PAKEs (README):
  > "Crates in this repository have not yet received any formal cryptographic and security reviews." ... "No efforts were yet taken in regards of blinding or erasing secrets from the memory."
- `spake2` 0.4.0 (https://docs.rs/spake2/latest/spake2/, README in the same repo):
  > "This crate has never received an independent third party audit for security and correctness. USE AT YOUR OWN RISK!"
- `aucpace` (same repo, README): same warning, word for word.
- `pake-cpace` 0.1.7 (https://docs.rs/pake-cpace/latest/pake_cpace/, ISC, README at https://github.com/jedisct1/rust-cpace): "CPace-Ristretto255, a balanced PAKE ... This is a port of the CPace implementation for libsodium", linking to **draft-haase-cpace-01**, an early draft, not the current CFRG text. No audit statement: **UNPROVEN** that it was ever audited.
- Verdict: the plan's precondition ("an audited PAKE") is **refuted** for Rust today.

### 11b. Alternative: commitment-based short authentication string (SAS)
- https://www.rfc-editor.org/rfc/rfc6189.html (ZRTP), section 4.4.1.1:
  > "The use of hash commitment in the DH exchange constrains the attacker to only one guess to generate the correct Short Authentication String (SAS) in his attack, which means the SAS can be quite short. A 16-bit SAS, for example, provides the attacker only one chance out of 65536 of not being detected."
- Scheme for Swapdesk (**GUESS**, a design to put through the M0 step 6 review): the two peers open a QUIC/TLS 1.3 connection with fresh RPK keys (no trust yet). The initiator sends `commit = H(nonce_i || pk_i)`, the responder replies with `nonce_r`, the initiator reveals `nonce_i`. Both compute `SAS = 6 digits of H(exporter("swapdesk pair") || nonce_i || nonce_r || pk_i || pk_r)` and **show it on both screens**; the user confirms on both. Accept pins the peer key.
- Trade-offs:

| Option | Security basis | Code | UX | Risk |
|---|---|---|---|---|
| **A. Commitment SAS, compare on both screens (recommended)** | one MITM guess, 1 in 10^6 for 6 digits (RFC 6189 argument); only hash + TLS exporter, which come from the already-chosen rustls provider | small, no new crate | user reads the code on both machines and confirms both | needs someone at both machines (true for first pairing at home) |
| B. `spake2` with the typed code | sound protocol, crate unaudited | new crate (owner yes) | type the host's code on the viewer | unaudited implementation; key confirmation is ours to add |
| C. `pake-cpace` with the typed code | protocol sound, crate tracks draft-01, unaudited | new crate | same as B | old draft, single maintainer |
| D. HMAC(code, exporter) without PAKE | **broken**: an active MITM brute-forces 10^6 codes offline | small | same as B | rejected |

- Recommendation: **A**. It needs no unaudited crate, binds to the TLS channel through the exporter (as the plan wants), and the "3 wrong attempts then lockout" rule becomes "3 rejected comparisons then lockout". It changes the pairing screen in the design Artifact (show and confirm, instead of type): **an owner decision** (product UX), to be put to the owner with B as the alternative for "type the code" UX.

---

## 12. Hyper-V on Windows 11 Home

- https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/get-started/install-hyper-v
  > "Note: The Hyper-V role can't be installed on Windows 10 Home or Windows 11 Home."
- **LOCAL:** this PC is "Windows 11 Home Single Language 10.0.26300" (session environment).
- Verdict: confirmed; the plan's limit stands. Alternatives, each a third-party dependency or a purchase (owner decision): a Windows 11 Pro upgrade (enables Hyper-V), or a third-party hypervisor. **UNPROVEN** that DDA, SendSAS and the secure desktop behave in any VM as on hardware (a VM display adapter changes DDA and is useless for the hybrid-GPU cases). Recommend: keep Win-Win across two machines "unverified" in v1; loopback on this PC covers the protocol.

---

## Summary table

| # | Question | Verdict | Consequence for the design | Source |
|---|---|---|---|---|
| 1a | DDA secure desktop needs LocalSystem | confirmed | helper with SYSTEM token on the secure desktop | learn.microsoft.com/.../nf-dxgi1_2-idxgioutput1-duplicateoutput |
| 1b | DDA from session 0 | refuted (fails) | capture only in the helper in the console session | .../nf-dxgi-idxgiadapter-enumoutputs |
| 1c | ACCESS_LOST / UNSUPPORTED on switch, recreate | confirmed | release and `DuplicateOutput` again; wait on `EVENT_SYSTEM_DESKTOPSWITCH`; 4-process limit | .../idxgioutputduplication-acquirenextframe; duplicateoutput |
| 1d | OpenInputDesktop + SetThreadDesktop | confirmed | capture/input threads own no windows or hooks; WinEvent hook on its own thread | .../nf-winuser-setthreaddesktop; openinputdesktop |
| 1e | WGC on secure desktop / from a service | unproven | WGC out of v1 | .../media-authoring-processing/screen-capture |
| 2a | DDA adapter on hybrid | confirmed, plan wording corrected | device per output on the adapter that enumerates it, which may be the dGPU | duplicateoutput; KB 3019314 |
| 2b | Map output to adapter | confirmed | EnumAdapters1 → EnumOutputs → LUID/HMONITOR | dxgi_output_desc1; enumoutputs |
| 2c | Frames to NVENC / Quick Sync | confirmed (MF per LUID), unproven (D3D11 cross-adapter) | MF only: `MFTEnum2` + `MFT_ENUM_ADAPTER_LUID`; NVIDIA and Intel H.264 MFTs present here (LOCAL); CPU copy only if needed | .../nf-mfapi-mftenum2; .../direct3d12/shared-heaps |
| 3a | MF encoder low latency, keyframe, bitrate | confirmed | probe `ICodecAPI` per vendor MFT; async MFT loop; BGRA→NV12 on GPU | .../medfound/h-264-video-encoder; hardware-mfts |
| 3b | MF decoder | confirmed (4:2:0, level 5.1, Annex B) | Annex B on the wire; fps cap per resolution | .../medfound/h-264-video-decoder |
| 3c | 4:4:4 | refuted for v1 | "text" preset = 4:2:0 high bitrate; 4:4:4 to Later | h-264-video-decoder; NVENC application note |
| 3d | NVENC SDK licence | confirmed (header MIT-style) but not needed | no NVENC SDK in v1 | nvidia-video-codec-sdk-license-agreement; FFmpeg/nv-codec-headers |
| 4a | Cisco openh264 patent terms | confirmed | must be downloaded on the user's device, user toggle, attribution; drop it on Windows (MF software MFT exists); reopens D4 | openh264.org/BINARY_LICENSE.txt; faq |
| 4b | `openh264` crate loading | confirmed | `default-features=false, features=["libloading"]`, `from_blob_path` | docs.rs/openh264 |
| 5a | SendInput and UIPI | confirmed | SYSTEM helper reaches elevated windows; test by effect, not return value | .../nf-winuser-sendinput; mandatory-integrity-control |
| 5b | SendSAS from the helper | refuted | the **service** calls SendSAS on IPC request | .../nf-sas-sendsas |
| 5c | SoftwareSASGeneration | confirmed (name, key); unproven (DWORD values, Home) | installer saves, sets "Services", restores; test on Home in M6 | .../policy-csp-admx-winlogon |
| 5d | Launch into console session | confirmed | DuplicateTokenEx + TokenSessionId + CreateProcessAsUser `winsta0\default`; WTSQueryUserToken for the user helper | wtsgetactiveconsolesessionid; wtsqueryusertoken; createprocessasuserw |
| 6a | WASAPI loopback | confirmed | event-driven shared-mode loopback | .../coreaudio/loopback-recording |
| 6b | Process-exclusion loopback | confirmed (build 20348+); unproven from SYSTEM | exclude viewer PID tree on same-PC sessions; maybe move audio to the user helper | audioclient_process_loopback_params |
| 7 | Firewall category and LocalSubnet | confirmed | rule Private/Domain + LocalSubnet; detect Public via NLM **and** firewall APIs; no SetCategory | nlm_network_category; inetfwrule-get_remoteaddresses; new-netfirewallrule |
| 8 | mDNS coexistence | confirmed (LOCAL + Winsock docs + crate source) | `mdns-sd` shares 5353 with Dnscache; bind to guard interfaces; TTL 255 for mDNS, hop limit 1 only for QUIC | using-so-reuseaddr-and-so-exclusiveaddruse; mdns-sd source |
| 9 | Tauri installer hooks | confirmed | NSIS perMachine + hooks calling `swapdesk-service install/uninstall`; MSI optional; offline WebView2 | v2.tauri.app/distribute/windows-installer; tauri-utils WixConfig |
| 10a-b | quinn datagrams, reset/stop | confirmed | audio datagrams; per-frame streams reset/stop; priorities | docs.rs/quinn Connection, SendStream, RecvStream |
| 10c | rustls RPK + exporter | confirmed in rustls; unproven through quinn | spike in stage 10; fallback SPKI pinning | rustls manual features; quinn export_keying_material |
| 10d | ring vs aws-lc-rs | confirmed | both build here; recommend ring (D4) | aws-lc-rs Windows requirements; ring BUILDING.md |
| 11 | Audited PAKE crate | refuted (none audited) | recommend commitment SAS compared on both screens; owner decides the UX | RustCrypto/PAKEs README; spake2 README; RFC 6189 |
| 12 | Hyper-V on 11 Home | confirmed (not available) | Win-Win across machines stays unverified | .../install-hyper-v |
