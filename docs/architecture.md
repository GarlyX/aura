# aura — architecture

Design targets, in order: **memory footprint**, **frame throughput**,
correctness. Every subsystem below exists because profiling of a reference
layer on the test machine (16 GB RAM) showed the title's world frames taking
0.5–15 s, driven by paging (process at 23–25 GB), synchronous GPU submission,
and per-request DirectStorage writes into paged memory.

## 1. Title memory is owned, not copied

The reference layer keeps a system-memory shadow of GPU resources, hashes it
to find changes, then uploads. That costs gigabytes of commit and tens of
milliseconds per frame.

aura instead:

- Maps the title's address space as far as the host allows (a graphics heap
  for buffers and textures where the host GPU accepts the layout; a plain
  reservation plus targeted upload where it does not).
- Detects writes with page protection (write-watch) as the *only* change
  signal; hashing is a fallback for aliased views and is always incremental
  (a rotating slice per frame, escalating to full scans only for regions that
  keep changing — e.g. video playback targets).
- Never keeps a second full-size shadow of a title resource on the CPU side.

Budget: the layer's own committed memory stays under ~1 GB plus the D3D12
heaps it must own.

## 2. Asynchronous GPU submission

Submissions are never waited on by the submitting thread. A relay thread
retires completed flights and releases their resources. Waits that the title
requests (fences, memory-value waits) are answered from GPU progress, not by
polling page tables per call.

## 3. DirectStorage write path

- Persistent page windows for `PAGE_DESTINATION` requests: mapped once,
  reused, instead of one fresh view per request.
- Writes land in committed (non-paged) staging before touching title memory
  when the working set is under pressure.
- Decode (zlib/BCPack) parallel across all cores, priorities by weighted
  round-robin.

## 4. Shaders

Console GPU code is translated to DXIL ahead of time where the pipeline
library allows it, on first use otherwise, and cached on disk. Translation
failures are counted per pipeline and reported; no draw is silently dropped
without a statistic naming its shader.

## 5. Clocks

The title reads time through the performance counter. When rendered frames
run long, the counter shown to the title slows so that one frame never looks
longer than a configurable cap (default 100 ms); this keeps the title's
streaming balance intact while the world is slow. The layer's own clock used
for pacing and presentation is unaffected.

## 6. Structure

The loader runs the title's x86-64 code unchanged in-process; the HLE crates
supply the console OS surface underneath it. Import resolution order: our
module, then a host function if policy allows, then a DLL shipped with the
title, then a stub that names itself when called.

## What is deliberately out of scope for now

Ray-traced lighting, HDR output, networked play. The title is offline by
design.
