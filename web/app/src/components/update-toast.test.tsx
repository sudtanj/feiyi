/**
 * @vitest-environment jsdom
 */
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  (globalThis as unknown as { __DEV__: boolean }).__DEV__ = false;
});

const { hasNewBuild, platform } = vi.hoisted(() => ({
  hasNewBuild: vi.fn<(builtId: string) => Promise<boolean>>(),
  platform: { isWeb: true },
}));

vi.mock("react-native-unistyles", () => {
  const theme = {
    spacing: { 1: 4, 3: 12, 4: 16 },
    fontSize: { sm: 13, base: 15 },
    fontWeight: { semibold: "600" },
    borderRadius: { md: 6, lg: 8 },
    borderWidth: { 1: 1 },
    shadow: { md: {} },
    colors: {
      surface2: "#111",
      foreground: "#fff",
      foregroundMuted: "#aaa",
      border: "#555",
      primary: "#00f",
      primaryForeground: "#fff",
    },
  };
  return {
    StyleSheet: {
      create: (factory: unknown) => (typeof factory === "function" ? factory(theme) : factory),
    },
    useUnistyles: () => ({ theme }),
  };
});

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("lucide-react-native", () => ({
  X: () => null,
}));

vi.mock("react-native", async () => {
  const ReactModule = await import("react");
  return {
    View: ({ children }: { children?: React.ReactNode }) =>
      ReactModule.createElement("div", null, children),
    Text: ({ children }: { children?: React.ReactNode }) =>
      ReactModule.createElement("span", null, children),
    Pressable: ({
      children,
      onPress,
      accessibilityLabel,
    }: {
      children?: React.ReactNode;
      onPress?: () => void;
      accessibilityLabel?: string;
    }) =>
      ReactModule.createElement(
        "button",
        { onClick: onPress, "aria-label": accessibilityLabel },
        children,
      ),
  };
});

vi.mock("@/constants/platform", () => ({
  get isWeb() {
    return platform.isWeb;
  },
  isNative: false,
}));

vi.mock("@/utils/web-build-version", () => ({
  BUILT_ID: "1",
  hasNewBuild,
}));

const CHECK_INTERVAL_MS = 15 * 60 * 1000;

let container: HTMLElement;
let root: Root;

async function mountToast() {
  const { UpdateToast } = await import("./update-toast");
  await act(async () => {
    root.render(<UpdateToast />);
  });
}

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

async function setVisibility(state: "visible" | "hidden") {
  Object.defineProperty(document, "visibilityState", { value: state, configurable: true });
  await act(async () => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

describe("UpdateToast", () => {
  beforeEach(() => {
    (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
      true;
    vi.useFakeTimers();
    platform.isWeb = true;
    hasNewBuild.mockReset();
    hasNewBuild.mockResolvedValue(false);
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("shows nothing until a check finds a newer build", async () => {
    await mountToast();
    expect(container.textContent).toBe("");
    expect(hasNewBuild).not.toHaveBeenCalled();

    await advance(CHECK_INTERVAL_MS);
    expect(hasNewBuild).toHaveBeenCalledWith("1");
    expect(container.textContent).toBe("");
  });

  it("shows the notice when the interval check finds a newer build", async () => {
    hasNewBuild.mockResolvedValue(true);
    await mountToast();
    await advance(CHECK_INTERVAL_MS - 1);
    expect(hasNewBuild).not.toHaveBeenCalled();

    await advance(1);
    expect(container.textContent).toContain("desktop.updates.callout.availableTitle");
  });

  it("checks when the page becomes visible and skips while hidden", async () => {
    hasNewBuild.mockResolvedValue(true);
    await mountToast();

    await setVisibility("hidden");
    expect(hasNewBuild).not.toHaveBeenCalled();
    await advance(CHECK_INTERVAL_MS);
    expect(hasNewBuild).not.toHaveBeenCalled();

    await setVisibility("visible");
    expect(hasNewBuild).toHaveBeenCalledTimes(1);
    expect(container.textContent).toContain("desktop.updates.callout.availableTitle");
  });

  it("does not check off the web", async () => {
    platform.isWeb = false;
    hasNewBuild.mockResolvedValue(true);
    await mountToast();
    await setVisibility("visible");
    await advance(CHECK_INTERVAL_MS);
    expect(hasNewBuild).not.toHaveBeenCalled();
    expect(container.textContent).toBe("");
  });

  it("does not check when the bundle has no build id", async () => {
    vi.resetModules();
    vi.doMock("@/utils/web-build-version", () => ({ BUILT_ID: "", hasNewBuild }));
    hasNewBuild.mockResolvedValue(true);
    await mountToast();
    await setVisibility("visible");
    await advance(CHECK_INTERVAL_MS);
    expect(hasNewBuild).not.toHaveBeenCalled();
    expect(container.textContent).toBe("");
    vi.doUnmock("@/utils/web-build-version");
  });

  it("reloads the page on Update", async () => {
    const reload = vi.fn();
    vi.stubGlobal("location", { reload });
    hasNewBuild.mockResolvedValue(true);
    await mountToast();
    await advance(CHECK_INTERVAL_MS);

    const update = Array.from(container.querySelectorAll("button")).find((button) =>
      button.textContent?.includes("settings.about.updates.update"),
    );
    await act(async () => {
      update?.click();
    });
    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("hides the notice on dismiss and keeps it hidden after later checks", async () => {
    hasNewBuild.mockResolvedValue(true);
    await mountToast();
    await advance(CHECK_INTERVAL_MS);

    const dismiss = container.querySelector<HTMLButtonElement>("button[aria-label]");
    expect(dismiss?.getAttribute("aria-label")).toBe("common.actions.dismiss");
    await act(async () => {
      dismiss?.click();
    });
    expect(container.textContent).toBe("");

    await advance(CHECK_INTERVAL_MS);
    expect(hasNewBuild).toHaveBeenCalledTimes(2);
    expect(container.textContent).toBe("");
  });

  it("removes the listener and interval on unmount", async () => {
    const remove = vi.spyOn(document, "removeEventListener");
    await mountToast();
    expect(vi.getTimerCount()).toBe(1);

    act(() => root.unmount());
    expect(remove).toHaveBeenCalledWith("visibilitychange", expect.any(Function));
    expect(vi.getTimerCount()).toBe(0);

    await setVisibility("visible");
    await advance(CHECK_INTERVAL_MS);
    expect(hasNewBuild).not.toHaveBeenCalled();
    root = createRoot(container);
  });
});
