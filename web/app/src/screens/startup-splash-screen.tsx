import { useCallback, useEffect, useMemo, useState } from "react";
import { ScrollView, Text, View } from "react-native";
import Animated, {
  cancelAnimation,
  Easing,
  useAnimatedStyle,
  useSharedValue,
  withRepeat,
  withTiming,
} from "react-native-reanimated";
import MaskedView from "@react-native-masked-view/masked-view";
import Svg, { Defs, LinearGradient as SvgLinearGradient, Rect, Stop } from "react-native-svg";
import * as Clipboard from "expo-clipboard";
import { useTranslation } from "react-i18next";
import { openExternalUrl } from "@/utils/open-external-url";
import { BookOpen, Copy, RotateCw, TriangleAlert } from "lucide-react-native";
import { StyleSheet, useUnistyles } from "react-native-unistyles";
import { PaseoLogo } from "@/components/icons/paseo-logo";
import { Button } from "@/components/ui/button";
import { getDesktopDaemonLogs, type DesktopDaemonLogs } from "@/desktop/daemon/desktop-daemon";
import { TitlebarDragRegion } from "@/components/desktop/titlebar-drag-region";
import { isNative, isWeb } from "@/constants/platform";
import { CODE_SURFACE_DATASET } from "@/styles/code-surface";

interface StartupSplashScreenProps {
  bootstrapState?: {
    splashError: string | null;
    retry: () => void;
  };
}

const GITHUB_ISSUE_URL = "https://github.com/getpaseo/paseo/issues/new";
const DOCS_URL = "https://paseo.sh/docs";

const LOGO_SIZE = 96;
const SHIMMER_PEAK_WIDTH = 120;
const SHIMMER_DURATION_MS = 1800;

function openGithubIssue(): void {
  void openExternalUrl(GITHUB_ISSUE_URL);
}

function openDocs(): void {
  void openExternalUrl(DOCS_URL);
}

const WEB_SPLASH_SHIMMER_KEYFRAME_ID = "paseo-splash-shimmer-keyframes";
const WEB_SPLASH_SHIMMER_ANIMATION_NAME = "paseo-splash-shimmer";

const WEB_SPLASH_SHIMMER_KEYFRAME_CSS = `
  @keyframes ${WEB_SPLASH_SHIMMER_ANIMATION_NAME} {
    0% {
      background-position: -${LOGO_SIZE + SHIMMER_PEAK_WIDTH}px 0;
    }
    100% {
      background-position: ${LOGO_SIZE + SHIMMER_PEAK_WIDTH}px 0;
    }
  }
`;

let webSplashShimmerRegistered = false;

function ensureWebSplashShimmerKeyframes() {
  if (isNative) {
    return;
  }
  if (webSplashShimmerRegistered) {
    return;
  }
  const existing = document.getElementById(WEB_SPLASH_SHIMMER_KEYFRAME_ID);
  if (existing) {
    webSplashShimmerRegistered = true;
    return;
  }
  const styleElement = document.createElement("style");
  styleElement.id = WEB_SPLASH_SHIMMER_KEYFRAME_ID;
  styleElement.textContent = WEB_SPLASH_SHIMMER_KEYFRAME_CSS;
  document.head.appendChild(styleElement);
  webSplashShimmerRegistered = true;
}

function LogoShimmer() {
  const { theme } = useUnistyles();

  if (isWeb) {
    return <WebLogoShimmer color={theme.colors.foreground} />;
  }

  return <NativeLogoShimmer color={theme.colors.foreground} />;
}

