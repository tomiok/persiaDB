---
title: "Server, cloud, and Kubernetes"
description: "The planned gRPC and HTTP server, object storage, sharding, and SDKs."
---

**Planned.** Nothing here is running. The server milestone (M11) starts after the embedded API.
Cloud backends (M12) and distribution (M13) come after the server. SDKs (M14) follow the protocol.

## Server

gRPC is the primary API; HTTP/JSON mirrors it for tooling. Both track `persia.proto`
(SPEC §12.1–§12.2). Generated Rust code is committed; building the project does not need `protoc`
(ADR-0003). The service sketch in the SPEC is illustrative:

```proto
service Persia {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc Put(stream PutRequest) returns (PutResponse);        // docs, optional blob chunks
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  rpc Get(GetRequest) returns (Document);
  rpc Search(SearchRequest) returns (SearchResponse);      // includes relaxation info
  rpc GetBlob(GetBlobRequest) returns (stream BlobChunk);  // supports ranges
  rpc Commit(CommitRequest) returns (CommitResponse);
  rpc Stats(StatsRequest) returns (StatsResponse);
}
```

Responses carry the manifest sequence they were served from, so a client can ask for
read-your-writes with `min_seq`. Errors map to canonical gRPC codes.

TLS is optional and on by configuration, with API keys or bearer tokens, and optional mutual TLS
(SPEC §12.3). Embedded mode stays unauthenticated. Health checks are `/livez` and `/readyz`
(ready means recovery has finished). Shutdown drains, flushes, and drops the writer lock or lease.

Observability is `tracing` spans, OpenTelemetry export, and a Prometheus `/metrics` endpoint:
query latency by relaxation level, cache hit ratio, segment counts, merge backlog, WAL bytes,
replica lag, and blob throughput (SPEC §12.4). Logs are structured JSON, and secrets are not logged
(SPEC §17).

The container image is planned as distroless, non-root, with a read-only root filesystem, built in
CI for amd64 and arm64 with an SBOM (ROADMAP 11.5, SPEC §11.6).

## Cloud

Opening `s3://…`, `gs://…`, or `az://…` selects the backend (SPEC §10.2). The remote layout under
a prefix is immutable manifests, segments, tombstone sidecars, WAL chunks, blobs, and a `LEASE`
object (SPEC §10.4). Commit is put-if-absent of the next manifest. Each provider's conditional
create is part of the work (MinIO `If-None-Match`, GCS generation match, Azure `If-None-Match`),
and a backend that cannot do it fails clearly (ROADMAP 12.1.4).

Provider quickstarts and a note on request cost are ROADMAP 12.6, written when the backends exist.
Development emulators are wired in the integration environment at M2 and filled in at M12
(ROADMAP 2.5, 12.2.4).

## Kubernetes and sharding

A cluster is a set of shards. Each shard is its own database under `<prefix>/shard-<n>/`.
Documents route by `hash(id) mod shard_count` using xxh3. The shard count is fixed when the
collection set is created; splitting shards is a post-v1 design that takes advantage of immutable
segments (SPEC §11.1, §18.5, ROADMAP 13.6).

Roles, all in `persia-server` (SPEC §11.2):

- **Writer** — holds one shard's lease, accepts writes, compacts, uploads segments.
- **Reader** — serves queries from committed manifests, holds no shard lease, scales horizontally.
- **Router** — scatters a query, merges top-k and facets. It may run in the same process.

The lease carries a fencing token. Every manifest embeds that token. A writer whose lease has
lapsed stops writing, and readers reject a manifest whose token went backwards (SPEC §11.3).
Readers poll for the next manifest (default 1 second, with jitter) and swap snapshots
atomically. Lag is `persia_replica_lag_seconds` (SPEC §11.4).

Cross-shard scoring uses each shard's own statistics by default. `global_stats: true` makes the
router collect statistics before scoring (SPEC §11.5).

The Helm chart planned in SPEC §11.6: a StatefulSet for writers (one pod per shard, a volume for
cache and WAL), a Deployment plus HPA for readers, a headless Service for discovery, pod disruption
budgets, readiness and liveness probes, and a ServiceMonitor. Workload-identity examples for AWS,
GCP, and Azure are part of that milestone (ROADMAP 13.5).

## SDKs

These are the **network** clients. Embedded Go and TypeScript, which open a file in-process, are a
different design: [Embedded SDKs](/guide/sdks/) (ADR-0018). Neither family is implemented.

Rust (`sdk/rust`, crate `persia-client`) and Go (`sdk/go`, module `persia-go`) sit on the same
protocol (SPEC §13.2). Each adds connection pooling, retries with backoff on idempotent calls,
deadlines, batching, blob streaming, and shard-aware writes. A shared, language-neutral conformance
suite runs against both SDKs and the server (ROADMAP 14.1). A compatibility matrix is part of the
SDK milestone (ROADMAP 14.4). Both clients are part of that milestone, after the protocol exists.
