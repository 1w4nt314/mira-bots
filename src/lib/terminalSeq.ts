// Pure helpers for the terminal output stream (`agent-output` chunks and snapshots).

/** Base64 -> raw bytes (xterm treats a Uint8Array as UTF-8, so no text decoding here). */
export function decodeBase64(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export type ChunkPlan = { kind: "skip" } | { kind: "write"; from: number } | { kind: "resync" };

/**
 * What to do with a chunk, given the byte counter `lastSeq` already written to the terminal.
 * A chunk covers the bytes `[seqAfter - len, seqAfter)` of the agent's output stream.
 *
 * | case                                  | condition                          | plan               |
 * |---------------------------------------|------------------------------------|--------------------|
 * | duplicate (already in the snapshot)   | seqAfter <= lastSeq                | skip               |
 * | exactly next                          | start === lastSeq                  | write(from = 0)    |
 * | overlap (partly in the snapshot)      | start < lastSeq < seqAfter         | write(lastSeq - start) |
 * | gap (bytes missing)                   | start > lastSeq                    | resync (snapshot)  |
 *
 * `from` is the offset into the chunk's bytes where writing starts.
 */
export function planChunk(lastSeq: number, seqAfter: number, len: number): ChunkPlan {
  const start = seqAfter - len;
  if (seqAfter <= lastSeq) return { kind: "skip" };
  if (start === lastSeq) return { kind: "write", from: 0 };
  if (start < lastSeq) return { kind: "write", from: lastSeq - start };
  return { kind: "resync" };
}
