# Distributed processor exports for Abner

23 November 2026 · Technical proposal

## Decision

Let a single export distribute independent units of processor work across paired LAN workers. The initiating Abner instance owns the export and schedules the units. An **Include This Mac** control lets its own processor runtime take assignments from the same queue. The coordinator always owns final output publication.

Start with one real processor that can demonstrate equivalent output when split, and use Abner's existing writer on the initiating Mac. Design the result protocol so worker-side encoding can be added if transfers limit throughput. Success means a faster complete export of one clip at equivalent quality, including discovery, source staging, transfers, and finalization.

## What the code supports today

- [Processor API](crates/processor-api/src/lib.rs) is host-driven. Abner pushes ordered frames into a session, the session returns zero or more frames, and finish drains its tail. A plugin can buffer a batch internally. The `Batch { min_frames, max_frames }` declaration describes bounds, with short-tail padding left to the implementation. It does not expose the exact partition, prove that separate sessions produce equivalent results, or let a worker pull a batch from Abner today.
- [Export pipeline](src/media_io.rs) opens a source per kept range, configures stage dimensions and rates, binds spatial inputs, negotiates formats, and runs a chain between bounded decode and writer handoffs. It validates output timing, and its writer encodes video and muxes audio. Kept ranges already provide intentional session boundaries. An arbitrary worker boundary inside a kept range has different semantics.
- [Export page](src/export_page.rs) owns launch, pause, progress, and completion of the current queue. It can continue to expose one export job while that job has several active assignments.
- [RIFE Metal](plugins/defaults/rife-metal/src/lib.rs) declares a temporal window and changes output rate. It is a concrete follow-up candidate for testing pair overlap and timestamp ownership. A batch upscaler such as SeedVR2 is the preferred performance pilot **if** its installed implementation can expose its actual partition and pass the equivalence tests. The public batch declaration alone is insufficient. No distributed performance or model equivalence has been measured.

## Processor contract and planning

Add a versioned, opt-in planning capability to the processor runtime. Keep the existing streaming interface for local execution and legacy plugins. Use the same partition planner for serial and distributed execution of the pilot processor, so changing the worker count never changes the model's temporal grouping.

For a configured processor, the plan must define the canonical input grouping, permitted reset points, borrowed context, first/last-unit behavior, padding at the actual segment tail, and output ownership. Each unit gets a stable ID and a half-open output interval expressed in integer ticks or output frame indices. Inputs carry original timestamps, source segment identity, and an explicit decode range. An implementation can group adjacent units in one assignment to amortize setup, while preserving the canonical boundaries inside it.

| Capability | Rule for splitting one kept range |
| --- | --- |
| Independent frames | Opt in once Abner can verify session resets and output timing are independent. Bundle several frames into a unit. |
| Bounded window | Supply real neighboring frames within the original kept range. Each unit emits only its owned output. Maintain phase for rate-changing processors. |
| Bounded batch | Ask the configured implementation for the canonical grouping and padding. Confirm any dependence on prior batches before dispatch. |
| Recurrent | Treat the original segment as indivisible unless the plugin supplies a verified restart contract. |
| Unspecified or whole clip | Keep existing execution semantics. The current export path already rejects whole-clip stages. |

For the pilot, distribute a single supported stage on a static-input export. Use Abner's existing serial path for other configurations. Expand to chains after validating how upstream rate changes, format conversion, crops and masks, and overlapped input compose. A later serial stage can consume reordered output from distributed work in one uninterrupted session. Splitting through multiple stages requires a valid split and time mapping for the entire distributed portion. Custom frame sources and named spatial inputs qualify only after their dependencies and seek behavior can be reproduced remotely.

## Worker discovery, pairing, and compatibility

- Run the worker headlessly using an entry point such as `abner worker`, with **Share Processing** as a later GUI entry point to the same runtime. Use Apple Silicon macOS for the pilot. Bind to the intended LAN interface and advertise the service via Bonjour. Offer an explicit host endpoint for networks where discovery traffic is isolated. Probe the service's advertised capability endpoint after discovery.
- Require one-time pairing, using a short-lived pairing code with confirmation of peer identity. Store peer identities and use authenticated, encrypted connections. Treat discovery data as untrusted. The worker executes registered processor IDs and validated settings using installed plugins. The protocol accepts job-scoped media assets and typed requests. Give workers per-peer admission limits, configurable compute and disk quotas, and a way to revoke a pairing.
- Match protocol version, processor implementation and model fingerprints, supported layouts and color metadata, and required runtime capabilities before assignment. Compatible hardware may differ in throughput and numerical output. Qualify mixed workers against the pilot's output tolerances before enabling those combinations.
- **Include This Mac** changes eligibility for processor assignments. Local decoding for previews, coordination, and final encoding or muxing can still use resources. When remote-only mode has no eligible worker, show the reason and wait or allow cancellation. A user can explicitly select local execution.

## Scheduling and media transfer

Make the coordinator the sole owner of job state. Workers pull assignments when they have capacity. Start at one active GPU assignment per worker and keep the model loaded where the plugin supports reuse with correct session resets. Prioritize the next output interval the writer needs and cap in-flight work by both interval count and bytes. A slow worker gets fewer assignments automatically. Use measured stage time and cache locality to group work and decide whether another worker is likely to help.

