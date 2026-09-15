# ADR-0014 — IQ capture ring: pre-allocated, persistent on-disk ring for the rolling IQ buffer

**Status:** PROVISIONAL (T-178, core interface, reviewed before merge)
**Touches:** C26 spectrum history / recordings, the Capture timeline ([ADR-0013 §4.9 gap 1](0013-ui-architecture.md)), [ADR-0006](0006-storage.md) storage homes
**Code:** `crates/hk-store/src/iqbuffer.rs` (format, writer, recovery), `crates/hk-pipeline/src/iqbuffer.rs` (feeder, clip export), `crates/hk-api/src/iqbuffer.rs` (routes). Contract: [`docs/api.md` "IQ capture buffer"](../api.md).

## Context

T-157 shipped the rolling IQ capture buffer as grow-and-delete chunk files with an in-memory index, deleted when a run ends. The user decided (2026-09-15):

1. Retention **survives restarts**.
2. Retention stays time-based with an optional size cap. The flags and defaults are unchanged: `--iq-retention` (default `2m`) and `--iq-buffer-max`.
3. Storage is a **pre-allocated on-disk ring**:
   - the full quota is allocated up front and the oldest data is overwritten in place, with no grow-and-delete;
   - disk usage is bounded and known from the start, and flash wear is even;
   - crash recovery re-reads the ring and its index.
4. Large quotas (staging runs `--iq-retention 1h`, 144 GB at 20 Msps) must not block serving. Too little free space must refuse or shrink the ring, with a clear status flag.

Constraints carried in from T-157:
- capture never blocks, and drops are counted;
- clips select exact sample-index ranges;
- clips have a size guard;
- a free-space floor is kept;
- clip export needs auth.

## Decision

### Files (`<data dir>/iqbuffer/`)

| File | Content | Size |
|---|---|---|
| `ring.ci8` | `slot_count` slots of `slot_bytes`, interleaved ci8 (2 bytes/sample) | `slot_count × slot_bytes`, fixed while open |
| `ring.journal` | append-only CRC-framed records; rewritten as a snapshot on open and when it outgrows 1 MiB and 4× its last snapshot | bounded (KiB..MiB) |
| `ring.lock` | `flock(LOCK_EX\|LOCK_NB)`: a second buffer on the same directory fails to open (`allocation: "locked"`) | 0 |

