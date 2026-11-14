# pc-heater

Turns your PC into a space heater by loading the CPU and GPU.

The CPU and GPU are controlled separately, each with three modes that can be
switched at any time from the window:

- **Off** – no load
- **Half** – about 50% utilisation
- **Full** – 100% utilisation

The window also shows CPU and GPU temperatures: current, min, max, average
since start and average over the last five minutes.

## Building

Linux:

    cargo build --release

Windows (cross-compiled from Linux, needs `mingw-w64` and
`rustup target add x86_64-pc-windows-gnu`):

    cargo build --release --target x86_64-pc-windows-gnu

## Usage

    pc-heater [--cpu off|half|full] [--gpu off|half|full]

Both start at Off unless given on the command line.

## Notes

- The GPU load runs through Vulkan on Linux and DirectX 12 on Windows, so it
  works on any vendor's card.
- GPU temperature is read through the NVIDIA driver, or the kernel's hardware
  sensors for other cards on Linux. CPU temperature on Windows depends on what
  the firmware exposes and may be unavailable.
