import { describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

const root = join(import.meta.dir, "..");

async function repositoryFile(path: string): Promise<string> {
  try {
    return await readFile(join(root, path), "utf8");
  } catch (error) {
    throw new Error(`required repository file is missing: ${path}`, { cause: error });
  }
}

describe("8991 test builder contract", () => {
  test("Dockerfile.test uses reproducible cached release builds", async () => {
    const dockerfile = await repositoryFile("Dockerfile.test");

    expect(dockerfile).toContain("mount=type=cache");
    expect(dockerfile).toContain(
      "cargo build --release --locked --no-default-features",
    );
  });

  test("docker-compose.test.yml isolates the public test service", async () => {
    const compose = await repositoryFile("docker-compose.test.yml");

    expect(compose).toContain("0.0.0.0:8991:8990");
    expect(compose).toContain("./data-test:/app/config");
    expect(compose).toContain("kiro-rs-test:${TEST_IMAGE_TAG:-latest}");
  });

  test("test deploy script performs detached, disposable health-checked deploys", async () => {
    const script = await repositoryFile("scripts/test-deploy.sh");

    expect(script).toContain("git checkout --detach");
    expect(script).toContain("http://127.0.0.1:8991/");
    expect(script).toContain("docker run --rm");
    expect(script).not.toMatch(/docker\s+(?:stop|rm)\s+kiro-rs-admin/);
  });

  test("runtime test data is excluded from build context and version control", async () => {
    const [dockerignore, gitignore] = await Promise.all([
      repositoryFile(".dockerignore"),
      repositoryFile(".gitignore"),
    ]);

    expect(dockerignore).toMatch(/^data-test\/$/m);
    expect(gitignore).toMatch(/^\/data-test\/$/m);
  });
});
