# Audio Splitter GUI

[中文说明](./README.zh-CN.md)

A lightweight Rust + WASAPI tool for Windows that duplicates system playback audio to an extra output line (for example `CABLE Input`), while keeping your original speaker/headphone playback.

## Features

- WASAPI loopback capture from system playback
- Duplicate to selected output endpoint (virtual cable, hardware line, etc.)
- Minimal GUI: refresh device list, choose output, start/stop
- Real-time level visualization for input/output flow

## What This Project Is Useful For

- Redirecting media player audio into a microphone-like input
- Live speech transcription pipelines
- Meeting note capture workflows
- Call recording / conversation archival workflows
- Routing audio into other software lines or processing tools

## Requirements

- Windows 10/11
- Rust toolchain (for building from source)
- A virtual audio cable driver if you want apps to receive this as microphone input

## VB-Audio Virtual Cable Recommendation

`VB-Audio Virtual Cable` is a third-party driver and must be installed manually (it is not built into Windows).

Recommended setup:

1. Install VB-CABLE and reboot if requested.
2. Keep your normal headphone/speaker as Windows default playback device.
3. In this app, select `CABLE Input` as the output target, then click `Start`.
4. In your voice/meeting software, set microphone/input to `CABLE Output`.
5. In that software, disable options similar to **"Mute while speaking" / "Mute during voice input"**.

## Build & Run

```bash
cargo run --release
```

## Routing Notes

- This app does not create virtual devices by itself. It uses devices already present in Windows.
- If you lose headphone playback, you likely switched system default playback to `CABLE Input`. Set default playback back to your headphone/speaker.
- For stability, keep sample-rate settings aligned at 48 kHz where possible.

## License

MIT
