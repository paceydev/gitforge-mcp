"""Regression checks for the required-gate routing decision."""
import unittest
from changes import documentation_only


class Routing(unittest.TestCase):
    def test_docs_only_push_and_pr(self):
        for event in ("push", "pull_request"):
            self.assertTrue(documentation_only(event, "refs/heads/main", ["README.md", "docs/src/guide/getting-started.md"]))

    def test_code_configuration_and_mixed_changes_require_full_ci(self):
        for path in ("src/server.rs", "Cargo.toml", "Cargo.lock", "tests/docs.rs", "install.sh",
                     ".github/workflows/ci.yml", "scripts/ci/changes.py", "tests/fixtures/example.md"):
            self.assertFalse(documentation_only("push", "refs/heads/main", ["README.md", path]))

    def test_removed_source_still_requires_full_ci(self):
        self.assertFalse(documentation_only("pull_request", "refs/pull/1/merge", ["src/old.rs", "docs/new.md"]))

    def test_releases_manual_runs_unknown_events_and_empty_diffs_are_full(self):
        for event, ref, paths in (("push", "refs/tags/v1.0.0", ["README.md"]),
                                  ("workflow_dispatch", "refs/heads/main", ["README.md"]),
                                  ("workflow_call", "refs/heads/main", ["README.md"]),
                                  ("push", "refs/heads/main", [])):
            self.assertFalse(documentation_only(event, ref, paths))


if __name__ == "__main__":
    unittest.main()