function WebLogoShimmer({ color }: { color: string }) {
  useEffect(() => {
    ensureWebSplashShimmerKeyframes();
  }, []);

  const shimmerStyle = useMemo(
    () => ({
      width: LOGO_SIZE,
      height: LOGO_SIZE,
      WebkitMaskImage: `url("data:image/svg+xml,${encodeURIComponent(`<svg xmlns='http://www.w3.org/2000/svg' width='${LOGO_SIZE}' height='${LOGO_SIZE}' viewBox='60 60 392 392'><path fill='black' d='M240 231Q243 229 243 227Q243 224 241 221Q239 218 236 216Q235 215 235 215L277 213Q274 214 273 215Q272 217 272 219Q271 221 271 222Q271 225 276 227Q297 234 315 245Q318 247 320 247Q323 247 324 244Q326 242 326 240Q327 237 327 236Q327 235 326 234Q325 232 320 230Q316 227 307 223Q298 220 281 214Q279 213 278 213L304 212Q306 212 308 212Q310 212 310 209Q310 208 309 206Q308 204 306 202L309 161Q309 160 310 159Q310 158 310 156Q310 154 307 151Q304 149 300 149H298L280 150Q282 149 282 145V132L318 130Q317 132 316 135Q315 138 314 141Q314 141 315 141Q314 141 314 141Q311 140 307 139Q304 137 300 136Q298 135 296 134Q295 134 293 134Q291 134 291 136Q291 138 296 142Q306 151 311 154Q316 156 317 156Q319 156 322 154Q324 152 326 150Q327 148 328 144Q330 140 331 132Q332 131 332 129Q333 128 333 127Q333 124 330 122Q328 119 322 119H321L282 121V115L317 113Q320 113 322 112Q323 112 323 110Q323 107 320 103L322 95Q322 94 323 92Q323 91 323 90Q323 88 321 85Q319 82 314 82H312L282 84V73Q282 70 281 69Q280 68 276 66Q274 65 272 65Q271 65 270 65Q266 65 266 67Q266 68 267 70Q268 71 269 73Q269 75 269 77V85L244 86Q244 85 244 81Q244 78 244 75Q244 72 241 70Q238 68 235 68Q232 67 231 67Q228 67 228 70Q228 71 229 73Q231 76 231 80Q231 81 231 82Q231 83 231 84Q231 85 231 86V87L198 89H196Q194 89 192 89Q190 88 189 88Q188 88 187 88Q185 88 185 90Q185 91 186 94Q187 96 190 98Q192 100 196 100Q197 100 199 100Q200 100 202 100L230 98Q230 102 229 106L203 107Q200 106 197 104Q194 103 193 103Q191 103 191 106Q191 107 191 108Q191 109 191 110Q191 114 190 117L186 127Q186 129 186 131Q186 134 188 136Q191 137 193 137Q194 137 196 137Q198 137 199 137L216 136Q211 142 202 148Q194 154 186 158Q180 161 180 164Q180 166 183 166Q185 166 190 165Q194 163 202 159L202 161Q203 162 203 164L205 203Q205 204 206 205Q206 206 206 207Q206 210 205 214Q205 214 205 214Q205 215 205 215Q205 219 207 221Q210 222 212 223Q214 223 215 223Q220 223 220 218V218L220 215L230 215Q229 215 229 216Q228 220 225 222Q216 227 206 232Q196 237 184 241Q179 242 179 245Q179 247 183 247Q184 247 184 247Q185 247 185 247Q200 245 213 241Q226 238 240 231ZM292 194 291 201 219 204 219 197ZM293 177 293 183 218 186 218 180ZM294 161 294 166 217 170 217 164ZM203 118 227 117Q226 118 225 120Q224 123 224 124L201 126ZM269 115V122L238 123Q239 123 239 121Q240 118 240 117ZM269 96 269 104 243 106Q243 104 243 102Q244 99 244 98ZM308 94 307 102 282 103V96ZM277 150 215 153 214 153Q218 150 223 146Q228 141 232 135L269 133Q269 133 269 136Q269 138 268 140Q268 141 268 141Q268 142 268 142Q268 145 270 147Q272 149 274 150Q276 150 277 150Z M252 388Q251 388 250 388Q248 388 248 391L249 394V396Q249 398 248 399L245 406Q244 408 244 411Q244 414 248 416Q251 419 254 419Q256 419 258 418Q259 418 261 417L289 416V421Q289 426 288 437V438Q288 442 292 444Q295 446 298 446Q303 446 303 441V416L340 415H340Q345 414 345 411Q345 408 341 405Q337 401 335 401Q333 401 330 402Q327 403 324 403L303 404V388L328 387Q331 386 333 386Q334 385 334 382Q334 380 330 377Q326 373 324 373Q322 373 320 374Q318 375 312 375L303 376V365L322 364Q324 364 325 364Q327 363 327 361Q327 359 323 355L326 342Q326 341 327 340Q328 339 328 337Q328 336 325 333Q323 330 317 330L264 333Q254 330 251 330Q248 330 248 332Q248 334 250 337Q251 340 252 344L253 356V361Q253 362 252 363V365Q252 367 255 370Q258 372 262 372Q267 372 267 367V367L289 366L289 376V376L258 377H257Q252 377 247 376Q245 376 245 378Q245 379 245 380Q248 387 252 388ZM194 297 230 295Q235 294 235 291Q235 286 227 282Q224 281 224 281Q223 281 221 282Q218 282 214 283L189 284H188Q184 284 180 283H178Q176 283 176 286L177 289Q180 297 188 297H191Q192 297 194 297ZM199 327 198 421Q198 429 197 432Q197 434 197 435Q197 438 200 440Q203 443 208 443Q213 443 213 438L213 327L236 325Q242 325 242 321Q242 319 240 317Q238 315 235 313Q233 311 231 311Q229 311 228 312Q226 312 221 313L178 316H177Q173 316 169 315Q168 315 167 315Q164 315 164 317Q164 317 166 321Q167 324 172 328Q173 329 176 329Q179 329 182 328ZM223 348 222 391Q222 396 222 399Q221 402 221 404Q221 407 224 410Q227 412 231 412Q236 412 236 407L236 344Q236 340 233 338Q228 336 223 336Q220 336 220 338Q220 339 221 342Q223 344 223 348ZM193 346V346Q193 342 189 340Q185 338 180 338Q176 338 176 341Q176 341 177 344Q178 346 178 350Q178 388 169 415Q169 417 169 420Q169 423 171 423Q174 423 179 414Q183 405 188 387Q192 370 193 346ZM312 342 310 354 266 356 265 344ZM311 299 309 311 285 313 288 300ZM342 322Q348 321 348 317Q348 314 341 310Q338 308 337 308Q336 308 335 309Q333 309 332 310Q330 310 329 310L323 310L325 297Q326 297 326 296Q327 295 327 293Q327 292 325 289Q323 286 317 286H316L290 288L292 275Q293 275 293 273Q293 268 285 267Q282 266 279 266Q277 266 277 268Q277 269 277 270Q278 272 278 274Q278 276 277 278L276 288L258 290H256Q251 290 250 289Q248 289 247 289Q245 289 245 291Q245 293 248 298Q250 302 255 302H258Q259 302 261 302L274 301L271 313L253 314H251Q248 314 246 314Q244 313 243 313Q241 313 241 315Q241 317 242 319Q245 326 252 326ZM255 389Q256 389 257 389H260L289 388V405L259 406L263 398Q263 397 263 396Q263 392 255 389Z'/></svg>`)}")`,
      WebkitMaskSize: "contain",
      WebkitMaskRepeat: "no-repeat",
      WebkitMaskPosition: "center",
      background: `linear-gradient(90deg, ${color} 0%, ${color}88 40%, ${color}FF 50%, ${color}88 60%, ${color} 100%)`,
      backgroundSize: `${LOGO_SIZE + SHIMMER_PEAK_WIDTH * 2}px ${LOGO_SIZE}px`,
      animationName: WEB_SPLASH_SHIMMER_ANIMATION_NAME,
      animationDuration: `${SHIMMER_DURATION_MS}ms`,
      animationTimingFunction: "linear",
      animationIterationCount: "infinite",
    }),
    [color],
  );

  return <View style={shimmerStyle as never} />;
}

