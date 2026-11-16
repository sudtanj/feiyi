import React, { useCallback, useState } from "react";
import { Text, View } from "react-native";
import { StyleSheet } from "react-native-unistyles";
import type { MutableDaemonConfigPatch } from "@getpaseo/protocol/messages";
import { SettingsSection } from "@/components/settings/headings/settings-section";
import { Button } from "@/components/ui/button";
import { Field, FormTextInput } from "@/components/ui/form-field";
import { Switch } from "@/components/ui/switch";
import { useDaemonConfig } from "@/hooks/use-daemon-config";
import { settingsStyles } from "@/styles/settings";
import { confirmDialog } from "@/utils/confirm-dialog";

export interface McpServer {
  name: string;
  url: string;
  tokenSet: boolean;
  claude: boolean;
  codex: boolean;
}

interface McpServerPatch {
  name: string;
  url: string;
  claude: boolean;
  codex: boolean;
  token?: string;
}

type Provider = "claude" | "codex";

const MAX_SERVERS = 32;
const NAME_PATTERN = /^[A-Za-z0-9_-]{1,64}$/;
const RESERVED_NAMES = ["zilong-router", "feiyi"];

/** `mcp.servers` from the daemon config; null on an older daemon that does not send it. */
export function parseMcpServers(config: unknown): McpServer[] | null {
  const servers = (config as { mcp?: { servers?: unknown } } | null)?.mcp?.servers;
  return Array.isArray(servers) ? (servers as McpServer[]) : null;
}

function validateServer(
  form: { name: string; url: string },
  servers: McpServer[],
  editingName: string | null,
): string | null {
  const name = form.name.trim();
  if (!NAME_PATTERN.test(name)) {
    return "Name must be 1-64 characters: letters, digits, - or _.";
  }
  if (RESERVED_NAMES.includes(name)) {
    return `"${name}" is a built-in server name.`;
  }
  if (name !== editingName && servers.some((server) => server.name === name)) {
    return `A server named "${name}" already exists.`;
  }
  if (editingName === null && servers.length >= MAX_SERVERS) {
    return `At most ${MAX_SERVERS} servers are allowed.`;
  }
  let protocol = "";
  try {
    protocol = new URL(form.url.trim()).protocol;
  } catch {
    // an unparseable URL gets the same message as a wrong scheme
  }
  if (protocol !== "http:" && protocol !== "https:") {
    return "URL must start with http:// or https://.";
  }
  return null;
}

function toPatch(server: McpServer): McpServerPatch {
  return { name: server.name, url: server.url, claude: server.claude, codex: server.codex };
}

interface ServerFormProps {
  editing: McpServer | null;
  saving: boolean;
  onSave: (server: McpServerPatch) => void;
  onCancel: () => void;
  validate: (form: { name: string; url: string }) => string | null;
}

function ServerForm({ editing, saving, onSave, onCancel, validate }: ServerFormProps) {
  const [name, setName] = useState(editing?.name ?? "");
  const [url, setUrl] = useState(editing?.url ?? "");
  const [token, setToken] = useState("");
  const [clearToken, setClearToken] = useState(false);
  const [claude, setClaude] = useState(editing?.claude ?? true);
  const [codex, setCodex] = useState(editing?.codex ?? true);
  const [error, setError] = useState<string | null>(null);

  const toggleClearToken = useCallback(() => {
    setClearToken((current) => !current);
    setToken("");
  }, []);

  const handleSave = useCallback(() => {
    const message = validate({ name, url });
    const tokenKept = !clearToken && token === "";
    const keptTokenToNewHost =
      !message && editing?.tokenSet === true && tokenKept && url.trim() !== editing.url;
    const blocked = keptTokenToNewHost
      ? "The URL changed. Enter the token again or clear it so the stored token is not sent to the new host."
      : message;
    setError(blocked);
    if (blocked) return;
    const patch: McpServerPatch = { name: name.trim(), url: url.trim(), claude, codex };
    if (clearToken) patch.token = "";
    else if (token !== "") patch.token = token;
    onSave(patch);
  }, [validate, name, url, claude, codex, clearToken, token, onSave]);

  return (
    <View style={[settingsStyles.rowBorder, styles.form]} testID="host-page-mcp-servers-form">
      <Field label="Name">
        <FormTextInput
          initialValue={name}
          onChangeText={setName}
          placeholder="my-server"
          autoCapitalize="none"
          autoCorrect={false}
          editable={!saving && editing === null}
          accessibilityLabel="MCP server name"
          testID="host-page-mcp-servers-name-input"
        />
      </Field>
      <Field label="URL">
        <FormTextInput
          initialValue={url}
          onChangeText={setUrl}
          placeholder="https://example.com/mcp"
          autoCapitalize="none"
          autoCorrect={false}
          keyboardType="url"
          editable={!saving}
          accessibilityLabel="MCP server URL"
          testID="host-page-mcp-servers-url-input"
        />
      </Field>
      <Field
        label="Token"
        hint={clearToken ? "The stored token will be removed when you save." : undefined}
        trailing={
          editing?.tokenSet ? (
            <Button
              size="sm"
              variant="ghost"
              onPress={toggleClearToken}
              disabled={saving}
              accessibilityLabel={clearToken ? "Keep stored token" : "Clear stored token"}
              testID="host-page-mcp-servers-clear-token"
            >
              {clearToken ? "Keep token" : "Clear token"}
            </Button>
          ) : undefined
        }
      >
        <FormTextInput
          key={clearToken ? "cleared" : "kept"}
          initialValue=""
          onChangeText={setToken}
          placeholder={editing?.tokenSet ? "unchanged" : "Optional bearer token"}
          secureTextEntry
          autoCapitalize="none"
          autoCorrect={false}
          editable={!saving && !clearToken}
          accessibilityLabel="MCP server token"
          testID="host-page-mcp-servers-token-input"
        />
      </Field>
      <View style={styles.switchRow}>
        <Text style={settingsStyles.rowTitle}>Claude Code</Text>
        <Switch
          value={claude}
          onValueChange={setClaude}
          disabled={saving}
          accessibilityLabel="Inject into Claude Code"
          testID="host-page-mcp-servers-form-claude"
        />
        <Text style={settingsStyles.rowTitle}>Codex</Text>
        <Switch
          value={codex}
          onValueChange={setCodex}
          disabled={saving}
          accessibilityLabel="Inject into Codex"
          testID="host-page-mcp-servers-form-codex"
        />
      </View>
      {error ? (
        <Text style={settingsStyles.rowError} testID="host-page-mcp-servers-form-error">
          {error}
        </Text>
      ) : null}
      <View style={styles.actions}>
        <Button
          variant="default"
          onPress={handleSave}
          disabled={saving}
          loading={saving}
          accessibilityLabel="Save MCP server"
          testID="host-page-mcp-servers-save"
        >
          Save
        </Button>
        <Button
          onPress={onCancel}
          disabled={saving}
          accessibilityLabel="Cancel"
          testID="host-page-mcp-servers-cancel"
        >
          Cancel
        </Button>
      </View>
    </View>
  );
}

