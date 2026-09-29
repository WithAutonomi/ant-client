// Summarize a stream-diag log: first frame after connect, first range read, seek resume,
// and the waits after resuming (count and total seconds).
import { readFileSync } from "node:fs";
for (const file of process.argv.slice(2)) {
  const lines = readFileSync(file, "utf8").split("\n");
  const at = (pattern) => { const line = lines.find((l) => pattern.test(l)); return line ? Number(line.split(" ")[0]) : NaN; };
  const connected = at(/ connected$/), firstFrame = at(/video playing/), seek = at(/seeking to/), resumed = at(/video seeked/);
  const firstRange = Number(lines.find((l) => /range 0\+1048576 -> ok/.test(l))?.match(/in (\d+)ms/)?.[1]);
  let waits = 0, waited = 0, since;
  for (const l of lines) {
    const t = Number(l.split(" ")[0]);
    if (!(t > resumed)) continue;
    if (/video waiting/.test(l)) { since = t; waits++; }
    if (/video playing/.test(l) && since !== undefined) { waited += t - since; since = undefined; }
  }
  if (since !== undefined) waited += Number(lines.filter(Boolean).at(-1).split(" ")[0]) - since;
  console.log(`${file.split("/").at(-1)}: first frame ${(firstFrame - connected).toFixed(1)}s (first range ${(firstRange / 1000).toFixed(1)}s), seek resume ${(resumed - seek).toFixed(1)}s, after-seek waits ${waits} totalling ${waited.toFixed(1)}s`);
}
