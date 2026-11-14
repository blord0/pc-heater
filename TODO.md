# TODO

## Feature ideas

In rough priority order.

1. **Temperature safety cut-off** – drop a device to Off if it passes a limit
   (e.g. 90 °C). Most important, since the app runs hardware flat out
   unattended.
2. **Timer / schedule** – "run for 2 hours" or "off at 23:00".
3. **Energy used and cost** – session kWh, plus a running cost from a price
   per kWh. The power readings needed are already there.
4. **Remember settings** – reopen in the modes last used.
5. **Tray icon and start with the system** – minimise to the tray, wattage in
   the tooltip.
6. **Thermostat mode** – aim for a target wattage or temperature instead of
   fixed Off/Half/Full. Biggest change of the lot.

## Still to test

- AMD CPUs and GPUs (no hardware available so far).
- Declining the admin prompt on Windows, and the relaunch guard.
- PawnIO auto-install failing (no internet, or no winget).
- Long runs: an hour at CPU Full on each OS, to cover the energy counter
  rolling over and the 5-minute average acting as a rolling window.
- Sleep and resume while heating (the GPU load may not recover).
- The Windows exe cross-compiled from Linux (only native builds were run).
- Windows 10, a laptop, a high-DPI screen, two dedicated cards.
