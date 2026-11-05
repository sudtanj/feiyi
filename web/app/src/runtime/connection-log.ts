import type { DaemonClient, Logger } from "@getpaseo/client/internal/daemon-client";

// Connection events kept in localStorage so they survive the app being killed, and sent to the
// daemon log (prefix "pwa") once a client is connected again. Web only.
const KEY = "feiyi.connectionLog";
const MAX_LINES = 200;
const MAX_LINE = 300;

function storage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

function read(store: Storage): string[] {
  try {
    const lines: unknown = JSON.parse(store.getItem(KEY) ?? "[]");
    return Array.isArray(lines) ? lines.filter((line): line is string => typeof line === "string") : [];
  } catch {
    return [];
  }
}

export function logConnection(event: string, detail?: object): void {
  const store = storage();
  if (!store) return;
  let text = event;
  if (detail) {
    try {
      const json = JSON.stringify(detail, (_key, value) =>
        value instanceof Error ? value.message : value,
      );
      text += ` ${json}`;
    } catch {
      /* Unserialisable detail; the event name is enough. */
    }
  }
  const offline = typeof navigator !== "undefined" && navigator.onLine === false ? "offline " : "";
  const line = `${new Date().toISOString()} ${offline}${text}`.slice(0, MAX_LINE);
  try {
    store.setItem(KEY, JSON.stringify([...read(store), line].slice(-MAX_LINES)));
  } catch {
    /* Storage full or blocked: logging must never break the connection code. */
  }
}

// The client's own state transitions arrive at debug level; its info level is periodic metrics.
export const connectionLogger: Logger = {
  debug: (obj, msg) => logConnection(`client ${msg ?? ""}`, obj),
  info: () => {},
  warn: (obj, msg) => logConnection(`client warn ${msg ?? ""}`, obj),
  error: (obj, msg) => logConnection(`client error ${msg ?? ""}`, obj),
};

// ponytail: uses the client's private sendJsonMessage, because the public send path validates
// against the upstream protocol schema, which has no log message. Recheck on an upstream bump.
export function flushConnectionLog(client: DaemonClient): void {
  const store = storage();
  if (!store) return;
  const lines = read(store);
  if (lines.length === 0) return;
  const send = (client as unknown as { sendJsonMessage?: (...args: unknown[]) => void })
    .sendJsonMessage;
  if (typeof send !== "function") return;
  try {
    send.call(client, "session", "feiyi.client_log", {
      type: "session",
      message: { type: "feiyi.client_log", lines },
    });
    store.removeItem(KEY);
  } catch {
    /* Not connected after all; the lines stay for the next connection. */
  }
}
