# Rust VCam

A lightweight virtual camera for Windows 11, written in Rust. It passes your
webcam through to a Media Foundation virtual camera that any camera app
(Camera, Teams, Zoom, OBS) can select, optionally blurring or replacing the
background.

## How it works

- `crates/vcam-source` builds `rust_vcam_source.dll`, a COM in-process server
  implementing `IMFActivate`, `IMFMediaSourceEx` and `IMFMediaStream2`. The
  Windows Camera Frame Server service (`svchost -k Camera`, running as
  `LocalService`) loads it and pulls frames from it.
- `crates/vcam-app` calls `MFCreateVirtualCamera` with the source's CLSID and
  keeps the camera alive (Windows removes it when the app exits). It captures
  from your webcam and publishes frames to the source.
- `crates/vcam-shared` holds the frame format, the test pattern and the
  shared-memory transport between the two.

No kernel driver is involved; this needs Windows 11 (build 22000 or later).

The virtual camera offers NV12 and RGB32 at 1280x720 (default), 1920x1080 and
640x360, at up to 30 fps.

### Webcam capture

The webcam is opened only while a client is watching Rust VCam, so its light
is off otherwise. A Media Foundation source reader with advanced video
processing decodes and scales the webcam's native mode to exactly the format
the client negotiated; the native mode is chosen as the smallest one at 30 fps
or more that covers that format. If the webcam can't be opened, the app sends
an animated test pattern and retries every 5 seconds.

### Background effects

`--blur [N]` and `--background IMAGE` segment each webcam frame on ONNX
Runtime (the `ort` crate), using DirectML on the GPU and falling back to the
CPU. Two models are supported and detected automatically:

