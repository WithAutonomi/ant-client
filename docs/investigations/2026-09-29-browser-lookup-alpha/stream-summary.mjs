// Summarize stream-seek logs, plain or gzipped: the first frame after connecting (before the
// seek), the first range read, seek resume, the waits after resuming (count and total seconds),
// and how long playback was watched after resuming. A wait still open when the log ends is
// counted to the last line and flagged, because its true length is unknown.
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
for (const file of process.argv.slice(2)) {
  const raw = readFileSync(file);
  const lines = (file.endsWith(".gz") ? gunzipSync(raw) : raw).toString("utf8").split("\n").filter(Boolean);
  const time = (line) => Number(line.split(" ")[0]);
  const at = (pattern, from = lines) => { const line = from.find((l) => pattern.test(l)); return line ? time(line) : NaN; };
  const connected = at(/ connected$/), seek = at(/seeking to/), resumed = at(/video seeked/);
  const beforeSeek = Number.isNaN(seek) ? lines : lines.filter((l) => time(l) < seek);
  const firstFrame = at(/video playing/, beforeSeek);
  const firstRange = Number(lines.find((l) => /range 0\+1048576 -> ok/.test(l))?.match(/in (\d+)ms/)?.[1]);
  const end = time(lines.at(-1));
  let waits = 0, waited = 0, since;
  for (const l of lines) {
    const t = time(l);
    if (!(t > resumed)) continue;
    if (/video waiting/.test(l) && since === undefined) { since = t; waits++; }
    if (/video playing/.test(l) && since !== undefined) { waited += t - since; since = undefined; }
  }
  if (since !== undefined) waited += end - since;
  const frame = Number.isNaN(firstFrame) ? "none before the seek" : `${(firstFrame - connected).toFixed(1)}s`;
  const open = since === undefined ? "" : ", still waiting when the log ended";
  console.log(`${file.split("/").at(-1).replace(/\.gz$/, "")}: first frame ${frame} (first range ${(firstRange / 1000).toFixed(1)}s), seek resume ${(resumed - seek).toFixed(1)}s, after-seek waits ${waits} totalling ${waited.toFixed(1)}s${open}, watched ${(end - resumed).toFixed(1)}s after resuming`);
}
