import { parseBrowserManifest } from "../pkg/ant_core.js";

export { parseBrowserManifest };

export async function fetchBrowserManifest(url) {
  const response = await fetch(url, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`Manifest request failed with HTTP ${response.status}`);
  }
  return parseBrowserManifest(await response.json());
}