function NativeLogoShimmer({ color }: { color: string }) {
  const shimmerTranslateX = useSharedValue(-SHIMMER_PEAK_WIDTH);

  useEffect(() => {
    shimmerTranslateX.value = -SHIMMER_PEAK_WIDTH;
    shimmerTranslateX.value = withRepeat(
      withTiming(LOGO_SIZE + SHIMMER_PEAK_WIDTH, {
        duration: SHIMMER_DURATION_MS,
        easing: Easing.linear,
      }),
      -1,
      false,
    );
    return () => {
      cancelAnimation(shimmerTranslateX);
    };
  }, [shimmerTranslateX]);

  const peakStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: shimmerTranslateX.value }],
  }));

  const trackStyle = useMemo(
    () => [styles.nativeShimmerTrack, { width: LOGO_SIZE, height: LOGO_SIZE }],
    [],
  );

  const peakCombinedStyle = useMemo(
    () => [styles.nativeShimmerPeak, peakStyle, { width: SHIMMER_PEAK_WIDTH, height: LOGO_SIZE }],
    [peakStyle],
  );

  const maskElement = useMemo(
    () => (
      <View style={styles.shimmerMask}>
        <PaseoLogo size={LOGO_SIZE} color="#000000" />
      </View>
    ),
    [],
  );

  return (
    <MaskedView style={trackStyle} maskElement={maskElement}>
      <View style={trackStyle}>
        <View style={styles.nativeShimmerBase}>
          <PaseoLogo size={LOGO_SIZE} color={color} />
        </View>
        <Animated.View style={peakCombinedStyle}>
          <Svg width="100%" height="100%" preserveAspectRatio="none">
            <Defs>
              <SvgLinearGradient id="splashShimmer" x1="0" y1="0" x2="1" y2="0">
                <Stop offset="0" stopColor="#FFFFFF" stopOpacity="0" />
                <Stop offset="0.5" stopColor="#FFFFFF" stopOpacity="0.4" />
                <Stop offset="1" stopColor="#FFFFFF" stopOpacity="0" />
              </SvgLinearGradient>
            </Defs>
            <Rect x="0" y="0" width="100%" height="100%" fill="url(#splashShimmer)" />
          </Svg>
        </Animated.View>
      </View>
    </MaskedView>
  );
}

