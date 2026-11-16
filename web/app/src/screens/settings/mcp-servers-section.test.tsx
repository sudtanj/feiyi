/**
 * @vitest-environment jsdom
 */
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { configState, patchConfig, confirmDialog } = vi.hoisted(() => ({
  configState: { config: null as unknown, isLoading: false, patchConfig: undefined as unknown },
  patchConfig: vi.fn(),
  confirmDialog: vi.fn(),
}));

type Children = { children?: React.ReactNode };

vi.mock("react-native", () => ({
  View: ({ children, testID }: Children & { testID?: string }) =>
    React.createElement("div", { "data-testid": testID }, children),
  Text: ({ children, testID }: Children & { testID?: string }) =>
    React.createElement("span", { "data-testid": testID }, children),
}));
vi.mock("react-native-unistyles", () => ({
  StyleSheet: { create: (factory: unknown) => (typeof factory === "function" ? {} : factory) },
}));
vi.mock("@/styles/settings", () => ({ settingsStyles: {} }));
vi.mock("@/components/settings/headings/settings-section", () => ({
  SettingsSection: ({ title, children }: Children & { title: string }) =>
    React.createElement("section", { "aria-label": title }, children),
}));
vi.mock("@/components/ui/button", () => ({
  Button: ({
    children,
    onPress,
    disabled,
    loading,
    testID,
  }: Children & {
    onPress?: () => void;
    disabled?: boolean;
    loading?: boolean;
    testID?: string;
  }) =>
    React.createElement(
      "button",
      { "data-testid": testID, disabled: disabled || loading, onClick: onPress },
      children,
    ),
}));
vi.mock("@/components/ui/switch", () => ({
  Switch: ({
    value,
    onValueChange,
    disabled,
    testID,
  }: {
    value: boolean;
    onValueChange: (value: boolean) => void;
    disabled?: boolean;
    testID?: string;
  }) =>
    React.createElement("input", {
      type: "checkbox",
      "data-testid": testID,
      checked: value,
      disabled,
      onChange: (event: React.ChangeEvent<HTMLInputElement>) =>
        onValueChange(event.target.checked),
    }),
}));
vi.mock("@/components/ui/form-field", () => ({
  Field: ({ children, trailing, hint }: Children & { trailing?: React.ReactNode; hint?: string }) =>
    React.createElement("div", null, trailing, children, hint),
  FormTextInput: ({
    initialValue,
    onChangeText,
    editable,
    placeholder,
    testID,
  }: {
    initialValue: string;
    onChangeText: (value: string) => void;
    editable?: boolean;
    placeholder?: string;
    testID?: string;
  }) =>
    React.createElement("input", {
      "data-testid": testID,
      defaultValue: initialValue,
      placeholder,
      disabled: editable === false,
      onChange: (event: React.ChangeEvent<HTMLInputElement>) => onChangeText(event.target.value),
    }),
}));
vi.mock("@/hooks/use-daemon-config", () => ({ useDaemonConfig: () => configState }));
vi.mock("@/utils/confirm-dialog", () => ({ confirmDialog }));

import { McpServersSection } from "./mcp-servers-section";

let root: Root | null = null;
let container: HTMLDivElement;

const alpha = {
  name: "alpha",
  url: "https://a.example/mcp",
  tokenSet: true,
  claude: true,
  codex: false,
};
const beta = {
  name: "beta",
  url: "http://b.example/mcp",
  tokenSet: false,
  claude: false,
  codex: true,
};

beforeEach(() => {
  patchConfig.mockReset().mockResolvedValue(undefined);
  confirmDialog.mockReset().mockResolvedValue(true);
  configState.patchConfig = patchConfig;
});

afterEach(() => {
  act(() => root?.unmount());
  root = null;
  document.body.innerHTML = "";
});

function render(config: unknown, isLoading = false) {
  configState.config = config;
  configState.isLoading = isLoading;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  act(() => root?.render(<McpServersSection serverId="s" />));
}

function byId<T extends HTMLElement>(id: string): T {
  const element = container.querySelector<T>(`[data-testid="${id}"]`);
  if (!element) throw new Error(`no element ${id}`);
  return element;
}

async function click(id: string) {
  await act(async () => {
    byId(id).click();
  });
}

