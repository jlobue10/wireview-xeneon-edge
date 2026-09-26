# WireView Pro II widgets for the Corsair Xeneon Edge

Live **per-wire current**, **total current** and **total power** from a
[Thermal Grizzly WireView Pro II](https://www.thermal-grizzly.com/en/wireview-pro-ii-gpu/s-tg-wv-p2-h19n)
on a [Corsair Xeneon Edge](https://www.corsair.com/us/en/s/xeneon-edge), through iCUE's built-in
**iFrame** widget.

| Per-wire current | Total current | Total power |
|---|---|---|
| ![per-wire](docs/img/per-wire_840x344.png) | ![total current](docs/img/total-current_840x344.png) | ![total power](docs/img/total-power_840x344.png) |

The widgets are plain HTML pages hosted on GitHub Pages:

- <https://jlobue10.github.io/wireview-xeneon-edge/per-wire/>
- <https://jlobue10.github.io/wireview-xeneon-edge/total-current/>
- <https://jlobue10.github.io/wireview-xeneon-edge/total-power/>

They read live values from a tiny **bridge** running on the same PC, which in turn reads
HWiNFO's shared memory. Nothing leaves the machine; the hosted page only fetches
`http://localhost:8765`.

## How it works

```
WireView Pro II ──USB──▶ HWiNFO64 (shared memory) ──▶ bridge/wireview_bridge.py ──localhost JSON──▶ widget page in iCUE iFrame
```

- HWiNFO 8.41+ supports the WireView Pro II natively (per-pin voltage/current/power, totals,
  two temperatures and the fault flags). The Thermal Grizzly app has no export of its own and
  cannot share the USB port with HWiNFO, so **close the WireView app** while HWiNFO runs.
- The bridge (`bridge/`, Python 3.10+, standard library only) maps `Global\HWiNFO_SENS_SM2`,
  picks the WireView sensor and serves it as JSON with CORS enabled. It also serves the widget
  pages from `docs/`, so `http://localhost:8765/per-wire/` works without GitHub Pages.

## Setup

1. Install [HWiNFO64](https://www.hwinfo.com/download/) 8.41 or newer. Start it in
   **Sensors-only** mode and enable **Settings → Main Settings → Shared Memory Support**.
   Set it to run at startup (Settings → General → Auto Start) and disable the WireView app's
   auto-start so they don't fight over the device.
   The free HWiNFO build stops the shared-memory feed after 12 hours per session; HWiNFO Pro
   removes the cap. If the feed stops, restart HWiNFO.
2. Clone this repository and start the bridge:
   ```
   python bridge\wireview_bridge.py
   ```
   `http://localhost:8765/api/wireview` should return JSON with `"ok": true`.
   To start it at login: `powershell -ExecutionPolicy Bypass -File bridge\install-startup.ps1`.
3. In iCUE select the Xeneon Edge, add an **iFrame** widget in the slot you want, and paste a
   widget URL. Use the copy served by the bridge:
   - `http://localhost:8765/per-wire/`
   - `http://localhost:8765/total-current/`
   - `http://localhost:8765/total-power/`

   Append query options to match your device limits, e.g.
   `http://localhost:8765/per-wire/?wire_limit=10.5&total_limit=55&cable_w=600`.

   **Why not the GitHub Pages URL?** It works only where the webview allows an https page to
   fetch `localhost`. Chromium 138+ classifies that as a Local Network Access request and asks
   for permission; an embedded webview may deny it silently and the widget shows
   "Bridge offline". Served from the bridge, page and data share one origin and nothing can
   block it. iCUE 5.51 bundles Qt WebEngine 6.9 (Chromium 130), which predates that rule, so
   the hosted URLs above should also work inside the iFrame widget today; the localhost form
   is simply future-proof. The Pages site remains the documentation and a live preview when
   your browser grants the permission.

## URL options

| Option | Default | Meaning |
|---|---|---|
| `host` | `http://localhost:8765` | Bridge origin |
| `wire_limit` | `10.5` | Amps per wire treated as 100 % (per-wire widget) |
| `total_limit` | `55` | Amps total treated as 100 % (total-current widget) |
| `cable_w` | `600` | Cable rating in W (total-power widget) |
| `decimals` | `2` | Decimals on the headline numbers |
| `interval` | `1000` | Poll interval in ms |
| `accent`, `bg`, `fg` | orange / black / white | Hex colours without `#` |
| `label=0` | | Hide the caption line |

Warning colour begins at 80 % of a limit, critical at 100 %. The device's own fault flags
(over-current, wire over-current, over-power, chip/sensor over-temperature, current imbalance)
always show as critical with their name.

The pages size themselves with `vmin` units and were checked at every Xeneon Edge slot size
(840×344, 840×696, 1688×696, 2536×696 and the vertical 696×416 … 696×2536).

## Bridge API

`GET /api/wireview`

```json
{
  "ok": true, "hwinfo_running": true, "device_found": true, "age_s": 0.4,
  "pins": [{"n": 1, "voltage": 12.04, "current": 2.22, "power": 26.7}, "... 6 entries"],
  "total_current": 12.49, "total_power": 150.4, "avg_voltage": 12.04,
  "temp_in": 35.5, "temp_out": 35.8,
  "faults": {"temp_chip": false, "temp_sensor": false, "over_current_total": false,
             "over_current_wire": false, "over_power": false, "imbalance": false},
  "faults_logged": {"...": "same keys, latched since last clear"}
}
```

`GET /api/health` returns `{"ok": true}`. Options: `--port`, `--bind`, `--no-static`.

## Companion project

[wireview-nexus](https://github.com/jlobue10/wireview-nexus) shows the same readings on a
Corsair iCUE Nexus, which cannot display web content, by driving the panel directly.

## License

MIT