Send an immutable manifest with asset IDs, kept ranges, processor fingerprints and settings, frame and color contracts, and the unit plan. Workers read validated shared-storage copies when configured or receive compressed media into a bounded cache keyed by content hash. Resolve local and shared paths on each machine from the asset mapping. Use resumable staging, verify content before use, and pin active assets against eviction. Include required side inputs in the manifest when supported. For a cold cache, account for upload time before assigning small jobs. Begin with whole-source staging for eligible jobs and measure before adding compressed range transfer. Range transfer would need demux metadata, reference frames, and decoder preroll.

Decode on the worker from a suitable random-access point and discard preroll before the planned input. Preserve the negotiated format, plane dimensions, alpha, bit depth, color metadata, and timestamps on the wire. Use bounded binary payloads with length and integrity checks. Keep control traffic responsive during transfers, and apply backpressure from the final writer all the way to assignment. Never silently convert high-bit-depth or alpha-bearing output to RGBA8 for transport.

## Result transport and encoding

| Mode | Worker returns | Coordinator does | Cost |
| --- | --- | --- | --- |
| Central encode, pilot | Timestamped frames in the negotiated layout, with optional lossless transport compression | Validates and orders frames, then feeds the existing writer and encoder | Preserves one continuous encode. Transfers and the serial writer can limit throughput. |
| Worker encode, follow-up | Video-only packets for a contiguous span of one or more units | Validates codec configuration and timing, rebases timestamps, and muxes video with the original audio and chapters | Reduces traffic and parallelizes encoding. Requires explicit codec, quality, and container qualification. |

An encoded segment is a separate unit of work from a plugin batch. A worker can keep one encoder open across several consecutive batches, then return the segment. Start worker encoding with a verified ProRes profile because samples are independently coded. Even then, require matching profile, dimensions, pixel and color metadata, and valid sample timing before assembling. For H.264 or HEVC, require independently decodable segment starts, no cross-segment references, compatible parameter sets/sample entries, and correct decode and presentation timestamps, including frame reorder. Segment resets alter GOP placement, rate control, and lookahead, so quality and bitrate can differ from one continuous encode. Verify packet-level remux and playback before offering each combination. A lossy intermediate followed by final encode would add another compression pass and needs a separate quality decision.

UHD RGBA8 is 33.2 MB/frame. Four aggregate output frames per second would already be about 133 MB/s before transport overhead, above the theoretical 125 MB/s for a 1 Gb/s link. Size the pilot using actual negotiated layouts and aggregate **rendered** fps. Lossless compression ratios depend on the footage. Track cold and warm source-cache runs separately.

## Lifecycle and correctness

- Lease each assignment with a job ID, unit ID, and attempt ID. Renew by heartbeat, independently of progress. Requeue after lease expiry or retryable failure and accept results only for the current attempt. Use a finite retry budget and report deterministic plugin or media failures directly.
- Commit a unit only after validating the complete result or its staged file. Bound reorder storage and drain contiguous units as soon as the writer can accept them. Validate global timestamp continuity, owned-frame count, and expected output duration across unit joins and existing edit boundaries. Audio, chapters, subtitles where supported, and existing kept-range mapping stay with the coordinator.
- On pause, stop new leases and let in-flight units finish within the bounded result budget. Surface **Pausing** until those units settle. Resume from completed units. Cancel invalidates leases, requests cooperative cancellation, and cleans unpublished output. The pilot keeps job progress in the running coordinator. Crash-resume across coordinator restarts requires a durable manifest and completed-unit journal and is a follow-up.
- Show completed output time, active workers, transfers, and finalization separately. Count unique committed units once despite retries. Publish the final file atomically using the export path's temporary output and no-overwrite behavior. If writing or muxing fails, surface the error and preserve any existing destination.

## Implementation and acceptance

1. **Prove split equivalence in-process.** Extract the export preparation and execution needed by one headless unit while reusing the current session, format, and writer code. Add one explicit planner and a deterministic test processor. Compare one-stream execution with independently rendered units delivered in arbitrary order. Qualify the real pilot plugin on short tails, cuts, nonzero starts, and high-motion seams. Require exact timing, frame counts, and metadata. Require exact pixels for deterministic fixtures and document any measured tolerance for the real model across supported hardware.
2. **Prove one export across two Macs.** Add the versioned worker protocol, pairing/discovery, compressed source staging, lease scheduler, and central-encode result path. Exercise remote-only and local-plus-remote runs with the same clip and settings. Force a disconnect, a slow worker, a stale duplicate result, and a pause or cancel during processing and transfer. Verify output order, A/V sync, bounded memory and disk usage, and that only unfinished work is retried. Then wire the existing Export page to the proven path. Whole-job scheduling can reuse worker infrastructure later for chains that cannot split.
3. **Measure before widening rollout.** Run the same source, model, and encoder settings on one Mac, two Macs, and more if available, with cold and warm caches on the user's actual LAN. Record wall-clock export time, time to first output, per-worker throughput, staging and return bytes/time, writer utilization, and peak memory. Compare several runs and inspect every join against the serial baseline. Use a proposed pilot target of at least **1.3× end-to-end speedup** on a representative compute-heavy export with two similarly capable Macs and a warm cache. Publish cold-cache results and the break-even clip length. If results miss the target, identify the measured bottleneck before choosing further work.
4. **Add worker encoding where justified.** Qualify one ProRes configuration end to end. Compare packet timing, playback across joins, throughput, and output size; then evaluate interframe codecs individually. Add more processors and chain shapes only after their split behavior is verified.

The main unknowns are whether the chosen upscaler can reproduce its local batch semantics in independent sessions, whether model initialization can be amortized across units, and whether the result link or single encoder becomes the next bottleneck. The first two milestones expose those risks before work expands to general distributed exports.
