"""The real HTTP fixture never treats its request path as an arbitrary disk path."""

from pathlib import Path
import unittest

from health import payload_path


class HealthRouteTests(unittest.TestCase):
    def test_original_routes_are_unchanged(self):
        for name in ("frontend", "backend"):
            self.assertEqual(
                payload_path(f"/{name}"),
                Path("/srv/shipforge-acceptance") / name / "current" / "health.txt",
            )

    def test_management_namespace_does_not_read_original_routes(self):
        namespace = "management-01234567-1234-789a-abcd-0123456789ab"
        for name in ("frontend", "backend"):
            self.assertEqual(
                payload_path(f"/{namespace}/{name}"),
                Path("/srv/shipforge-acceptance") / namespace / name / "current" / "health.txt",
            )
            self.assertNotEqual(payload_path(f"/{namespace}/{name}"), payload_path(f"/{name}"))

    def test_rejects_traversal_noncanonical_names_and_queries(self):
        for route in (
            "/../frontend", "/management-../backend", "//frontend", "/frontend/",
            "/frontend?secret=x", "/management-not-a-uuid/frontend", "/etc/passwd",
            "/management-01234567-1234-789A-ABCD-0123456789AB/frontend",
            "/management-01234567-1234-789a-abcd-0123456789ab/worker",
        ):
            self.assertIsNone(payload_path(route), route)


if __name__ == "__main__":
    unittest.main()
