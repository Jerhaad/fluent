"""Check the Java version gate before launching the formal models."""

import unittest

from check import is_java_21_version


class JavaVersionTests(unittest.TestCase):
    def test_accepts_feature_only_and_patch_versions(self):
        self.assertTrue(is_java_21_version('openjdk version "21" 2023-09-19 LTS'))
        self.assertTrue(is_java_21_version('openjdk version "21.0.5" 2024-10-15'))
        self.assertTrue(is_java_21_version('openjdk 21.0.5 2024-10-15'))

    def test_rejects_other_feature_versions(self):
        self.assertFalse(is_java_21_version('openjdk version "20.0.2"'))
        self.assertFalse(is_java_21_version('openjdk version "22"'))
        self.assertFalse(is_java_21_version('openjdk version "210"'))


if __name__ == "__main__":
    unittest.main()
