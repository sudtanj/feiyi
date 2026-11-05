export const BUILT_ID = process.env.EXPO_PUBLIC_BUILD_ID || "";

// version.json is written by pages.yml next to the build; offline or malformed answers mean "unknown".
export async function hasNewBuild(builtId: string): Promise<boolean> {
  try {
    const response = await fetch(`${process.env.EXPO_PUBLIC_PAGES_BASE_PATH || ""}/version.json`, {
      cache: "no-store",
    });
    if (!response.ok) return false;
    const { id } = (await response.json()) as { id?: unknown };
    return typeof id === "string" && id !== builtId;
  } catch {
    return false;
  }
}
