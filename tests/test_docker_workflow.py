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
        self.assertIn("org.opencontainers.image.revision=${{ github.sha }}", self.text)
        self.assertIn("sha-${SHORT_SHA}", self.text)


if __name__ == "__main__":
    unittest.main()