const styles = StyleSheet.create((theme) => ({
  container: {
    position: "relative",
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    backgroundColor: theme.colors.surface0,
    paddingHorizontal: theme.spacing[8],
    paddingVertical: theme.spacing[8],
  },
  errorScreen: {
    position: "relative",
    flex: 1,
    backgroundColor: theme.colors.surface0,
  },
  errorScrollView: {
    flex: 1,
    ...(isWeb
      ? {
          overflowX: "auto",
          overflowY: "auto",
          WebkitAppRegion: "no-drag",
        }
      : null),
  },
  errorScrollContent: {
    flexGrow: 1,
    alignItems: "center",
    justifyContent: "flex-start",
    paddingHorizontal: theme.spacing[8],
    paddingVertical: theme.spacing[8],
    paddingTop: theme.spacing[16],
  },
  errorContent: {
    alignItems: "stretch",
    maxWidth: 720,
    width: "100%",
    gap: theme.spacing[6],
  },
  errorHeader: {
    alignItems: "flex-start",
  },
  title: {
    color: theme.colors.foreground,
    fontSize: theme.fontSize.base,
    fontWeight: theme.fontWeight.semibold,
    textAlign: "left",
  },
  errorDescription: {
    color: theme.colors.foregroundMuted,
    fontSize: theme.fontSize.base,
    lineHeight: 22,
  },
  errorMessage: {
    color: theme.colors.destructive,
    fontSize: theme.fontSize.code,
    lineHeight: 20,
    fontFamily: theme.fontFamily.mono,
  },
  logsMeta: {
    color: theme.colors.foregroundMuted,
    fontSize: theme.fontSize.base,
  },
  logsContainer: {
    height: 200,
    borderRadius: theme.borderRadius.xl,
    backgroundColor: theme.colors.surface1,
    borderWidth: theme.borderWidth[1],
    borderColor: theme.colors.border,
    overflow: "hidden",
  },
  logsScroll: {
    flexGrow: 0,
  },
  logsContent: {
    padding: theme.spacing[4],
  },
  logsText: {
    fontFamily: theme.fontFamily.mono,
    fontSize: theme.fontSize.code,
    color: theme.colors.foreground,
    lineHeight: 18,
    ...(isWeb
      ? {
          whiteSpace: "pre",
          overflowWrap: "normal",
        }
      : null),
  },
  actionRow: {
    flexDirection: "row",
    gap: theme.spacing[3],
    flexWrap: "wrap",
  },
  shimmerMask: {
    width: LOGO_SIZE,
    height: LOGO_SIZE,
    alignItems: "center",
    justifyContent: "center",
  },
  nativeShimmerTrack: {
    overflow: "hidden",
  },
  nativeShimmerBase: {
    position: "absolute",
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
  },
  nativeShimmerPeak: {
    position: "absolute",
    top: 0,
    bottom: 0,
  },
}));

