# pc-heater

Turns your PC into a space heater by loading the CPU and GPU.

The CPU and each GPU are controlled separately, each with three modes that can
be switched at any time from the window:

- **Off** – no load
- **Half** – about 50% utilisation
- **Full** – 100% utilisation

Every hardware graphics card in the machine gets its own section, so a PC
with integrated and dedicated graphics, or several cards, can heat with all of
them. Software renderers are ignored.

The window also shows power draw and temperature for the CPU and each GPU:
current, min, max, average since start and average over the last five minutes.

## Building

Linux:

    cargo build --release

Windows (cross-compiled from Linux, needs `mingw-w64` and
`rustup target add x86_64-pc-windows-gnu`):

    cargo build --release --target x86_64-pc-windows-gnu

## Usage

    pc-heater [--cpu off|half|full] [--gpu off|half|full]

Both start at Off unless given on the command line. `--gpu` applies to every
card.

For development on Windows, setting the environment variable
`PC_HEATER_NO_ELEVATE` skips the administrator prompt at startup.

## Notes

- The GPU load is an off-screen rendering job (blended layers of texture
  reads and arithmetic) that runs through Vulkan on Linux and DirectX 12 on
  Windows, so it works on any vendor's card. Windows Task Manager can show
  well under 100% for it even when the card is at its power limit; go by the
  power reading.
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

- Power draw is shown next to the temperatures. GPU power comes from the
  graphics driver. CPU power comes from the processor's own energy counter:
  on Windows through PawnIO (so it needs administrator, like the CPU
  temperature); on Linux the kernel only lets root read it by default. To
  allow it until the next reboot:

      sudo chmod a+r /sys/class/powercap/intel-rapl:*/energy_uj

## Third-party components

`assets/pawnio/*.bin` are prebuilt, signed modules from
[PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), licensed under
the LGPL 2.1 or later (see `assets/pawnio/COPYING`).
