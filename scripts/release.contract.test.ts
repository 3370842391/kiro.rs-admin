import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function read(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('one-command release contract', () => {
  test('release script enforces branch, remote, clean tree and explicit confirmation', async () => {
    const script = await read('scripts/release.ps1')

    expect(script).toContain('当前分支必须是 master')
    expect(script).toContain("status', '--porcelain', '--untracked-files=no")
    expect(script).toContain("remote', 'get-url', '--push', 'deploy")
    expect(script).toContain('3370842391/kiro.rs-admin.git')
    expect(script).toContain('git merge-base --is-ancestor refs/remotes/deploy/master HEAD')
    expect(script).toContain('Read-Host "输入 RELEASE 确认发布')
    expect(script).toContain("$answer -cne 'RELEASE'")
    expect(script).toContain('ReadAllBytes')
    expect(script).toContain('WriteAllBytes')
    expect(script).toContain("restore', '--staged', '--', 'Cargo.toml', 'Cargo.lock")
    expect(script).not.toContain('git add -A')
    expect(script).not.toContain('reset --hard')
    expect(script).not.toContain('--force')
  })

  test('release script pushes master before creating and pushing the tag', async () => {
    const script = await read('scripts/release.ps1')
    const commit = script.indexOf("commit', '-m', \"chore(release): 发布 $tag\"")
    const pushMaster = script.indexOf("push', 'deploy', 'master'")
    const createTag = script.indexOf("tag', '-a', $tag")
    const pushTag = script.indexOf("push', 'deploy', $tag")

    expect(commit).toBeGreaterThan(-1)
    expect(pushMaster).toBeGreaterThan(commit)
    expect(createTag).toBeGreaterThan(pushMaster)
    expect(pushTag).toBeGreaterThan(createTag)
  })

  test('dry-run exits before file preparation and all remote writes', async () => {
    const script = await read('scripts/release.ps1')
    const dryRunExit = script.indexOf('if ($DryRun) {')
    const backup = script.indexOf('[System.IO.File]::ReadAllBytes')
    const pushMaster = script.indexOf("push', 'deploy', 'master'")

    expect(dryRunExit).toBeGreaterThan(-1)
    expect(backup).toBeGreaterThan(dryRunExit)
    expect(pushMaster).toBeGreaterThan(backup)
  })

  test('release notes fall back from changelog to GitHub generated notes and fixed text', async () => {
    const workflow = await read('.github/workflows/release.yaml')
    const writeNotes = workflow.indexOf('- name: Write release notes')
    const createRelease = workflow.indexOf('- name: Create GitHub Release')
    const notesStep = workflow.slice(writeNotes, createRelease)

    expect(writeNotes).toBeGreaterThan(-1)
    expect(createRelease).toBeGreaterThan(writeNotes)
    expect(notesStep).toContain('GH_TOKEN: ${{ github.token }}')
    expect(notesStep).toContain('/repos/${GITHUB_REPOSITORY}/releases/generate-notes')
    expect(notesStep).toContain('--method POST')
    expect(notesStep).toContain("--jq '.body'")
    expect(notesStep).toContain('Kiro.rs ${VERSION}')
  })
})
