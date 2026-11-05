import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Pressable, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import { StyleSheet, useUnistyles } from "react-native-unistyles";
import { X } from "lucide-react-native";
import { isWeb } from "@/constants/platform";
import { BUILT_ID, hasNewBuild } from "@/utils/web-build-version";

const CHECK_INTERVAL_MS = 15 * 60 * 1000;

export function UpdateToast() {
  const { theme } = useUnistyles();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const [available, setAvailable] = useState(false);
  const [dismissed, setDismissed] = useState(false);

  useEffect(() => {
    if (!isWeb || !BUILT_ID) return undefined;
    const check = () => {
      if (document.visibilityState !== "visible") return;
      void hasNewBuild(BUILT_ID).then((newer) => {
        if (newer) setAvailable(true);
      });
    };
    document.addEventListener("visibilitychange", check);
    const interval = setInterval(check, CHECK_INTERVAL_MS);
    return () => {
      document.removeEventListener("visibilitychange", check);
      clearInterval(interval);
    };
  }, []);

  const containerStyle = useMemo(
    () => [styles.container, { bottom: theme.spacing[4] + insets.bottom }],
    [theme.spacing, insets.bottom],
  );

  const handleUpdate = useCallback(() => {
    location.reload();
  }, []);
  const handleDismiss = useCallback(() => setDismissed(true), []);

  if (!available || dismissed) {
    return null;
  }

  return (
    <View style={containerStyle} pointerEvents="box-none">
      <View style={styles.toast}>
        <Text style={styles.title}>{t("desktop.updates.callout.availableTitle")}</Text>
        <Pressable
          onPress={handleUpdate}
          accessibilityRole="button"
          hitSlop={8}
          style={styles.action}
        >
          <Text style={styles.actionLabel}>{t("settings.about.updates.update")}</Text>
        </Pressable>
        <Pressable
          onPress={handleDismiss}
          accessibilityRole="button"
          accessibilityLabel={t("common.actions.dismiss")}
          hitSlop={8}
          style={styles.dismiss}
        >
          <X size={16} color={theme.colors.foregroundMuted} />
        </Pressable>
      </View>
    </View>
  );
}

const styles = StyleSheet.create((theme) => ({
  container: {
    position: "absolute",
    left: theme.spacing[4],
    right: theme.spacing[4],
    zIndex: 1000,
  },
  toast: {
    flexDirection: "row",
    alignItems: "center",
    gap: theme.spacing[3],
    backgroundColor: theme.colors.surface2,
    borderRadius: theme.borderRadius.lg,
    borderWidth: theme.borderWidth[1],
    borderColor: theme.colors.border,
    paddingVertical: theme.spacing[3],
    paddingHorizontal: theme.spacing[4],
    ...theme.shadow.md,
  },
  title: {
    flex: 1,
    color: theme.colors.foreground,
    fontSize: theme.fontSize.base,
    fontWeight: theme.fontWeight.semibold,
  },
  action: {
    paddingVertical: theme.spacing[1],
    paddingHorizontal: theme.spacing[3],
    borderRadius: theme.borderRadius.md,
    backgroundColor: theme.colors.primary,
  },
  actionLabel: {
    color: theme.colors.primaryForeground,
    fontSize: theme.fontSize.sm,
    fontWeight: theme.fontWeight.semibold,
  },
  dismiss: {
    padding: theme.spacing[1],
  },
}));
