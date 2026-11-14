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
- Temperature sources:

  |            | Linux                         | Windows                          |
  |------------|-------------------------------|----------------------------------|
  | Intel CPU  | kernel sensors (coretemp)     | PawnIO driver, as administrator  |
  | AMD CPU    | kernel sensors (k10temp)      | PawnIO driver, as administrator (Zen and later) |
  | NVIDIA GPU | NVIDIA driver                 | NVIDIA driver                    |
  | AMD GPU    | kernel sensors (amdgpu)       | Radeon driver                    |
  | Intel GPU  | kernel sensors (Arc only)     | not available                    |

- Windows has no built-in way to read CPU temperatures, so pc-heater uses the
  [PawnIO](https://github.com/namazso/PawnIO) driver. It asks for
  administrator rights every time it starts; when it has them and the driver
  is missing, it installs it automatically with
  `winget install namazso.PawnIO`. If you decline the prompt, everything else
  still works and the CPU temperature shows "no sensor found".

## Third-party components

`assets/pawnio/*.bin` are prebuilt, signed modules from
[PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), licensed under
the LGPL 2.1 or later (see `assets/pawnio/COPYING`).