interface ServerRowProps {
  server: McpServer;
  first: boolean;
  disabled: boolean;
  onToggle: (server: McpServer, provider: Provider, value: boolean) => void;
  onEdit: (server: McpServer) => void;
  onRemove: (server: McpServer) => void;
}

function ServerRow({ server, first, disabled, onToggle, onEdit, onRemove }: ServerRowProps) {
  const id = `host-page-mcp-servers-${server.name}`;
  const toggleClaude = useCallback(
    (value: boolean) => onToggle(server, "claude", value),
    [onToggle, server],
  );
  const toggleCodex = useCallback(
    (value: boolean) => onToggle(server, "codex", value),
    [onToggle, server],
  );
  const edit = useCallback(() => onEdit(server), [onEdit, server]);
  const remove = useCallback(() => onRemove(server), [onRemove, server]);
  return (
    <View style={[styles.serverRow, first ? null : settingsStyles.rowBorder]} testID={id}>
      <View>
        <Text style={settingsStyles.rowTitle}>{server.name}</Text>
        <Text style={settingsStyles.rowHint}>{server.url}</Text>
        <Text style={settingsStyles.rowHint}>{server.tokenSet ? "Token set" : "No token"}</Text>
      </View>
      <View style={styles.switchRow}>
        <Text style={settingsStyles.rowTitle}>Claude Code</Text>
        <Switch
          value={server.claude}
          onValueChange={toggleClaude}
          disabled={disabled}
          accessibilityLabel={`Inject ${server.name} into Claude Code`}
          testID={`${id}-claude`}
        />
        <Text style={settingsStyles.rowTitle}>Codex</Text>
        <Switch
          value={server.codex}
          onValueChange={toggleCodex}
          disabled={disabled}
          accessibilityLabel={`Inject ${server.name} into Codex`}
          testID={`${id}-codex`}
        />
      </View>
      <View style={styles.actions}>
        <Button
          size="sm"
          onPress={edit}
          disabled={disabled}
          accessibilityLabel={`Edit ${server.name}`}
          testID={`${id}-edit`}
        >
          Edit
        </Button>
        <Button
          size="sm"
          variant="destructive"
          onPress={remove}
          disabled={disabled}
          accessibilityLabel={`Remove ${server.name}`}
          testID={`${id}-remove`}
        >
          Remove
        </Button>
      </View>
    </View>
  );
}

type FormState = { mode: "closed" } | { mode: "add" } | { mode: "edit"; server: McpServer };

