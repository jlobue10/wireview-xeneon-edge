# WireView Pro II widgets for the Corsair Xeneon Edge

Live **per-wire current**, **total current** and **total power** from a
[Thermal Grizzly WireView Pro II](https://www.thermal-grizzly.com/en/wireview-pro-ii-gpu/s-tg-wv-p2-h19n)
on a [Corsair Xeneon Edge](https://www.corsair.com/us/en/s/xeneon-edge), through iCUE's built-in
**iFrame** widget.

| Per-wire current | Total current | Total power |
|---|---|---|
| ![per-wire](docs/img/per-wire_840x344.png) | ![total current](docs/img/total-current_840x344.png) | ![total power](docs/img/total-power_840x344.png) |

The widgets are plain HTML pages. A tiny **bridge** on the same PC reads the WireView
**straight over USB serial** and serves the readings as JSON on `http://localhost:8765`, and
serves the widget pages too. No HWiNFO, no Thermal Grizzly app; nothing leaves the machine,
and only the widget pages (the bridge's own origin and the GitHub Pages copy) may read the
JSON from a browser.

## Install

One command, in PowerShell:

```
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/main/install.ps1 | iex"
```

It downloads this repository to `%LOCALAPPDATA%\wireview-xeneon-edge`, installs Python 3.12
with winget if no Python 3.10+ is present, creates a venv with pyserial, registers a per-user
Scheduled Task named "WireView Bridge" that runs the bridge at logon (no admin rights needed),
and starts it. Then:

1. **Close the Thermal Grizzly WireView app** and turn off its auto-start. Only one program
   can hold the WireView's USB serial port.
2. Check <http://localhost:8765/api/wireview> shows `"ok": true`.
3. In iCUE select the Xeneon Edge, add an **iFrame** widget in the slot you want, and paste
   one of:
   - `http://localhost:8765/per-wire/`
   - `http://localhost:8765/total-current/`
   - `http://localhost:8765/total-power/`

   Append query options to match your limits, e.g.
   `http://localhost:8765/per-wire/?wire_limit=10.5&total_limit=55`.

Re-running the installer updates the files and restarts the bridge. Remove everything with
`install.ps1 -Uninstall` (from `%LOCALAPPDATA%\wireview-xeneon-edge`).

The installer fetches the latest tagged release (or `main` while there is none) and prints the
archive's SHA-256. To install exactly what you reviewed, pass `-Ref <tag|branch|commit>` and
optionally `-Sha256 <hash>` (each release's notes list the archive hash):

```
powershell -ExecutionPolicy Bypass -c "& ([scriptblock]::Create((irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/main/install.ps1))) -Ref v1.0.0 -Sha256 <hash>"
```

<details>
<summary>Manual setup from a clone</summary>

```
python -m venv venv
venv\Scripts\pip install -r bridge\requirements.txt
venv\Scripts\python bridge\wireview_bridge.py
powershell -ExecutionPolicy Bypass -File install.ps1   # venv + run at logon
```

The last line does the same steps in place. `-NoStart` registers without starting.
</details>

## How it works

```
WireView Pro II ──USB serial (COMx, 115200 8N1)──▶ bridge/wireview_bridge.py ──localhost JSON──▶ widget page in iCUE iFrame
```

- `bridge/wireview_serial.py` speaks the WireView's serial protocol, as recovered by the Linux
  community projects [wireview-pro-ii](https://github.com/Gustav0ar/wireview-pro-ii)
  (`docs/protocol.md`) and [wireview-hwmon](https://github.com/emaspa/wireview-hwmon). Only
  read-only commands are used (vendor data, UID, build info, sensor values) plus "resume
  display updates". The 100-byte sensor frame carries per-pin voltage/current/power, totals,
  average voltage, in/out and two external temperatures, fan duty, the cable's power rating
  and both fault masks.
- `bridge/wireview_source.py` picks the reader. `--source auto` (default) opens the COM port
  directly and falls back to HWiNFO64 shared memory (`hwinfo_wireview.py`, HWiNFO 8.41+ with
  Shared Memory Support) only while the port is unavailable. `--source serial` / `hwinfo`
  force one; `--serial-port COM5` overrides auto-detection by USB ID 0483:5740.
- While the bridge holds the port, HWiNFO's own WireView sensor stops updating; it resumes
  when the bridge exits.
- The bridge also serves `docs/`, so `http://localhost:8765/per-wire/` works without GitHub
  Pages. Cross-origin reads of the JSON are allowed only from its own loopback origin and
  `https://jlobue10.github.io`; add others with `--allow-origin`. Requests whose `Host`
  header is not a loopback name are refused (DNS rebinding), and the device's hardware UID
  is never served, so a random website open in your browser cannot read or fingerprint the
  device. Everything else on the PC (the Nexus daemon, `curl`) reads it freely.

**Which URL in iCUE?** The same pages are hosted at
<https://jlobue10.github.io/wireview-xeneon-edge/>, but that is an https page reaching into
`localhost`. Chromium 138+ classifies that as a Local Network Access request and asks for
permission; an embedded webview may deny it silently and the widget shows "Bridge offline".
Served from the bridge, page and data share one origin and nothing can block it. iCUE 5.51
bundles Chromium 130, which predates the rule, so the hosted URLs also work today; the
localhost form is simply future-proof.

## URL options

| Option | Default | Meaning |
|---|---|---|
| `host` | `http://localhost:8765` | Bridge origin |
| `wire_limit` | `10.5` | Amps per wire treated as 100 % (per-wire widget) |
| `total_limit` | `55` | Amps total treated as 100 % (total-current widget) |
| `cable_w` | cable's own rating | Cable rating in W (total-power widget); the WireView reports 600/450/300/150 |
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
  "ok": true, "source": "serial", "device_found": true, "poll_time": 1790473743.8, "age_s": 0.0,
  "device": {"port": "COM5", "fw": 5, "build": "TG-WV-PRO2-FW_20260430_1838"},
  "pins": [{"n": 1, "voltage": 12.04, "current": 2.22, "power": 26.7}, "... 6 entries"],
  "total_current": 12.49, "total_power": 150.4, "avg_voltage": 12.04,
  "temp_in": 35.5, "temp_out": 35.8, "temp_ext": [null, null], "vdd": 3.417, "fan_duty": 0,
  "cable_w": 600,
  "faults": {"temp_chip": false, "temp_sensor": false, "over_current_total": false,
             "over_current_wire": false, "over_power": false, "imbalance": false},
  "faults_logged": {"...": "same keys, latched since last clear"}
}
```

When there is no reading, `ok` is false and `status` / `hint` say why (for example
`"COM port busy"` / `"close the WireView app (and HWiNFO)"`). `GET /api/health` returns
`{"ok": true}`. Options: `--port`, `--bind`, `--no-static`, `--source`, `--serial-port`,
`--allow-origin`. `--bind` other than loopback exposes the readings and widgets to that
network and turns the `Host` check off; leave it at the default unless you mean that.

## Companion project

[wireview-nexus](https://github.com/jlobue10/wireview-nexus) shows the same readings on a
Corsair iCUE Nexus, which cannot display web content, by driving the panel directly. When
both run on one PC the Nexus daemon reads from this bridge, so the two never fight over the
COM port. `wireview_serial.py`, `wireview_source.py` and `hwinfo_wireview.py` are identical
in both repositories.

## License

MIT. WireView serial protocol details from wireview-pro-ii and wireview-hwmon (MIT).