function type(id: string, value: string) {
  const input = byId<HTMLInputElement>(id);
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
  act(() => {
    setter?.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

const withServers = (servers: unknown[]) => ({ mcp: { injectIntoAgents: true, servers } });

describe("McpServersSection", () => {
  it("lists servers from the config", () => {
    render(withServers([alpha, beta]));
    const text = container.textContent ?? "";
    expect(text).toContain("alpha");
    expect(text).toContain("https://a.example/mcp");
    expect(text).toContain("Token set");
    expect(text).toContain("No token");
    expect(byId<HTMLInputElement>("host-page-mcp-servers-alpha-claude").checked).toBe(true);
    expect(byId<HTMLInputElement>("host-page-mcp-servers-alpha-codex").checked).toBe(false);
  });

  it("shows the empty state", () => {
    render(withServers([]));
    expect(container.textContent).toContain("No MCP servers yet.");
  });

  it("says so when the daemon sends no mcp.servers", () => {
    render({ mcp: { injectIntoAgents: true } });
    expect(container.textContent).toContain("not available from this daemon");
  });

  it("adds a server for both providers", async () => {
    render(withServers([alpha]));
    await click("host-page-mcp-servers-add");
    type("host-page-mcp-servers-name-input", "gamma");
    type("host-page-mcp-servers-url-input", "https://g.example/mcp");
    type("host-page-mcp-servers-token-input", "secret");
    await click("host-page-mcp-servers-save");
    expect(patchConfig).toHaveBeenCalledWith({
      mcp: {
        servers: [
          { name: "alpha", url: alpha.url, claude: true, codex: false },
          {
            name: "gamma",
            url: "https://g.example/mcp",
            claude: true,
            codex: true,
            token: "secret",
          },
        ],
      },
    });
  });

  it("omits the token when editing without touching it", async () => {
    render(withServers([alpha, beta]));
    await click("host-page-mcp-servers-alpha-edit");
    await click("host-page-mcp-servers-form-codex");
    await click("host-page-mcp-servers-save");
    expect(patchConfig.mock.calls[0]?.[0]).toStrictEqual({
      mcp: {
        servers: [
          { name: "alpha", url: alpha.url, claude: true, codex: true },
          { name: "beta", url: beta.url, claude: false, codex: true },
        ],
      },
    });
  });

  it("sends a new token when editing with one typed", async () => {
    render(withServers([alpha]));
    await click("host-page-mcp-servers-alpha-edit");
    type("host-page-mcp-servers-url-input", "https://a2.example/mcp");
    type("host-page-mcp-servers-token-input", "new");
    await click("host-page-mcp-servers-save");
    expect(patchConfig.mock.calls[0]?.[0]).toStrictEqual({
      mcp: {
        servers: [
          { name: "alpha", url: "https://a2.example/mcp", claude: true, codex: false, token: "new" },
        ],
      },
    });
  });

  it("blocks a URL change that would send the stored token to a new host", async () => {
    render(withServers([alpha]));
    await click("host-page-mcp-servers-alpha-edit");
    type("host-page-mcp-servers-url-input", "https://a2.example/mcp");
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-form-error").textContent).toContain("Enter the token again");
    expect(patchConfig).not.toHaveBeenCalled();
  });

  it("sends the full list without tokens when a row switch is toggled", async () => {
    render(withServers([alpha, beta]));
    await click("host-page-mcp-servers-beta-claude");
    expect(patchConfig.mock.calls[0]?.[0]).toStrictEqual({
      mcp: {
        servers: [
          { name: "alpha", url: alpha.url, claude: true, codex: false },
          { name: "beta", url: beta.url, claude: true, codex: true },
        ],
      },
    });
  });

  it("disables Add at 32 servers", () => {
    const many = Array.from({ length: 32 }, (_, index) => ({ ...beta, name: `s${index}` }));
    render(withServers(many));
    expect(byId<HTMLButtonElement>("host-page-mcp-servers-add").disabled).toBe(true);
  });

  it("reports an edit whose server was removed elsewhere", async () => {
    render(withServers([alpha]));
    await click("host-page-mcp-servers-alpha-edit");
    configState.config = withServers([]);
    await act(async () => root?.render(<McpServersSection serverId="s" />));
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-error").textContent).toContain("was removed");
    expect(patchConfig).not.toHaveBeenCalled();
  });

  it("sends an empty token to clear it", async () => {
    render(withServers([alpha]));
    await click("host-page-mcp-servers-alpha-edit");
    await click("host-page-mcp-servers-clear-token");
    await click("host-page-mcp-servers-save");
    expect(patchConfig.mock.calls[0]?.[0]).toStrictEqual({
      mcp: { servers: [{ name: "alpha", url: alpha.url, claude: true, codex: false, token: "" }] },
    });
  });

  it("removes a server after confirming", async () => {
    render(withServers([alpha, beta]));
    await click("host-page-mcp-servers-alpha-remove");
    expect(patchConfig).toHaveBeenCalledWith({
      mcp: { servers: [{ name: "beta", url: beta.url, claude: false, codex: true }] },
    });
  });

  it("keeps the server when removal is declined", async () => {
    confirmDialog.mockResolvedValue(false);
    render(withServers([alpha]));
    await click("host-page-mcp-servers-alpha-remove");
    expect(patchConfig).not.toHaveBeenCalled();
  });

  it("rejects an invalid name without sending", async () => {
    render(withServers([]));
    await click("host-page-mcp-servers-add");
    type("host-page-mcp-servers-name-input", "bad name!");
    type("host-page-mcp-servers-url-input", "https://g.example/mcp");
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-form-error").textContent).toContain("Name must be");
    expect(patchConfig).not.toHaveBeenCalled();
  });

  it("rejects built-in names, duplicates and non-http URLs", async () => {
    render(withServers([alpha]));
    await click("host-page-mcp-servers-add");
    type("host-page-mcp-servers-url-input", "https://g.example/mcp");
    type("host-page-mcp-servers-name-input", "feiyi");
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-form-error").textContent).toContain("built-in");
    type("host-page-mcp-servers-name-input", "alpha");
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-form-error").textContent).toContain("already exists");
    type("host-page-mcp-servers-name-input", "gamma");
    type("host-page-mcp-servers-url-input", "ftp://g.example");
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-form-error").textContent).toContain("http://");
    expect(patchConfig).not.toHaveBeenCalled();
  });

  it("shows the daemon error and keeps the form open", async () => {
    patchConfig.mockRejectedValue(new Error("name already used"));
    render(withServers([]));
    await click("host-page-mcp-servers-add");
    type("host-page-mcp-servers-name-input", "gamma");
    type("host-page-mcp-servers-url-input", "https://g.example/mcp");
    await click("host-page-mcp-servers-save");
    expect(byId("host-page-mcp-servers-error").textContent).toBe("name already used");
    expect(container.querySelector('[data-testid="host-page-mcp-servers-form"]')).not.toBeNull();
  });
});
