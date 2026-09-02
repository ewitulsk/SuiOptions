#!/usr/bin/env python3
"""Guardrail tests for affected.py's service<->path mapping.

The bug these exist to prevent (SO-315) is silent: when the deploy filter
under-selects, the workflow still goes green and the skipped service keeps
running an image built against an older crate. Nothing fails, so nothing
gets noticed. These tests are the only thing that notices.

Run:
    python3 -m unittest discover -s rust-backend/deployment -v
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import affected  # noqa: E402

REPO_ROOT = affected.REPO_ROOT
RUST = REPO_ROOT / "rust-backend"

# Changed-file list of ec2b714, the SO-313 merge that exposed the bug:
# `git diff --name-only ec2b714^ ec2b714`. Pinned rather than shelled out to
# git so the test does not depend on the commit being fetched.
SO313_CHANGED_FILES = [
    "frontend/src/api/tradingVaults.ts",
    "frontend/src/api/useTradingVaults.ts",
    "frontend/src/screens/TradingVaultDetail.tsx",
    "rust-backend/crates/indexer-graphql/src/lib.rs",
    "rust-backend/services/api-service/src/handlers/trading_vaults.rs",
    "rust-backend/services/api-service/src/router.rs",
    "rust-backend/services/api-service/src/sui_rpc.rs",
]


def _resolve_transitively(service: str) -> set[str]:
    """Second, deliberately independent implementation of the closure.

    affected.py memoises every crate's deps up front and works a worklist;
    this walks the graph recursively from the service manifest only. Two
    implementations that agree is the point — a test that reuses
    `affected.crate_globs` would only prove the function equals itself.
    """
    ws = tomllib.loads((RUST / "Cargo.toml").read_text())
    crate_dir = {}
    for name, spec in ws["workspace"]["dependencies"].items():
        if isinstance(spec, dict) and str(spec.get("path", "")).startswith("crates/"):
            crate_dir[name] = spec["path"].split("/", 1)[1]

    def image_deps(manifest_path: Path) -> set[str]:
        m = tomllib.loads(manifest_path.read_text())
        sections = [m.get("dependencies", {}), m.get("build-dependencies", {})]
        for cfg in m.get("target", {}).values():
            sections += [cfg.get("dependencies", {}), cfg.get("build-dependencies", {})]
        return {crate_dir[n] for s in sections for n in s if n in crate_dir}

    out: set[str] = set()

    def walk(crates: set[str]) -> None:
        for c in crates - out:
            out.add(c)
            walk(image_deps(RUST / "crates" / c / "Cargo.toml"))

    walk(image_deps(RUST / "services" / service / "Cargo.toml"))
    return out


class TestServiceRoster(unittest.TestCase):
    """A new service must not be able to join the workspace un-watched."""

    def test_all_services_matches_deploy_sh(self):
        script = (RUST / "deployment" / "ec2" / "deploy.sh").read_text()
        m = re.search(r"^ALL_SERVICES=\(([^)]*)\)", script, re.MULTILINE)
        self.assertIsNotNone(m, "ALL_SERVICES array not found in deploy.sh")
        self.assertEqual(affected.ALL_SERVICES, m.group(1).split())

    def test_service_globs_covers_exactly_all_services(self):
        watched = sorted([*affected.SERVICE_GLOBS, *affected.GO_SERVICE_GLOBS])
        self.assertEqual(watched, sorted(affected.ALL_SERVICES))

    def test_every_rust_service_has_a_manifest(self):
        # Go services live in go-backend/ (single module, no Cargo.toml);
        # only the Rust roster is manifest-backed.
        for svc in affected.SERVICE_GLOBS:
            with self.subTest(service=svc):
                self.assertTrue((RUST / "services" / svc / "Cargo.toml").is_file())

    def test_every_service_watches_its_own_dir_and_a_dockerfile(self):
        for svc, globs in affected.SERVICE_GLOBS.items():
            with self.subTest(service=svc):
                self.assertIn(f"rust-backend/services/{svc}/**", globs)
                dockerfiles = [g for g in globs if "/Dockerfile." in g]
                self.assertEqual(
                    len(dockerfiles), 1, f"{svc}: expected exactly one Dockerfile glob"
                )
                self.assertTrue(
                    (REPO_ROOT / dockerfiles[0]).is_file(),
                    f"{svc}: {dockerfiles[0]} does not exist",
                )

    def test_service_globs_holds_no_hand_written_crate_globs(self):
        """Crate coverage is derived. A literal crates/ glob here is drift."""
        for svc, globs in affected.SERVICE_GLOBS.items():
            with self.subTest(service=svc):
                self.assertEqual([g for g in globs if "/crates/" in g], [])


class TestDerivedCrateCoverage(unittest.TestCase):
    """The half that makes it stay fixed: coverage tracks the manifests."""

    @classmethod
    def setUpClass(cls):
        cls.derived = affected.crate_globs()

    def test_matches_independent_transitive_resolution(self):
        for svc in affected.SERVICE_GLOBS:
            with self.subTest(service=svc):
                expected = {
                    f"rust-backend/crates/{c}/**" for c in _resolve_transitively(svc)
                }
                self.assertEqual(set(self.derived[svc]), expected)

    def test_dev_dependencies_are_excluded(self):
        """staging-mm-bot dev-depends on exchange-book; a test-only crate must not deploy."""
        bot = tomllib.loads((RUST / "services" / "staging-mm-bot" / "Cargo.toml").read_text())
        self.assertIn(
            "exchange-book",
            bot["dev-dependencies"],
            "fixture drifted: staging-mm-bot no longer dev-depends on exchange-book",
        )
        self.assertNotIn("exchange-book", bot.get("dependencies", {}))
        self.assertNotIn("rust-backend/crates/exchange-book/**", self.derived["staging-mm-bot"])

    def test_transitive_crate_is_covered(self):
        """mm-bot never names deployments; token-info-client pulls it in."""
        mm_bot = tomllib.loads((RUST / "services" / "mm-bot" / "Cargo.toml").read_text())
        self.assertNotIn("deployments", mm_bot["dependencies"])
        self.assertIn("rust-backend/crates/deployments/**", self.derived["mm-bot"])


class TestRegressionPins(unittest.TestCase):
    """The specific under-selections SO-315 was filed for."""

    def test_so313_replay_now_includes_mm_bot(self):
        got = affected.affected_services(SO313_CHANGED_FILES)
        self.assertIn("mm-bot", got, "SO-315 regression: mm-bot skipped again")
        self.assertEqual(
            got,
            [
                "api-service",
                # SO-418: balance-monitor watches vault capital state via
                # indexer-graphql, so indexer-graphql changes reach it.
                "balance-monitor",
                "keeper",
                "mm-bot",
                "price-charting",
                "quoting-service",
                "staging-mm-bot",
            ],
        )

    def test_indexer_graphql_reaches_mm_bot(self):
        got = affected.affected_services(["rust-backend/crates/indexer-graphql/src/lib.rs"])
        self.assertIn("mm-bot", got)

    def test_token_info_client_reaches_every_dependent(self):
        got = affected.affected_services(
            ["rust-backend/crates/token-info-client/src/lib.rs"]
        )
        # Those that were silently skipped before SO-315.
        for svc in (
            "api-service",
            "gas-station",
            "indexer",
            "mm-bot",
            "quoting-service",
        ):
            with self.subTest(service=svc):
                self.assertIn(svc, got)
        self.assertEqual(got, sorted(_dependents_of("token-info-client")))


def _dependents_of(crate: str) -> set[str]:
    return {
        svc for svc in affected.SERVICE_GLOBS if crate in _resolve_transitively(svc)
    }


class TestGoServices(unittest.TestCase):
    """go-backend/ is one Go module: any change rebuilds both Go services."""

    def test_go_change_selects_exactly_the_go_services(self):
        got = affected.affected_services(
            ["go-backend/internal/leaderboard/store/store.go"]
        )
        self.assertEqual(got, ["event-ingestor", "leaderboard"])

    def test_go_dockerfile_change_selects_the_go_services(self):
        got = affected.affected_services(["go-backend/Dockerfile.leaderboard"])
        self.assertEqual(got, ["event-ingestor", "leaderboard"])

    def test_rust_crate_change_does_not_select_go_services(self):
        got = affected.affected_services(
            ["rust-backend/crates/indexer-graphql/src/lib.rs"]
        )
        self.assertNotIn("leaderboard", got)
        self.assertNotIn("event-ingestor", got)

    def test_go_dockerfiles_exist(self):
        for svc in affected.GO_SERVICE_GLOBS:
            with self.subTest(service=svc):
                self.assertTrue(
                    (REPO_ROOT / "go-backend" / f"Dockerfile.{svc}").is_file()
                )


class TestDataRoom(unittest.TestCase):
    """data-room (SO-449) shares the workspace but never rolls a service."""

    DR_SRC = "rust-backend/data-room/collector/src/main.rs"
    DR_MANIFEST = "rust-backend/data-room/gold/Cargo.toml"

    def test_data_room_change_selects_nothing(self):
        self.assertEqual(affected.affected_services([self.DR_SRC, self.DR_MANIFEST]), [])

    def test_data_room_crates_are_not_workspace_crates(self):
        """`crate_globs()` must not mistake data-room/crates/* for crates/*."""
        crates = affected._workspace_crates(REPO_ROOT)
        for name in ("data-room-schema", "data-room-adapters", "data-room-store"):
            self.assertNotIn(name, crates)

    def test_no_service_manifest_depends_on_a_data_room_crate(self):
        ws = tomllib.loads((RUST / "Cargo.toml").read_text())
        data_room = {
            n for n, s in ws["workspace"]["dependencies"].items()
            if isinstance(s, dict) and str(s.get("path", "")).startswith("data-room/")
        }
        self.assertTrue(data_room, "expected data-room crates in [workspace.dependencies]")
        for svc in affected.SERVICE_GLOBS:
            m = tomllib.loads((RUST / "services" / svc / "Cargo.toml").read_text())
            self.assertFalse(
                data_room & affected._dep_names(m),
                f"{svc} depends on a data-room crate; the lockfile carve-out is no longer safe",
            )

    def test_lockfile_with_only_data_room_changes_selects_nothing(self):
        got = affected.affected_services([self.DR_MANIFEST, "rust-backend/Cargo.lock"])
        self.assertEqual(got, [])

    def test_lockfile_with_data_room_and_non_rust_paths_selects_nothing(self):
        got = affected.affected_services(
            [self.DR_MANIFEST, "rust-backend/Cargo.lock", "docs/data-room-spec.md", "README.md"]
        )
        self.assertEqual(got, [])

    def test_lockfile_alone_still_rebuilds_all(self):
        got = affected.affected_services(["rust-backend/Cargo.lock"])
        self.assertEqual(got, sorted(affected.ALL_SERVICES))

    def test_lockfile_with_data_room_and_protocol_path_rebuilds_all(self):
        got = affected.affected_services(
            [self.DR_MANIFEST, "rust-backend/Cargo.lock", "rust-backend/crates/pricing/src/lib.rs"]
        )
        self.assertEqual(got, sorted(affected.ALL_SERVICES))

    def test_root_manifest_change_still_rebuilds_all(self):
        """A data-room dep declared at workspace level still rolls everything:
        the root manifest can move a version every protocol image uses."""
        got = affected.affected_services([self.DR_MANIFEST, "rust-backend/Cargo.toml"])
        self.assertEqual(got, sorted(affected.ALL_SERVICES))


class TestOutputContract(unittest.TestCase):
    """The workflow short-circuits on `[]`; do not change this."""

    def _run(self, args, stdin=""):
        return subprocess.run(
            [sys.executable, str(RUST / "deployment" / "affected.py"), *args],
            input=stdin,
            capture_output=True,
            text=True,
        )

    def test_empty_input_is_empty_array_and_exit_zero(self):
        r = self._run([])
        self.assertEqual(r.returncode, 0)
        self.assertEqual(r.stdout, "[]\n")

    def test_unmatched_paths_are_empty_array_and_exit_zero(self):
        r = self._run(["README.md", "docs/whatever.md"])
        self.assertEqual(r.returncode, 0)
        self.assertEqual(r.stdout, "[]\n")

    def test_stdout_is_a_sorted_json_array(self):
        r = self._run(["rust-backend/crates/protocol-types/src/lib.rs"])
        self.assertEqual(r.returncode, 0)
        parsed = json.loads(r.stdout)
        self.assertIsInstance(parsed, list)
        self.assertEqual(parsed, sorted(parsed))
        self.assertTrue(set(parsed) <= set(affected.ALL_SERVICES))

    def test_rebuild_all_glob_returns_every_service(self):
        r = self._run(["rust-backend/Cargo.lock"])
        self.assertEqual(json.loads(r.stdout), sorted(affected.ALL_SERVICES))

    def test_runs_from_any_cwd(self):
        """_deploy.yml runs it from the repo root; don't depend on that."""
        r = subprocess.run(
            [sys.executable, str(RUST / "deployment" / "affected.py"),
             "rust-backend/crates/indexer-graphql/src/lib.rs"],
            capture_output=True, text=True, cwd=tempfile.gettempdir(),
        )
        self.assertEqual(r.returncode, 0)
        self.assertIn("mm-bot", json.loads(r.stdout))


class TestFailsClosed(unittest.TestCase):
    """Over-deploying costs build minutes. Under-deploying is the bug."""

    def test_missing_manifests_return_all_services(self):
        with tempfile.TemporaryDirectory() as empty:
            got = affected.affected_services(
                ["rust-backend/crates/indexer-graphql/src/lib.rs"], root=Path(empty)
            )
        self.assertEqual(got, sorted(affected.ALL_SERVICES))

    def test_unparseable_manifest_returns_all_services(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "rust-backend").mkdir()
            (root / "rust-backend" / "Cargo.toml").write_text("this is not = = toml\n")
            got = affected.affected_services(
                ["rust-backend/crates/indexer-graphql/src/lib.rs"], root=root
            )
        self.assertEqual(got, sorted(affected.ALL_SERVICES))

    def test_dep_on_a_nonexistent_crate_dir_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            rust = root / "rust-backend"
            (rust / "crates").mkdir(parents=True)
            (rust / "Cargo.toml").write_text(
                '[workspace.dependencies]\nghost = { path = "crates/ghost" }\n'
            )
            for svc in affected.ALL_SERVICES:
                (rust / "services" / svc).mkdir(parents=True)
                (rust / "services" / svc / "Cargo.toml").write_text(
                    '[dependencies]\nghost = { workspace = true }\n'
                )
            got = affected.affected_services(
                ["rust-backend/crates/ghost/src/lib.rs"], root=root
            )
        self.assertEqual(got, sorted(affected.ALL_SERVICES))

    def test_empty_input_still_wins_over_failing_closed(self):
        """`[]` means "nothing changed", not "we could not tell"."""
        with tempfile.TemporaryDirectory() as empty:
            self.assertEqual(affected.affected_services([], root=Path(empty)), [])


if __name__ == "__main__":
    unittest.main()
