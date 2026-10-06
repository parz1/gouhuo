# Client runtime

This crate contains the client voice lifecycle, recovery policy and self-state
projection. It depends on `voice-core` and `protocol`; it has no Slint dependency
and imports no platform audio API. Desktop, mobile or another native frontend
supplies an `AudioBackend` adapter.

The default `in-process` feature includes the lifecycle worker. With
`--no-default-features`, this crate instead exposes frontend data, `VoiceControl`,
call commands, projections and recovery policies without compiling the worker,
Opus or APM. `CallController` accepts `&dyn VoiceControl`; the current
in-process `RuntimeHandle` and `client-process::RuntimeHandle` implement it.
The process proxy supplies the same call controls and cached facts.
Intent changes must be visible locally before a
control method returns; successful transmission still comes from engine facts.

`voice-types` owns `TransmitMode`, `VoiceStats` and the volume limit, with no
dependencies by default and an optional serde feature. The old
`voice_core::pipeline` paths re-export these same types. The `ipc` feature
defines versioned wire messages; Rust types alone are not a stable ABI.
In-process startup receives a shared sequence allocator. Cross-process startup
transfers fresh authenticated keys and lets the engine own that allocator.
Identity remains a minimal `client-core` dependency on `voice-core`.

The desktop uses `client-process` and the separate `gouhuo-voice` executable.
The workspace dependency disables the default worker; engine hosts must enable
`in-process` explicitly. Automatic engine installation and rollback remain
future work. See [the engine boundary](../../docs/voice-engine-boundary.md).

```powershell
cargo check -p client-runtime --no-default-features --lib --locked
cargo test -p client-runtime --no-default-features --locked
python scripts/check-voice-boundary.py
```

`VoiceRuntime` starts one long-lived lifecycle worker. Frontends use a cloned
`RuntimeHandle` for intentions, commands and snapshots. The worker is the only
owner of a `Pipeline` or `MicCheck`. A frontend handle cannot join audio threads
when it is dropped.

```text
frontend → RuntimeHandle → shared intentions / light controls
                        → lifecycle requests → worker → Pipeline / MicCheck
frontend ← RuntimeSnapshot ← statistics / portable diagnostics
frontend ← SelfStateView::project(snapshot, connection, ptt_bound)
frontend ← CallHealth::tick(now, snapshot, reconnecting)
```

The portable `call` module adds a lightweight presentation boundary:
`CallState` owns the control-connection phase and notices, `CallController`
accepts typed commands using the current Client/runtime, and `CallViewModel`
projects plain Rust rows, members, chat, permissions and audio state. It contains
no GUI types or images. `project_audio` is the frequent poll path and needs no
roster walk. Frontends adapt these values into their own toolkit; window focus,
overlays, drafts, scene layout and device preferences stay in the frontend.
The client does not rehydrate local mute/deafen intentions from old server
echoes or presentation properties. See `docs/client-ui-architecture.md`.

Before the first `start_voice`, apply the intended local mute/deafen state and
transmit mode. `StartVoice` supplies the host, UDP port, session ID, session keys,
shared `VoiceSequences` and device IDs. DNS, APM initialization, replacement and
retirement run on the worker. Reusing session keys across a device replacement
requires reusing the same sequence allocator. A new authenticated session must
provide fresh keys/sequences, clear member volume IDs and update the server mute
constraint from the new roster. Local intentions remain separate from server
constraints and self-state echoes.

Each start/stop receives a monotonically increasing request ID. An in-flight
preparation checks that ID before opening the pipeline and before committing
the result. Candidates initially have all UDP sending disabled, including
keepalives. The worker applies the latest mute/deafen/PTT/mode/volume intentions
before accepting and enabling the candidate. Stale results are retired without
sending. Muting, PTT release and quiescing use current lightweight controls and
do not wait behind DNS, APM or device retirement.

`AudioBackend::capture` and `render` run on their respective audio threads, at
the first read/write. Returned streams are used and destroyed on that same
thread. `processor` runs on the lifecycle worker. A diagnostics provider returns
quick snapshots of already collected data; it must not enumerate/open devices
or perform other blocking work. Only `CaptureInfo` crosses the platform boundary.

`Preparing` means background initialization is pending. `Starting` means the
pipeline exists but the required devices or authenticated UDP probe are still
pending. `Ready` requires capture, playback and UDP health. `RuntimeSnapshot.timings`
records measured queue/retirement/DNS/processor/start durations and times from
request submission to the first successful capture/render frame. Authenticated
UDP readiness is first observed by the worker with up to 20 ms polling delay.
These measurements describe voice initialization; they do not measure GUI frame
rate or the total time taken to enter a page. Directional capture,
playback and transport errors remain distinct: a playback failure does not claim
that an operational microphone stopped sending.

`stop` immediately invalidates pending work and disables current audio, then
retires it on the worker. `quiesce` disables authenticated transport while
allowing local cue playback. `stop_after` preserves local playback for a short
farewell cue and cannot stop a later session or mic check. `shutdown` is terminal;
it is used when the application exits. After the GUI event loop has ended, an
exit coordinator can call `VoiceRuntime::wait_stopped` with a bounded timeout.

Tests combine controlled audio factories with the real Opus/encrypted UDP
pipeline. They cover stale preparation, initial mute, immediate PTT/mute,
device-thread affinity, worker cancellation, delayed retirement and reordered
commands, alongside pure self-state and recovery policy tests.

```powershell
cargo test -p client-runtime --offline
cargo clippy -p client-runtime --all-targets --offline -- -D warnings
```
