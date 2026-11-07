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

  it("keeps the queue per client instance sharing a prototype", async () => {
    class Client {
      received: unknown[] = [];
      handleTransportMessage(data: unknown) {
        this.received.push(data);
      }
    }
    installFrameCompression(Client.prototype);
    const a = new Client();
    const b = new Client();

    a.handleTransportMessage(frame('{"from":"a"}'));
    b.handleTransportMessage('{"from":"b"}');

    expect(b.received).toEqual(['{"from":"b"}']);
    expect(a.received).toEqual([]);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(a.received).toEqual(['{"from":"a"}']);
  });

  it("drops queued frames when the transport changed while they waited", async () => {
    class Client {
      transport: unknown = "old";
      received: unknown[] = [];
      handleTransportMessage(data: unknown) {
        this.received.push(data);
      }
    }
    installFrameCompression(Client.prototype);
    const client = new Client();

    client.handleTransportMessage(frame('{"n":1}'));
    client.handleTransportMessage('{"n":2}');
    client.transport = "new";
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(client.received).toEqual([]);

    client.handleTransportMessage(frame('{"n":3}'));
    client.handleTransportMessage('{"n":4}');
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(client.received).toEqual(['{"n":3}', '{"n":4}']);
  });

  it("passes uncompressed messages through synchronously", () => {
    const received: unknown[] = [];
    const target = { handleTransportMessage: (data: unknown) => void received.push(data) };
    installFrameCompression(target);

    target.handleTransportMessage('{"type":"small"}');

    expect(received).toEqual(['{"type":"small"}']);
  });
});
