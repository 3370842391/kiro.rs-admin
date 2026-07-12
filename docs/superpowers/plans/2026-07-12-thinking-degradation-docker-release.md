# Thinking Degradation and Docker Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (\`- [ ]\`) syntax for tracking.

**Goal:** Preserve valid text/tool responses when Kiro omits reasoning and make Docker images traceable to the exact deployed commit.

**Architecture:** Add a default-off strict validation flag and pass it into non-stream and stream response validation. Keep empty-response/tool errors strict. Simplify Docker CI so master and release tags always build immutable SHA-tagged images with OCI provenance.

**Tech Stack:** Rust 2024, Axum, Serde, Tokio/SSE, GitHub Actions, Docker Buildx, Python unittest.

---

### Task 1: Add the strict-thinking configuration boundary

**Files:**
- Modify: \`src/model/config.rs\`
- Modify: \`src/kiro/provider.rs\`
- Modify: \`config.example.json\`

- [ ] **Step 1: Write failing tests**

Add beside existing config tests:

\`\`\`rust
#[test]
fn strict_thinking_validation_defaults_to_false() {
    let config: Config = serde_json::from_str("{}").unwrap();
    assert!(!config.strict_thinking_validation);
}

#[test]
fn strict_thinking_validation_can_be_enabled() {
    let config: Config =
        serde_json::from_str(r#"{"strictThinkingValidation":true}"#).unwrap();
    assert!(config.strict_thinking_validation);
}
\`\`\`

- [ ] **Step 2: Verify RED**

Run \`cargo test strict_thinking_validation -j 1\`.
Expected: compile failure because the field does not exist.

- [ ] **Step 3: Add minimal implementation**

Add to \`Config\` and its default:

\`\`\`rust
#[serde(default)]
pub strict_thinking_validation: bool,
\`\`\`

\`\`\`rust
strict_thinking_validation: false,
\`\`\`

Expose it from \`KiroProvider\`:

\`\`\`rust
pub fn strict_thinking_validation(&self) -> bool {
    self.token_manager.config().strict_thinking_validation
}
\`\`\`

Add \`"strictThinkingValidation": false\` to \`config.example.json\`.

- [ ] **Step 4: Verify GREEN**

Run \`cargo test strict_thinking_validation -j 1\`.
Expected: 2 passed.

- [ ] **Step 5: Commit**

\`\`\`powershell
git add -- src/model/config.rs src/kiro/provider.rs config.example.json
git commit -m "feat(thinking): 增加严格校验配置开关"
\`\`\`

### Task 2: Make non-stream missing-thinking responses degrade honestly

**Files:**
- Modify: \`src/anthropic/handlers.rs\`

- [ ] **Step 1: Write failing policy tests**

\`\`\`rust
#[test]
fn compatible_thinking_accepts_plain_text_without_reasoning_block() {
    let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
    assert!(validate_required_thinking(true, false, &content).is_ok());
}

#[test]
fn strict_thinking_rejects_plain_text_without_reasoning_block() {
    let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
    assert!(validate_required_thinking(true, true, &content).is_err());
}
\`\`\`

Update the redacted-thinking test to pass a strict flag.

- [ ] **Step 2: Verify RED**

Run \`cargo test required_thinking -j 1\`.
Expected: helper arity mismatch.

- [ ] **Step 3: Implement policy**

\`\`\`rust
fn validate_required_thinking(
    thinking_enabled: bool,
    strict_validation: bool,
    content: &[serde_json::Value],
) -> Result<(), &'static str> {
    if !thinking_enabled || !strict_validation {
        return Ok(());
    }
    if content.iter().any(|block| {
        matches!(
            block.get("type").and_then(serde_json::Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    }) {
        Ok(())
    } else {
        Err("client requested thinking but upstream produced no thinking content")
    }
}
\`\`\`

Pass \`provider.strict_thinking_validation()\` at the non-stream call site. When compatible mode accepts visible output without reasoning, log a warning without prompt content. Leave empty-response and tool validation unchanged.

- [ ] **Step 4: Verify GREEN**

Run:

\`\`\`powershell
cargo test required_thinking -j 1
cargo test empty_upstream_content_is_not_a_successful_non_stream_response -j 1
\`\`\`

Expected: all pass.

- [ ] **Step 5: Commit**

\`\`\`powershell
git add -- src/anthropic/handlers.rs
git commit -m "fix(thinking): 非流式缺失推理时兼容降级"
\`\`\`

### Task 3: Preserve stream text and tool calls without reasoning

**Files:**
- Modify: \`src/anthropic/stream.rs\`
- Modify: \`src/anthropic/handlers.rs\`

- [ ] **Step 1: Write failing stream tests**

\`\`\`rust
#[test]
fn compatible_thinking_stream_finishes_when_plain_text_arrives() {
    let mut ctx = StreamContext::new_with_constraints(
        "claude-opus-4-7",
        10,
        true,
        false,
        HashMap::new(),
        std::collections::HashSet::new(),
        super::super::converter::ToolChoicePolicy::Auto {
            disable_parallel_tool_use: false,
        },
    );
    let mut events = ctx.generate_initial_events();
    events.extend(ctx.process_assistant_response("正常中文回复"));
    events.extend(ctx.generate_final_events());
    assert!(events.iter().any(|event| event.event == "message_stop"));
    assert!(!events.iter().any(|event| event.event == "error"));
}

#[test]
fn strict_thinking_stream_errors_when_plain_text_arrives() {
    let mut ctx = StreamContext::new_with_constraints(
        "claude-opus-4-7",
        10,
        true,
        true,
        HashMap::new(),
        std::collections::HashSet::new(),
        super::super::converter::ToolChoicePolicy::Auto {
            disable_parallel_tool_use: false,
        },
    );
    let mut events = ctx.generate_initial_events();
    events.extend(ctx.process_assistant_response("plain text"));
    events.extend(ctx.generate_final_events());
    assert!(events.iter().any(|event| event.event == "error"));
    assert!(!events.iter().any(|event| event.event == "message_stop"));
}
\`\`\`

- [ ] **Step 2: Verify RED**

Run \`cargo test thinking_stream -j 1\`.
Expected: constructor arity mismatch.

- [ ] **Step 3: Implement propagation**

Add \`strict_thinking_validation: bool\` to \`StreamContext\` and all normal/buffered constructors. Gate the terminal error with:

\`\`\`rust
if self.thinking_enabled
    && self.strict_thinking_validation
    && !self.saw_reasoning_output
    && self.tool_json_error.is_none()
    && self.terminal_protocol_error.is_none()
{
    // retain the existing upstream_thinking_protocol_error
}
\`\`\`

Pass \`provider.strict_thinking_validation()\` from both streaming handlers. In compatibility mode, warn only when visible output exists and reasoning is absent.

- [ ] **Step 4: Verify stream, UTF-8, and tool behavior**

\`\`\`powershell
cargo test thinking_stream -j 1
cargo test identity -j 1
cargo test tool_choice -j 1
\`\`\`

Expected: all pass, including the Chinese stream reaching \`message_stop\`.

- [ ] **Step 5: Commit**

\`\`\`powershell
git add -- src/anthropic/stream.rs src/anthropic/handlers.rs
git commit -m "fix(stream): 缺失推理时保留有效输出"
\`\`\`

### Task 4: Make Docker images immutable and traceable

**Files:**
- Create: \`tests/test_docker_workflow.py\`
- Modify: \`.github/workflows/docker-build.yaml\`

- [ ] **Step 1: Write failing workflow contract tests**

\`\`\`python
from pathlib import Path
import unittest

WORKFLOW = Path(__file__).parents[1] / ".github" / "workflows" / "docker-build.yaml"

class DockerWorkflowContractTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text(encoding="utf-8")

    def test_tagged_master_is_not_skipped(self):
        self.assertNotIn("Skip build when commit already has release tag", self.text)
        self.assertNotIn("should_build", self.text)

    def test_release_tags_trigger_docker_build(self):
        self.assertIn("tags:", self.text)
        self.assertIn("- 'v*'", self.text)

    def test_images_have_revision_and_immutable_sha_tag(self):
        self.assertIn("org.opencontainers.image.revision=\${{ github.sha }}", self.text)
        self.assertIn("sha-\${SHORT_SHA}", self.text)

if __name__ == "__main__":
    unittest.main()
\`\`\`

- [ ] **Step 2: Verify RED**

Run \`python -m unittest tests.test_docker_workflow -v\`.
Expected: failures for skip logic, tag trigger, revision label, and SHA tag.

- [ ] **Step 3: Simplify Docker workflow**

Remove the pre-check job and \`should_build\` conditions. Trigger on \`master\` and \`v*\`. Resolve version with:

\`\`\`bash
if [[ "\${GITHUB_REF}" == refs/tags/v* ]]; then
  echo "version=\${GITHUB_REF_NAME#v}" >> "$GITHUB_OUTPUT"
  echo "is_beta=false" >> "$GITHUB_OUTPUT"
else
  SHORT_SHA="\${GITHUB_SHA::6}"
  echo "version=beta-\${SHORT_SHA}" >> "$GITHUB_OUTPUT"
  echo "is_beta=true" >> "$GITHUB_OUTPUT"
fi
\`\`\`

Publish both the version tag and \`sha-\${SHORT_SHA}\` tag. Add labels:

\`\`\`yaml
org.opencontainers.image.revision=\${{ github.sha }}
org.opencontainers.image.version=\${{ steps.version.outputs.version }}
org.opencontainers.image.created=\${{ steps.build_meta.outputs.created }}
\`\`\`

Keep \`beta\` only for master builds and \`latest\` only for release/manual builds.

- [ ] **Step 4: Verify GREEN**

Run \`python -m unittest tests.test_docker_workflow -v\`.
Expected: 3 passed.

- [ ] **Step 5: Commit**

\`\`\`powershell
git add -- .github/workflows/docker-build.yaml tests/test_docker_workflow.py
git commit -m "fix(ci): 让容器镜像绑定提交版本"
\`\`\`

### Task 5: Verify, merge, publish, deploy, and re-audit

**Files:**
- Modify only if verification exposes a reproduced regression.

- [ ] **Step 1: Run full local verification**

\`\`\`powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:CARGO_BUILD_JOBS='1'
cargo test -j 1 -q
cargo check --all-features -j 1 -q
python -m unittest tests.test_docker_workflow -v
git diff --check
\`\`\`

Expected: zero failures; document the existing dead-code warning if it remains.

- [ ] **Step 2: Run local release and black-box probes**

Build release, start port 8990 with ignored local config/credentials, then run \`anthropic_probe\`. Expected: thinking, tool_choice, parallel_canary, and stream pass.

- [ ] **Step 3: Verify 4.6/4.7 missing-thinking behavior**

Send thinking-enabled requests. Accept real thinking or successful text/tool content; reject any gateway-generated \`upstream_thinking_protocol_error\` in compatible mode.

- [ ] **Step 4: Finish and merge locally**

Use \`superpowers:finishing-a-development-branch\`, merge into master, and re-run tests on master.

- [ ] **Step 5: Push and deploy exact SHA image**

Push because the user explicitly requested a new production version. Wait for GitHub Actions, pull the immutable SHA tag/digest on \`43.225.196.10\`, recreate only \`kiro-rs-admin\`, and retain the old digest for rollback.

- [ ] **Step 6: Verify production and score regression**

Verify health, UTF-8 streaming, tool-result continuation, absence of \`identity.rs\` panic, and absence of missing-thinking 502. Run local ARA then Ztest; compare D5, D7, S3, D10, D17, D19, and S5 against the 74-point baseline. Fix only reproducible general protocol/data-flow bugs.
