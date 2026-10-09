# Security policy

Persia DB parses bytes from disk, network and users, and stores data people care about.
We treat memory-safety bugs, panics on hostile input, data loss and data corruption as security issues.

## Supported versions

Persia DB is pre-alpha. Only the latest commit on `main` receives fixes. This table will grow with releases.

| Version | Supported |
|---|---|
| `main` | ✅ |

## Reporting a vulnerability

**Please do not open a public issue.** Instead, use one of:

- GitHub private reporting: <https://github.com/tomiok/persiaDB/security/advisories/new>
- Email: <tomaslingotti@gmail.com>, with subject starting `[persia-security]`

Include the affected version or commit, a minimal reproduction (an input file or a sequence of operations),
and the impact you observed (panic, wrong results, data loss, corruption, resource exhaustion).

We aim to acknowledge within 3 working days and to agree on a fix and disclosure timeline with you.
Reporters are credited in the advisory unless they ask not to be.

## In scope

- Panics, undefined behavior, or unbounded memory/CPU use when opening or reading a crafted database,
  segment, WAL, blob pack, or network request (SPEC §17).
- Writes acknowledged under `Fsync` durability (the default at commit, SPEC §8.2) lost, or committed data
  corrupted, after a crash (SPEC §4.5, §8.6). Loss of un-synced writes under `Buffered`/`Async` is by design.
- Path traversal through collection or blob names (SPEC §17).
- Authentication or authorization bypass in `persia-server`.
- Secrets leaking into logs or debug output.

## Out of scope

- Attacks that need write access to the database files themselves, other than crafted-input parsing bugs.
- Denial of service through query volume alone (use rate limiting in front of the server).
- Encryption at rest: not provided by the format in v1 (SPEC §1.2); use storage-level encryption.
