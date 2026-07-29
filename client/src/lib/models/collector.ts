// The client domain model — see docs/architecture/client-model-refactor.md.

/**
 * Accumulate streamed rows until a terminal event flushes them. Replaces the
 * ad-hoc `xBuf` arrays that pair with a streamed response (roles, grants, pins,
 * search hits, threads, …): push each row as it arrives, then `flush()` the
 * batch when the terminator lands. Wired in during Phase 5 (the reducer).
 */
export class Collector<T> {
  private buf: T[] = [];

  push(row: T): void {
    this.buf.push(row);
  }

  /** Return everything collected so far and reset for the next batch. */
  flush(): T[] {
    const out = this.buf;
    this.buf = [];
    return out;
  }

  get size(): number {
    return this.buf.length;
  }
}
