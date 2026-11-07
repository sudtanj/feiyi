const COMPRESSED_FRAME = 0xc0;

export const frameCompressionSupported = typeof DecompressionStream === "function";

type MessageSink = { handleTransportMessage(data: unknown): void };
const installed = new WeakSet<MessageSink>();

async function inflate(bytes: Uint8Array): Promise<string> {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return new Response(stream).text();
}

/**
 * The daemon sends large JSON as COMPRESSED_FRAME + raw deflate once the hello advertised
 * `frame_compression`. Inflating is async, so later frames queue behind a pending one.
 */
export function installFrameCompression(target: MessageSink): void {
  if (installed.has(target)) return;
  installed.add(target);
  const original = target.handleTransportMessage;
  let pending = 0;
  let tail: Promise<void> = Promise.resolve();
  target.handleTransportMessage = function (this: MessageSink, data: unknown) {
    const compressed =
      data instanceof ArrayBuffer && new Uint8Array(data)[0] === COMPRESSED_FRAME
        ? new Uint8Array(data, 1)
        : null;
    if (!compressed && pending === 0) {
      original.call(this, data);
      return;
    }
    pending += 1;
    tail = tail.then(async () => {
      try {
        original.call(this, compressed ? await inflate(compressed) : data);
      } catch {
        // A corrupt frame is dropped; the connection's own liveness checks recover.
      } finally {
        pending -= 1;
      }
    });
  };
}
