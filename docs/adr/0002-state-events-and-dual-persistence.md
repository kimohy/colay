# ADR 0002: Transactional state plus append-only audit replica

Status: Accepted

SQLite is the authoritative projection and event outbox. Each task mutation validates the aggregate revision and writes its event in the same database transaction. A single flusher appends events in global order to `events.jsonl`, flushes and fsyncs, then marks them exported.

This ordering makes crashes recoverable despite SQLite and a file not sharing a transaction. The JSONL hash chain is an integrity and ordering control, not a signature. Provider transcripts remain bounded redacted artifacts referenced by events rather than embedded payloads.