export function McpServersSection({ serverId }: { serverId: string }) {
  const { config, isLoading, patchConfig } = useDaemonConfig(serverId);
  const servers = parseMcpServers(config);
  const [form, setForm] = useState<FormState>({ mode: "closed" });
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const replaceList = useCallback(
    async (next: McpServerPatch[]): Promise<boolean> => {
      setSaving(true);
      setError(null);
      try {
        // The protocol package may predate `mcp.servers`, so its patch type does not list it.
        await patchConfig({ mcp: { servers: next } } as unknown as MutableDaemonConfigPatch);
        return true;
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
        return false;
      } finally {
        setSaving(false);
      }
    },
    [patchConfig],
  );

  const handleSave = useCallback(
    async (entry: McpServerPatch) => {
      const current = servers ?? [];
      if (form.mode === "edit" && !current.some((server) => server.name === form.server.name)) {
        setError(`"${form.server.name}" was removed from the daemon. Cancel and add it again.`);
        return;
      }
      const next =
        form.mode === "edit"
          ? current.map((server) => (server.name === form.server.name ? entry : toPatch(server)))
          : [...current.map(toPatch), entry];
      if (await replaceList(next)) setForm({ mode: "closed" });
    },
    [form, servers, replaceList],
  );

  const handleToggle = useCallback(
    (target: McpServer, provider: Provider, value: boolean) => {
      void replaceList(
        (servers ?? []).map((server) =>
          server.name === target.name ? { ...toPatch(server), [provider]: value } : toPatch(server),
        ),
      );
    },
    [servers, replaceList],
  );

  const handleRemove = useCallback(
    async (target: McpServer) => {
      const confirmed = await confirmDialog({
        title: `Remove ${target.name}?`,
        message: "Its stored token is deleted too. New agent processes stop getting this server.",
        confirmLabel: "Remove",
        cancelLabel: "Cancel",
        destructive: true,
      });
      if (!confirmed) return;
      await replaceList(
        (servers ?? []).filter((server) => server.name !== target.name).map(toPatch),
      );
    },
    [servers, replaceList],
  );

  const validate = useCallback(
    (values: { name: string; url: string }) =>
      validateServer(values, servers ?? [], form.mode === "edit" ? form.server.name : null),
    [servers, form],
  );

  const openAdd = useCallback(() => {
    setError(null);
    setForm({ mode: "add" });
  }, []);
  const openEdit = useCallback((server: McpServer) => {
    setError(null);
    setForm({ mode: "edit", server });
  }, []);
  const closeForm = useCallback(() => {
    setError(null);
    setForm({ mode: "closed" });
  }, []);
  const submit = useCallback((entry: McpServerPatch) => void handleSave(entry), [handleSave]);
  const remove = useCallback((server: McpServer) => void handleRemove(server), [handleRemove]);

  let body;
  if (!servers) {
    body = (
      <View style={settingsStyles.row}>
        <Text style={settingsStyles.rowHint} testID="host-page-mcp-servers-unavailable">
          {isLoading ? "Loading..." : "MCP servers are not available from this daemon."}
        </Text>
      </View>
    );
  } else {
    body = (
      <>
        {servers.length === 0 && form.mode === "closed" ? (
          <View style={settingsStyles.row}>
            <Text style={settingsStyles.rowHint} testID="host-page-mcp-servers-empty">
              No MCP servers yet. Add one to give every agent its tools.
            </Text>
          </View>
        ) : null}
        {servers.map((server, index) => (
          <ServerRow
            key={server.name}
            server={server}
            first={index === 0}
            disabled={saving || form.mode !== "closed"}
            onToggle={handleToggle}
            onEdit={openEdit}
            onRemove={remove}
          />
        ))}
        {form.mode === "closed" ? null : (
          <ServerForm
            key={form.mode === "edit" ? form.server.name : "add"}
            editing={form.mode === "edit" ? form.server : null}
            saving={saving}
            onSave={submit}
            onCancel={closeForm}
            validate={validate}
          />
        )}
        {error ? (
          <View style={[settingsStyles.row, settingsStyles.rowBorder]}>
            <Text style={settingsStyles.rowError} testID="host-page-mcp-servers-error">
              {error}
            </Text>
          </View>
        ) : null}
        <View style={[settingsStyles.row, settingsStyles.rowBorder]}>
          <View style={settingsStyles.rowContent}>
            <Text style={settingsStyles.rowHint}>
              The built-in zilong-router and feiyi servers are not listed. Changes reach new agent
              processes, and Codex when its app-server next starts.
            </Text>
          </View>
          <Button
            size="sm"
            onPress={openAdd}
            disabled={saving || form.mode !== "closed" || servers.length >= MAX_SERVERS}
            accessibilityLabel="Add MCP server"
            testID="host-page-mcp-servers-add"
          >
            Add server
          </Button>
        </View>
      </>
    );
  }

  return (
    <SettingsSection title="MCP servers" testID="host-page-mcp-servers-section">
      <View style={settingsStyles.card}>{body}</View>
    </SettingsSection>
  );
}

const styles = StyleSheet.create((theme) => ({
  serverRow: {
    padding: theme.spacing[4],
    gap: theme.spacing[3],
  },
  switchRow: {
    flexDirection: "row",
    flexWrap: "wrap",
    alignItems: "center",
    gap: theme.spacing[3],
  },
  actions: {
    flexDirection: "row",
    gap: theme.spacing[2],
  },
  form: {
    padding: theme.spacing[4],
    gap: theme.spacing[3],
  },
}));