- **Geometry.** `slot_bytes` = quota / 16, clamped to 64 KiB..64 MiB (T-157's chunk size). `slot_count` = ⌊quota / slot_bytes⌋, at least 2. A 1 h ring at 20 Msps is 2146 × 64 MiB.
- **Allocation.** `preallocate`:
  - Linux: `fallocate(fd, 0, 0, len)`.
  - macOS: `fcntl(F_PREALLOCATE)` (contiguous, then any), then `ftruncate`.
  - Elsewhere, or when the filesystem refuses: a sparse `ftruncate`, reported as `preallocated: false`.
  - Neither call writes data, so allocation is metadata-only.
  - **Measured** on the dev Mac (APFS, 2026-09-15): `F_PREALLOCATE` of 8 GiB plus `ftruncate` took 269 ms, and free space dropped by 8 GiB (the space was reserved). Extrapolated linearly, a 144 GB ring takes about 5 s. That time is not verified at that size.
- **In the background** (fix round). The whole open (lock, version check, recovery, allocation, snapshot) runs on `hk-iqbuffer-alloc`, so a large quota (or a filesystem without `fallocate`) never delays the run's start or the API.
  - Allocation grows the file in steps of whole slots near 1 GiB (`ALLOCATION_STEP_BYTES`), publishing `allocation_progress` and checking a cancel flag between steps. Stopping the run cancels and joins it (at most one step), and a cancelled or failed allocation truncates the file back to its old size.
  - While allocating, status is `enabled: false, allocation: "allocating"`, clips are unavailable, and **capture is not buffered**. Each segment's feeder first waits up to 2 s (`ALLOCATION_WAIT`, or until the run stops) without reading, so a quickly opened ring buffers from the segment's first block; after that it reads and discards blocks until the writer exists. Capture never waits for the feeder, and the bounded wait caps how long a stop or re-plumb can wait on it. The alternative (writing into the already-allocated prefix) was rejected: recovery and the slot geometry would need a variable ring size mid-run, for a few seconds of history.
- **Lock held** by another process: the open fails with `RingLocked`; the status reports `allocation: "locked"` and a reason, and the run carries on without a buffer.

### Logical log and slots

- **Addressing.** Bytes live on a monotonic logical log. Logical slot `L` covers `[L·S, (L+1)·S)` and sits at the ring position its `open` record names.
- **Choosing a position.** A new slot takes the lowest unused position. If none is free, it takes the position of the **oldest** slot:
  1. the log floor passes that slot's end, under the index mutex;
  2. its segments are trimmed;
  3. `open {slot, pos}` is journalled with fsync;
  4. then its first byte is overwritten.

  At steady state the positions cycle in order, so every position is rewritten in turn.
- **Positions are explicit** rather than `L mod N`, so a slot-count change can keep data without copying.

### Journal records

Each record is framed as `[len u32 LE][crc32 u32 LE][JSON payload]`. Recovery stops at the first short, CRC-failing or unparsable record.

| `k` | Fields | Meaning |
|---|---|---|
| `header` | `magic, version (1), slot_bytes, slots` | first record; a different `slot_bytes` or an older version resets the ring; a **newer** version (read loosely from the first record, before anything is changed) disables the buffer with `allocation: "incompatible"` and leaves the ring untouched |
| `run` | `run` | a buffer opened the ring; stream indices restart per run |
| `open` | `slot, pos` | slot `L` is written at `pos` from here on; any older slot at `pos` is dead |
| `seal` | `slot, bytes, crc, floor` | the first `bytes` of the slot are durable with CRC-32 `crc`; the log floor (duration eviction) is `floor` |
| `seg` | `id, run, log_start, global_index, t_ns, content_class, dropped_before, provenance` | a segment starts at logical byte `log_start`; it ends at the next segment's start or the sealed end |

### Write path and durability

- **Writer.** One writer thread (a ring reader, as in T-157) `pwrite`s at `pos·S + offset`. A CRC-32 of the current slot runs incrementally (slicing-by-8).
- **Checkpoint.** A checkpoint happens every 1 s, when a slot fills, and when a run's segment ends or the writer drops. It runs in order:
  1. `fdatasync` the ring;
  2. append the not-yet-journalled `seg` records that hold samples, and `seal` of the current slot;
  3. `fdatasync` the journal.

  A seal therefore always follows the data it covers, and a segment start always precedes the seal that covers its bytes.
- **Failed ring fsync: poison, never seal** (fix round). After a failed `fsync`, the unsynced pages may be gone while a *later* `fsync` succeeds (Linux clears the error), so sealing the same bytes then would put a seal over data that never reached the disk. The writer instead rolls the current slot back to its last seal: the cursor, the slot's byte count and CRC return to the sealed point, the log end and the segments covering the span are cut there (segments starting inside it are dropped; they are never journalled), and the open segment ends. The span's samples are counted in `poisoned_samples` and the failure in `sync_errors`. No clip can export the span, no seal covers it, and the next writes overwrite it and are sealed only after their own successful fsync. Only the current slot can hold unsealed bytes, because a slot is sealed before the writer moves on.
- **Failed writes.** A failed journal append truncates the journal back to its last good length, so a torn record never hides later ones. Write failures keep T-157's semantics:
  - the segment ends;
  - `write_errors` and `failed_samples` are counted;
  - writes back off from 100 ms, doubling to 10 s.
- **Never blocks capture.** All I/O is on the writer thread and outside the index mutex. A lapped reader counts `dropped_samples`.
  - **Measured** (fix round, dev Mac APFS, debug build, `iq_capture_ring_rate`, ignored by default): the mock SDR device at 20 Msps paced in real time for 30.3 s with a 1 GiB ring (64 MiB slots, so it wrapped once and checkpointed every second and every slot) produced 600 244 224 samples; the buffer stored 600 178 688 (the one-block difference was in flight when the status was read), **dropped 0**, with 0 write, fsync or poisoned samples. The pipeline's always-on readers lost 0 samples both with the ring on and off. The feeder is already a dedicated thread behind a bounded queue (the capture ring), which drops and counts and never blocks, so no change was needed.

### Recovery (on open)

1. Take the lock. Delete T-157 chunk files (`<16 digits>.ci8`).
2. Read the journal up to its first bad record. Reset if the header is missing or its `slot_bytes` or version differ.
3. Replay the records:
   - the latest `open` per position wins;
   - each slot keeps its longest seal;
   - the floor is the maximum sealed `floor`.
4. Keep the slots that meet all of these:
   - sealed, with `pos < slot_count` (after a resize);
   - lying inside the ring file as it was before this open (a truncated file drops them);
   - newest first, logically contiguous, every older slot full, at most `slot_count`.
5. Re-read the newest sealed slot and check its CRC. On failure, drop it and everything newer, then check the next one, up to 4 slots. None passing leaves the ring empty. This bounds startup I/O to at most 4 slots, 256 MiB. Older slots are trusted because their data was fsynced before their seal.
6. Rebuild the segments from `seg` records, cut to `[floor, sealed end)`, and resume writing at the sealed end (inside the newest slot if it is partial).
7. Increment `run`. Write a compacted snapshot: a temp file plus fsync, renamed over the journal, then fsync the directory.

Unsealed data is lost on a crash: at most 1 s of writes, or one partial slot.

### Quota change between runs

- **Slot size changes** (the quota crosses the 64 KiB..64 MiB clamp range, e.g. 256 MiB to 4 GiB): the ring **resets**.
- **Slot count grows:** the file is extended and everything is kept. The new positions are used before any old slot is overwritten.
- **Slot count shrinks:** the file is truncated. The slots stored at positions below the new count are kept, as the newest contiguous run of them (which may be older data), and the rest are discarded (`discarded_slots`).

### Free space

- **Check at allocation.** Usable space = `free − floor + existing ring file size`, where the floor is T-157's: 10 % of the filesystem, clamped to 2..8 GiB.
  - If the quota does not fit, the ring shrinks to ⌊usable / slot⌋ slots (`allocation: "shrunk"`).
  - Below 2 slots it is refused: the buffer is disabled with a reason and `allocation: "refused"`.
- **Sparse ring.** It keeps T-157's pause: writing pauses while the next slot would leave less than the floor free.

### Runs, clips and the API

Stream indices and sample-clock times restart when a process restarts. A replayed recording even repeats its times. So:
- every segment carries `run`;
- `POST /api/iqbuffer/clip` accepts an optional `run`.

**Range selection without `run`:**
- an index range selects in the current run;
- a time range may match only one run, otherwise `409 conflict` (never spliced across a restart).

**Clip reads.** A clip plans its reads under the mutex and reads outside it. After each read it re-checks that the floor has not passed the read's start. Data overwritten meanwhile fails the clip (`404 not_found`) instead of exporting another slot's bytes.

**New status fields:**
- `slot_count`, `allocated_bytes`;
- `allocation` (`full`/`shrunk`/`allocating`/`refused`/`locked`/`incompatible`), `allocation_progress`, `preallocated`, `persisted: true`;
- `run`, `recovered_segments`, `discarded_slots`;
- `head_slot`, `head_offset_bytes`, `wrap_count` (head slot ÷ slot count);
- `sync_errors`, `poisoned_samples`.

**Changed meanings:**
- `chunk_bytes` is the slot size;
- `chunk_files` counts slots holding retained data;
- `disk_bytes` is ring plus journal;
- `evicted.chunks` counts overwritten slots.

### Tests keep rings small

The default quota is 4.8 GB, allocated up front. So (fix round, replacing a nextest setup script):
- `PipelineConfig::new` leaves the buffer **off**; only the `hk`/`hackriffd` composition (`config_for`) turns it on from the environment and flags;
- every test that buffers sets an explicit small quota (`IqBufferConfig { max_bytes: Some(..) }` or `--iq-buffer-max`), so plain `cargo test` and nextest need no environment or experimental features.

## Consequences

- **Disk.** Usage is the quota from the first second and never changes while a run is open. Staging at `--iq-retention 1h` reserves 144 GB at 20 Msps (14.4 GB at 2 Msps), so it should set `--iq-buffer-max`.
- **Flash wear.** Positions are rewritten in rotation. The write rate is unchanged, as T-157 documented.
- **Restart cost.** A snapshot rewrite plus at most 4 slot CRC reads.
- **Loss on crash.** A crash (not a clean stop) loses up to 1 s of the newest data.

## Alternatives considered

- **Single file with `L mod N` positions.** Simpler, but a quota change would need copying up to 144 GB, or a reset.
- **Fixed per-slot header table in the ring file.** A table bounds metadata, but segments per slot are unbounded (retunes, overruns). The compacted journal holds both.
- **CRC of the whole ring on recovery.** Minutes for 144 GB. Rejected: fsync-before-seal plus a check of the newest slots covers torn writes.

## Unverified

- Whether `F_PREALLOCATE` reserves space on APFS until written. It returns success and reported free space drops, but behaviour under snapshots is unmeasured.
- The fsync cost on the Jetson's eMMC or NVMe at 20 Msps.
