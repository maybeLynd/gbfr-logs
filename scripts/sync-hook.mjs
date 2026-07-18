import { copyFileSync, existsSync } from "node:fs";
import { resolve } from "node:path";

const targetDirectory = process.env.CARGO_TARGET_DIR || "target";
const source = resolve(targetDirectory, "release/hook.dll");
const destination = resolve("src-tauri/hook.dll");

if (!existsSync(source)) {
  throw new Error(`Hook build was not found at ${source}`);
}

copyFileSync(source, destination);
console.log(`Synchronized ${source} -> ${destination}`);
