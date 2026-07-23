# Error Snapshot Safety Design

## Goal

Prevent error-snapshot collection or maintenance from blocking Kiro RS request handling or
growing storage without a hard bound. Routine upstream failures such as suspended credentials,
rate limits, and quota exhaustion remain visible in Trace and credential health state, but no
longer create error snapshots.

## Incident and Root Cause

Production reached a 216 GB `error_snapshots.db` while configured with a 200 GB limit. The
hourly maintenance task then performed synchronous, row-by-row deletion while running directly
inside a Tokio task. SQLite record deletion did not immediately shrink the allocated database
file, so the maintenance loop consumed one CPU continuously and request handling stalled. Nginx
reported repeated 499 responses while the Admin UI waited indefinitely.

The volume was amplified by the capture rule: any failed upstream attempt made a successful
failover request a “recovered” error. With `errorSnapshotCaptureRecovered=true`, common 401,
403, 429, quota, transient, and network failovers stored request bodies in the snapshot database.
A final 403 was also stored as an `auth_failed` snapshot.

## Options Considered

### 1. Configuration-only mitigation

Lower retention and capacity defaults and disable recovered/body capture. This is easy but leaves
the unbounded synchronous maintenance loop and allows an operator setting to freeze the service
again.

### 2. In-process safety boundaries (selected)

Separate routine upstream observability from diagnostic snapshots, enforce per-record and
database admission budgets, and run bounded maintenance on the blocking pool. This fixes the
root causes without introducing another service.

### 3. Separate snapshot worker/service

Send snapshots to an external worker and database. This provides the strongest isolation but adds
queueing, deployment, and operational complexity that is not justified for the current project.

## Capture Policy

Trace remains the complete request-attempt ledger. Error snapshots become a narrow diagnostic
tool for failures that require payload-level inspection.

| Event | Trace | Credential state | Error snapshot |
|---|---:|---:|---:|
| 401/403 authentication or suspension | Yes | Yes | No |
| 402 quota exhaustion | Yes | Yes | No |
| 408/429/5xx/network failure | Yes | Yes, where applicable | No |
| Client validation/bad request | Yes | No | No |
| Successful failover after a routine upstream failure | Yes | Yes | No |
| Tool/thinking/SSE protocol corruption | Yes | No | Yes |
| Stream interruption after bytes were emitted | Yes | No | Yes |
| Internal serialization/integrity failure | Yes | No | Yes |

The decision is made by a focused capture-policy function before payload encoding. It receives the
final status, promoted error type, protocol diagnostics, and attempt outcomes. Routine provider
outcomes are skipped even when the overall request fails. Critical protocol and internal errors
continue to support full diagnostic capture.

## Payload Budget

Each snapshot has an 8 MiB total uncompressed diagnostic budget, including request, upstream
response, diagnostics, and stream tail. Payloads beyond the remaining budget are replaced with
metadata containing their original length and SHA-256 digest. No individual request can therefore
consume an unbounded amount of database space.

Existing secret and binary redaction remains in force. Authorization and token header values must
also never be emitted by DEBUG provider logs; logs retain only the header name and `[REDACTED]`.

## Capacity Admission

`CaptureMode` gains a disabled/capacity-exhausted state:

- Below 80% of the configured maximum: capture eligible snapshots normally.
- At or above 80%, or below the configured free-disk reserve: capture metadata only.
- At or above 100%: skip new snapshots without failing the user request.

Capacity uses SQLite page metrics (`page_count`, `freelist_count`, `page_size`) plus WAL and
fallback sizes. Allocated bytes and live bytes are reported separately. Freed pages count as
reusable capacity even though the database file does not physically shrink. The filesystem probe
is sampled and cached so normal requests do not recursively scan directories.

Fallback writes obey the same admission decision and per-snapshot budget; they cannot bypass the
hard limit. Capacity skips are represented as a non-error insert outcome and counted for operator
visibility.

## Maintenance Isolation

Periodic maintenance runs through `tokio::task::spawn_blocking`, never directly on an async worker.
Only one maintenance run may execute at a time.

Each run:

1. Deletes expired eligible records in bounded batches.
2. When live bytes exceed the low-water target (70%), deletes at most 512 eligible records or
   spends at most 250 ms in one batch.
3. Commits between batches and yields back to the scheduler.
4. Never performs an unbounded row-by-row loop.
5. Never runs automatic full vacuum. SQLite reuses freed pages; physical compaction remains an
   explicit offline/manual operation.

When capacity remains above the low-water target, the scheduler queues another bounded blocking
batch after a short yield; it does not wait for the next hourly maintenance window. Normal
retention maintenance remains hourly once capacity is healthy.

Recent trace-link reconciliation is also bounded and executed on the blocking pool. New snapshot
links continue to be attached at insert/finalize time, so reconciliation is recovery work rather
than a hot-path requirement.

## Safe Defaults

New configurations use:

- `errorSnapshotRetentionDays=7`
- `errorSnapshotMaxStorageGb=5`
- `errorSnapshotCaptureRecovered=false`
- `errorSnapshotCaptureBodies=true` (only eligible diagnostic errors, within the 8 MiB budget)
- `errorSnapshotMinFreeDiskGb=10`

Existing explicit configuration remains compatible. The admission and maintenance safety rules
apply regardless of configured values. The production server may keep error snapshots disabled
until the new build has been verified under load.

## Failure Handling

- Snapshot encoding, capacity rejection, busy SQLite, or maintenance failure must never change the
  API response delivered to the client.
- Database-busy fallback remains available only when capacity permits.
- A maintenance panic/error is logged and the next scheduled run may retry; request serving stays
  available.
- Storage status exposes allocated/live/free-page bytes, capture mode, skipped-capacity count, and
  whether maintenance is running.

## Tests

1. A final suspended 403 (`auth_failed`) produces Trace data but no snapshot.
2. A recovered 403/429 failover produces Trace attempts but no snapshot.
3. Protocol corruption and stream interruption still create snapshots.
4. Payloads over 8 MiB are bounded and replaced with length/hash metadata.
5. Soft and hard admission thresholds select full, metadata-only, and disabled modes.
6. Fallback cannot bypass the hard capacity limit.
7. Maintenance deletes only a bounded batch and preserves pinned/critical records.
8. A single-thread Tokio heartbeat remains responsive while maintenance performs blocking work.
9. Safe configuration defaults and existing camelCase configuration round-trip correctly.
10. Provider DEBUG logging never contains Authorization/token values.

## Rollout and Rollback

Run focused Rust tests followed by the full test suite and release build. Deploy a versioned image
to `kiro-rs-admin`, retaining the previous image reference and configuration backup. Verify Admin
UI/API latency, normal gateway traffic, CPU, and snapshot size before considering re-enabling
diagnostic snapshots. Rollback replaces only the Kiro RS Admin image; persistent Trace and
credential data remain untouched.