| Model | File | License | Notes |
|---|---|---|---|
| [Robust Video Matting](https://github.com/PeterL1n/RobustVideoMatting) MobileNetV3, fp16 (default on the GPU) | `rvm_mobilenetv3_fp16.onnx`, 7.5 MB | **GPL-3.0** | Made for webcam video; recurrent state across frames keeps it stable and keeps arms and hands. Runs at 960x540 with its low-res pass at 480x270, as RVM recommends for 720p portrait video; ~18 ms per frame on DirectML here. Uses 25% less GPU memory than fp32. |
| RVM MobileNetV3, fp32 (default on the CPU) | `rvm_mobilenetv3_fp32.onnx`, 15.0 MB | **GPL-3.0** | Same model in full precision; fp16 is emulated and slow on the CPU. |
| [MODNet](https://github.com/ZHKKKe/MODNet) | `modnet.onnx`, 25.9 MB | Apache-2.0 | Single-image portrait matting; flickers more and drops hands away from the body. Runs at 512x288. |

RVM is GPL-3.0: fine for personal use, but distributing the app with it means
distributing under the GPL. MODNet has no such condition.

```powershell
Invoke-WebRequest https://github.com/PeterL1n/RobustVideoMatting/releases/download/v1.0.0/rvm_mobilenetv3_fp16.onnx -OutFile models\rvm_mobilenetv3_fp16.onnx
Invoke-WebRequest https://github.com/PeterL1n/RobustVideoMatting/releases/download/v1.0.0/rvm_mobilenetv3_fp32.onnx -OutFile models\rvm_mobilenetv3_fp32.onnx
Invoke-WebRequest https://huggingface.co/Xenova/modnet/resolve/main/onnx/model.onnx -OutFile models\modnet.onnx
```

SHA-256: RVM fp16 `6a0d5ce6cc17702613be548559879b4521ed424cfe14ddc48d1acaa44d616f64`,
RVM fp32 `88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828`,
MODNet `07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9` (the
app warns about unknown files). The app looks in `models\` next to the
executable, then in the repository's `models\` folder, and prefers RVM (fp16
with the GPU, fp32 without), then MODNet; `--model PATH` or the settings
window picks one explicitly.

ONNX Runtime runs without its CPU memory arena and allocates weights
directly, so it doesn't keep peak-size buffers around (on the CPU path this
cut private memory from 197 MB to 114 MB at the same speed). The model is
loaded only while frames need it and unloaded after 30 seconds without a
client or preview, which releases its GPU device.

The model runs on its own thread, one frame ahead of compositing: frame N is
submitted while frame N-1 is blended with its own mask, which the GPU computed
while the camera delivered frame N. Masks therefore match their frames exactly
(no trail behind movement) at full frame rate, for one frame (33 ms) of added
latency. On the CPU, where the model can't keep up, the newest mask is used
instead. The mask is sharpened with a smoothstep (and, for MODNet, smoothed
over time where nothing moves), scaled up, and used to blend the frame over the
replacement image or a *masked* blur: the person is weighted out before
blurring at quarter resolution, so their colors don't smear into the
background. Compositing works on NV12 on the CPU, row-parallel on a 4-thread
rayon pool (`RAYON_NUM_THREADS` overrides).

ONNX Runtime is linked statically; `DirectML.dll` is copied next to the
executable by the build and must ship with it.

Don't combine this with Zoom's or Teams' own virtual background: their
segmenter then runs on our already-processed video and the two fight.

### Frame transport

The source creates a `Global\RustVCam_Frames_v2` section when a client opens
the camera (only a service can create global objects; users get read/write
access). It publishes the format the client negotiated plus a heartbeat. The
app writes frames in that format into one of three slots and publishes the
newest; a per-slot sequence counter lets the source detect torn reads without
locks. After each frame the app sets a shared event, and the source delivers
on that cadence, so output follows the webcam's real timing like a hardware
camera (repeating the last frame if none arrives within 100 ms). If the app
isn't publishing, the source shows a dim idle pattern at 30 fps.

## Install

Run `RustVCam-Setup-<version>.exe` (built as described below). It installs
to `C:\Program Files\Rust VCam`, registers the camera with Windows (one admin
prompt), and adds a Start menu shortcut, optionally a desktop shortcut and
"Start with Windows". It bundles the models, `DirectML.dll` and the Visual
C++ runtime, so nothing else is needed. Uninstall from Settings > Apps; it
removes the camera registration, the files, the shortcuts and the autostart
entry (your settings in `%APPDATA%\RustVCam` stay).

Start **Rust VCam** from the Start menu; it runs as you, in the tray.

## Build and run

Requirements: Rust (MSVC toolchain) and Visual Studio Build Tools with the C++
workload; [Inno Setup 6](https://jrsoftware.org/isinfo.php) for the installer
(`winget install JRSoftware.InnoSetup`).

```powershell
# Build everything and the installer: target\installer\RustVCam-Setup-<version>.exe
.\scripts\package.ps1
```

`package.ps1` builds release, stages the executables, the camera DLL,
`DirectML.dll`, the models in `models\` and the VC++ runtime DLLs into
`target\package`, and compiles `installer\rust-vcam.iss`. Both executables
carry the app icon and version info (embedded by `crates\vcam-app\build.rs`).

For development without installing:

```powershell
# Once, from an elevated PowerShell: copies the camera DLL to
# C:\Program Files\RustVCam and registers it
.\scripts\install.ps1

# As your normal user (not an administrator prompt: an elevated process's
# camera is invisible to normal apps)
cargo build --release
.\target\release\rust-vcam.exe
```

`install.ps1` and the installer register the same camera, so whichever ran
last decides which camera DLL Windows loads. When the installed app's
registration is active, `install.ps1` stops unless you pass `-Force`; rerun
the installer afterwards to point the camera back at the installed app.
`uninstall.ps1` only removes the development copy and never unregisters the
installed app's camera.

Two programs are built from `crates/vcam-app`:

- **`rust-vcam.exe`**, the settings and tray app. It creates the camera at
  start and shows a window with a live preview, the settings (webcam,
  background none/blur/image, blur strength, model, GPU, start with Windows)
  and status (client connected, frame rate, segmentation, per-frame timings).
  Changes apply immediately and are saved to
  `%APPDATA%\RustVCam\settings.toml`. Closing the window keeps the camera
  running in the tray; the tray menu's **Quit** removes it. "Start with
  Windows" adds a per-user `HKCU\...\Run` entry that starts it hidden
  (`--minimized`).
- **`vcam-app.exe`**, the command-line tool described below.

Only one of them can run at a time (they share the camera); a second copy
refuses to start.

Open the Windows Camera app (or Teams, Zoom, OBS) and select **Rust VCam**.

### Command-line tool

`vcam-app.exe` runs the camera from a console until you press Enter.

Options:

- `--camera NAME` captures from the first webcam whose name contains NAME.
  By default the first webcam that doesn't look like a virtual camera is used.
- `--list-cameras` lists webcams and their native modes.
- `--blur [N]` blurs the background (strength 1-20, default 8).
- `--background IMAGE` replaces the background with a JPEG or PNG, scaled to cover.
- `--model PATH` uses a different segmentation model file.
- `--cpu` runs segmentation on the CPU instead of DirectML.
- `--show-mask` shows the segmentation mask (white = person) instead of an effect, for debugging.
- `--bench-effects` times the effect pipeline on synthetic frames, no camera needed (combine with `--blur`/`--background`/`--model`/`--cpu`/`--seconds`).
- `--pattern` sends the animated test pattern instead of a webcam.
- `--no-publish` keeps the camera up without sending frames (idle pattern).
- `--seconds N` stops after N seconds instead of waiting for Enter.
- `--probe [FRAMES]` opens the running camera as a client, reads frames and
  reports frame rate and content. Set `RUST_VCAM_PROBE_SNAPSHOT=frame.bmp` to
  save the last frame.

To remove everything: `.\scripts\uninstall.ps1` (elevated).

## Resource use

Measured on the development machine (20 cores) with a 2560x1440 webcam
scaled to 1280x720 NV12, release build, for `vcam-app` while a client watches:

Effect pipeline, from `vcam-app --bench-effects` (synthetic 720p frames at
30 fps, no camera; includes rendering those frames), measured with
`scripts\measure-memory.ps1`:

| Mode | CPU (one core) | Working set | Private | GPU memory | Notes |
|---|---|---|---|---|---|
| Blur, RVM fp16 on DirectML | ~70% | ~300 MB | ~430 MB | 164 MB | ~6 ms per frame; segmentation 30/s at ~18 ms |
| Blur, RVM fp32 on DirectML | ~75% | ~293 MB | ~474 MB | 218 MB | segmentation at ~20 ms |
| Blur, RVM fp32 on the CPU | ~240% | ~129 MB | ~114 MB | 0 | segmentation ~9/s at ~115 ms; video stays at 30 fps |

About 300 MB of the DirectML rows' private memory is the GPU device itself
(D3D12, DirectML and the driver), independent of the model.

The settings/tray app (`rust-vcam`) with Blur on and the preview off:

| State | Working set | Private | GPU memory |
|---|---|---|---|
| Idle at start (model not loaded) | 150 MB | 94 MB | 0 |
| Streaming blur to a client | 467 MB | 542 MB | 164 MB |
| 40 s after the client left (model unloaded) | 208 MB | 128 MB | 0 |

Plain passthrough uses about 12% of one core and 50 MB. With no client
watching, CPU use is 0% and the webcam is closed. Before pipelining and
parallel compositing the effect pipeline took ~36 ms per frame, over the 33 ms
budget.

## Debugging

The source logs to `%ProgramData%\RustVCam\source.log` and to
`OutputDebugString` (use Sysinternals DebugView with "Capture Global Win32").
After rebuilding the DLL, rerun `install.ps1`; it stops the Frame Server so the
loaded DLL can be replaced. The app and the source must come from the same
build: the transport is versioned and an old DLL won't talk to a new app.

## Next steps

1. Keep multi-app mode on across runs. Windows lets a second app open a
   camera that is already in use only when "Allow multiple apps to use camera
   at the same time" is on (Settings > Bluetooth & devices > Cameras >
   Rust VCam > Advanced camera options). Without it, the second client fails
   with `MF_E_HW_MFT_FAILED_START_STREAMING` before the Frame Server calls the
   source. Other virtual cameras behave the same way. With the setting on,
   two `--probe` clients both stream at 30 fps. But the app removes the camera
   when it exits, so the setting resets on every run. Look into a
   longer-lived registration (`MFVirtualCameraLifetime_System`) or another
   way to keep the device and its setting between runs.
2. Consider GPU compositing (wgpu or D3D11) to cut the CPU cost of effects
   further.
3. Sign the installer and executables, so Windows SmartScreen doesn't warn
   about an unknown publisher.
