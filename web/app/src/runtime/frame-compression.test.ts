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

  it("passes uncompressed messages through synchronously", () => {
    const received: unknown[] = [];
    const target = { handleTransportMessage: (data: unknown) => void received.push(data) };
    installFrameCompression(target);

    target.handleTransportMessage('{"type":"small"}');

    expect(received).toEqual(['{"type":"small"}']);
  });
});
