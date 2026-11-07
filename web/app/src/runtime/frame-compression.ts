const COMPRESSED_FRAME = 0xc0;

export const frameCompressionSupported = typeof DecompressionStream === "function";

type MessageSink = { handleTransportMessage(data: unknown): void };
type Queue = { pending: number; tail: Promise<void> };
const installed = new WeakSet<MessageSink>();
const queues = new WeakMap<object, Queue>();

async function inflate(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return new Response(stream).text();
}

/**
 * The daemon sends large JSON as COMPRESSED_FRAME + raw deflate once the hello advertised
 * `frame_compression`. Inflating is async, so later frames of the same client queue behind a
 * pending one. A queued frame is dropped if the client's transport changed while it waited,
 * like the client's own `this.transport === transport` check does for live frames.
 */
export function installFrameCompression(target: MessageSink): void {
  if (installed.has(target)) return;
  installed.add(target);
  const original = target.handleTransportMessage;
  target.handleTransportMessage = function (this: MessageSink, data: unknown) {
    let queue = queues.get(this);
    if (!queue) {
      queue = { pending: 0, tail: Promise.resolve() };
      queues.set(this, queue);
    }
    const q = queue;
    const compressed =
      data instanceof ArrayBuffer && new Uint8Array(data)[0] === COMPRESSED_FRAME
        ? new Uint8Array(data, 1)
        : null;
    if (!compressed && q.pending === 0) {
      original.call(this, data);
      return;
    }
    const transport = (this as { transport?: unknown }).transport;
    q.pending += 1;
    q.tail = q.tail.then(async () => {
      try {
        const message = compressed ? await inflate(compressed) : data;
        if ((this as { transport?: unknown }).transport !== transport) return;
        original.call(this, message);
      } catch {
        // A corrupt frame is dropped; the connection's own liveness checks recover.
      } finally {
        q.pending -= 1;
      }
    });
  };
}
