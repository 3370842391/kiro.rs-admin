import { describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

const root = join(import.meta.dir, "..");

describe("error snapshot smoke script", () => {
  test("is a safe, opt-in admin API smoke test", async () => {
    const script = await readFile(join(root, "scripts/error-snapshot-smoke.sh"), "utf8");

    expect(script).toContain("set -Eeuo pipefail");
    expect(script).toContain("ERROR_SNAPSHOT_BASE_URL");
    expect(script).toContain("ERROR_SNAPSHOT_ADMIN_TOKEN");
    expect(script).toContain("http://127.0.0.1:8991/api/admin");
    expect(script).not.toContain("http://127.0.0.1:8991/admin}");
    expect(script).toContain("/error-snapshots/storage");
    expect(script).toContain("/error-snapshots?limit=1");
    expect(script).toContain("/error-snapshots/cleanup");
    expect(script).toContain("curl --fail --silent --show-error");
    expect(script).not.toContain('echo "${ERROR_SNAPSHOT_ADMIN_TOKEN}"');
    expect(script).not.toContain('printf "%s" "${ERROR_SNAPSHOT_ADMIN_TOKEN}"');
  });

  test("supports dry-run and does not mutate snapshots by default", async () => {
    const script = await readFile(join(root, "scripts/error-snapshot-smoke.sh"), "utf8");

    expect(script).toContain("ERROR_SNAPSHOT_SMOKE_MUTATE");
    expect(script).toContain("if [[ \"${ERROR_SNAPSHOT_SMOKE_MUTATE}\" == \"1\" ]]");
    expect(script).toContain("/error-snapshots/cleanup");
  });
});
