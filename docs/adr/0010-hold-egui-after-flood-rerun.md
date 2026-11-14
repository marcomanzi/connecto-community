# ADR 0010: hold egui after the D3 flood run; budget stays open

**Context.** The reversal trigger in ARCHITECTURE.md section 4 says to switch to Tauri if the widget cannot meet the frame budget with one session flooding, or real TUIs do not render and take keys. D3 ran it without readback, old and new metrics side by side.

**Measured, run A (`yes` beside idle Codex, 28 steady windows).** 27 windows had worst p99 of 9.0 ms, against a 16.7 ms target. One window had p99 49.7 ms and max 56.4 ms, over the 50 ms limit, and lost about 22 frames. Startup reached 224 ms. Real TUIs rendered, but throttle themselves and do not load the renderer, so runs B and C prove little beyond wake latency.

**Attribution, corrected after D4.** I first cited `glow_integration.rs`, but the app runs wgpu, which is eframe 0.37's default. That was the wrong file. Reread on the right path, the conclusion stands: `wgpu_integration.rs:901` subtracts `vsync_secs`, which egui-wgpu `winit.rs` builds from `get_current_texture`, `Queue::submit` and `present`. Our own frame code took at most 1.0 ms in that window and `cpu_usage` hit 61.7 ms, so the time sits outside those three calls: in eframe's other frame work, or the thread was off the CPU. It is therefore **not** the `nextDrawable` wait of R19, and not idle waiting for the compositor. The cause is not established.

**Decision.** Do not reverse. The trigger is not cleanly met: nothing shows the widget cannot do it, one hiccup in 28 s is not a sustained failure, and a move to a web view brings the risks already rejected without any evidence it removes this stall. **The budget stays open and is reported as missed in this run.** It is not closed by choosing the new metric.

**Startup** is excluded from the flood budget by this decision, in the open, as a one-time cost. It is recorded separately and not hidden.

**Reverse if** the Tester's run of three 60 s floods, on the final build with the other pane idle, shows more than one steady window out of budget per 30 s in at least two of three runs, or a TUI renders wrongly. Then the decision comes back here.

**Tester floods on `8542a75d`, not the final build.** The clean set is run 1 and the two reruns, with no build or `find` process sampled during them. The two original runs overlapped builds and are kept, not counted.

| Run | Steady windows out of budget, of 60 | Worst window p99, old / pending | Max gap | `cpu_us` max in that run |
| - | - | - | - | - |
| `flood-1` | 0 | 13.5 / 16.5 ms | 28.6 ms | 22.5 ms |
| `flood-2-clean` | 2 (windows 3, 49) | 25.2 / 23.2 ms | 42.1 ms | 31.8 ms |
| `flood-3-clean` | 1 (window 50) | 18.8 / 18.7 ms | **146.8 ms** | 22.0 ms |

All 18 peer inputs arrived, and memory did not grow. No pane contention in the three windows out of budget.

**Reading.** The trigger is not met: 0, 2 and 1 windows, and it needs more than 2 in a 60 s run in at least two runs. `flood-2-clean` sits exactly at the line of one per 30 s. The other trigger, a TUI rendering wrongly, is not established for these runs: they capture no frames, by design, so readback cannot affect timing. Real TUIs were readable in separate non-flood runs. The final floods capture frames after the 60 s measurement. **The budget is missed, and by more than in D3:** 146.8 ms is over the 50 ms limit. The rule was set before these runs and is not changed after seeing them.

There are two different kinds of miss. Windows 3 and 49 of `flood-2-clean` have `cpu_us` p99 near 22 ms, like the D3 stall. Window 50 of `flood-3-clean` is one gap with `cpu_us` at most 2.4 ms and build at most 0.3 ms, so the frame's own work did not take the time. It sat in the excluded `get_current_texture`, `submit` or `present` calls, or the thread was off the CPU. These data cannot tell which. Load at the start of the clean reruns was 10.4 and 6.0 on a shared worker. Other load is possible, but not shown to be the cause.

**Final floods on `2f13dddc`, the final build.** Feature binary `d733370d…`, rebuilt byte-identical in an empty target. Same flood input and peer script as the `8542a75d` runs, about 7,230 frames per run, no compiler or `find` sampled in any run, sleep blocked (`tester/final-2f13/flood-summary.json`). The Architect read the summary and compared the workload.

| Run | Steady windows out of budget, of 60 | Worst window p99, old / pending | Max gap, old / pending | `cpu_us` max |
| - | - | - | - | - |
| `flood-1` | 0 | 9.0 / 12.4 ms | 16.6 / 15.1 ms | 12.3 ms |
| `flood-2` | 0 | 9.8 / 8.8 ms | 16.7 / 16.7 ms | 1.4 ms |
| `flood-3` | 0 | 9.0 / 9.7 ms | 16.6 / 15.3 ms | 12.4 ms |

**Reversal rule: not met.** 0, 0 and 0 windows, against more than 2 in a 60 s run in at least two of three runs. No TUI rendered wrongly in what was inspected. The timed frames themselves were not captured, because native capture was unavailable, so a separate 15 s flood with screenshots was inspected instead: its text was coherent and the peer pane isolated. That flood and the timed floods run plain `yes` beside a peer script, not TUIs. Real TUIs were read in the window runs on the same binary, for example `ui-smoke` shot 0038.

**Budget.** Met on the final build in 3 of 3 runs, 0 of 180 windows. **It is not shown to be fixed.** The stalls on `8542a75d`, up to 146.8 ms, have no established cause, the two trees' frame paths were not shown to differ in a way that explains this, and three clean minutes do not rule out a rare stall. Load at the start of these runs was about 4 to 5, against 6 to 10 for the two `8542a75d` reruns that missed, so load may matter; that is not established either. The cause stays a next step.

**Status.** Final for this slice: hold egui.
