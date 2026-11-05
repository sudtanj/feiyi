import { afterEach, describe, expect, it, vi } from "vitest";
import { hasNewBuild } from "@/utils/web-build-version";

function stubFetch(impl: () => Promise<Partial<Response>>) {
  vi.stubGlobal("fetch", vi.fn(impl));
}

describe("hasNewBuild", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("is true when the deployed id differs from the built id", async () => {
    stubFetch(async () => ({ ok: true, json: async () => ({ id: "2" }) }));
    expect(await hasNewBuild("1")).toBe(true);
  });

  it("is false when the ids match", async () => {
    stubFetch(async () => ({ ok: true, json: async () => ({ id: "1" }) }));
    expect(await hasNewBuild("1")).toBe(false);
  });

  it("is false when offline, on an error status or on a malformed body", async () => {
    stubFetch(async () => {
      throw new TypeError("Failed to fetch");
    });
    expect(await hasNewBuild("1")).toBe(false);
    stubFetch(async () => ({ ok: false }));
    expect(await hasNewBuild("1")).toBe(false);
    stubFetch(async () => ({
      ok: true,
      json: async () => {
        throw new SyntaxError("bad json");
      },
    }));
    expect(await hasNewBuild("1")).toBe(false);
    stubFetch(async () => ({ ok: true, json: async () => ({}) }));
    expect(await hasNewBuild("1")).toBe(false);
  });
});
