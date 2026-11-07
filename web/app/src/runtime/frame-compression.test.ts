import { deflateRawSync } from "node:zlib";
import { describe, expect, it } from "vitest";
import { installFrameCompression } from "./frame-compression";

function frame(text: string): ArrayBuffer {
  const body = deflateRawSync(Buffer.from(text));
  const bytes = new Uint8Array(body.length + 1);
  bytes[0] = 0xc0;
  bytes.set(body, 1);
  return bytes.buffer;
}

describe("frame compression", () => {
  it("inflates compressed frames and keeps order with frames that follow", async () => {
    const received: unknown[] = [];
    const target = { handleTransportMessage: (data: unknown) => void received.push(data) };
    installFrameCompression(target);
    const terminal = new Uint8Array([0x01, 0x00, 0x41]).buffer;

    target.handleTransportMessage(frame('{"type":"big"}'));
    target.handleTransportMessage('{"type":"small"}');
    target.handleTransportMessage(terminal);
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(received).toEqual(['{"type":"big"}', '{"type":"small"}', terminal]);
  });

  it("keeps order across interleaved CompressionStream frames and survives a corrupt one", async () => {
    async function daemonFrame(text: string): Promise<ArrayBuffer> {
      const body = await new Response(
        new Blob([text]).stream().pipeThrough(new CompressionStream("deflate-raw")),
      ).arrayBuffer();
      const bytes = new Uint8Array(body.byteLength + 1);
      bytes[0] = 0xc0;
      bytes.set(new Uint8Array(body), 1);
      return bytes.buffer;
    }
    const received: unknown[] = [];
    const target = { handleTransportMessage: (data: unknown) => void received.push(data) };
    installFrameCompression(target);
    const corrupt = new Uint8Array([0xc0, 0xff, 0xff, 0xff, 0xff]).buffer;
    const big = "x".repeat(50_000);

    target.handleTransportMessage(await daemonFrame(`{"n":1,"pad":"${big}"}`));
    target.handleTransportMessage('{"n":2}');
    target.handleTransportMessage(corrupt);
    target.handleTransportMessage(await daemonFrame('{"n":4}'));
    target.handleTransportMessage('{"n":5}');
    await new Promise((resolve) => setTimeout(resolve, 200));

    expect(received).toEqual([`{"n":1,"pad":"${big}"}`, '{"n":2}', '{"n":4}', '{"n":5}']);

    target.handleTransportMessage('{"n":6}');
    expect(received.at(-1)).toBe('{"n":6}');
  });

  it("passes uncompressed messages through synchronously", () => {
    const received: unknown[] = [];
    const target = { handleTransportMessage: (data: unknown) => void received.push(data) };
    installFrameCompression(target);

    target.handleTransportMessage('{"type":"small"}');

    expect(received).toEqual(['{"type":"small"}']);
  });
});
