import { readFile } from "node:fs/promises";
import initAntCore from "./pkg/ant_core.js";

const wasm = await readFile(new URL("./pkg/ant_core_bg.wasm", import.meta.url));
await initAntCore({ module_or_path: wasm });
