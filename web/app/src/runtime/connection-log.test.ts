import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DaemonClient } from "@getpaseo/client/internal/daemon-client";
import { flushConnectionLog, logConnection } from "./connection-log";

function memoryStorage(): Storage {
  const items = new Map<string, string>();
  return {
    get length() {
      return items.size;
    },
    clear: () => items.clear(),
    getItem: (key) => items.get(key) ?? null,
    key: (index) => [...items.keys()][index] ?? null,
    removeItem: (key) => void items.delete(key),
    setItem: (key, value) => void items.set(key, value),
  };
}

describe("connection log", () => {
  beforeEach(() => vi.stubGlobal("localStorage", memoryStorage()));
  afterEach(() => vi.unstubAllGlobals());

  it("sends the kept lines to the daemon once and keeps only the newest 200", () => {
    for (let i = 0; i < 205; i++) logConnection(`event ${i}`, { err: new Error("boom") });
    const sent: unknown[][] = [];
    const client = { sendJsonMessage: (...args: unknown[]) => sent.push(args) };

    flushConnectionLog(client as unknown as DaemonClient);
    flushConnectionLog(client as unknown as DaemonClient);

    expect(sent).toHaveLength(1);
    const [envelope, type, message] = sent[0] as [string, string, { message: { lines: string[] } }];
    expect([envelope, type]).toEqual(["session", "feiyi.client_log"]);
    const lines = message.message.lines;
    expect(lines).toHaveLength(200);
    expect(lines[0]).toContain('event 5 {"err":"boom"}');
  });

  it("keeps the lines when the client cannot send", () => {
    logConnection("foreground");
    const client = {
      sendJsonMessage: () => {
        throw new Error("Transport not connected");
      },
    };
    flushConnectionLog(client as unknown as DaemonClient);
    expect(localStorage.getItem("feiyi.connectionLog")).toContain("foreground");
  });
});