export function StartupSplashScreen({ bootstrapState }: StartupSplashScreenProps) {
  const { t } = useTranslation();
  const { theme } = useUnistyles();
  const [daemonLogs, setDaemonLogs] = useState<DesktopDaemonLogs | null>(null);
  const [logsError, setLogsError] = useState<string | null>(null);
  const [isLoadingLogs, setIsLoadingLogs] = useState(false);

  const isError = bootstrapState !== undefined && bootstrapState.splashError !== null;

  useEffect(() => {
    if (!isError) {
      setDaemonLogs(null);
      setLogsError(null);
      setIsLoadingLogs(false);
      return;
    }

    let isCancelled = false;
    setIsLoadingLogs(true);
    setLogsError(null);

    void getDesktopDaemonLogs()
      .then((logs) => {
        if (isCancelled) {
          return;
        }
        setDaemonLogs(logs);
        return;
      })
      .catch((error) => {
        if (isCancelled) {
          return;
        }
        const message = error instanceof Error ? error.message : String(error);
        setDaemonLogs(null);
        setLogsError(t("startup.logs.loadFailed", { message }));
      })
      .finally(() => {
        if (!isCancelled) {
          setIsLoadingLogs(false);
        }
      });

    return () => {
      isCancelled = true;
    };
  }, [isError, t]);

  const logsText = useMemo(() => {
    if (isLoadingLogs) {
      return t("startup.logs.loading");
    }
    if (daemonLogs?.contents) {
      return daemonLogs.contents;
    }
    if (logsError) {
      return logsError;
    }
    return t("startup.logs.unavailable");
  }, [daemonLogs?.contents, isLoadingLogs, logsError, t]);

  const handleCopyLogs = useCallback(() => {
    const payload = daemonLogs?.logPath
      ? `${daemonLogs.logPath}\n\n${daemonLogs.contents}`
      : logsText;
    void Clipboard.setStringAsync(payload);
  }, [daemonLogs?.logPath, daemonLogs?.contents, logsText]);

  const copyIcon = useMemo(
    () => <Copy size={16} color={theme.colors.foreground} />,
    [theme.colors.foreground],
  );
  const warningIcon = useMemo(
    () => <TriangleAlert size={16} color={theme.colors.foreground} />,
    [theme.colors.foreground],
  );
  const bookIcon = useMemo(
    () => <BookOpen size={16} color={theme.colors.foreground} />,
    [theme.colors.foreground],
  );
  const retryIcon = useMemo(
    () => <RotateCw size={16} color={theme.colors.palette.white} />,
    [theme.colors.palette.white],
  );

  if (!isError) {
    return (
      <View testID="startup-splash" style={styles.container}>
        <TitlebarDragRegion />
        <LogoShimmer />
      </View>
    );
  }

  return (
    <View style={styles.errorScreen}>
      <TitlebarDragRegion />
      <ScrollView
        style={styles.errorScrollView}
        contentContainerStyle={styles.errorScrollContent}
        showsVerticalScrollIndicator
      >
        <View style={styles.errorContent}>
          <View style={styles.errorHeader}>
            <PaseoLogo size={64} />
            <Text style={styles.title}>{t("startup.errorTitle")}</Text>
          </View>

          <Text style={styles.errorDescription}>{t("startup.errorDescription")}</Text>

          <Text dataSet={CODE_SURFACE_DATASET} style={styles.errorMessage}>
            {bootstrapState.splashError}
          </Text>

          {daemonLogs?.logPath ? <Text style={styles.logsMeta}>{daemonLogs.logPath}</Text> : null}

          <View style={styles.logsContainer}>
            <ScrollView
              style={styles.logsScroll}
              contentContainerStyle={styles.logsContent}
              showsVerticalScrollIndicator
            >
              <Text dataSet={CODE_SURFACE_DATASET} selectable style={styles.logsText}>
                {logsText}
              </Text>
            </ScrollView>
          </View>

          <View style={styles.actionRow}>
            <Button variant="secondary" leftIcon={copyIcon} onPress={handleCopyLogs}>
              Copy logs
            </Button>
            <Button variant="outline" leftIcon={warningIcon} onPress={openGithubIssue}>
              Open GitHub issue
            </Button>
            <Button variant="outline" leftIcon={bookIcon} onPress={openDocs}>
              Docs
            </Button>
            <Button variant="default" leftIcon={retryIcon} onPress={bootstrapState.retry}>
              Retry
            </Button>
          </View>
        </View>
      </ScrollView>
    </View>
  );
}
