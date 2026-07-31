#!/usr/bin/env python3
"""Guardrail tests for resolve_network.py and assert_testnet.sh.

The bug these exist to prevent is one we shipped twice and never noticed,
because both instances of it PASSED:

    redeploy-contract.yml   refused when $NETWORK == "mainnet"; $NETWORK was
                            hardcoded "testnet" for every env
    wipe-provision-db.yml   refused when the env was "production"/"mainnet";
                            the input offers only "staging"/"prod"

Two guards, two green checks, zero coverage. So the tests that matter here
are not the ones asserting the guard exists — that was true before. They are
the ones asserting it FIRES: given a mainnet environment, the guard must
block, and this file fails if it does not.

Run:
    python3 -m unittest discover -s rust-backend/deployment -v
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import resolve_network  # noqa: E402
from resolve_network import Ambiguous, resolve  # noqa: E402

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent
GUARD = HERE / "assert_testnet.sh"


def fake_tree(root: Path, env: str, services: dict[str, str | None]) -> None:
    """Build services/<svc>/config/config.<env>.toml for each entry.

    A None value writes a config with no network key at all; a value
    containing '=' is written verbatim so tests can produce table-nested
    declarations.
    """
    for service, network in services.items():
        cfg = root / "services" / service / "config"
        cfg.mkdir(parents=True, exist_ok=True)
        if network is None:
            body = 'bind_addr = "0.0.0.0:1"\n'
        elif "=" in network:
            body = network
        else:
            body = f'network = "{network}"\n'
        (cfg / f"config.{env}.toml").write_text(body)


class ResolverAgainstTheRealTree(unittest.TestCase):
    """Pins current behaviour so the change is provably inert today."""

    def test_both_envs_resolve_to_testnet_today(self):
        # The workflows previously hardcoded testnet for both envs. If this
        # ever disagrees, the replacement changed behaviour rather than
        # deriving it, which is the thing to catch before a deploy.
        self.assertEqual(resolve("staging"), "testnet")
        self.assertEqual(resolve("prod"), "testnet")

    def test_cctp_relay_is_excluded_and_named(self):
        # It declares mainnet on staging by design. Including it would make
        # staging permanently ambiguous, so the exclusion is load-bearing.
        self.assertIn("cctp-relay", resolve_network.INDEPENDENT_OF_ENV_NETWORK)
        self.assertNotIn("cctp-relay", resolve_network.declared_networks("staging"))

    def test_sample_is_not_empty(self):
        # Guards against the resolver "agreeing" over nothing: unanimity of
        # an empty set is how a check quietly stops covering anything.
        self.assertGreaterEqual(len(resolve_network.declared_networks("prod")), 5)


class ResolverFailsClosed(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def test_mainnet_env_resolves_to_mainnet(self):
        # The case the old guards could not express at all.
        fake_tree(self.root, "prod", {"indexer": "mainnet", "mm-bot": "mainnet"})
        self.assertEqual(resolve("prod", self.root), "mainnet")

    def test_partial_migration_is_ambiguous(self):
        # The likeliest real failure: someone flips some services and not
        # others. Majority-wins would answer "testnet" and let the wipe run.
        fake_tree(
            self.root, "prod",
            {"indexer": "mainnet", "mm-bot": "testnet", "token-info": "testnet"},
        )
        with self.assertRaises(Ambiguous) as ctx:
            resolve("prod", self.root)
        self.assertIn("partially migrated", str(ctx.exception))

    def test_no_declarations_is_ambiguous(self):
        fake_tree(self.root, "prod", {"indexer": None})
        with self.assertRaises(Ambiguous):
            resolve("prod", self.root)

    def test_unknown_env_is_ambiguous(self):
        with self.assertRaises(Ambiguous):
            resolve("does-not-exist", self.root)

    def test_unknown_network_value_is_ambiguous(self):
        fake_tree(self.root, "prod", {"indexer": "mainnett"})
        with self.assertRaises(Ambiguous):
            resolve("prod", self.root)

    def test_table_nested_network_is_ambiguous_not_ignored(self):
        # If a service moves its network into a [sui] table, the resolver
        # must refuse rather than silently drop it from the sample and keep
        # reporting a confident answer over fewer services.
        fake_tree(
            self.root, "prod",
            {
                "indexer": "testnet",
                "newsvc": '[sui]\nnetwork = "mainnet"\n',
            },
        )
        with self.assertRaises(Ambiguous) as ctx:
            resolve("prod", self.root)
        self.assertIn("newsvc", str(ctx.exception))

    def test_unparseable_config_is_ambiguous(self):
        fake_tree(self.root, "prod", {"indexer": "not = [valid toml\n"})
        with self.assertRaises(Ambiguous):
            resolve("prod", self.root)


class GuardActuallyFires(unittest.TestCase):
    """The tests the old guards would have failed.

    Each runs assert_testnet.sh for real and asserts on its exit status, so
    "the guard blocks" is demonstrated rather than asserted.
    """

    def run_guard(self, env: str, root: Path | None = None):
        script = GUARD
        if root is not None:
            # Point the guard at a fixture tree by copying it alongside a
            # resolver that reads from there.
            script = root / "assert_testnet.sh"
            script.write_text(
                GUARD.read_text().replace(
                    'python3 "$HERE/resolve_network.py" "$ENV"',
                    f'python3 "{HERE}/resolve_network.py" "$ENV"',
                )
            )
            script.chmod(0o755)
        env_arg = env
        return subprocess.run(
            ["bash", str(script), env_arg],
            capture_output=True, text=True, cwd=str(root or REPO_ROOT),
        )

    def test_passes_for_todays_testnet_envs(self):
        for env in ("staging", "prod"):
            with self.subTest(env=env):
                r = self.run_guard(env)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertEqual(r.stdout.strip(), "testnet")

    def test_BLOCKS_a_mainnet_env(self):
        # The whole point. Build a mainnet env and require a refusal.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_tree(root, "prodnet", {"indexer": "mainnet", "mm-bot": "mainnet"})
            patched = root / "resolve_network.py"
            patched.write_text(
                resolve_network.__file__ and
                Path(resolve_network.__file__).read_text().replace(
                    "REPO_ROOT = Path(__file__).resolve().parents[1]",
                    f"REPO_ROOT = Path({str(root)!r})",
                )
            )
            guard = root / "assert_testnet.sh"
            guard.write_text(GUARD.read_text())
            guard.chmod(0o755)
            r = subprocess.run(
                ["bash", str(guard), "prodnet"],
                capture_output=True, text=True,
            )
            self.assertNotEqual(r.returncode, 0, "guard did NOT fire on mainnet")
            self.assertIn("REFUSING", r.stderr)
            self.assertIn("mainnet", r.stderr)

    def test_BLOCKS_an_unresolvable_env(self):
        r = self.run_guard("no-such-env")
        self.assertNotEqual(r.returncode, 0, "guard did NOT fire on unknown env")
        self.assertIn("REFUSING", r.stderr)

    def test_refusal_prints_nothing_usable_on_stdout(self):
        # Callers do `NETWORK=$(assert_testnet.sh "$ENV")`. A refusal must not
        # hand back a string that reads like a network.
        r = self.run_guard("no-such-env")
        self.assertEqual(r.stdout.strip(), "")


class WorkflowsCallTheGuard(unittest.TestCase):
    """The guards are only wired if the workflows actually invoke them."""

    WORKFLOWS = (
        Path(".github/workflows/redeploy-contract.yml"),
        Path(".github/workflows/wipe-provision-db.yml"),
    )

    def test_both_destructive_workflows_invoke_the_guard(self):
        for wf in self.WORKFLOWS:
            path = REPO_ROOT.parent / wf
            with self.subTest(workflow=str(wf)):
                text = path.read_text()
                self.assertIn("assert_testnet.sh", text)

    def test_no_workflow_still_hardcodes_prod_to_testnet(self):
        # The exact resolver that made the old guard undeadable.
        for wf in self.WORKFLOWS:
            path = REPO_ROOT.parent / wf
            with self.subTest(workflow=str(wf)):
                text = path.read_text()
                self.assertNotIn('prod)    echo "network=testnet"', text)


if __name__ == "__main__":
    unittest.main()
