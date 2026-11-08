import { Text, View } from "react-native";
import { StyleSheet } from "react-native-unistyles";
import { SettingsSection } from "@/components/settings/headings/settings-section";
import { StatusBadge } from "@/components/ui/status-badge";
import { useDaemonConfig } from "@/hooks/use-daemon-config";
import { useTimeAgo } from "@/hooks/use-time-ago";
import { settingsStyles } from "@/styles/settings";

export interface BackupStatus {
  enabled: boolean;
  intervalSecs: number;
  lastSuccessAt: string | null;
  lastError: string | null;
  rowsPushed: number;
}

/** Read-only `backupStatus` the daemon adds to its config; absent on older daemons. */
export function parseBackupStatus(config: unknown): BackupStatus | null {
  const raw = (config as { backupStatus?: Record<string, unknown> } | null)?.backupStatus;
  if (!raw || typeof raw.enabled !== "boolean") return null;
  return {
    enabled: raw.enabled,
    intervalSecs: typeof raw.intervalSecs === "number" ? raw.intervalSecs : 0,
    lastSuccessAt: typeof raw.lastSuccessAt === "string" ? raw.lastSuccessAt : null,
    lastError: typeof raw.lastError === "string" ? raw.lastError : null,
    rowsPushed: typeof raw.rowsPushed === "number" ? raw.rowsPushed : 0,
  };
}

function formatInterval(secs: number): string {
  if (secs % 3600 === 0) return `${secs / 3600} h`;
  if (secs % 60 === 0) return `${secs / 60} min`;
  return `${secs} s`;
}

function SinceSuccess({ iso }: { iso: string }) {
  const date = new Date(iso);
  const label = useTimeAgo(Number.isNaN(date.getTime()) ? null : date);
  return <>{label}</>;
}

function BackupBody({ status }: { status: BackupStatus }) {
  if (!status.enabled) {
    return (
      <View style={settingsStyles.row}>
        <View style={settingsStyles.rowContent}>
          <Text style={settingsStyles.rowTitle}>Off</Text>
          <Text style={settingsStyles.rowHint}>
            Set FEIYI_BACKUP_TURSO_URL, FEIYI_BACKUP_TURSO_TOKEN and FEIYI_BACKUP_PASSPHRASE on the
            server to back up to Turso.
          </Text>
        </View>
      </View>
    );
  }
  const failing = status.lastError !== null;
  return (
    <>
      <View style={settingsStyles.row}>
        <View style={settingsStyles.rowContent}>
          <Text style={settingsStyles.rowTitle}>
            {failing ? "Failing" : status.lastSuccessAt ? "OK" : "Waiting for the first backup"}
          </Text>
          {failing ? <Text style={settingsStyles.rowError}>{status.lastError}</Text> : null}
          {status.lastSuccessAt ? (
            <Text style={settingsStyles.rowHint}>
              Last success <SinceSuccess iso={status.lastSuccessAt} />
            </Text>
          ) : null}
        </View>
        <StatusBadge
          label={failing ? "Failing" : "On"}
          variant={failing ? "error" : "success"}
          size="xs"
        />
      </View>
      <View style={[settingsStyles.row, settingsStyles.rowBorder]}>
        <Text style={settingsStyles.rowTitle}>Rows pushed</Text>
        <Text style={styles.value}>{status.rowsPushed}</Text>
      </View>
      <View style={[settingsStyles.row, settingsStyles.rowBorder]}>
        <Text style={settingsStyles.rowTitle}>Interval</Text>
        <Text style={styles.value}>{formatInterval(status.intervalSecs)}</Text>
      </View>
    </>
  );
}

export function BackupSection({ serverId }: { serverId: string }) {
  const { config, isLoading } = useDaemonConfig(serverId);
  const status = parseBackupStatus(config);

  let body;
  if (status) {
    body = <BackupBody status={status} />;
  } else {
    body = (
      <View style={settingsStyles.row}>
        <Text style={settingsStyles.rowHint}>
          {isLoading ? "Loading..." : "Backup status is not available from this daemon."}
        </Text>
      </View>
    );
  }

  return (
    <SettingsSection title="Backup" testID="host-page-backup-section">
      <View style={settingsStyles.card}>{body}</View>
    </SettingsSection>
  );
}

const styles = StyleSheet.create((theme) => ({
  value: {
    color: theme.colors.foregroundMuted,
    fontSize: theme.fontSize.base,
    fontVariant: ["tabular-nums"],
  },
}));
