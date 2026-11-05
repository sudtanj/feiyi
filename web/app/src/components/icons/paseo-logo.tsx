import Svg, { Path } from "react-native-svg";
import { useUnistyles } from "react-native-unistyles";

interface PaseoLogoProps {
  size?: number;
  color?: string;
}

// feiyi's mark (web/logo.svg) without its background.
export function PaseoLogo({ size = 64, color }: PaseoLogoProps) {
  const { theme } = useUnistyles();
  const fill = color ?? theme.colors.foreground;

  return (
    <Svg width={size} height={size} viewBox="120 124 272 272" fill="none">
      <Path
        d="M144 176H192V344L144 392Z M144 176H328L368 128L344 224H144Z M192 272H280L312 240L296 312H192Z"
        fill={fill}
      />
    </Svg>
  );
}
