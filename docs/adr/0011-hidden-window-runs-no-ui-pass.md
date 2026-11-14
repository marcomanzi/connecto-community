# ADR 0011: a hidden window runs no UI pass

**Context.** Scripted V-08 runs failed about half the time, with frames stopping for 15 to 38 s (U3). A lost repaint wake-up was suspected.

**Finding.** eframe 0.37 runs no egui pass while a window is occluded or minimized, by design. Nothing is lost: there is nothing to show. `ui-test-driver` input is applied only by a pass, so the driver waits until the window is shown. Over 50 runs, all 15 stalls had a second occlusion event, and AlwaysOnTop had none in 10 runs. The trace does not record the event's value. A direct reading now confirms it: a helper window covering the app for about 10 s gave 11 watchdog lines with `occluded:true`, `visible:false`, and frames resumed when it was uncovered (`d5/watchdog/metrics.jsonl`). That run had no flood, relay turn or login, so V-31 is still needed.

**Decision.** Accept this behaviour. Do not force painting while hidden in the default build. No session work, relay step, or Stop may depend on a UI pass. The test driver keeps its window uncovered, behind `ui-test-driver` only. The watchdog must report occlusion truthfully or label it as of the last pass.

**Consequences.** Failures in scripted runs must be classed by occlusion before they count as product failures. V-31 tests the hidden case. The earlier "not occluded" evidence is retracted, because the field was stale during a stall.
