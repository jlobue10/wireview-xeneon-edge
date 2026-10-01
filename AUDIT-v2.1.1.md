# v2.1.1 audit follow-up

Audited tag `v2.1.1`, commit `7128fe49e24f401381e467400d1c413cda1a2538`, on 2026-10-01 UTC.
The companion Nexus was audited at `e50baec54225404f6a840c32a5c9926d4e36e28b`.

## New finding

| ID | Severity | Trigger and effect | Fix |
| --- | --- | --- | --- |
| WV-12 | Low | Run a copied `install.ps1` with an executable beside it and `-Dir` pointing to a different folder. The installer skips the release download, then reports a missing destination executable or runs an existing destination binary while ignoring the requested release and checksum. | Select in-place mode only when the script folder is also the destination. A different explicit destination follows the normal download and checksum checks. |

Regression cases cover an empty destination and one holding an older executable. Both verify that the destination receives the downloaded binary and install marker. Existing install/uninstall cases still preserve source files and user files.

## Verification of the released tag

- 67 Rust tests, nine widget tests, formatting, Clippy and the PowerShell installer regressions passed on Linux.
- The tagged commit's [Windows/Linux CI](https://github.com/jlobue10/wireview-xeneon-edge/actions/runs/36781532423) passed.
- `cargo audit` 0.22.2 checked 72 locked packages against RustSec commit `9b3a3b73a7f42606494c943e95f8196e9994df46`: zero vulnerabilities and zero warnings.
- The published executable's SHA-256 is `91a1faa98eb949ee1f4d0a45453f87d89b62a8b917e17fefacb602efd44331f2`. It matches the release checksum. GitHub CLI verified its signed provenance against this repository's release workflow at this exact tag and commit, rejecting self-hosted runners.
- The release installer matches the tagged source after normalizing Windows line endings.
- A deterministic 20,000-input sweep exercised the HWiNFO, HTTP and serial parsers without a panic or an out-of-bounds HWiNFO read.

The earlier fixes for HWiNFO completeness, widget freshness/polling/ports, response deadlines, secret creation and uninstall ownership are present and their regressions pass. v2.1.1's additional visibility-failure-reset regression passes too. No new confirmed security vulnerability was found in the reviewed scope.

## Limits

The independent runtime checks ran on Linux; real Windows Shared Memory, Task Scheduler, USB serial and iCUE behavior were not exercised here. Windows results above come from the repository's CI. HWiNFO fault records still rely on their expected ordering; no new real-device dump was available to validate selectively hidden fault records. Optional disk-served pages retain the previously noted filesystem race/Windows-path review questions; these were not promoted to confirmed vulnerabilities.
