/**
 * @vitest-environment jsdom
 */
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

const { configState } = vi.hoisted(() => ({
  configState: { config: null as unknown, isLoading: false },
}));

vi.mock("react-native", () => ({
  View: ({ children }: { children?: React.ReactNode }) => React.createElement("div", null, children),
  Text: ({ children }: { children?: React.ReactNode }) =>
    React.createElement("span", null, children),
}));
vi.mock("react-native-unistyles", () => ({
  StyleSheet: { create: (factory: unknown) => (typeof factory === "function" ? {} : factory) },
}));
vi.mock("@/styles/settings", () => ({ settingsStyles: {} }));
vi.mock("@/components/settings/headings/settings-section", () => ({
  SettingsSection: ({ title, children }: { title: string; children?: React.ReactNode }) =>
    React.createElement("section", { "aria-label": title }, children),
}));
vi.mock("@/components/ui/status-badge", () => ({
  StatusBadge: ({ label }: { label: string }) => React.createElement("b", null, label),
}));
vi.mock("@/hooks/use-time-ago", () => ({ useTimeAgo: () => "5m ago" }));
vi.mock("@/hooks/use-daemon-config", () => ({ useDaemonConfig: () => configState }));

import { BackupSection, parseBackupStatus } from "./backup-section";

let root: Root | null = null;

afterEach(() => {
  act(() => root?.unmount());
  root = null;
  document.body.innerHTML = "";
});

function render(config: unknown, isLoading = false): string {
  configState.config = config;
  configState.isLoading = isLoading;
  const container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  act(() => root?.render(<BackupSection serverId="s" />));
  return container.textContent ?? "";
}

const base = { enabled: true, intervalSecs: 900, lastSuccessAt: null, lastError: null, rowsPushed: 0 };

describe("BackupSection", () => {
  it("shows a loading note until the config arrives", () => {
    expect(render(null, true)).toContain("Loading...");
  });

  it("says so when an older daemon sends no backup status", () => {
    expect(render({ providers: {} })).toContain("not available from this daemon");
  });

  it("explains the Off state with the variables to set", () => {
    const text = render({ backupStatus: { ...base, enabled: false } });
    expect(text).toContain("Off");
    expect(text).toContain("FEIYI_BACKUP_TURSO_URL");
    expect(text).toContain("FEIYI_BACKUP_PASSPHRASE");
  });

  it("shows OK with the relative last success, rows and interval", () => {
    const text = render({
      backupStatus: { ...base, lastSuccessAt: "2026-11-08T10:00:00.000Z", rowsPushed: 42 },
    });
    expect(text).toContain("OK");
    expect(text).toContain("Last success 5m ago");
    expect(text).toContain("42");
    expect(text).toContain("15 min");
  });

  it("shows Failing with the error text", () => {
    const text = render({ backupStatus: { ...base, lastError: "turso returned 401" } });
    expect(text).toContain("Failing");
    expect(text).toContain("turso returned 401");
  });
});

describe("parseBackupStatus", () => {
  it("returns null for a missing or malformed field", () => {
    expect(parseBackupStatus(null)).toBeNull();
    expect(parseBackupStatus({ backupStatus: { enabled: "yes" } })).toBeNull();
  });
});
